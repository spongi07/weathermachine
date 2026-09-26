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
use wm_strategy::{NoEdgeModel, ProbabilityModel};
use wm_weather::{
    CollectorConfig, CollectorRegistry, CollectorStatus, IemArchive, PollingHints, StationCollector,
};

/// The runtime stopped so that the process restarts and loads a newly trained
/// probability model (the container's restart policy brings it back).
#[derive(Debug)]
pub struct RestartRequested;

impl std::fmt::Display for RestartRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("restart requested to load the newly trained probability model")
    }
}

impl std::error::Error for RestartRequested {}

/// Process exit code for [`RestartRequested`] (EX_TEMPFAIL): any restart
/// policy, including `on-failure`, brings the service back.
pub const RESTART_EXIT_CODE: u8 = 75;

type SharedModel = Arc<Mutex<ModelDto>>;

fn set_model(status: &SharedModel, state: &str, detail: String, progress: Option<(u32, u32)>) {
    let mut g = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    g.state = state.to_owned();
    g.detail = detail;
    g.progress = progress;
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

impl PersistBatch {
    fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.decisions.is_empty()
            && self.orders.is_empty()
            && self.fills.is_empty()
            && self.books.is_empty()
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
) {
    while let Some(b) = rx.recv().await {
        let res: Result<(), wm_storage::StorageError> = async {
            if journal && !b.events.is_empty() {
                store.append_events(&run, &b.events).await?;
            }
            store.record_decisions(&run, &b.decisions).await?;
            for o in &b.orders {
                store.upsert_order(&run, o).await?;
            }
            for f in &b.fills {
                store.record_fill(f).await?;
            }
            for book in &b.books {
                store.record_orderbook(book).await?;
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => ok.store(true, Ordering::Release),
            Err(e) => {
                ok.store(false, Ordering::Release);
                metrics::counter!("wm_persist_failures_total", "component" => "engine")
                    .increment(1);
                tracing::error!(error = %e, "engine persistence failed — new positions blocked until storage recovers");
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

fn review_status_of(s: &str) -> SpecReviewStatus {
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
        self.alerts.push_back(AlertDto {
            at_ms: Utc::now().timestamp_millis(),
            level: level.into(),
            message: message.into(),
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
                            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book }),
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
    let mut train_into: Option<std::path::PathBuf> = None;
    let model: Arc<dyn ProbabilityModel> = match setup::find_model(&cfg) {
        ModelLoad::Loaded(m) => {
            set_model(
                &model_status,
                "loaded",
                format!(
                    "{} · {} samples · {} → {}",
                    m.id,
                    m.total_samples(),
                    m.trained_from,
                    m.trained_to
                ),
                None,
            );
            m
        }
        ModelLoad::Missing(path) if cfg.file.model.auto_train.enabled => {
            set_model(
                &model_status,
                "training",
                "starting: training from IEM METAR history".into(),
                None,
            );
            train_into = Some(path);
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

    // -- Probability model training (only while no model file exists) ------------------
    let (model_ready_tx, mut model_ready_rx) = watch::channel(false);
    let mut model_ready_open = true;
    let mut keep_model_ready_tx = Some(model_ready_tx);
    if let Some(path) = train_into {
        match providers.fetcher("iem") {
            Some(f) => {
                let archive = IemArchive::new(Arc::clone(f), &cfg.file.providers.iem.base_url);
                let plan = TrainPlan::from_config(&cfg, path, clock.now().date_naive())?;
                let audit: Option<Arc<dyn wm_core::ingest::IngestSink>> = store
                    .as_ref()
                    .map(|s| Arc::new(s.clone()) as Arc<dyn wm_core::ingest::IngestSink>);
                tasks.push(tokio::spawn(auto_train_loop(
                    archive,
                    plan,
                    std::time::Duration::from_secs(
                        cfg.file.model.auto_train.retry_after_secs.max(60),
                    ),
                    audit,
                    Arc::clone(&model_status),
                    Arc::clone(&side),
                    keep_model_ready_tx
                        .take()
                        .unwrap_or_else(|| watch::channel(false).0),
                    Arc::clone(&clock),
                    shutdown.clone(),
                )));
            }
            None => set_model(
                &model_status,
                "missing",
                "no model and the IEM provider is disabled — no weather trades".into(),
                None,
            ),
        }
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
            let since = clock.now() - Duration::hours(36);
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
        )));
        tx
    });

    // -- Engine loop ---------------------------------------------------------------------
    let mut session =
        SimulationSession::new(engine_cfg, SimConfig::default(), Duration::hours(2), model)
            .with_event_capture(true);
    session.engine_mut().set_storage_ok(store.is_some());
    let mut last_knowledge = DateTime::<Utc>::MIN_UTC;
    for e in warm_events {
        last_knowledge = last_knowledge.max(e.available_at);
        session.push(e);
    }
    let mut loop_state = EngineLoopState {
        record_books: cfg.file.app.record_orderbooks,
        ..EngineLoopState::default()
    };
    let warm = session.run_until(clock.now());
    loop_state.absorb(&session, warm, &persist_tx, &storage_ok, &hint_txs, &side);
    last_knowledge = last_knowledge.max(session.last_time().unwrap_or(last_knowledge));

    let gates_for_ui: Vec<Arc<ProviderGate>> = [
        "polymarket_gamma",
        "polymarket_clob",
        "polymarket_ws",
        "iem",
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
    let mut restart = false;
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
            r = model_ready_rx.changed(), if model_ready_open => match r {
                Ok(()) if *model_ready_rx.borrow() => {
                    tracing::info!("new probability model installed: restarting to load it");
                    restart = true;
                    break;
                }
                Ok(()) => {}
                Err(_) => model_ready_open = false,
            },
            r = shutdown_rx.changed() => if r.is_err() || *shutdown_rx.borrow() { break; },
        }
        let ok = storage_ok.load(Ordering::Acquire);
        session.engine_mut().set_storage_ok(ok);
        metrics::gauge!("wm_storage_ok").set(if ok { 1.0 } else { 0.0 });
        let out = session.run_until(clock.now());
        loop_state.absorb(&session, out, &persist_tx, &storage_ok, &hint_txs, &side);
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
            };
            publisher.publish(dto::build(&snap, &inputs, now));
        }
    }

    // -- Shutdown --------------------------------------------------------------------------
    tracing::info!("shutting down");
    let _ = stop_tx.send(true);
    ready.store(false, Ordering::Release);
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
                if restart {
                    "weather machine restarting to load a new model"
                } else {
                    "weather machine stopped"
                },
                &serde_json::json!({ "run_id": run_id.to_string() }),
            )
            .await;
    }
    drop(keep_model_ready_tx);
    if restart {
        return Err(RestartRequested.into());
    }
    Ok(())
}

