//! Forecast-derived features, shared by live trading and model training
//! (train/serve consistency).
//!
//! The decision core uses exactly one forecast input, the **forecast rise**:
//! the maximum of the day's hourly forecast over the rest of the local day
//! minus its maximum over the part already elapsed. Level errors of the
//! forecast (grid cell vs. runway sensor, seasonal bias) cancel in the
//! difference; what remains is whether the forecast expects the day to get
//! warmer later — the situation in which an observed high is least likely
//! to be final.
//!
//! HYPOTHESIS TO BACKTEST — not a fact: the rise adds information beyond the
//! observed trajectory. A model only uses it after an out-of-sample
//! evaluation adopted it (see `wm_backtest::research`).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use wm_core::time::local_day_bounds;

/// A local day's hourly forecast, usable from `known_at` on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastDay {
    pub date: NaiveDate,
    /// Knowledge time: the series must not influence anything before it.
    pub known_at: DateTime<Utc>,
    /// `(valid time, tenths °C)`, strictly ascending, covering the whole
    /// local day `[00:00, 24:00]` without gaps longer than one hour.
    pub hourly: Vec<(DateTime<Utc>, i32)>,
}

impl ForecastDay {
    /// Cut local `date` out of an hourly series. `None` unless the series
    /// covers the whole day — a partial forecast is no forecast (fail closed:
    /// the model then behaves as without forecasts).
    pub fn from_series(
        date: NaiveDate,
        tz: Tz,
        series: &[(DateTime<Utc>, i32)],
        known_at: DateTime<Utc>,
    ) -> Option<Self> {
        let (start, end) = local_day_bounds(date, tz);
        let mut hourly: Vec<(DateTime<Utc>, i32)> = series
            .iter()
            .filter(|(t, _)| *t >= start && *t <= end)
            .copied()
            .collect();
        hourly.sort_by_key(|p| p.0);
        hourly.dedup_by_key(|p| p.0);
        let hour = Duration::hours(1);
        let (first, last) = (hourly.first()?.0, hourly.last()?.0);
        if first - start >= hour || end - last >= hour {
            return None;
        }
        if hourly.windows(2).any(|w| w[1].0 - w[0].0 > hour) {
            return None;
        }
        Some(Self {
            date,
            known_at,
            hourly,
        })
    }

    /// Forecast rise at `now` in tenths °C: max over `(now, 24:00]` minus
    /// max over `[00:00, now]`. `None` before `known_at`, or when either part
    /// of the day holds no forecast value.
    pub fn rise_tenths(&self, now: DateTime<Utc>) -> Option<i32> {
        if now < self.known_at {
            return None;
        }
        let elapsed = self.max_where(|t| t <= now)?;
        let remaining = self.max_where(|t| t > now)?;
        Some(remaining - elapsed)
    }

    /// Forecast maximum over the rest of the day (display only).
    pub fn remaining_max_tenths(&self, now: DateTime<Utc>) -> Option<i32> {
        if now < self.known_at {
            return None;
        }
        self.max_where(|t| t > now)
    }

    /// Forecast maximum of the whole local day (display only).
    pub fn day_max_tenths(&self) -> Option<i32> {
        self.max_where(|_| true)
    }

    fn max_where(&self, keep: impl Fn(DateTime<Utc>) -> bool) -> Option<i32> {
        self.hourly
            .iter()
            .filter(|(t, _)| keep(*t))
            .map(|(_, v)| *v)
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;
    use wm_core::time::local_day_start;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
    }

    /// Hourly series from `from` for `hours` hours with `f(hour index)`.
    fn series(from: &str, hours: i64, f: impl Fn(i64) -> i32) -> Vec<(DateTime<Utc>, i32)> {
        (0..hours)
            .map(|h| (utc(from) + Duration::hours(h), f(h)))
            .collect()
    }

    #[test]
    fn rise_compares_the_rest_of_the_day_with_the_part_already_elapsed() {
        // 1 July (UTC+2): local day = 30 Jun 22:00 UTC .. 1 Jul 22:00 UTC.
        // Peak 250 at 13:00 UTC (15:00 local), cooling afterwards.
        let s = series("2026-06-30T12:00:00Z", 48, |h| {
            let hour_utc = (12 + h) % 24;
            250 - 10 * (hour_utc - 13).abs() as i32
        });
        let day =
            ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &s, utc("2026-07-01T06:00:00Z"))
                .unwrap();
        assert_eq!(day.hourly.len(), 25, "00:00 .. 24:00 local, hourly");
        assert_eq!(day.day_max_tenths(), Some(250));
        // 11:30 UTC: warmest hour still ahead → positive rise (+2.0 °C).
        assert_eq!(day.rise_tenths(utc("2026-07-01T11:30:00Z")), Some(20));
        // 15:10 UTC: the forecast peak is past → negative rise.
        assert_eq!(day.rise_tenths(utc("2026-07-01T15:10:00Z")), Some(-30));
        assert_eq!(
            day.remaining_max_tenths(utc("2026-07-01T15:10:00Z")),
            Some(220)
        );
        // Before the knowledge time nothing is known.
        assert_eq!(day.rise_tenths(utc("2026-07-01T05:59:00Z")), None);
        // After the last value of the day nothing remains.
        assert_eq!(day.rise_tenths(utc("2026-07-01T22:00:00Z")), None);
    }

    #[test]
    fn incomplete_days_are_rejected() {
        let known = utc("2026-07-01T06:00:00Z");
        let full = series("2026-06-30T22:00:00Z", 25, |_| 200);
        assert!(ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &full, known).is_some());
        // A missing hour (null in the payload) in the middle.
        let mut gap = full.clone();
        gap.remove(12);
        assert!(ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &gap, known).is_none());
        // Series ends at 22:00 local — the last two hours of the day are unknown.
        let short = series("2026-06-30T22:00:00Z", 23, |_| 200);
        assert!(ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &short, known).is_none());
        // Starts too late.
        let late = series("2026-06-30T23:00:00Z", 24, |_| 200);
        assert!(ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &late, known).is_none());
        assert!(ForecastDay::from_series(d(2026, 7, 1), Amsterdam, &[], known).is_none());
    }

    #[test]
    fn dst_days_have_23_and_25_local_hours() {
        let known = utc("2026-01-01T00:00:00Z");
        for (date, hours) in [(d(2026, 3, 29), 23), (d(2026, 10, 25), 25)] {
            let start = local_day_start(date, Amsterdam);
            let s: Vec<_> = (0..=hours)
                .map(|h| (start + Duration::hours(h), 100 + h as i32))
                .collect();
            let day = ForecastDay::from_series(date, Amsterdam, &s, known).unwrap();
            assert_eq!(day.hourly.len() as i64, hours + 1, "{date}");
        }
    }
}
