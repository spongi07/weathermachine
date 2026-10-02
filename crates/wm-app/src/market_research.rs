//! `research market`: the model against the market on settled days.
//!
//! 1. For each local date, the event is looked up on Gamma by slug; only
//!    settled events count (every bucket closed, exactly one resolved YES).
//! 2. The event's trades come from the Data API (taker side, all buckets in
//!    one query, the local day plus six hours before it).
//! 3. Settled days are cached under `research/polymarket/<STATION>/` (the raw
//!    Gamma body and the trades): they never change, so a rerun asks the
//!    network only for new days. Wallets are stored as short hashes — they
//!    are used only to count distinct traders.
//! 4. The METAR history and the forecast history come from the training
//!    caches, downloaded the same way when missing; the model is evaluated
//!    with the forecast only when the installed model uses it, the strategy
//!    lab reads it either way. With `WM_KNMI_API_KEY` set, KNMI's ten-minute
//!    readings of the station are downloaded a week at a time (cached once a
//!    week is more than eight days old: KNMI may fill gaps for seven) for
//!    strategy K, and for the lab its global radiation and, at Schiphol, the
//!    temperatures of three neighbouring stations. A week that will not
//!    download costs only that week; an input that will not download at all
//!    leaves its strategies unreplayed.
//! 5. [`wm_backtest::market_study_lab`] replays it all prequentially; the
//!    report is written as Markdown and JSON.
//!
//! Read-only: nothing here trades or changes the model.

use crate::training::{self, Progress, TrainPlan};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::sync::watch;
use wm_backtest::{
    ForecastHistory, KnmiHistory, LabInputs, MarketDay, MarketStudyConfig, MarketStudyReport,
    MarketTrade, SeriesHistory, market_study_lab,
};
use wm_core::ids::ConditionId;
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::time::local_day_bounds;
use wm_core::weather::TenMinuteObservation;
use wm_polymarket::{
    DataApiClient, GammaClient, GammaEvent, LocationMarketSpec, build_market, event_slug,
    parse_events,
};
use wm_weather::knmi::wigos_id;
use wm_weather::{IemArchive, KnmiError, KnmiTenMinute, OpenMeteoPreviousRuns, SeriesPoint};

/// Longest wait for a gate per request.
const MAX_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Trades are read from this long before the local day starts.
const TRADES_BEFORE_DAY: Duration = Duration::hours(6);

/// Attempts per Gamma lookup while the failure is transient (the gate spaces
/// them by the server's Retry-After and its own backoff).
const GAMMA_ATTEMPTS: u32 = 5;

/// Downloads stop after this many market days in a row failed even after
/// retries: the source is down, and the days so far are studied.
const MAX_FAILED_DAYS_IN_A_ROW: u32 = 3;

/// Everything one study needs.
#[derive(Debug, Clone)]
pub struct MarketResearchPlan {
    /// History and forecast settings, as for training.
    pub train: TrainPlan,
    pub spec: LocationMarketSpec,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub study: MarketStudyConfig,
    /// Evaluate the model with the forecast (as the installed model does).
    pub use_forecast: bool,
    /// KNMI EDR location of the station (strategy K's replay); `None`: none.
    pub knmi_location: Option<String>,
    pub cache_dir: PathBuf,
    /// Markdown report; the JSON report is written beside it.
    pub report_out: PathBuf,
}

/// The read-only clients a study uses.
pub struct MarketResearchClients<'a> {
    pub gamma: &'a GammaClient,
    pub data: &'a DataApiClient,
    pub archive: &'a IemArchive,
    /// Required only when the plan uses the forecast.
    pub forecast: Option<&'a OpenMeteoPreviousRuns>,
    /// KNMI's ten-minute readings (needs `WM_KNMI_API_KEY`); `None`: K is not
    /// replayed.
    pub knmi: Option<&'a KnmiTenMinute>,
}

/// Progress callbacks.
#[derive(Debug, Clone, PartialEq)]
pub enum ResearchProgress {
    Market {
        date: NaiveDate,
        done: u32,
        total: u32,
    },
    History(Progress),
    /// KNMI readings of the week from `from` (cached or downloading).
    Knmi {
        from: NaiveDate,
        to: NaiveDate,
    },
    /// The strategy lab's KNMI readings (`what`: radiation, a neighbour) of
    /// the week from `from`.
    KnmiSeries {
        what: String,
        from: NaiveDate,
        to: NaiveDate,
    },
    Studying {
        market_days: usize,
    },
}

