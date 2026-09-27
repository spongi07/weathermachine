#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The forecast evaluation on synthetic weather with a known truth: some days
//! bring a late-evening warm surge that no observation-based feature can see
//! coming. A forecast that knows about the surge must be adopted; forecasts
//! unrelated to the day, or not yet known at decision time, must not.

use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Europe::Amsterdam;
use wm_backtest::{EvaluationConfig, ForecastHistory, StudyConfig, study, study_with_forecasts};
use wm_core::forecast::ForecastProduct;
use wm_core::ids::{ProviderId, StationId};
use wm_core::resolution::ObservationFilter;
use wm_core::rng::SplitMix64;
use wm_core::time::local_day_bounds;
use wm_core::units::TempC;
use wm_core::weather::{Observation, ObservationKey, QualityFlags, ReportType, TempPrecision};
use wm_strategy::PeakConfig;

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

#[derive(Clone, Copy)]
struct Day {
    date: NaiveDate,
    min: f64,
    max: f64,
    peak_hour: f64,
    /// Evening surge amplitude (tenths) at 18:30 local, 0 = none.
    surge: f64,
    seed: u64,
}

impl Day {
    fn new(date: NaiveDate, rng: &mut SplitMix64) -> Self {
        let max = 180.0 + (rng.next_f64() - 0.5) * 80.0;
        Self {
            date,
            min: max - 70.0 - rng.next_f64() * 30.0,
            max,
            peak_hour: 13.5 + rng.next_f64() * 2.0,
            surge: if rng.next_f64() < 0.35 {
                25.0 + rng.next_f64() * 15.0
            } else {
                0.0
            },
            seed: rng.next_u64(),
        }
    }

    /// Temperature (tenths) at a local fractional hour.
    fn signal(&self, hour: f64) -> f64 {
        let amp = (self.max - self.min) / 2.0;
        let mid = (self.max + self.min) / 2.0;
        let x = hour - self.peak_hour;
        let phase = if x <= 0.0 { x / 9.0 } else { x / 13.0 };
        let surge = self.surge * (-((hour - 18.5) / 1.2).powi(2)).exp();
        mid + amp * (std::f64::consts::PI * phase).cos() + surge
    }

    fn local_hour(t: DateTime<Utc>) -> f64 {
        let l = t.with_timezone(&Amsterdam);
        f64::from(l.hour()) + f64::from(l.minute()) / 60.0
    }

