//! The maker side of the market (`research market`): what resting orders
//! earned on the settled markets' trades, and the live rules replayed as
//! limit orders.
//!
//! Every recorded trade has a taker, who crossed the spread and paid the
//! taker fee, and a maker, whose resting order was hit: no fee, and a share
//! of the taker fee back as a rebate (Polymarket weather markets: 25 %).
//! Before fees the maker's P&L is the taker's with the sign flipped, so the
//! trades show segment by segment where resting orders were paid for their
//! liquidity and where better-informed takers picked them off — before a
//! METAR report, on the bucket holding the high, for example.
//!
//! The replays post the live rules' orders at the opposite quote: a YES bid
//! at the latest taker sell of YES (A, E), a NO bid at one minus the latest
//! taker buy of YES (B). An order counts as filled only when a later trade
//! goes through its price — a conservative rule, since a trade at a worse
//! price would have met the resting order first. Orders are cancelled
//! `cancel_before_report_min` before the next routine report, when informed
//! selling concentrates. Fills pay no fee and earn the rebate. The public
//! tape shows no queue positions, so fills remain estimates.

use crate::forecast_eval::ratio_ci;
use crate::market_eval::MarketTrade;
use crate::market_sim::{
    Decision, Kind, MarketSimConfig, STRUCTURES, SimTrade, StrategyRow, buckets_for, local_hm,
    range_label, row,
};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use wm_core::market::TemperatureBucket;
use wm_strategy::{PeakFeatures, log_pool};

/// Maker replay settings (research only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MakerSim {
    /// Replay the live rules as limit orders.
    pub enabled: bool,
    /// Resting orders are cancelled this long before the next routine
    /// report.
    pub cancel_before_report_min: i64,
    /// Share of the taker fee paid back to makers.
    pub rebate_share: f64,
}

impl Default for MakerSim {
    fn default() -> Self {
        Self {
            enabled: true,
            cancel_before_report_min: 10,
            rebate_share: 0.25,
        }
    }
}

/// The first routine report strictly after `t`: `minutes` past each UTC
/// hour. `None` without a schedule.
pub(crate) fn next_routine(t: DateTime<Utc>, minutes: &[u8]) -> Option<DateTime<Utc>> {
    let hour = t
        .with_minute(0)
        .and_then(|h| h.with_second(0))
        .and_then(|h| h.with_nanosecond(0))?;
    (0..=1)
        .flat_map(|h| {
            minutes
                .iter()
                .map(move |m| hour + Duration::hours(h) + Duration::minutes(i64::from(*m)))
        })
        .filter(|x| *x > t)
        .min()
}

/// When an order placed at `at` is cancelled: `before_min` ahead of the next
/// routine report, or `None` when that is not after `at`.
pub(crate) fn cancel_time(
    at: DateTime<Utc>,
    routine_minutes: &[u8],
    before_min: i64,
) -> Option<DateTime<Utc>> {
    let cancel = next_routine(at, routine_minutes)? - Duration::minutes(before_min);
    (cancel > at).then_some(cancel)
}

/// A resting order in YES terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Resting {
    /// A bid for YES at this price.
    YesBid(f64),
    /// An ask for YES at this price: a bid for NO at one minus it.
    YesAsk(f64),
}

/// Time of the first trade in `(from, until]` that goes through the resting
/// price: a taker selling YES below a bid, or buying YES above an ask.
/// `trades` are one bucket's, oldest first.
pub(crate) fn through_fill(
    trades: &[&MarketTrade],
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    order: Resting,
) -> Option<DateTime<Utc>> {
    let start = trades.partition_point(|t| t.at <= from);
    trades[start..]
        .iter()
        .take_while(|t| t.at <= until)
        .find(|t| match order {
            Resting::YesBid(b) => !t.taker_buys_yes && t.yes_price < b - 1e-9,
            Resting::YesAsk(a) => t.taker_buys_yes && t.yes_price > a + 1e-9,
        })
        .map(|t| t.at)
}