/// What a study produced.
#[derive(Debug, Clone)]
pub struct MarketResearchOutcome {
    pub report: MarketStudyReport,
    pub markdown: PathBuf,
    pub json: PathBuf,
    /// Dates without a usable settled market, with the reason.
    pub unavailable: Vec<(NaiveDate, String)>,
    pub days_cached: u32,
    pub days_downloaded: u32,
}

/// One trade as cached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedTrade {
    at: DateTime<Utc>,
    asset: String,
    side: Side,
    price: f64,
    size: f64,
    /// Short hash of the taker's wallet.
    taker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedTrades {
    requests: u32,
    truncated: bool,
    trades: Vec<CachedTrade>,
}

/// A wallet as the caches and the paper strategies keep it: the first 16
/// hex digits of its SHA-256.
pub(crate) fn short_hash(s: &str) -> String {
    wm_core::hash::sha256_hex(s.as_bytes())[..16].to_owned()
}

/// Index of the bucket that resolved YES, or why there is none.
pub fn settled_winner(ev: &GammaEvent, market: &DailyTemperatureMarket) -> Result<usize, String> {
    if ev.markets.iter().any(|m| m.closed != Some(true)) {
        return Err("not settled yet (a bucket is still open)".into());
    }
    let mut winners = Vec::new();
    for m in &ev.markets {
        let yes = m
            .outcomes
            .iter()
            .position(|o| o.eq_ignore_ascii_case("yes"));
        let price = yes
            .and_then(|i| m.outcome_prices.get(i))
            .and_then(|p| p.trim().parse::<f64>().ok());
        if price.is_some_and(|p| p >= 0.999)
            && let Some(cond) = m.condition_id.as_deref()
            && let Some(i) = market
                .outcomes
                .iter()
                .position(|o| o.condition_id.as_str() == cond)
        {
            winners.push(i);
        }
    }
    match winners.as_slice() {
        [w] => Ok(*w),
        [] => Err("closed without a bucket resolved YES".into()),
        _ => Err("several buckets resolved YES".into()),
    }
}

