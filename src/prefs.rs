//! Persisted user preferences.
//!
//! A tiny `key=value` file (`<DATA_DIR>/prefs`) so the choices you make while
//! reviewing survive both the next card and the next launch:
//!
//! - `mono`     — colour (`0`) vs black-and-white (`1`) rendering, toggled by
//!                tapping the top-left counts.
//! - `zoom_idx` — index into `render::ZOOM_STEPS`; the pinch-zoom level you
//!                settled on becomes the default every later card is laid out at.
//!
//! Written atomically (temp file + rename) so a crash mid-write can't leave a
//! truncated file; any unreadable/garbled file just yields the defaults.

use std::io::Write;
use std::path::Path;

use crate::render::ZOOM_STEPS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefs {
    pub mono: bool,
    pub zoom_idx: usize,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { mono: false, zoom_idx: 0 }
    }
}

impl Prefs {
    /// Read prefs from `path`; missing/unparseable → defaults (never fails).
    pub fn load(path: &Path) -> Prefs {
        match std::fs::read_to_string(path) {
            Ok(s) => Prefs::parse(&s),
            Err(_) => Prefs::default(),
        }
    }

    fn parse(s: &str) -> Prefs {
        let mut p = Prefs::default();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            match (k.trim(), v.trim()) {
                ("mono", v) => p.mono = matches!(v, "1" | "true"),
                ("zoom_idx", v) => {
                    if let Ok(i) = v.parse::<usize>() {
                        // Clamp so a file from a build with more steps can't index
                        // past the table.
                        p.zoom_idx = i.min(ZOOM_STEPS.len() - 1);
                    }
                }
                _ => {}
            }
        }
        p
    }

    fn serialize(&self) -> String {
        format!(
            "# ankimarkable preferences — edited by the app, safe to delete\nmono={}\nzoom_idx={}\n",
            self.mono as u8, self.zoom_idx
        )
    }

    /// Atomically write prefs to `path` (temp file in the same dir + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(self.serialize().as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// `save` that only warns on failure — a prefs write must never take the
    /// review session down with it.
    pub fn save_or_warn(&self, path: &Path) {
        if let Err(e) = self.save(path) {
            eprintln!("ankimarkable: could not save prefs to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("ankimarkable-prefs-{}", std::process::id()));
        let path = dir.join("prefs");
        // Missing file → defaults.
        assert_eq!(Prefs::load(&path), Prefs::default());
        let p = Prefs { mono: true, zoom_idx: 2 };
        p.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), p);
        // Garbage / out-of-range values degrade gracefully.
        std::fs::write(&path, "mono=maybe\nzoom_idx=99\nnonsense\n").unwrap();
        let q = Prefs::load(&path);
        assert!(!q.mono);
        assert_eq!(q.zoom_idx, ZOOM_STEPS.len() - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
