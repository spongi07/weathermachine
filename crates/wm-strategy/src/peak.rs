//! Peak detection: trajectory features and confirmation windows.
//!
//! HYPOTHESIS TO BACKTEST (never encoded as fact): the probability that the
//! observed high is final increases with confirmed time since the high,
//! drop from the high, a declining trajectory, lack of retests and the end of
//! the local heating cycle. This module only *measures* those features; the
//! probability model estimates their predictive value from history.

use crate::forecast::ForecastDay;
use crate::state::{DayState, ObsPoint, ViewKind};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use wm_core::ids::StationId;
use wm_core::time::{Season, local_minute_of_day, solar_noon_local_minutes};

/// Confirmation windows under research (minutes without a higher observation).
pub const CONFIRMATION_WINDOWS: [u32; 9] = [30, 45, 60, 75, 90, 105, 120, 150, 180];

/// Shape of the trajectory after the last touch of the high.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryClass {
    /// The latest observation is the high (still at/near the peak).
    AtHigh,
    /// Non-increasing since the high with at least one decrease (e.g. 18 → 17.8 → 17.5 → 17.2).
    SteadyDecline,
    /// Went down and back up below the high (e.g. 18 → 17.9 → 18 → 17.9 pattern below the high).
    Oscillating,
    /// Too few points to classify.
    Insufficient,
}

impl TrajectoryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TrajectoryClass::AtHigh => "at_high",
            TrajectoryClass::SteadyDecline => "steady_decline",
            TrajectoryClass::Oscillating => "oscillating",
            TrajectoryClass::Insufficient => "insufficient",
        }
    }
}

/// Features describing a candidate daily peak at a point in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakFeatures {
    pub station: StationId,
    pub date: NaiveDate,
    pub view: ViewKind,
    pub high_tenths: i32,
    /// High in whole degrees (resolution precision, ICAO half-up rounding).
    pub high_whole: i32,
    pub high_at: DateTime<Utc>,
    pub current_tenths: i32,
    pub drop_tenths: i32,
    /// Observed coverage since the last touch of the high.
    pub minutes_since_high: i64,
    pub minutes_since_first_high: i64,
    pub lower_obs_since_high: u32,
    pub retests: u32,
    pub slope_c_per_hour: Option<f64>,
    pub accel_c_per_hour2: Option<f64>,
    pub trajectory: TrajectoryClass,
    pub local_minute_now: u16,
    pub high_local_minute: u16,
    /// Minutes of `now` after local solar noon (negative before noon).
    pub minutes_after_solar_noon: i32,
    pub month: u32,
    pub season: Season,
    pub observation_count: u32,
    /// `now − last observation`, minutes.
    pub data_age_minutes: i64,
    /// Forecast rise (tenths °C) of the day's fixed-lead forecast at `now`,
    /// see [`crate::forecast`]. `None`: no usable forecast (the model then
    /// uses only observation features).
    #[serde(default)]
    pub forecast_rise_tenths: Option<i32>,
}

impl PeakFeatures {
    /// Whether a confirmation window of `minutes` has been met by observed data.
    pub fn window_met(&self, minutes: u32) -> bool {
        self.minutes_since_high >= i64::from(minutes)
    }

    pub fn windows_met(&self) -> Vec<u32> {
        CONFIRMATION_WINDOWS
            .iter()
            .copied()
            .filter(|w| self.window_met(*w))
            .collect()
    }
}

/// Configuration of the peak detector (engineering parameters, not optimized).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakConfig {
    /// Station longitude (degrees east) for solar-noon computation.
    pub longitude: f64,
    pub southern_hemisphere: bool,
    /// Peak-watch window relative to solar noon (minutes).
    pub watch_start_after_noon: i32,
    pub watch_end_after_noon: i32,
    /// Current temperature within this many tenths of the high ⇒ peak watch.
    pub watch_margin_tenths: i32,
}

impl Default for PeakConfig {
    fn default() -> Self {
        Self {
            longitude: 4.76,
            southern_hemisphere: false,
            watch_start_after_noon: -180,
            watch_end_after_noon: 300,
            watch_margin_tenths: 10,
        }
    }
}

/// Result of assessing a day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakAssessment {
    pub features: PeakFeatures,
    pub windows_met: Vec<u32>,
    /// Engine hint for the collector: poll more attentively (inside arrival windows only).
    pub peak_watch: bool,
}

fn classify(points_after: &[ObsPoint], high_tenths: i32, latest_is_high: bool) -> TrajectoryClass {
    if latest_is_high {
        return TrajectoryClass::AtHigh;
    }
    if points_after.is_empty() {
        return TrajectoryClass::Insufficient;
    }
    let mut prev = high_tenths;
    let mut rose = false;
    for p in points_after {
        let t = p.temp.tenths();
        if t > prev {
            rose = true;
        }
        prev = t;
    }
    if rose {
        TrajectoryClass::Oscillating
    } else {
        TrajectoryClass::SteadyDecline
    }
}

