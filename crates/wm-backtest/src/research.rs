//! First strategy experiment (blueprint §38):
//!
//! P(observed high remains final | no higher observation for N minutes),
//! N ∈ {30, 45, 60, 75, 90, 105, 120, 150, 180}, stratified by season, local
//! time, drop from high and trajectory — plus training of the empirical model
//! with *exactly* the live feature code (train/serve consistency).
//!
//! HYPOTHESIS TO BACKTEST — not a fact: the probability rises with N, with
//! larger drops, with steady declines and later in the afternoon.

use crate::forecast_eval::{
    EvaluationConfig, ForecastEvaluation, ForecastHistory, PLACEBO_OFFSET_DAYS, Scorer,
};
use crate::selection::{Comparer, SelectionConfig, StructureComparison};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use wm_core::ids::StationId;
use wm_core::resolution::ObservationFilter;
use wm_core::time::{Season, local_date, local_day_bounds};
use wm_core::weather::Observation;
use wm_strategy::probability::{drop_bucket, headroom_bucket, hour_bucket, rise_bucket};
use wm_strategy::{
    CONFIRMATION_WINDOWS, EmpiricalPeakModel, ModelStructure, PeakConfig, PeakDetectionEngine,
    PeakFeatures, PeakTimes, PeakTimesBuilder, TemperatureStateEngine, ViewKind,
};

/// Wilson score interval (95 %).
pub fn wilson(successes: u64, n: u64) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let z = 1.959_963_984_540_054_f64;
    let n = n as f64;
    let p = successes as f64 / n;
    let denom = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denom;
    let half = z * ((p * (1.0 - p) / n) + z * z / (4.0 * n * n)).sqrt() / denom;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

/// One stratum of the survival table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurvivalRow {
    pub window_minutes: u32,
    pub stratum: String,
    pub samples: u64,
    pub final_count: u64,
    pub p_final: f64,
    pub ci_low: f64,
    pub ci_high: f64,
}

/// Result of the peak-survival study.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurvivalReport {
    pub station: String,
    pub view: String,
    pub days: u64,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub rows: Vec<SurvivalRow>,
}

impl SurvivalReport {
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "# Peak survival — {} ({} view)\n\n{} days, {} → {}\n\nP(observed high is final | no higher observation for N minutes). 95 % Wilson intervals.\n\n| N (min) | stratum | samples | final | P(final) | 95 % CI |\n|---:|---|---:|---:|---:|---|\n",
            self.station,
            self.view,
            self.days,
            self.from.map(|d| d.to_string()).unwrap_or_default(),
            self.to.map(|d| d.to_string()).unwrap_or_default()
        );
        for r in &self.rows {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {:.4} | [{:.4}, {:.4}] |\n",
                r.window_minutes,
                r.stratum,
                r.samples,
                r.final_count,
                r.p_final,
                r.ci_low,
                r.ci_high
            ));
        }
        s
    }

    pub fn row(&self, window: u32, stratum: &str) -> Option<&SurvivalRow> {
        self.rows
            .iter()
            .find(|r| r.window_minutes == window && r.stratum == stratum)
    }
}

/// Configuration of the study.
#[derive(Debug, Clone)]
pub struct StudyConfig {
    pub station: StationId,
    pub tz: Tz,
    pub filter: ObservationFilter,
    pub peak: PeakConfig,
    /// Increment classes for the trained model (last = "≥ K−1").
    pub k_classes: usize,
    /// Survival samples only for highs last touched at/after this local minute.
    /// Overnight "highs" (the day starts at local midnight and cools for hours)
    /// would otherwise dominate long windows with almost-never-final samples.
    /// Model training still uses every sample (local time is a model feature).
    pub min_high_local_minute: u16,
}

fn view_of(f: ObservationFilter) -> ViewKind {
    match f {
        ObservationFilter::AllRows => ViewKind::All,
        f => ViewKind::Filtered { filter: f },
    }
}

