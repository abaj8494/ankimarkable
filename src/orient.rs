//! Panel-orientation tracker (portrait vs portrait-inverted).
//!
//! xochitl auto-rotates its whole QML scene from the accelerometer, and the
//! AppLoad/QTFB surface — plus the touch input rm-appload forwards — rotates
//! with it. The marker is the ONE input we read as raw evdev (native panel
//! frame), so when the scene is portrait-inverted every ink sample lands
//! point-mirrored unless we apply the same 180° flip ourselves.
//!
//! We read the same sensor xochitl does (the lis2dw12, via sysfs raw channels)
//! and latch an orientation only while gravity is confidently in the panel
//! plane. Lying flat carries NO orientation information — exactly when xochitl
//! also keeps its previous latch — so the latch persists to a state file and a
//! launch on a flat tablet inherits the last known orientation instead of
//! defaulting wrong. `ANKIMARKABLE_ORIENT=normal|inverted` pins it for testing.
//!
//! Landscape (gravity along the panel's short axis) never changes the latch:
//! the portrait surface isn't usable sideways anyway, and flapping the flip
//! there would only corrupt the next portrait latch.

use std::time::{Duration, Instant};

const STATE_FILE: &str = "/home/root/.ankimarkable/orientation";
const IIO_ROOT: &str = "/sys/bus/iio/devices";
const ACCEL_NAME: &str = "lis2dw12_accel";

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
    /// `None` when pinned by ANKIMARKABLE_ORIENT or the sensor is missing —
    /// either way the latch never changes.
    accel_dir: Option<String>,
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
            _ => (false, load_state()),
        };
        let accel_dir = if pinned { None } else { resolve_accel() };
        if !pinned && accel_dir.is_none() {
            eprintln!("ankimarkable: no {ACCEL_NAME} iio device — orientation latched only");
        }
        Self {
            accel_dir,
            flip180: initial,
            candidate: None,
            candidate_reads: 0,
            last_poll: None,
        }
    }

    pub fn flip180(&self) -> bool {
        self.flip180
    }

    /// Rate-limited sensor check; `Some(new_flip)` only on a confirmed change.
    pub fn poll(&mut self) -> Option<bool> {
        let dir = self.accel_dir.as_ref()?; // pinned or sensor-less: never changes
        if self.last_poll.map_or(false, |t| t.elapsed() < POLL_EVERY) {
            return None;
        }
        self.last_poll = Some(Instant::now());

        // Sensor frame → device frame per the DT mount matrix ("0,1,0; 1,0,0;
        // 0,0,-1"): x_dev = raw_y, y_dev = raw_x. +y_dev points at the native
        // portrait top edge, so upright-normal reads y_dev ≈ +1 g.
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