/// Stateless peak detector.
#[derive(Debug, Clone, Default)]
pub struct PeakDetectionEngine {
    pub config: PeakConfig,
}

impl PeakDetectionEngine {
    pub fn new(config: PeakConfig) -> Self {
        Self { config }
    }

    pub fn assess(&self, state: &DayState, tz: Tz, now: DateTime<Utc>) -> Option<PeakAssessment> {
        self.assess_with(state, tz, now, None)
    }

    /// [`Self::assess`] plus the forecast rise from `forecast` — the day's
    /// fixed-lead series, which must be for `state.date`.
    pub fn assess_with(
        &self,
        state: &DayState,
        tz: Tz,
        now: DateTime<Utc>,
        forecast: Option<&ForecastDay>,
    ) -> Option<PeakAssessment> {
        let high = state.high?;
        let current = state.current?;
        let after: Vec<ObsPoint> = state
            .points
            .iter()
            .filter(|p| p.observed_at > high.last_at)
            .copied()
            .collect();
        let latest_is_high = current.observed_at == high.last_at;
        let noon = solar_noon_local_minutes(state.date, self.config.longitude, tz);
        let local_now = local_minute_of_day(now, tz);
        let minutes_after_noon = (f64::from(local_now) - noon).round() as i32;
        let features = PeakFeatures {
            station: state.station.clone(),
            date: state.date,
            view: state.view,
            high_tenths: high.value.tenths(),
            high_whole: high.value.round_half_up_whole(),
            high_at: high.last_at,
            current_tenths: current.temp.tenths(),
            drop_tenths: high.value.diff_tenths(current.temp),
            minutes_since_high: state.observed_minutes_since_high.unwrap_or(0),
            minutes_since_first_high: (current.observed_at - high.first_at).num_minutes(),
            lower_obs_since_high: state.lower_since_high,
            retests: high.retests,
            slope_c_per_hour: state.slope_c_per_hour,
            accel_c_per_hour2: state.accel_c_per_hour2,
            trajectory: classify(&after, high.value.tenths(), latest_is_high),
            local_minute_now: local_now,
            high_local_minute: local_minute_of_day(high.last_at, tz),
            minutes_after_solar_noon: minutes_after_noon,
            month: state.date.month(),
            season: Season::from_month(state.date.month(), self.config.southern_hemisphere),
            observation_count: state.observation_count,
            data_age_minutes: state
                .last_observation_at
                .map_or(i64::MAX, |t| (now - t).num_minutes()),
            forecast_rise_tenths: forecast
                .filter(|f| f.date == state.date)
                .and_then(|f| f.rise_tenths(now)),
        };
        let near_high = features.drop_tenths <= self.config.watch_margin_tenths;
        let in_heating = (self.config.watch_start_after_noon..=self.config.watch_end_after_noon)
            .contains(&minutes_after_noon);
        Some(PeakAssessment {
            windows_met: features.windows_met(),
            peak_watch: near_high && in_heating,
            features,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TemperatureStateEngine;
    use chrono_tz::Europe::Amsterdam;
    use wm_core::ids::ProviderId;
    use wm_core::units::TempC;
    use wm_core::weather::{Observation, ObservationKey, QualityFlags, ReportType, TempPrecision};

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn obs(t: &str, tenths: i32) -> Observation {
        Observation {
            key: ObservationKey {
                station: StationId::new("EHAM").unwrap(),
                observed_at: utc(t),
                report_type: ReportType::Metar,
            },
            version: 1,
            temperature: Some(TempC::from_tenths(tenths)),
            dewpoint: None,
            precision: TempPrecision::WholeDegree,
            raw_text: String::new(),
            content_hash: t.to_owned(),
            provider: ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: utc(t),
            parser_version: 1,
            quality: QualityFlags::default(),
        }
    }

    fn assess(series: &[(&str, i32)], now: &str) -> PeakAssessment {
        let mut e = TemperatureStateEngine::new(3);
        let st = StationId::new("EHAM").unwrap();
        e.register_station(st.clone(), Amsterdam);
        for (t, v) in series {
            e.apply_observation(&obs(t, *v));
        }
        let now = utc(now);
        let date = wm_core::time::local_date(now, Amsterdam);
        let s = e.day_state(&st, date, ViewKind::All, now).unwrap();
        PeakDetectionEngine::default()
            .assess(&s, Amsterdam, now)
            .unwrap()
    }

    #[test]
    fn steady_decline_vs_oscillation() {
        let a = assess(
            &[
                ("2026-07-01T12:00:00Z", 180),
                ("2026-07-01T12:30:00Z", 178),
                ("2026-07-01T13:00:00Z", 175),
                ("2026-07-01T13:30:00Z", 172),
            ],
            "2026-07-01T13:31:00Z",
        );
        assert_eq!(a.features.trajectory, TrajectoryClass::SteadyDecline);
        assert_eq!(a.features.minutes_since_high, 90);
        assert_eq!(a.windows_met, vec![30, 45, 60, 75, 90]);
        let b = assess(
            &[
                ("2026-07-01T12:00:00Z", 180),
                ("2026-07-01T12:30:00Z", 179),
                ("2026-07-01T13:00:00Z", 179),
                ("2026-07-01T13:30:00Z", 179),
            ],
            "2026-07-01T13:31:00Z",
        );
        assert_eq!(b.features.trajectory, TrajectoryClass::SteadyDecline);
        let c = assess(
            &[
                ("2026-07-01T12:00:00Z", 180),
                ("2026-07-01T12:30:00Z", 178),
                ("2026-07-01T13:00:00Z", 179),
                ("2026-07-01T13:30:00Z", 178),
            ],
            "2026-07-01T13:31:00Z",
        );
        assert_eq!(c.features.trajectory, TrajectoryClass::Oscillating);
        let d = assess(
            &[("2026-07-01T12:00:00Z", 170), ("2026-07-01T12:30:00Z", 180)],
            "2026-07-01T12:31:00Z",
        );
        assert_eq!(d.features.trajectory, TrajectoryClass::AtHigh);
        assert!(d.windows_met.is_empty());
    }

    #[test]
    fn peak_watch_only_near_high_during_heating_hours() {
        // 14:00 CEST, at the high → watch.
        let a = assess(
            &[("2026-07-01T11:30:00Z", 175), ("2026-07-01T12:00:00Z", 180)],
            "2026-07-01T12:01:00Z",
        );
        assert!(a.peak_watch);
        // 23:00 CEST → no watch even at the high.
        let b = assess(
            &[("2026-07-01T20:30:00Z", 150), ("2026-07-01T21:00:00Z", 151)],
            "2026-07-01T21:01:00Z",
        );
        assert!(!b.peak_watch);
        // Far below the high → no watch.
        let c = assess(
            &[("2026-07-01T10:00:00Z", 200), ("2026-07-01T12:00:00Z", 160)],
            "2026-07-01T12:01:00Z",
        );
        assert!(!c.peak_watch);
    }

    #[test]
    fn features_capture_time_and_season() {
        let a = assess(
            &[("2026-07-01T12:00:00Z", 180), ("2026-07-01T13:00:00Z", 170)],
            "2026-07-01T13:05:00Z",
        );
        assert_eq!(a.features.season, Season::Summer);
        assert_eq!(a.features.month, 7);
        assert_eq!(a.features.high_local_minute, 14 * 60);
        assert_eq!(a.features.high_whole, 18);
        assert_eq!(a.features.drop_tenths, 10);
        assert_eq!(a.features.data_age_minutes, 5);
        assert!((a.features.minutes_after_solar_noon - (15 * 60 + 5 - 13 * 60 - 44)).abs() <= 5);
        assert_eq!(a.features.forecast_rise_tenths, None, "no forecast given");
    }

    #[test]
    fn forecast_rise_comes_only_from_the_same_day() {
        let mut e = TemperatureStateEngine::new(3);
        let st = StationId::new("EHAM").unwrap();
        e.register_station(st.clone(), Amsterdam);
        e.apply_observation(&obs("2026-07-01T12:00:00Z", 180));
        e.apply_observation(&obs("2026-07-01T13:00:00Z", 170));
        let now = utc("2026-07-01T13:05:00Z");
        let date = wm_core::time::local_date(now, Amsterdam);
        let s = e.day_state(&st, date, ViewKind::All, now).unwrap();
        let start = wm_core::time::local_day_start(date, Amsterdam);
        // Forecast: 20.0 °C until 15:00 UTC, then a late warm-up to 21.5 °C.
        let series: Vec<(DateTime<Utc>, i32)> = (0..=24)
            .map(|h| {
                let t = start + chrono::Duration::hours(h);
                (
                    t,
                    if t > utc("2026-07-01T15:00:00Z") {
                        215
                    } else {
                        200
                    },
                )
            })
            .collect();
        let fc = ForecastDay::from_series(date, Amsterdam, &series, utc("2026-07-01T06:00:00Z"))
            .unwrap();
        let engine = PeakDetectionEngine::default();
        let a = engine.assess_with(&s, Amsterdam, now, Some(&fc)).unwrap();
        assert_eq!(a.features.forecast_rise_tenths, Some(15));
        let mut other = fc.clone();
        other.date = date.succ_opt().unwrap();
        let b = engine
            .assess_with(&s, Amsterdam, now, Some(&other))
            .unwrap();
        assert_eq!(
            b.features.forecast_rise_tenths, None,
            "another day's forecast"
        );
        // Everything else is identical with or without the forecast.
        let c = engine.assess(&s, Amsterdam, now).unwrap();
        assert_eq!(b, c);
    }
}