/// Run the survival study and train an empirical model in one pass.
pub fn study(
    observations: &[Observation],
    cfg: &StudyConfig,
) -> (SurvivalReport, EmpiricalPeakModel) {
    let out = run_study(observations, cfg, None, None);
    (out.report, out.model)
}

/// Result of [`study_with_forecasts`] and [`study_and_select`].
#[derive(Debug, Clone)]
pub struct StudyOutput {
    pub report: SurvivalReport,
    /// The current structure, trained on every day (with a forecast
    /// refinement level when forecasts were given). Whether it may be used
    /// with forecasts is the evaluation's decision
    /// ([`EmpiricalPeakModel::without_refinements`] otherwise).
    pub model: EmpiricalPeakModel,
    pub evaluation: Option<ForecastEvaluation>,
    /// The candidate structure trained in the same pass ([`study_and_select`]).
    pub candidate: Option<CandidateStudy>,
    /// When each season's days first reported their high (also stored in
    /// every model trained here).
    pub peak_times: PeakTimes,
}

/// The candidate structure: its model, its own forecast evaluation (with
/// its forecast input) and the walk-forward comparison with the current one.
#[derive(Debug, Clone)]
pub struct CandidateStudy {
    pub model: EmpiricalPeakModel,
    pub evaluation: Option<ForecastEvaluation>,
    pub comparison: StructureComparison,
}

impl StudyOutput {
    /// The structure the comparison selected — the candidate only when it
    /// was adopted — with that structure's forecast evaluation.
    pub fn selected(&self) -> (&EmpiricalPeakModel, Option<&ForecastEvaluation>) {
        match &self.candidate {
            Some(c) if c.comparison.adopted => (&c.model, c.evaluation.as_ref()),
            _ => (&self.model, self.evaluation.as_ref()),
        }
    }
}

/// [`study`] joined with a fixed-lead forecast history: the survival table
/// gains forecast strata, the model a forecast refinement level, and a
/// prequential evaluation measures whether the forecast improves decisions.
pub fn study_with_forecasts(
    observations: &[Observation],
    cfg: &StudyConfig,
    forecasts: &ForecastHistory,
    eval: EvaluationConfig,
) -> StudyOutput {
    run_study(observations, cfg, Some((forecasts, eval)), None)
}

/// [`study`] (with forecasts when given) plus the candidate structure,
/// trained in the same pass and compared walk-forward
/// ([`crate::selection`]). Each structure's forecast input is evaluated
/// separately, with its own placebo.
pub fn study_and_select(
    observations: &[Observation],
    cfg: &StudyConfig,
    forecasts: Option<(&ForecastHistory, EvaluationConfig)>,
    selection: SelectionConfig,
) -> StudyOutput {
    run_study(observations, cfg, forecasts, Some(selection))
}

/// One structure being trained: its model, and with forecasts a placebo
/// model (same levels, trained with the placebo's forecast inputs) and the
/// forecast scorer.
struct Arm {
    model: EmpiricalPeakModel,
    placebo: Option<EmpiricalPeakModel>,
    scorer: Option<Scorer>,
}

impl Arm {
    fn new(
        structure: ModelStructure,
        cfg: &StudyConfig,
        view: ViewKind,
        eval: Option<&EvaluationConfig>,
        with_forecast: bool,
    ) -> Self {
        let levels = if with_forecast {
            EmpiricalPeakModel::with_refinement(structure.levels(), structure.refinement())
        } else {
            structure.levels()
        };
        let suffix = match structure {
            ModelStructure::Current => "",
            ModelStructure::Candidate => "-first-reach",
        };
        let model = EmpiricalPeakModel::new(
            format!("empirical-{}-{}{suffix}", cfg.station, view.label()),
            cfg.station.to_string(),
            view.label(),
            cfg.k_classes,
            levels,
        );
        let scorer = eval.map(|e| Scorer::new(e.clone(), structure.refinement()));
        Self {
            placebo: scorer.as_ref().map(|_| model.clone()),
            model,
            scorer,
        }
    }
}

