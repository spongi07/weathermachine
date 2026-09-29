//! Probability-model training from real METAR history, on the deployment host.
//!
//! 1. Download the station's METARs from the IEM archive, one station-year per
//!    request through the rate-limited `iem` gate. Finished years are cached
//!    under `research/iem/<STATION>/` and not downloaded again (unless the
//!    cached file is unreadable or the year looked sparse); the current year
//!    is fetched up to today on every training.
//! 2. Import them with our own METAR parser (`import_iem_csv`).
//! 3. When forecasts are enabled, download the day-1 forecast history
//!    (Open-Meteo Previous Runs, newest year first, finished years cached
//!    under `research/open-meteo/<STATION>/<model>-d<lead>/`) and run the
//!    study joined with it: a walk-forward evaluation with a placebo control
//!    decides whether the model may use the forecast (see
//!    `wm_backtest::forecast_eval`). Without forecast history the model is
//!    trained exactly as before.
//! 4. Run the peak-survival study, which also builds the empirical model —
//!    both pre-registered structures in the same pass. A walk-forward
//!    comparison decides whether the candidate structure replaces the
//!    current one (see `wm_backtest::selection`); each structure's forecast
//!    input is evaluated separately.
//! 5. Refuse too little data, then write the model and the report atomically
//!    (write + rename), so a reader never sees a partial file.
//!
//! Nothing here fabricates data: a year IEM cannot deliver stays missing and
//! the attempt fails (the service keeps running without a model, fail closed);
//! forecast history that cannot be obtained simply leaves the forecast unused.

use crate::config::AppConfig;
use crate::setup;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;
use wm_backtest::{
    EvaluationConfig, ForecastEvaluation, ForecastHistory, SelectionConfig, StructureComparison,
    StudyConfig, StudyOutput, import_iem_csv, study, study_and_select, study_with_forecasts,
};
use wm_core::forecast::ForecastProduct;
use wm_core::ids::StationId;
use wm_core::ingest::{IngestBatch, IngestSink, ProviderRequestRecord};
use wm_core::market::FeeSchedule;
use wm_core::resolution::ObservationFilter;
use wm_core::weather::Observation;
use wm_strategy::{
    EmpiricalPeakModel, ForecastModelInfo, ModelStructure, PeakConfig, StructureSelection,
};
use wm_weather::open_meteo::{known, parse_series};
use wm_weather::{IemArchive, OpenMeteoError, OpenMeteoPreviousRuns};

/// Longest wait for the IEM gate per request. Normal spacing (15 s) and short
/// throttles are waited out; a longer closure ends this attempt.
const MAX_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Assumed publication delay of historical reports (knowledge time).
const PUBLICATION_DELAY_MIN: i64 = 5;

/// A finished year with fewer reports than this (a half-hourly station makes
/// ~17,500) may be a transient or truncated answer: it is used for this run
/// but downloaded again next time instead of being cached for good.
const MIN_ROWS_TO_CACHE: usize = 1_000;

/// A forecast year with fewer hourly values than this is not cached for good.
const MIN_FORECAST_VALUES_TO_CACHE: usize = 24 * 28;

/// Everything one training run needs.
#[derive(Debug, Clone)]
pub struct TrainPlan {
    pub station: StationId,
    pub tz: Tz,
    pub peak: PeakConfig,
    pub from_year: i32,
    /// UTC date of the run: the current year is fetched up to (excluding) it.
    pub today: NaiveDate,
    pub cache_dir: PathBuf,
    pub model_out: PathBuf,
    pub report_out: PathBuf,
    pub min_days: u64,
    /// Forecast history and its evaluation; `None` = forecasts disabled.
    pub forecast: Option<ForecastPlan>,
    /// Walk-forward comparison of the two model structures; `None` = train
    /// the current structure only.
    pub selection: Option<SelectionConfig>,
    /// Strategy F's slot quantiles, for the peak-time table of the report.
    pub slot_quantiles: (f64, f64),
}

