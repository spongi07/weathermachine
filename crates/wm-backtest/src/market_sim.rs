//! Strategies A, B and E replayed at the prices the market actually traded
//! (`research market`), and the report-by-report replay of chosen days.
//!
//! A decision is taken at every report (plus the knowledge delay) with the
//! models as trained on the days before, exactly like the probability
//! scoring. Prices come from executed trades: the latest taker buy of YES is
//! the YES ask, the latest taker sell of YES the YES bid (so the NO ask is
//! one minus it). Depth is not known: every simulated order is assumed to
//! fill at that price plus the slippage allowance, which is paid.
//!
//! One *variant* is a model structure × strategy × confirmation window ×
//! ask range; each takes at most one trade per day and bucket (the first
//! decision that passes every gate). The live rule is one of them and is
//! marked. Many variants are tried, so the best one overstates what to expect.
//!
//! Strategy E's book condition is replayed by a stand-in, since only trades
//! are archived: over the lookback takers bought enough YES at or below the
//! price cap, more than they sold, and the ask proxy did not fall
//! ([`BookConfirmedSim`]).

use crate::forecast_eval::ratio_ci;
use crate::market_makers::{MakerRule, MakerSim};
use crate::market_peak::PeakSlotSim;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use wm_core::market::TemperatureBucket;
use wm_strategy::{IncrementDistribution, PeakFeatures, log_pool};

/// Names of the two model structures, in the order each decision holds
/// their distributions.
pub const STRUCTURES: [&str; 2] = ["current", "candidate"];

/// Replay settings (research only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarketSimConfig {
    /// Confirmation windows replayed (minutes since the last report at the
    /// high, as the live strategies count).
    pub windows: Vec<u32>,
    /// Ask ranges replayed (YES for A, NO for B).
    pub ranges: Vec<(f64, f64)>,
    /// The live rule, marked in the report.
    pub live_window: u32,
    pub live_range: (f64, f64),
    pub min_edge: f64,
    pub slippage: f64,
    /// Widest ask − bid at which the midpoint pools with the model.
    pub max_market_spread: f64,
    /// Strategy B: buckets holding the high + these distances.
    pub no_distances: Vec<i32>,
    /// Stake per simulated trade (USD at the ask).
    pub stake_usd: f64,
    /// Strategy E.
    pub e: BookConfirmedSim,
    /// Strategy F (and its variants).
    pub f: PeakSlotSim,
    /// The live rules replayed as limit orders.
    pub maker: MakerSim,
}

impl Default for MarketSimConfig {
    fn default() -> Self {
        Self {
            windows: vec![0, 30, 60],
            ranges: vec![(0.90, 0.99), (0.70, 0.99)],
            live_window: 60,
            live_range: (0.90, 0.99),
            min_edge: 0.01,
            slippage: 0.005,
            max_market_spread: 0.10,
            no_distances: vec![1, 2, 3],
            stake_usd: 10.0,
            e: BookConfirmedSim::default(),
            f: PeakSlotSim::default(),
            maker: MakerSim::default(),
        }
    }
}

/// Strategy E replayed (the live `[strategies.book_confirmed]` settings).
///
/// The order book is not archived, so "the book is shrinking" is replayed by
/// a trade-flow stand-in: over the lookback, takers bought at least
/// `min_bought_shares` of YES at or below `max_price`, more than they sold,
/// and the ask proxy (latest taker buy) did not fall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BookConfirmedSim {
    /// E is enabled live (its rule is marked **live**).
    pub live: bool,
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    pub min_minutes_at_high: i64,
    pub min_drop_tenths: i32,
    pub lookback_minutes: i64,
    /// Shares that must have left the book live: `min_depth_shares ×
    /// min_depth_shrink`.
    pub min_bought_shares: f64,
    pub min_price: f64,
    pub max_price: f64,
    /// The live model veto (0 = none).
    pub min_model_p: f64,
    /// The model veto of the third replayed variant.
    pub veto_model_p: f64,
}

impl BookConfirmedSim {
    /// The replay of the live strategy's settings.
    pub fn from_live(c: &wm_strategy::BookConfirmedConfig) -> Self {
        Self {
            live: c.enabled,
            start_local_minute: c.start_local_minute,
            end_local_minute: c.end_local_minute,
            min_minutes_at_high: c.min_minutes_at_high,
            min_drop_tenths: c.min_drop_tenths,
            lookback_minutes: c.lookback_minutes,
            min_bought_shares: c.min_depth_shares * c.min_depth_shrink,
            min_price: c.min_price.as_f64(),
            max_price: c.max_price.as_f64(),
            min_model_p: c.min_model_p,
            veto_model_p: 0.90,
        }
    }

    /// The table's confirmation column: minutes since the first report at
    /// the high.
    pub(crate) fn window(&self) -> u32 {
        u32::try_from(self.min_minutes_at_high.clamp(0, 1_440)).unwrap_or(0)
    }

    pub(crate) fn range(&self) -> String {
        range_label((self.min_price, self.max_price))
    }