fn run_study(
    observations: &[Observation],
    cfg: &StudyConfig,
    forecasts: Option<(&ForecastHistory, EvaluationConfig)>,
    selection: Option<SelectionConfig>,
) -> StudyOutput {
    let view = view_of(cfg.filter);
    let mut engine = TemperatureStateEngine::new(3);
    engine.register_station(cfg.station.clone(), cfg.tz);
    let peak = PeakDetectionEngine::new(cfg.peak.clone());
    let (forecasts, eval) = match forecasts {
        Some((h, e)) => (Some(h), Some(e)),
        None => (None, None),
    };
    let arm = |s| Arm::new(s, cfg, view, eval.as_ref(), forecasts.is_some());
    let mut current = arm(ModelStructure::Current);
    let mut candidate = selection.as_ref().map(|_| arm(ModelStructure::Candidate));
    let mut comparer = selection.map(Comparer::new);
    let mut days_with_forecast = 0u64;

    let mut by_day: BTreeMap<NaiveDate, Vec<&Observation>> = BTreeMap::new();
    for o in observations.iter().filter(|o| o.key.station == cfg.station) {
        by_day
            .entry(local_date(o.key.observed_at, cfg.tz))
            .or_default()
            .push(o);
    }
    // (window, stratum) → (samples, finals)
    let mut table: BTreeMap<(u32, String), (u64, u64)> = BTreeMap::new();
    let mut peaks = PeakTimesBuilder::new();
    let mut days = 0u64;
    for (date, obs) in &by_day {
        for o in obs {
            engine.apply_observation(o);
        }
        let (_, end) = local_day_bounds(*date, cfg.tz);
        let Some(final_state) = engine.day_state(&cfg.station, *date, view, end) else {
            continue;
        };
        let Some(final_high) = final_state.high.map(|h| h.value.round_half_up_whole()) else {
            continue;
        };
        days += 1;
        peaks.add_day(
            *date,
            Season::from_month(date.month(), cfg.peak.southern_hemisphere),
            &final_state.points,
            final_high,
        );
        let forecast = forecasts.and_then(|h| h.days.get(date));
        days_with_forecast += u64::from(forecast.is_some());
        let placebo = match (eval.is_some(), forecasts) {
            (true, Some(h)) => h.placebo_day(*date, cfg.tz, PLACEBO_OFFSET_DAYS),
            _ => None,
        };
        let compare_today = comparer.as_ref().is_some_and(|c| c.scores_day(days));
        // Evaluate as of each eligible observation time (knowledge-consistent).
        // The day is scored before it is learned: no day informs itself.
        let times: Vec<DateTime<Utc>> = final_state.points.iter().map(|p| p.observed_at).collect();
        let mut used: BTreeSet<(u32, DateTime<Utc>)> = BTreeSet::new();
        let mut samples = Vec::with_capacity(times.len());
        for t in times {
            let Some(s) = engine.day_state(&cfg.station, *date, view, t) else {
                continue;
            };
            let Some(a) = peak.assess_with(&s, cfg.tz, t, forecast) else {
                continue;
            };
            let f = a.features;
            let increment = final_high - f.high_whole;
            // The same moment with the placebo forecast's inputs.
            let placebo_f = PeakFeatures {
                forecast_rise_tenths: placebo.as_ref().and_then(|p| p.rise_tenths(t)),
                forecast_headroom_tenths: placebo
                    .as_ref()
                    .and_then(|p| p.remaining_max_tenths(t))
                    .map(|m| m - f.high_tenths),
                ..f.clone()
            };
            for w in CONFIRMATION_WINDOWS {
                // One sample per (window, high-touch): the first observation whose
                // observed coverage since the high reaches the window.
                if f.high_local_minute >= cfg.min_high_local_minute
                    && f.window_met(w)
                    && used.insert((w, f.high_at))
                {
                    let is_final = final_high == f.high_whole;
                    let mut strata = vec![
                        "all".to_owned(),
                        format!("season={}", f.season.as_str()),
                        format!("hour={}", hour_bucket(f.local_minute_now)),
                        format!("drop={}", drop_bucket(f.drop_tenths)),
                        format!("trajectory={}", f.trajectory.as_str()),
                    ];
                    if let Some(r) = f.forecast_rise_tenths {
                        strata.push(format!("forecast_rise={}", rise_bucket(r)));
                    }
                    if let Some(h) = f.forecast_headroom_tenths {
                        strata.push(format!("forecast_headroom={}", headroom_bucket(h)));
                    }
                    for st in strata {
                        let e = table.entry((w, st)).or_insert((0, 0));
                        e.0 += 1;
                        e.1 += u64::from(is_final);
                    }
                    for arm in std::iter::once(&mut current).chain(candidate.as_mut()) {
                        if let (Some(sc), Some(pm)) = (arm.scorer.as_mut(), arm.placebo.as_ref())
                            && w == sc.window()
                        {
                            sc.score(*date, &arm.model, pm, &f, &placebo_f, increment);
                        }
                    }
                }
            }
            if compare_today
                && let (Some(c), Some(cand)) = (comparer.as_mut(), candidate.as_ref())
                && c.in_window(&f)
            {
                c.score(*date, &current.model, &cand.model, &f, increment);
            }
            samples.push((f, placebo_f, increment));
        }
        for (f, placebo_f, increment) in &samples {
            for arm in std::iter::once(&mut current).chain(candidate.as_mut()) {
                arm.model.observe(f, *increment);
                if let Some(pm) = arm.placebo.as_mut() {
                    pm.observe(placebo_f, *increment);
                }
            }
        }
        engine.prune(*date);
    }
    let rows = table
        .into_iter()
        .map(|((w, stratum), (n, k))| {
            let (lo, hi) = wilson(k, n);
            SurvivalRow {
                window_minutes: w,
                stratum,
                samples: n,
                final_count: k,
                p_final: if n == 0 { 0.0 } else { k as f64 / n as f64 },
                ci_low: lo,
                ci_high: hi,
            }
        })
        .collect();
    let from = by_day.keys().next().copied();
    let to = by_day.keys().next_back().copied();
    let peak_times = peaks.build();
    let finish = |mut arm: Arm| {
        if let (Some(f), Some(t)) = (from, to) {
            arm.model.trained_from = f;
            arm.model.trained_to = t;
        }
        arm.model.peak_times = Some(peak_times.clone());
        let evaluation = match (arm.scorer, forecasts) {
            (Some(sc), Some(h)) => Some(sc.finish(&h.product, days_with_forecast)),
            _ => None,
        };
        (arm.model, evaluation)
    };
    let (model, evaluation) = finish(current);
    let adopted = |e: &Option<ForecastEvaluation>| e.as_ref().is_some_and(|e| e.adopted);
    let candidate = match (candidate.map(finish), comparer) {
        (Some((cand, cand_eval)), Some(c)) => Some(CandidateStudy {
            comparison: c.finish(adopted(&evaluation), adopted(&cand_eval)),
            model: cand,
            evaluation: cand_eval,
        }),
        _ => None,
    };
    StudyOutput {
        report: SurvivalReport {
            station: cfg.station.to_string(),
            view: view.label(),
            days,
            from,
            to,
            rows,
        },
        model,
        evaluation,
        candidate,
        peak_times,
    }
}

