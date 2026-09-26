//! Probability-model training from real METAR history, on the deployment host.
//!
//! 1. Download the station's METARs from the IEM archive, one station-year per
//!    request through the rate-limited `iem` gate. Finished years are cached
//!    under `research/iem/<STATION>/` and not downloaded again (unless the
//!    cached file is unreadable or the year looked sparse); the current year
//!    is fetched up to today on every training.
//! 2. Import them with our own METAR parser (`import_iem_csv`).
//! 3. Run the peak-survival study, which also builds the empirical model.
//! 4. Refuse too little data, then write the model and the survival report
//!    atomically (write + rename), so a reader never sees a partial file.
//!
//! Nothing here fabricates data: a year IEM cannot deliver stays missing and
//! the attempt fails (the service keeps running without a model, fail closed).

use crate::config::AppConfig;
use crate::setup;
use anyhow::{Context, Result, bail};
use chrono::{Datelike, Duration, NaiveDate};
use chrono_tz::Tz;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;
use wm_backtest::{StudyConfig, import_iem_csv, study};
use wm_core::ids::StationId;
use wm_core::ingest::{IngestBatch, IngestSink, ProviderRequestRecord};
use wm_core::resolution::ObservationFilter;
use wm_core::weather::Observation;
use wm_strategy::{EmpiricalPeakModel, PeakConfig};
use wm_weather::IemArchive;

/// Longest wait for the IEM gate per request. Normal spacing (15 s) and short
/// throttles are waited out; a longer closure ends this attempt.
const MAX_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Assumed publication delay of historical reports (knowledge time).
const PUBLICATION_DELAY_MIN: i64 = 5;

/// A finished year with fewer reports than this (a half-hourly station makes
/// ~17,500) may be a transient or truncated answer: it is used for this run
/// but downloaded again next time instead of being cached for good.
const MIN_ROWS_TO_CACHE: usize = 1_000;

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
        })
    }

    fn years(&self) -> Vec<i32> {
        (self.from_year..=self.today.year()).collect()
    }
}

/// Progress callbacks (dashboard and logs).
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Downloading { year: i32, done: u32, total: u32 },
    Training { observations: usize },
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
}

/// One year's CSV on disk and its provenance.
struct YearFile {
    year: i32,
    path: PathBuf,
    sha256: String,
    rows: usize,
}

/// Download (or reuse) the history, train, validate and install the model.
pub async fn train(
    archive: &IemArchive,
    plan: &TrainPlan,
    audit: Option<&Arc<dyn IngestSink>>,
    progress: &(dyn Fn(Progress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<TrainOutcome> {
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
    let (report, mut model) = tokio::task::spawn_blocking(move || {
        let cfg = StudyConfig {
            station: plan2.station.clone(),
            tz: plan2.tz,
            filter: ObservationFilter::AllRows,
            peak: plan2.peak.clone(),
            k_classes: 4,
            min_high_local_minute: 9 * 60,
        };
        study(&observations, &cfg)
    })
    .await
    .context("training task")?;

    if report.days < plan.min_days || model.total_samples() == 0 {
        bail!(
            "not enough history to trust a model: {} usable days (minimum {}), {} samples",
            report.days,
            plan.min_days,
            model.total_samples()
        );
    }
    model.id = format!("{}-{}", model.id, plan.today.format("%Y%m%d"));
    let n_obs = files.iter().map(|f| f.rows).sum::<usize>();
    let mut md = report.to_markdown();
    md.push_str(&provenance(plan, &files, &model));
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
    })
}

fn import_all(paths: &[PathBuf], station: &StationId) -> Result<Vec<Observation>> {
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
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(())
}
