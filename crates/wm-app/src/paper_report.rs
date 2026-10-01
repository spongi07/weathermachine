//! `report paper`: the paper run day by day, read from the database.
//!
//! The dashboard keeps only the last 100 decisions and forgets them on a
//! restart; the database keeps everything. For every local day this report
//! shows:
//!
//! * the weather: the METAR high, the reports and how late each was known
//!   (and from which source), and the day-1 forecast's error;
//! * every evaluation: per strategy, what blocked it and how often, and the
//!   closest calls with how they would have ended;
//! * the model against the market on the bucket that won;
//! * proposals with the risk verdicts, paper orders, fills and P&L;
//! * provider requests, health changes and logged events.
//!
//! Outcomes are judged by the METAR high, as paper settlement does: the
//! official resolution is not stored. The day in progress is provisional.

use crate::config::AppConfig;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;
use wm_core::event::ForecastEvent;
use wm_core::market::{TempUnit, TemperatureBucket};
use wm_core::time::{Clock, local_date, local_day_bounds};
use wm_core::units::TempC;
use wm_storage::{
    PgStore, ReportBookTop, ReportDecision, ReportFill, ReportHealthChange, ReportObservation,
    ReportOrder, ReportOutcome, ReportRequestDay, ReportRun, ReportSystemEvent,
};
use wm_strategy::{ForecastDay, IncrementDistribution};

/// A report is "caught up" rather than late when it was first fetched this
/// long after its observation: the service was not running (or the feed was
/// down) when it was published.
const CATCH_UP_SECS: i64 = 30 * 60;
/// Reports known later than this are listed one by one.
const LATE_SECS: i64 = 6 * 60;
/// A recorded book older than this does not say what the market thought.
const BOOK_MAX_AGE_MIN: i64 = 15;
/// Probabilities are clamped here for the log loss (a sure miss costs ~4.6).
const LOG_LOSS_FLOOR: f64 = 0.01;
/// Closest calls listed per strategy and day.
const CLOSEST_PER_STRATEGY: usize = 3;
/// Blocker patterns listed per strategy and day.
const BLOCKERS_PER_STRATEGY: usize = 4;
/// Rows listed per day for proposals, health changes and events.
const LIST_CAP: usize = 12;

/// What to report.
#[derive(Debug, Clone)]
pub struct PaperReportPlan {
    pub location: String,
    pub station: String,
    pub tz: Tz,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// The location a report is about: the first configured one.
#[derive(Debug, Clone)]
pub struct ReportTarget {
    pub location: String,
    pub station: String,
    pub tz: Tz,
}

impl ReportTarget {
    pub fn from_config(cfg: &AppConfig) -> Result<Self> {
        let loc = cfg.locations.first().context("no location configured")?;
        let tz =
            loc.location.timezone.parse::<Tz>().map_err(|e| {
                anyhow::anyhow!("invalid timezone '{}': {e}", loc.location.timezone)
            })?;
        Ok(Self {
            location: loc.location.id.clone(),
            station: loc.station.id.clone(),
            tz,
        })
    }

    /// Local dates `from` (default: the day the service first ran) to `to`
    /// (default and latest: today), at most `max_days` long if given (the
    /// latest days are kept).
    pub async fn plan(
        &self,
        store: &PgStore,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
        max_days: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<PaperReportPlan> {
        let today = local_date(now, self.tz);
        let to = to.unwrap_or(today).min(today);
        let mut from = match from {
            Some(f) => f,
            None => store
                .first_run_started_at()
                .await?
                .map_or(to, |t| local_date(t, self.tz)),
        };
        if from > to {
            bail!("the first day ({from}) is after the last ({to})");
        }
        if let Some(n) = max_days {
            from = from.max(to - Duration::days(n.max(1) - 1));
        }
        Ok(PaperReportPlan {
            location: self.location.clone(),
            station: self.station.clone(),
            tz: self.tz,
            from,
            to,
        })
    }
}

/// Serves the report from the running service (the dashboard's
/// `/api/v1/report/paper`): one report at a time, the latest reused for a
/// minute, at most [`ReportService::MAX_DAYS`] days.
pub struct ReportService {
    store: PgStore,
    target: ReportTarget,
    clock: Arc<dyn Clock>,
    last: tokio::sync::Mutex<Option<CachedReport>>,
}

struct CachedReport {
    made: std::time::Instant,
    key: (Option<NaiveDate>, Option<NaiveDate>),
    report: Arc<PaperReport>,
}

impl ReportService {
    pub const MAX_DAYS: i64 = 31;
    const REUSE: std::time::Duration = std::time::Duration::from_secs(60);

    pub fn new(store: PgStore, target: ReportTarget, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            target,
            clock,
            last: tokio::sync::Mutex::new(None),
        }
    }

    /// The report the dashboard links to: from the service's first day to
    /// today, at most [`ReportService::MAX_DAYS`] days.
    pub async fn latest(&self) -> Result<Arc<PaperReport>> {
        self.report(None, None).await
    }