/// Trades in YES terms, keyed to the market's buckets; unknown assets dropped.
fn to_market_trades(market: &DailyTemperatureMarket, trades: &[CachedTrade]) -> Vec<MarketTrade> {
    let mut out: Vec<MarketTrade> = trades
        .iter()
        .filter_map(|t| {
            let (bucket, side) = market.outcomes.iter().enumerate().find_map(|(i, o)| {
                if o.yes_token.as_str() == t.asset {
                    Some((i, OutcomeSide::Yes))
                } else if o.no_token.as_str() == t.asset {
                    Some((i, OutcomeSide::No))
                } else {
                    None
                }
            })?;
            let yes = side == OutcomeSide::Yes;
            Some(MarketTrade {
                at: t.at,
                bucket,
                yes_price: if yes { t.price } else { 1.0 - t.price },
                taker_buys_yes: yes == (t.side == Side::Buy),
                shares: t.size,
                taker: t.taker.clone(),
            })
        })
        .collect();
    out.sort_by_key(|t| t.at);
    out
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A settled market day of `spec`, from the cache in `cache_dir` or the
/// network (then cached). `Ok((Err(why), _))`: no settled market that day.
/// The paper strategies' wallet scores read the same cache.
pub(crate) async fn market_day(
    gamma: &GammaClient,
    data: &DataApiClient,
    spec: &LocationMarketSpec,
    cache_dir: &Path,
    date: NaiveDate,
    now: DateTime<Utc>,
) -> Result<(Result<MarketDay, String>, bool)> {
    let slug = event_slug(&spec.slug_template, date);
    let event_path = cache_dir.join(format!("{slug}.event.json"));
    let trades_path = cache_dir.join(format!("{slug}.trades.json"));
    let cached_event = std::fs::read(&event_path)
        .ok()
        .and_then(|b| parse_events(&b).ok().map(|e| (e, b)));
    let cached_trades: Option<CachedTrades> = read_json(&trades_path);
    let from_cache = cached_event.is_some() && cached_trades.is_some();
    let (events, body) = match cached_event {
        Some(e) if from_cache => e,
        _ => {
            let (events, body) = gamma
                .events_by_slug_attempts(&slug, MAX_GATE_WAIT, GAMMA_ATTEMPTS)
                .await
                .with_context(|| format!("Gamma event {slug}"))?;
            (events, body.to_vec())
        }
    };
    let Some(ev) = events.iter().find(|e| e.slug == slug) else {
        return Ok((Err("no event with this slug".into()), false));
    };
    let market = match build_market(ev, spec, date, now) {
        Ok(m) => m,
        Err(e) => return Ok((Err(format!("market mapping failed: {e}")), false)),
    };
    let winner = match settled_winner(ev, &market) {
        Ok(w) => w,
        Err(why) => return Ok((Err(why), false)),
    };
    let trades = match cached_trades.filter(|_| from_cache) {
        Some(t) => t,
        None => {
            let (start, end) = local_day_bounds(date, spec.timezone);
            let conditions: Vec<ConditionId> = market
                .outcomes
                .iter()
                .map(|o| o.condition_id.clone())
                .collect();
            let h = data
                .trades(&conditions, start - TRADES_BEFORE_DAY, end, MAX_GATE_WAIT)
                .await
                .with_context(|| format!("Data API trades of {slug}"))?;
            if h.skipped > 0 {
                tracing::warn!(slug, skipped = h.skipped, "unusable trade records skipped");
            }
            let cached = CachedTrades {
                requests: h.requests,
                truncated: h.truncated,
                trades: h
                    .trades
                    .into_iter()
                    .map(|t| CachedTrade {
                        at: t.at,
                        asset: t.asset.as_str().to_owned(),
                        side: t.side,
                        price: t.price,
                        size: t.size,
                        taker: t.taker.as_deref().map(short_hash),
                    })
                    .collect(),
            };
            std::fs::create_dir_all(cache_dir)
                .with_context(|| format!("creating {}", cache_dir.display()))?;
            training::write_atomic(&trades_path, &serde_json::to_vec(&cached)?)?;
            training::write_atomic(&event_path, &body)?;
            cached
        }
    };
    let day = MarketDay {
        date,
        event_slug: slug,
        buckets: market.outcomes.iter().map(|o| o.bucket).collect(),
        labels: market.outcomes.iter().map(|o| o.label.clone()).collect(),
        winner,
        trades: to_market_trades(&market, &trades.trades),
        truncated: trades.truncated,
    };
    Ok((Ok(day), from_cache))
}

/// Run the study and write its reports.
pub async fn run(
    clients: &MarketResearchClients<'_>,
    plan: &MarketResearchPlan,
    now: DateTime<Utc>,
    progress: &(dyn Fn(ResearchProgress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<MarketResearchOutcome> {
    if plan.from > plan.to {
        bail!("--from {} is after --to {}", plan.from, plan.to);
    }
    let dates = wm_backtest::date_range(plan.from, plan.to);
    let total = u32::try_from(dates.len()).unwrap_or(u32::MAX);
    let mut days = Vec::new();
    let mut unavailable = Vec::new();
    let (mut cached, mut downloaded) = (0u32, 0u32);
    let mut failed_in_a_row = 0u32;
    for (i, &date) in dates.iter().enumerate() {
        if *shutdown.borrow() {
            bail!("shutting down");
        }
        progress(ResearchProgress::Market {
            date,
            done: u32::try_from(i).unwrap_or(u32::MAX),
            total,
        });
        let fetched = tokio::select! {
            r = market_day(clients.gamma, clients.data, &plan.spec, &plan.cache_dir, date, now) => r,
            _ = shutdown.changed() => bail!("shutting down"),
        };
        // A day that still fails after the retries is skipped, not fatal:
        // nothing of it is cached, so the next run asks for it again.
        let (day, from_cache) = match fetched {
            Ok(r) => {
                failed_in_a_row = 0;
                r
            }
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!(%date, error = %why, "market day skipped: download failed after retries");
                unavailable.push((date, format!("download failed (rerun to retry): {why}")));
                failed_in_a_row += 1;
                if failed_in_a_row >= MAX_FAILED_DAYS_IN_A_ROW {
                    tracing::warn!(
                        failed_in_a_row,
                        "Polymarket keeps failing: no further days are requested"
                    );
                    for &rest in &dates[i + 1..] {
                        unavailable.push((
                            rest,
                            "not requested: the previous days' downloads kept failing".into(),
                        ));
                    }
                    break;
                }
                continue;
            }
        };
        match day {
            Ok(d) => {
                if from_cache {
                    cached += 1;
                } else {
                    downloaded += 1;
                }
                days.push(d);
            }
            Err(why) => unavailable.push((date, why)),
        }
    }
    if days.is_empty() {
        let failed = unavailable
            .iter()
            .filter(|(_, why)| why.starts_with("download failed"))
            .count();
        if failed > 0 {
            bail!(
                "no market day between {} and {} could be downloaded: {failed} failed even after retries (the reasons are logged above); rerun later",
                plan.from,
                plan.to
            );
        }
        bail!(
            "no settled market between {} and {} (check the slug template '{}')",
            plan.from,
            plan.to,
            plan.spec.slug_template
        );
    }

    let hist_progress = |p: Progress| progress(ResearchProgress::History(p));
    let history =
        training::iem_history(clients.archive, &plan.train, None, &hist_progress, shutdown).await?;
    let forecasts = match (&plan.train.forecast, clients.forecast) {
        (Some(fp), Some(client)) if plan.use_forecast => {
            let f =
                training::forecast_history(client, &plan.train, fp, None, &hist_progress, shutdown)
                    .await
                    .context("forecast history (the installed model uses the forecast)")?;
            Some(ForecastHistory::from_hourly(
                fp.product.clone(),
                plan.train.tz,
                &f.hourly,
            ))
        }
        _ => None,
    };
    let knmi = match (clients.knmi, plan.knmi_location.as_deref()) {
        (Some(client), Some(location)) => {
            match knmi_history(client, plan, location, now, progress, shutdown).await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "KNMI readings unavailable: strategy K is not replayed");
                    None
                }
            }
        }
        _ => None,
    };
    let lab = lab_inputs(
        clients,
        plan,
        forecasts.is_some(),
        now,
        &hist_progress,
        progress,
        shutdown,
    )
    .await?;
    progress(ResearchProgress::Studying {
        market_days: days.len(),
    });
    let paths: Vec<PathBuf> = history.files.iter().map(|f| f.path.clone()).collect();
    let station = plan.train.station.clone();
    let study = plan.study.clone();
    let report = tokio::task::spawn_blocking(move || -> Result<MarketStudyReport> {
        let observations = training::import_all(&paths, &station)?;
        Ok(market_study_lab(
            &observations,
            forecasts.as_ref(),
            knmi.as_ref(),
            &lab,
            &days,
            &study,
        ))
    })
    .await
    .context("study task")??;

    let markdown = plan.report_out.clone();
    let json = markdown.with_extension("json");
    if let Some(dir) = markdown.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut md = report.to_markdown();
    if !unavailable.is_empty() {
        md.push_str("\n## Dates without a settled market\n\n");
        for (d, why) in &unavailable {
            md.push_str(&format!("* {d}: {why}\n"));
        }
    }
    md.push_str(&format!(
        "\nSources: Polymarket Gamma events (`{}`), Polymarket Data API trades (taker side), IEM METAR archive{}; generated {}.\n",
        plan.spec.slug_template,
        if report.knmi_days > 0
            || report.lab_coverage.radiation_days > 0
            || report.lab_coverage.neighbour_days > 0
        {
            ", KNMI Data Platform ten-minute observations (EDR API)"
        } else {
            ""
        },
        now.format("%Y-%m-%d %H:%M UTC")
    ));
    training::write_atomic(&markdown, md.as_bytes())?;
    training::write_atomic(&json, &serde_json::to_vec_pretty(&report)?)?;
    Ok(MarketResearchOutcome {
        report,
        markdown,
        json,
        unavailable,
        days_cached: cached,
        days_downloaded: downloaded,
    })
}

