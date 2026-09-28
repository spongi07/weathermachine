#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The structure comparison on synthetic weather with a known truth.
//!
//! * "Plateau world": the high is reached late in the morning and then held
//!   exactly for a while; on half of the days it rises one degree more after
//!   a random wait. The longer a plateau has lasted, the more likely it is
//!   final — visible to the first-reached clock, invisible to the last-touch
//!   clock (which restarts at every report equal to the high). The candidate
//!   structure must be adopted.
//! * "Ramp world": strictly rising to a single peak, then strictly falling —
//!   no report ever repeats the high, so both clocks agree; scored only from
//!   noon, where both hour splits agree. The predictions must be identical
//!   and nothing may change.

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Europe::Amsterdam;
use wm_backtest::{
    EvaluationConfig, ForecastHistory, SelectionConfig, StudyConfig, study, study_and_select,
};
use wm_core::forecast::ForecastProduct;
use wm_core::ids::{ProviderId, StationId};
use wm_core::resolution::ObservationFilter;
use wm_core::rng::SplitMix64;
use wm_core::time::local_day_bounds;
use wm_core::units::TempC;
use wm_core::weather::{Observation, ObservationKey, QualityFlags, ReportType, TempPrecision};
use wm_strategy::{ModelStructure, PeakConfig, ProbabilityModel};

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn cfg() -> StudyConfig {
    StudyConfig {
        station: eham(),
        tz: Amsterdam,
        filter: ObservationFilter::AllRows,
        peak: PeakConfig::default(),
        k_classes: 4,
        min_high_local_minute: 9 * 60,
    }
}

fn selection(burn_in_days: u64, min_days: u64) -> SelectionConfig {
    SelectionConfig {
        burn_in_days,
        min_days,
        bootstrap_iterations: 400,
        ..SelectionConfig::default()
    }
}

fn local_minute(t: DateTime<Utc>) -> i32 {
    let l = t.with_timezone(&Amsterdam);
    (l.hour() * 60 + l.minute()) as i32
}

/// Whole-degree reports at HH:25 and HH:55 UTC of `temp(local minute)`.
fn reports(date: NaiveDate, temp: impl Fn(i32) -> i32) -> Vec<Observation> {
    let (start, end) = local_day_bounds(date, Amsterdam);
    let mut t = start + Duration::minutes(25);
    let mut out = Vec::new();
    while t < end {
        let v = temp(local_minute(t));
        out.push(Observation {
            key: ObservationKey {
                station: eham(),
                observed_at: t,
                report_type: ReportType::Metar,
            },
            version: 1,
            temperature: Some(TempC::from_whole(v)),
            dewpoint: None,
            precision: TempPrecision::WholeDegree,
            raw_text: format!("EHAM {} {v:02}/05 Q1015", t.format("%d%H%MZ")),
            content_hash: format!("{t}"),
            provider: ProviderId::synthetic(),
            provider_receipt_at: None,
            fetched_at: t + Duration::minutes(5),
            parser_version: 1,
            quality: QualityFlags::default(),
        });
        t += Duration::minutes(30);
    }
    out
}

#[derive(Clone, Copy)]
struct PlateauDay {
    date: NaiveDate,
    high: i32,
    /// Local minute the high is first reached.
    reached: i32,
    /// `Some(wait)`: one degree more after `wait` minutes on the plateau.
    rise_after: Option<i32>,
    /// Plateau length on days without a rise.
    hold: i32,
}

impl PlateauDay {
    fn new(date: NaiveDate, rng: &mut SplitMix64) -> Self {
        let pick = |rng: &mut SplitMix64, lo: i32, hi: i32| {
            lo + (rng.next_f64() * f64::from(hi - lo)).floor() as i32
        };
        Self {
            date,
            high: pick(rng, 14, 24),
            reached: pick(rng, 10 * 60, 12 * 60),
            rise_after: (rng.next_f64() < 0.5).then(|| pick(rng, 30, 240)),
            hold: pick(rng, 60, 240),
        }
    }

    /// Temperature (whole °C) at a local minute.
    fn temp(&self, m: i32) -> i32 {
        let (h, t1) = (self.high, self.reached);
        if m < t1 {
            // One degree per hour up to the high.
            return h - ((t1 - m + 59) / 60).min(8);
        }
        match self.rise_after {
            None if m < t1 + self.hold => h,
            None => h - 1 - (m - t1 - self.hold) / 60,
            Some(d) if m < t1 + d => h,
            Some(d) if m < t1 + d + 90 => h + 1,
            Some(d) => h - (m - t1 - d - 90) / 60,
        }
    }