    pub async fn report(
        &self,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
    ) -> Result<Arc<PaperReport>> {
        let mut last = self.last.lock().await;
        if let Some(c) = last.as_ref()
            && c.key == (from, to)
            && c.made.elapsed() < Self::REUSE
        {
            return Ok(Arc::clone(&c.report));
        }
        let now = self.clock.now();
        let plan = self
            .target
            .plan(&self.store, from, to, Some(Self::MAX_DAYS), now)
            .await?;
        let inputs = collect(&self.store, &plan).await?;
        let report = Arc::new(build(&inputs, &plan, now));
        *last = Some(CachedReport {
            made: std::time::Instant::now(),
            key: (from, to),
            report: Arc::clone(&report),
        });
        Ok(report)
    }
}

/// Everything the report reads (see [`collect`]).
#[derive(Debug, Clone, Default)]
pub struct ReportInputs {
    pub observations: Vec<ReportObservation>,
    pub runs: Vec<ReportRun>,
    pub forecasts: Vec<(DateTime<Utc>, ForecastEvent)>,
    pub decisions: Vec<ReportDecision>,
    pub outcomes: Vec<ReportOutcome>,
    /// Recorded book tops of each day's winning YES token, by token.
    pub books: HashMap<String, Vec<ReportBookTop>>,
    pub orders: Vec<ReportOrder>,
    pub fills: Vec<ReportFill>,
    pub requests: Vec<ReportRequestDay>,
    pub health: Vec<ReportHealthChange>,
    pub system: Vec<ReportSystemEvent>,
}

/// Read everything the plan's days need.
pub async fn collect(store: &PgStore, plan: &PaperReportPlan) -> Result<ReportInputs> {
    let (start, _) = local_day_bounds(plan.from, plan.tz);
    let (_, end) = local_day_bounds(plan.to, plan.tz);
    let observations = store.report_observations(&plan.station, start, end).await?;
    let outcomes = store
        .report_outcomes(&plan.location, plan.from, plan.to)
        .await?;
    let mut books = HashMap::new();
    for (date, high) in daily_highs(&observations, plan.tz) {
        if let Some(w) = winner(&outcomes, date, high) {
            let (s, e) = local_day_bounds(date, plan.tz);
            let tops = store.report_book_tops(&w.yes_token, s, e).await?;
            books.insert(w.yes_token.clone(), tops);
        }
    }
    Ok(ReportInputs {
        observations,
        runs: store.report_runs(start, end).await?,
        // A day's forecast may have been fetched the evening before.
        forecasts: store
            .report_forecasts(start - Duration::days(1), end)
            .await?,
        decisions: store.report_decisions(&plan.location, start, end).await?,
        outcomes,
        books,
        orders: store.report_orders(&plan.location, start, end).await?,
        fills: store.report_fills(&plan.location, start, end).await?,
        requests: store.report_requests(plan.tz.name(), start, end).await?,
        health: store.report_health_changes(start, end).await?,
        system: store.report_system_events(start, end).await?,
    })
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct PaperReport {
    pub generated_at: DateTime<Utc>,
    pub location: String,
    pub station: String,
    pub timezone: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub totals: Totals,
    pub days: Vec<DayReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayReport {
    pub date: NaiveDate,
    /// Today: outcomes are provisional.
    pub in_progress: bool,
    pub starts: Vec<RunStart>,
    pub weather: DayWeather,
    pub forecast: Option<DayForecast>,
    pub latency: Latency,
    pub market: Option<DayMarket>,
    /// Evaluation records (one per weather report while a market was open).
    pub evaluations: usize,
    pub strategies: Vec<StrategyDay>,
    pub model_vs_market: Option<ModelVsMarket>,
    pub proposals: Vec<ProposalRow>,
    pub orders: Vec<OrderRow>,
    pub fills: usize,
    /// Paper P&L of this day's market, settled at the METAR high (`None`:
    /// nothing traded, or the day is still open).
    pub pnl_usd: Option<f64>,
    /// The same per strategy (engine id; fills of orders placed before the
    /// report's first day count as `?`).
    pub strategy_pnl: BTreeMap<String, f64>,
    pub providers: Vec<ProviderDay>,
    pub health_changes: Vec<String>,
    pub events: Vec<EventCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunStart {
    pub at: String,
    pub version: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DayWeather {
    pub reports: usize,
    pub specis: usize,
    pub corrected: usize,
    /// Whole °C, rounded half up as the engine does.
    pub high_c: Option<i32>,
    /// Local time the high was first and last reported.
    pub high_first_at: Option<String>,
    pub high_last_at: Option<String>,
    pub first_report_at: Option<String>,
    pub last_report_at: Option<String>,
    /// Longest wait between two consecutive reports, in minutes.
    pub max_gap_min: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayForecast {
    pub product: String,
    pub received_at: DateTime<Utc>,
    pub day_max_c: f64,
    /// Forecast day maximum minus the METAR high.
    pub error_c: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Latency {
    /// Reports known within [`CATCH_UP_SECS`] of their observation.
    pub timely: usize,
    pub median_s: Option<i64>,
    pub p90_s: Option<i64>,
    pub max_s: Option<i64>,
    pub over_5min: usize,
    /// Reports first fetched long after publication (service down or
    /// restarting): left out of the delays.
    pub catch_up: usize,
    pub late: Vec<LateReport>,
    pub by_source: Vec<SourceShare>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LateReport {
    pub observed_at: String,
    pub delay_s: i64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceShare {
    pub source: String,
    /// Reports this source delivered first.
    pub first: usize,
    pub median_s: Option<i64>,
    pub failover: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayMarket {
    pub event_slug: String,
    pub buckets: usize,
    /// The bucket holding the METAR high.
    pub winner: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyDay {
    pub strategy: String,
    /// Bucket evaluations (lines) over the day.
    pub lines: usize,
    pub signals: Vec<Call>,
    pub blockers: Vec<BlockerCount>,
    pub closest: Vec<Call>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlockerCount {
    /// The blocker with its numbers replaced by `#`.
    pub pattern: String,
    pub count: usize,
    /// The latest occurrence, verbatim.
    pub example: String,
}

/// One bucket evaluation and how it would have ended.
#[derive(Debug, Clone, Serialize)]
pub struct Call {
    pub at: String,
    pub bucket: String,
    pub side: String,
    pub ask: Option<f64>,
    pub p: Option<f64>,
    pub model: Option<f64>,
    pub market: Option<f64>,
    pub ev: Option<f64>,
    pub verdict: String,
    /// Did this side win (METAR high)?
    pub won: Option<bool>,
    /// P&L per share had it been bought at the ask, taker fee included.
    pub pnl_per_share: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelVsMarket {
    pub bucket: String,
    pub samples: usize,
    /// Average probability each gave the winning bucket.
    pub model_mean: f64,
    pub market_mean: f64,
    pub model_log_loss: f64,
    pub market_log_loss: f64,
    /// Evaluations at which each gave the winner more probability.
    pub model_higher: usize,
    pub market_higher: usize,
    /// First local time each gave the winner at least 0.9.
    pub model_sure_at: Option<String>,
    pub market_sure_at: Option<String>,
    pub widest: Option<TrailPoint>,
    pub trail: Vec<TrailPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrailPoint {
    pub at: String,
    pub high_c: i32,
    pub model: f64,
    pub market: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProposalRow {
    pub at: String,
    pub strategy: String,
    pub summary: String,
    pub approved: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderRow {
    pub at: String,
    pub strategy: String,
    pub market_date: Option<NaiveDate>,
    pub bucket: String,
    pub outcome_side: String,
    pub side: String,
    pub limit: f64,
    pub shares: f64,
    pub status: String,
    pub filled: f64,
    pub avg_price: Option<f64>,
    pub fees_usd: f64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderDay {
    pub provider: String,
    pub requests: i64,
    pub failures: i64,
    pub throttled: i64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub max_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventCount {
    pub level: String,
    pub kind: String,
    pub count: usize,
    pub example: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub days: usize,
    pub evaluations: usize,
    /// Signals per strategy.
    pub signals: BTreeMap<String, usize>,
    pub proposals: usize,
    pub approved: usize,
    pub orders: usize,
    pub fills: usize,
    /// Settled paper P&L (days that are over).
    pub pnl_usd: f64,
    /// The same per strategy.
    pub pnl_by_strategy: BTreeMap<String, f64>,
    pub forecast_days: usize,
    pub forecast_mean_error_c: Option<f64>,
    pub forecast_mean_abs_error_c: Option<f64>,
    pub reports: usize,
    pub delay_median_s: Option<i64>,
    pub delay_p90_s: Option<i64>,
    pub delay_max_s: Option<i64>,
    pub catch_up: usize,
    /// Model against market on the winning bucket (days that are over).
    pub mvm_samples: usize,
    pub model_log_loss: Option<f64>,
    pub market_log_loss: Option<f64>,
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// Build the report from its inputs (pure).
pub fn build(inputs: &ReportInputs, plan: &PaperReportPlan, now: DateTime<Utc>) -> PaperReport {
    let today = local_date(now, plan.tz);
    let tokens = token_index(&inputs.outcomes);
    let highs: HashMap<NaiveDate, i32> = daily_highs(&inputs.observations, plan.tz)
        .into_iter()
        .collect();
    let pnl = settle(&inputs.fills, &tokens, &highs, today);
    let by_strategy = settle_by_strategy(&inputs.fills, &inputs.orders, &tokens, &highs, today);
    let mut days = Vec::new();
    let mut date = plan.from;
    while date <= plan.to {
        let mut day = build_day(inputs, plan, date, date >= today, &tokens, &pnl);
        day.strategy_pnl = by_strategy.get(&date).cloned().unwrap_or_default();
        days.push(day);
        match date.succ_opt() {
            Some(d) => date = d,
            None => break,
        }
    }
    let totals = totals(&days, inputs, plan);
    PaperReport {
        generated_at: now,
        location: plan.location.clone(),
        station: plan.station.clone(),
        timezone: plan.tz.name().to_owned(),
        from: plan.from,
        to: plan.to,
        totals,
        days,
    }
}

fn hhmm(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

fn bucket_of(o: &ReportOutcome) -> TemperatureBucket {
    TemperatureBucket {
        lower: o.lower,
        upper: o.upper,
        unit: TempUnit::Celsius,
    }
}

/// The outcome of `date` whose bucket holds `high`.
fn winner(outcomes: &[ReportOutcome], date: NaiveDate, high: i32) -> Option<&ReportOutcome> {
    outcomes
        .iter()
        .find(|o| o.local_date == date && bucket_of(o).contains(high))
}

/// The current version of every report, oldest first.
fn current_versions(obs: &[ReportObservation]) -> Vec<&ReportObservation> {
    let mut latest: BTreeMap<(DateTime<Utc>, &str), &ReportObservation> = BTreeMap::new();
    for o in obs {
        let key = (o.observed_at, o.report_type.as_str());
        if latest.get(&key).is_none_or(|l| o.version > l.version) {
            latest.insert(key, o);
        }
    }
    latest.into_values().collect()
}

/// METAR high (whole °C, rounded half up) per local date.
fn daily_highs(obs: &[ReportObservation], tz: Tz) -> Vec<(NaiveDate, i32)> {
    let mut highs: BTreeMap<NaiveDate, i32> = BTreeMap::new();
    for o in current_versions(obs) {
        if let Some(dc) = o.temperature_dc {
            let whole = TempC::from_tenths(dc).round_half_up_whole();
            let e = highs.entry(local_date(o.observed_at, tz)).or_insert(whole);
            *e = (*e).max(whole);
        }
    }
    highs.into_iter().collect()
}

/// What a token is: its market's date, bucket label and side.
#[derive(Debug, Clone)]
struct TokenInfo {
    date: NaiveDate,
    label: String,
    yes: bool,
    bucket: TemperatureBucket,
}

impl TokenInfo {
    fn wins(&self, high: i32) -> bool {
        self.bucket.contains(high) == self.yes
    }
}

fn token_index(outcomes: &[ReportOutcome]) -> HashMap<String, TokenInfo> {
    let mut idx = HashMap::new();
    for o in outcomes {
        for (token, yes) in [(&o.yes_token, true), (&o.no_token, false)] {
            idx.insert(
                token.clone(),
                TokenInfo {
                    date: o.local_date,
                    label: o.label.clone(),
                    yes,
                    bucket: bucket_of(o),
                },
            );
        }
    }
    idx
}

/// Settled paper P&L per market date: cash flows of every fill plus the
/// shares still held, paid out at the METAR high. Days not over are left out.
fn settle(
    fills: &[ReportFill],
    tokens: &HashMap<String, TokenInfo>,
    highs: &HashMap<NaiveDate, i32>,
    today: NaiveDate,
) -> HashMap<NaiveDate, f64> {
    let mut held: HashMap<&str, (f64, f64)> = HashMap::new(); // token → (shares, cash)
    for f in fills {
        let shares = f.shares_micros as f64 / 1e6;
        let price = f64::from(f.price_micros) / 1e6;
        let fee = f.fee_micros as f64 / 1e6;
        let e = held.entry(f.token_id.as_str()).or_insert((0.0, 0.0));
        if f.side.eq_ignore_ascii_case("buy") {
            e.0 += shares;
            e.1 -= shares * price + fee;
        } else {
            e.0 -= shares;
            e.1 += shares * price - fee;
        }
    }
    let mut out: HashMap<NaiveDate, f64> = HashMap::new();
    for (token, (shares, cash)) in held {
        let Some(info) = tokens.get(token) else {
            continue;
        };
        if info.date >= today {
            continue;
        }
        let Some(&high) = highs.get(&info.date) else {
            continue;
        };
        let payout = if info.wins(high) { shares } else { 0.0 };
        *out.entry(info.date).or_insert(0.0) += cash + payout;
    }
    out
}

/// [`settle`] per strategy: each fill counts for its order's strategy.
fn settle_by_strategy(
    fills: &[ReportFill],
    orders: &[ReportOrder],
    tokens: &HashMap<String, TokenInfo>,
    highs: &HashMap<NaiveDate, i32>,
    today: NaiveDate,
) -> HashMap<NaiveDate, BTreeMap<String, f64>> {
    let strategy_of: HashMap<&str, &str> = orders
        .iter()
        .map(|o| (o.client_order_id.as_str(), o.strategy.as_str()))
        .collect();
    let mut groups: BTreeMap<&str, Vec<ReportFill>> = BTreeMap::new();
    for f in fills {
        let s = strategy_of
            .get(f.client_order_id.as_str())
            .copied()
            .unwrap_or("?");
        groups.entry(s).or_default().push(f.clone());
    }
    let mut out: HashMap<NaiveDate, BTreeMap<String, f64>> = HashMap::new();
    for (strategy, fills) in groups {
        for (date, pnl) in settle(&fills, tokens, highs, today) {
            out.entry(date)
                .or_default()
                .insert(strategy.to_owned(), pnl);
        }
    }
    out
}

fn build_day(
    inputs: &ReportInputs,
    plan: &PaperReportPlan,
    date: NaiveDate,
    in_progress: bool,
    tokens: &HashMap<String, TokenInfo>,
    pnl: &HashMap<NaiveDate, f64>,
) -> DayReport {
    let tz = plan.tz;
    let (start, end) = local_day_bounds(date, tz);
    let within = |t: DateTime<Utc>| t >= start && t < end;

    let obs: Vec<ReportObservation> = inputs
        .observations
        .iter()
        .filter(|o| within(o.observed_at))
        .cloned()
        .collect();
    let weather = day_weather(&obs, tz);
    let high = weather.high_c;
    let forecast = day_forecast(&inputs.forecasts, plan, date, high);
    let latency = latency(&obs, tz);

    let outcomes: Vec<&ReportOutcome> = inputs
        .outcomes
        .iter()
        .filter(|o| o.local_date == date)
        .collect();
    let won = high.and_then(|h| outcomes.iter().find(|o| bucket_of(o).contains(h)).copied());
    let market = outcomes.first().map(|o| DayMarket {
        event_slug: o.event_slug.clone(),
        buckets: outcomes.len(),
        winner: won.map(|w| w.label.clone()),
    });
    let buckets: HashMap<&str, TemperatureBucket> = outcomes
        .iter()
        .map(|o| (o.label.as_str(), bucket_of(o)))
        .collect();
    let fee_rate = outcomes
        .first()
        .map_or(0.0, |o| f64::from(o.taker_fee_rate_micros) / 1e6);

    let decisions: Vec<&ReportDecision> =
        inputs.decisions.iter().filter(|d| within(d.at)).collect();
    let evaluations: Vec<&ReportDecision> = decisions
        .iter()
        .filter(|d| d.strategy == "evaluation")
        .copied()
        .collect();
    let strategies = strategy_days(&evaluations, &buckets, high, fee_rate, tz);
    let model_vs_market = won.and_then(|w| {
        let tops = inputs.books.get(&w.yes_token)?;
        model_vs_market(&evaluations, &w.label, &bucket_of(w), tops, tz)
    });
    let proposals = decisions
        .iter()
        .filter(|d| d.strategy != "evaluation")
        .map(|d| ProposalRow {
            at: hhmm(d.at, tz),
            strategy: d.strategy.clone(),
            summary: d.summary.clone(),
            approved: d.approved,
            reasons: d.reasons.clone(),
        })
        .collect();
    let orders = inputs
        .orders
        .iter()
        .filter(|o| within(o.created_at))
        .map(|o| {
            let info = tokens.get(&o.token_id);
            OrderRow {
                at: hhmm(o.created_at, tz),
                strategy: o.strategy.clone(),
                market_date: info.map(|i| i.date),
                bucket: info.map_or_else(|| o.token_id.clone(), |i| i.label.clone()),
                outcome_side: o.outcome_side.clone(),
                side: o.side.clone(),
                limit: f64::from(o.limit_price_micros) / 1e6,
                shares: o.shares_micros as f64 / 1e6,
                status: o.status.clone(),
                filled: o.filled_micros as f64 / 1e6,
                avg_price: o.avg_price_micros.map(|p| f64::from(p) / 1e6),
                fees_usd: o.fees_micros as f64 / 1e6,
                reason: o.reason.clone(),
            }
        })
        .collect();
    let fills = inputs.fills.iter().filter(|f| within(f.ts)).count();
    let providers = inputs
        .requests
        .iter()
        .filter(|r| r.date == date)
        .map(|r| ProviderDay {
            provider: r.provider.clone(),
            requests: r.requests,
            failures: r.failures,
            throttled: r.throttled,
            p50_ms: r.p50_ms,
            p90_ms: r.p90_ms,
            max_ms: r.max_ms,
        })
        .collect();
    let health_changes = inputs
        .health
        .iter()
        .filter(|h| within(h.at))
        .map(|h| {
            format!(
                "{} {} {} → {} ({})",
                hhmm(h.at, tz),
                h.provider,
                h.previous_state.as_deref().unwrap_or("?"),
                h.state,
                h.reason
            )
        })
        .collect();
    let mut events: BTreeMap<(String, String), EventCount> = BTreeMap::new();
    for e in inputs.system.iter().filter(|e| within(e.at)) {
        let c = events
            .entry((e.level.clone(), e.kind.clone()))
            .or_insert_with(|| EventCount {
                level: e.level.clone(),
                kind: e.kind.clone(),
                count: 0,
                example: String::new(),
            });
        c.count += 1;
        c.example = format!("{} {}", hhmm(e.at, tz), e.message);
    }
    let starts = inputs
        .runs
        .iter()
        .filter(|r| within(r.started_at))
        .map(|r| RunStart {
            at: hhmm(r.started_at, tz),
            version: r.version.clone(),
            model: r.model_id.clone(),
        })
        .collect();
    DayReport {
        date,
        in_progress,
        starts,
        weather,
        forecast,
        latency,
        market,
        evaluations: evaluations.len(),
        strategies,
        model_vs_market,
        proposals,
        orders,
        fills,
        pnl_usd: pnl.get(&date).copied(),
        strategy_pnl: BTreeMap::new(),
        providers,
        health_changes,
        events: events.into_values().collect(),
    }
}

fn day_weather(obs: &[ReportObservation], tz: Tz) -> DayWeather {
    let current = current_versions(obs);
    let mut w = DayWeather {
        reports: current.len(),
        specis: current.iter().filter(|o| o.report_type == "SPECI").count(),
        corrected: current.iter().filter(|o| o.version > 1).count(),
        first_report_at: current.first().map(|o| hhmm(o.observed_at, tz)),
        last_report_at: current.last().map(|o| hhmm(o.observed_at, tz)),
        max_gap_min: current
            .windows(2)
            .map(|p| (p[1].observed_at - p[0].observed_at).num_minutes())
            .max(),
        ..DayWeather::default()
    };
    let whole = |o: &ReportObservation| {
        o.temperature_dc
            .map(|dc| TempC::from_tenths(dc).round_half_up_whole())
    };
    w.high_c = current.iter().filter_map(|o| whole(o)).max();
    if let Some(h) = w.high_c {
        let at_high: Vec<&&ReportObservation> =
            current.iter().filter(|o| whole(o) == Some(h)).collect();
        w.high_first_at = at_high.first().map(|o| hhmm(o.observed_at, tz));
        w.high_last_at = at_high.last().map(|o| hhmm(o.observed_at, tz));
    }
    w
}

fn day_forecast(
    forecasts: &[(DateTime<Utc>, ForecastEvent)],
    plan: &PaperReportPlan,
    date: NaiveDate,
    high: Option<i32>,
) -> Option<DayForecast> {
    let (start, end) = local_day_bounds(date, plan.tz);
    let relevant = || {
        forecasts
            .iter()
            .rev()
            .filter(|(at, _)| *at < end && *at >= start - Duration::days(1))
            .filter(|(_, f)| f.location.as_str() == plan.location)
    };
    // The newest series covering the whole day, as a forecast of its maximum.
    let day_max = |(at, f): &(DateTime<Utc>, ForecastEvent)| {
        let series: Vec<(DateTime<Utc>, i32)> =
            f.hourly.iter().map(|(t, v)| (*t, v.tenths())).collect();
        let max = ForecastDay::from_series(date, plan.tz, &series, *at)?.day_max_tenths()?;
        Some(DayForecast {
            product: match f.lead_days {
                Some(d) => format!("{}/{}/d{d}", f.provider, f.model),
                None => format!("{}/{}", f.provider, f.model),
            },
            received_at: *at,
            day_max_c: f64::from(max) / 10.0,
            error_c: high.map(|h| f64::from(max - h * 10) / 10.0),
        })
    };
    // Prefer the fixed-lead day-1 product the model is trained on.
    relevant()
        .filter(|(_, f)| f.lead_days == Some(1))
        .find_map(day_max)
        .or_else(|| relevant().find_map(day_max))
}

fn median(sorted: &[i64]) -> Option<i64> {
    percentile(sorted, 0.5)
}

/// Nearest-rank percentile of an ascending slice.
fn percentile(sorted: &[i64], q: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn latency(obs: &[ReportObservation], tz: Tz) -> Latency {
    let mut delays = Vec::new();
    let mut catch_up = 0;
    let mut late = Vec::new();
    let mut by_source: BTreeMap<&str, (Vec<i64>, usize)> = BTreeMap::new();
    for o in obs.iter().filter(|o| o.version == 1) {
        let d = (o.fetched_at - o.observed_at).num_seconds();
        if d > CATCH_UP_SECS {
            catch_up += 1;
            continue;
        }
        delays.push(d);
        let e = by_source.entry(o.provider.as_str()).or_default();
        e.0.push(d);
        if o.from_failover {
            e.1 += 1;
        }
        if d > LATE_SECS {
            late.push(LateReport {
                observed_at: hhmm(o.observed_at, tz),
                delay_s: d,
                source: o.provider.clone(),
            });
        }
    }
    delays.sort_unstable();
    late.sort_by(|a, b| b.delay_s.cmp(&a.delay_s));
    Latency {
        timely: delays.len(),
        median_s: median(&delays),
        p90_s: percentile(&delays, 0.9),
        max_s: delays.last().copied(),
        over_5min: delays.iter().filter(|d| **d > 300).count(),
        catch_up,
        late,
        by_source: by_source
            .into_iter()
            .map(|(source, (mut v, failover))| {
                v.sort_unstable();
                SourceShare {
                    source: source.to_owned(),
                    first: v.len(),
                    median_s: median(&v),
                    failover,
                }
            })
            .collect(),
    }
}

/// One parsed evaluation line, as the engine writes it:
/// `A 25°C YES · ask 0.94 · p 0.895 (model 0.910, market 0.925) · EV -0.0529 — blocker; blocker`.
#[derive(Debug, Clone, PartialEq)]
struct EvalLine {
    tag: String,
    bucket: String,
    side: String,
    ask: Option<f64>,
    p: Option<f64>,
    model: Option<f64>,
    market: Option<f64>,
    ev: Option<f64>,
    signal: bool,
    verdict: String,
}

fn parse_line(line: &str) -> Option<EvalLine> {
    let (head, verdict) = line.split_once(" — ")?;
    let mut parts = head.split(" · ");
    let (tag, rest) = parts.next()?.split_once(' ')?;
    let (bucket, side) = rest.rsplit_once(' ')?;
    let mut e = EvalLine {
        tag: tag.to_owned(),
        bucket: bucket.to_owned(),
        side: side.to_owned(),
        ask: None,
        p: None,
        model: None,
        market: None,
        ev: None,
        signal: verdict.trim() == "SIGNAL",
        verdict: verdict.trim().to_owned(),
    };
    for part in parts {
        if let Some(a) = part.strip_prefix("ask ") {
            e.ask = a.trim().parse().ok();
        } else if let Some(v) = part.strip_prefix("EV ") {
            e.ev = v.trim().parse().ok();
        } else if let Some(p) = part.strip_prefix("p ") {
            let (value, extra) = match p.split_once(" (") {
                Some((v, x)) => (v, x.trim_end_matches(')')),
                None => (p, ""),
            };
            e.p = value.trim().parse().ok();
            for kv in extra.split(", ") {
                if let Some(v) = kv.strip_prefix("model ") {
                    e.model = v.trim().parse().ok();
                } else if let Some(v) = kv.strip_prefix("market ") {
                    e.market = v.trim().parse().ok();
                }
            }
        }
    }
    Some(e)
}

/// A blocker with its numbers replaced by `#`, so repeats group together.
fn blocker_pattern(b: &str) -> String {
    let mut out = String::with_capacity(b.len());
    let mut chars = b.chars().peekable();
    let mut prev_alnum = false;
    while let Some(c) = chars.next() {
        let starts_number = c.is_ascii_digit()
            || (c == '-' && !prev_alnum && chars.peek().is_some_and(char::is_ascii_digit));
        if starts_number {
            while chars
                .peek()
                .is_some_and(|n| n.is_ascii_digit() || *n == '.')
            {
                chars.next();
            }
            out.push('#');
            prev_alnum = true;
        } else {
            out.push(c);
            prev_alnum = c.is_alphanumeric();
        }
    }
    out
}

/// Polymarket's taker fee per share at price `p`.
fn taker_fee(rate: f64, p: f64) -> f64 {
    rate * p * (1.0 - p)
}

fn call(
    at: DateTime<Utc>,
    l: &EvalLine,
    buckets: &HashMap<&str, TemperatureBucket>,
    high: Option<i32>,
    fee_rate: f64,
    tz: Tz,
) -> Call {
    let won = match (buckets.get(l.bucket.as_str()), high) {
        (Some(b), Some(h)) => Some(b.contains(h) == (l.side == "YES")),
        _ => None,
    };
    let pnl_per_share = match (won, l.ask) {
        (Some(w), Some(a)) => Some(f64::from(u8::from(w)) - a - taker_fee(fee_rate, a)),
        _ => None,
    };
    Call {
        at: hhmm(at, tz),
        bucket: l.bucket.clone(),
        side: l.side.clone(),
        ask: l.ask,
        p: l.p,
        model: l.model,
        market: l.market,
        ev: l.ev,
        verdict: l.verdict.clone(),
        won,
        pnl_per_share,
    }
}

/// How close a line came to trading, compared as a pair (higher is closer).
type Closeness = (f64, f64);

/// The [`Closeness`] of an evaluation line.
/// A, B and D trade on a model edge, so their EV ranks them. E and F trade
/// on a price rule and claim no edge: fewer blockers first, then the higher
/// ask, since their trigger is the ask rising into a range.
fn closeness(l: &EvalLine) -> Option<Closeness> {
    match l.tag.as_str() {
        "E" | "F" => {
            let blockers = if l.signal {
                0
            } else {
                l.verdict.split("; ").filter(|b| !b.is_empty()).count()
            };
            Some((-(blockers as f64), l.ask?))
        }
        _ => Some((l.ev?, 0.0)),
    }
}

fn by_closeness(a: Closeness, b: Closeness) -> std::cmp::Ordering {
    a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
}

fn strategy_days(
    evaluations: &[&ReportDecision],
    buckets: &HashMap<&str, TemperatureBucket>,
    high: Option<i32>,
    fee_rate: f64,
    tz: Tz,
) -> Vec<StrategyDay> {
    #[derive(Default)]
    struct Acc {
        lines: usize,
        signals: Vec<Call>,
        blockers: BTreeMap<String, (usize, String)>,
        best: BTreeMap<(String, String), (Closeness, DateTime<Utc>, EvalLine)>,
    }
    let mut by_tag: BTreeMap<String, Acc> = BTreeMap::new();
    for d in evaluations {
        let lines = d
            .outputs
            .get("evaluations")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .filter_map(parse_line);
        for l in lines {
            let acc = by_tag.entry(l.tag.clone()).or_default();
            acc.lines += 1;
            if l.signal {
                acc.signals
                    .push(call(d.at, &l, buckets, high, fee_rate, tz));
            } else {
                for b in l.verdict.split("; ").filter(|b| !b.is_empty()) {
                    let e = acc
                        .blockers
                        .entry(blocker_pattern(b))
                        .or_insert((0, String::new()));
                    e.0 += 1;
                    e.1 = b.to_owned();
                }
            }
            if let Some(c) = closeness(&l) {
                let key = (l.bucket.clone(), l.side.clone());
                if acc
                    .best
                    .get(&key)
                    .is_none_or(|(best, ..)| by_closeness(c, *best).is_gt())
                {
                    acc.best.insert(key, (c, d.at, l));
                }
            }
        }
    }
    by_tag
        .into_iter()
        .map(|(tag, acc)| {
            let mut blockers: Vec<BlockerCount> = acc
                .blockers
                .into_iter()
                .map(|(pattern, (count, example))| BlockerCount {
                    pattern,
                    count,
                    example,
                })
                .collect();
            blockers.sort_by(|a, b| b.count.cmp(&a.count).then(a.pattern.cmp(&b.pattern)));
            blockers.truncate(BLOCKERS_PER_STRATEGY);
            let mut best: Vec<(Closeness, DateTime<Utc>, EvalLine)> =
                acc.best.into_values().collect();
            best.sort_by(|a, b| by_closeness(b.0, a.0));
            let closest = best
                .iter()
                .take(CLOSEST_PER_STRATEGY)
                .map(|(_, at, l)| call(*at, l, buckets, high, fee_rate, tz))
                .collect();
            StrategyDay {
                strategy: tag,
                lines: acc.lines,
                signals: acc.signals,
                blockers,
                closest,
            }
        })
        .collect()
}

/// The model's view in an evaluation record: `(high_whole, probabilities)`
/// of the unfiltered view, else the first view with a distribution.
fn model_view(inputs: &serde_json::Value) -> Option<(i32, Vec<f64>)> {
    let views = inputs.get("views")?.as_array()?;
    let parse = |v: &serde_json::Value| -> Option<(i32, Vec<f64>)> {
        let high = i32::try_from(v.get("high_whole")?.as_i64()?).ok()?;
        let p: Vec<f64> = v
            .get("p")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .collect();
        (!p.is_empty()).then_some((high, p))
    };
    views
        .iter()
        .find(|v| v.get("view").and_then(|x| x.as_str()) == Some("all"))
        .and_then(parse)
        .or_else(|| views.iter().find_map(parse))
}

/// The market's probability at `at`: the midpoint of the latest recorded
/// book (or its one side), if it is recent enough.
fn market_p(tops: &[ReportBookTop], at: DateTime<Utc>) -> Option<f64> {
    let i = tops.partition_point(|t| t.captured_at <= at);
    let t = tops.get(i.checked_sub(1)?)?;
    if at - t.captured_at > Duration::minutes(BOOK_MAX_AGE_MIN) {
        return None;
    }
    let (bid, ask) = (
        t.bid_micros.map(|b| f64::from(b) / 1e6),
        t.ask_micros.map(|a| f64::from(a) / 1e6),
    );
    match (bid, ask) {
        (Some(b), Some(a)) => Some((b + a) / 2.0),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

fn log_loss(p: f64) -> f64 {
    -p.clamp(LOG_LOSS_FLOOR, 1.0).ln()
}

fn model_vs_market(
    evaluations: &[&ReportDecision],
    label: &str,
    bucket: &TemperatureBucket,
    tops: &[ReportBookTop],
    tz: Tz,
) -> Option<ModelVsMarket> {
    let mut points: Vec<(DateTime<Utc>, TrailPoint)> = Vec::new();
    for d in evaluations {
        let Some((high, probs)) = model_view(&d.inputs) else {
            continue;
        };
        let Some(market) = market_p(tops, d.at) else {
            continue;
        };
        let dist = IncrementDistribution {
            probs,
            support: 0,
            source: String::new(),
        };
        points.push((
            d.at,
            TrailPoint {
                at: hhmm(d.at, tz),
                high_c: high,
                model: dist.p_in_bucket_lower(high, bucket),
                market,
            },
        ));
    }
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    let mean = |f: &dyn Fn(&TrailPoint) -> f64| points.iter().map(|(_, p)| f(p)).sum::<f64>() / n;
    let sure = |f: &dyn Fn(&TrailPoint) -> f64| {
        points
            .iter()
            .find(|(_, p)| f(p) >= 0.9)
            .map(|(_, p)| p.at.clone())
    };
    // About six points spread over the day, always including the last.
    let step = points.len().div_ceil(6).max(1);
    let mut trail: Vec<TrailPoint> = points
        .iter()
        .step_by(step)
        .map(|(_, p)| p.clone())
        .collect();
    if let Some((_, last)) = points.last()
        && trail.last().is_none_or(|t| t.at != last.at)
    {
        trail.push(last.clone());
    }
    Some(ModelVsMarket {
        bucket: label.to_owned(),
        samples: points.len(),
        model_mean: mean(&|p| p.model),
        market_mean: mean(&|p| p.market),
        model_log_loss: mean(&|p| log_loss(p.model)),
        market_log_loss: mean(&|p| log_loss(p.market)),
        model_higher: points.iter().filter(|(_, p)| p.model > p.market).count(),
        market_higher: points.iter().filter(|(_, p)| p.market > p.model).count(),
        model_sure_at: sure(&|p| p.model),
        market_sure_at: sure(&|p| p.market),
        widest: points
            .iter()
            .max_by(|a, b| {
                (a.1.model - a.1.market)
                    .abs()
                    .total_cmp(&(b.1.model - b.1.market).abs())
            })
            .map(|(_, p)| p.clone()),
        trail,
    })
}

fn totals(days: &[DayReport], inputs: &ReportInputs, plan: &PaperReportPlan) -> Totals {
    let mut t = Totals {
        days: days.len(),
        ..Totals::default()
    };
    let mut errors = Vec::new();
    let (mut ll_model, mut ll_market) = (0.0, 0.0);
    for d in days {
        t.evaluations += d.evaluations;
        for s in &d.strategies {
            *t.signals.entry(s.strategy.clone()).or_default() += s.signals.len();
        }
        t.proposals += d.proposals.len();
        t.approved += d.proposals.iter().filter(|p| p.approved).count();
        t.orders += d.orders.len();
        t.fills += d.fills;
        t.pnl_usd += d.pnl_usd.unwrap_or(0.0);
        for (k, v) in &d.strategy_pnl {
            *t.pnl_by_strategy.entry(k.clone()).or_default() += v;
        }
        if let Some(e) = d.forecast.as_ref().and_then(|f| f.error_c)
            && !d.in_progress
        {
            errors.push(e);
        }
        t.catch_up += d.latency.catch_up;
        if let Some(m) = d.model_vs_market.as_ref().filter(|_| !d.in_progress) {
            t.mvm_samples += m.samples;
            ll_model += m.model_log_loss * m.samples as f64;
            ll_market += m.market_log_loss * m.samples as f64;
        }
    }
    t.forecast_days = errors.len();
    if !errors.is_empty() {
        let n = errors.len() as f64;
        t.forecast_mean_error_c = Some(errors.iter().sum::<f64>() / n);
        t.forecast_mean_abs_error_c = Some(errors.iter().map(|e| e.abs()).sum::<f64>() / n);
    }
    if t.mvm_samples > 0 {
        t.model_log_loss = Some(ll_model / t.mvm_samples as f64);
        t.market_log_loss = Some(ll_market / t.mvm_samples as f64);
    }
    let (start, _) = local_day_bounds(plan.from, plan.tz);
    let (_, end) = local_day_bounds(plan.to, plan.tz);
    let mut delays: Vec<i64> = inputs
        .observations
        .iter()
        .filter(|o| o.version == 1 && o.observed_at >= start && o.observed_at < end)
        .map(|o| (o.fetched_at - o.observed_at).num_seconds())
        .filter(|d| *d <= CATCH_UP_SECS)
        .collect();
    delays.sort_unstable();
    t.reports = delays.len();
    t.delay_median_s = median(&delays);
    t.delay_p90_s = percentile(&delays, 0.9);
    t.delay_max_s = delays.last().copied();
    t
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// `$+1.20 (A_buy_yes_final_high $+1.20)`: a total with its split per
/// strategy, when there is one.
fn with_split(total: String, split: &BTreeMap<String, f64>) -> String {
    if split.is_empty() {
        return total;
    }
    let parts: Vec<String> = split
        .iter()
        .map(|(k, v)| format!("{k} {}", usd(*v)))
        .collect();
    format!("{total} ({})", parts.join(", "))
}

fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "–".to_owned(), |x| x.to_string())
}

pub(crate) fn p2(v: Option<f64>) -> String {
    v.map_or_else(|| "–".to_owned(), |x| format!("{x:.2}"))
}

fn secs(v: Option<i64>) -> String {
    v.map_or_else(|| "–".to_owned(), |s| format!("{s} s"))
}

pub(crate) fn usd(v: f64) -> String {
    // Avoid "-0.00" for an exact break-even.
    let v = if v.abs() < 0.005 { 0.0 } else { v };
    if v < 0.0 {
        format!("−${:.2}", -v)
    } else {
        format!("${v:.2}")
    }
}

pub(crate) fn call_text(c: &Call) -> String {
    let mut s = format!("{} {} {}", c.at, c.bucket, c.side);
    if let Some(a) = c.ask {
        let _ = write!(s, " · ask {a:.2}");
    }
    if let Some(p) = c.p {
        let _ = write!(s, " · p {p:.3}");
    }
    match (c.model, c.market) {
        (Some(m), Some(k)) => {
            let _ = write!(s, " (model {m:.3}, market {k:.3})");
        }
        (None, Some(k)) => {
            let _ = write!(s, " (market {k:.3})");
        }
        _ => {}
    }
    if let Some(ev) = c.ev {
        let _ = write!(s, " · EV {ev:+.4}");
    }
    let _ = write!(s, " — {}", c.verdict);
    match (c.won, c.pnl_per_share) {
        (Some(true), Some(p)) => {
            let _ = write!(s, " → **won** ({p:+.3}/share at the ask)");
        }
        (Some(false), Some(p)) => {
            let _ = write!(s, " → **lost** ({p:+.3}/share at the ask)");
        }
        (Some(w), None) => {
            let _ = write!(s, " → **{}**", if w { "won" } else { "lost" });
        }
        _ => {}
    }
    s
}

/// Render the report as Markdown.
pub fn markdown(r: &PaperReport) -> String {
    let mut s = String::new();
    let t = &r.totals;
    let _ = writeln!(
        s,
        "# Paper run — {} ({}), {} → {}\n",
        r.location, r.station, r.from, r.to
    );
    let _ = writeln!(
        s,
        "Generated {} UTC. Times are local ({}). Outcomes are judged by the METAR high, as paper settlement does; today's are provisional.\n",
        r.generated_at.format("%Y-%m-%d %H:%M"),
        r.timezone
    );
    let _ = writeln!(s, "## Summary\n");
    let signals: Vec<String> = t
        .signals
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{k} {n}"))
        .collect();
    let _ = writeln!(
        s,
        "- {} day(s), {} evaluations, signals: {}; {} proposals ({} approved), {} orders, {} fills, settled paper P&L {}.",
        t.days,
        t.evaluations,
        if signals.is_empty() {
            "none".to_owned()
        } else {
            signals.join(", ")
        },
        t.proposals,
        t.approved,
        t.orders,
        t.fills,
        with_split(usd(t.pnl_usd), &t.pnl_by_strategy)
    );
    let _ = writeln!(
        s,
        "- Reports: {} known on time, median delay {}, 90th percentile {}, slowest {}; {} caught up after a restart or outage (left out).",
        t.reports,
        secs(t.delay_median_s),
        secs(t.delay_p90_s),
        secs(t.delay_max_s),
        t.catch_up
    );
    match (t.forecast_mean_error_c, t.forecast_mean_abs_error_c) {
        (Some(bias), Some(mae)) => {
            let _ = writeln!(
                s,
                "- Day-1 forecast against the METAR high over {} finished day(s): mean error {bias:+.1} °C, mean absolute error {mae:.1} °C.",
                t.forecast_days
            );
        }
        _ => {
            let _ = writeln!(s, "- Day-1 forecast: no finished day with a forecast.");
        }
    }
    match (t.model_log_loss, t.market_log_loss) {
        (Some(m), Some(k)) => {
            let _ = writeln!(
                s,
                "- Model against market on the winning bucket (finished days, {} evaluations): log loss model {m:.3}, market {k:.3} — lower is better.",
                t.mvm_samples
            );
        }
        _ => {
            let _ = writeln!(
                s,
                "- Model against market: no finished day with both a model distribution and a recorded book."
            );
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "| Day | METAR high | Forecast (error) | Reports · median delay · slowest | Evaluations | Signals | Proposals (approved) | Orders · fills | P&L | Winner: model / market |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
    for d in &r.days {
        let high = match (&d.weather.high_c, &d.weather.high_first_at) {
            (Some(h), Some(at)) => format!("{h} °C ({at})"),
            (Some(h), None) => format!("{h} °C"),
            _ => "–".to_owned(),
        };
        let fc = d.forecast.as_ref().map_or_else(
            || "–".to_owned(),
            |f| match f.error_c {
                Some(e) => format!("{:.1} ({e:+.1})", f.day_max_c),
                None => format!("{:.1}", f.day_max_c),
            },
        );
        let sig: Vec<String> = d
            .strategies
            .iter()
            .filter(|x| !x.signals.is_empty())
            .map(|x| format!("{} {}", x.strategy, x.signals.len()))
            .collect();
        let mvm = d.model_vs_market.as_ref().map_or_else(
            || "–".to_owned(),
            |m| format!("{:.2} / {:.2}", m.model_mean, m.market_mean),
        );
        let _ = writeln!(
            s,
            "| {}{} | {high} | {fc} | {} · {} · {} | {} | {} | {} ({}) | {} · {} | {} | {mvm} |",
            d.date,
            if d.in_progress { " (today)" } else { "" },
            d.weather.reports,
            secs(d.latency.median_s),
            secs(d.latency.max_s),
            d.evaluations,
            if sig.is_empty() {
                "none".to_owned()
            } else {
                sig.join(", ")
            },
            d.proposals.len(),
            d.proposals.iter().filter(|p| p.approved).count(),
            d.orders.len(),
            d.fills,
            d.pnl_usd.map_or_else(|| "–".to_owned(), usd),
        );
    }
    let _ = writeln!(s);
    for d in &r.days {
        day_markdown(&mut s, d);
    }
    s
}

fn day_markdown(s: &mut String, d: &DayReport) {
    let _ = writeln!(
        s,
        "## {} {}{}\n",
        d.date,
        d.date.format("%a"),
        if d.in_progress {
            " — in progress (provisional)"
        } else {
            ""
        }
    );
    if !d.starts.is_empty() {
        let starts: Vec<String> = d
            .starts
            .iter()
            .map(|r| format!("{} (v{}, model {})", r.at, r.version, r.model))
            .collect();
        let _ = writeln!(s, "- **Service started:** {}", starts.join("; "));
    }
    let w = &d.weather;
    let _ = writeln!(
        s,
        "- **Weather:** {} reports ({} SPECI, {} corrected), {} → {}, longest gap {} min. METAR high {}{}.",
        w.reports,
        w.specis,
        w.corrected,
        opt(w.first_report_at.as_deref()),
        opt(w.last_report_at.as_deref()),
        opt(w.max_gap_min),
        w.high_c
            .map_or_else(|| "–".to_owned(), |h| format!("{h} °C")),
        match (&w.high_first_at, &w.high_last_at) {
            (Some(a), Some(b)) if a != b => format!(" (first {a}, last {b})"),
            (Some(a), _) => format!(" ({a})"),
            _ => String::new(),
        }
    );
    match &d.forecast {
        Some(f) => {
            let _ = writeln!(
                s,
                "- **Forecast:** {} day maximum {:.1} °C{} (received {} UTC).",
                f.product,
                f.day_max_c,
                f.error_c
                    .map_or_else(String::new, |e| format!(", error {e:+.1} °C")),
                f.received_at.format("%m-%d %H:%M")
            );
        }
        None => {
            let _ = writeln!(s, "- **Forecast:** none covering the whole day.");
        }
    }
    let l = &d.latency;
    let sources: Vec<String> = l
        .by_source
        .iter()
        .map(|x| {
            format!(
                "{} {} (median {}{})",
                x.source,
                x.first,
                secs(x.median_s),
                if x.failover > 0 {
                    format!(", {} as failover", x.failover)
                } else {
                    String::new()
                }
            )
        })
        .collect();
    let _ = writeln!(
        s,
        "- **Report delays** (observation → first fetch): {} on time, median {}, 90th percentile {}, slowest {}, {} over 5 min{}. First delivered by: {}.",
        l.timely,
        secs(l.median_s),
        secs(l.p90_s),
        secs(l.max_s),
        l.over_5min,
        if l.catch_up > 0 {
            format!("; {} caught up after a restart or outage", l.catch_up)
        } else {
            String::new()
        },
        if sources.is_empty() {
            "–".to_owned()
        } else {
            sources.join(", ")
        }
    );
    if !l.late.is_empty() {
        let late: Vec<String> = l
            .late
            .iter()
            .take(LIST_CAP)
            .map(|x| format!("{} +{} s ({})", x.observed_at, x.delay_s, x.source))
            .collect();
        let _ = writeln!(s, "  - Late: {}", late.join(", "));
    }
    match &d.market {
        Some(m) => {
            let _ = writeln!(
                s,
                "- **Market:** `{}` ({} buckets), winner {}.",
                m.event_slug,
                m.buckets,
                m.winner.as_deref().unwrap_or("unknown")
            );
        }
        None => {
            let _ = writeln!(s, "- **Market:** none discovered for this day.");
        }
    }
    if let Some(m) = &d.model_vs_market {
        let _ = writeln!(
            s,
            "- **Model against market on {}** ({} evaluations with a book): mean probability model {:.2}, market {:.2}; log loss model {:.3}, market {:.3}; model higher {}×, market higher {}×. Sure (≥ 0.90) first: model {}, market {}.",
            m.bucket,
            m.samples,
            m.model_mean,
            m.market_mean,
            m.model_log_loss,
            m.market_log_loss,
            m.model_higher,
            m.market_higher,
            opt(m.model_sure_at.as_deref()),
            opt(m.market_sure_at.as_deref())
        );
        if let Some(w) = &m.widest {
            let _ = writeln!(
                s,
                "  - Widest gap: {} (high {} °C) model {:.2}, market {:.2}.",
                w.at, w.high_c, w.model, w.market
            );
        }
        let trail: Vec<String> = m
            .trail
            .iter()
            .map(|p| {
                format!(
                    "{} high {}: {:.2} / {:.2}",
                    p.at, p.high_c, p.model, p.market
                )
            })
            .collect();
        let _ = writeln!(
            s,
            "  - Model / market through the day: {}",
            trail.join(" · ")
        );
    }
    let _ = writeln!(s, "- **Evaluations:** {}", d.evaluations);
    for st in &d.strategies {
        let _ = writeln!(
            s,
            "  - **{}**: {} bucket evaluations, {} signal(s).",
            st.strategy,
            st.lines,
            st.signals.len()
        );
        if !st.blockers.is_empty() {
            let b: Vec<String> = st
                .blockers
                .iter()
                .map(|b| {
                    format!(
                        "`{}` ×{} (e.g. \"{}\")",
                        cell(&b.pattern),
                        b.count,
                        cell(&b.example)
                    )
                })
                .collect();
            let _ = writeln!(s, "    - Blocked by: {}", b.join("; "));
        }
        for c in &st.signals {
            let _ = writeln!(s, "    - Signal: {}", cell(&call_text(c)));
        }
        for c in &st.closest {
            let _ = writeln!(s, "    - Closest: {}", cell(&call_text(c)));
        }
    }
    if d.proposals.is_empty() {
        let _ = writeln!(s, "- **Proposals:** none.");
    } else {
        let _ = writeln!(s, "- **Proposals:** {}", d.proposals.len());
        for p in d.proposals.iter().take(LIST_CAP) {
            let _ = writeln!(
                s,
                "  - {} {}{}",
                p.at,
                cell(&p.summary),
                if p.reasons.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", cell(&p.reasons.join("; ")))
                }
            );
        }
        if d.proposals.len() > LIST_CAP {
            let _ = writeln!(s, "  - … {} more", d.proposals.len() - LIST_CAP);
        }
    }
    if d.orders.is_empty() {
        let _ = writeln!(s, "- **Paper orders:** none.");
    } else {
        let _ = writeln!(s, "- **Paper orders:**\n");
        let _ = writeln!(
            s,
            "| Time | Strategy | Order | Limit | Shares | Status | Filled @ avg | Fees |"
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
        for o in &d.orders {
            let market = match o.market_date {
                Some(md) if md != d.date => format!(" ({md})"),
                _ => String::new(),
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} {} {}{market} | {:.2} | {:.2} | {}{} | {:.2} @ {} | {} |",
                o.at,
                o.strategy,
                o.side,
                o.outcome_side,
                cell(&o.bucket),
                o.limit,
                o.shares,
                o.status,
                o.reason
                    .as_deref()
                    .map_or_else(String::new, |r| format!(" ({})", cell(r))),
                o.filled,
                p2(o.avg_price),
                usd(o.fees_usd)
            );
        }
        let _ = writeln!(s);
    }
    if let Some(p) = d.pnl_usd {
        let _ = writeln!(
            s,
            "- **Settled paper P&L of this day's market:** {}",
            with_split(usd(p), &d.strategy_pnl)
        );
    }
    if !d.providers.is_empty() {
        let p: Vec<String> = d
            .providers
            .iter()
            .map(|p| {
                format!(
                    "{} {} requests ({} failed, {} throttled; p50 {:.0} ms, p90 {:.0} ms, max {} ms)",
                    p.provider, p.requests, p.failures, p.throttled, p.p50_ms, p.p90_ms, p.max_ms
                )
            })
            .collect();
        let _ = writeln!(s, "- **Providers:** {}", p.join("; "));
    }
    if !d.health_changes.is_empty() {
        let _ = writeln!(
            s,
            "- **Health changes:** {}{}",
            d.health_changes
                .iter()
                .take(LIST_CAP)
                .map(|h| cell(h))
                .collect::<Vec<_>>()
                .join("; "),
            if d.health_changes.len() > LIST_CAP {
                format!("; … {} more", d.health_changes.len() - LIST_CAP)
            } else {
                String::new()
            }
        );
    }
    if !d.events.is_empty() {
        let e: Vec<String> = d
            .events
            .iter()
            .map(|e| {
                format!(
                    "{} {} ×{} (last: {})",
                    e.level,
                    e.kind,
                    e.count,
                    cell(&e.example)
                )
            })
            .collect();
        let _ = writeln!(s, "- **Logged events:** {}", e.join("; "));
    }
    let _ = writeln!(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluation_lines_parse() {
        let l = parse_line(
            "A 25°C YES · ask 0.94 · p 0.895 (market 0.925) · EV -0.0529 — confirmation 0m < 60m; ask 0.94 outside [0.90, 0.93]",
        )
        .unwrap();
        assert_eq!(l.tag, "A");
        assert_eq!(l.bucket, "25°C");
        assert_eq!(l.side, "YES");
        assert_eq!(l.ask, Some(0.94));
        assert_eq!(l.p, Some(0.895));
        assert_eq!(l.model, None);
        assert_eq!(l.market, Some(0.925));
        assert_eq!(l.ev, Some(-0.0529));
        assert!(!l.signal);
        assert_eq!(
            l.verdict,
            "confirmation 0m < 60m; ask 0.94 outside [0.90, 0.93]"
        );

        let l = parse_line(
            "B 26°C or higher NO · ask 0.60 · p 0.970 (model 0.990, market 0.400) · EV +0.3400 — SIGNAL",
        )
        .unwrap();
        assert_eq!(l.bucket, "26°C or higher");
        assert_eq!(l.side, "NO");
        assert_eq!(l.model, Some(0.99));
        assert_eq!(l.market, Some(0.4));
        assert_eq!(l.ev, Some(0.34));
        assert!(l.signal);

        let l = parse_line("E 25°C YES · no ask — no ask; strategy disabled").unwrap();
        assert_eq!(l.ask, None);
        assert_eq!(l.p, None);
        assert_eq!(l.ev, None);
        assert!(parse_line("garbage without a verdict").is_none());
    }

    /// 30 Sep: F's closest call is the 15:27 near-miss (ask 0.94, too few
    /// shares), not the line with the best model EV; A keeps the EV order.
    #[test]
    fn price_rules_rank_closest_calls_by_blockers_then_ask() {
        let at = |h: u32, m: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
                .and_utc()
        };
        let rec = |t: DateTime<Utc>, lines: &[&str]| ReportDecision {
            at: t,
            strategy: "evaluation".into(),
            event_slug: None,
            summary: String::new(),
            inputs: serde_json::Value::Null,
            outputs: serde_json::json!({ "evaluations": lines }),
            approved: false,
            reasons: Vec::new(),
        };
        let recs = [
            rec(
                at(9, 28),
                &[
                    "F 21°C YES · ask 0.009 · p 0.090 · EV +0.0755 — 11:28 outside the autumn slot 13:25–16:01; ask 0.009 not above 0.90",
                    "A 21°C YES · ask 0.009 · p 0.090 · EV +0.0755 — confirmation 0m < 60m; ask 0.009 outside [0.90, 0.99]",
                ],
            ),
            rec(
                at(12, 57),
                &[
                    "F 23°C YES · ask 0.74 · p 0.437 · EV -0.3175 — ask 0.74 not above 0.90",
                    "A 23°C YES · ask 0.74 · p 0.437 · EV -0.3175 — confirmation 0m < 60m; ask 0.74 outside [0.90, 0.99]",
                ],
            ),
            rec(
                at(13, 27),
                &[
                    "F 23°C YES · ask 0.94 · p 0.745 (market 0.910) · EV -0.2029 — only 24.14 shares offered ≤ 0.95 (need 100)",
                ],
            ),
            rec(
                at(13, 57),
                &[
                    "F 23°C YES · ask 0.988 · p 0.745 (market 0.966) · EV -0.2487 — ask 0.988 above 0.95; already positioned",
                ],
            ),
        ];
        let refs: Vec<&ReportDecision> = recs.iter().collect();
        let days = strategy_days(
            &refs,
            &HashMap::new(),
            Some(23),
            0.05,
            chrono_tz::Europe::Amsterdam,
        );
        let tag = |t: &str| days.iter().find(|d| d.strategy == t).unwrap();
        let f: Vec<(&str, &str)> = tag("F")
            .closest
            .iter()
            .map(|c| (c.at.as_str(), c.bucket.as_str()))
            .collect();
        // One line per bucket: 23 °C's 15:27 near-miss, then 21 °C.
        assert_eq!(f, vec![("15:27", "23°C"), ("11:28", "21°C")]);
        let a: Vec<&str> = tag("A").closest.iter().map(|c| c.at.as_str()).collect();
        assert_eq!(a, vec!["11:28", "14:57"], "A: by model EV");
    }

    #[test]
    fn blockers_group_by_pattern() {
        assert_eq!(
            blocker_pattern("confirmation 12m < 60m"),
            "confirmation #m < #m"
        );
        assert_eq!(
            blocker_pattern("only -0 shares offered ≤ 0.99 15m ago"),
            "only # shares offered ≤ # #m ago"
        );
        assert_eq!(blocker_pattern("EV -0.0529 < 0.01"), "EV # < #");
        assert_eq!(blocker_pattern("no ask"), "no ask");
        assert_eq!(blocker_pattern("B-2 left"), "B-# left");
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let v = [150, 170, 180, 190, 660];
        assert_eq!(median(&v), Some(180));
        assert_eq!(percentile(&v, 0.9), Some(660));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn book_midpoint_needs_a_recent_book() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let tops = vec![
            ReportBookTop {
                captured_at: t("2026-09-29T14:00:00Z"),
                bid_micros: Some(700_000),
                ask_micros: Some(760_000),
            },
            ReportBookTop {
                captured_at: t("2026-09-29T14:10:00Z"),
                bid_micros: Some(990_000),
                ask_micros: None,
            },
        ];
        assert_eq!(market_p(&tops, t("2026-09-29T13:59:00Z")), None);
        assert!((market_p(&tops, t("2026-09-29T14:05:00Z")).unwrap() - 0.73).abs() < 1e-9);
        assert_eq!(market_p(&tops, t("2026-09-29T14:10:00Z")), Some(0.99));
        assert_eq!(market_p(&tops, t("2026-09-29T14:40:00Z")), None);
    }
}
