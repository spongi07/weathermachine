//! Live paper-trading runtime.
//!
//! ```text
//!  StationCollector ×N ─┐                      ┌─> hints ──> collectors
//!  Market discovery ────┤  mpsc  ┌──────────┐  ├─> persistence task ──> PostgreSQL
//!  Market WS stream  ───┼──────> │  engine  │──┤   (journal, decisions, orders, fills)
//!  REST book fallback ──┤        │   loop   │  └─> dashboard snapshot (watch → HTTP/SSE)
//!  heartbeat / operator ┘        └──────────┘
//! ```
//!
//! The engine loop owns the deterministic kernel (inside a
//! [`SimulationSession`] with the simulated venue) and never awaits I/O:
//! persistence is handed to a separate task through a bounded queue, and a
//! full queue or a failed write turns `storage_ok` off, which blocks new
//! positions (fail closed). Live order placement does not exist in this build.

use crate::config::AppConfig;
use crate::dto::{self, DtoInputs};
use crate::http::Publisher;
use crate::setup::{self, ModelLoad, Providers};
use crate::training::{self, Progress, TrainPlan};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use wm_backtest::{SessionOutput, SimulationSession};
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OperatorCommand,
    OrderBookEvent, TimerEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::forecast::ForecastProduct;
use wm_core::health::{CircuitState, ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{EventSlug, RunId, StationId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, Side};
use wm_core::resolution::{ObservationFilter, SpecReviewStatus};
use wm_core::time::{Clock, SystemClock, local_date};
use wm_core::trading::{DecisionRecord, Fill, RunMode};
use wm_core::weather::DedupClass;
use wm_dashboard_api::{AlertDto, ModelDto};
use wm_execution::{OrderRecord, SimConfig};
use wm_net::ProviderGate;
use wm_polymarket::{
    ClobClient, GammaClient, LocationMarketSpec, MarketStream, MarketStreamConfig, StreamStatus,
    build_market, event_slug,
};
use wm_storage::PgStore;
use wm_storage::wm_execution_record::OrderRow;
use wm_strategy::{EmpiricalPeakModel, NoEdgeModel, ProbabilityModel};
use wm_weather::forecast::{ForecastProvider, ForecastQuery};
use wm_weather::{
    CollectorConfig, CollectorRegistry, CollectorStatus, IemArchive, OpenMeteoPreviousRuns,
    PollingHints, StationCollector,
};

type SharedModel = Arc<Mutex<ModelDto>>;

fn update_model(status: &SharedModel, f: impl FnOnce(&mut ModelDto)) {
    f(&mut status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner));
}

fn set_model(status: &SharedModel, state: &str, detail: String, progress: Option<(u32, u32)>) {
    update_model(status, |g| {
        g.state = state.to_owned();
        g.detail = detail;
        g.progress = progress;
    });
}

fn set_loaded(status: &SharedModel, m: &EmpiricalPeakModel) {
    update_model(status, |g| {
        g.state = "loaded".into();
        g.detail = format!(
            "{} · {} samples · {} → {}",
            m.id,
            m.total_samples(),
            m.trained_from,
            m.trained_to
        );
        g.progress = None;
        g.forecast = m.forecast.as_ref().map(|f| f.verdict.clone());
        g.structure = m.selection.as_ref().map(|s| s.verdict.clone());
        g.retraining = None;
    });
}

/// Handles the runtime shares with the HTTP server.
pub struct RuntimeContext {
    pub publisher: Publisher,
    pub commands: mpsc::Receiver<OperatorCommand>,
    pub ready: Arc<AtomicBool>,
    pub shutdown: watch::Receiver<bool>,
}

/// One persistence unit (written in FK order: journal → decisions → orders → fills → books).
#[derive(Debug, Default)]
struct PersistBatch {
    events: Vec<EventEnvelope>,
    decisions: Vec<DecisionRecord>,
    orders: Vec<OrderRow>,
    fills: Vec<Fill>,
    books: Vec<OrderBook>,
}

/// Events coalesced into one write: bursts collapse into a single transaction.
const MAX_COALESCED_EVENTS: usize = 20_000;
/// Events held back while storage is slow or down. Beyond this, order-book
/// journal entries (the bulk) are shed first; decisions, orders, fills and
/// every other event are never dropped.
const MAX_PENDING_EVENTS: usize = 200_000;

impl PersistBatch {
    fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.decisions.is_empty()
            && self.orders.is_empty()
            && self.fills.is_empty()
            && self.books.is_empty()
    }

    /// Append a later batch (order preserved: later order states win).
    fn merge(&mut self, later: PersistBatch) {
        self.events.extend(later.events);
        self.decisions.extend(later.decisions);
        self.orders.extend(later.orders);
        self.fills.extend(later.fills);
        self.books.extend(later.books);
    }

    /// Last resort when storage is stalled: drop order-book entries once the
    /// batch exceeds `cap` events. Returns how many were dropped.
    fn shed_books(&mut self, cap: usize) -> usize {
        if self.events.len() <= cap {
            return 0;
        }
        let before = self.events.len() + self.books.len();
        self.events
            .retain(|e| !matches!(e.event, WeatherMachineEvent::OrderBookUpdate(_)));
        self.books.clear();
        before - self.events.len()
    }
}

/// Flatten an order record for storage.
pub fn order_row(o: &OrderRecord) -> OrderRow {
    OrderRow {
        client_order_id: o.client_order_id.to_string(),
        decision_id: i64::try_from(o.decision_id.0).unwrap_or(i64::MAX),
        strategy: o.strategy.to_string(),
        location: o.location.to_string(),
        event_slug: o.event_slug.to_string(),
        token: o.token.to_string(),
        condition_id: o.condition_id.to_string(),
        outcome_side: o.outcome_side.as_str().to_owned(),
        side: match o.side {
            Side::Buy => "BUY".to_owned(),
            Side::Sell => "SELL".to_owned(),
        },
        kind: format!("{:?}", o.kind).to_lowercase(),
        limit_price_micros: i32::try_from(o.limit_price.micros()).unwrap_or(i32::MAX),
        shares_micros: o.shares.micros(),
        tif: serde_json::to_value(o.tif).unwrap_or(serde_json::Value::Null),
        status: o.status.as_str().to_owned(),
        filled_micros: o.filled.micros(),
        avg_price_micros: o.avg_price.and_then(|p| i32::try_from(p.micros()).ok()),
        fees_micros: o.fees.micros(),
        venue_order_id: o.venue_order_id.clone(),
        reason: o.reason.clone(),
        created_at: o.created_at,
        updated_at: o.updated_at,
    }
}