    /// Whole-degree reports at HH:25 and HH:55 UTC.
    fn observations(&self) -> Vec<Observation> {
        let (start, end) = local_day_bounds(self.date, Amsterdam);
        let mut rng = SplitMix64::new(self.seed);
        let mut t = start + Duration::minutes(25);
        let mut out = Vec::new();
        while t < end {
            let noise = (rng.next_f64() - 0.5) * 6.0;
            let v = TempC::from_tenths((self.signal(Self::local_hour(t)) + noise).round() as i32)
                .round_half_up_whole();
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

    /// Hourly values (tenths) over the local day with a margin, from `f`.
    fn hourly(&self, f: impl Fn(f64) -> f64) -> Vec<(DateTime<Utc>, i32)> {
        let (start, end) = local_day_bounds(self.date, Amsterdam);
        let mut t = start - Duration::hours(1);
        let mut v = Vec::new();
        while t <= end + Duration::hours(1) {
            v.push((t, f(Self::local_hour(t)).round() as i32));
            t += Duration::hours(1);
        }
        v
    }
}

fn days(n: u32) -> Vec<Day> {
    let mut rng = SplitMix64::new(2026);
    let from = NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();
    (0..n)
        .map(|i| Day::new(from + Duration::days(i64::from(i)), &mut rng))
        .collect()
}

fn product(ready_local_minute: u16) -> ForecastProduct {
    ForecastProduct {
        provider: ProviderId::open_meteo(),
        model: "test".into(),
        lead_days: 1,
        ready_local_minute,
    }
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

fn eval_cfg() -> EvaluationConfig {
    EvaluationConfig {
        bootstrap_iterations: 400,
        min_days: 200,
        ..EvaluationConfig::default()
    }
}

fn observations(ds: &[Day]) -> Vec<Observation> {
    ds.iter().flat_map(Day::observations).collect()
}

fn history(
    ds: &[Day],
    ready: u16,
    f: impl Fn(&Day) -> Vec<(DateTime<Utc>, i32)>,
) -> ForecastHistory {
    let hourly: Vec<(DateTime<Utc>, i32)> = ds.iter().flat_map(f).collect();
    ForecastHistory::from_hourly(product(ready), Amsterdam, &hourly)
}

#[test]
fn an_informative_forecast_is_adopted_and_beats_its_placebo() {
    let ds = days(720);
    let obs = observations(&ds);
    // The forecast knows each day's true curve, including the evening surge.
    let h = history(&ds, 480, |d| d.hourly(|x| d.signal(x)));
    assert!(h.days.len() >= 715, "{}", h.days.len());
    let out = study_with_forecasts(&obs, &cfg(), &h, eval_cfg());
    let e = out.evaluation.unwrap();
    assert!(e.adopted, "{}\n{}", e.verdict, e.to_markdown());
    assert!(e.logloss_diff < 0.0 && e.logloss_diff_ci_high < 0.0);
    assert!(
        e.placebo_diff_ci_high < 0.0,
        "placebo must do worse: {}",
        e.verdict
    );
    assert!(e.scored_days >= 200 && e.points > e.scored_days / 2);
    // Warming forecast ⇒ the high is less often final than with cooling ahead.
    let p = |b: &str| e.rise.iter().find(|r| r.bucket == b).map(|r| r.p_final);
    let (warm, cool) = (p("warm").unwrap(), p("cool2").or(p("cool")).unwrap());
    assert!(warm + 0.1 < cool, "warm {warm} vs cool {cool}");
    // The survival table gains forecast strata and the model its refinement.
    assert!(
        out.report
            .rows
            .iter()
            .any(|r| r.stratum.starts_with("forecast_rise="))
    );
    assert!(out.model.uses_forecast());
    let md = e.to_markdown();
    assert!(
        md.contains("Verdict: adopted") && md.contains("Placebo control"),
        "{md}"
    );
}

#[test]
fn a_forecast_unrelated_to_the_day_is_not_adopted() {
    let ds = days(720);
    let obs = observations(&ds);
    // Each day gets the curve of a *different* random day: realistic shapes,
    // surges included, but no information about the day itself.
    let mut rng = SplitMix64::new(77);
    let fake: Vec<Day> = ds
        .iter()
        .map(|d| Day {
            date: d.date,
            ..Day::new(d.date, &mut rng)
        })
        .collect();
    let h = history(&fake, 480, |d| d.hourly(|x| d.signal(x)));
    let e = study_with_forecasts(&obs, &cfg(), &h, eval_cfg())
        .evaluation
        .unwrap();
    assert!(!e.adopted, "{}", e.verdict);
    assert!(
        e.scored_days >= 200,
        "enough data — rejected on the evidence: {}",
        e.verdict
    );
}

#[test]
fn a_forecast_known_only_after_the_decisions_is_never_used() {
    let ds = days(300);
    let obs = observations(&ds);
    // Informative, but ready only at 23:00 local: after every daytime decision.
    let h = history(&ds, 23 * 60, |d| d.hourly(|x| d.signal(x)));
    let out = study_with_forecasts(&obs, &cfg(), &h, eval_cfg());
    let e = out.evaluation.unwrap();
    assert_eq!((e.scored_days, e.points), (0, 0));
    assert!(
        !e.adopted && e.verdict.contains("0 scored days"),
        "{}",
        e.verdict
    );
    // Refinement cells hold only samples from 23:00 local on.
    let late = out
        .model
        .cells
        .keys()
        .filter(|k| k.contains("ForecastRise="))
        .all(|k| k.contains("LocalHour=h18+"));
    assert!(late);
}

#[test]
fn no_day_learns_from_itself_and_the_base_model_is_unchanged() {
    let ds = days(120);
    let obs = observations(&ds);
    // Only one day has a forecast: its decisions are scored before anything
    // about forecasts was learned, so both predictions must be identical.
    let h = history(&ds[100..101], 480, |d| d.hourly(|x| d.signal(x)));
    assert_eq!(h.days.len(), 1);
    let out = study_with_forecasts(
        &obs,
        &cfg(),
        &h,
        EvaluationConfig {
            min_days: 1,
            ..eval_cfg()
        },
    );
    let e = out.evaluation.unwrap();
    assert!(e.points > 0);
    assert_eq!(e.logloss_diff, 0.0);
    assert!(!e.adopted);
    // Stripped of its refinement, the model is exactly the one `study` trains.
    let (_, plain) = study(&obs, &cfg());
    let mut stripped = out.model.without_refinements();
    stripped.created_at = plain.created_at;
    assert_eq!(stripped, plain);
}