/// The live rules replayed as limit orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MakerRule {
    A,
    B,
    E,
}

impl MakerRule {
    pub(crate) const ALL: [MakerRule; 3] = [MakerRule::A, MakerRule::B, MakerRule::E];

    pub(crate) fn label(self) -> &'static str {
        match self {
            MakerRule::A => "A maker",
            MakerRule::B => "B maker",
            MakerRule::E => "E maker",
        }
    }

    /// The rule's confirmation and ask range, as the table shows them.
    fn key(self, sim: &MarketSimConfig) -> (u32, String) {
        match self {
            MakerRule::A | MakerRule::B => (sim.live_window, range_label(sim.live_range)),
            MakerRule::E => (sim.e.window(), sim.e.range()),
        }
    }
}

/// One order a rule would post at a decision.
struct Order {
    bucket: usize,
    resting: Resting,
    /// Price of the token bought (YES for A and E, NO for B).
    price: f64,
    p_model: f64,
    /// The market probability of that token, when the book is tight.
    market: Option<f64>,
}

/// Orders of one rule at one decision: the live gates, with the maker's
/// price and no fee or slippage in the edge.
fn orders(
    rule: MakerRule,
    d: &Decision,
    s: usize,
    buckets: &[TemperatureBucket],
    sim: &MarketSimConfig,
    min_support: u32,
) -> Vec<Order> {
    let Some(dist) = &d.dists[s] else {
        return Vec::new();
    };
    let high = d.f.high_whole;
    let in_range = |p: f64, (lo, hi): (f64, f64)| p >= lo - 1e-12 && p <= hi + 1e-12;
    match rule {
        MakerRule::A | MakerRule::B => {
            if dist.support < min_support || d.f.minutes_since_high < i64::from(sim.live_window) {
                return Vec::new();
            }
            let kind = if rule == MakerRule::A {
                Kind::A
            } else {
                Kind::B
            };
            buckets_for(kind, buckets, high, &sim.no_distances)
                .into_iter()
                .filter_map(|i| {
                    let q = &d.quotes[i];
                    let mid = q.pool_mid(sim.max_market_spread);
                    let (resting, price, p_model, market) = if rule == MakerRule::A {
                        // A YES bid at the latest taker sell, below any ask.
                        let b = q.yes_bid?;
                        if q.yes_ask.is_some_and(|a| b >= a) {
                            return None;
                        }
                        (
                            Resting::YesBid(b),
                            b,
                            dist.p_in_bucket_lower(high, &buckets[i]),
                            mid,
                        )
                    } else {
                        // A NO bid at one minus the latest taker buy of YES.
                        let a = q.yes_ask?;
                        if q.yes_bid.is_some_and(|b| a <= b) {
                            return None;
                        }
                        (
                            Resting::YesAsk(a),
                            1.0 - a,
                            1.0 - dist.p_in_bucket_upper(high, &buckets[i]),
                            mid.map(|m| 1.0 - m),
                        )
                    };
                    if !in_range(price, sim.live_range) {
                        return None;
                    }
                    Some(Order {
                        bucket: i,
                        resting,
                        price,
                        p_model,
                        market,
                    })
                })
                .collect()
        }
        MakerRule::E => {
            let e = &sim.e;
            if !e.conditions_hold(&d.f) {
                return Vec::new();
            }
            let Some(i) = buckets.iter().position(|b| b.contains(high)) else {
                return Vec::new();
            };
            let q = &d.quotes[i];
            let Some(b) = q.yes_bid else {
                return Vec::new();
            };
            let p_model = dist.p_in_bucket_lower(high, &buckets[i]);
            let lifting = d
                .flows
                .get(i)
                .is_some_and(|fl| fl.lifting(q.yes_ask, e.min_bought_shares));
            if q.yes_ask.is_some_and(|a| b >= a)
                || !in_range(b, (e.min_price, e.max_price))
                || !lifting
                || p_model < e.min_model_p
            {
                return Vec::new();
            }
            vec![Order {
                bucket: i,
                resting: Resting::YesBid(b),
                price: b,
                p_model,
                market: q.pool_mid(sim.max_market_spread),
            }]
        }
    }
}