/// The forecast part of a training run.
#[derive(Debug, Clone)]
pub struct ForecastPlan {
    pub product: ForecastProduct,
    pub latitude: f64,
    pub longitude: f64,
    /// Earliest date requested.
    pub from: NaiveDate,
    pub cache_dir: PathBuf,
    pub eval: EvaluationConfig,
}

impl TrainPlan {
    /// Plan for the first configured location, writing the model to
    /// `model_out` (the service's model path).
    pub fn from_config(cfg: &AppConfig, model_out: PathBuf, today: NaiveDate) -> Result<Self> {
        let loc = cfg
            .locations
            .first()
            .context("no location configured to train a model for")?;
        let ids = setup::location_ids(loc)?;
        let at = &cfg.file.model.auto_train;
        let data = Path::new(&at.data_dir);
        let lower = ids.station.as_str().to_ascii_lowercase();
        let forecast = cfg.forecast_product().map(|product| {
            let yes = cfg.buy_yes();
            ForecastPlan {
                cache_dir: data
                    .join("research")
                    .join("open-meteo")
                    .join(ids.station.as_str())
                    .join(format!("{}-d{}", product.model, product.lead_days)),
                product,
                latitude: loc.station.latitude,
                longitude: loc.station.longitude,
                from: cfg.file.forecast.history_from,
                eval: EvaluationConfig {
                    fees: FeeSchedule::taker(loc.market.taker_fee_rate.micros()),
                    slippage: yes.slippage_allowance,
                    min_edge: yes.min_edge,
                    min_days: cfg.file.forecast.min_eval_days,
                    ..EvaluationConfig::default()
                },
            }
        });
        Ok(Self {
            station: ids.station.clone(),
            tz: ids.timezone,
            peak: setup::peak_config(loc),
            from_year: at.from_year,
            today,
            cache_dir: data.join("research").join("iem").join(ids.station.as_str()),
            model_out,
            report_out: data.join("research").join(format!("{lower}-survival.md")),
            min_days: at.min_days,
            forecast,
            selection: Some(SelectionConfig::default()),
            slot_quantiles: {
                let f = cfg.peak_slot();
                (f.slot_from_quantile, f.slot_to_quantile)
            },
        })
    }

    fn years(&self) -> Vec<i32> {
        (self.from_year..=self.today.year()).collect()
    }
}

/// Progress callbacks (dashboard and logs).
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Downloading {
        year: i32,
        done: u32,
        total: u32,
    },
    /// Forecast history, newest year first (`done` years so far).
    Forecast {
        year: i32,
        done: u32,
    },
    Training {
        observations: usize,
    },
}

/// What a successful training produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainOutcome {
    pub model_id: String,
    pub days: u64,
    pub samples: u64,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub observations: usize,
    pub years_downloaded: u32,
    pub years_cached: u32,
    /// The forecast evaluation's verdict (or why there was none).
    pub forecast_verdict: Option<String>,
    pub forecast_adopted: bool,
    /// The structure comparison's verdict (`None`: no comparison ran).
    pub structure_verdict: Option<String>,
    /// The model uses the candidate structure.
    pub candidate_adopted: bool,
}

/// One year's history file on disk and its provenance.
#[derive(Debug, Clone)]
pub(crate) struct YearFile {
    pub(crate) year: i32,
    pub(crate) path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) rows: usize,
}

/// The station's METAR history on disk.
#[derive(Debug, Clone, Default)]
pub(crate) struct IemHistory {
    pub(crate) files: Vec<YearFile>,
    pub(crate) downloaded: u32,
    pub(crate) cached: u32,
}

