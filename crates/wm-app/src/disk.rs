//! The disk under the data directory: its free space every minute, the
//! database's size every ten. On a default Docker install the volumes (the
//! database's, the backups', `/data`) share one disk, so the free space of
//! `/data` is the database's too.
//!
//! Under `app.disk_warn_free_gb` an alert is raised (and pushed, with
//! Pushover set); under `app.disk_brake_free_gb` the order books, nearly
//! every row the database gains, are not written until there is room again,
//! so the database does not fill the disk itself. Decisions, orders and fills
//! are always written. On 17 October 2026 the disk of the production host
//! filled and PostgreSQL could not start for hours: nothing had said it was
//! filling.

use std::path::Path;

/// A GB here is 1024³ bytes, as `df -h` counts.
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A level is left only once the free space is this far above its
/// threshold, so a disk at the edge does not flap between levels.
pub const MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// One reading of the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// Free to an unprivileged process (what PostgreSQL may use), bytes.
    pub free: u64,
    pub total: u64,
}

impl Reading {
    /// The share of the disk in use, in percent (what `df` shows).
    pub fn used_pct(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        100.0 * (1.0 - self.free as f64 / self.total as f64)
    }
}

/// The disk that holds `path`.
pub fn read(path: &Path) -> std::io::Result<Reading> {
    let s = rustix::fs::statvfs(path)?;
    let unit = if s.f_frsize > 0 {
        s.f_frsize
    } else {
        s.f_bsize
    };
    Ok(Reading {
        free: s.f_bavail.saturating_mul(unit),
        total: s.f_blocks.saturating_mul(unit),
    })
}

/// How full the disk is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    #[default]
    Ok,
    /// Under the alert's threshold.
    Low,
    /// Under the brake's: order books are not written.
    Brake,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Low => "low",
            Level::Brake => "brake",
        }
    }
}

/// The alert's and the brake's thresholds, in bytes free (0 = off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub warn: u64,
    pub brake: u64,
}

impl Thresholds {
    pub fn from_gb(warn_gb: f64, brake_gb: f64) -> Self {
        let bytes = |gb: f64| (gb.max(0.0) * GB) as u64;
        Self {
            warn: bytes(warn_gb),
            brake: bytes(brake_gb),
        }
    }
}

/// The level at `free` bytes, coming from `prev`: a level is entered under
/// its threshold and left only [`MARGIN_BYTES`] above it.
pub fn level(prev: Level, free: u64, t: Thresholds) -> Level {
    let under = |limit: u64, held: bool| {
        limit > 0
            && free
                < if held {
                    limit.saturating_add(MARGIN_BYTES)
                } else {
                    limit
                }
    };
    if under(t.brake, prev == Level::Brake) {
        Level::Brake
    } else if under(t.warn, prev >= Level::Low) {
        Level::Low
    } else {
        Level::Ok
    }
}