    /// The true curve as an hourly "forecast" (tenths), knowing the rise.
    fn hourly(&self) -> Vec<(DateTime<Utc>, i32)> {
        let (start, end) = local_day_bounds(self.date, Amsterdam);
        let mut t = start - Duration::hours(1);
        let mut v = Vec::new();
        while t <= end + Duration::hours(1) {
            v.push((t, self.temp(local_minute(t)) * 10));
            t += Duration::hours(1);
        }
        v
    }
}

fn plateau_days(n: u32, seed: u64) -> Vec<PlateauDay> {
    let mut rng = SplitMix64::new(seed);
    let from = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();
    (0..n)
        .map(|i| PlateauDay::new(from + Duration::days(i64::from(i)), &mut rng))
        .collect()
}

fn plateau_observations(ds: &[PlateauDay]) -> Vec<Observation> {
    ds.iter()
        .flat_map(|d| reports(d.date, |m| d.temp(m)))
        .collect()
}

#[test]
fn the_first_reached_clock_is_adopted_where_highs_plateau() {
    let ds = plateau_days(420, 7);
    let obs = plateau_observations(&ds);
    let out = study_and_select(&obs, &cfg(), None, selection(60, 200));
    let cand = out.candidate.as_ref().unwrap();
    let c = &cand.comparison;
    assert!(c.adopted, "{}\n{}", c.verdict, c.to_markdown());
    assert!(c.diff < 0.0 && c.diff_ci_high < 0.0, "{}", c.verdict);
    assert!(c.verdict.starts_with("candidate structure adopted"));
    assert!(c.scored_days >= 350 && c.points > c.scored_days * 10);
    // The gain comes from plateaus, where the last-touch clock sees nothing.
    let row = |g: &str| c.rows.iter().find(|r| r.group == g).unwrap();
    let plateau = row("high repeated (plateau)");
    assert!(
        plateau.logloss_candidate + 0.02 < plateau.logloss_current,
        "{plateau:?}"
    );
    // Only trading hours after the burn-in are scored.
    let first = c.first_scored.unwrap();
    assert_eq!(first, ds[60].date, "day 61 is the first scored day");
    assert!(c.rows.iter().all(|r| !r.group.contains("h<09")
        && !r.group.contains("h09")
        && !r.group.contains("h18+")));
    // The selected model is the candidate, with the candidate's cells.
    let (m, e) = out.selected();
    assert_eq!(m.structure(), ModelStructure::Candidate);
    assert!(e.is_none(), "no forecasts were given");
    assert!(m.id.ends_with("-first-reach"), "{}", m.id);
    assert!(m.cells.keys().any(|k| k.contains("MinutesSinceFirstHigh=")));
    assert!(m.cells.keys().any(|k| k.contains("LocalHourFine=h11")));
    // The current structure is still the one `study` trains.
    let (_, plain) = study(&obs, &cfg());
    let mut current = out.model.clone();
    current.created_at = plain.created_at;
    assert_eq!(current, plain);
    let md = c.to_markdown();
    assert!(md.contains("Verdict: candidate structure adopted") && md.contains("| hour h11 |"));
}

#[test]
fn identical_predictions_change_nothing() {
    // Ramp world: rise 1 °C per report to a peak report between 12:00 and
    // 16:00, then fall 1 °C per report. No DST change in the range, so every
    // report has its own local time and no value repeats the high.
    let mut rng = SplitMix64::new(11);
    let from = NaiveDate::from_ymd_opt(2024, 4, 1).unwrap();
    let obs: Vec<Observation> = (0..200)
        .flat_map(|i| {
            let peak = (12 * 60 + (rng.next_f64() * 240.0) as i32 - 25) / 30;
            let top = 30 + (rng.next_f64() * 10.0) as i32;
            reports(from + Duration::days(i), move |m| {
                top - ((m - 25) / 30 - peak).abs()
            })
        })
        .collect();
    let sel = SelectionConfig {
        from_local_minute: 12 * 60,
        ..selection(30, 100)
    };
    let out = study_and_select(&obs, &cfg(), None, sel);
    let c = out.candidate.unwrap().comparison;
    assert!(c.points > 1000, "{}", c.points);
    assert_eq!(c.diff, 0.0);
    assert_eq!((c.diff_ci_low, c.diff_ci_high), (0.0, 0.0));
    assert_eq!(c.logloss_current, c.logloss_candidate);
    assert!(!c.adopted);
    assert!(
        c.verdict
            .starts_with("current structure kept: no clear improvement"),
        "{}",
        c.verdict
    );
    assert!(c.rows.iter().all(|r| r.group != "high repeated (plateau)"));
}