    /// E's clock and temperature conditions at a report: inside the local
    /// window, the high first reached long enough ago and the report far
    /// enough below it.
    pub(crate) fn conditions_hold(&self, f: &PeakFeatures) -> bool {
        (self.start_local_minute..self.end_local_minute).contains(&f.local_minute_now)
            && f.minutes_since_first_high >= self.min_minutes_at_high
            && f.drop_tenths >= self.min_drop_tenths
    }
}

impl Default for BookConfirmedSim {
    fn default() -> Self {
        Self::from_live(&wm_strategy::BookConfirmedConfig::default())
    }
}

/// What the market's trades said about one bucket at a decision.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Quote {
    /// Midpoint of the latest taker buy and sell of YES (or the one seen).
    pub(crate) mid: Option<f64>,
    /// Latest taker buy of YES: the YES ask proxy.
    pub(crate) yes_ask: Option<f64>,
    /// Latest taker sell of YES: the YES bid proxy (NO ask = 1 − bid).
    pub(crate) yes_bid: Option<f64>,
}

impl Quote {
    /// The midpoint as the strategies pool it: both sides seen and close.
    pub(crate) fn pool_mid(&self, max_spread: f64) -> Option<f64> {
        match (self.yes_ask, self.yes_bid) {
            (Some(a), Some(b)) if (a - b).abs() <= max_spread + 1e-12 => Some((a + b) / 2.0),
            _ => None,
        }
    }
}

/// Taker flow on one bucket's YES over strategy E's lookback before a
/// decision: the stand-in for a shrinking book.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Flow {
    /// YES shares takers bought (lifting offers) at or below E's price cap.
    pub(crate) bought: f64,
    /// YES shares takers sold (hitting bids).
    pub(crate) sold: f64,
    /// The ask proxy when the lookback began (else its first taker buy).
    pub(crate) ask_then: Option<f64>,
}

impl Flow {
    /// Buyers lifted the offers: enough bought, more than sold, and the ask
    /// not lower than when the lookback began.
    pub(crate) fn lifting(&self, ask_now: Option<f64>, min_bought: f64) -> bool {
        self.bought >= min_bought
            && self.bought > self.sold
            && matches!((self.ask_then, ask_now), (Some(a), Some(b)) if b >= a - 1e-12)
    }
}

/// One decision of a market day: the features at a report and each
/// structure's distribution (trained on earlier days), with every bucket's
/// quote and taker flow at the decision time.
#[derive(Debug, Clone)]
pub(crate) struct Decision {
    /// Observation time of the report.
    pub(crate) at: DateTime<Utc>,
    /// When the bot knows the report (observation + knowledge delay): the
    /// quotes and flows are taken then.
    pub(crate) knowledge: DateTime<Utc>,
    pub(crate) f: PeakFeatures,
    /// Current and candidate structure ([`STRUCTURES`]).
    pub(crate) dists: [Option<IncrementDistribution>; 2],
    pub(crate) quotes: Vec<Quote>,
    pub(crate) flows: Vec<Flow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// YES on the bucket that holds the high.
    A,
    /// NO on the buckets above it.
    B,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Kind::A => "A",
            Kind::B => "B",
        }
    }
}

/// Strategy E's replayed variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EVariant {
    /// As configured, the book by its trade-flow stand-in.
    Live,
    /// Without the book condition.
    NoBook,
    /// With the model's probability ≥ `veto_model_p` as well.
    Model,
}

impl EVariant {
    const ALL: [EVariant; 3] = [EVariant::Live, EVariant::NoBook, EVariant::Model];

    fn label(self) -> &'static str {
        match self {
            EVariant::Live => "E",
            EVariant::NoBook => "E w/o book",
            EVariant::Model => "E + model",
        }
    }
}

/// One simulated trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimTrade {
    pub date: NaiveDate,
    /// Local time of the report the decision followed.
    pub report: String,
    pub structure: String,
    pub strategy: String,
    pub window: u32,
    pub range: String,
    pub bucket: String,
    /// `YES` or `NO`.
    pub side: String,
    /// Ask proxy paid (before slippage).
    pub price: f64,
    pub p_model: f64,
    /// After pooling with the market (never above the model).
    pub p_used: f64,
    pub won: bool,
    /// At the configured stake, after the taker fee and slippage.
    pub pnl_usd: f64,
    /// The bucket that resolved YES.
    #[serde(default)]
    pub resolved: String,
    /// Local time of the fill when it came after the knowledge time (a
    /// resting order, or F buying from the tape between reports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filled: Option<String>,
}

/// Results of one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyRow {
    pub structure: String,
    pub strategy: String,
    pub window: u32,
    pub range: String,
    /// The configured live rule (window and range).
    pub live: bool,
    pub trades: u64,
    pub wins: u64,
    pub days: u64,
    pub mean_price: f64,
    pub pnl_per_trade: f64,
    /// 95 % day-block bootstrap interval of the P&L per trade.
    pub ci_low: f64,
    pub ci_high: f64,
    pub total_usd: f64,
}

