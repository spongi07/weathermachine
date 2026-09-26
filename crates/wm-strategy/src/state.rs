//! Temperature state engine.
//!
//! Maintains, per station and local calendar day, the ordered series of
//! observations and derives the day's state for one or more *views*:
//!
//! * `All` — every METAR/SPECI.
//! * `Filtered(filter)` — only rows the market's resolution source counts
//!   (e.g. WRH "hourly" rows, minutes :51–:59).
//!
//! Replay and live use exactly this code. State is always derived from the
//! set of observations known *so far*, so a backtest that feeds events in
//! knowledge-time order cannot see the future.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use wm_core::event::CorrectionEvent;
use wm_core::ids::StationId;
use wm_core::resolution::ObservationFilter;
use wm_core::time::{local_date, local_minute_of_day, local_minute_of_hour};
use wm_core::units::TempC;
use wm_core::weather::{Observation, ReportType};

/// Which observations a state is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum ViewKind {
    All,
    Filtered { filter: ObservationFilter },
}

impl ViewKind {
    pub fn label(&self) -> String {
        match self {
            ViewKind::All => "all".into(),
            ViewKind::Filtered { filter } => format!("filtered{}", filter.label()),
        }
    }

    fn admits(&self, p: &ObsPoint) -> bool {
        match self {
            ViewKind::All => true,
            ViewKind::Filtered { filter } => filter.admits_minute(p.local_minute_of_hour),
        }
    }
}

/// One observation point in a day series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsPoint {
    pub observed_at: DateTime<Utc>,
    pub local_minute_of_day: u16,
    pub local_minute_of_hour: u8,
    pub temp: TempC,
    pub report_type: ReportType,
    pub version: u32,
}

/// The day's high for a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighInfo {
    pub value: TempC,
    /// First time the high value was reached.
    pub first_at: DateTime<Utc>,
    /// Most recent time the high value was observed (retests move this).
    pub last_at: DateTime<Utc>,
    /// Number of later observations equal to the high (after first reaching it).
    pub retests: u32,
}

/// Derived state of one day for one view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayState {
    pub station: StationId,
    pub date: NaiveDate,
    pub view: ViewKind,
    pub observation_count: u32,
    pub current: Option<ObsPoint>,
    pub high: Option<HighInfo>,
    /// Minutes of *observed* coverage after the last touch of the high
    /// (`last_observation_at − high.last_at`). A data gap never counts as confirmation.
    pub observed_minutes_since_high: Option<i64>,
    /// Wall-clock minutes since the last touch of the high (`now − high.last_at`).
    pub minutes_since_high: Option<i64>,
    /// `high − current` in tenths.
    pub drop_from_high_tenths: Option<i32>,
    /// Observations after the last touch of the high that are below it.
    pub lower_since_high: u32,
    /// Least-squares slope over the last 90 minutes (°C/h), ≥ 3 points.
    pub slope_c_per_hour: Option<f64>,
    /// Slope(last 60 min) − slope(previous 60 min), °C/h per hour.
    pub accel_c_per_hour2: Option<f64>,
    pub last_observation_at: Option<DateTime<Utc>>,
    /// Points in time order (view-filtered).
    pub points: Vec<ObsPoint>,
}

#[derive(Debug, Clone, Default)]
struct DaySeries {
    points: BTreeMap<(DateTime<Utc>, ReportType), ObsPoint>,
    missing_temperature: u32,
}

#[derive(Debug, Clone)]
struct StationDays {
    tz: Tz,
    days: BTreeMap<NaiveDate, DaySeries>,
}

/// Multi-station temperature state.
#[derive(Debug, Clone, Default)]
pub struct TemperatureStateEngine {
    stations: HashMap<StationId, StationDays>,
    retention_days: i64,
}

fn slope(points: &[ObsPoint]) -> Option<f64> {
    if points.len() < 3 {
        return None;
    }
    let t0 = points[0].observed_at;
    let xs: Vec<f64> = points
        .iter()
        .map(|p| (p.observed_at - t0).num_seconds() as f64 / 3600.0)
        .collect();
    let ys: Vec<f64> = points.iter().map(|p| p.temp.as_f64()).collect();
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if sxx <= f64::EPSILON {
        return None;
    }
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    Some(sxy / sxx)
}

