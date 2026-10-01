//! Replay and backtest engine.
//!
//! Events are released strictly in knowledge-time order (`available_at`),
//! with deterministic tie-breaking; the engine clock can never run backwards,
//! and nothing with `available_at > now` is visible. The kernel is exactly
//! the one used live; fills come from the same simulated exchange as paper.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;
use wm_core::event::{EventEnvelope, EventSource, TimerEvent, TimerKind, WeatherMachineEvent};
use wm_core::ids::{ClientOrderId, EventSlug, StationId, StrategyId};
use wm_core::market::Side;
use wm_core::trading::DecisionRecord;
use wm_core::units::{Rounding, Usd, notional};
use wm_engine::{Engine, EngineConfig, RestoreState, RestoreSummary, StationHint};
use wm_execution::{SimConfig, SimulatedExchange};
use wm_risk::ApprovedIntent;
use wm_strategy::ProbabilityModel;

/// How realistic the market data is (always reported with results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    /// Recorded full order books (our own recorder).
    TrueOrderBook,
    /// Books reconstructed from public trades only.
    TradesOnly,
    /// Prices only (e.g. `/prices-history`): approximate.
    PriceOnly,
    /// Synthetic market data: plumbing test, not evidence.
    Synthetic,
}

/// Backtest configuration.
#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub engine: EngineConfig,
    pub sim: SimConfig,
    pub fidelity: Fidelity,
    /// Settle a market this long after its local day ends.
    pub settle_grace: Duration,
    /// Heartbeat timer cadence (re-evaluation without new events).
    pub heartbeat: Duration,
}

/// One completed or open position lifecycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub client_order_id: ClientOrderId,
    pub strategy: StrategyId,
    pub event_slug: EventSlug,
    pub bucket_label: String,
    pub side: String,
    pub price: String,
    pub shares: String,
    pub fee: Usd,
    pub cost_or_proceeds: Usd,
    pub at: DateTime<Utc>,
}

/// Backtest results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestReport {
    pub fidelity: Fidelity,
    pub events: u64,
    pub decisions: u64,
    pub approvals: u64,
    pub fills: u64,
    pub trades: Vec<TradeRecord>,
    pub settled_markets: u64,
    pub realized_pnl: Usd,
    pub daily_pnl: Vec<(NaiveDate, Usd)>,
    pub winning_days: u64,
    pub losing_days: u64,
    pub max_drawdown: Usd,
    /// 95 % bootstrap interval of mean daily PnL (seeded, by day).
    pub mean_daily_pnl_ci: (f64, f64),
    pub pnl_by_strategy: BTreeMap<String, Usd>,
    pub alerts: Vec<String>,
    /// Every trade decision (approved or rejected), excluding per-observation
    /// evaluation summaries. Used for audits and look-ahead tests.
    pub decision_log: Vec<wm_core::trading::DecisionRecord>,
}

#[derive(Debug)]
struct Queued {
    env: EventEnvelope,
    priority: u8,
    order: u64,
}