#[test]
fn a_short_history_keeps_the_current_structure() {
    let ds = plateau_days(90, 3);
    let obs = plateau_observations(&ds);
    let out = study_and_select(&obs, &cfg(), None, selection(60, 365));
    let c = &out.candidate.as_ref().unwrap().comparison;
    assert!(!c.adopted);
    assert_eq!(c.scored_days, 30);
    assert_eq!(
        c.verdict,
        "current structure kept: 30 scored days, 365 needed"
    );
    assert_eq!(out.selected().0.structure(), ModelStructure::Current);
    // A burn-in longer than the history scores nothing.
    let out = study_and_select(&obs, &cfg(), None, selection(365, 30));
    let c = out.candidate.unwrap().comparison;
    assert_eq!((c.points, c.scored_days, c.adopted), (0, 0, false));
}

#[test]
fn each_structure_evaluates_its_own_forecast_input() {
    let ds = plateau_days(420, 5);
    let obs = plateau_observations(&ds);
    let hourly: Vec<(DateTime<Utc>, i32)> = ds.iter().flat_map(PlateauDay::hourly).collect();
    let h = ForecastHistory::from_hourly(
        ForecastProduct {
            provider: ProviderId::open_meteo(),
            model: "test".into(),
            lead_days: 1,
            ready_local_minute: 480,
        },
        Amsterdam,
        &hourly,
    );
    let eval = EvaluationConfig {
        bootstrap_iterations: 300,
        min_days: 150,
        ..EvaluationConfig::default()
    };
    let out = study_and_select(&obs, &cfg(), Some((&h, eval)), selection(60, 200));
    let current = out.evaluation.as_ref().unwrap();
    let cand = out.candidate.as_ref().unwrap();
    let cand_eval = cand.evaluation.as_ref().unwrap();
    assert_eq!(current.feature, "rise");
    assert_eq!(cand_eval.feature, "headroom");
    // The forecast knows whether the day will rise: both inputs carry it,
    // and both beat their placebo.
    assert!(current.adopted, "{}", current.verdict);
    assert!(cand_eval.adopted, "{}", cand_eval.verdict);
    // Buckets follow the input: at 60′ decision points in this world the
    // high is always final and the true curve is below it.
    let buckets: Vec<&str> = cand_eval.rise.iter().map(|r| r.bucket.as_str()).collect();
    assert!(!buckets.is_empty(), "{buckets:?}");
    assert!(
        buckets
            .iter()
            .all(|b| ["below", "level", "above1", "above2"].contains(b)),
        "{buckets:?}"
    );
    assert!(
        current
            .rise
            .iter()
            .all(|r| ["cool2", "cool", "flat", "warm"].contains(&r.bucket.as_str()))
    );
    assert!(cand_eval.placebo_points > 0 && cand_eval.placebo_diff_ci_high < 0.0);
    // The comparison used the forecast inputs, as the service would.
    assert!(cand.comparison.current_forecast && cand.comparison.candidate_forecast);
    assert!(out.model.cells.keys().any(|k| k.contains("ForecastRise=")));
    assert!(
        cand.model
            .cells
            .keys()
            .any(|k| k.contains("ForecastHeadroom="))
    );
    assert!(!cand.model.cells.keys().any(|k| k.contains("ForecastRise=")));
    assert_eq!(
        cand.model.structure().refinement().forecast_name(),
        "headroom"
    );
    let md = cand_eval.to_markdown();
    assert!(
        md.contains("by forecast headroom") && md.contains("| below |"),
        "{md}"
    );
    // The selected pair is consistent.
    let (m, e) = out.selected();
    assert_eq!(
        m.structure() == ModelStructure::Candidate,
        cand.comparison.adopted
    );
    assert_eq!(
        e.unwrap().feature,
        m.structure().refinement().forecast_name()
    );
    // A live prediction on the candidate with a forecast reads the headroom cell.
    let f = m
        .cells
        .keys()
        .find(|k| k.contains("ForecastHeadroom=level"));
    assert!(f.is_some() || m.structure() == ModelStructure::Current);
    assert!(
        m.forecast_product().is_none(),
        "adoption is recorded by training"
    );
}