/// Days of readings per download; a week of one station is ~1,000 values.
const KNMI_CHUNK_DAYS: i64 = 7;

/// A chunk is cached once its last day is this far in the past: KNMI may
/// add missing observations for seven days.
const KNMI_FINAL_AFTER_DAYS: i64 = 8;

/// Where the research caches live (`research/`).
fn research_root(plan: &MarketResearchPlan) -> PathBuf {
    plan.cache_dir
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| plan.cache_dir.clone(), Path::to_path_buf)
}

/// One series of KNMI downloads.
struct KnmiWeeks<'a> {
    /// Cache directory of the weeks.
    dir: PathBuf,
    /// What the readings are for, in the logs.
    what: &'a str,
    /// Give up after this many weeks in a row failed (`None`: never): a
    /// station or parameter the API does not know fails every week.
    give_up_after: Option<usize>,
}

/// KNMI readings of the plan's days, by local date of their interval's end:
/// a week per request, cached once final. A week that fails to download is
/// left out (logged); only when no week came is it an error.
#[allow(clippy::too_many_arguments)]
async fn knmi_weeks<T, F, Fut>(
    plan: &MarketResearchPlan,
    weeks: KnmiWeeks<'_>,
    now: DateTime<Utc>,
    progress: &(dyn Fn(ResearchProgress) + Send + Sync),
    event: &dyn Fn(NaiveDate, NaiveDate) -> ResearchProgress,
    shutdown: &mut watch::Receiver<bool>,
    fetch: F,
    end_of: fn(&T) -> DateTime<Utc>,
) -> Result<BTreeMap<NaiveDate, Vec<T>>>
where
    T: Serialize + for<'de> Deserialize<'de>,
    F: Fn(DateTime<Utc>, DateTime<Utc>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, KnmiError>>,
{
    let tz = plan.spec.timezone;
    let today = wm_core::time::local_date(now, tz);
    let mut history: BTreeMap<NaiveDate, Vec<T>> = BTreeMap::new();
    let mut failed: Vec<String> = Vec::new();
    let mut failed_in_a_row = 0usize;
    let mut start = plan.from;
    while start <= plan.to {
        let end = (start + Duration::days(KNMI_CHUNK_DAYS - 1)).min(plan.to);
        progress(event(start, end));
        let path = weeks.dir.join(format!("{start}_{end}.json"));
        let cached: Option<Vec<T>> = read_json(&path);
        let readings = match cached {
            Some(r) => r,
            None => {
                let (from, _) = local_day_bounds(start, tz);
                let (_, to) = local_day_bounds(end, tz);
                let fetched = tokio::select! {
                    r = fetch(from, to) => r,
                    _ = shutdown.changed() => bail!("shutting down"),
                };
                // A week that will not download costs that week, not the
                // whole replay.
                let r = match fetched {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(from = %start, to = %end, what = weeks.what, error = %e, "KNMI readings of a week unavailable: replayed without them");
                        failed.push(format!("{start} → {end}: {e}"));
                        failed_in_a_row += 1;
                        if weeks.give_up_after.is_some_and(|n| failed_in_a_row >= n) {
                            break;
                        }
                        start = end + Duration::days(1);
                        continue;
                    }
                };
                if (today - end).num_days() >= KNMI_FINAL_AFTER_DAYS {
                    std::fs::create_dir_all(&weeks.dir)
                        .with_context(|| format!("creating {}", weeks.dir.display()))?;
                    training::write_atomic(&path, &serde_json::to_vec(&r)?)?;
                }
                r
            }
        };
        failed_in_a_row = 0;
        for r in readings {
            history
                .entry(wm_core::time::local_date(end_of(&r), tz))
                .or_default()
                .push(r);
        }
        start = end + Duration::days(1);
    }
    if history.is_empty()
        && let Some(first) = failed.first()
    {
        bail!(
            "no week of KNMI readings ({}) could be downloaded ({} failed; the first: {first})",
            weeks.what,
            failed.len()
        );
    }
    Ok(history)
}