async fn persistence_loop(
    store: PgStore,
    run: RunId,
    mut rx: mpsc::Receiver<PersistBatch>,
    ok: Arc<AtomicBool>,
    journal: bool,
    side: SharedSide,
) {
    let mut pending = PersistBatch::default();
    let mut backoff = std::time::Duration::from_secs(1);
    let mut failures = 0u32;
    loop {
        if pending.is_empty() {
            match rx.recv().await {
                Some(b) => pending = b,
                None => return,
            }
        }
        // Coalesce everything already queued into this write.
        let mut closed = false;
        while pending.events.len() < MAX_COALESCED_EVENTS {
            match rx.try_recv() {
                Ok(b) => pending.merge(b),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        let batch = wm_storage::EngineBatch {
            events: if journal { &pending.events } else { &[] },
            decisions: &pending.decisions,
            orders: &pending.orders,
            fills: &pending.fills,
            books: &pending.books,
        };
        match store.persist_engine_batch(&run, batch).await {
            Ok(()) => {
                pending = PersistBatch::default();
                ok.store(true, Ordering::Release);
                if failures > 0 {
                    with_side(&side, |st| {
                        st.alert("info", format!("audit storage recovered after {failures} failed write(s); nothing was lost"))
                    });
                }
                failures = 0;
                backoff = std::time::Duration::from_secs(1);
                if closed {
                    return;
                }
            }
            Err(e) => {
                ok.store(false, Ordering::Release);
                failures += 1;
                metrics::counter!("wm_persist_failures_total", "component" => "engine")
                    .increment(1);
                tracing::error!(error = %e, pending_events = pending.events.len(), retry_in_s = backoff.as_secs(), "engine persistence failed — new positions blocked; retrying the same batch");
                if failures == 1 {
                    with_side(&side, |st| {
                        st.alert(
                            "critical",
                            format!(
                                "audit storage write failed ({e}); new positions blocked, retrying"
                            ),
                        )
                    });
                }
                let shed = pending.shed_books(MAX_PENDING_EVENTS);
                if shed > 0 {
                    metrics::counter!("wm_journal_shed_total").increment(shed as u64);
                    with_side(&side, |st| {
                        st.alert("critical", format!("journal gap: {shed} order-book updates not persisted while storage was down"))
                    });
                }
                if closed && failures >= 3 {
                    tracing::error!(
                        events = pending.events.len(),
                        decisions = pending.decisions.len(),
                        orders = pending.orders.len(),
                        "shutting down with unpersisted engine records"
                    );
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

/// Provider health for REST/WS market-data gates (not station-scoped).
fn gate_snapshot(gate: &ProviderGate, now: DateTime<Utc>) -> ProviderHealthSnapshot {
    let st = gate.stats();
    let mut s = ProviderHealthSnapshot::new(gate.provider().clone(), None, now);
    s.state = match st.circuit {
        CircuitState::Open => ProviderHealthState::Unavailable,
        CircuitState::HalfOpen => ProviderHealthState::Degraded,
        // Never used yet, e.g. the REST book fallback while the stream is live.
        CircuitState::Closed if st.requests_total == 0 => ProviderHealthState::Standby,
        CircuitState::Closed if st.politeness_multiplier > 1 => ProviderHealthState::Throttled,
        CircuitState::Closed if st.consecutive_failures > 0 => ProviderHealthState::Degraded,
        CircuitState::Closed => ProviderHealthState::Healthy,
    };
    s.reason = if st.requests_total == 0 {
        "standby: no requests yet".into()
    } else {
        format!(
            "{} ok / {} failed / {} throttled",
            st.successes_total, st.failures_total, st.throttled_total
        )
    };
    s.consecutive_failures = st.consecutive_failures;
    s.current_backoff_ms = u64::try_from(st.current_backoff.as_millis()).unwrap_or(u64::MAX);
    s.blocked_until = gate.blocked_until_utc();
    s.throttle_events = st.throttled_total;
    s.requests_total = st.requests_total;
    s.requests_today = st.requests_today;
    s.daily_budget = gate.policy().daily_budget;
    s.circuit = st.circuit;
    s
}

/// How long after a market's local day it is settled (paper/live).
fn settle_grace() -> Duration {
    Duration::hours(2)
}

/// Load earlier runs' paper book into the session and say what came back.
/// A failure is loud but not fatal: the run then starts flat, as before.
async fn restore_paper_book(
    store: &PgStore,
    cfg: &AppConfig,
    session: &mut SimulationSession,
    now: DateTime<Utc>,
    side: &SharedSide,
) {
    match crate::restore::load(store, cfg, now, settle_grace()).await {
        Ok((state, warnings)) => {
            for w in &warnings {
                tracing::warn!(warning = %w, "paper book restore");
                with_side(side, |st| st.alert("warning", w.clone()));
            }
            if state.is_empty() {
                return;
            }
            let summary = session.restore(&state, now);
            let text = crate::restore::describe(&summary);
            tracing::info!(
                positions = summary.open_positions,
                fills = summary.fills,
                markets = summary.markets,
                "{text}"
            );
            let level = if summary.rejected.is_empty() {
                "info"
            } else {
                "warning"
            };
            with_side(side, |st| st.alert(level, text.clone()));
            let details = serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
            let store = store.clone();
            tokio::spawn(async move {
                let _ = store
                    .record_system_event(level, "restore", &text, &details)
                    .await;
            });
        }
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "paper book restore failed");
            with_side(side, |st| {
                st.alert(
                    "critical",
                    format!("could not restore the paper book of earlier runs: {e:#}; this run starts flat"),
                )
            });
        }
    }
}

pub(crate) fn review_status_of(s: &str) -> SpecReviewStatus {
    match s {
        "approved" => SpecReviewStatus::Approved,
        "rejected" => SpecReviewStatus::Rejected,
        _ => SpecReviewStatus::AutoParsed,
    }
}

/// Shared state written by background tasks and read when publishing.
#[derive(Default)]
struct SideState {
    rules_review: HashMap<String, String>,
    alerts: VecDeque<AlertDto>,
}

impl SideState {
    fn alert(&mut self, level: &str, message: impl Into<String>) {
        let message = message.into();
        let now = Utc::now().timestamp_millis();
        // One line per incident: the same alert within a minute is not repeated.
        if self
            .alerts
            .iter()
            .rev()
            .take(10)
            .any(|a| a.level == level && a.message == message && now - a.at_ms < 60_000)
        {
            return;
        }
        self.alerts.push_back(AlertDto {
            at_ms: now,
            level: level.into(),
            message,
        });
        while self.alerts.len() > 80 {
            self.alerts.pop_front();
        }
    }
}

type SharedSide = Arc<Mutex<SideState>>;

fn with_side<R>(side: &SharedSide, f: impl FnOnce(&mut SideState) -> R) -> R {
    let mut g = side
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut g)
}

/// Discover today's/tomorrow's markets for every location on a fixed cadence.
#[allow(clippy::too_many_arguments)]
async fn discovery_loop(
    gamma: GammaClient,
    specs: Vec<LocationMarketSpec>,
    store: Option<PgStore>,
    events: mpsc::Sender<EventEnvelope>,
    assets: watch::Sender<Vec<TokenId>>,
    today_tokens: watch::Sender<Vec<TokenId>>,
    side: SharedSide,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut known: HashMap<EventSlug, DailyTemperatureMarket> = HashMap::new();
    loop {
        let now = clock.now();
        for spec in &specs {
            let today = local_date(now, spec.timezone);
            for date in [today, today.succ_opt().unwrap_or(today)] {
                let slug = event_slug(&spec.slug_template, date);
                match gamma
                    .events_by_slug(&slug, std::time::Duration::from_secs(30))
                    .await
                {
                    Ok((list, raw)) => {
                        let Some(ev) = list.iter().find(|e| e.slug == slug) else {
                            tracing::debug!(%slug, "no event published yet");
                            continue;
                        };
                        match build_market(ev, spec, date, clock.now()) {
                            Ok(mut m) => {
                                if let Some(s) = &store {
                                    if let Err(e) = s.upsert_market(&m, Some(&raw)).await {
                                        tracing::error!(%slug, error = %e, "failed to store market");
                                    }
                                    if let Ok(Some(status)) =
                                        s.rules_review_status(&m.rules.sha256).await
                                    {
                                        m.resolution.review = review_status_of(&status);
                                        with_side(&side, |st| {
                                            st.rules_review.insert(m.rules.sha256.clone(), status)
                                        });
                                    }
                                }
                                let changed = known.get(&m.event_slug).is_none_or(|k| {
                                    k.rules.sha256 != m.rules.sha256
                                        || k.outcomes != m.outcomes
                                        || k.closed != m.closed
                                        || k.resolution.review != m.resolution.review
                                });
                                if changed {
                                    tracing::info!(%slug, buckets = m.outcomes.len(), tradable = m.resolution.is_machine_tradable(), "market discovered/updated");
                                    let env = EventEnvelope::new(
                                        clock.now(),
                                        EventSource::Live,
                                        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent {
                                            market: m.clone(),
                                        }),
                                    );
                                    if events.send(env).await.is_err() {
                                        return;
                                    }
                                }
                                known.insert(m.event_slug.clone(), m);
                            }
                            Err(e) => {
                                tracing::warn!(%slug, error = %e, "market could not be mapped — it will not be traded");
                                with_side(&side, |st| {
                                    st.alert("warning", format!("market {slug} not mapped: {e}"))
                                });
                            }
                        }
                    }
                    Err(e) => tracing::warn!(%slug, error = %e, "market discovery request failed"),
                }
            }
        }
        // Forget markets older than yesterday; subscribe to today's and tomorrow's tokens.
        let now = clock.now();
        known.retain(|_, m| {
            m.local_date
                >= local_date(now, m.timezone)
                    .pred_opt()
                    .unwrap_or(m.local_date)
        });
        let mut all: BTreeSet<TokenId> = BTreeSet::new();
        let mut today: BTreeSet<TokenId> = BTreeSet::new();
        for m in known.values().filter(|m| !m.closed) {
            let is_today = m.local_date == local_date(now, m.timezone);
            for o in &m.outcomes {
                all.insert(o.yes_token.clone());
                all.insert(o.no_token.clone());
                if is_today {
                    today.insert(o.yes_token.clone());
                    today.insert(o.no_token.clone());
                }
            }
        }
        assets.send_if_modified(|v| {
            let next: Vec<TokenId> = all.iter().cloned().collect();
            if *v == next {
                false
            } else {
                *v = next;
                true
            }
        });
        today_tokens.send_replace(today.into_iter().collect());
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {}
            r = shutdown.changed() => { if r.is_err() || *shutdown.borrow() { return; } }
        }
    }
}

/// REST order-book fallback: polls today's books only while the stream is down.
async fn book_fallback_loop(
    clob: ClobClient,
    today_tokens: watch::Receiver<Vec<TokenId>>,
    stream: Option<watch::Receiver<StreamStatus>>,
    events: mpsc::Sender<EventEnvelope>,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let connected = stream.as_ref().is_some_and(|s| s.borrow().connected);
        if !connected {
            let tokens = today_tokens.borrow().clone();
            for t in tokens {
                if *shutdown.borrow() {
                    return;
                }
                match clob.book(&t, std::time::Duration::from_secs(5)).await {
                    Ok(book) => {
                        let env = EventEnvelope::new(
                            clock.now(),
                            EventSource::Live,
                            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
                                book: book.truncated(wm_core::market::ENGINE_BOOK_DEPTH),
                            }),
                        );
                        if events.send(env).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(token = %t, error = %e, "REST book fetch failed");
                        break;
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            r = shutdown.changed() => { if r.is_err() || *shutdown.borrow() { return; } }
        }
    }
}

async fn connect_store(url: &str, max_connections: u32) -> Result<PgStore> {
    let mut wait = std::time::Duration::from_secs(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        match PgStore::connect(url, max_connections).await {
            Ok(s) => return Ok(s),
            Err(e) if std::time::Instant::now() < deadline => {
                tracing::warn!(error = %e, retry_in_s = wait.as_secs(), "database not reachable yet");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(15));
            }
            Err(e) => return Err(e).context("connecting to PostgreSQL"),
        }
    }
}

/// Run the paper-trading service until shutdown.
pub async fn run(cfg: AppConfig, ctx: RuntimeContext) -> Result<()> {
    let RuntimeContext {
        mut publisher,
        mut commands,
        ready,
        shutdown: external_shutdown,
    } = ctx;
    // Every task stops on this: the external shutdown signal, or the runtime's
    // own decision to stop (restart to load a new model).
    let (stop_tx, shutdown) = watch::channel(false);
    {
        let mut ext = external_shutdown;
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            loop {
                if *ext.borrow() {
                    let _ = stop_tx.send(true);
                    return;
                }
                if ext.changed().await.is_err() {
                    let _ = stop_tx.send(true);
                    return;
                }
            }
        });
    }
    match cfg.file.app.mode {
        RunMode::Paper => {}
        RunMode::Live => bail!(
            "live trading is not available in this build (Phase 14 gate); use mode = \"paper\""
        ),
        RunMode::Backtest => {
            bail!("mode \"backtest\" is a batch job: use `weather-machine backtest`")
        }
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let user_agent = cfg
        .user_agent()
        .context("NOAA/NWS require an identifying User-Agent: set WM_CONTACT (or WM_USER_AGENT)")?;

    // -- Storage -------------------------------------------------------------------
    let store = match &cfg.env.database_url {
        Some(url) => {
            let s = connect_store(url, cfg.file.database.max_connections).await?;
            if cfg.file.app.auto_migrate {
                s.migrate().await.context("running migrations")?;
                tracing::info!("database migrations applied");
            }
            Some(s)
        }
        None => {
            tracing::warn!(
                "WM_DATABASE_URL is not set: no audit storage, so new positions are blocked (fail closed)"
            );
            None
        }
    };
    let run_id = RunId::new_v7();
    let engine_cfg = setup::engine_config(&cfg, RunMode::Paper, run_id)?;
    let model_status: SharedModel = Arc::new(Mutex::new(ModelDto::default()));
    let mut invalid_model: Option<String> = None;
    // Model file the maintenance task keeps current, and the model it starts from.
    let mut maintain: Option<(std::path::PathBuf, Option<Arc<EmpiricalPeakModel>>)> = None;
    let model: Arc<dyn ProbabilityModel> = match setup::find_model(&cfg) {
        ModelLoad::Loaded(m) => {
            set_loaded(&model_status, &m);
            if cfg.file.model.auto_train.enabled
                && let Some(path) = setup::model_path(&cfg)
            {
                maintain = Some((path, Some(Arc::clone(&m))));
            }
            m
        }
        ModelLoad::Missing(path) if cfg.file.model.auto_train.enabled => {
            set_model(
                &model_status,
                "training",
                "starting: training from IEM METAR history".into(),
                None,
            );
            maintain = Some((path, None));
            Arc::new(NoEdgeModel)
        }
        ModelLoad::Missing(path) => {
            set_model(
                &model_status,
                "missing",
                format!(
                    "no model at {} and auto-training is off — no weather trades",
                    path.display()
                ),
                None,
            );
            Arc::new(NoEdgeModel)
        }
        ModelLoad::NotConfigured => {
            set_model(
                &model_status,
                "disabled",
                "no model configured and auto-training is off — no weather trades".into(),
                None,
            );
            Arc::new(NoEdgeModel)
        }
        ModelLoad::Invalid(path, e) => {
            tracing::error!(path = %path.display(), error = %e, "probability model unusable — running without one (no weather trades)");
            let msg = format!("model {} unusable: {e} — fix or delete it", path.display());
            set_model(&model_status, "invalid", msg.clone(), None);
            invalid_model = Some(msg);
            Arc::new(NoEdgeModel)
        }
    };
    if let Some(s) = &store {
        let cfg_json = serde_json::json!({ "app": cfg.file.app, "risk": cfg.file.risk, "strategies": cfg.file.strategies, "polling": cfg.file.polling, "locations": cfg.locations });
        s.record_run(&run_id, RunMode::Paper, model.id(), &cfg_json, clock.now())
            .await
            .context("recording run")?;
        let _ = s
            .record_system_event(
                "info",
                "startup",
                "weather machine started",
                &serde_json::json!({ "run_id": run_id.to_string(), "version": wm_core::VERSION }),
            )
            .await;
    }
    tracing::info!(run = %run_id, model = %model.id(), locations = cfg.locations.len(), "starting paper runtime");

    let providers = Providers::build(&cfg, Arc::clone(&clock), &user_agent)?;
    let side: SharedSide = Arc::new(Mutex::new(SideState::default()));
    let (events_tx, mut events_rx) = mpsc::channel::<EventEnvelope>(16_384);
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    if let Some(msg) = invalid_model {
        with_side(&side, |st| st.alert("critical", msg));
    }

    // -- Probability model: first training, forecast evaluation, periodic retraining ----
    // New models are swapped into the running engine; trading never stops.
    let (models_tx, mut models_rx) = mpsc::channel::<Arc<EmpiricalPeakModel>>(2);
    let mut models_open = true;
    if let Some((path, current)) = maintain {
        match providers.fetcher("iem") {
            Some(f) => {
                let archive = IemArchive::new(Arc::clone(f), &cfg.file.providers.iem.base_url);
                let plan = TrainPlan::from_config(&cfg, path, clock.now().date_naive())?;
                let at = &cfg.file.model.auto_train;
                let policy = MaintenancePolicy {
                    retry_after: std::time::Duration::from_secs(at.retry_after_secs.max(60)),
                    retrain_after: (at.retrain_after_days > 0).then(|| {
                        Duration::days(i64::try_from(at.retrain_after_days).unwrap_or(i64::MAX / 2))
                    }),
                    product: plan.forecast.as_ref().map(|f| f.product.clone()),
                    retrain_existing: cfg.file.model.path.is_none(),
                    compare_structures: plan.selection.is_some(),
                };
                let audit: Option<Arc<dyn wm_core::ingest::IngestSink>> = store
                    .as_ref()
                    .map(|s| Arc::new(s.clone()) as Arc<dyn wm_core::ingest::IngestSink>);
                tasks.push(tokio::spawn(model_maintenance_loop(
                    archive,
                    setup::forecast_client(&cfg, &providers),
                    plan,
                    policy,
                    current,
                    audit,
                    Arc::clone(&model_status),
                    Arc::clone(&side),
                    models_tx,
                    Arc::clone(&clock),
                    shutdown.clone(),
                )));
            }
            None if current.is_none() => set_model(
                &model_status,
                "missing",
                "no model and the IEM provider is disabled — no weather trades".into(),
                None,
            ),
            None => {}
        }
    } else {
        drop(models_tx);
    }

    // -- Station collectors (one per station, cross-process lease) -------------------
    let registry = CollectorRegistry::new();
    let mut hint_txs: HashMap<StationId, watch::Sender<PollingHints>> = HashMap::new();
    let mut collector_status: HashMap<StationId, watch::Receiver<CollectorStatus>> = HashMap::new();
    let mut leases = Vec::new();
    let mut warm_events: Vec<EventEnvelope> = Vec::new();
    let mut confirmed_filters: HashMap<StationId, ObservationFilter> = HashMap::new();
    for l in &cfg.locations {
        let ids = setup::location_ids(l)?;
        if let Some(f) = l.market.confirmed_filter {
            confirmed_filters.insert(ids.station.clone(), f.filter());
        }
        if let Some(s) = &store {
            match s.try_station_lease(&ids.station).await? {
                Some(lease) => leases.push(lease),
                None => {
                    tracing::error!(station = %ids.station, "another instance holds this station's collector lease; not polling it here");
                    with_side(&side, |st| {
                        st.alert("critical", format!("{}: collector lease held by another instance — no data, no trading", ids.station))
                    });
                    continue;
                }
            }
        }
        let claim = registry
            .claim(&ids.station)
            .map_err(|e| anyhow::anyhow!("station {} configured twice: {e:?}", ids.station))?;
        let sources = providers.observation_sources(&cfg, l)?;
        let sink: Arc<dyn wm_core::ingest::IngestSink> = match &store {
            Some(s) => Arc::new(s.clone()),
            None => Arc::new(wm_core::ingest::NullIngestSink),
        };
        let (hint_tx, hint_rx) = watch::channel(PollingHints::default());
        let collector_cfg = CollectorConfig {
            station: ids.station.clone(),
            location: ids.location.clone(),
            timezone: ids.timezone,
            policy: setup::polling_policy(&cfg, l),
            health: cfg.file.health.clone(),
            max_gate_wait: std::time::Duration::ZERO,
        };
        let mut collector = StationCollector::new(
            claim,
            collector_cfg,
            sources,
            sink,
            events_tx.clone(),
            hint_rx,
            Arc::clone(&clock),
        );
        if let Some(s) = &store {
            // 60 h covers every local day a restored market can settle on
            // (its whole day, for the final high), see `crate::restore`.
            let since = clock.now() - Duration::hours(60);
            let mut obs = s
                .observations_since(&ids.station, since)
                .await
                .context("loading recent observations")?;
            obs.sort_by_key(|o| (o.key.observed_at, o.version));
            tracing::info!(station = %ids.station, observations = obs.len(), "warm start from storage");
            for o in &obs {
                warm_events.push(EventEnvelope::new(
                    o.fetched_at,
                    EventSource::Replay,
                    WeatherMachineEvent::WeatherObservation(ObservationEvent {
                        observation: o.clone(),
                        class: DedupClass::New,
                    }),
                ));
            }
            collector.warm_start(obs);
        }
        collector_status.insert(ids.station.clone(), collector.status());
        hint_txs.insert(ids.station.clone(), hint_tx);
        tasks.push(tokio::spawn(collector.run(shutdown.clone())));
    }

    // -- Day-1 forecast (predictive input; used only by a model that adopted it) -----------
    if let (Some(product), Some(client)) = (
        cfg.forecast_product(),
        setup::forecast_client(&cfg, &providers),
    ) {
        let targets = cfg
            .locations
            .iter()
            .map(|l| {
                let ids = setup::location_ids(l)?;
                Ok(ForecastTarget {
                    location: ids.location,
                    tz: ids.timezone,
                    latitude: l.station.latitude,
                    longitude: l.station.longitude,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tasks.push(tokio::spawn(forecast_loop(
            client,
            targets,
            product,
            std::time::Duration::from_secs(cfg.file.forecast.refresh_minutes.max(15) * 60),
            events_tx.clone(),
            Arc::clone(&side),
            Arc::clone(&clock),
            shutdown.clone(),
        )));
    }

    // -- Markets: discovery, stream, REST fallback --------------------------------------
    let (assets_tx, assets_rx) = watch::channel(Vec::<TokenId>::new());
    let (today_tx, today_rx) = watch::channel(Vec::<TokenId>::new());
    let mut stream_status: Option<watch::Receiver<StreamStatus>> = None;
    if let Some(f) = providers.fetcher("polymarket_gamma") {
        let specs = cfg
            .locations
            .iter()
            .map(setup::market_spec)
            .collect::<Result<Vec<_>>>()?;
        let gamma = GammaClient::new(Arc::clone(f), &cfg.file.providers.polymarket_gamma.base_url);
        tasks.push(tokio::spawn(discovery_loop(
            gamma,
            specs,
            store.clone(),
            events_tx.clone(),
            assets_tx,
            today_tx,
            Arc::clone(&side),
            Arc::clone(&clock),
            shutdown.clone(),
        )));
    } else {
        tracing::warn!("polymarket_gamma disabled: no markets will be discovered");
    }
    if let Some(gate) = providers.gate("polymarket_ws") {
        let stream = MarketStream::new(
            MarketStreamConfig {
                url: cfg.file.providers.polymarket_ws.base_url.clone(),
                ..MarketStreamConfig::default()
            },
            Arc::clone(gate),
            Arc::clone(&clock),
        );
        stream_status = Some(stream.status());
        tasks.push(tokio::spawn(stream.run(
            assets_rx,
            events_tx.clone(),
            shutdown.clone(),
        )));
    }
    if let Some(f) = providers.fetcher("polymarket_clob") {
        let clob = ClobClient::new(Arc::clone(f), &cfg.file.providers.polymarket_clob.base_url);
        tasks.push(tokio::spawn(book_fallback_loop(
            clob,
            today_rx,
            stream_status.clone(),
            events_tx.clone(),
            Arc::clone(&clock),
            shutdown.clone(),
        )));
    }
    drop(events_tx);

    // -- Persistence task ------------------------------------------------------------
    let storage_ok = Arc::new(AtomicBool::new(store.is_some()));
    let persist_tx = store.clone().map(|s| {
        let (tx, rx) = mpsc::channel::<PersistBatch>(1_024);
        tasks.push(tokio::spawn(persistence_loop(
            s,
            run_id,
            rx,
            Arc::clone(&storage_ok),
            cfg.file.app.journal,
            Arc::clone(&side),
        )));
        tx
    });

    // -- Journal retention (bounded disk use) ----------------------------------------------
    if let Some(s) = store.clone()
        && cfg.file.app.journal_book_retention_days > 0
    {
        tasks.push(tokio::spawn(journal_retention_loop(
            s,
            cfg.file.app.journal_book_retention_days,
            Arc::clone(&clock),
            shutdown.clone(),
        )));
    }

    // -- Engine loop ---------------------------------------------------------------------
    // The strategy pages: the catalog, and strategy F's slots from the
    // installed model's peak times (updated when a new model is installed).
    let strategy_catalog = crate::strategies::catalog(&cfg);
    let peak_slot_cfg = cfg.peak_slot();
    let mut peak_times = model.peak_times().cloned();
    let mut session =
        SimulationSession::new(engine_cfg, SimConfig::default(), settle_grace(), model)
            .with_event_capture(true);
    session.engine_mut().set_storage_ok(store.is_some());
    let mut last_knowledge = DateTime::<Utc>::MIN_UTC;
    for e in warm_events {
        last_knowledge = last_knowledge.max(e.available_at);
        session.push(e);
    }
    let mut loop_state = EngineLoopState {
        record_books: cfg.file.app.record_orderbooks,
        books: BookRecorder::new(Duration::seconds(
            i64::try_from(cfg.file.app.book_record_interval_secs.max(1)).unwrap_or(10),
        )),
        ..EngineLoopState::default()
    };
    let warm = session.run_until(clock.now());
    loop_state.absorb(&session, warm, &persist_tx, &storage_ok, &hint_txs, &side);
    last_knowledge = last_knowledge.max(session.last_time().unwrap_or(last_knowledge));

    // -- The paper book of earlier runs (positions, today's limits) -------------------------
    // After the weather replay, so the replay records no evaluations of the
    // restored markets; before the first live event, so no strategy trades
    // without seeing them.
    if let Some(s) = &store {
        restore_paper_book(s, &cfg, &mut session, clock.now(), &side).await;
    }

    let gates_for_ui: Vec<Arc<ProviderGate>> = [
        "polymarket_gamma",
        "polymarket_clob",
        "polymarket_ws",
        "iem",
        "open_meteo",
    ]
    .iter()
    .filter_map(|n| providers.gate(n).cloned())
    .collect();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
        cfg.file.app.heartbeat_secs.max(5),
    ));
    let mut publish_tick = tokio::time::interval(std::time::Duration::from_millis(
        cfg.file.app.snapshot_interval_ms,
    ));
    publish_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown_rx = shutdown.clone();
    ready.store(true, Ordering::Release);
    tracing::info!("paper runtime ready");

    loop {
        let mut publish = false;
        tokio::select! {
            ev = events_rx.recv() => match ev {
                Some(mut env) => {
                    let now = clock.now();
                    last_knowledge = now.max(last_knowledge);
                    env.available_at = last_knowledge;
                    env.recorded_at = now;
                    session.push(env);
                }
                None => {
                    if !*shutdown_rx.borrow() {
                        tracing::warn!("all event producers stopped");
                    }
                    break;
                }
            },
            cmd = commands.recv() => if let Some(c) = cmd {
                let now = clock.now();
                last_knowledge = now.max(last_knowledge);
                session.push(EventEnvelope::new(last_knowledge, EventSource::Operator, WeatherMachineEvent::Operator(c)));
            },
            _ = heartbeat.tick() => {
                let now = clock.now();
                last_knowledge = now.max(last_knowledge);
                session.push(EventEnvelope::new(last_knowledge, EventSource::Live, WeatherMachineEvent::Timer(TimerEvent { due_at: last_knowledge, kind: TimerKind::Heartbeat })));
            }
            _ = publish_tick.tick() => publish = true,
            m = models_rx.recv(), if models_open => match m {
                Some(m) => {
                    let previous = session.engine().model_id().to_owned();
                    session.engine_mut().set_model(Arc::clone(&m) as Arc<dyn ProbabilityModel>);
                    set_loaded(&model_status, &m);
                    peak_times = m.peak_times.clone();
                    tracing::info!(previous = %previous, model = %m.id, "probability model installed");
                    if let Some(s) = store.clone() {
                        let details = serde_json::json!({
                            "run_id": run_id.to_string(),
                            "previous": previous,
                            "model": m.id,
                            "forecast": m.forecast.as_ref().map(|f| f.verdict.clone()),
                        });
                        tokio::spawn(async move {
                            let _ = s
                                .record_system_event("info", "model", "probability model installed", &details)
                                .await;
                        });
                    }
                }
                None => models_open = false,
            },
            r = shutdown_rx.changed() => if r.is_err() || *shutdown_rx.borrow() { break; },
        }
        let ok = storage_ok.load(Ordering::Acquire) && !loop_state.backlogged;
        session.engine_mut().set_storage_ok(ok);
        metrics::gauge!("wm_storage_ok").set(if ok { 1.0 } else { 0.0 });
        let out = session.run_until(clock.now());
        loop_state.absorb(&session, out, &persist_tx, &storage_ok, &hint_txs, &side);
        loop_state.flush(clock.now(), &persist_tx, &storage_ok, &side);
        if publish {
            let snap = session.engine().snapshot();
            metrics::gauge!("wm_global_exposure_usd").set(snap.exposure.global_worst_case.as_f64());
            metrics::gauge!("wm_kill_switch").set(if snap.kill_switch.is_some() {
                1.0
            } else {
                0.0
            });
            let statuses: HashMap<StationId, CollectorStatus> = collector_status
                .iter()
                .map(|(k, v)| (k.clone(), v.borrow().clone()))
                .collect();
            let stream = stream_status.as_ref().map(|s| s.borrow().clone());
            let now = clock.now();
            let extra: Vec<ProviderHealthSnapshot> =
                gates_for_ui.iter().map(|g| gate_snapshot(g, now)).collect();
            let (alerts, reviews) = with_side(&side, |st| {
                (
                    st.alerts.iter().cloned().collect::<Vec<_>>(),
                    st.rules_review.clone(),
                )
            });
            let model_now = model_status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let inputs = DtoInputs {
                demo: false,
                instance: &cfg.file.app.instance,
                collectors: &statuses,
                stream: stream.as_ref(),
                alerts: &alerts,
                confirmed_filters: &confirmed_filters,
                extra_providers: &extra,
                rules_review: &reviews,
                model: &model_now,
                yes_pooling: cfg.buy_yes().pooling(),
                no_pooling: cfg.buy_no().pooling(),
                strategies: &strategy_catalog,
                peak_slot: &peak_slot_cfg,
                peak_times: peak_times.as_ref(),
            };
            publisher.publish(dto::build(&snap, &inputs, now));
        }
    }

    // -- Shutdown --------------------------------------------------------------------------
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    ready.store(false, Ordering::Release);
    // Hand over anything still held back (full queue or pending book changes)
    // before closing the writer's queue.
    if let Some(tx) = &persist_tx {
        let mut held = loop_state.overflow.take().unwrap_or_default();
        if loop_state.record_books {
            loop_state
                .books
                .due(clock.now() + Duration::days(1), &mut held.books);
        }
        if !held.is_empty()
            && tokio::time::timeout(std::time::Duration::from_secs(10), tx.send(held))
                .await
                .is_err()
        {
            tracing::error!("persistence writer did not accept the final batch within 10 s");
        }
    }
    drop(persist_tx);
    for t in tasks {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), t).await;
    }
    for l in leases {
        let _ = l.release().await;
    }
    if let Some(s) = &store {
        let _ = s
            .record_system_event(
                "info",
                "shutdown",
                "weather machine stopped",
                &serde_json::json!({ "run_id": run_id.to_string() }),
            )
            .await;
    }
    Ok(())
}

