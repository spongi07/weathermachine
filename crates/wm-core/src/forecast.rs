//! Forecast products: which series a model was trained on, and from when a
//! day's series may be used (knowledge rule).
//!
//! A fixed-lead series (e.g. Open-Meteo `temperature_2m_previous_day1`) takes
//! each hourly value from the run initialised `lead_days × 24 h` before its
//! valid time. For lead ≥ 1 every value of a local day comes from runs
//! initialised before (or shortly after) that day began, so after a short
//! processing delay the whole day's series is known. `ready_local_minute`
//! is that delay, conservatively: training uses a day's series only from
//! local midnight + `ready_local_minute`, and live trading only when it was
//! retrieved at or after that instant — identical rules on both sides.

use crate::event::ForecastEvent;
use crate::ids::ProviderId;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Identity of a forecast series as used by a trained model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForecastProduct {
    pub provider: ProviderId,
    /// Provider model identifier, e.g. `gfs_global`.
    pub model: String,
    /// Lead in days of every value (1 = forecast 24 h before valid time).
    pub lead_days: u8,
    /// A local day's series is usable from this local minute on.
    pub ready_local_minute: u16,
}

impl ForecastProduct {
    /// Whether `event` is a series of this product.
    pub fn matches(&self, event: &ForecastEvent) -> bool {
        event.provider == self.provider
            && event.model == self.model
            && event.lead_days == Some(self.lead_days)
    }

    /// First instant at which the series for local `date` may be used.
    pub fn usable_from(&self, date: NaiveDate, tz: Tz) -> DateTime<Utc> {
        usable_from(date, tz, self.ready_local_minute)
    }

    /// Short label, e.g. `open_meteo/gfs_global/d1`.
    pub fn label(&self) -> String {
        format!("{}/{}/d{}", self.provider, self.model, self.lead_days)
    }
}

/// Local midnight of `date` plus `minute` minutes of local wall-clock time
/// (DST-correct: 08:00 local is 08:00 local on transition days too).
pub fn usable_from(date: NaiveDate, tz: Tz, minute: u16) -> DateTime<Utc> {
    let start = crate::time::local_day_start(date, tz);
    let wall = date.and_hms_opt(0, 0, 0).unwrap_or_default() + Duration::minutes(i64::from(minute));
    wall.and_local_timezone(tz)
        .earliest()
        .map_or(start + Duration::minutes(i64::from(minute)), |t| {
            t.with_timezone(&Utc)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::TempC;
    use chrono_tz::Europe::Amsterdam;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default()
    }

    fn product() -> ForecastProduct {
        ForecastProduct {
            provider: ProviderId::open_meteo(),
            model: "gfs_global".into(),
            lead_days: 1,
            ready_local_minute: 480,
        }
    }

    #[test]
    fn usable_from_is_local_wall_clock_time_also_on_dst_days() {
        let p = product();
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default();
        assert_eq!(
            p.usable_from(d(2026, 7, 1), Amsterdam),
            utc("2026-07-01T06:00:00Z")
        );
        assert_eq!(
            p.usable_from(d(2026, 1, 15), Amsterdam),
            utc("2026-01-15T07:00:00Z")
        );
        // 29 March 2026: clocks jump 02:00 → 03:00; 08:00 local is 06:00 UTC.
        assert_eq!(
            p.usable_from(d(2026, 3, 29), Amsterdam),
            utc("2026-03-29T06:00:00Z")
        );
        // 25 October 2026: clocks fall back 03:00 → 02:00; 08:00 local is 07:00 UTC.
        assert_eq!(
            p.usable_from(d(2026, 10, 25), Amsterdam),
            utc("2026-10-25T07:00:00Z")
        );
    }

    #[test]
    fn matches_requires_provider_model_and_lead() {
        let p = product();
        let mut e = ForecastEvent {
            location: crate::ids::LocationId::new("amsterdam").unwrap_or_else(|_| unreachable!()),
            provider: ProviderId::open_meteo(),
            model: "gfs_global".into(),
            issued_at: utc("2026-07-01T06:30:00Z"),
            predicted_max: None,
            hourly: vec![(utc("2026-07-01T12:00:00Z"), TempC::from_whole(20))],
            lead_days: Some(1),
        };
        assert!(p.matches(&e));
        e.lead_days = None;
        assert!(!p.matches(&e), "a single run is not the day-1 product");
        e.lead_days = Some(2);
        assert!(!p.matches(&e));
        e.lead_days = Some(1);
        e.model = "ecmwf_ifs".into();
        assert!(!p.matches(&e), "another model is another product");
        assert_eq!(p.label(), "open_meteo/gfs_global/d1");
    }
}