impl PartialEq for Queued {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Queued {
    // Min-heap on (available_at, priority, insertion order).
    fn cmp(&self, o: &Self) -> Ordering {
        (o.env.available_at, o.priority, o.order).cmp(&(
            self.env.available_at,
            self.priority,
            self.order,
        ))
    }
}

fn priority(e: &WeatherMachineEvent) -> u8 {
    match e {
        WeatherMachineEvent::ProviderHealthChanged(_) | WeatherMachineEvent::Operator(_) => 0,
        WeatherMachineEvent::MarketSnapshot(_) => 1,
        WeatherMachineEvent::OrderUpdate(_) => 2,
        WeatherMachineEvent::OrderBookUpdate(_)
        | WeatherMachineEvent::MarketTrade(_)
        | WeatherMachineEvent::MarketStreamHeartbeat(_) => 3,
        WeatherMachineEvent::WeatherObservation(_)
        | WeatherMachineEvent::WeatherCorrection(_)
        | WeatherMachineEvent::ForecastUpdate(_) => 4,
        WeatherMachineEvent::Timer(_) => 5,
    }
}

/// Knowledge-time ordered queue.
struct ReplayQueue {
    heap: BinaryHeap<Queued>,
    counter: u64,
    seq: u64,
}

impl ReplayQueue {
    fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            counter: 0,
            seq: 0,
        }
    }

    fn push(&mut self, env: EventEnvelope) {
        self.counter += 1;
        let p = priority(&env.event);
        self.heap.push(Queued {
            env,
            priority: p,
            order: self.counter,
        });
    }

    fn pop(&mut self) -> Option<EventEnvelope> {
        let mut q = self.heap.pop()?;
        self.seq += 1;
        q.env.seq = self.seq;
        Some(q.env)
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Seeded bootstrap of the mean of `xs`.
pub fn bootstrap_mean_ci(xs: &[f64], iterations: usize, seed: u64) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = wm_core::rng::SplitMix64::new(seed);
    let mut means: Vec<f64> = (0..iterations)
        .map(|_| {
            (0..xs.len())
                .map(|_| xs[rng.next_below(xs.len() as u64) as usize])
                .sum::<f64>()
                / xs.len() as f64
        })
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    (percentile(&means, 0.025), percentile(&means, 0.975))
}

/// A settled market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settlement {
    pub event_slug: EventSlug,
    pub local_date: NaiveDate,
    pub final_value: i32,
    pub pnl: Usd,
    pub at: DateTime<Utc>,
}

/// Everything one [`SimulationSession::run_until`] call produced.
#[derive(Debug, Default)]
pub struct SessionOutput {
    /// Engine inputs processed (only when event capture is on).
    pub processed: Vec<EventEnvelope>,
    /// Number of engine inputs processed (always counted).
    pub events: u64,
    pub decisions: Vec<DecisionRecord>,
    pub approved: Vec<ApprovedIntent>,
    pub alerts: Vec<String>,
    pub hints: Vec<(StationId, StationHint)>,
    pub trades: Vec<TradeRecord>,
    pub settlements: Vec<Settlement>,
}

impl SessionOutput {
    pub fn is_empty(&self) -> bool {
        self.events == 0 && self.settlements.is_empty()
    }
}

/// Engine + simulated venue + knowledge-time queue: the single loop shared by
/// backtests, demo mode and paper trading. Live paper runs push events as they
/// arrive and call [`run_until`](Self::run_until) with the wall clock; replays
/// push everything up front and drain the queue.
pub struct SimulationSession {
    engine: Engine,
    exchange: SimulatedExchange,
    queue: ReplayQueue,
    latency: Duration,
    settle_grace: Duration,
    strategy_of: BTreeMap<ClientOrderId, StrategyId>,
    settlement_timers: BTreeSet<EventSlug>,
    capture_events: bool,
    last_time: Option<DateTime<Utc>>,
}

impl SimulationSession {
    pub fn new(
        engine: EngineConfig,
        sim: SimConfig,
        settle_grace: Duration,
        model: Arc<dyn ProbabilityModel>,
    ) -> Self {
        let latency = Duration::milliseconds(sim.latency_ms.max(0));
        Self {
            engine: Engine::new(engine, model),
            exchange: SimulatedExchange::new(sim),
            queue: ReplayQueue::new(),
            latency,
            settle_grace,
            strategy_of: BTreeMap::new(),
            settlement_timers: BTreeSet::new(),
            capture_events: false,
            last_time: None,
        }
    }