/// Walk-forward split: train on `train_days`, skip `embargo_days`, test on
/// `test_days`, roll forward by `test_days`. No test date is ever before its
/// training window ends (no look-ahead), and embargo days separate them.
pub fn walk_forward_splits(
    dates: &[NaiveDate],
    train_days: usize,
    embargo_days: usize,
    test_days: usize,
) -> Vec<(Vec<NaiveDate>, Vec<NaiveDate>)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start + train_days + embargo_days + test_days <= dates.len() {
        let train = dates[start..start + train_days].to_vec();
        let test_start = start + train_days + embargo_days;
        let test = dates[test_start..test_start + test_days].to_vec();
        out.push((train, test));
        start += test_days;
    }
    out
}

/// Dates within `[from, to]`.
pub fn date_range(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let mut v = Vec::new();
    let mut d = from;
    while d <= to {
        v.push(d);
        d += Duration::days(1);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::synthetic_history;
    use wm_strategy::ProbabilityModel;

    #[test]
    fn wilson_interval_properties() {
        let (lo, hi) = wilson(95, 100);
        assert!(lo < 0.95 && hi > 0.95 && hi <= 1.0);
        let (lo, hi) = wilson(0, 0);
        assert_eq!((lo, hi), (0.0, 1.0));
        let (lo, _) = wilson(1000, 1000);
        assert!(lo > 0.99);
    }

    #[test]
    fn study_on_synthetic_history() {
        let st = StationId::new("EHAM").unwrap();
        let obs = synthetic_history(
            &st,
            NaiveDate::from_ymd_opt(2025, 6, 1).unwrap(),
            60,
            11,
            Duration::minutes(5),
        );
        let cfg = StudyConfig {
            station: st,
            tz: chrono_tz::Europe::Amsterdam,
            filter: ObservationFilter::AllRows,
            peak: PeakConfig::default(),
            k_classes: 4,
            min_high_local_minute: 9 * 60,
        };
        let (report, model) = study(&obs, &cfg);
        assert_eq!(report.days, 60);
        let r30 = report.row(30, "all").unwrap();
        let r180 = report.row(180, "all").unwrap();
        assert!(r30.samples > 0 && r180.samples > 0);
        // On a smooth synthetic diurnal cycle, longer confirmation ⇒ more often final.
        assert!(
            r180.p_final >= r30.p_final,
            "30: {} 180: {}",
            r30.p_final,
            r180.p_final
        );
        assert!(report.to_markdown().contains("| 180 | all |"));
        assert!(model.total_samples() > 1000);
        assert_eq!(
            model.trained_from,
            NaiveDate::from_ymd_opt(2025, 6, 1).unwrap()
        );
        // The trained model produces a normalized distribution for live features.
        let f = crate::research::tests::any_features(&model);
        assert!(f.is_some());
    }

    pub(crate) fn any_features(
        model: &EmpiricalPeakModel,
    ) -> Option<wm_strategy::IncrementDistribution> {
        let st = StationId::new("EHAM").unwrap();
        let obs = synthetic_history(
            &st,
            NaiveDate::from_ymd_opt(2025, 8, 1).unwrap(),
            1,
            3,
            Duration::minutes(5),
        );
        let mut e = TemperatureStateEngine::new(3);
        e.register_station(st.clone(), chrono_tz::Europe::Amsterdam);
        for o in &obs {
            e.apply_observation(o);
        }
        let t = obs[30].key.observed_at;
        let s = e.day_state(
            &st,
            local_date(t, chrono_tz::Europe::Amsterdam),
            ViewKind::All,
            t,
        )?;
        let a = PeakDetectionEngine::default().assess(&s, chrono_tz::Europe::Amsterdam, t)?;
        let d = model.distribution(&a.features)?;
        assert!((d.probs.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        Some(d)
    }

    #[test]
    fn walk_forward_has_no_overlap_or_lookahead() {
        let dates = date_range(
            NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
        );
        let splits = walk_forward_splits(&dates, 120, 7, 30);
        assert!(splits.len() >= 7);
        for (train, test) in &splits {
            let last_train = *train.last().unwrap();
            let first_test = *test.first().unwrap();
            assert!(first_test > last_train + Duration::days(7));
            assert_eq!(test.len(), 30);
        }
    }
}