/// KNMI's ten-minute readings of the plan's days for strategy K, by local
/// date.
async fn knmi_history(
    client: &KnmiTenMinute,
    plan: &MarketResearchPlan,
    location: &str,
    now: DateTime<Utc>,
    progress: &(dyn Fn(ResearchProgress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<KnmiHistory> {
    let station = &plan.train.station;
    let weeks = KnmiWeeks {
        dir: research_root(plan).join("knmi").join(station.as_str()),
        what: "strategy K",
        give_up_after: None,
    };
    let mut history = knmi_weeks(
        plan,
        weeks,
        now,
        progress,
        &|from, to| ResearchProgress::Knmi { from, to },
        shutdown,
        |from, to| client.fetch(station, location, from, to, MAX_GATE_WAIT, 3),
        |r: &TenMinuteObservation| r.interval_end,
    )
    .await?;
    for v in history.values_mut() {
        v.sort_by_key(|r| r.interval_end);
        v.dedup_by_key(|r| r.interval_end);
    }
    Ok(history)
}

/// KNMI readings of other parameters or stations for the strategy lab.
#[allow(clippy::too_many_arguments)]
async fn knmi_series(
    client: &KnmiTenMinute,
    plan: &MarketResearchPlan,
    location: &str,
    parameters: &[&str],
    what: &str,
    now: DateTime<Utc>,
    progress: &(dyn Fn(ResearchProgress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<SeriesHistory> {
    let weeks = KnmiWeeks {
        dir: research_root(plan)
            .join("knmi-series")
            .join(location)
            .join(parameters.join("-")),
        what,
        give_up_after: Some(2),
    };
    let mut history = knmi_weeks(
        plan,
        weeks,
        now,
        progress,
        &|from, to| ResearchProgress::KnmiSeries {
            what: what.to_owned(),
            from,
            to,
        },
        shutdown,
        |from, to| client.fetch_series(location, parameters, from, to, MAX_GATE_WAIT, 3),
        |p: &SeriesPoint| p.interval_end,
    )
    .await?;
    for v in history.values_mut() {
        v.sort_by_key(|p| p.interval_end);
        v.dedup_by_key(|p| p.interval_end);
    }
    history.retain(|_, v| !v.is_empty());
    Ok(history)
}

/// The strategy lab's own inputs, each optional: the day-1 forecast when the
/// model's evaluation did not load it, KNMI's global radiation at the
/// station and, at Schiphol, the neighbouring stations' temperatures. One
/// that will not download is logged and leaves its strategies unreplayed.
#[allow(clippy::too_many_arguments)]
async fn lab_inputs(
    clients: &MarketResearchClients<'_>,
    plan: &MarketResearchPlan,
    model_has_forecast: bool,
    now: DateTime<Utc>,
    hist_progress: &(dyn Fn(Progress) + Send + Sync),
    progress: &(dyn Fn(ResearchProgress) + Send + Sync),
    shutdown: &mut watch::Receiver<bool>,
) -> Result<LabInputs> {
    let mut lab = LabInputs::default();
    if !model_has_forecast
        && let (Some(fp), Some(client)) = (&plan.train.forecast, clients.forecast)
    {
        match training::forecast_history(client, &plan.train, fp, None, hist_progress, shutdown)
            .await
        {
            Ok(f) => {
                lab.forecasts = Some(ForecastHistory::from_hourly(
                    fp.product.clone(),
                    plan.train.tz,
                    &f.hourly,
                ));
            }
            Err(e) => {
                if *shutdown.borrow() {
                    bail!("shutting down");
                }
                tracing::warn!(error = %format!("{e:#}"), "forecast history unavailable: the lab's forecast strategies (L14–L17) are not replayed");
            }
        }
    }
    let (Some(client), Some(location)) = (clients.knmi, plan.knmi_location.as_deref()) else {
        return Ok(lab);
    };
    let lab_sim = &plan.study.sim.lab;
    let radiation = lab_sim.radiation_parameter.as_str();
    match knmi_series(
        client,
        plan,
        location,
        &[radiation],
        "global radiation",
        now,
        progress,
        shutdown,
    )
    .await
    {
        Ok(h) if !h.is_empty() => lab.radiation = Some(h),
        Ok(_) => tracing::warn!(
            parameter = radiation,
            "KNMI returned no radiation readings: L25 is not replayed"
        ),
        Err(e) => {
            if *shutdown.borrow() {
                bail!("shutting down");
            }
            tracing::warn!(error = %format!("{e:#}"), "KNMI radiation readings unavailable: L25 is not replayed");
        }
    }
    // The neighbours and their bearings are Schiphol's.
    if location != wigos_id("06240") {
        return Ok(lab);
    }
    let temperature = lab_sim.neighbour_parameter.as_str();
    for n in &lab_sim.neighbours {
        let what = format!("{} ({})", n.name, n.wmo);
        match knmi_series(
            client,
            plan,
            &wigos_id(&n.wmo),
            &[temperature],
            &what,
            now,
            progress,
            shutdown,
        )
        .await
        {
            Ok(h) if !h.is_empty() => {
                lab.neighbours.insert(n.wmo.clone(), h);
            }
            Ok(_) => {
                tracing::warn!(station = %what, "KNMI returned no readings of a neighbour: L24 is replayed without it")
            }
            Err(e) => {
                if *shutdown.borrow() {
                    bail!("shutting down");
                }
                tracing::warn!(station = %what, error = %format!("{e:#}"), "KNMI readings of a neighbour unavailable: L24 is replayed without it");
            }
        }
    }
    Ok(lab)
}