/// Hourly: delete journal order-book updates older than the retention.
async fn journal_retention_loop(
    store: PgStore,
    days: u32,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    // First pass shortly after start, then hourly.
    let mut wait = std::time::Duration::from_secs(120);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
        }
        wait = std::time::Duration::from_secs(3600);
        let before = clock.now() - Duration::days(i64::from(days));
        match store.prune_journal_books(before).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                deleted = n,
                days,
                "journal order-book updates past retention deleted"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "journal retention pass failed; retrying in an hour")
            }
        }
    }
}

/// When the probability model is (re)trained.
#[derive(Debug, Clone)]
struct MaintenancePolicy {
    /// Wait after a failed attempt.
    retry_after: std::time::Duration,
    /// Retrain models older than this.
    retrain_after: Option<Duration>,
    /// The configured forecast product (`None`: forecasts off).
    product: Option<ForecastProduct>,
    /// `false` when the operator supplied the model file: train it only when
    /// it is missing, never replace it.
    retrain_existing: bool,
    /// Training compares the model structures: a model trained without the
    /// comparison is retrained once, so the comparison does not wait for
    /// the next periodic retraining.
    compare_structures: bool,
}

/// Why the model should be (re)trained now, if at all.
fn training_due(
    model: Option<&EmpiricalPeakModel>,
    p: &MaintenancePolicy,
    now: DateTime<Utc>,
) -> Option<String> {
    let Some(m) = model else {
        return Some("no model yet".into());
    };
    if !p.retrain_existing {
        return None;
    }
    let retry = Duration::from_std(p.retry_after).unwrap_or_else(|_| Duration::hours(6));
    if let Some(product) = &p.product {
        match &m.forecast {
            None => return Some(format!("evaluate the {} forecast", product.label())),
            Some(f) if f.product != *product => {
                return Some(format!("forecast changed to {}", product.label()));
            }
            Some(f) if !f.evaluated && now - f.evaluated_at >= retry => {
                return Some("forecast history was unavailable at the last training".into());
            }
            _ => {}
        }
    }
    if p.compare_structures && m.selection.is_none() {
        return Some("compare the candidate model structure".into());
    }
    if m.peak_times.is_none() {
        return Some("learn when each season's high is first reported (strategy F)".into());
    }
    match p.retrain_after {
        Some(age) if now - m.created_at >= age => Some(format!(
            "model is {} days old",
            (now - m.created_at).num_days()
        )),
        _ => None,
    }
}