/// Download (or reuse) the history, train, validate and install the model.
/// `forecast` is the client for `plan.forecast` (ignored when that is `None`).
pub async fn train(
    archive: &IemArchive,
    forecast: Option<&OpenMeteoPreviousRuns>,
    plan: &TrainPlan,
    audit: Option<&Arc<dyn IngestSink>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<TrainOutcome> {
    let IemHistory {
        files,
        downloaded,
        cached,
    } = iem_history(archive, plan, audit, progress, shutdown).await?;

    // Forecast history (optional: failure leaves the forecast unused).
    let fc = match (&plan.forecast, forecast) {
        (Some(fp), Some(client)) => Some(
            match forecast_history(client, plan, fp, audit, progress, shutdown).await {
                Ok(h) => h,
                Err(e) if *shutdown.borrow() => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "forecast history unavailable: training without it");
                    ForecastDownload::failed(format!("{e:#}"))
                }
            },
        ),
        (Some(_), None) => Some(ForecastDownload::failed(
            "the open_meteo provider is disabled".into(),
        )),
        (None, _) => None,
    };
    train_on(plan, &files, downloaded, cached, fc, progress).await
}

/// Download (or reuse the cached) METAR history years of `plan`: finished
/// years are cached for good, the current year is fetched up to today.
pub(crate) async fn iem_history(
    archive: &IemArchive,
    plan: &TrainPlan,
    audit: Option<&Arc<dyn IngestSink>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<IemHistory> {
    std::fs::create_dir_all(&plan.cache_dir)
        .with_context(|| format!("creating {}", plan.cache_dir.display()))?;
    let years = plan.years();
    let total = u32::try_from(years.len()).unwrap_or(u32::MAX);
    let (mut downloaded, mut cached) = (0u32, 0u32);
    let mut files = Vec::with_capacity(years.len());
    for (i, &year) in years.iter().enumerate() {
        progress(Progress::Downloading {
            year,
            done: u32::try_from(i).unwrap_or(u32::MAX),
            total,
        });
        let complete = year < plan.today.year();
        let final_path = plan.cache_dir.join(format!("{year}.csv"));
        let partial_path = plan.cache_dir.join(format!("{year}-partial.csv"));
        if complete && final_path.is_file() {
            let cached_year = std::fs::read(&final_path)
                .map_err(anyhow::Error::from)
                .and_then(|b| {
                    let rows = wm_weather::history::validate_csv(&b)?;
                    Ok((b, rows))
                });
            match cached_year {
                Ok((bytes, rows)) => {
                    files.push(YearFile {
                        year,
                        path: final_path,
                        sha256: wm_core::hash::sha256_hex(&bytes),
                        rows,
                    });
                    cached += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(file = %final_path.display(), error = %e, "cached history year unusable: downloading it again");
                    let _ = std::fs::remove_file(&final_path);
                }
            }
        }
        let until = if complete {
            NaiveDate::from_ymd_opt(year + 1, 1, 1).context("date")?
        } else {
            plan.today
        };
        if until <= NaiveDate::from_ymd_opt(year, 1, 1).context("date")? {
            continue; // 1 January: nothing of this year is complete yet
        }
        let fetched = tokio::select! {
            r = archive.fetch_year(&plan.station, year, until, MAX_GATE_WAIT) => r,
            _ = shutdown.changed() => bail!("shutting down"),
        };
        let fetched = match fetched {
            Ok(y) => y,
            Err(e) => {
                if let Some(r) = e.record() {
                    record(audit, r).await;
                }
                return Err(anyhow::anyhow!(e))
                    .with_context(|| format!("downloading {} {year} from IEM", plan.station));
            }
        };
        record(audit, &fetched.record).await;
        let keep = complete && fetched.rows >= MIN_ROWS_TO_CACHE;
        let path = if keep {
            final_path
        } else {
            partial_path.clone()
        };
        write_atomic(&path, &fetched.csv)?;
        if keep {
            let _ = std::fs::remove_file(&partial_path);
        }
        files.push(YearFile {
            year,
            path,
            sha256: wm_core::hash::sha256_hex(&fetched.csv),
            rows: fetched.rows,
        });
        downloaded += 1;
        tracing::info!(station = %plan.station, year, rows = fetched.rows, "IEM history year downloaded");
    }
    Ok(IemHistory {
        files,
        downloaded,
        cached,
    })
}

/// Train, validate and install the model from downloaded history.
async fn train_on(
    plan: &TrainPlan,
    files: &[YearFile],
    downloaded: u32,
    cached: u32,
    fc: Option<ForecastDownload>,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<TrainOutcome> {
    // CPU-bound part off the async runtime.
    let plan2 = plan.clone();
    let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    let observations = tokio::task::spawn_blocking(move || import_all(&paths, &plan2.station))
        .await
        .context("import task")??;
    progress(Progress::Training {
        observations: observations.len(),
    });
    let plan2 = plan.clone();
    let hourly = fc.as_ref().map(|f| f.hourly.clone()).unwrap_or_default();
    let out = tokio::task::spawn_blocking(move || {
        let cfg = StudyConfig {
            station: plan2.station.clone(),
            tz: plan2.tz,
            filter: ObservationFilter::AllRows,
            peak: plan2.peak.clone(),
            k_classes: 4,
            min_high_local_minute: 9 * 60,
        };
        let history = match &plan2.forecast {
            Some(fp) if !hourly.is_empty() => Some((
                ForecastHistory::from_hourly(fp.product.clone(), plan2.tz, &hourly),
                fp.eval.clone(),
            )),
            _ => None,
        };
        match (plan2.selection.clone(), history) {
            (Some(sel), h) => study_and_select(
                &observations,
                &cfg,
                h.as_ref().map(|(h, e)| (h, e.clone())),
                sel,
            ),
            (None, Some((h, e))) => study_with_forecasts(&observations, &cfg, &h, e),
            (None, None) => {
                let (report, model) = study(&observations, &cfg);
                StudyOutput {
                    report,
                    peak_times: model.peak_times.clone().unwrap_or_default(),
                    model,
                    evaluation: None,
                    candidate: None,
                }
            }
        }
    })
    .await
    .context("training task")?;
    let (report, model, evaluation, comparison, other) = select(out);

    if report.days < plan.min_days || model.total_samples() == 0 {
        bail!(
            "not enough history to trust a model: {} usable days (minimum {}), {} samples",
            report.days,
            plan.min_days,
            model.total_samples()
        );
    }
    let (mut model, verdict, adopted) = finalize(model, plan, fc.as_ref(), evaluation.as_ref());
    model.selection = comparison.as_ref().map(|c| StructureSelection {
        structure: model.structure(),
        candidate_adopted: c.adopted,
        verdict: c.verdict.clone(),
        evaluated_at: Utc::now(),
    });
    let candidate_adopted = model.structure() == ModelStructure::Candidate;
    model.id = format!(
        "{}-{}{}",
        model.id,
        plan.today.format("%Y%m%d"),
        if adopted { "+fc" } else { "" }
    );
    let n_obs = files.iter().map(|f| f.rows).sum::<usize>();
    let mut md = report.to_markdown();
    if let Some(pt) = &model.peak_times {
        md.push_str(&pt.to_markdown(plan.slot_quantiles.0, plan.slot_quantiles.1));
    }
    if let Some(c) = &comparison {
        md.push_str(&c.to_markdown());
    }
    match (&evaluation, &fc) {
        (Some(e), _) => {
            md.push_str(&e.to_markdown());
            // The structure not selected: its forecast evaluation, for reference.
            if let Some(o) = &other {
                let _ = write!(
                    md,
                    "\n*Structure not selected — for reference only:*\n{}",
                    o.to_markdown()
                );
            }
        }
        (None, Some(f)) => {
            let _ = write!(
                md,
                "\n## Forecast evaluation\n\n**Verdict: {}**\n",
                verdict.as_deref().unwrap_or_default()
            );
            if let Some(note) = &f.note {
                let _ = writeln!(md, "\n{note}");
            }
        }
        (None, None) => {}
    }
    md.push_str(&provenance(plan, files, &model));
    if let Some(f) = &fc {
        md.push_str(&forecast_provenance(plan, f));
    }
    if let Some(dir) = plan.model_out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomic(&plan.report_out, md.as_bytes())?;
    write_atomic(&plan.model_out, &serde_json::to_vec_pretty(&model)?)?;
    // Loading it back is the same check the service applies at startup.
    setup::read_model(&plan.model_out)?;
    Ok(TrainOutcome {
        model_id: model.id.clone(),
        days: report.days,
        samples: model.total_samples(),
        from: report.from,
        to: report.to,
        observations: n_obs,
        years_downloaded: downloaded,
        years_cached: cached,
        forecast_verdict: verdict,
        forecast_adopted: adopted,
        structure_verdict: comparison.map(|c| c.verdict),
        candidate_adopted,
    })
}

/// The structure the comparison selected (the candidate only when adopted),
/// its forecast evaluation, the comparison, and the forecast evaluation of
/// the structure not selected.
fn select(
    out: StudyOutput,
) -> (
    wm_backtest::SurvivalReport,
    EmpiricalPeakModel,
    Option<ForecastEvaluation>,
    Option<StructureComparison>,
    Option<ForecastEvaluation>,
) {
    match out.candidate {
        Some(c) if c.comparison.adopted => (
            out.report,
            c.model,
            c.evaluation,
            Some(c.comparison),
            out.evaluation,
        ),
        Some(c) => (
            out.report,
            out.model,
            out.evaluation,
            Some(c.comparison),
            c.evaluation,
        ),
        None => (out.report, out.model, out.evaluation, None, None),
    }
}

/// Keep the forecast refinement only if the evaluation adopted it, and record
/// the evaluation (or why there was none) in the model.
fn finalize(
    model: EmpiricalPeakModel,
    plan: &TrainPlan,
    fc: Option<&ForecastDownload>,
    evaluation: Option<&ForecastEvaluation>,
) -> (EmpiricalPeakModel, Option<String>, bool) {
    let Some(fp) = &plan.forecast else {
        return (model, None, false);
    };
    let (mut m, info) = match evaluation {
        Some(e) => {
            let m = if e.adopted {
                model
            } else {
                model.without_refinements()
            };
            (
                m,
                ForecastModelInfo {
                    product: fp.product.clone(),
                    adopted: e.adopted,
                    verdict: e.verdict.clone(),
                    days_with_forecast: e.days_with_forecast,
                    evaluated: true,
                    evaluated_at: Utc::now(),
                },
            )
        }
        None => {
            let why = fc
                .and_then(|f| f.error.clone().or_else(|| f.note.clone()))
                .unwrap_or_else(|| "no forecast history covers the METAR history".into());
            (
                model.without_refinements(),
                ForecastModelInfo {
                    product: fp.product.clone(),
                    adopted: false,
                    verdict: format!("not evaluated: {why}"),
                    days_with_forecast: 0,
                    // Retry later only when the download failed; an archive
                    // that simply has no overlapping data will not change soon.
                    evaluated: fc.is_some_and(|f| f.error.is_none()),
                    evaluated_at: Utc::now(),
                },
            )
        }
    };
    let verdict = Some(info.verdict.clone());
    let adopted = info.adopted;
    m.forecast = Some(info);
    (m, verdict, adopted)
}

/// Downloaded forecast history.
#[derive(Debug, Clone, Default)]
pub(crate) struct ForecastDownload {
    /// Valid time and value (tenths °C), known hours only.
    pub(crate) hourly: Vec<(DateTime<Utc>, i32)>,
    files: Vec<YearFile>,
    downloaded: u32,
    cached: u32,
    /// Why the history starts where it does (e.g. the archive's first date).
    note: Option<String>,
    /// Set when the download failed: nothing was evaluated.
    error: Option<String>,
}

impl ForecastDownload {
    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }
}

/// Download the forecast history newest year first. The walk stops at the
/// first year the archive does not cover (so years before the archive are
/// requested at most once per training, and the gate's circuit breaker never
/// sees a string of rejections). A rejection that states the archive's first
/// date is retried once from that date.
pub(crate) async fn forecast_history(
    client: &OpenMeteoPreviousRuns,
    plan: &TrainPlan,
    fp: &ForecastPlan,
    audit: Option<&Arc<dyn IngestSink>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ForecastDownload> {
    std::fs::create_dir_all(&fp.cache_dir)
        .with_context(|| format!("creating {}", fp.cache_dir.display()))?;
    let variable = OpenMeteoPreviousRuns::variable(fp.product.lead_days);
    let last = plan.today - Duration::days(1);
    // First date of the archive, learned from an earlier rejection: years
    // before it are not requested again.
    let marker = fp.cache_dir.join("archive-start.txt");
    let known_start = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|t| NaiveDate::parse_from_str(t.trim(), "%Y-%m-%d").ok());
    let from = known_start.map_or(fp.from, |a| a.max(fp.from));
    let mut out = ForecastDownload::default();
    if let Some(a) = known_start {
        out.note = Some(format!("forecast archive starts {a}"));
    }
    let mut year = last.year();
    while year >= from.year() {
        progress(Progress::Forecast {
            year,
            done: out.downloaded + out.cached,
        });
        let complete = year < plan.today.year();
        let final_path = fp.cache_dir.join(format!("{year}.json"));
        let partial_path = fp.cache_dir.join(format!("{year}-partial.json"));
        if complete && final_path.is_file() {
            let parsed = std::fs::read(&final_path)
                .map_err(anyhow::Error::from)
                .and_then(|b| Ok((parse_series(&b, &variable)?, b)));
            match parsed {
                Ok((series, bytes)) => {
                    let values = known(&series);
                    out.files.push(YearFile {
                        year,
                        path: final_path,
                        sha256: wm_core::hash::sha256_hex(&bytes),
                        rows: values.len(),
                    });
                    out.hourly
                        .extend(values.into_iter().map(|(t, v)| (t, v.tenths())));
                    out.cached += 1;
                    year -= 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(file = %final_path.display(), error = %e, "cached forecast year unusable: downloading it again");
                    let _ = std::fs::remove_file(&final_path);
                }
            }
        }
        let mut start = NaiveDate::from_ymd_opt(year, 1, 1)
            .context("date")?
            .max(from);
        let end = NaiveDate::from_ymd_opt(year, 12, 31)
            .context("date")?
            .min(last);
        if start > end {
            break;
        }
        let mut retried = false;
        let series = loop {
            let fetched = tokio::select! {
                r = client.fetch_series(fp.latitude, fp.longitude, start, end, MAX_GATE_WAIT) => r,
                _ = shutdown.changed() => bail!("shutting down"),
            };
            if let Some(r) = fetched.as_ref().err().and_then(OpenMeteoError::record) {
                record(audit, r).await;
            }
            if let Err(OpenMeteoError::Rejected {
                allowed_from: Some(first),
                ..
            }) = &fetched
            {
                write_atomic(&marker, first.to_string().as_bytes())?;
            }
            match fetched {
                Ok(s) => break Some(s),
                Err(OpenMeteoError::Rejected {
                    allowed_from: Some(first),
                    ..
                }) if !retried && first > start && first <= end => {
                    out.note = Some(format!("forecast archive starts {first}"));
                    start = first;
                    retried = true;
                }
                Err(OpenMeteoError::Rejected { reason, .. }) => {
                    out.note = Some(format!("history ends before {year}: Open-Meteo: {reason}"));
                    break None;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(e))
                        .with_context(|| format!("downloading {year} forecasts from Open-Meteo"));
                }
            }
        };
        let Some(series) = series else { break };
        record(audit, &series.record).await;
        let values = series.values();
        if values.is_empty() {
            out.note = Some(format!("no forecast values before {}", year + 1));
            break;
        }
        let keep = complete && values.len() >= MIN_FORECAST_VALUES_TO_CACHE;
        let path = if keep {
            final_path
        } else {
            partial_path.clone()
        };
        write_atomic(&path, &series.body)?;
        if keep {
            let _ = std::fs::remove_file(&partial_path);
        }
        out.files.push(YearFile {
            year,
            path,
            sha256: wm_core::hash::sha256_hex(&series.body),
            rows: values.len(),
        });
        out.hourly
            .extend(values.into_iter().map(|(t, v)| (t, v.tenths())));
        out.downloaded += 1;
        tracing::info!(station = %plan.station, year, values = out.files.last().map_or(0, |f| f.rows), "forecast history year downloaded");
        if retried {
            break; // the archive starts inside this year
        }
        year -= 1;
    }
    out.hourly.sort_by_key(|p| p.0);
    out.hourly.dedup_by_key(|p| p.0);
    out.files.sort_by_key(|f| f.year);
    Ok(out)
}