/// The live rules as limit orders on one market day: at most one fill per
/// rule, structure and bucket.
#[allow(clippy::too_many_arguments)]
pub(crate) fn simulate_makers(
    date: NaiveDate,
    buckets: &[TemperatureBucket],
    labels: &[String],
    winner: usize,
    decisions: &[Decision],
    per_bucket: &[Vec<&MarketTrade>],
    sim: &MarketSimConfig,
    fee_rate: f64,
    market_weight: f64,
    min_support: u32,
    routine_minutes: &[u8],
    tz: Tz,
) -> Vec<SimTrade> {
    let m = &sim.maker;
    if !m.enabled {
        return Vec::new();
    }
    let rebate = |p: f64| m.rebate_share * fee_rate * p * (1.0 - p);
    let mut out = Vec::new();
    for (s, structure) in STRUCTURES.iter().enumerate() {
        for rule in MakerRule::ALL {
            let (window, range) = rule.key(sim);
            let mut done: HashSet<usize> = HashSet::new();
            for d in decisions {
                let Some(cancel) =
                    cancel_time(d.knowledge, routine_minutes, m.cancel_before_report_min)
                else {
                    continue;
                };
                for o in orders(rule, d, s, buckets, sim, min_support) {
                    if done.contains(&o.bucket) {
                        continue;
                    }
                    let p_used = log_pool(o.p_model, o.market, market_weight).min(o.p_model);
                    // Rules with a model edge (A, B) need it at the maker's price.
                    if rule != MakerRule::E && p_used - o.price < sim.min_edge {
                        continue;
                    }
                    let trades = per_bucket.get(o.bucket).map_or(&[][..], Vec::as_slice);
                    if through_fill(trades, d.knowledge, cancel, o.resting).is_none() {
                        continue;
                    }
                    done.insert(o.bucket);
                    let won = match rule {
                        MakerRule::A | MakerRule::E => o.bucket == winner,
                        MakerRule::B => o.bucket != winner,
                    };
                    let per_share = f64::from(u8::from(won)) - o.price + rebate(o.price);
                    out.push(SimTrade {
                        date,
                        report: local_hm(d.at, tz),
                        structure: (*structure).to_owned(),
                        strategy: rule.label().to_owned(),
                        window,
                        range: range.clone(),
                        bucket: labels[o.bucket].clone(),
                        side: if rule == MakerRule::B { "NO" } else { "YES" }.to_owned(),
                        price: o.price,
                        p_model: o.p_model,
                        p_used,
                        won,
                        pnl_usd: sim.stake_usd / o.price * per_share,
                        resolved: labels[winner].clone(),
                    });
                }
            }
        }
    }
    out
}