fn progress_text(p: &Progress) -> (String, Option<(u32, u32)>) {
    match *p {
        Progress::Downloading { year, done, total } => (
            format!("downloading METAR history from IEM: {year} ({done}/{total} years)"),
            Some((done, total)),
        ),
        Progress::Forecast { year, done } => (
            format!("downloading day-1 forecast history: {year} ({done} years so far)"),
            None,
        ),
        Progress::Training { observations } => (
            format!("training and evaluating on {observations} historical reports"),
            None,
        ),
    }
}

/// Keeps the model current: trains it when missing, evaluates the forecast
/// once it is enabled, and retrains periodically — in the background, while
/// the current model keeps trading. Each new model goes to the engine loop.
#[allow(clippy::too_many_arguments)]
async fn model_maintenance_loop(
    archive: IemArchive,
    forecast: Option<OpenMeteoPreviousRuns>,
    mut plan: TrainPlan,
    policy: MaintenancePolicy,
    mut current: Option<Arc<EmpiricalPeakModel>>,
    audit: Option<Arc<dyn wm_core::ingest::IngestSink>>,
    status: SharedModel,
    side: SharedSide,
    installed: mpsc::Sender<Arc<EmpiricalPeakModel>>,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
    loop {
        let now = clock.now();
        let wait = match training_due(current.as_deref(), &policy, now) {
            None => CHECK_EVERY,
            Some(reason) => {
                plan.today = now.date_naive();
                let background = current.is_some();
                let st = Arc::clone(&status);
                let progress = move |p: Progress| {
                    let (text, fraction) = progress_text(&p);
                    if background {
                        update_model(&st, |g| g.retraining = Some(format!("retraining: {text}")));
                    } else {
                        set_model(&st, "training", text, fraction);
                    }
                };
                tracing::info!(station = %plan.station, %reason, background, "training the probability model");
                if background {
                    update_model(&status, |g| {
                        g.retraining = Some(format!("retraining: {reason}"))
                    });
                } else {
                    set_model(&status, "training", format!("starting: {reason}"), None);
                }
                let result = training::train(
                    &archive,
                    forecast.as_ref(),
                    &plan,
                    audit.as_ref(),
                    &progress,
                    &mut shutdown,
                )
                .await
                .and_then(|o| setup::read_model(&plan.model_out).map(|m| (o, m)));
                match result {
                    Ok((o, m)) => {
                        let msg = format!(
                            "model {} trained on {} days ({} → {}), {} samples{}{}",
                            o.model_id,
                            o.days,
                            o.from.map(|d| d.to_string()).unwrap_or_default(),
                            o.to.map(|d| d.to_string()).unwrap_or_default(),
                            o.samples,
                            o.structure_verdict
                                .as_deref()
                                .map(|v| format!("; {v}"))
                                .unwrap_or_default(),
                            o.forecast_verdict
                                .as_deref()
                                .map(|v| format!("; forecast {v}"))
                                .unwrap_or_default()
                        );
                        tracing::info!(
                            years_downloaded = o.years_downloaded,
                            years_cached = o.years_cached,
                            observations = o.observations,
                            "{msg}"
                        );
                        with_side(&side, |s| s.alert("info", msg));
                        let m = Arc::new(m);
                        if installed.send(Arc::clone(&m)).await.is_err() {
                            return;
                        }
                        current = Some(m);
                        CHECK_EVERY
                    }
                    Err(_) if *shutdown.borrow() => return,
                    Err(e) => {
                        let retry_at = clock.now()
                            + Duration::from_std(policy.retry_after)
                                .unwrap_or_else(|_| Duration::hours(6));
                        let next = retry_at.format("%Y-%m-%d %H:%M");
                        match &current {
                            Some(m) => {
                                let msg = format!(
                                    "retraining failed: {e:#}; keeping model {} — next attempt {next} UTC",
                                    m.id
                                );
                                tracing::warn!("{msg}");
                                update_model(&status, |g| g.retraining = None);
                                with_side(&side, |s| s.alert("warning", msg));
                            }
                            None => {
                                let msg = format!(
                                    "model training failed: {e:#}; next attempt {next} UTC — no weather trades until then"
                                );
                                tracing::warn!("{msg}");
                                set_model(&status, "failed", msg.clone(), None);
                                with_side(&side, |s| s.alert("warning", msg));
                            }
                        }
                        policy.retry_after
                    }
                }
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
        }
    }
}

/// A location whose day-1 forecast is fetched.
#[derive(Debug, Clone)]
struct ForecastTarget {
    location: wm_core::ids::LocationId,
    tz: chrono_tz::Tz,
    latitude: f64,
    longitude: f64,
}

/// Fetches each location's day-1 series every `refresh` and just after each
/// day's ready time, and hands it to the engine. Failures only mean the
/// model runs without the forecast; they are alerted once until recovery.
#[allow(clippy::too_many_arguments)]
async fn forecast_loop(
    client: OpenMeteoPreviousRuns,
    targets: Vec<ForecastTarget>,
    product: ForecastProduct,
    refresh: std::time::Duration,
    events: mpsc::Sender<EventEnvelope>,
    side: SharedSide,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut failing = false;
    loop {
        for t in &targets {
            let query = ForecastQuery {
                location: t.location.clone(),
                latitude: t.latitude,
                longitude: t.longitude,
                local_date: local_date(clock.now(), t.tz),
            };
            let fetched = tokio::select! {
                r = client.fetch(&query, std::time::Duration::from_secs(60)) => r,
                _ = shutdown.changed() => return,
            };
            match fetched {
                Ok(ev) => {
                    if failing {
                        failing = false;
                        with_side(&side, |s| s.alert("info", "day-1 forecast available again"));
                    }
                    let env = EventEnvelope::new(
                        clock.now(),
                        EventSource::Live,
                        WeatherMachineEvent::ForecastUpdate(ev),
                    );
                    if events.send(env).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(location = %t.location, error = %e, "day-1 forecast unavailable");
                    if !failing {
                        failing = true;
                        with_side(&side, |s| {
                            s.alert(
                                "warning",
                                format!(
                                    "day-1 forecast unavailable ({e}); the model runs without it"
                                ),
                            )
                        });
                    }
                }
            }
        }
        // Next refresh, or two minutes after the next local ready time.
        let now = clock.now();
        let next_ready = targets
            .iter()
            .map(|t| {
                let today = local_date(now, t.tz);
                let r = product.usable_from(today, t.tz);
                if r > now {
                    r
                } else {
                    product.usable_from(today.succ_opt().unwrap_or(today), t.tz)
                }
            })
            .min();
        let wait = next_ready
            .and_then(|r| (r + Duration::minutes(2) - now).to_std().ok())
            .map_or(refresh, |w| w.min(refresh));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
        }
    }
}

