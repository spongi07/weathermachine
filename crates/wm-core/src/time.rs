//! Clocks and calendar helpers.
//!
//! Strategy and risk code never read the system clock directly: they receive
//! time through [`Clock`] (live) or through the event envelope (replay). This is
//! what makes backtests deterministic and free of look-ahead.

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Source of time.
pub trait Clock: Send + Sync + 'static {
    /// Wall-clock time (UTC).
    fn now(&self) -> DateTime<Utc>;
    /// Monotonic time since the clock was created. Used for rate limiting so
    /// wall-clock adjustments (NTP steps) can never shorten a politeness interval.
    fn monotonic(&self) -> Duration;
}

/// Real clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Manually driven clock for tests and simulations. Cloning shares the same time.
#[derive(Debug, Clone)]
pub struct ManualClock {
    inner: Arc<Mutex<ManualState>>,
}

#[derive(Debug)]
struct ManualState {
    now: DateTime<Utc>,
    mono: Duration,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self { inner: Arc::new(Mutex::new(ManualState { now: start, mono: Duration::ZERO })) }
    }

    /// Advance both wall and monotonic time.
    pub fn advance(&self, by: Duration) {
        let mut s = self.lock();
        s.now += ChronoDuration::from_std(by).unwrap_or(ChronoDuration::zero());
        s.mono += by;
    }

    /// Jump wall-clock time forward to `t` (monotonic advances by the same amount).
    /// Moving backwards is ignored: simulated time never runs backwards.
    pub fn advance_to(&self, t: DateTime<Utc>) {
        let mut s = self.lock();
        if t > s.now {
            let delta = (t - s.now).to_std().unwrap_or(Duration::ZERO);
            s.now = t;
            s.mono += delta;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ManualState> {
        // A poisoned lock only means another test thread panicked; the data is still valid.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        self.lock().now
    }

    fn monotonic(&self) -> Duration {
        self.lock().mono
    }
}

/// Local calendar date of `ts` in `tz`.
pub fn local_date(ts: DateTime<Utc>, tz: Tz) -> NaiveDate {
    ts.with_timezone(&tz).date_naive()
}

/// Minutes since local midnight of `ts` in `tz` (0..1440).
pub fn local_minute_of_day(ts: DateTime<Utc>, tz: Tz) -> u16 {
    let local = ts.with_timezone(&tz);
    (local.hour() * 60 + local.minute()) as u16
}

/// Minute-of-hour of `ts` as displayed in `tz`.
pub fn local_minute_of_hour(ts: DateTime<Utc>, tz: Tz) -> u8 {
    ts.with_timezone(&tz).minute() as u8
}

/// First instant of the local day `date` in `tz`. Handles zones where local
/// midnight falls in a DST gap by taking the first valid instant after it.
pub fn local_day_start(date: NaiveDate, tz: Tz) -> DateTime<Utc> {
    let mut naive = date.and_hms_opt(0, 0, 0).unwrap_or_default();
    for _ in 0..(4 * 60) {
        if let Some(t) = tz.from_local_datetime(&naive).earliest() {
            return t.with_timezone(&Utc);
        }
        naive += ChronoDuration::minutes(1);
    }
    // Unreachable for real time zones (no gap is longer than a few hours).
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap_or_default())
}

/// Half-open UTC interval `[start, end)` covering local calendar day `date` in `tz`.
/// Its length is 23, 24 or 25 hours around DST transitions.
pub fn local_day_bounds(date: NaiveDate, tz: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = local_day_start(date, tz);
    let next = date.succ_opt().unwrap_or(date);
    (start, local_day_start(next, tz))
}

/// Meteorological season.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Season {
    Winter,
    Spring,
    Summer,
    Autumn,
}

impl Season {
    /// Northern-hemisphere meteorological seasons (DJF, MAM, JJA, SON).
    /// Southern-hemisphere locations pass `southern = true`.
    pub fn from_month(month: u32, southern: bool) -> Season {
        let north = match month {
            12 | 1 | 2 => Season::Winter,
            3..=5 => Season::Spring,
            6..=8 => Season::Summer,
            _ => Season::Autumn,
        };
        if !southern {
            return north;
        }
        match north {
            Season::Winter => Season::Summer,
            Season::Spring => Season::Autumn,
            Season::Summer => Season::Winter,
            Season::Autumn => Season::Spring,
        }
    }

