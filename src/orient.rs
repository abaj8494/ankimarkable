//! Panel-orientation tracker (portrait vs portrait-inverted).
//!
//! xochitl auto-rotates its whole QML scene from the accelerometer, and the
//! AppLoad/QTFB surface — plus the touch input rm-appload forwards — rotates
//! with it. The marker is the ONE input we read as raw evdev (native panel
//! frame), so when the scene is portrait-inverted every ink sample lands
//! point-mirrored unless we apply the same 180° flip ourselves.
//!
//! PRIMARY SOURCE: xochitl's actual scene rotation, exported in degrees to
//! /tmp/.screen-orientation by the screenOrientation.qmd plugin (ferrari repo,
//! scripts/qmd/) on every rotationChanged + a 60s heartbeat. Only xochitl
//! knows the truth — it holds the single iio event fd of the 6D orientation
//! sensor, and its latch (e.g. across a flat-on-desk flip) is what the scene
//! shows. The file lives on tmpfs, so a reboot clears it and a dead exporter
//! goes stale by mtime; either way we fall back.
//!
//! FALLBACK: read the same lis2dw12 via sysfs raw channels and latch an
//! orientation only while gravity is confidently in the panel plane. Lying
//! flat carries NO orientation information — exactly when xochitl also keeps
//! its previous latch — so the latch persists to a state file and a launch on
//! a flat tablet inherits the last known orientation instead of defaulting
//! wrong. `ANKIMARKABLE_ORIENT=normal|inverted` pins everything for testing.
//!
//! Landscape (gravity along the panel's short axis / rotation 90|270) never
//! changes the latch: the portrait surface isn't usable sideways anyway, and
//! flapping the flip there would only corrupt the next portrait latch.

use std::time::{Duration, Instant};

const STATE_FILE: &str = "/home/root/.ankimarkable/orientation";
const IIO_ROOT: &str = "/sys/bus/iio/devices";
const ACCEL_NAME: &str = "lis2dw12_accel";

// Scene rotation exported by screenOrientation.qmd. Stale = the exporter died
// (three missed heartbeats) — a torn read mid-PUT just parses as no-info.
const SYSTEM_FILE: &str = "/tmp/.screen-orientation";
const SYSTEM_STALE: Duration = Duration::from_secs(180);

// Raw counts per 1 g on this part at its configured full scale (measured on
// ferrari: flat on a desk reads z_raw ≈ -15880). The in-plane threshold is
// ~0.35 g — upright/handheld reading angles clear it easily, a near-flat
// tablet doesn't, so desk-writing keeps the latch from the last pickup.
const ONE_G_RAW: i32 = 15880;
const IN_PLANE_THRESH: i32 = (ONE_G_RAW as i64 * 35 / 100) as i32;

const POLL_EVERY: Duration = Duration::from_millis(500);
// Consecutive consistent reads required before switching — one transient
// sample mid-handling can't flip the pen frame.
const CONFIRM_READS: u32 = 2;

pub struct OrientTracker {
    /// `<iio dir>` of the lis2dw12 accel channels, resolved by name (the
    /// iio:deviceN index is as unstable as event indexes — never hardcode).
    /// `None` = sensor missing; the exported scene rotation still applies.
    accel_dir: Option<String>,
    /// ANKIMARKABLE_ORIENT set: ignore every source, the latch never changes.
    pinned: bool,
    flip180: bool,
    candidate: Option<bool>,
    candidate_reads: u32,
    last_poll: Option<Instant>,
}

impl OrientTracker {
    pub fn new() -> Self {
        let (pinned, initial) = match std::env::var("ANKIMARKABLE_ORIENT").as_deref() {
            Ok("inverted") => (true, true),
            Ok("normal") => (true, false),
            // The live scene rotation (when exported) beats the persisted latch.
            _ => (false, read_system().unwrap_or_else(load_state)),
        };
        let accel_dir = if pinned { None } else { resolve_accel() };
        if !pinned && accel_dir.is_none() {
            eprintln!(
                "ankimarkable: no {ACCEL_NAME} iio device — orientation from {SYSTEM_FILE} only"
            );
        }
        Self {
            accel_dir,
            pinned,
            flip180: initial,
            candidate: None,
            candidate_reads: 0,
            last_poll: None,
        }
    }