/// Book-keeping around the kernel's outputs.
#[derive(Default)]
struct EngineLoopState {
    record_books: bool,
    books: BookRecorder,
    /// Batches the writer could not accept yet (queue full). Merged and sent
    /// first on the next attempt; never dropped.
    overflow: Option<PersistBatch>,
    /// Inside a backlog episode (one alert per episode, positions blocked).
    backlogged: bool,
}

/// Market-history recorder: a token's book is stored when it changed, at most
/// once per `interval`; the latest change inside the interval is kept and
/// stored when the interval ends, so the end of a burst is never lost.
#[derive(Default)]
struct BookRecorder {
    interval: Duration,
    last: HashMap<TokenId, (DateTime<Utc>, u64)>,
    pending: HashMap<TokenId, OrderBook>,
}

fn book_fingerprint(b: &OrderBook) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for l in b.bids.iter().chain(b.asks.iter()) {
        l.price.micros().hash(&mut h);
        l.size.micros().hash(&mut h);
    }
    b.bids.len().hash(&mut h);
    h.finish()
}

impl BookRecorder {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            ..Self::default()
        }
    }

    fn offer(&mut self, book: &OrderBook, at: DateTime<Utc>, out: &mut Vec<OrderBook>) {
        let fp = book_fingerprint(book);
        match self.last.get(&book.token) {
            Some((_, f)) if *f == fp => {
                self.pending.remove(&book.token);
            }
            Some((t, _)) if at - *t < self.interval => {
                self.pending.insert(book.token.clone(), book.clone());
            }
            _ => {
                self.last.insert(book.token.clone(), (at, fp));
                self.pending.remove(&book.token);
                out.push(book.clone());
            }
        }
    }

    /// Store pending changes whose interval has ended.
    fn due(&mut self, now: DateTime<Utc>, out: &mut Vec<OrderBook>) {
        let ready: Vec<TokenId> = self
            .pending
            .keys()
            .filter(|t| {
                self.last
                    .get(*t)
                    .is_none_or(|(at, _)| now - *at >= self.interval)
            })
            .cloned()
            .collect();
        for t in ready {
            if let Some(b) = self.pending.remove(&t) {
                self.last.insert(t, (now, book_fingerprint(&b)));
                out.push(b);
            }
        }
    }
}

