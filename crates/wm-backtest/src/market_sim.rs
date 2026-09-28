//! Strategies A and B replayed at the prices the market actually traded
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

use crate::forecast_eval::ratio_ci;
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
        }
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
    fn pool_mid(&self, max_spread: f64) -> Option<f64> {
        match (self.yes_ask, self.yes_bid) {
            (Some(a), Some(b)) if (a - b).abs() <= max_spread + 1e-12 => Some((a + b) / 2.0),
            _ => None,
        }
    }
}

/// One decision of a market day: the features at a report and each
/// structure's distribution (trained on earlier days), with every bucket's
/// quote at the decision time.
#[derive(Debug, Clone)]
pub(crate) struct Decision {
    /// Observation time of the report.
    pub(crate) at: DateTime<Utc>,
    pub(crate) f: PeakFeatures,
    /// Current and candidate structure ([`STRUCTURES`]).
    pub(crate) dists: [Option<IncrementDistribution>; 2],
    pub(crate) quotes: Vec<Quote>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// YES on the bucket that holds the high.
    A,
    /// NO on the buckets above it.
    B,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::A => "A",
            Kind::B => "B",
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
    /// Trades the replayed variants took at this report.
    pub trades: Vec<String>,
}

/// A replayed day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayTimeline {
    pub date: NaiveDate,
    pub winner: String,
    pub rows: Vec<TimelineRow>,
}

fn local_hm(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

fn range_label((lo, hi): (f64, f64)) -> String {
    format!("{lo:.2}–{hi:.2}")
}

/// Buckets a strategy trades at a high.
fn buckets_for(
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
                            });
                        }
                    }
                }
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
                    let label = range_label(range);
                    let ts: Vec<&SimTrade> = trades
                        .iter()
                        .filter(|t| {
                            t.structure == structure
                                && t.strategy == kind.label()
                                && t.window == window
                                && t.range == label
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
                    let total: f64 = ts.iter().map(|t| t.pnl_usd).sum();
                    rows.push(StrategyRow {
                        structure: structure.to_owned(),
                        strategy: kind.label().to_owned(),
                        window,
                        range: label,
                        live: window == sim.live_window
                            && (range.0 - sim.live_range.0).abs() < 1e-9
                            && (range.1 - sim.live_range.1).abs() < 1e-9,
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
                    });
                }
            }
        }
    }
    rows
}

/// Plain-language conclusions of the replay.
pub(crate) fn verdict(rows: &[StrategyRow], sim: &MarketSimConfig) -> Vec<String> {
    let mut v = Vec::new();
    if rows.iter().all(|r| r.trades == 0) {
        v.push("Strategies at traded prices: no replayed variant found a trade — the model never saw enough edge at the prices the market traded.".into());
        return v;
    }
    for structure in STRUCTURES {
        let live: Vec<&StrategyRow> = rows
            .iter()
            .filter(|r| r.structure == structure && r.live)
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
            }
        })
        .collect();
    DayTimeline {
        date,
        winner: winner.to_owned(),
        rows,
    }
}

/// Trades of one report, variants that made the same trade on one line:
/// `B · current · 0′/30′/60′ · 0.90–0.99/0.70–0.99: NO 22°C @ 0.982 (p 0.998) → won +0.12 $`.
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
            format!(
                "{} · {} · {} · {}: {} {} @ {:.3} (p {:.3}) → {} {:+.2} $",
                t.strategy,
                t.structure,
                windows
                    .iter()
                    .map(|w| format!("{w}′"))
                    .collect::<Vec<_>>()
                    .join("/"),
                ranges.join("/"),
                t.side,
                t.bucket,
                t.price,
                t.p_used,
                if t.won { "won" } else { "lost" },
                t.pnl_usd
            )
        })
        .collect()
}

pub(crate) fn strategies_markdown(
    rows: &[StrategyRow],
    sim: &MarketSimConfig,
    verdict: &[String],
) -> String {
    let mut s = format!(
        "\n## Strategies at traded prices\n\nStrategies A (YES on the bucket holding the high) and B (NO on the buckets {} above it) replayed at every report with each structure's prequential model. Ask = the latest taker buy of YES (A) or one minus the latest taker sell of YES (B); filled at that price plus {:.3} slippage, taker fee paid, ${:.0} per trade; the model is pooled with the traded midpoint and capped at the model, as live. At most one trade per day, bucket and variant. Depth is unknown, so fills are optimistic in thin markets. **live** marks the configured rule.\n\n",
        sim.no_distances
            .iter()
            .map(|k| format!("+{k}"))
            .collect::<Vec<_>>()
            .join("/"),
        sim.slippage,
        sim.stake_usd
    );
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
    s
}

pub(crate) fn timeline_markdown(t: &DayTimeline, delay_s: i64) -> String {
    let c = |v: Option<f64>, d: usize| v.map_or_else(|| "—".to_owned(), |x| format!("{x:.d$}"));
    let signed = |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |x| format!("{x:+.1}"));
    let mut s = format!(
        "\n## Day replay — {} (resolved {})\n\nEvery report from the first decision time; decisions {delay_s} s after the report. P(high) = the model's probability that the final high stays in the bucket holding the current high; market = that bucket's traded midpoint, ask = its latest taker buy.\n\n| report | temp | high | since last / first touch | rise / headroom | P(high) current | P(high) candidate | market | ask | trades |\n|---|---:|---:|---|---|---:|---:|---:|---:|---|\n",
        t.date, t.winner
    );
    for r in &t.rows {
        let _ = writeln!(
            s,
            "| {} | {:.1} | {} | {} / {} min | {} / {} | {} | {} | {} | {} | {} |",
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
        }
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