/// Rows of the maker replays, per structure and rule.
pub(crate) fn maker_rows(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    iterations: usize,
    seed: u64,
) -> Vec<StrategyRow> {
    if !sim.maker.enabled {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for structure in STRUCTURES {
        for rule in MakerRule::ALL {
            let (window, range) = rule.key(sim);
            rows.push(row(
                trades,
                (structure, rule.label(), window, range),
                false,
                iterations,
                seed,
            ));
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// The other side of every trade
// ---------------------------------------------------------------------------

/// Segment families and their groups, in report order.
const FAMILIES: [(&str, &[&str]); 7] = [
    ("all trades", &["all"]),
    ("the taker bought", &["YES", "NO"]),
    (
        "price the taker paid",
        &[
            "0.00–0.02",
            "0.02–0.10",
            "0.10–0.30",
            "0.30–0.70",
            "0.70–0.90",
            "0.90–0.98",
            "0.98–1.00",
        ],
    ),
    (
        "bucket against the reported high",
        &[
            "no report yet",
            "below the high (decided)",
            "holds the high",
            "+1 above",
            "+2 above",
            "+3 or more above",
        ],
    ),
    (
        "minutes to the next routine report",
        &["0–5", "5–10", "10–20", "20 or more"],
    ),
    (
        "the high's bucket, minutes to the next report",
        &["0–5", "5–10", "10–20", "20 or more"],
    ),
    (
        "local time of the trade",
        &[
            "the evening before",
            "00:00–09:00",
            "09:00–12:00",
            "12:00–15:00",
            "15:00–18:00",
            "18:00–24:00",
        ],
    ),
];

/// One segment of the maker/taker study. Amounts per share, in USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowRow {
    pub family: String,
    pub group: String,
    pub trades: u64,
    pub shares: f64,
    /// What the takers paid.
    pub volume_usd: f64,
    /// Days with a trade in the segment.
    pub days: u64,
    /// The takers' P&L before fees (the makers' is its negative).
    pub taker_per_share: f64,
    /// Taker fee.
    pub fee_per_share: f64,
    /// The makers' P&L with the rebate, and its 95 % day-block interval.
    pub maker_net_per_share: f64,
    pub maker_ci_low: f64,
    pub maker_ci_high: f64,
}

/// What resting orders earned on the studied trades.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MakerTakerStudy {
    pub fee_rate: f64,
    pub rebate_share: f64,
    pub rows: Vec<FlowRow>,
    pub verdict: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Cell {
    trades: u64,
    shares: f64,
    volume: f64,
    pnl: f64,
    fee: f64,
}

impl Cell {
    fn add(&mut self, o: &Cell) {
        self.trades += o.trades;
        self.shares += o.shares;
        self.volume += o.volume;
        self.pnl += o.pnl;
        self.fee += o.fee;
    }
}

fn price_band(p: f64) -> usize {
    [0.02, 0.10, 0.30, 0.70, 0.90, 0.98]
        .iter()
        .position(|b| p < *b)
        .unwrap_or(6)
}

/// The bucket against the high published by then (`None`: no report yet).
fn position(bucket: &TemperatureBucket, high: Option<i32>) -> usize {
    let Some(h) = high else { return 0 };
    if bucket.upper.is_some_and(|u| u < h) {
        1
    } else if bucket.contains(h) {
        2
    } else {
        match bucket.lower.map(|l| l - h) {
            Some(1) => 3,
            Some(2) => 4,
            _ => 5,
        }
    }
}

fn minutes_band(minutes: i64) -> usize {
    match minutes {
        m if m < 5 => 0,
        m if m < 10 => 1,
        m if m < 20 => 2,
        _ => 3,
    }
}

/// Collects every trade of the studied days by segment and day.
#[derive(Debug, Default)]
pub(crate) struct FlowCollector {
    cells: BTreeMap<(usize, usize), BTreeMap<NaiveDate, Cell>>,
}

impl FlowCollector {
    /// Add one market day's trades. `states` are the day's reports (time,
    /// features), oldest first; a report counts from `delay` after it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_day(
        &mut self,
        date: NaiveDate,
        buckets: &[TemperatureBucket],
        winner: usize,
        trades: &[MarketTrade],
        states: &[(DateTime<Utc>, PeakFeatures)],
        delay: Duration,
        routine_minutes: &[u8],
        fee_rate: f64,
        tz: Tz,
    ) {
        for t in trades {
            let Some(bucket) = buckets.get(t.bucket) else {
                continue;
            };
            if !(t.shares > 0.0 && (0.0..=1.0).contains(&t.yes_price)) {
                continue;
            }
            let yes_won = t.bucket == winner;
            let (price, won) = if t.taker_buys_yes {
                (t.yes_price, yes_won)
            } else {
                (1.0 - t.yes_price, !yes_won)
            };
            let cell = Cell {
                trades: 1,
                shares: t.shares,
                volume: t.shares * price,
                pnl: t.shares * (f64::from(u8::from(won)) - price),
                fee: t.shares * fee_rate * t.yes_price * (1.0 - t.yes_price),
            };
            let known = states.partition_point(|(at, _)| *at + delay <= t.at);
            let high = known.checked_sub(1).map(|i| states[i].1.high_whole);
            let pos = position(bucket, high);
            let to_report =
                next_routine(t.at, routine_minutes).map(|n| minutes_band((n - t.at).num_minutes()));
            let local = t.at.with_timezone(&tz);
            let when = if local.date_naive() < date {
                0
            } else {
                match local.hour() {
                    0..=8 => 1,
                    9..=11 => 2,
                    12..=14 => 3,
                    15..=17 => 4,
                    _ => 5,
                }
            };
            let mut add = |family: usize, group: usize| {
                self.cells
                    .entry((family, group))
                    .or_default()
                    .entry(date)
                    .or_default()
                    .add(&cell);
            };
            add(0, 0);
            add(1, usize::from(!t.taker_buys_yes));
            add(2, price_band(price));
            add(3, pos);
            if let Some(m) = to_report {
                add(4, m);
                if pos == 2 {
                    add(5, m);
                }
            }
            add(6, when);
        }
    }

    /// Rows per segment with day-block intervals, and the verdict.
    pub(crate) fn finish(
        &self,
        fee_rate: f64,
        rebate_share: f64,
        iterations: usize,
        seed: u64,
    ) -> MakerTakerStudy {
        let rows: Vec<FlowRow> = self
            .cells
            .iter()
            .filter_map(|(&(f, g), days)| {
                let mut total = Cell::default();
                for c in days.values() {
                    total.add(c);
                }
                if total.shares <= 0.0 {
                    return None;
                }
                let sums: Vec<(f64, f64)> = days.values().map(|c| (c.pnl, c.shares)).collect();
                let (lo, hi) = ratio_ci(&sums, iterations, seed ^ ((f as u64) << 8 | g as u64));
                let taker = total.pnl / total.shares;
                let rebate = rebate_share * total.fee / total.shares;
                Some(FlowRow {
                    family: FAMILIES[f].0.to_owned(),
                    group: FAMILIES[f].1[g].to_owned(),
                    trades: total.trades,
                    shares: total.shares,
                    volume_usd: total.volume,
                    days: days.len() as u64,
                    taker_per_share: taker,
                    fee_per_share: total.fee / total.shares,
                    maker_net_per_share: -taker + rebate,
                    maker_ci_low: -hi + rebate,
                    maker_ci_high: -lo + rebate,
                })
            })
            .collect();
        let verdict = study_verdict(&rows, rebate_share);
        MakerTakerStudy {
            fee_rate,
            rebate_share,
            rows,
            verdict,
        }
    }
}

fn cents(x: f64) -> f64 {
    100.0 * x
}

/// Plain-language conclusions of the maker/taker study.
fn study_verdict(rows: &[FlowRow], rebate_share: f64) -> Vec<String> {
    let Some(all) = rows.iter().find(|r| r.family == FAMILIES[0].0) else {
        return vec!["Makers and takers: no trades to study.".into()];
    };
    let mut v = vec![format!(
        "Makers and takers: over {} trades ({:.0} shares, ${:.0} paid by takers) the takers {} {:.2}¢ per share before fees and paid {:.2}¢ in fees; the makers on the other side {} {:.2}¢ per share with the {:.0}% rebate (95% CI {:+.2} … {:+.2}), ${:+.2} in all.",
        all.trades,
        all.shares,
        all.volume_usd,
        if all.taker_per_share < 0.0 {
            "lost"
        } else {
            "gained"
        },
        cents(all.taker_per_share.abs()),
        cents(all.fee_per_share),
        if all.maker_net_per_share >= 0.0 {
            "earned"
        } else {
            "lost"
        },
        cents(all.maker_net_per_share.abs()),
        100.0 * rebate_share,
        cents(all.maker_ci_low),
        cents(all.maker_ci_high),
        all.maker_net_per_share * all.shares,
    )];
    // Segments big enough to act on: ≥ 2 % of the shares, on ≥ 5 days.
    let floor = 0.02 * all.shares;
    let segments: Vec<&FlowRow> = rows
        .iter()
        .filter(|r| r.family != FAMILIES[0].0 && r.shares >= floor && r.days >= 5)
        .collect();
    let best = segments
        .iter()
        .max_by(|a, b| a.maker_net_per_share.total_cmp(&b.maker_net_per_share));
    let worst = segments
        .iter()
        .min_by(|a, b| a.maker_net_per_share.total_cmp(&b.maker_net_per_share));
    if let (Some(b), Some(w)) = (best, worst) {
        v.push(format!(
            "Resting orders paid best on {}: {} ({:+.2}¢ per share, 95% CI {:+.2} … {:+.2}, {:.0} shares) and were picked off most on {}: {} ({:+.2}¢, 95% CI {:+.2} … {:+.2}, {:.0} shares).",
            b.family,
            b.group,
            cents(b.maker_net_per_share),
            cents(b.maker_ci_low),
            cents(b.maker_ci_high),
            b.shares,
            w.family,
            w.group,
            cents(w.maker_net_per_share),
            cents(w.maker_ci_low),
            cents(w.maker_ci_high),
            w.shares,
        ));
    }
    v
}

pub(crate) fn maker_taker_markdown(m: &MakerTakerStudy) -> String {
    if m.rows.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\n## Makers and takers: the other side of every trade\n\nEvery trade has a taker, who crossed the spread and paid the taker fee ({:.2} × p × (1 − p) per share), and a maker, whose resting order was hit: no fee, and {:.0}% of the taker fee back as a rebate. Before fees the maker's P&L is the taker's with the sign flipped. Per share at settlement; the 95% interval resamples whole days. \"The high\" is the METAR high published by the trade (observation + the decision delay); report times follow the routine schedule.\n\n",
        m.fee_rate,
        100.0 * m.rebate_share
    );
    for line in &m.verdict {
        let _ = writeln!(s, "* {line}");
    }
    s.push_str("\n| segment | trades | shares | taker P&L | taker fee | maker net | maker 95% CI | maker total |\n|---|---:|---:|---:|---:|---:|---|---:|\n");
    let mut family: &str = "";
    for r in &m.rows {
        if r.family != family {
            let _ = writeln!(s, "| **{}** | | | | | | | |", r.family);
            family = &r.family;
        }
        let _ = writeln!(
            s,
            "| {} | {} | {:.0} | {:+.2}¢ | {:.2}¢ | {:+.2}¢ | [{:+.2}, {:+.2}] | ${:+.2} |",
            r.group,
            r.trades,
            r.shares,
            cents(r.taker_per_share),
            cents(r.fee_per_share),
            cents(r.maker_net_per_share),
            cents(r.maker_ci_low),
            cents(r.maker_ci_high),
            r.maker_net_per_share * r.shares
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::market::TempUnit;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn trade(at: &str, bucket: usize, yes_price: f64, buy: bool, shares: f64) -> MarketTrade {
        MarketTrade {
            at: utc(at),
            bucket,
            yes_price,
            taker_buys_yes: buy,
            shares,
            taker: None,
        }
    }

    #[test]
    fn the_next_routine_report_is_the_first_scheduled_one_after_a_time() {
        let m = [25_u8, 55];
        let next = |t: &str| next_routine(utc(t), &m).unwrap();
        assert_eq!(next("2026-07-01T12:24:59Z"), utc("2026-07-01T12:25:00Z"));
        assert_eq!(next("2026-07-01T12:25:00Z"), utc("2026-07-01T12:55:00Z"));
        assert_eq!(next("2026-07-01T12:56:00Z"), utc("2026-07-01T13:25:00Z"));
        assert_eq!(next("2026-07-01T23:56:00Z"), utc("2026-07-02T00:25:00Z"));
        assert_eq!(next_routine(utc("2026-07-01T12:00:00Z"), &[]), None);
        // Orders rest until ten minutes before it, if that is still ahead.
        let c = |t: &str| cancel_time(utc(t), &m, 10);
        assert_eq!(c("2026-07-01T12:28:00Z"), Some(utc("2026-07-01T12:45:00Z")));
        assert_eq!(c("2026-07-01T12:45:00Z"), None);
        assert_eq!(c("2026-07-01T12:48:00Z"), None);
    }

    #[test]
    fn a_resting_order_fills_only_when_a_later_trade_goes_through_it() {
        let owned = [
            trade("2026-07-01T12:28:00Z", 0, 0.90, false, 5.0), // at `from`: before us
            trade("2026-07-01T12:29:00Z", 0, 0.95, false, 5.0), // at the bid: queue
            trade("2026-07-01T12:30:00Z", 0, 0.97, true, 5.0),  // the other side
            trade("2026-07-01T12:31:00Z", 0, 0.94, false, 5.0), // through the bid
            trade("2026-07-01T12:32:00Z", 0, 0.06, true, 5.0),  // through a 0.05 ask
        ];
        let v: Vec<&MarketTrade> = owned.iter().collect();
        let from = utc("2026-07-01T12:28:00Z");
        let fill = |until: &str, o: Resting| through_fill(&v, from, utc(until), o);
        assert_eq!(
            fill("2026-07-01T12:45:00Z", Resting::YesBid(0.95)),
            Some(utc("2026-07-01T12:31:00Z"))
        );
        // Cancelled before the through trade; a trade at `until` still counts.
        assert_eq!(fill("2026-07-01T12:30:59Z", Resting::YesBid(0.95)), None);
        assert_eq!(
            fill("2026-07-01T12:31:00Z", Resting::YesBid(0.95)),
            Some(utc("2026-07-01T12:31:00Z"))
        );
        assert_eq!(
            fill("2026-07-01T12:45:00Z", Resting::YesAsk(0.05)),
            Some(utc("2026-07-01T12:30:00Z")),
            "a taker buying YES above the ask"
        );
        assert_eq!(fill("2026-07-01T12:45:00Z", Resting::YesAsk(0.97)), None);
        assert_eq!(through_fill(&[], from, from, Resting::YesBid(0.5)), None);
    }

    fn features(high_whole: i32) -> PeakFeatures {
        let o = wm_core::weather::Observation {
            key: wm_core::weather::ObservationKey {
                station: wm_core::ids::StationId::new("EHAM").unwrap(),
                observed_at: utc("2026-07-01T11:55:00Z"),
                report_type: wm_core::weather::ReportType::Metar,
            },
            version: 1,
            temperature: Some(wm_core::units::TempC::from_whole(high_whole)),
            dewpoint: None,
            precision: wm_core::weather::TempPrecision::WholeDegree,
            raw_text: String::new(),
            content_hash: "x".into(),
            provider: wm_core::ids::ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: utc("2026-07-01T11:55:00Z"),
            parser_version: 1,
            quality: wm_core::weather::QualityFlags::default(),
        };
        let mut e = wm_strategy::TemperatureStateEngine::new(3);
        let station = o.key.station.clone();
        e.register_station(station.clone(), chrono_tz::Europe::Amsterdam);
        e.apply_observation(&o);
        let date = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let now = utc("2026-07-01T11:55:00Z");
        let s = e
            .day_state(&station, date, wm_strategy::ViewKind::All, now)
            .unwrap();
        wm_strategy::PeakDetectionEngine::default()
            .assess(&s, chrono_tz::Europe::Amsterdam, now)
            .unwrap()
            .features
    }

    #[test]
    fn the_other_side_of_every_trade_is_segmented() {
        let u = TempUnit::Celsius;
        // ≤16, 17 … 21, ≥22; 19 wins; the 11:55Z report says 18.
        let mut buckets = vec![TemperatureBucket::at_or_below(16, u)];
        buckets.extend((17..=21).map(|v| TemperatureBucket::exact(v, u)));
        buckets.push(TemperatureBucket::at_or_above(22, u));
        let winner = 3;
        let states = vec![(utc("2026-07-01T11:55:00Z"), features(18))];
        let trades = [
            // Before the report is known (11:55 + 3 min): no report yet.
            trade("2026-07-01T11:56:00Z", 3, 0.40, true, 10.0),
            // YES of 21 (+3 above) bought at 0.05: the taker loses 5.
            trade("2026-07-01T12:00:00Z", 5, 0.05, true, 100.0),
            // YES of 18 (the high's bucket) sold at 0.30: NO at 0.70 wins 15.
            trade("2026-07-01T12:20:00Z", 2, 0.30, false, 50.0),
            // Dust and nonsense are ignored.
            trade("2026-07-01T12:21:00Z", 2, 0.30, false, 0.0),
            trade("2026-07-01T12:22:00Z", 9, 0.30, false, 5.0),
        ];
        let mut c = FlowCollector::default();
        c.add_day(
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            &buckets,
            winner,
            &trades,
            &states,
            Duration::minutes(3),
            &[25, 55],
            0.05,
            chrono_tz::Europe::Amsterdam,
        );
        let m = c.finish(0.05, 0.25, 200, 7);
        let get = |family: &str, group: &str| {
            m.rows
                .iter()
                .find(|r| r.family == family && r.group == group)
                .unwrap_or_else(|| panic!("no {family} / {group}: {:?}", m.rows))
        };
        let all = get("all trades", "all");
        assert_eq!((all.trades, all.shares), (3, 160.0));
        // Taker P&L: +6 (19 won at 0.40) − 5 + 15 = 16 over 160 shares.
        assert!((all.taker_per_share - 16.0 / 160.0).abs() < 1e-12);
        let fee = 10.0 * 0.05 * 0.4 * 0.6 + 100.0 * 0.05 * 0.05 * 0.95 + 50.0 * 0.05 * 0.3 * 0.7;
        assert!((all.fee_per_share - fee / 160.0).abs() < 1e-12);
        assert!((all.maker_net_per_share - (-0.1 + 0.25 * fee / 160.0)).abs() < 1e-12);
        assert_eq!(get("the taker bought", "NO").trades, 1);
        assert_eq!(get("price the taker paid", "0.02–0.10").shares, 100.0);
        assert_eq!(get("price the taker paid", "0.70–0.90").shares, 50.0);
        assert_eq!(get("price the taker paid", "0.30–0.70").shares, 10.0);
        assert_eq!(
            get("bucket against the reported high", "no report yet").trades,
            1
        );
        assert_eq!(
            get("bucket against the reported high", "+3 or more above").trades,
            1
        );
        let high = get("bucket against the reported high", "holds the high");
        assert!((high.taker_per_share - 0.30).abs() < 1e-12);
        assert!((high.maker_net_per_share - (-0.30 + 0.25 * 0.05 * 0.3 * 0.7)).abs() < 1e-12);
        // 12:20Z is five minutes before the 12:25Z report.
        assert_eq!(get("minutes to the next routine report", "5–10").trades, 1);
        assert_eq!(
            get("the high's bucket, minutes to the next report", "5–10").trades,
            1
        );
        assert_eq!(
            get("minutes to the next routine report", "20 or more").trades,
            2
        );
        // 11:56Z–12:20Z is 13:56–14:20 in Amsterdam (CEST).
        assert_eq!(get("local time of the trade", "12:00–15:00").trades, 3);
        assert!(
            m.rows
                .iter()
                .all(|r| r.family != "local time of the trade" || r.group == "12:00–15:00")
        );
        assert!(m.verdict[0].starts_with("Makers and takers: over 3 trades"));
        let md = maker_taker_markdown(&m);
        assert!(md.contains("| **all trades** |"));
        assert!(md.contains("| holds the high | 1 | 50 | +30.00¢ |"), "{md}");
        assert!(maker_taker_markdown(&MakerTakerStudy::default()).is_empty());
    }
}