impl EngineLoopState {
    fn absorb(
        &mut self,
        session: &SimulationSession,
        out: SessionOutput,
        persist: &Option<mpsc::Sender<PersistBatch>>,
        storage_ok: &Arc<AtomicBool>,
        hints: &HashMap<StationId, watch::Sender<PollingHints>>,
        side: &SharedSide,
    ) {
        if out.is_empty() {
            return;
        }
        metrics::counter!("wm_engine_events_total").increment(out.events);
        metrics::counter!("wm_decisions_total").increment(out.decisions.len() as u64);
        metrics::counter!("wm_orders_approved_total").increment(out.approved.len() as u64);
        for (station, h) in &out.hints {
            if let Some(tx) = hints.get(station) {
                tx.send_replace(PollingHints {
                    peak_watch: h.peak_watch,
                    has_exposure: h.has_exposure,
                });
            }
        }
        with_side(side, |st| {
            for a in &out.alerts {
                st.alert(
                    if a.contains("KILL") {
                        "critical"
                    } else {
                        "warning"
                    },
                    a.clone(),
                );
            }
            for t in &out.trades {
                st.alert(
                    "info",
                    format!(
                        "paper fill {} {} {} @ {} × {}",
                        t.strategy, t.side, t.bucket_label, t.price, t.shares
                    ),
                );
            }
            for s in &out.settlements {
                st.alert(
                    "info",
                    format!(
                        "settled {} at {} °C (observed): PnL {}",
                        s.event_slug, s.final_value, s.pnl
                    ),
                );
            }
        });
        let Some(tx) = persist else { return };
        let mut batch = PersistBatch {
            decisions: out.decisions,
            ..PersistBatch::default()
        };
        let mut touched: BTreeSet<String> = out
            .approved
            .iter()
            .map(|a| a.client_order_id().to_string())
            .collect();
        for env in &out.processed {
            match &env.event {
                WeatherMachineEvent::OrderUpdate(u) => {
                    touched.insert(u.update.client_order_id.to_string());
                    if let Some(f) = &u.fill {
                        batch.fills.push(f.clone());
                    }
                }
                WeatherMachineEvent::OrderBookUpdate(b) if self.record_books => {
                    self.books
                        .offer(&b.book, env.available_at, &mut batch.books);
                }
                _ => {}
            }
        }
        batch.orders = session
            .engine()
            .orders()
            .recent(10_000)
            .into_iter()
            .filter(|o| touched.contains(o.client_order_id.as_str()))
            .map(order_row)
            .collect();
        batch.events = out.processed;
        self.submit(batch, tx, storage_ok, side);
    }