/// One report of a replayed day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineRow {
    /// Local time of the report (decisions follow after the delay).
    pub report: String,
    pub temp_c: f64,
    pub high: i32,
    /// Minutes since the last / the first report at the high.
    pub since_last_touch: i64,
    pub since_first_reach: i64,
    pub rise_c: Option<f64>,
    pub headroom_c: Option<f64>,
    /// Probability that the final high stays in the bucket holding the high.
    pub p_high_current: Option<f64>,
    pub p_high_candidate: Option<f64>,
    pub cell_current: Option<String>,
    pub cell_candidate: Option<String>,
    /// That bucket's market midpoint and YES ask.
    pub market_high: Option<f64>,
    pub yes_ask_high: Option<f64>,
    /// That bucket's YES shares taker-bought (at or below E's cap) and
    /// taker-sold over E's lookback: the stand-in for its book.
    #[serde(default)]
    pub bought_high: Option<f64>,
    #[serde(default)]
    pub sold_high: Option<f64>,
    /// Trades the replayed variants took at this report.
    pub trades: Vec<String>,
}

/// A replayed day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayTimeline {
    pub date: NaiveDate,
    pub winner: String,
    /// Strategy E's lookback (minutes) of the flow columns.
    #[serde(default)]
    pub flow_minutes: i64,
    pub rows: Vec<TimelineRow>,
}

