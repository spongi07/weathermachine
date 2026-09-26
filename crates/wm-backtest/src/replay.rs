//! Replay and backtest engine.
//!
//! Events are released strictly in knowledge-time order (`available_at`),
//! with deterministic tie-breaking; the engine clock can never run backwards,
//! and nothing with `available_at > now` is visible. The kernel is exactly
//! the one used live; fills come from the same simulated exchange as paper.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;
use wm_core::event::{EventEnvelope, EventSource, TimerEvent, TimerKind, WeatherMachineEvent};
use wm_core::ids::{ClientOrderId, EventSlug, StrategyId};
use wm_core::market::Side;
use wm_core::units::{Rounding, Usd, notional};
use wm_engine::{Engine, EngineConfig};
use wm_execution::{SimConfig, SimulatedExchange};
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
        (o.env.available_at, o.priority, o.order).cmp(&(self.env.available_at, self.priority, self.order))
    }
}

fn priority(e: &WeatherMachineEvent) -> u8 {
    match e {
        WeatherMachineEvent::ProviderHealthChanged(_) | WeatherMachineEvent::Operator(_) => 0,
        WeatherMachineEvent::MarketSnapshot(_) => 1,
        WeatherMachineEvent::OrderUpdate(_) => 2,
        WeatherMachineEvent::OrderBookUpdate(_) | WeatherMachineEvent::MarketTrade(_) => 3,
        WeatherMachineEvent::WeatherObservation(_) | WeatherMachineEvent::WeatherCorrection(_) | WeatherMachineEvent::ForecastUpdate(_) => 4,
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
        Self { heap: BinaryHeap::new(), counter: 0, seq: 0 }
    }

    fn push(&mut self, env: EventEnvelope) {
        self.counter += 1;
        let p = priority(&env.event);
        self.heap.push(Queued { env, priority: p, order: self.counter });
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
        .map(|_| (0..xs.len()).map(|_| xs[rng.next_below(xs.len() as u64) as usize]).sum::<f64>() / xs.len() as f64)
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    (percentile(&means, 0.025), percentile(&means, 0.975))
}

/// Run a backtest over pre-built input events.
pub fn run_backtest(inputs: Vec<EventEnvelope>, cfg: &BacktestConfig, model: Arc<dyn ProbabilityModel>) -> (BacktestReport, Engine) {
    let mut engine = Engine::new(cfg.engine.clone(), model);
    let mut exchange = SimulatedExchange::new(cfg.sim.clone());
    let mut q = ReplayQueue::new();
    let first = inputs.iter().map(|e| e.available_at).min();
    let last = inputs.iter().map(|e| e.available_at).max();
    for mut e in inputs {
        e.source = EventSource::Replay;
        // Guarantee a settlement check after every market's day (+ grace).
        if let WeatherMachineEvent::MarketSnapshot(m) = &e.event {
            let (_, end) = wm_core::time::local_day_bounds(m.market.local_date, m.market.timezone);
            let at = end + cfg.settle_grace + Duration::seconds(1);
            q.push(EventEnvelope::new(at, EventSource::Replay, WeatherMachineEvent::Timer(TimerEvent { due_at: at, kind: TimerKind::Heartbeat })));
        }
        q.push(e);
    }
    // Heartbeat timers across the whole span.
    if let (Some(a), Some(b)) = (first, last)
        && cfg.heartbeat > Duration::zero()
    {
        let mut t = a;
        while t <= b + cfg.settle_grace {
            q.push(EventEnvelope::new(t, EventSource::Replay, WeatherMachineEvent::Timer(TimerEvent { due_at: t, kind: TimerKind::Heartbeat })));
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
    let mut last_time: Option<DateTime<Utc>> = None;
    let mut daily: BTreeMap<NaiveDate, Usd> = BTreeMap::new();
    let mut strategy_of: BTreeMap<ClientOrderId, StrategyId> = BTreeMap::new();

    while let Some(env) = q.pop() {
        if let Some(t) = last_time {
            debug_assert!(env.available_at >= t, "replay clock went backwards");
        }
        last_time = Some(env.available_at);
        let now = env.available_at;
        report.events += 1;

        // The exchange sees market data first (knowledge-consistent).
        match &env.event {
            WeatherMachineEvent::OrderBookUpdate(b) => {
                for u in exchange.on_book(&b.book, now) {
                    q.push(EventEnvelope::new(now, EventSource::Replay, WeatherMachineEvent::OrderUpdate(u)));
                }
            }
            WeatherMachineEvent::MarketTrade(t) => {
                for u in exchange.on_trade(&t.trade, now) {
                    q.push(EventEnvelope::new(now, EventSource::Replay, WeatherMachineEvent::OrderUpdate(u)));
                }
            }
            WeatherMachineEvent::OrderUpdate(u) => {
                if let Some(f) = &u.fill {
                    report.fills += 1;
                    let strategy = strategy_of.get(&f.client_order_id).cloned().unwrap_or_else(|| StrategyId::from_static("unknown"));
                    let slug = engine.orders().get(&f.client_order_id).map(|o| o.event_slug.clone());
                    let label = engine.orders().get(&f.client_order_id).map(|o| o.bucket.label()).unwrap_or_default();
                    let rounding = if f.side == Side::Buy { Rounding::Up } else { Rounding::Down };
                    report.trades.push(TradeRecord {
                        client_order_id: f.client_order_id.clone(),
                        strategy,
                        event_slug: slug.unwrap_or_else(|| EventSlug::from_static("unknown")),
                        bucket_label: label,
                        side: if f.side == Side::Buy { "BUY".into() } else { "SELL".into() },
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
        for u in exchange.process_due(now) {
            q.push(EventEnvelope::new(now, EventSource::Replay, WeatherMachineEvent::OrderUpdate(u)));
        }

        let out = engine.handle(&env);
        report.decisions += out.decisions.len() as u64;
        report.decision_log.extend(out.decisions.iter().filter(|d| d.strategy.as_str() != "evaluation").cloned());
        report.alerts.extend(out.alerts);
        for a in out.approved {
            report.approvals += 1;
            strategy_of.insert(a.client_order_id().clone(), a.intent().strategy.clone());
            let fees = engine.markets().get(&a.intent().event_slug).map(|m| m.fees).unwrap_or(wm_core::market::FeeSchedule::ZERO);
            for u in exchange.submit(&a, fees, now) {
                q.push(EventEnvelope::new(now, EventSource::Replay, WeatherMachineEvent::OrderUpdate(u)));
            }
            // Make sure latency-delayed orders are processed even without new book events.
            let due = now + Duration::milliseconds(cfg.sim.latency_ms.max(0));
            q.push(EventEnvelope::new(due, EventSource::Replay, WeatherMachineEvent::Timer(TimerEvent { due_at: due, kind: TimerKind::Heartbeat })));
        }

        // Settle finished days using the observed resolution value.
        for slug in engine.settleable_markets(cfg.settle_grace) {
            let Some(final_value) = engine.final_value(&slug) else { continue };
            let date = engine.markets().get(&slug).map(|m| m.local_date);
            let pnl = engine.settle(&slug, final_value);
            report.settled_markets += 1;
            if let Some(d) = date {
                *daily.entry(d).or_insert(Usd::ZERO) += pnl;
            }
        }
    }

    report.realized_pnl = engine.realized_pnl_total();
    // Attribute PnL per strategy from the trade ledger (cost/proceeds) plus settlement payouts.
    for p in engine.positions().iter() {
        let strategy = report
            .trades
            .iter()
            .find(|t| engine.orders().get(&t.client_order_id).is_some_and(|o| o.token == p.instrument.token))
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
        let t = DateTime::parse_from_rfc3339("2026-07-01T12:00:00Z").unwrap().with_timezone(&Utc);
        let mut q = ReplayQueue::new();
        q.push(EventEnvelope::new(t + Duration::seconds(5), EventSource::Replay, WeatherMachineEvent::Timer(TimerEvent { due_at: t, kind: TimerKind::Heartbeat })));
        q.push(EventEnvelope::new(t, EventSource::Replay, WeatherMachineEvent::Timer(TimerEvent { due_at: t, kind: TimerKind::Heartbeat })));
        q.push(EventEnvelope::new(t, EventSource::Replay, WeatherMachineEvent::Operator(wm_core::event::OperatorCommand::KillSwitch { engaged: false, reason: String::new() })));
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