    /// Periodic work without new kernel output: store book changes whose
    /// interval ended and retry a held-back batch.
    fn flush(
        &mut self,
        now: DateTime<Utc>,
        persist: &Option<mpsc::Sender<PersistBatch>>,
        storage_ok: &Arc<AtomicBool>,
        side: &SharedSide,
    ) {
        let Some(tx) = persist else { return };
        let mut batch = PersistBatch::default();
        if self.record_books {
            self.books.due(now, &mut batch.books);
        }
        self.submit(batch, tx, storage_ok, side);
    }

    /// Hand a batch to the writer. A full queue never loses records: the batch
    /// is held (merged with anything held before) and new positions stay
    /// blocked until the writer has caught up.
    fn submit(
        &mut self,
        batch: PersistBatch,
        tx: &mpsc::Sender<PersistBatch>,
        storage_ok: &Arc<AtomicBool>,
        side: &SharedSide,
    ) {
        let batch = match self.overflow.take() {
            Some(mut held) => {
                held.merge(batch);
                held
            }
            None => batch,
        };
        if batch.is_empty() {
            return;
        }
        match tx.try_send(batch) {
            Ok(()) => {
                if self.backlogged {
                    self.backlogged = false;
                    with_side(side, |st| {
                        st.alert(
                            "info",
                            "persistence caught up — nothing was lost; new positions allowed again",
                        )
                    });
                }
            }
            Err(mpsc::error::TrySendError::Full(mut held)) => {
                storage_ok.store(false, Ordering::Release);
                if !self.backlogged {
                    self.backlogged = true;
                    metrics::counter!("wm_persist_backlog_episodes_total").increment(1);
                    with_side(side, |st| {
                        st.alert("critical", "persistence backlog — new positions blocked until the writer catches up")
                    });
                }
                let shed = held.shed_books(MAX_PENDING_EVENTS);
                if shed > 0 {
                    metrics::counter!("wm_journal_shed_total").increment(shed as u64);
                    with_side(side, |st| {
                        st.alert("critical", format!("journal gap: {shed} order-book updates not persisted while storage was stalled"))
                    });
                }
                self.overflow = Some(held);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => storage_ok.store(false, Ordering::Release),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use wm_core::market::BookLevel;
    use wm_core::units::{Price, Shares};

    fn product(model: &str) -> ForecastProduct {
        ForecastProduct {
            provider: wm_core::ids::ProviderId::open_meteo(),
            model: model.into(),
            lead_days: 1,
            ready_local_minute: 480,
        }
    }

    fn policy() -> MaintenancePolicy {
        MaintenancePolicy {
            retry_after: std::time::Duration::from_secs(6 * 3600),
            retrain_after: Some(Duration::days(30)),
            product: Some(product("gfs_global")),
            retrain_existing: true,
            compare_structures: false,
        }
    }

    fn model_at(created: &str, forecast: Option<(bool, &str)>) -> EmpiricalPeakModel {
        let mut m =
            EmpiricalPeakModel::new("m", "EHAM", "all", 4, EmpiricalPeakModel::default_levels());
        m.created_at = t(created);
        m.peak_times = Some(wm_strategy::PeakTimes::default());
        m.forecast = forecast.map(|(evaluated, model)| wm_strategy::ForecastModelInfo {
            product: product(model),
            adopted: false,
            verdict: "v".into(),
            days_with_forecast: 0,
            evaluated,
            evaluated_at: t(created),
        });
        m
    }

    #[test]
    fn training_is_due_for_missing_unevaluated_changed_and_old_models() {
        let p = policy();
        let now = t("2026-09-27T12:00:00Z");
        assert_eq!(training_due(None, &p, now).as_deref(), Some("no model yet"));
        // A model trained before forecasts existed: evaluate them now.
        let old = model_at("2026-09-26T12:00:00Z", None);
        assert!(
            training_due(Some(&old), &p, now)
                .unwrap()
                .contains("evaluate")
        );
        // Evaluated, fresh: nothing to do.
        let fresh = model_at("2026-09-26T12:00:00Z", Some((true, "gfs_global")));
        assert_eq!(training_due(Some(&fresh), &p, now), None);
        // Another forecast model configured since.
        let other = model_at("2026-09-26T12:00:00Z", Some((true, "ecmwf_ifs")));
        assert!(
            training_due(Some(&other), &p, now)
                .unwrap()
                .contains("changed")
        );
        // History was unavailable: retried only after `retry_after`.
        let failed = model_at("2026-09-27T09:00:00Z", Some((false, "gfs_global")));
        assert_eq!(training_due(Some(&failed), &p, now), None, "within 6 h");
        assert!(training_due(Some(&failed), &p, t("2026-09-27T15:00:01Z")).is_some());
        // Age.
        let aged = model_at("2026-08-27T11:00:00Z", Some((true, "gfs_global")));
        assert!(
            training_due(Some(&aged), &p, now)
                .unwrap()
                .contains("31 days old")
        );
        let never = MaintenancePolicy {
            retrain_after: None,
            ..policy()
        };
        assert_eq!(training_due(Some(&aged), &never, now), None);
        // Forecasts off: an unevaluated model is fine.
        let off = MaintenancePolicy {
            product: None,
            ..policy()
        };
        assert_eq!(training_due(Some(&old), &off, now), None);
        // An operator-supplied model file is never replaced.
        let operator = MaintenancePolicy {
            retrain_existing: false,
            ..policy()
        };
        assert_eq!(training_due(Some(&aged), &operator, now), None);
        assert!(
            training_due(None, &operator, now).is_some(),
            "but trained when missing"
        );
    }

    #[test]
    fn a_model_without_peak_times_is_retrained_once() {
        let p = policy();
        let now = t("2026-09-29T12:00:00Z");
        let mut m = model_at("2026-09-28T12:00:00Z", Some((true, "gfs_global")));
        assert_eq!(training_due(Some(&m), &p, now), None, "has them: fresh");
        m.peak_times = None;
        assert!(
            training_due(Some(&m), &p, now)
                .unwrap()
                .contains("strategy F")
        );
        // Never for an operator-supplied model file.
        let operator = MaintenancePolicy {
            retrain_existing: false,
            ..p
        };
        assert_eq!(training_due(Some(&m), &operator, now), None);
    }

    #[test]
    fn a_model_without_the_structure_comparison_is_retrained_once() {
        let p = MaintenancePolicy {
            compare_structures: true,
            ..policy()
        };
        let now = t("2026-09-28T12:00:00Z");
        let mut m = model_at("2026-09-27T12:00:00Z", Some((true, "gfs_global")));
        assert!(
            training_due(Some(&m), &p, now)
                .unwrap()
                .contains("model structure")
        );
        m.selection = Some(wm_strategy::StructureSelection {
            structure: wm_strategy::ModelStructure::Current,
            candidate_adopted: false,
            verdict: "current structure kept: …".into(),
            evaluated_at: t("2026-09-28T11:00:00Z"),
        });
        assert_eq!(training_due(Some(&m), &p, now), None, "compared: fresh");
        // Never for an operator-supplied model file.
        let operator = MaintenancePolicy {
            retrain_existing: false,
            ..p
        };
        m.selection = None;
        assert_eq!(training_due(Some(&m), &operator, now), None);
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn book(token: &str, bid: &str, at: &str) -> OrderBook {
        OrderBook {
            token: TokenId::new(token).unwrap(),
            bids: vec![BookLevel {
                price: Price::parse(bid).unwrap(),
                size: Shares::parse("100").unwrap(),
            }],
            asks: vec![],
            tick_size: Price::parse("0.01").unwrap(),
            min_order_size: Shares::parse("5").unwrap(),
            exchange_ts: None,
            received_at: t(at),
            hash: None,
            confirmed_at: None,
        }
    }

    fn book_env(b: OrderBook) -> EventEnvelope {
        EventEnvelope::new(
            b.received_at,
            EventSource::Live,
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
        )
    }

    #[test]
    fn recorder_stores_changes_at_most_once_per_interval_and_keeps_the_last() {
        let mut r = BookRecorder::new(Duration::seconds(10));
        let mut out = Vec::new();
        r.offer(
            &book("a", "0.40", "2026-09-27T12:00:00Z"),
            t("2026-09-27T12:00:00Z"),
            &mut out,
        );
        assert_eq!(out.len(), 1, "first sight is stored");
        r.offer(
            &book("a", "0.40", "2026-09-27T12:00:03Z"),
            t("2026-09-27T12:00:03Z"),
            &mut out,
        );
        assert_eq!(out.len(), 1, "unchanged book is not stored again");
        r.offer(
            &book("a", "0.41", "2026-09-27T12:00:04Z"),
            t("2026-09-27T12:00:04Z"),
            &mut out,
        );
        r.offer(
            &book("a", "0.42", "2026-09-27T12:00:06Z"),
            t("2026-09-27T12:00:06Z"),
            &mut out,
        );
        assert_eq!(out.len(), 1, "changes inside the interval are held");
        r.due(t("2026-09-27T12:00:09Z"), &mut out);
        assert_eq!(out.len(), 1);
        r.due(t("2026-09-27T12:00:10Z"), &mut out);
        assert_eq!(
            out.len(),
            2,
            "the latest held change is stored when the interval ends"
        );
        assert_eq!(out[1].bids[0].price, Price::parse("0.42").unwrap());
        r.due(t("2026-09-27T12:01:00Z"), &mut out);
        assert_eq!(out.len(), 2, "nothing pending");
        // A change back to an already stored state is still a change.
        r.offer(
            &book("a", "0.40", "2026-09-27T12:01:00Z"),
            t("2026-09-27T12:01:00Z"),
            &mut out,
        );
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn a_full_queue_holds_batches_blocks_trading_and_alerts_once() {
        let (tx, mut rx) = mpsc::channel::<PersistBatch>(1);
        let ok = Arc::new(AtomicBool::new(true));
        let side: SharedSide = Arc::new(Mutex::new(SideState::default()));
        let mut st = EngineLoopState::default();
        let batch = |n: usize| PersistBatch {
            events: (0..n)
                .map(|_| book_env(book("a", "0.40", "2026-09-27T12:00:00Z")))
                .collect(),
            ..PersistBatch::default()
        };
        st.submit(batch(1), &tx, &ok, &side);
        assert!(!st.backlogged);
        for _ in 0..5 {
            st.submit(batch(2), &tx, &ok, &side);
        }
        assert!(st.backlogged);
        assert!(!ok.load(Ordering::Acquire), "new positions blocked");
        assert_eq!(
            st.overflow.as_ref().unwrap().events.len(),
            10,
            "nothing dropped"
        );
        let alerts = |side: &SharedSide| with_side(side, |s| s.alerts.len());
        assert_eq!(alerts(&side), 1, "one alert per episode");
        // The writer catches up: the held batch goes out whole and in order.
        assert_eq!(rx.try_recv().unwrap().events.len(), 1);
        st.flush(t("2026-09-27T12:00:01Z"), &Some(tx.clone()), &ok, &side);
        assert!(!st.backlogged && st.overflow.is_none());
        assert_eq!(rx.try_recv().unwrap().events.len(), 10);
        assert_eq!(alerts(&side), 2, "recovery is announced");
    }

    #[test]
    fn shedding_drops_only_order_book_entries() {
        let mut b = PersistBatch {
            events: (0..5)
                .map(|_| book_env(book("a", "0.40", "2026-09-27T12:00:00Z")))
                .chain(std::iter::once(EventEnvelope::new(
                    t("2026-09-27T12:00:00Z"),
                    EventSource::Operator,
                    WeatherMachineEvent::Timer(TimerEvent {
                        due_at: t("2026-09-27T12:00:00Z"),
                        kind: TimerKind::Heartbeat,
                    }),
                )))
                .collect(),
            books: vec![book("a", "0.40", "2026-09-27T12:00:00Z")],
            ..PersistBatch::default()
        };
        assert_eq!(b.shed_books(10), 0, "under the cap nothing is shed");
        assert_eq!(b.shed_books(3), 6);
        assert_eq!(b.events.len(), 1);
        assert!(matches!(b.events[0].event, WeatherMachineEvent::Timer(_)));
        assert!(b.books.is_empty());
    }

    #[test]
    fn identical_alerts_within_a_minute_are_not_repeated() {
        let mut s = SideState::default();
        for _ in 0..30 {
            s.alert("critical", "persistence backlog");
        }
        s.alert("info", "something else");
        assert_eq!(s.alerts.len(), 2);
    }
}