pub(crate) fn local_hm(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

/// Minutes after local midnight as `HH:MM`.
pub(crate) fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

pub(crate) fn range_label((lo, hi): (f64, f64)) -> String {
    format!("{lo:.2}–{hi:.2}")
}

/// Buckets a strategy trades at a high.
pub(crate) fn buckets_for(
    kind: Kind,
    buckets: &[TemperatureBucket],
    high: i32,
    distances: &[i32],
) -> Vec<usize> {
    match kind {
        Kind::A => buckets
            .iter()
            .position(|b| b.contains(high))
            .into_iter()
            .collect(),
        Kind::B => {
            let mut out: Vec<usize> = Vec::new();
            for k in distances {
                if let Some(i) = buckets.iter().position(|b| b.contains(high + k))
                    && !buckets[i].contains(high)
                    && !out.contains(&i)
                {
                    out.push(i);
                }
            }
            out
        }
    }
}

/// Every variant's trades on one market day.
#[allow(clippy::too_many_arguments)]
pub(crate) fn simulate_day(
    date: NaiveDate,
    buckets: &[TemperatureBucket],
    labels: &[String],
    winner: usize,
    decisions: &[Decision],
    sim: &MarketSimConfig,
    fee_rate: f64,
    market_weight: f64,
    min_support: u32,
    tz: Tz,
) -> Vec<SimTrade> {
    let fee = |p: f64| fee_rate * p * (1.0 - p);
    let mut out = Vec::new();
    for (s, structure) in STRUCTURES.iter().enumerate() {
        for kind in [Kind::A, Kind::B] {
            for &window in &sim.windows {
                for &range in &sim.ranges {
                    let mut done: HashSet<usize> = HashSet::new();
                    for d in decisions {
                        let Some(dist) = &d.dists[s] else { continue };
                        if dist.support < min_support || d.f.minutes_since_high < i64::from(window)
                        {
                            continue;
                        }
                        let high = d.f.high_whole;
                        for i in buckets_for(kind, buckets, high, &sim.no_distances) {
                            if done.contains(&i) {
                                continue;
                            }
                            let q = &d.quotes[i];
                            let mid = q.pool_mid(sim.max_market_spread);
                            let (price, p_model, market) = match kind {
                                Kind::A => {
                                    (q.yes_ask, dist.p_in_bucket_lower(high, &buckets[i]), mid)
                                }
                                Kind::B => (
                                    q.yes_bid.map(|b| 1.0 - b),
                                    1.0 - dist.p_in_bucket_upper(high, &buckets[i]),
                                    mid.map(|m| 1.0 - m),
                                ),
                            };
                            let Some(price) = price else { continue };
                            if price < range.0 - 1e-12 || price > range.1 + 1e-12 {
                                continue;
                            }
                            // Like live: with weight on the market, no trade
                            // unless the market can check the model.
                            if market_weight > 0.0 && market.is_none() {
                                continue;
                            }
                            let p_used = log_pool(p_model, market, market_weight).min(p_model);
                            if p_used - price - fee(price) - sim.slippage < sim.min_edge {
                                continue;
                            }
                            done.insert(i);
                            let won = match kind {
                                Kind::A => i == winner,
                                Kind::B => i != winner,
                            };
                            let per_share =
                                f64::from(u8::from(won)) - price - fee(price) - sim.slippage;
                            out.push(SimTrade {
                                date,
                                report: local_hm(d.at, tz),
                                structure: (*structure).to_owned(),
                                strategy: kind.label().to_owned(),
                                window,
                                range: range_label(range),
                                bucket: labels[i].clone(),
                                side: if kind == Kind::A { "YES" } else { "NO" }.to_owned(),
                                price,
                                p_model,
                                p_used,
                                won,
                                pnl_usd: sim.stake_usd / price * per_share,
                                resolved: labels[winner].clone(),
                                filled: None,
                            });
                        }
                    }
                }
            }
        }
        let e = &sim.e;
        for variant in EVariant::ALL {
            let mut done: HashSet<usize> = HashSet::new();
            for d in decisions {
                // Like live: a model is required, not a model edge.
                let Some(dist) = &d.dists[s] else { continue };
                let f = &d.f;
                if !e.conditions_hold(f) {
                    continue;
                }
                let high = f.high_whole;
                let Some(i) = buckets.iter().position(|b| b.contains(high)) else {
                    continue;
                };
                let q = &d.quotes[i];
                let Some(price) = q.yes_ask else { continue };
                if done.contains(&i) || price < e.min_price - 1e-12 || price > e.max_price + 1e-12 {
                    continue;
                }
                if variant != EVariant::NoBook
                    && !d
                        .flows
                        .get(i)
                        .is_some_and(|fl| fl.lifting(q.yes_ask, e.min_bought_shares))
                {
                    continue;
                }
                let p_model = dist.p_in_bucket_lower(high, &buckets[i]);
                let veto = match variant {
                    EVariant::Model => e.min_model_p.max(e.veto_model_p),
                    EVariant::Live | EVariant::NoBook => e.min_model_p,
                };
                if p_model < veto {
                    continue;
                }
                done.insert(i);
                let won = i == winner;
                let per_share = f64::from(u8::from(won)) - price - fee(price) - sim.slippage;
                out.push(SimTrade {
                    date,
                    report: local_hm(d.at, tz),
                    structure: (*structure).to_owned(),
                    strategy: variant.label().to_owned(),
                    window: e.window(),
                    range: e.range(),
                    bucket: labels[i].clone(),
                    side: "YES".to_owned(),
                    price,
                    p_model,
                    p_used: log_pool(p_model, q.pool_mid(sim.max_market_spread), market_weight)
                        .min(p_model),
                    won,
                    pnl_usd: sim.stake_usd / price * per_share,
                    resolved: labels[winner].clone(),
                    filled: None,
                });
            }
        }
    }
    out
}

/// Results per variant, the live rule first within each structure.
pub(crate) fn strategy_rows(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    iterations: usize,
    seed: u64,
) -> Vec<StrategyRow> {
    let mut rows = Vec::new();
    for structure in STRUCTURES {
        for kind in [Kind::A, Kind::B] {
            for &window in &sim.windows {
                for &range in &sim.ranges {
                    let live = window == sim.live_window
                        && (range.0 - sim.live_range.0).abs() < 1e-9
                        && (range.1 - sim.live_range.1).abs() < 1e-9;
                    let key = (structure, kind.label(), window, range_label(range));
                    rows.push(row(trades, key, live, iterations, seed));
                }
            }
        }
        for variant in EVariant::ALL {
            let key = (structure, variant.label(), sim.e.window(), sim.e.range());
            let live = variant == EVariant::Live && sim.e.live;
            rows.push(row(trades, key, live, iterations, seed));
        }
    }
    rows
}

/// One variant's results: (structure, strategy, window, range).
pub(crate) fn row(
    trades: &[SimTrade],
    (structure, strategy, window, range): (&str, &str, u32, String),
    live: bool,
    iterations: usize,
    seed: u64,
) -> StrategyRow {
    let ts: Vec<&SimTrade> = trades
        .iter()
        .filter(|t| {
            t.structure == structure
                && t.strategy == strategy
                && t.window == window
                && t.range == range
        })
        .collect();
    let mut per_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
    for t in &ts {
        let e = per_day.entry(t.date).or_insert((0.0, 0.0));
        e.0 += t.pnl_usd;
        e.1 += 1.0;
    }
    let sums: Vec<(f64, f64)> = per_day.values().copied().collect();
    let (ci_low, ci_high) = ratio_ci(&sums, iterations, seed);
    let n = ts.len() as f64;
    // An empty float sum is −0.0, which would print as "-0.00".
    let total: f64 = if ts.is_empty() {
        0.0
    } else {
        ts.iter().map(|t| t.pnl_usd).sum()
    };
    StrategyRow {
        structure: structure.to_owned(),
        strategy: strategy.to_owned(),
        window,
        range,
        live,
        trades: ts.len() as u64,
        wins: ts.iter().filter(|t| t.won).count() as u64,
        days: per_day.len() as u64,
        mean_price: if n > 0.0 {
            ts.iter().map(|t| t.price).sum::<f64>() / n
        } else {
            0.0
        },
        pnl_per_trade: if n > 0.0 { total / n } else { 0.0 },
        ci_low,
        ci_high,
        total_usd: total,
    }
}

/// Plain-language conclusions of the replay.
pub(crate) fn verdict(rows: &[StrategyRow], sim: &MarketSimConfig) -> Vec<String> {
    let mut v = Vec::new();
    if rows.iter().all(|r| r.trades == 0) {
        v.push("Strategies at traded prices: no replayed variant found a trade — A and B never saw enough edge, and E's conditions never held, at the prices the market traded.".into());
        return v;
    }
    for structure in STRUCTURES {
        let live: Vec<&StrategyRow> = rows
            .iter()
            .filter(|r| {
                r.structure == structure && r.live && (r.strategy == "A" || r.strategy == "B")
            })
            .collect();
        let n: u64 = live.iter().map(|r| r.trades).sum();
        let wins: u64 = live.iter().map(|r| r.wins).sum();
        let total: f64 = live.iter().map(|r| r.total_usd).sum();
        v.push(format!(
            "Live rule ({}′ confirmation, asks {}) at traded prices with the {structure} structure: {n} trades (A {}, B {}), {wins} won, total ${total:+.2} at ${:.0} per trade.",
            sim.live_window,
            range_label(sim.live_range),
            live.iter().find(|r| r.strategy == "A").map_or(0, |r| r.trades),
            live.iter().find(|r| r.strategy == "B").map_or(0, |r| r.trades),
            sim.stake_usd
        ));
    }
    let e = &sim.e;
    for structure in STRUCTURES {
        let find = |x: EVariant| {
            rows.iter()
                .find(|r| r.structure == structure && r.strategy == x.label())
        };
        let (Some(live), Some(no_book), Some(model)) = (
            find(EVariant::Live),
            find(EVariant::NoBook),
            find(EVariant::Model),
        ) else {
            continue;
        };
        v.push(format!(
            "Strategy E{} ({}–{} local, high first reached ≥ {}′ before and ≥ {:.1} °C below, asks {}) with the {structure} structure: {} trades, {} won, ${:+.2} (${:+.3} per trade, 95% CI {:+.3} … {:+.3}); without the book condition {} trades, {} won, ${:+.2}; with the model ≥ {:.2} as well {} trades, {} won, ${:+.2}.",
            if e.live { "" } else { " (disabled live)" },
            hm(e.start_local_minute),
            hm(e.end_local_minute),
            e.min_minutes_at_high,
            f64::from(e.min_drop_tenths) / 10.0,
            e.range(),
            live.trades,
            live.wins,
            live.total_usd,
            live.pnl_per_trade,
            live.ci_low,
            live.ci_high,
            no_book.trades,
            no_book.wins,
            no_book.total_usd,
            e.min_model_p.max(e.veto_model_p),
            model.trades,
            model.wins,
            model.total_usd,
        ));
    }
    if sim.maker.enabled {
        for structure in STRUCTURES {
            let parts: Vec<String> = MakerRule::ALL
                .iter()
                .filter_map(|rule| {
                    rows.iter()
                        .find(|r| r.structure == structure && r.strategy == rule.label())
                        .map(|r| {
                            if r.trades == 0 {
                                format!("{} no fill", rule.label())
                            } else {
                                format!(
                                    "{} {} trades, {} won, ${:+.2}",
                                    rule.label(),
                                    r.trades,
                                    r.wins,
                                    r.total_usd
                                )
                            }
                        })
                })
                .collect();
            if !parts.is_empty() {
                v.push(format!(
                    "Live rules as limit orders with the {structure} structure (filled only when a later trade goes through the price, cancelled {}′ before each report, {:.0}% fee rebate): {}.",
                    sim.maker.cancel_before_report_min,
                    100.0 * sim.maker.rebate_share,
                    parts.join("; ")
                ));
            }
        }
    }
    let tried = rows.len();
    if let Some(best) = rows
        .iter()
        .filter(|r| r.trades >= 5)
        .max_by(|a, b| a.total_usd.total_cmp(&b.total_usd))
    {
        v.push(format!(
            "Best of {tried} replayed variants (≥ 5 trades): {} · {} · {}′ · {} — {} trades, {} won, ${:+.2} total, ${:+.3} per trade (95% CI {:+.3} … {:+.3}). With {tried} variants tried, the best one overstates what to expect: treat it as a hypothesis to confirm on later days, not as a result.",
            best.strategy,
            best.structure,
            best.window,
            best.range,
            best.trades,
            best.wins,
            best.total_usd,
            best.pnl_per_trade,
            best.ci_low,
            best.ci_high
        ));
    }
    v
}

/// The report-by-report replay of one day.
pub(crate) fn timeline(
    date: NaiveDate,
    buckets: &[TemperatureBucket],
    winner: &str,
    decisions: &[Decision],
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    tz: Tz,
) -> DayTimeline {
    let cell = |d: &Option<IncrementDistribution>| {
        d.as_ref().map(|d| {
            d.source
                .split_once(':')
                .map_or(d.source.as_str(), |(_, c)| c)
                .to_owned()
        })
    };
    let rows = decisions
        .iter()
        .map(|d| {
            let high = d.f.high_whole;
            let hb = buckets.iter().position(|b| b.contains(high));
            let p = |x: &Option<IncrementDistribution>| {
                hb.and_then(|i| x.as_ref().map(|d| d.p_in_bucket_lower(high, &buckets[i])))
            };
            let report = local_hm(d.at, tz);
            let here: Vec<&SimTrade> = trades
                .iter()
                .filter(|t| t.date == date && t.report == report)
                .collect();
            TimelineRow {
                trades: grouped(&here),
                report,
                temp_c: f64::from(d.f.current_tenths) / 10.0,
                high,
                since_last_touch: d.f.minutes_since_high,
                since_first_reach: d.f.minutes_since_first_high,
                rise_c: d.f.forecast_rise_tenths.map(|t| f64::from(t) / 10.0),
                headroom_c: d.f.forecast_headroom_tenths.map(|t| f64::from(t) / 10.0),
                p_high_current: p(&d.dists[0]),
                p_high_candidate: p(&d.dists[1]),
                cell_current: cell(&d.dists[0]),
                cell_candidate: cell(&d.dists[1]),
                market_high: hb.and_then(|i| d.quotes[i].mid),
                yes_ask_high: hb.and_then(|i| d.quotes[i].yes_ask),
                bought_high: hb.and_then(|i| d.flows.get(i)).map(|f| f.bought),
                sold_high: hb.and_then(|i| d.flows.get(i)).map(|f| f.sold),
            }
        })
        .collect();
    DayTimeline {
        date,
        winner: winner.to_owned(),
        flow_minutes: sim.e.lookback_minutes,
        rows,
    }
}

/// Trades of one report, variants that made the same trade on one line:
/// `B · current · 0′/30′/60′ · 0.90–0.99/0.70–0.99: NO 22°C @ 0.982 (p 0.998) → won +0.12 $`
/// (F: `F · 0.90–0.95: YES 22°C @ 0.930 at 15:41 (p 0.960) → won +6.20 $`).
fn grouped(trades: &[&SimTrade]) -> Vec<String> {
    let mut groups: Vec<(&SimTrade, Vec<u32>, Vec<&str>)> = Vec::new();
    for t in trades {
        let same = |g: &&SimTrade| {
            g.structure == t.structure
                && g.strategy == t.strategy
                && g.bucket == t.bucket
                && g.side == t.side
                && g.price.to_bits() == t.price.to_bits()
                && g.p_used.to_bits() == t.p_used.to_bits()
                && g.won == t.won
                && g.filled == t.filled
        };
        match groups.iter_mut().find(|(g, _, _)| same(g)) {
            Some((_, windows, ranges)) => {
                if !windows.contains(&t.window) {
                    windows.push(t.window);
                }
                if !ranges.contains(&t.range.as_str()) {
                    ranges.push(&t.range);
                }
            }
            None => groups.push((t, vec![t.window], vec![&t.range])),
        }
    }
    groups
        .into_iter()
        .map(|(t, windows, ranges)| {
            // F has neither a model structure nor a confirmation window.
            let variant = if t.structure == crate::market_peak::STRUCTURE {
                ranges.join("/")
            } else {
                format!(
                    "{} · {} · {}",
                    t.structure,
                    windows
                        .iter()
                        .map(|w| format!("{w}′"))
                        .collect::<Vec<_>>()
                        .join("/"),
                    ranges.join("/")
                )
            };
            format!(
                "{} · {variant}: {} {} @ {:.3}{} (p {:.3}) → {} {:+.2} $",
                t.strategy,
                t.side,
                t.bucket,
                t.price,
                t.filled
                    .as_ref()
                    .map_or_else(String::new, |f| format!(" at {f}")),
                t.p_used,
                if t.won { "won" } else { "lost" },
                t.pnl_usd
            )
        })
        .collect()
}

pub(crate) fn strategies_markdown(
    rows: &[StrategyRow],
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    verdict: &[String],
) -> String {
    let mut s = format!(
        "\n## Strategies at traded prices\n\nStrategies A (YES on the bucket holding the high) and B (NO on the buckets {} above it) replayed at every report with each structure's prequential model. Ask = the latest taker buy of YES (A) or one minus the latest taker sell of YES (B); filled at that price plus {:.3} slippage, taker fee paid, ${:.0} per trade; the model is pooled with the traded midpoint and capped at the model, as live, and with weight on the market no trade happens without a midpoint (both sides traded, at most {:.2} apart). At most one trade per day, bucket and variant. Depth is unknown, so fills are optimistic in thin markets. **live** marks the configured rule.\n\n",
        sim.no_distances
            .iter()
            .map(|k| format!("+{k}"))
            .collect::<Vec<_>>()
            .join("/"),
        sim.slippage,
        sim.stake_usd,
        sim.max_market_spread
    );
    let e = &sim.e;
    let _ = write!(
        s,
        "Strategy E buys YES on the bucket holding the high between {} and {} local once the high was first reached ≥ {}′ before the report and the report is ≥ {:.1} °C below it, at asks {}. Only trades are archived, so its book condition is replayed by a stand-in: in the {}′ before the decision takers bought ≥ {:.0} YES shares at or below {:.2}, more than they sold, and the ask (latest taker buy) did not fall. *E w/o book* drops that condition; *E + model* also needs the model's probability ≥ {:.2}. Like live, E needs a model but no model edge; its confirmation column counts from the first report at the high.\n\n",
        hm(e.start_local_minute),
        hm(e.end_local_minute),
        e.min_minutes_at_high,
        f64::from(e.min_drop_tenths) / 10.0,
        e.range(),
        e.lookback_minutes,
        e.min_bought_shares,
        e.max_price,
        e.min_model_p.max(e.veto_model_p),
    );
    if sim.maker.enabled {
        let _ = write!(
            s,
            "*A maker*, *B maker* and *E maker* post the live rules' orders as limit orders instead of paying the ask: a YES bid at the latest taker sell of YES (A, E) or a NO bid at one minus the latest taker buy of YES (B), at the same gates, with A's and B's edge measured at that price. An order counts as filled only when a later trade goes through its price, is cancelled {}′ before the next routine report, pays no fee and earns {:.0}% of the taker fee as a rebate. The public tape shows no queue positions, so these fills are estimates.\n\n",
            sim.maker.cancel_before_report_min,
            100.0 * sim.maker.rebate_share
        );
    }
    for line in verdict {
        let _ = writeln!(s, "* {line}");
    }
    s.push_str("\n| strategy | structure | confirmation | asks | trades | won | days | mean price | P&L per trade | 95% CI | total |\n|---|---|---:|---|---:|---:|---:|---:|---:|---|---:|\n");
    for r in rows {
        let _ = writeln!(
            s,
            "| {}{} | {} | {}′ | {} | {} | {} | {} | {:.3} | {:+.3} | [{:+.3}, {:+.3}] | {:+.2} |",
            r.strategy,
            if r.live { " **live**" } else { "" },
            r.structure,
            r.window,
            r.range,
            r.trades,
            r.wins,
            r.days,
            r.mean_price,
            r.pnl_per_trade,
            r.ci_low,
            r.ci_high,
            r.total_usd
        );
    }
    s.push_str(&losses_markdown(
        trades,
        'E',
        "Strategy E's losing trades (at these prices one loss costs as much as 20–30 wins)",
    ));
    s
}

/// One strategy's losing trades (its label starts with `letter`), one row
/// per trade, with the variants and structures that took it.
pub(crate) fn losses_markdown(trades: &[SimTrade], letter: char, title: &str) -> String {
    let mut groups: Vec<(&SimTrade, Vec<(&str, &str)>)> = Vec::new();
    for t in trades
        .iter()
        .filter(|t| t.strategy.starts_with(letter) && !t.won)
    {
        let same = |g: &&SimTrade| {
            g.date == t.date
                && g.report == t.report
                && g.bucket == t.bucket
                && g.price.to_bits() == t.price.to_bits()
        };
        match groups.iter_mut().find(|(g, _)| same(g)) {
            Some((_, by)) => by.push((&t.strategy, &t.structure)),
            None => groups.push((t, vec![(&t.strategy, &t.structure)])),
        }
    }
    if groups.is_empty() {
        return String::new();
    }
    groups.sort_by(|a, b| (a.0.date, &a.0.report).cmp(&(b.0.date, &b.0.report)));
    let mut s = format!(
        "\n{title}:\n\n| date | report | bought | ask | resolved | taken by |\n|---|---|---|---:|---|---|\n"
    );
    for (t, by) in groups {
        let mut variants: Vec<(&str, Vec<&str>)> = Vec::new();
        for (v, structure) in by {
            match variants.iter_mut().find(|(x, _)| *x == v) {
                Some((_, ss)) => ss.push(structure),
                None => variants.push((v, vec![structure])),
            }
        }
        let taken = variants
            .iter()
            .map(|(v, ss)| {
                if ss.iter().all(|x| *x == crate::market_peak::STRUCTURE) {
                    (*v).to_owned()
                } else {
                    format!("{v} ({})", ss.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            s,
            "| {} | {}{} | YES {} | {:.3} | {} | {taken} |",
            t.date,
            t.report,
            t.filled
                .as_ref()
                .map_or_else(String::new, |f| format!(" (filled {f})")),
            t.bucket,
            t.price,
            t.resolved
        );
    }
    s
}

pub(crate) fn timeline_markdown(t: &DayTimeline, delay_s: i64) -> String {
    let c = |v: Option<f64>, d: usize| v.map_or_else(|| "—".to_owned(), |x| format!("{x:.d$}"));
    let signed = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |x| format!("{x:+.1}"));
    let mut s = format!(
        "\n## Day replay — {} (resolved {})\n\nEvery report from the first decision time; decisions {delay_s} s after the report. P(high) = the model's probability that the final high stays in the bucket holding the current high; market = that bucket's traded midpoint, ask = its latest taker buy; bought / sold = its YES shares taker-bought (at or below E's cap) and taker-sold in the {}′ before the decision, strategy E's stand-in for its book.\n\n| report | temp | high | since last / first touch | rise / headroom | P(high) current | P(high) candidate | market | ask | bought / sold | trades |\n|---|---:|---:|---|---|---:|---:|---:|---:|---|---|\n",
        t.date, t.winner, t.flow_minutes
    );
    for r in &t.rows {
        let _ = writeln!(
            s,
            "| {} | {:.1} | {} | {} / {} min | {} / {} | {} | {} | {} | {} | {} / {} | {} |",
            r.report,
            r.temp_c,
            r.high,
            r.since_last_touch,
            r.since_first_reach,
            signed(r.rise_c),
            signed(r.headroom_c),
            c(r.p_high_current, 3),
            c(r.p_high_candidate, 3),
            c(r.market_high, 3),
            c(r.yes_ask_high, 3),
            c(r.bought_high, 0),
            c(r.sold_high, 0),
            if r.trades.is_empty() {
                String::new()
            } else {
                r.trades.join("<br>")
            }
        );
    }
    s.push_str("\n| report | current cell | candidate cell |\n|---|---|---|\n");
    for r in &t.rows {
        let _ = writeln!(
            s,
            "| {} | `{}` | `{}` |",
            r.report,
            r.cell_current.as_deref().unwrap_or("—"),
            r.cell_candidate.as_deref().unwrap_or("—")
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::market::TempUnit;

    #[test]
    fn b_trades_only_buckets_above_the_high_once_each() {
        let u = TempUnit::Celsius;
        let b = vec![
            TemperatureBucket::at_or_below(18, u),
            TemperatureBucket::exact(19, u),
            TemperatureBucket::exact(20, u),
            TemperatureBucket::at_or_above(21, u),
        ];
        assert_eq!(buckets_for(Kind::A, &b, 19, &[1, 2, 3]), vec![1]);
        // 20 and the open top bucket (21, 22 → the same bucket).
        assert_eq!(buckets_for(Kind::B, &b, 19, &[1, 2, 3]), vec![2, 3]);
        // At a high inside the open top bucket nothing lies above it.
        assert!(buckets_for(Kind::B, &b, 21, &[1, 2, 3]).is_empty());
        assert_eq!(buckets_for(Kind::A, &b, 25, &[1]), vec![3]);
    }

    fn trade(structure: &str, window: u32, range: &str, won: bool) -> SimTrade {
        SimTrade {
            date: NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(),
            report: "12:25".into(),
            structure: structure.into(),
            strategy: "A".into(),
            window,
            range: range.into(),
            bucket: "21°C".into(),
            side: "YES".into(),
            price: 0.86,
            p_model: 0.95,
            p_used: 0.9,
            won,
            pnl_usd: 1.5,
            resolved: if won { "21°C" } else { "22°C" }.into(),
            filled: None,
        }
    }

    #[test]
    fn e_losses_are_listed_once_with_everyone_who_took_them() {
        let loss = |strategy: &str, structure: &str| SimTrade {
            strategy: strategy.into(),
            price: 0.965,
            ..trade(structure, 60, "0.90–0.99", false)
        };
        let ts = [
            loss("E", "current"),
            loss("E w/o book", "current"),
            loss("E", "candidate"),
            SimTrade {
                strategy: "E".into(),
                ..trade("current", 60, "0.90–0.99", true)
            },
            trade("current", 60, "0.90–0.99", false),
        ];
        let md = losses_markdown(&ts, 'E', "E's losses");
        assert_eq!(
            md.lines()
                .filter(|l| l.starts_with("| 2026"))
                .collect::<Vec<_>>(),
            vec![
                "| 2026-09-28 | 12:25 | YES 21°C | 0.965 | 22°C | E (current, candidate), E w/o book (current) |"
            ]
        );
        assert!(
            losses_markdown(&ts[3..], 'E', "E's losses").is_empty(),
            "A's losses and E's wins are not listed"
        );
    }

    #[test]
    fn identical_trades_of_several_variants_share_a_line() {
        let ts = [
            trade("current", 0, "0.70–0.99", true),
            trade("current", 30, "0.70–0.99", true),
            trade("current", 30, "0.90–0.99", true),
            trade("candidate", 0, "0.70–0.99", true),
        ];
        let refs: Vec<&SimTrade> = ts.iter().collect();
        assert_eq!(
            grouped(&refs),
            vec![
                "A · current · 0′/30′ · 0.70–0.99/0.90–0.99: YES 21°C @ 0.860 (p 0.900) → won +1.50 $"
                    .to_owned(),
                "A · candidate · 0′ · 0.70–0.99: YES 21°C @ 0.860 (p 0.900) → won +1.50 $".to_owned(),
            ]
        );
        assert!(grouped(&[]).is_empty());
    }

    #[test]
    fn buyers_lift_the_offers_only_with_enough_net_buying_and_a_held_ask() {
        let f = Flow {
            bought: 40.0,
            sold: 10.0,
            ask_then: Some(0.93),
        };
        assert!(f.lifting(Some(0.95), 15.0));
        assert!(f.lifting(Some(0.93), 15.0), "an unchanged ask holds");
        assert!(!f.lifting(Some(0.92), 15.0), "the ask fell");
        assert!(!f.lifting(None, 15.0));
        assert!(!f.lifting(Some(0.95), 50.0), "too little bought");
        let even = Flow { sold: 40.0, ..f };
        assert!(!even.lifting(Some(0.95), 15.0), "sellers as active");
        let unknown = Flow {
            ask_then: None,
            ..f
        };
        assert!(!unknown.lifting(Some(0.95), 15.0));
        assert_eq!(hm(12 * 60 + 5), "12:05");
    }

    #[test]
    fn e_replays_the_live_settings() {
        let live = wm_strategy::BookConfirmedConfig::default();
        let e = BookConfirmedSim::from_live(&live);
        assert!(e.live);
        assert!(
            (e.min_bought_shares - 15.0).abs() < 1e-9,
            "50 shares × 30 %"
        );
        assert_eq!((e.window(), e.range()), (60, "0.90–0.99".to_owned()));
        assert_eq!(MarketSimConfig::default().e, e);
    }

    #[test]
    fn the_pool_midpoint_needs_both_sides_close_together() {
        let q = Quote {
            mid: Some(0.5),
            yes_ask: Some(0.86),
            yes_bid: Some(0.80),
        };
        assert!((q.pool_mid(0.10).unwrap() - 0.83).abs() < 1e-12);
        assert_eq!(q.pool_mid(0.05), None);
        let one = Quote { yes_bid: None, ..q };
        assert_eq!(one.pool_mid(0.10), None);
    }
}