    pub fn of(ts: DateTime<Utc>, tz: Tz, southern: bool) -> Season {
        Season::from_month(ts.with_timezone(&tz).month(), southern)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Season::Winter => "winter",
            Season::Spring => "spring",
            Season::Summer => "summer",
            Season::Autumn => "autumn",
        }
    }
}

/// Approximate local solar noon (minutes after local midnight) using the NOAA
/// equation-of-time approximation. Accuracy ≈ ±1 minute — ample for feature
/// engineering (it is never used as a hard trading rule).
pub fn solar_noon_local_minutes(date: NaiveDate, longitude_deg: f64, tz: Tz) -> f64 {
    let day_of_year = f64::from(date.ordinal());
    let gamma = 2.0 * std::f64::consts::PI / 365.0 * (day_of_year - 1.0);
    let eq_time = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());
    let noon_utc_minutes = 720.0 - 4.0 * longitude_deg - eq_time;
    // UTC offset of the zone at local noon on that day.
    let offset_minutes = date
        .and_hms_opt(12, 0, 0)
        .and_then(|n| tz.from_local_datetime(&n).earliest())
        .map(|t| {
            let utc_naive = t.naive_utc();
            let local_naive = t.naive_local();
            (local_naive - utc_naive).num_minutes() as f64
        })
        .unwrap_or(0.0);
    noon_utc_minutes + offset_minutes
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn amsterdam_day_bounds_regular_summer_day() {
        let (start, end) = local_day_bounds(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), Amsterdam);
        assert_eq!(start, utc("2026-06-30T22:00:00Z"));
        assert_eq!(end, utc("2026-07-01T22:00:00Z"));
    }

    #[test]
    fn amsterdam_dst_transition_days_have_23_and_25_hours() {
        // 2026-03-29: clocks go forward (23h day). 2026-10-25: back (25h day).
        let (s, e) = local_day_bounds(NaiveDate::from_ymd_opt(2026, 3, 29).unwrap(), Amsterdam);
        assert_eq!((e - s).num_hours(), 23);
        let (s, e) = local_day_bounds(NaiveDate::from_ymd_opt(2026, 10, 25).unwrap(), Amsterdam);
        assert_eq!((e - s).num_hours(), 25);
    }

    #[test]
    fn local_date_and_minutes() {
        let t = utc("2026-09-25T22:30:00Z"); // 00:30 CEST on the 26th
        assert_eq!(local_date(t, Amsterdam), NaiveDate::from_ymd_opt(2026, 9, 26).unwrap());
        assert_eq!(local_minute_of_day(t, Amsterdam), 30);
        assert_eq!(local_minute_of_hour(utc("2026-09-26T12:55:00Z"), Amsterdam), 55);
    }

    #[test]
    fn manual_clock_is_monotonic() {
        let clock = ManualClock::new(utc("2026-09-26T12:00:00Z"));
        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.now(), utc("2026-09-26T12:01:30Z"));
        assert_eq!(clock.monotonic(), Duration::from_secs(90));
        clock.advance_to(utc("2026-09-26T11:00:00Z")); // ignored
        assert_eq!(clock.now(), utc("2026-09-26T12:01:30Z"));
        clock.advance_to(utc("2026-09-26T12:02:30Z"));
        assert_eq!(clock.monotonic(), Duration::from_secs(150));
    }

    #[test]
    fn seasons() {
        assert_eq!(Season::from_month(7, false), Season::Summer);
        assert_eq!(Season::from_month(1, false), Season::Winter);
        assert_eq!(Season::from_month(7, true), Season::Winter);
        assert_eq!(Season::from_month(10, false), Season::Autumn);
    }

    #[test]
    fn amsterdam_solar_noon_is_about_1340_in_summer_and_1240_in_winter() {
        let summer =
            solar_noon_local_minutes(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), 4.76, Amsterdam);
        let winter =
            solar_noon_local_minutes(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), 4.76, Amsterdam);
        assert!((summer - (13.0 * 60.0 + 44.0)).abs() < 5.0, "summer {summer}");
        assert!((winter - (12.0 * 60.0 + 50.0)).abs() < 5.0, "winter {winter}");
    }
}