/// Bytes in GB, or in MB under one GB ("0.0 GB" said nothing).
pub fn gb(bytes: u64) -> String {
    let b = bytes as f64;
    if b < GB {
        format!("{:.0} MB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// "4.2 GB free of 40.0 GB (90 % used), the database 21.3 GB".
pub fn summary(r: &Reading, database: Option<u64>) -> String {
    let mut s = format!(
        "{} free of {} ({:.0} % used)",
        gb(r.free),
        gb(r.total),
        r.used_pct()
    );
    if let Some(db) = database {
        s.push_str(&format!(", the database {}", gb(db)));
    }
    s
}

/// What to say when the level goes from `prev` to `now`: the alert's level,
/// its text, and the push message's title. `None` when it did not change.
pub fn change(
    prev: Level,
    now: Level,
    r: &Reading,
    database: Option<u64>,
    t: Thresholds,
) -> Option<(&'static str, String, String)> {
    if prev == now {
        return None;
    }
    let at = summary(r, database);
    let free = gb(r.free);
    Some(match now {
        Level::Brake => (
            "critical",
            format!(
                "disk almost full: {at}. Order books (the journal's updates, the market history) are not written until {} is free; decisions, orders and fills are. Free space now: docs/deployment/portainer.md, \"Disk full\"",
                gb(t.brake.saturating_add(MARGIN_BYTES))
            ),
            format!("Disk almost full: {free} free, books not recorded"),
        ),
        Level::Low if prev == Level::Brake => (
            "warning",
            format!(
                "disk: {at}. Order books are written again; still under the {} the alert asks for",
                gb(t.warn)
            ),
            format!("Disk: {free} free, books recorded again"),
        ),
        Level::Low => (
            "critical",
            format!(
                "disk nearly full: {at}. {}Free space before it fills: docs/deployment/portainer.md, \"Disk full\"",
                if t.brake > 0 {
                    format!(
                        "Under {} free, order books stop being written. ",
                        gb(t.brake)
                    )
                } else {
                    String::new()
                }
            ),
            format!("Disk nearly full: {free} free"),
        ),
        Level::Ok => (
            "info",
            format!(
                "disk: {at} again{}",
                if prev == Level::Brake {
                    "; order books are written again"
                } else {
                    ""
                }
            ),
            format!("Disk OK again: {free} free"),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1024 * 1024 * 1024;

    fn t() -> Thresholds {
        Thresholds::from_gb(5.0, 2.0)
    }

    /// A level is entered under its threshold and left only half a GB above
    /// it; the brake can be reached from OK in one reading and left for
    /// either level; a threshold of 0 is off.
    #[test]
    fn levels_enter_under_and_leave_above_with_a_margin() {
        let t = t();
        assert_eq!(t.warn, 5 * G);
        assert_eq!(level(Level::Ok, 6 * G, t), Level::Ok);
        assert_eq!(level(Level::Ok, 5 * G - 1, t), Level::Low);
        assert_eq!(level(Level::Low, 5 * G + 100, t), Level::Low, "margin");
        assert_eq!(level(Level::Low, 5 * G + MARGIN_BYTES, t), Level::Ok);
        assert_eq!(
            level(Level::Ok, G, t),
            Level::Brake,
            "straight to the brake"
        );
        assert_eq!(level(Level::Brake, 2 * G + 100, t), Level::Brake, "margin");
        assert_eq!(level(Level::Brake, 3 * G, t), Level::Low);
        assert_eq!(level(Level::Brake, 9 * G, t), Level::Ok);
        let off = Thresholds { warn: 0, brake: 0 };
        assert_eq!(level(Level::Ok, 0, off), Level::Ok);
        let brake_only = Thresholds {
            warn: 0,
            brake: 2 * G,
        };
        assert_eq!(level(Level::Ok, G, brake_only), Level::Brake);
        assert_eq!(level(Level::Brake, 3 * G, brake_only), Level::Ok);
    }

    /// Each change says what it means and what to do; no change says
    /// nothing.
    #[test]
    fn changes_are_said_once_each() {
        let t = t();
        let r = Reading {
            free: 4 * G + G / 5,
            total: 40 * G,
        };
        assert_eq!(
            summary(&r, Some(21 * G + G / 3)),
            "4.2 GB free of 40.0 GB (90 % used), the database 21.3 GB"
        );
        assert_eq!(gb(9 * 1024 * 1024), "9 MB", "a small database in MB");
        assert_eq!(gb(G), "1.0 GB");
        assert!(change(Level::Low, Level::Low, &r, None, t).is_none());
        let (level, text, title) = change(Level::Ok, Level::Low, &r, None, t).unwrap();
        assert_eq!(level, "critical");
        assert_eq!(title, "Disk nearly full: 4.2 GB free");
        assert!(text.starts_with("disk nearly full: 4.2 GB free of 40.0 GB"));
        assert!(text.contains("Under 2.0 GB free, order books stop being written"));
        assert!(text.contains("\"Disk full\""));
        let (level, text, title) = change(Level::Low, Level::Brake, &r, None, t).unwrap();
        assert_eq!(level, "critical");
        assert!(title.ends_with("books not recorded"));
        assert!(text.contains("not written until 2.5 GB is free"));
        assert!(text.contains("decisions, orders and fills are"));
        let (level, text, _) = change(Level::Brake, Level::Low, &r, None, t).unwrap();
        assert_eq!(level, "warning");
        assert!(text.contains("written again"));
        let (level, text, title) = change(Level::Brake, Level::Ok, &r, None, t).unwrap();
        assert_eq!(level, "info");
        assert_eq!(title, "Disk OK again: 4.2 GB free");
        assert!(text.ends_with("again; order books are written again"));
        let (_, text, _) = change(Level::Low, Level::Ok, &r, None, t).unwrap();
        assert!(text.ends_with("(90 % used) again"));
    }

    /// The disk this test runs on can be read.
    #[test]
    fn the_working_directory_can_be_read() {
        let r = read(Path::new(".")).unwrap();
        assert!(r.total > 0 && r.free <= r.total);
        assert!((0.0..=100.0).contains(&r.used_pct()));
    }
}