impl TemperatureStateEngine {
    pub fn new(retention_days: i64) -> Self {
        Self {
            stations: HashMap::new(),
            retention_days: retention_days.max(1),
        }
    }

    pub fn register_station(&mut self, station: StationId, tz: Tz) {
        self.stations.entry(station).or_insert_with(|| StationDays {
            tz,
            days: BTreeMap::new(),
        });
    }

    pub fn timezone(&self, station: &StationId) -> Option<Tz> {
        self.stations.get(station).map(|s| s.tz)
    }

    fn point(obs: &Observation, tz: Tz) -> Option<ObsPoint> {
        Some(ObsPoint {
            observed_at: obs.key.observed_at,
            local_minute_of_day: local_minute_of_day(obs.key.observed_at, tz),
            local_minute_of_hour: local_minute_of_hour(obs.key.observed_at, tz),
            temp: obs.temperature?,
            report_type: obs.key.report_type,
            version: obs.version,
        })
    }

    /// Insert (or replace with a newer version) an observation. Returns the
    /// local date it belongs to, or `None` for unknown stations.
    pub fn apply_observation(&mut self, obs: &Observation) -> Option<NaiveDate> {
        let sd = self.stations.get_mut(&obs.key.station)?;
        let date = local_date(obs.key.observed_at, sd.tz);
        let series = sd.days.entry(date).or_default();
        match Self::point(obs, sd.tz) {
            Some(p) => {
                let key = (p.observed_at, p.report_type);
                let replace = series
                    .points
                    .get(&key)
                    .is_none_or(|old| p.version >= old.version);
                if replace {
                    series.points.insert(key, p);
                }
            }
            None => series.missing_temperature += 1,
        }
        Some(date)
    }

    /// Apply a correction: the new version replaces the old point; if the
    /// corrected report has no temperature the point is removed.
    pub fn apply_correction(&mut self, c: &CorrectionEvent) -> Option<NaiveDate> {
        let sd = self.stations.get_mut(&c.current.key.station)?;
        let date = local_date(c.current.key.observed_at, sd.tz);
        let series = sd.days.entry(date).or_default();
        let key = (c.current.key.observed_at, c.current.key.report_type);
        match Self::point(&c.current, sd.tz) {
            Some(p) => {
                series.points.insert(key, p);
            }
            None => {
                series.points.remove(&key);
            }
        }
        Some(date)
    }