    /// Keep processed envelopes in [`SessionOutput::processed`] (event journal).
    pub fn with_event_capture(mut self, on: bool) -> Self {
        self.capture_events = on;
        self
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    pub fn into_engine(self) -> Engine {
        self.engine
    }

    /// Queued (not yet processed) events.
    pub fn pending(&self) -> usize {
        self.queue.heap.len()
    }

    /// Knowledge time of the next queued event.
    pub fn next_due(&self) -> Option<DateTime<Utc>> {
        self.queue.heap.peek().map(|q| q.env.available_at)
    }

    /// Knowledge time of the last processed event.
    pub fn last_time(&self) -> Option<DateTime<Utc>> {
        self.last_time
    }

    /// Enqueue an input. A market snapshot also schedules one settlement check
    /// after its local day (+ grace), so every market settles without relying
    /// on later traffic.
    pub fn push(&mut self, env: EventEnvelope) {
        if let WeatherMachineEvent::MarketSnapshot(m) = &env.event
            && self.settlement_timers.insert(m.market.event_slug.clone())
        {
            let (_, end) = wm_core::time::local_day_bounds(m.market.local_date, m.market.timezone);
            let at = end + self.settle_grace + Duration::seconds(1);
            self.queue.push(EventEnvelope::new(
                at,
                env.source,
                WeatherMachineEvent::Timer(TimerEvent {
                    due_at: at,
                    kind: TimerKind::Heartbeat,
                }),
            ));
        }
        self.queue.push(env);
    }

    /// Rebuild earlier runs' paper book after a restart ([`Engine::restore`])
    /// and schedule the restored markets' settlement checks, so a finished
    /// market settles at the next step even without traffic. The checks are
    /// never queued before `now` or the last processed event.
    pub fn restore(&mut self, state: &RestoreState, now: DateTime<Utc>) -> RestoreSummary {
        let summary = self.engine.restore(state, now);
        let not_before = self.last_time.map_or(now, |t| t.max(now));
        for m in &state.markets {
            if self.settlement_timers.insert(m.event_slug.clone()) {
                let (_, end) = wm_core::time::local_day_bounds(m.local_date, m.timezone);
                let at = (end + self.settle_grace + Duration::seconds(1)).max(not_before);
                self.queue.push(EventEnvelope::new(
                    at,
                    EventSource::Replay,
                    WeatherMachineEvent::Timer(TimerEvent {
                        due_at: at,
                        kind: TimerKind::Heartbeat,
                    }),
                ));
            }
        }
        summary
    }

    /// Process every queued event with `available_at ≤ until`.
    pub fn run_until(&mut self, until: DateTime<Utc>) -> SessionOutput {
        let mut out = SessionOutput::default();
        while self.next_due().is_some_and(|t| t <= until) {
            let Some(env) = self.queue.pop() else { break };
            self.step(env, &mut out);
        }
        out
    }

    /// Drain the queue completely (replays).
    pub fn run_all(&mut self) -> SessionOutput {
        let mut out = SessionOutput::default();
        while let Some(env) = self.queue.pop() {
            self.step(env, &mut out);
        }
        out
    }

    fn step(&mut self, env: EventEnvelope, out: &mut SessionOutput) {
        if let Some(t) = self.last_time {
            debug_assert!(env.available_at >= t, "replay clock went backwards");
        }
        self.last_time = Some(env.available_at);
        let now = env.available_at;
        let source = env.source;
        out.events += 1;

        // The exchange sees market data first (knowledge-consistent).
        match &env.event {
            WeatherMachineEvent::OrderBookUpdate(b) => {
                for u in self.exchange.on_book(&b.book, now) {
                    self.queue.push(EventEnvelope::new(
                        now,
                        source,
                        WeatherMachineEvent::OrderUpdate(u),
                    ));
                }
            }
            WeatherMachineEvent::MarketTrade(t) => {
                for u in self.exchange.on_trade(&t.trade, now) {
                    self.queue.push(EventEnvelope::new(
                        now,
                        source,
                        WeatherMachineEvent::OrderUpdate(u),
                    ));
                }
            }
            WeatherMachineEvent::OrderUpdate(u) => {
                if let Some(f) = &u.fill {
                    let strategy = self
                        .strategy_of
                        .get(&f.client_order_id)
                        .cloned()
                        .unwrap_or_else(|| StrategyId::from_static("unknown"));
                    let order = self.engine.orders().get(&f.client_order_id);
                    let slug = order.map(|o| o.event_slug.clone());
                    let label = order.map(|o| o.bucket.label()).unwrap_or_default();
                    let rounding = if f.side == Side::Buy {
                        Rounding::Up
                    } else {
                        Rounding::Down
                    };
                    out.trades.push(TradeRecord {
                        client_order_id: f.client_order_id.clone(),
                        strategy,
                        event_slug: slug.unwrap_or_else(|| EventSlug::from_static("unknown")),
                        bucket_label: label,
                        side: if f.side == Side::Buy {
                            "BUY".into()
                        } else {
                            "SELL".into()
                        },
                        price: f.price.to_string(),
                        shares: f.shares.to_string(),
                        fee: f.fee,
                        cost_or_proceeds: notional(f.price, f.shares, rounding),
                        at: f.ts,
                    });
                }
            }
            _ => {}
        }
        for u in self.exchange.process_due(now) {
            self.queue.push(EventEnvelope::new(
                now,
                source,
                WeatherMachineEvent::OrderUpdate(u),
            ));
        }

        let result = self.engine.handle(&env);
        if self.capture_events {
            out.processed.push(env);
        }
        for a in &result.approved {
            self.strategy_of
                .insert(a.client_order_id().clone(), a.intent().strategy.clone());
            let fees = self
                .engine
                .markets()
                .get(&a.intent().event_slug)
                .map(|m| m.fees)
                .unwrap_or(wm_core::market::FeeSchedule::ZERO);
            for u in self.exchange.submit(a, fees, now) {
                self.queue.push(EventEnvelope::new(
                    now,
                    source,
                    WeatherMachineEvent::OrderUpdate(u),
                ));
            }
            // Latency-delayed orders are matched even without new book events.
            let due = now + self.latency;
            self.queue.push(EventEnvelope::new(
                due,
                source,
                WeatherMachineEvent::Timer(TimerEvent {
                    due_at: due,
                    kind: TimerKind::Heartbeat,
                }),
            ));
        }
        out.decisions.extend(result.decisions);
        out.approved.extend(result.approved);
        out.alerts.extend(result.alerts);
        out.hints.extend(result.hints);

        // Settle finished days using the observed resolution value.
        for slug in self.engine.settleable_markets(self.settle_grace) {
            let Some(final_value) = self.engine.final_value(&slug) else {
                continue;
            };
            let Some(date) = self.engine.markets().get(&slug).map(|m| m.local_date) else {
                continue;
            };
            let pnl = self.engine.settle(&slug, final_value);
            out.settlements.push(Settlement {
                event_slug: slug,
                local_date: date,
                final_value,
                pnl,
                at: now,
            });
        }
    }
}

/// Run a backtest over pre-built input events.
pub fn run_backtest(
    inputs: Vec<EventEnvelope>,
    cfg: &BacktestConfig,
    model: Arc<dyn ProbabilityModel>,
) -> (BacktestReport, Engine) {
    let mut session =
        SimulationSession::new(cfg.engine.clone(), cfg.sim.clone(), cfg.settle_grace, model);
    let first = inputs.iter().map(|e| e.available_at).min();
    let last = inputs.iter().map(|e| e.available_at).max();
    for mut e in inputs {
        e.source = EventSource::Replay;
        session.push(e);
    }
    // Heartbeat timers across the whole span.
    if let (Some(a), Some(b)) = (first, last)
        && cfg.heartbeat > Duration::zero()
    {
        let mut t = a;
        while t <= b + cfg.settle_grace {
            session.push(EventEnvelope::new(
                t,
                EventSource::Replay,
                WeatherMachineEvent::Timer(TimerEvent {
                    due_at: t,
                    kind: TimerKind::Heartbeat,
                }),
            ));
            t += cfg.heartbeat;
        }
    }

    let mut report = BacktestReport {
        fidelity: cfg.fidelity,
        events: 0,
        decisions: 0,
        approvals: 0,
        fills: 0,
        trades: Vec::new(),
        settled_markets: 0,
        realized_pnl: Usd::ZERO,
        daily_pnl: Vec::new(),
        winning_days: 0,
        losing_days: 0,
        max_drawdown: Usd::ZERO,
        mean_daily_pnl_ci: (0.0, 0.0),
        pnl_by_strategy: BTreeMap::new(),
        alerts: Vec::new(),
        decision_log: Vec::new(),
    };
    let mut daily: BTreeMap<NaiveDate, Usd> = BTreeMap::new();
    // Drain day by day so memory stays bounded on long replays.
    while let Some(next) = session.next_due() {
        let out = session.run_until(next + Duration::days(1));
        report.events += out.events;
        report.decisions += out.decisions.len() as u64;
        report.decision_log.extend(
            out.decisions
                .into_iter()
                .filter(|d| d.strategy.as_str() != "evaluation"),
        );
        report.approvals += out.approved.len() as u64;
        report.fills += out.trades.len() as u64;
        report.trades.extend(out.trades);
        report.alerts.extend(out.alerts);
        for s in out.settlements {
            report.settled_markets += 1;
            *daily.entry(s.local_date).or_insert(Usd::ZERO) += s.pnl;
        }
    }
    let engine = session.into_engine();

    report.realized_pnl = engine.realized_pnl_total();
    // Attribute PnL per strategy from the trade ledger (cost/proceeds) plus settlement payouts.
    for p in engine.positions().iter() {
        let strategy = report
            .trades
            .iter()
            .find(|t| {
                engine
                    .orders()
                    .get(&t.client_order_id)
                    .is_some_and(|o| o.token == p.instrument.token)
            })
            .map(|t| t.strategy.to_string())
            .unwrap_or_else(|| "unknown".into());
        *report.pnl_by_strategy.entry(strategy).or_insert(Usd::ZERO) += p.realized_pnl;
    }
    let mut equity = Usd::ZERO;
    let mut peak = Usd::ZERO;
    for (d, pnl) in &daily {
        equity += *pnl;
        peak = peak.max(equity);
        report.max_drawdown = report.max_drawdown.max(peak - equity);
        if *pnl > Usd::ZERO {
            report.winning_days += 1;
        } else if *pnl < Usd::ZERO {
            report.losing_days += 1;
        }
        report.daily_pnl.push((*d, *pnl));
    }
    let xs: Vec<f64> = report.daily_pnl.iter().map(|(_, p)| p.as_f64()).collect();
    report.mean_daily_pnl_ci = bootstrap_mean_ci(&xs, 2000, 17);
    (report, engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_orders_by_knowledge_time_then_priority() {
        let t = DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut q = ReplayQueue::new();
        q.push(EventEnvelope::new(
            t + Duration::seconds(5),
            EventSource::Replay,
            WeatherMachineEvent::Timer(TimerEvent {
                due_at: t,
                kind: TimerKind::Heartbeat,
            }),
        ));
        q.push(EventEnvelope::new(
            t,
            EventSource::Replay,
            WeatherMachineEvent::Timer(TimerEvent {
                due_at: t,
                kind: TimerKind::Heartbeat,
            }),
        ));
        q.push(EventEnvelope::new(
            t,
            EventSource::Replay,
            WeatherMachineEvent::Operator(wm_core::event::OperatorCommand::KillSwitch {
                engaged: false,
                reason: String::new(),
            }),
        ));
        let a = q.pop().unwrap();
        let b = q.pop().unwrap();
        let c = q.pop().unwrap();
        assert!(matches!(a.event, WeatherMachineEvent::Operator(_)));
        assert!(matches!(b.event, WeatherMachineEvent::Timer(_)) && b.available_at == t);
        assert_eq!(c.available_at, t + Duration::seconds(5));
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
    }

    #[test]
    fn bootstrap_is_seeded_and_brackets_mean() {
        let xs = [1.0, 2.0, 3.0, 4.0, 5.0, -1.0, 0.5];
        let a = bootstrap_mean_ci(&xs, 1000, 3);
        assert_eq!(a, bootstrap_mean_ci(&xs, 1000, 3));
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!(a.0 <= mean && a.1 >= mean);
    }
}