/// Train until a model is installed, retrying after failures. Stops on
/// shutdown; signals `ready` once the model file is in place.
#[allow(clippy::too_many_arguments)]
async fn auto_train_loop(
    archive: IemArchive,
    mut plan: TrainPlan,
    retry_after: std::time::Duration,
    audit: Option<Arc<dyn wm_core::ingest::IngestSink>>,
    status: SharedModel,
    side: SharedSide,
    ready: watch::Sender<bool>,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        plan.today = clock.now().date_naive();
        let st = Arc::clone(&status);
        let progress = move |p: Progress| match p {
            Progress::Downloading { year, done, total } => set_model(
                &st,
                "training",
                format!("downloading METAR history from IEM: {year} ({done}/{total} years)"),
                Some((done, total)),
            ),
            Progress::Training { observations } => set_model(
                &st,
                "training",
                format!("training on {observations} historical reports"),
                None,
            ),
        };
        tracing::info!(station = %plan.station, from = plan.from_year, "training the probability model from IEM history");
        match training::train(&archive, &plan, audit.as_ref(), &progress, &mut shutdown).await {
            Ok(o) => {
                let msg = format!(
                    "model {} trained on {} days ({} → {}), {} samples; restarting to load it",
                    o.model_id,
                    o.days,
                    o.from.map(|d| d.to_string()).unwrap_or_default(),
                    o.to.map(|d| d.to_string()).unwrap_or_default(),
                    o.samples
                );
                tracing::info!(
                    years_downloaded = o.years_downloaded,
                    years_cached = o.years_cached,
                    observations = o.observations,
                    "{msg}"
                );
                set_model(&status, "training", msg.clone(), None);
                with_side(&side, |s| s.alert("info", msg));
                let _ = ready.send(true);
                return;
            }
            Err(_) if *shutdown.borrow() => return,
            Err(e) => {
                let retry_at = clock.now()
                    + Duration::from_std(retry_after).unwrap_or_else(|_| Duration::hours(6));
                let msg = format!(
                    "model training failed: {e:#}; next attempt {} UTC — no weather trades until then",
                    retry_at.format("%Y-%m-%d %H:%M")
                );
                tracing::warn!("{msg}");
                set_model(&status, "failed", msg.clone(), None);
                with_side(&side, |s| s.alert("warning", msg));
                tokio::select! {
                    _ = tokio::time::sleep(retry_after) => {}
                    r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
                }
            }
        }
    }
}

/// Book-keeping around the kernel's outputs.
#[derive(Default)]
struct EngineLoopState {
    record_books: bool,
    last_book_record: HashMap<TokenId, DateTime<Utc>>,
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
                    let last = self.last_book_record.get(&b.book.token).copied();
                    if last.is_none_or(|t| env.available_at - t >= Duration::seconds(5)) {
                        self.last_book_record
                            .insert(b.book.token.clone(), env.available_at);
                        batch.books.push(b.book.clone());
                    }
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
        if batch.is_empty() {
            return;
        }
        match tx.try_send(batch) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                storage_ok.store(false, Ordering::Release);
                with_side(side, |st| {
                    st.alert("critical", "persistence backlog — new positions blocked")
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => storage_ok.store(false, Ordering::Release),
        }
    }
}