fn forecast_provenance(plan: &TrainPlan, f: &ForecastDownload) -> String {
    let Some(fp) = &plan.forecast else {
        return String::new();
    };
    let mut s = format!(
        "\nForecast history: `{}` from the Open-Meteo Previous Runs API (`temperature_2m_previous_day{}`, point {:.4}, {:.4}, UTC), knowledge time = local midnight + {} min.{}\n\n| year | hourly values | SHA-256 |\n|---:|---:|---|\n",
        fp.product.label(),
        fp.product.lead_days,
        fp.latitude,
        fp.longitude,
        fp.product.ready_local_minute,
        f.note
            .as_deref()
            .map(|n| format!(" {n}."))
            .unwrap_or_default(),
    );
    for y in &f.files {
        let _ = writeln!(s, "| {} | {} | `{}` |", y.year, y.rows, y.sha256);
    }
    s
}

pub(crate) fn import_all(paths: &[PathBuf], station: &StationId) -> Result<Vec<Observation>> {
    let mut all = Vec::new();
    for p in paths {
        let f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
        let (obs, stats) = import_iem_csv(
            std::io::BufReader::new(f),
            station,
            Duration::minutes(PUBLICATION_DELAY_MIN),
        )
        .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        tracing::debug!(file = %p.display(), imported = stats.imported, rows = stats.rows, unparseable = stats.skipped_unparseable, "history imported");
        all.extend(obs);
    }
    // Year files overlap at most at the boundary instant.
    all.sort_by(|a, b| {
        (a.key.observed_at, a.key.report_type, &a.content_hash).cmp(&(
            b.key.observed_at,
            b.key.report_type,
            &b.content_hash,
        ))
    });
    all.dedup_by(|a, b| {
        a.key.observed_at == b.key.observed_at
            && a.key.report_type == b.key.report_type
            && a.content_hash == b.content_hash
    });
    Ok(all)
}

fn provenance(plan: &TrainPlan, files: &[YearFile], model: &EmpiricalPeakModel) -> String {
    let mut s = format!(
        "\n## Provenance\n\nModel `{}` ({} samples) trained {} from IEM ASOS/METAR archive data for {} (`https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py`, `data=metar`, report types 3 + 4, UTC). Temperatures come from Weather Machine's own METAR parser; knowledge time = observation + {PUBLICATION_DELAY_MIN} min.\n\n| year | rows | SHA-256 |\n|---:|---:|---|\n",
        model.id,
        model.total_samples(),
        plan.today,
        plan.station,
    );
    for f in files {
        let _ = writeln!(s, "| {} | {} | `{}` |", f.year, f.rows, f.sha256);
    }
    s
}

async fn record(audit: Option<&Arc<dyn IngestSink>>, r: &ProviderRequestRecord) {
    let Some(sink) = audit else { return };
    let batch = IngestBatch {
        request: r.clone(),
        raw: None,
        observations: Vec::new(),
        corrections: Vec::new(),
        health: None,
    };
    if let Err(e) = sink.persist(batch).await {
        tracing::warn!(error = %e, "could not audit an IEM request");
    }
}

/// Write via a temporary file in the same directory and rename it into place.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}