    /// Derived state for `station`/`date`/`view` as of `now`.
    pub fn day_state(
        &self,
        station: &StationId,
        date: NaiveDate,
        view: ViewKind,
        now: DateTime<Utc>,
    ) -> Option<DayState> {
        let sd = self.stations.get(station)?;
        let series = sd.days.get(&date);
        let points: Vec<ObsPoint> = series
            .map(|s| {
                s.points
                    .values()
                    .filter(|p| view.admits(p) && p.observed_at <= now)
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        let mut state = DayState {
            station: station.clone(),
            date,
            view,
            observation_count: points.len() as u32,
            current: points.last().copied(),
            high: None,
            observed_minutes_since_high: None,
            minutes_since_high: None,
            drop_from_high_tenths: None,
            lower_since_high: 0,
            slope_c_per_hour: None,
            accel_c_per_hour2: None,
            last_observation_at: points.last().map(|p| p.observed_at),
            points: Vec::new(),
        };
        let mut high: Option<HighInfo> = None;
        for p in &points {
            match high.as_mut() {
                None => {
                    high = Some(HighInfo {
                        value: p.temp,
                        first_at: p.observed_at,
                        last_at: p.observed_at,
                        retests: 0,
                    })
                }
                Some(h) if p.temp > h.value => {
                    *h = HighInfo {
                        value: p.temp,
                        first_at: p.observed_at,
                        last_at: p.observed_at,
                        retests: 0,
                    }
                }
                Some(h) if p.temp == h.value => {
                    h.last_at = p.observed_at;
                    h.retests += 1;
                }
                _ => {}
            }
        }
        if let (Some(h), Some(cur)) = (high, state.current) {
            state.observed_minutes_since_high = Some((cur.observed_at - h.last_at).num_minutes());
            state.minutes_since_high = Some((now - h.last_at).num_minutes().max(0));
            state.drop_from_high_tenths = Some(h.value.diff_tenths(cur.temp));
            state.lower_since_high = points
                .iter()
                .filter(|p| p.observed_at > h.last_at && p.temp < h.value)
                .count() as u32;
        }
        state.high = high;
        if let Some(last) = state.last_observation_at {
            let recent: Vec<ObsPoint> = points
                .iter()
                .filter(|p| (last - p.observed_at).num_minutes() <= 90)
                .copied()
                .collect();
            state.slope_c_per_hour = slope(&recent);
            let last60: Vec<ObsPoint> = points
                .iter()
                .filter(|p| (last - p.observed_at).num_minutes() <= 60)
                .copied()
                .collect();
            let prev60: Vec<ObsPoint> = points
                .iter()
                .filter(|p| {
                    let m = (last - p.observed_at).num_minutes();
                    (60..=120).contains(&m)
                })
                .copied()
                .collect();
            if let (Some(a), Some(b)) = (slope(&last60), slope(&prev60)) {
                state.accel_c_per_hour2 = Some(a - b);
            }
        }
        state.points = points;
        Some(state)
    }

    /// Local dates with data for a station.
    pub fn dates(&self, station: &StationId) -> Vec<NaiveDate> {
        self.stations
            .get(station)
            .map(|s| s.days.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Drop days older than the retention window before `today`.
    pub fn prune(&mut self, today: NaiveDate) {
        let cutoff = today - chrono::Duration::days(self.retention_days);
        for sd in self.stations.values_mut() {
            sd.days.retain(|d, _| *d >= cutoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;
    use wm_core::ids::ProviderId;
    use wm_core::weather::{ObservationKey, QualityFlags, TempPrecision};

    pub(crate) fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    pub(crate) fn obs(t: &str, tenths: i32, version: u32) -> Observation {
        Observation {
            key: ObservationKey {
                station: StationId::new("EHAM").unwrap(),
                observed_at: utc(t),
                report_type: ReportType::Metar,
            },
            version,
            temperature: Some(TempC::from_tenths(tenths)),
            dewpoint: None,
            precision: TempPrecision::WholeDegree,
            raw_text: String::new(),
            content_hash: format!("{t}-{tenths}-{version}"),
            provider: ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: utc(t),
            parser_version: 1,
            quality: QualityFlags::default(),
        }
    }

    fn engine() -> TemperatureStateEngine {
        let mut e = TemperatureStateEngine::new(7);
        e.register_station(StationId::new("EHAM").unwrap(), Amsterdam);
        e
    }

    fn eham() -> StationId {
        StationId::new("EHAM").unwrap()
    }

    fn d() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
    }

    #[test]
    fn prompt_example_trajectory() {
        // 13:40 17.7, 14:00 18.0 (high), 14:20 17.9, 14:40 17.7, 15:00 17.5, 15:20 17.2, 15:40 17.0 (local CEST)
        let mut e = engine();
        for (t, v) in [
            ("11:40", 177),
            ("12:00", 180),
            ("12:20", 179),
            ("12:40", 177),
            ("13:00", 175),
            ("13:20", 172),
            ("13:40", 170),
        ] {
            e.apply_observation(&obs(&format!("2026-07-01T{t}:00Z"), v, 1));
        }
        let s = e
            .day_state(&eham(), d(), ViewKind::All, utc("2026-07-01T13:45:00Z"))
            .unwrap();
        let h = s.high.unwrap();
        assert_eq!(h.value, TempC::from_tenths(180));
        assert_eq!(h.first_at, utc("2026-07-01T12:00:00Z"));
        assert_eq!(s.observed_minutes_since_high, Some(100));
        assert_eq!(s.minutes_since_high, Some(105));
        assert_eq!(s.drop_from_high_tenths, Some(10));
        assert_eq!(s.lower_since_high, 5);
        assert!(s.slope_c_per_hour.unwrap() < -0.5, "declining trajectory");
        assert_eq!(s.observation_count, 7);
    }

    #[test]
    fn new_higher_observation_resets_candidate_and_retests_move_last_touch() {
        let mut e = engine();
        e.apply_observation(&obs("2026-07-01T11:55:00Z", 180, 1));
        e.apply_observation(&obs("2026-07-01T12:25:00Z", 170, 1));
        e.apply_observation(&obs("2026-07-01T12:55:00Z", 180, 1));
        let s = e
            .day_state(&eham(), d(), ViewKind::All, utc("2026-07-01T13:00:00Z"))
            .unwrap();
        let h = s.high.unwrap();
        assert_eq!(h.retests, 1);
        assert_eq!(h.last_at, utc("2026-07-01T12:55:00Z"));
        assert_eq!(s.observed_minutes_since_high, Some(0));
        e.apply_observation(&obs("2026-07-01T13:25:00Z", 190, 1));
        let s = e
            .day_state(&eham(), d(), ViewKind::All, utc("2026-07-01T13:30:00Z"))
            .unwrap();
        assert_eq!(s.high.unwrap().value, TempC::from_whole(19));
        assert_eq!(s.high.unwrap().retests, 0);
    }

    #[test]
    fn filtered_view_only_counts_resolution_rows() {
        let mut e = engine();
        e.apply_observation(&obs("2026-07-01T11:55:00Z", 180, 1));
        e.apply_observation(&obs("2026-07-01T12:25:00Z", 190, 1)); // :25 not in the :51–:59 window
        e.apply_observation(&obs("2026-07-01T12:55:00Z", 180, 1));
        let now = utc("2026-07-01T13:00:00Z");
        let all = e.day_state(&eham(), d(), ViewKind::All, now).unwrap();
        let hourly = e
            .day_state(
                &eham(),
                d(),
                ViewKind::Filtered {
                    filter: ObservationFilter::WRH_HOURLY_NWS_FAA,
                },
                now,
            )
            .unwrap();
        assert_eq!(all.high.unwrap().value, TempC::from_whole(19));
        assert_eq!(hourly.high.unwrap().value, TempC::from_whole(18));
        assert_eq!(hourly.observation_count, 2);
    }

    #[test]
    fn out_of_order_and_corrections_recompute_state() {
        let mut e = engine();
        e.apply_observation(&obs("2026-07-01T12:55:00Z", 180, 1));
        e.apply_observation(&obs("2026-07-01T12:25:00Z", 185, 1)); // late arrival, higher
        let now = utc("2026-07-01T13:00:00Z");
        let s = e.day_state(&eham(), d(), ViewKind::All, now).unwrap();
        assert_eq!(s.high.unwrap().value, TempC::from_tenths(185));
        assert_eq!(s.current.unwrap().observed_at, utc("2026-07-01T12:55:00Z"));
        let corr = CorrectionEvent {
            previous: obs("2026-07-01T12:25:00Z", 185, 1),
            current: obs("2026-07-01T12:25:00Z", 175, 2),
            labeled: true,
        };
        e.apply_correction(&corr);
        let s = e.day_state(&eham(), d(), ViewKind::All, now).unwrap();
        assert_eq!(s.high.unwrap().value, TempC::from_tenths(180));
        // Older version arriving late never overwrites a newer one.
        e.apply_observation(&obs("2026-07-01T12:25:00Z", 185, 1));
        let s = e.day_state(&eham(), d(), ViewKind::All, now).unwrap();
        assert_eq!(s.high.unwrap().value, TempC::from_tenths(180));
    }

    #[test]
    fn as_of_semantics_hide_future_points() {
        let mut e = engine();
        e.apply_observation(&obs("2026-07-01T11:55:00Z", 180, 1));
        e.apply_observation(&obs("2026-07-01T12:55:00Z", 200, 1));
        let s = e
            .day_state(&eham(), d(), ViewKind::All, utc("2026-07-01T12:00:00Z"))
            .unwrap();
        assert_eq!(s.high.unwrap().value, TempC::from_whole(18));
    }

    #[test]
    fn local_day_boundaries_use_station_timezone() {
        let mut e = engine();
        // 23:55 UTC on 30 June is 01:55 CEST on 1 July.
        let date = e
            .apply_observation(&obs("2026-06-30T23:55:00Z", 150, 1))
            .unwrap();
        assert_eq!(date, d());
        e.prune(NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());
        assert!(e.dates(&eham()).is_empty());
    }
}