    pub fn flip180(&self) -> bool {
        self.flip180
    }

    /// Rate-limited orientation check; `Some(new_flip)` only on a confirmed
    /// change. The exported scene rotation is authoritative and switches
    /// immediately; the accelerometer fallback needs CONFIRM_READS.
    pub fn poll(&mut self) -> Option<bool> {
        if self.pinned {
            return None;
        }
        if self.last_poll.map_or(false, |t| t.elapsed() < POLL_EVERY) {
            return None;
        }
        self.last_poll = Some(Instant::now());

        if let Some(flip) = read_system() {
            self.candidate = None;
            self.candidate_reads = 0;
            if flip != self.flip180 {
                self.flip180 = flip;
                save_state(flip);
                return Some(flip);
            }
            return None;
        }

        // Sensor frame → device frame per the DT mount matrix ("0,1,0; 1,0,0;
        // 0,0,-1"): x_dev = raw_y, y_dev = raw_x. +y_dev points at the native
        // portrait top edge, so upright-normal reads y_dev ≈ +1 g.
        let dir = self.accel_dir.as_ref()?;
        let y_dev = read_raw(dir, "in_accel_x_raw")?;
        let x_dev = read_raw(dir, "in_accel_y_raw")?;
        if y_dev.abs() < IN_PLANE_THRESH || y_dev.abs() < x_dev.abs() {
            // Flat or landscape-ish: no portrait information — keep the latch.
            self.candidate = None;
            self.candidate_reads = 0;
            return None;
        }

        let cand = y_dev < 0; // gravity toward the native top edge = held inverted
        if cand == self.flip180 {
            self.candidate = None;
            self.candidate_reads = 0;
            return None;
        }
        if self.candidate == Some(cand) {
            self.candidate_reads += 1;
        } else {
            self.candidate = Some(cand);
            self.candidate_reads = 1;
        }
        if self.candidate_reads < CONFIRM_READS {
            return None;
        }
        self.flip180 = cand;
        self.candidate = None;
        self.candidate_reads = 0;
        save_state(cand);
        Some(cand)
    }
}

/// The scene rotation xochitl actually shows, when the qmd exporter is alive:
/// 0 → normal, 180 → inverted, 90/270 (landscape) → no portrait information.
fn read_system() -> Option<bool> {
    let md = std::fs::metadata(SYSTEM_FILE).ok()?;
    let fresh = md
        .modified()
        .ok()?
        .elapsed()
        .map(|age| age < SYSTEM_STALE)
        .unwrap_or(true); // mtime in the future = clock skew, treat as fresh
    if !fresh {
        return None;
    }
    match std::fs::read_to_string(SYSTEM_FILE).ok()?.trim().parse::<i32>().ok()? {
        0 => Some(false),
        180 => Some(true),
        _ => None,
    }
}

fn load_state() -> bool {
    std::fs::read_to_string(STATE_FILE)
        .map(|s| s.trim() == "inverted")
        .unwrap_or(false)
}

fn save_state(flip180: bool) {
    let _ = std::fs::write(STATE_FILE, if flip180 { "inverted" } else { "normal" });
}

fn resolve_accel() -> Option<String> {
    let entries = std::fs::read_dir(IIO_ROOT).ok()?;
    for e in entries.flatten() {
        let dir = e.path();
        if let Ok(name) = std::fs::read_to_string(dir.join("name")) {
            if name.trim() == ACCEL_NAME {
                return Some(dir.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn read_raw(dir: &str, channel: &str) -> Option<i32> {
    std::fs::read_to_string(format!("{dir}/{channel}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}
