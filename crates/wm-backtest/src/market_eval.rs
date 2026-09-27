//! Model versus market on settled Polymarket days — measured, not assumed.
//!
//! The station's METAR history is replayed **prequentially**: every day is
//! scored with the model as trained on the days before it (the same state,
//! peak and model code as live trading), then learned. On days with a settled
//! market, at each report from `first_decision_minute` on, the model's
//! probability for every bucket that can still win is compared with the
//! market's at the moment the bot would act (observation + knowledge delay).
//!
//! What it answers:
//! * **Who predicts better** — log loss (and Brier score) of the model, the
//!   market and logarithmic pools of both, with 95 % day-block bootstrap
//!   intervals of the difference to the market; overall and where the
//!   strategies trade (prices 0.90–0.99 or 0.01–0.10). The best weight is the
//!   evidence for `market_weight`.
//! * **Where prices are wrong** — win rate by market price, and what happened
//!   when the model disagreed with the market.
//! * **Who is sure first** — when the winning bucket first reached 90/95/99 %
//!   in the market and in the model.
//! * **How fast dead buckets reprice** — after a report raises the high,
//!   trades that still sold the dead buckets' YES (bought their NO) at stale
//!   prices, by delay after the observation: the window strategy D needs, and
//!   how much of it faster traders took before our knowledge time.
//! * **Whether the METAR high is the resolution** — the observed high against
//!   the resolved bucket.
//!
//! Market prices come from executed trades, not quotes: the midpoint of the
//! latest taker buy and taker sell of YES (ask and bid proxies), each at most
//! `max_price_age` old. Stale trades only show liquidity someone took — a
//! lower bound on what was offered.

use crate::forecast_eval::{ForecastHistory, ratio_ci};
use crate::research::wilson;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use wm_core::ids::StationId;
use wm_core::market::TemperatureBucket;
use wm_core::time::{local_date, local_day_bounds};
use wm_core::weather::Observation;
use wm_strategy::{
    EmpiricalPeakModel, PeakConfig, PeakDetectionEngine, ProbabilityModel, TemperatureStateEngine,
    ViewKind, log_pool,
};

/// Probabilities are clamped to `[P_FLOOR, 1 − P_FLOOR]` before scoring:
/// prices move in ticks of at least 0.001, and one confident miss must not
/// dominate an average.
pub const P_FLOOR: f64 = 0.001;

/// Win-probability thresholds of the crossing-time comparison.
pub const CROSSING_THRESHOLDS: [f64; 3] = [0.90, 0.95, 0.99];

/// One executed trade, in YES terms.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketTrade {
    pub at: DateTime<Utc>,
    /// Index into [`MarketDay::buckets`].
    pub bucket: usize,
    /// Price of YES (a NO trade at `q` is a YES trade at `1 − q`).
    pub yes_price: f64,
    /// The taker gained YES exposure (bought YES or sold NO).
    pub taker_buys_yes: bool,
    pub shares: f64,
    /// Taker identity, used only to count distinct traders.
    pub taker: Option<String>,
}

/// A settled market day.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketDay {
    pub date: NaiveDate,
    pub event_slug: String,
    pub buckets: Vec<TemperatureBucket>,
    pub labels: Vec<String>,
    /// Index of the bucket that resolved YES.
    pub winner: usize,
    /// All buckets' trades, oldest first.
    pub trades: Vec<MarketTrade>,
    /// The trade history may be incomplete (source cap reached).
    pub truncated: bool,
}

/// Study settings (fixed before looking at results).
#[derive(Debug, Clone)]
pub struct MarketStudyConfig {
    pub station: StationId,
    pub tz: Tz,
    pub peak: PeakConfig,
    /// Increment classes of the model (as in training).
    pub k_classes: usize,
    /// The bot acts on a report this long after its observation time.
    pub knowledge_delay: Duration,
    /// Trades older than this at a decision do not price it.
    pub max_price_age: Duration,
    /// Smaller trades (dust) do not price a bucket.
    pub min_trade_shares: f64,
    /// Decisions from this local minute on.
    pub first_decision_minute: u16,
    /// Model cells with less support are skipped, as the strategies do.
    pub min_model_support: u32,
    /// Market weights of the scored pools (0 = model, 1 = market; both are
    /// always included).
    pub weights: Vec<f64>,
    /// The weight live trading uses (marked in the report).
    pub configured_weight: f64,
    pub taker_fee_rate: f64,
    /// A dead bucket's YES sold at this price or more is a stale quote taken.
    pub stale_min_price: f64,
    /// How long after a new high trades on the dead buckets are inspected.
    pub latency_window: Duration,
    pub bootstrap_iterations: usize,
    pub seed: u64,
}

impl MarketStudyConfig {
    pub fn new(station: StationId, tz: Tz, peak: PeakConfig) -> Self {
        Self {
            station,
            tz,
            peak,
            k_classes: 4,
            knowledge_delay: Duration::minutes(5),
            max_price_age: Duration::minutes(60),
            min_trade_shares: 1.0,
            first_decision_minute: 9 * 60,
            min_model_support: 50,
            weights: vec![0.0, 0.25, 0.5, 0.75, 1.0],
            configured_weight: 0.5,
            taker_fee_rate: 0.05,
            stale_min_price: 0.02,
            latency_window: Duration::minutes(30),
            bootstrap_iterations: 2_000,
            seed: 0x4D41_524B_4554,
        }
    }

    fn weights(&self) -> Vec<f64> {
        let mut w: Vec<f64> = self
            .weights
            .iter()
            .map(|w| w.clamp(0.0, 1.0))
            .chain([0.0, 1.0])
            .collect();
        w.sort_by(f64::total_cmp);
        w.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        w
    }
}

/// Scores of one predictor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreRow {
    /// "model", "market" or "pool w=…".
    pub name: String,
    /// Weight of the market (0 = model, 1 = market).
    pub weight: f64,
    pub log_loss: f64,
    pub brier: f64,
    /// Mean log loss minus the market's (negative = better than the market).
    pub diff_vs_market: f64,
    pub diff_ci_low: f64,
    pub diff_ci_high: f64,
}

/// Scores on one subset of decision points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreTable {
    pub subset: String,
    pub points: u64,
    pub days: u64,
    pub rows: Vec<ScoreRow>,
    /// Weight with the lowest log loss.
    pub best_weight: Option<f64>,
}

/// Outcomes grouped by market price or by disagreement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRow {
    pub group: String,
    pub points: u64,
    pub market_mean: f64,
    pub model_mean: f64,
    pub win_rate: f64,
    pub ci_low: f64,
    pub ci_high: f64,
}

/// When the winning bucket first reached a threshold, per day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossingDay {
    pub date: NaiveDate,
    pub winner: String,
    pub threshold: f64,
    /// Local time of the first decision at or above the threshold.
    pub market_at: Option<String>,
    pub model_at: Option<String>,
    /// Model minus market, minutes (positive = the market was sure first).
    pub model_lag_minutes: Option<i64>,
}

/// Crossing times over all days for one threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossingSummary {
    pub threshold: f64,
    pub days: u64,
    pub both: u64,
    pub market_first: u64,
    pub model_first: u64,
    pub same_time: u64,
    pub market_only: u64,
    pub model_only: u64,
    /// Median of model minus market over days where both crossed.
    pub median_model_lag_minutes: Option<f64>,
}

/// Stale-quote trades after new highs, by delay after the observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyBin {
    pub label: String,
    pub trades: u64,
    pub shares: f64,
    /// Seller's profit after the taker fee.
    pub profit_usd: f64,
}

/// One report that raised the high and killed buckets the market still priced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyEvent {
    pub date: NaiveDate,
    pub local_time: String,
    pub from_high: i32,
    pub to_high: i32,
    pub dead_buckets: Vec<String>,
    pub stale_trades: u64,
    /// Seconds from the observation to the first stale trade at or after it.
    pub first_stale_after_s: Option<i64>,
    /// Stale profit taken before / after our knowledge time.
    pub profit_before_knowledge_usd: f64,
    pub profit_after_knowledge_usd: f64,
    pub distinct_takers: u64,
}

/// The study's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketStudyReport {
    pub station: String,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub history_days: u64,
    pub market_days: u64,
    pub scored_days: u64,
    /// Market days that could not be scored, with the reason.
    pub skipped_days: Vec<(NaiveDate, String)>,
    pub truncated_days: u64,
    pub resolution_checked: u64,
    pub resolution_agreed: u64,
    /// Days whose METAR high was outside the resolved bucket.
    pub resolution_mismatches: Vec<String>,
    pub points: u64,
    pub skipped_no_price: u64,
    pub skipped_ambiguous: u64,
    pub skipped_support: u64,
    pub knowledge_delay_s: i64,
    pub max_price_age_min: i64,
    pub configured_weight: f64,
    pub scores: Vec<ScoreTable>,
    pub calibration: Vec<OutcomeRow>,
    pub disagreement: Vec<OutcomeRow>,
    pub crossings: Vec<CrossingSummary>,
    pub crossing_days: Vec<CrossingDay>,
    pub latency_events_total: u64,
    pub latency_bins: Vec<LatencyBin>,
    pub latency_events: Vec<LatencyEvent>,
    /// Plain-language conclusions.
    pub verdict: Vec<String>,
}

/// One scored (decision, bucket).
#[derive(Debug, Clone, Copy)]
struct Point {
    day: usize,
    at: DateTime<Utc>,
    bucket: usize,
    model: f64,
    market: f64,
    won: bool,
}

impl Point {
    /// Where the strategies trade: YES at 0.90–0.99 (A) or 0.01–0.10 (B).
    fn in_trading_region(&self) -> bool {
        (0.90..=0.99).contains(&self.market) || (0.01..=0.10).contains(&self.market)
    }
}

fn clamp_p(p: f64) -> f64 {
    p.clamp(P_FLOOR, 1.0 - P_FLOOR)
}

fn log_loss(p: f64, won: bool) -> f64 {
    let p = clamp_p(p);
    if won { -p.ln() } else { -(1.0 - p).ln() }
}

fn brier(p: f64, won: bool) -> f64 {
    let y = if won { 1.0 } else { 0.0 };
    (clamp_p(p) - y).powi(2)
}

fn local_hm(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

/// Market probability of YES at `at` from one bucket's trades (oldest first).
fn market_price(trades: &[&MarketTrade], at: DateTime<Utc>, max_age: Duration) -> Option<f64> {
    let end = trades.partition_point(|t| t.at <= at);
    let (mut buy, mut sell) = (None, None);
    for t in trades[..end].iter().rev() {
        if at - t.at > max_age {
            break;
        }
        if t.taker_buys_yes {
            buy.get_or_insert(t.yes_price);
        } else {
            sell.get_or_insert(t.yes_price);
        }
        if buy.is_some() && sell.is_some() {
            break;
        }
    }
    match (buy, sell) {
        (Some(a), Some(b)) => Some((a + b) / 2.0),
        (a, b) => a.or(b),
    }
}

/// Delay bins of the latency table (seconds after the observation).
const LATENCY_BINS: [(&str, i64, i64); 6] = [
    (
        "before the report (−10…0 min: anticipation or ordinary selling)",
        -600,
        0,
    ),
    ("0–1 min", 0, 60),
    ("1–2 min", 60, 120),
    ("2–5 min", 120, 300),
    ("5–10 min", 300, 600),
    ("10–30 min", 600, 1_800),
];

/// Run the study. `forecasts` (when the live model uses the forecast) must
/// cover the history as in training; `None` evaluates the model without it.
pub fn market_study(
    observations: &[Observation],
    forecasts: Option<&ForecastHistory>,
    days: &[MarketDay],
    cfg: &MarketStudyConfig,
) -> MarketStudyReport {
    let view = ViewKind::All;
    let mut engine = TemperatureStateEngine::new(3);
    engine.register_station(cfg.station.clone(), cfg.tz);
    let peak = PeakDetectionEngine::new(cfg.peak.clone());
    let levels = if forecasts.is_some() {
        EmpiricalPeakModel::with_forecast_refinement(EmpiricalPeakModel::default_levels())
    } else {
        EmpiricalPeakModel::default_levels()
    };
    let mut model = EmpiricalPeakModel::new(
        format!("prequential-{}", cfg.station),
        cfg.station.to_string(),
        view.label(),
        cfg.k_classes,
        levels,
    );
    let markets: HashMap<NaiveDate, &MarketDay> = days.iter().map(|d| (d.date, d)).collect();
    let mut by_day: BTreeMap<NaiveDate, Vec<&Observation>> = BTreeMap::new();
    for o in observations.iter().filter(|o| o.key.station == cfg.station) {
        by_day
            .entry(local_date(o.key.observed_at, cfg.tz))
            .or_default()
            .push(o);
    }

    let mut report = MarketStudyReport {
        station: cfg.station.to_string(),
        from: None,
        to: None,
        history_days: 0,
        market_days: days.len() as u64,
        scored_days: 0,
        skipped_days: Vec::new(),
        truncated_days: days.iter().filter(|d| d.truncated).count() as u64,
        resolution_checked: 0,
        resolution_agreed: 0,
        resolution_mismatches: Vec::new(),
        points: 0,
        skipped_no_price: 0,
        skipped_ambiguous: 0,
        skipped_support: 0,
        knowledge_delay_s: cfg.knowledge_delay.num_seconds(),
        max_price_age_min: cfg.max_price_age.num_minutes(),
        configured_weight: cfg.configured_weight,
        scores: Vec::new(),
        calibration: Vec::new(),
        disagreement: Vec::new(),
        crossings: Vec::new(),
        crossing_days: Vec::new(),
        latency_events_total: 0,
        latency_bins: LATENCY_BINS
            .iter()
            .map(|(l, _, _)| LatencyBin {
                label: (*l).to_owned(),
                trades: 0,
                shares: 0.0,
                profit_usd: 0.0,
            })
            .collect(),
        latency_events: Vec::new(),
        verdict: Vec::new(),
    };
    let mut points: Vec<Point> = Vec::new();
    let mut scored_dates: Vec<NaiveDate> = Vec::new();
    let mut seen_market: HashSet<NaiveDate> = HashSet::new();

    for (date, obs) in &by_day {
        for o in obs {
            engine.apply_observation(o);
        }
        let (_, end) = local_day_bounds(*date, cfg.tz);
        let Some(final_state) = engine.day_state(&cfg.station, *date, view, end) else {
            continue;
        };
        let Some(final_high) = final_state.high.map(|h| h.value.round_half_up_whole()) else {
            continue;
        };
        report.history_days += 1;
        let forecast = forecasts.and_then(|h| h.days.get(date));
        let times: Vec<DateTime<Utc>> = final_state.points.iter().map(|p| p.observed_at).collect();
        let mut samples = Vec::with_capacity(times.len());
        // (time, features) of the day, in order.
        let mut states = Vec::with_capacity(times.len());
        for &t in &times {
            let Some(s) = engine.day_state(&cfg.station, *date, view, t) else {
                continue;
            };
            let Some(a) = peak.assess_with(&s, cfg.tz, t, forecast) else {
                continue;
            };
            samples.push((a.features.clone(), final_high - a.features.high_whole));
            states.push((t, a.features));
        }

        if let Some(md) = markets.get(date) {
            seen_market.insert(*date);
            if md.winner >= md.buckets.len() || md.labels.len() != md.buckets.len() {
                report
                    .skipped_days
                    .push((*date, "market without a valid winner".into()));
            } else {
                report.resolution_checked += 1;
                if md.buckets[md.winner].contains(final_high) {
                    report.resolution_agreed += 1;
                } else {
                    report.resolution_mismatches.push(format!(
                        "{date}: METAR high {final_high} °C, resolved {}",
                        md.labels[md.winner]
                    ));
                }
                let day_index = scored_dates.len();
                let before = points.len();
                score_day(
                    md,
                    day_index,
                    &states,
                    &model,
                    cfg,
                    &mut points,
                    &mut report,
                );
                latency(md, &states, cfg, &mut report);
                if points.len() > before {
                    scored_dates.push(*date);
                } else {
                    report
                        .skipped_days
                        .push((*date, "no decision point with a market price".into()));
                }
            }
        }

        // Learn the day only after scoring it.
        for (f, increment) in &samples {
            model.observe(f, *increment);
        }
        engine.prune(*date);
    }
    for d in days {
        if !seen_market.contains(&d.date) {
            report
                .skipped_days
                .push((d.date, "no METAR history for the day".into()));
        }
    }
    report.skipped_days.sort();
    report.from = by_day.keys().next().copied();
    report.to = by_day.keys().next_back().copied();
    report.scored_days = scored_dates.len() as u64;
    report.points = points.len() as u64;

    let n_days = scored_dates.len();
    let all: Vec<&Point> = points.iter().collect();
    let trading: Vec<&Point> = points.iter().filter(|p| p.in_trading_region()).collect();
    report.scores = vec![
        score_table("every bucket that could still win", &all, n_days, cfg),
        score_table(
            "where the strategies trade (market 0.90–0.99 or 0.01–0.10)",
            &trading,
            n_days,
            cfg,
        ),
    ];
    report.calibration = calibration(&all);
    report.disagreement = disagreement(&all);
    let (summary, per_day) = crossings(&points, days, &scored_dates, cfg.tz);
    report.crossings = summary;
    report.crossing_days = per_day;
    report
        .latency_events
        .sort_by(|a, b| (a.date, &a.local_time).cmp(&(b.date, &b.local_time)));
    report.verdict = verdict(&report);
    report
}

/// Score every still-possible bucket at each decision of one market day.
#[allow(clippy::too_many_arguments)]
fn score_day(
    md: &MarketDay,
    day_index: usize,
    states: &[(DateTime<Utc>, wm_strategy::PeakFeatures)],
    model: &EmpiricalPeakModel,
    cfg: &MarketStudyConfig,
    points: &mut Vec<Point>,
    report: &mut MarketStudyReport,
) {
    let per_bucket = trades_by_bucket(md, cfg.min_trade_shares);
    for (t, f) in states {
        if f.local_minute_now < cfg.first_decision_minute {
            continue;
        }
        let Some(dist) = model.distribution(f) else {
            continue;
        };
        let knowledge = *t + cfg.knowledge_delay;
        for (i, bucket) in md.buckets.iter().enumerate() {
            if bucket.upper.is_some_and(|u| u < f.high_whole) {
                continue; // already decided by the observations
            }
            if dist.support < cfg.min_model_support {
                report.skipped_support += 1;
                continue;
            }
            let lower = dist.p_in_bucket_lower(f.high_whole, bucket);
            let upper = dist.p_in_bucket_upper(f.high_whole, bucket);
            if (upper - lower).abs() > 1e-9 {
                report.skipped_ambiguous += 1;
                continue;
            }
            let Some(market) = market_price(&per_bucket[i], knowledge, cfg.max_price_age) else {
                report.skipped_no_price += 1;
                continue;
            };
            points.push(Point {
                day: day_index,
                at: *t,
                bucket: i,
                model: lower,
                market,
                won: i == md.winner,
            });
        }
    }
}

fn trades_by_bucket(md: &MarketDay, min_shares: f64) -> Vec<Vec<&MarketTrade>> {
    let mut per: Vec<Vec<&MarketTrade>> = vec![Vec::new(); md.buckets.len()];
    for t in &md.trades {
        if t.bucket < per.len() && t.shares >= min_shares {
            per[t.bucket].push(t);
        }
    }
    for v in &mut per {
        v.sort_by_key(|t| t.at);
    }
    per
}

/// Stale-quote trades on the buckets each new high killed.
fn latency(
    md: &MarketDay,
    states: &[(DateTime<Utc>, wm_strategy::PeakFeatures)],
    cfg: &MarketStudyConfig,
    report: &mut MarketStudyReport,
) {
    // Every trade prices a bucket here, dust included: a dust trade at a
    // stale price is still a stale quote taken.
    let per_bucket = trades_by_bucket(md, 0.0);
    for pair in states.windows(2) {
        let (prev, (t, f)) = (&pair[0].1, &pair[1]);
        if f.high_whole <= prev.high_whole {
            continue;
        }
        // Buckets the new high killed that the market still priced as alive.
        let dead: Vec<usize> = md
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.upper
                    .is_some_and(|u| u >= prev.high_whole && u < f.high_whole)
            })
            .map(|(i, _)| i)
            .filter(|&i| {
                market_price(&per_bucket[i], *t, cfg.max_price_age)
                    .is_some_and(|p| p >= cfg.stale_min_price)
            })
            .collect();
        if dead.is_empty() {
            continue;
        }
        report.latency_events_total += 1;
        let knowledge = *t + cfg.knowledge_delay;
        let mut ev = LatencyEvent {
            date: md.date,
            local_time: local_hm(*t, cfg.tz),
            from_high: prev.high_whole,
            to_high: f.high_whole,
            dead_buckets: dead.iter().map(|&i| md.labels[i].clone()).collect(),
            stale_trades: 0,
            first_stale_after_s: None,
            profit_before_knowledge_usd: 0.0,
            profit_after_knowledge_usd: 0.0,
            distinct_takers: 0,
        };
        let mut takers: HashSet<&str> = HashSet::new();
        let from = *t - Duration::minutes(10);
        let to = *t + cfg.latency_window;
        for &i in &dead {
            for tr in per_bucket[i]
                .iter()
                .filter(|tr| tr.at >= from && tr.at <= to)
            {
                if tr.taker_buys_yes || tr.yes_price < cfg.stale_min_price {
                    continue;
                }
                let p = tr.yes_price;
                let profit = tr.shares * (p - cfg.taker_fee_rate * p * (1.0 - p));
                let after_s = (tr.at - *t).num_seconds();
                if let Some(bin) = LATENCY_BINS
                    .iter()
                    .position(|(_, lo, hi)| after_s >= *lo && after_s < *hi)
                    .or_else(|| (after_s == 1_800).then_some(LATENCY_BINS.len() - 1))
                {
                    let b = &mut report.latency_bins[bin];
                    b.trades += 1;
                    b.shares += tr.shares;
                    b.profit_usd += profit;
                }
                if after_s < 0 {
                    continue; // the bucket was still alive: only in the table
                }
                ev.stale_trades += 1;
                ev.first_stale_after_s = Some(
                    ev.first_stale_after_s
                        .map_or(after_s, |s: i64| s.min(after_s)),
                );
                if tr.at < knowledge {
                    ev.profit_before_knowledge_usd += profit;
                } else {
                    ev.profit_after_knowledge_usd += profit;
                }
                if let Some(who) = tr.taker.as_deref() {
                    takers.insert(who);
                }
            }
        }
        ev.distinct_takers = takers.len() as u64;
        report.latency_events.push(ev);
    }
}

fn score_table(subset: &str, pts: &[&Point], n_days: usize, cfg: &MarketStudyConfig) -> ScoreTable {
    let weights = cfg.weights();
    let pool = |p: &Point, w: f64| log_pool(p.model, Some(p.market), w);
    let n = pts.len() as f64;
    let days_with: HashSet<usize> = pts.iter().map(|p| p.day).collect();
    let mut rows = Vec::with_capacity(weights.len());
    for &w in &weights {
        let (mut ll, mut br) = (0.0, 0.0);
        // Per day: (Σ log loss − Σ market log loss, points).
        let mut per_day = vec![(0.0, 0.0); n_days.max(1)];
        for p in pts {
            let q = pool(p, w);
            let l = log_loss(q, p.won);
            ll += l;
            br += brier(q, p.won);
            let d = &mut per_day[p.day];
            d.0 += l - log_loss(p.market, p.won);
            d.1 += 1.0;
        }
        let per_day: Vec<(f64, f64)> = per_day.into_iter().filter(|d| d.1 > 0.0).collect();
        let diff = if n > 0.0 {
            per_day.iter().map(|d| d.0).sum::<f64>() / n
        } else {
            0.0
        };
        let (lo, hi) = if (w - 1.0).abs() < 1e-9 {
            (0.0, 0.0)
        } else {
            ratio_ci(&per_day, cfg.bootstrap_iterations, cfg.seed)
        };
        rows.push(ScoreRow {
            name: if w == 0.0 {
                "model".to_owned()
            } else if (w - 1.0).abs() < 1e-9 {
                "market".to_owned()
            } else {
                format!("pool w={w:.2}")
            },
            weight: w,
            log_loss: if n > 0.0 { ll / n } else { 0.0 },
            brier: if n > 0.0 { br / n } else { 0.0 },
            diff_vs_market: diff,
            diff_ci_low: lo,
            diff_ci_high: hi,
        });
    }
    let best_weight = (!pts.is_empty())
        .then(|| {
            rows.iter()
                .min_by(|a, b| a.log_loss.total_cmp(&b.log_loss))
                .map(|r| r.weight)
        })
        .flatten();
    ScoreTable {
        subset: subset.to_owned(),
        points: pts.len() as u64,
        days: days_with.len() as u64,
        rows,
        best_weight,
    }
}

fn outcome_row(group: String, pts: &[&Point]) -> OutcomeRow {
    let n = pts.len() as u64;
    let wins = pts.iter().filter(|p| p.won).count() as u64;
    let mean = |f: fn(&Point) -> f64| {
        if pts.is_empty() {
            0.0
        } else {
            pts.iter().map(|p| f(p)).sum::<f64>() / pts.len() as f64
        }
    };
    let (lo, hi) = wilson(wins, n);
    OutcomeRow {
        group,
        points: n,
        market_mean: mean(|p| p.market),
        model_mean: mean(|p| p.model),
        win_rate: if n == 0 { 0.0 } else { wins as f64 / n as f64 },
        ci_low: lo,
        ci_high: hi,
    }
}

/// Win rate by market price: where the prices are mis-calibrated.
fn calibration(pts: &[&Point]) -> Vec<OutcomeRow> {
    const EDGES: [f64; 8] = [0.0, 0.02, 0.10, 0.30, 0.70, 0.90, 0.98, 1.0];
    EDGES
        .windows(2)
        .map(|e| {
            let last = (e[1] - 1.0).abs() < 1e-12;
            let group: Vec<&Point> = pts
                .iter()
                .copied()
                .filter(|p| p.market >= e[0] && (p.market < e[1] || (last && p.market <= e[1])))
                .collect();
            outcome_row(format!("{:.2}–{:.2}", e[0], e[1]), &group)
        })
        .collect()
}

/// What happened when the model disagreed with the market by ≥ 5 points.
fn disagreement(pts: &[&Point]) -> Vec<OutcomeRow> {
    let pick =
        |f: &dyn Fn(&Point) -> bool| pts.iter().copied().filter(|p| f(p)).collect::<Vec<_>>();
    vec![
        outcome_row(
            "model ≥ 5 pp above the market".into(),
            &pick(&|p| p.model - p.market >= 0.05),
        ),
        outcome_row(
            "within 5 pp".into(),
            &pick(&|p| (p.model - p.market).abs() < 0.05),
        ),
        outcome_row(
            "model ≥ 5 pp below the market".into(),
            &pick(&|p| p.market - p.model >= 0.05),
        ),
    ]
}

/// First decision at which the winning bucket reached each threshold.
fn crossings(
    points: &[Point],
    days: &[MarketDay],
    scored_dates: &[NaiveDate],
    tz: Tz,
) -> (Vec<CrossingSummary>, Vec<CrossingDay>) {
    let by_date: HashMap<NaiveDate, &MarketDay> = days.iter().map(|d| (d.date, d)).collect();
    // The winning bucket's points of each scored day, in time order.
    let mut winners: Vec<Vec<&Point>> = vec![Vec::new(); scored_dates.len()];
    for p in points {
        let won = scored_dates
            .get(p.day)
            .and_then(|d| by_date.get(d))
            .is_some_and(|md| md.winner == p.bucket);
        if won {
            winners[p.day].push(p);
        }
    }
    for w in &mut winners {
        w.sort_by_key(|p| p.at);
    }
    let mut per_day = Vec::new();
    let mut summary = Vec::new();
    for &th in &CROSSING_THRESHOLDS {
        let mut s = CrossingSummary {
            threshold: th,
            days: 0,
            both: 0,
            market_first: 0,
            model_first: 0,
            same_time: 0,
            market_only: 0,
            model_only: 0,
            median_model_lag_minutes: None,
        };
        let mut lags = Vec::new();
        for (day, date) in scored_dates.iter().enumerate() {
            let (Some(md), winner) = (by_date.get(date), &winners[day]) else {
                continue;
            };
            if winner.is_empty() {
                continue;
            }
            s.days += 1;
            let market_at = winner.iter().find(|p| p.market >= th).map(|p| p.at);
            let model_at = winner.iter().find(|p| p.model >= th).map(|p| p.at);
            let lag = match (market_at, model_at) {
                (Some(a), Some(b)) => {
                    s.both += 1;
                    let l = (b - a).num_minutes();
                    match l.cmp(&0) {
                        std::cmp::Ordering::Greater => s.market_first += 1,
                        std::cmp::Ordering::Less => s.model_first += 1,
                        std::cmp::Ordering::Equal => s.same_time += 1,
                    }
                    lags.push(l as f64);
                    Some(l)
                }
                (Some(_), None) => {
                    s.market_only += 1;
                    None
                }
                (None, Some(_)) => {
                    s.model_only += 1;
                    None
                }
                (None, None) => None,
            };
            per_day.push(CrossingDay {
                date: *date,
                winner: md.labels[md.winner].clone(),
                threshold: th,
                market_at: market_at.map(|t| local_hm(t, tz)),
                model_at: model_at.map(|t| local_hm(t, tz)),
                model_lag_minutes: lag,
            });
        }
        lags.sort_by(f64::total_cmp);
        s.median_model_lag_minutes = (!lags.is_empty()).then(|| {
            let m = lags.len() / 2;
            if lags.len() % 2 == 1 {
                lags[m]
            } else {
                (lags[m - 1] + lags[m]) / 2.0
            }
        });
        summary.push(s);
    }
    (summary, per_day)
}

fn verdict(r: &MarketStudyReport) -> Vec<String> {
    let mut v = Vec::new();
    if r.scored_days == 0 {
        v.push(
            "No market day could be scored: nothing can be concluded about model versus market."
                .into(),
        );
    } else if r.scored_days < 30 {
        v.push(format!(
            "Only {} scored days: treat every number below as a first look, not evidence.",
            r.scored_days
        ));
    }
    if let Some(all) = r.scores.first().filter(|t| t.points > 0)
        && let Some(model) = all.rows.iter().find(|x| x.weight == 0.0)
    {
        let text = if model.diff_ci_high < 0.0 {
            format!(
                "The model predicted better than the market ({:+.4} log loss per decision, 95% CI {:+.4} … {:+.4}).",
                model.diff_vs_market, model.diff_ci_low, model.diff_ci_high
            )
        } else if model.diff_ci_low > 0.0 {
            format!(
                "The market predicted better than the model ({:+.4} log loss per decision for the model, 95% CI {:+.4} … {:+.4}): its prices already contain what the model knows, and more.",
                model.diff_vs_market, model.diff_ci_low, model.diff_ci_high
            )
        } else {
            format!(
                "No clear difference between model and market ({:+.4} log loss per decision for the model, 95% CI {:+.4} … {:+.4}).",
                model.diff_vs_market, model.diff_ci_low, model.diff_ci_high
            )
        };
        v.push(text);
        if let Some(best) = all.best_weight {
            let advice = if (best - 1.0).abs() < 1e-9 {
                "the market alone predicts best — the model adds nothing the prices do not already contain, so strategies A and B have no measurable information edge".to_owned()
            } else if best == 0.0 {
                "the model alone predicts best — the market adds nothing to it".to_owned()
            } else {
                format!("combining both predicts best (market weight {best:.2})")
            };
            v.push(format!(
                "Best pool: {advice}; live trading uses market_weight {:.2}.",
                r.configured_weight
            ));
        }
    }
    if let Some(s) = r
        .crossings
        .iter()
        .find(|c| (c.threshold - 0.95).abs() < 1e-9)
        && s.both > 0
    {
        v.push(format!(
            "Winning bucket at 95%: the market got there first on {} of {} days, the model on {}; median model lag {:+.0} min.",
            s.market_first,
            s.both,
            s.model_first,
            s.median_model_lag_minutes.unwrap_or(0.0)
        ));
    }
    let before: f64 = r
        .latency_events
        .iter()
        .map(|e| e.profit_before_knowledge_usd)
        .sum();
    let after: f64 = r
        .latency_events
        .iter()
        .map(|e| e.profit_after_knowledge_usd)
        .sum();
    if r.latency_events_total > 0 {
        let mut firsts: Vec<i64> = r
            .latency_events
            .iter()
            .filter_map(|e| e.first_stale_after_s)
            .collect();
        firsts.sort_unstable();
        let median = firsts.get(firsts.len() / 2).copied();
        v.push(format!(
            "Dead buckets after {} new highs: stale quotes worth ${before:.2} were taken before our knowledge time (observation + {} s) and ${after:.2} after it{}.",
            r.latency_events_total,
            r.knowledge_delay_s,
            median.map_or_else(String::new, |m| format!("; the first stale trade came a median {m} s after the observation"))
        ));
    } else {
        v.push(
            "No new high killed a bucket the market still priced: no stale quotes to take.".into(),
        );
    }
    if r.resolution_checked > 0 {
        v.push(format!(
            "The METAR high matched the resolved bucket on {} of {} days.",
            r.resolution_agreed, r.resolution_checked
        ));
    }
    v
}

impl MarketStudyReport {
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "# Model versus market — {}\n\n{} settled market days ({} scored), METAR history {} → {} ({} days, prequential: each day scored with the model trained on the days before it). Decisions at every report from the configured local time on; the bot acts at observation + {} s; market price = midpoint of the latest taker buy and sell of YES within {} min.\n\n",
            self.station,
            self.market_days,
            self.scored_days,
            self.from.map(|d| d.to_string()).unwrap_or_default(),
            self.to.map(|d| d.to_string()).unwrap_or_default(),
            self.history_days,
            self.knowledge_delay_s,
            self.max_price_age_min,
        );
        s.push_str("## Verdict\n\n");
        for line in &self.verdict {
            let _ = writeln!(s, "* {line}");
        }
        let _ = writeln!(
            s,
            "\n{} decision points (bucket × report). Skipped: {} without a recent market price, {} with an ambiguous tail probability, {} below the model's minimum support.{}",
            self.points,
            self.skipped_no_price,
            self.skipped_ambiguous,
            self.skipped_support,
            if self.truncated_days > 0 {
                format!(
                    " {} days' trade history may be incomplete.",
                    self.truncated_days
                )
            } else {
                String::new()
            }
        );
        s.push_str("\n## Who predicts better\n\nLog loss per decision (lower is better); difference to the market with a 95% day-block bootstrap interval (negative = better than the market). Pools: logit p = w·logit(market) + (1−w)·logit(model).\n");
        for t in &self.scores {
            let _ = write!(
                s,
                "\n### {} — {} points, {} days\n\n| predictor | log loss | Brier | vs market | 95% CI |\n|---|---:|---:|---:|---|\n",
                t.subset, t.points, t.days
            );
            for r in &t.rows {
                let mark = if t.best_weight == Some(r.weight) {
                    " **(best)**"
                } else {
                    ""
                };
                let live = if (r.weight - self.configured_weight).abs() < 1e-9 {
                    " (live)"
                } else {
                    ""
                };
                let _ = writeln!(
                    s,
                    "| {}{}{} | {:.4} | {:.4} | {:+.4} | [{:+.4}, {:+.4}] |",
                    r.name,
                    live,
                    mark,
                    r.log_loss,
                    r.brier,
                    r.diff_vs_market,
                    r.diff_ci_low,
                    r.diff_ci_high
                );
            }
        }
        s.push_str("\n## Where prices are wrong\n\nWin rate by market price (a well-calibrated market wins as often as its price says).\n\n| market price | points | mean price | mean model | win rate | 95% CI |\n|---|---:|---:|---:|---:|---|\n");
        for r in &self.calibration {
            let _ = writeln!(
                s,
                "| {} | {} | {:.3} | {:.3} | {:.3} | [{:.3}, {:.3}] |",
                r.group, r.points, r.market_mean, r.model_mean, r.win_rate, r.ci_low, r.ci_high
            );
        }
        s.push_str("\nWhen the model disagreed with the market — whoever is closer to the win rate was right.\n\n| case | points | mean market | mean model | win rate | 95% CI |\n|---|---:|---:|---:|---:|---|\n");
        for r in &self.disagreement {
            let _ = writeln!(
                s,
                "| {} | {} | {:.3} | {:.3} | {:.3} | [{:.3}, {:.3}] |",
                r.group, r.points, r.market_mean, r.model_mean, r.win_rate, r.ci_low, r.ci_high
            );
        }
        s.push_str("\n## Who is sure first\n\nFirst decision at which the winning bucket reached the threshold (lag = model − market; positive = the market was first).\n\n| threshold | days | both | market first | model first | same time | market only | model only | median lag (min) |\n|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        for c in &self.crossings {
            let _ = writeln!(
                s,
                "| {:.2} | {} | {} | {} | {} | {} | {} | {} | {} |",
                c.threshold,
                c.days,
                c.both,
                c.market_first,
                c.model_first,
                c.same_time,
                c.market_only,
                c.model_only,
                c.median_model_lag_minutes
                    .map_or_else(|| "—".to_owned(), |m| format!("{m:+.0}"))
            );
        }
        let _ = write!(
            s,
            "\n## How fast dead buckets reprice (strategy D)\n\n{} reports raised the high and killed buckets the market still priced at ≥ the stale threshold. Trades that sold those buckets' YES (or bought their NO) at stale prices, by delay after the observation time; profit is the seller's, after the taker fee.\n\n| delay | trades | shares | profit |\n|---|---:|---:|---:|\n",
            self.latency_events_total
        );
        for b in &self.latency_bins {
            let _ = writeln!(
                s,
                "| {} | {} | {:.0} | ${:.2} |",
                b.label, b.trades, b.shares, b.profit_usd
            );
        }
        let mut top: Vec<&LatencyEvent> = self.latency_events.iter().collect();
        top.sort_by(|a, b| {
            (b.profit_before_knowledge_usd + b.profit_after_knowledge_usd)
                .total_cmp(&(a.profit_before_knowledge_usd + a.profit_after_knowledge_usd))
        });
        if !top.is_empty() {
            s.push_str("\nLargest events:\n\n| date | local | high | dead buckets | stale trades | first after | before us | after us | takers |\n|---|---|---|---|---:|---:|---:|---:|---:|\n");
            for e in top.iter().take(20) {
                let _ = writeln!(
                    s,
                    "| {} | {} | {} → {} | {} | {} | {} | ${:.2} | ${:.2} | {} |",
                    e.date,
                    e.local_time,
                    e.from_high,
                    e.to_high,
                    e.dead_buckets.join(", "),
                    e.stale_trades,
                    e.first_stale_after_s
                        .map_or_else(|| "—".to_owned(), |x| format!("{x} s")),
                    e.profit_before_knowledge_usd,
                    e.profit_after_knowledge_usd,
                    e.distinct_takers
                );
            }
        }
        let _ = write!(
            s,
            "\n## Resolution check\n\nThe METAR high (this bot's resolution view) fell in the resolved bucket on {} of {} days.\n",
            self.resolution_agreed, self.resolution_checked
        );
        for m in &self.resolution_mismatches {
            let _ = writeln!(s, "* {m}");
        }
        if !self.skipped_days.is_empty() {
            s.push_str("\n## Days not scored\n\n");
            for (d, why) in &self.skipped_days {
                let _ = writeln!(s, "* {d}: {why}");
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::synthetic_history;
    use chrono::TimeZone;
    use wm_core::ids::ProviderId;
    use wm_core::market::TempUnit;
    use wm_core::units::TempC;
    use wm_core::weather::{ObservationKey, QualityFlags, ReportType, TempPrecision};

    const TZ: Tz = chrono_tz::Europe::Amsterdam;

    fn st() -> StationId {
        StationId::new("EHAM").unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn local(d: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        TZ.from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
            .single()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn cfg() -> MarketStudyConfig {
        MarketStudyConfig {
            min_model_support: 10,
            bootstrap_iterations: 400,
            ..MarketStudyConfig::new(st(), TZ, PeakConfig::default())
        }
    }

    /// Whole-degree final high of each local day, through the same engine.
    fn final_highs(obs: &[Observation]) -> BTreeMap<NaiveDate, i32> {
        let mut by_day: BTreeMap<NaiveDate, Vec<&Observation>> = BTreeMap::new();
        for o in obs {
            by_day
                .entry(local_date(o.key.observed_at, TZ))
                .or_default()
                .push(o);
        }
        let mut e = TemperatureStateEngine::new(3);
        e.register_station(st(), TZ);
        let mut out = BTreeMap::new();
        for (d, os) in by_day {
            for o in os {
                e.apply_observation(o);
            }
            let (_, end) = local_day_bounds(d, TZ);
            if let Some(h) = e
                .day_state(&st(), d, ViewKind::All, end)
                .and_then(|s| s.high)
            {
                out.insert(d, h.value.round_half_up_whole());
            }
            e.prune(d);
        }
        out
    }

    /// ≤h−3, h−2 … h+2, ≥h+3.
    fn buckets_around(h: i32) -> (Vec<TemperatureBucket>, Vec<String>) {
        let u = TempUnit::Celsius;
        let mut b = vec![TemperatureBucket::at_or_below(h - 3, u)];
        b.extend(((h - 2)..=(h + 2)).map(|v| TemperatureBucket::exact(v, u)));
        b.push(TemperatureBucket::at_or_above(h + 3, u));
        let labels = b.iter().map(TemperatureBucket::label).collect();
        (b, labels)
    }

    /// A market day whose buckets trade every 15 minutes around `price(i)`.
    fn market_day(d: NaiveDate, high: i32, price: &dyn Fn(usize, usize) -> f64) -> MarketDay {
        let (buckets, labels) = buckets_around(high);
        let winner = buckets.iter().position(|b| b.contains(high)).unwrap();
        let mut trades = Vec::new();
        for h in 7..24 {
            for m in [0, 15, 30, 45] {
                let at = local(d, h, m);
                for i in 0..buckets.len() {
                    let p = price(i, winner);
                    for (buy, q) in [(true, p + 0.002), (false, p - 0.002)] {
                        trades.push(MarketTrade {
                            at,
                            bucket: i,
                            yes_price: q.clamp(0.001, 0.999),
                            taker_buys_yes: buy,
                            shares: 20.0,
                            taker: Some(format!("w{i}")),
                        });
                    }
                }
            }
        }
        MarketDay {
            date: d,
            event_slug: format!("highest-temperature-in-amsterdam-on-{d}"),
            buckets,
            labels,
            winner,
            trades,
            truncated: false,
        }
    }

    fn history() -> (Vec<Observation>, BTreeMap<NaiveDate, i32>) {
        let obs = synthetic_history(&st(), date(2025, 1, 1), 420, 11, Duration::minutes(5));
        let highs = final_highs(&obs);
        (obs, highs)
    }

    fn last_days(highs: &BTreeMap<NaiveDate, i32>, n: usize) -> Vec<(NaiveDate, i32)> {
        let v: Vec<(NaiveDate, i32)> = highs.iter().map(|(d, h)| (*d, *h)).collect();
        v[v.len() - n - 1..v.len() - 1].to_vec()
    }

    fn row(t: &ScoreTable, w: f64) -> &ScoreRow {
        t.rows.iter().find(|r| (r.weight - w).abs() < 1e-9).unwrap()
    }

    #[test]
    fn a_market_that_knows_the_answer_beats_the_model() {
        let (obs, highs) = history();
        let days: Vec<MarketDay> = last_days(&highs, 40)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|i, w| if i == w { 0.97 } else { 0.005 }))
            .collect();
        let r = market_study(&obs, None, &days, &cfg());
        assert_eq!(r.market_days, 40);
        assert_eq!(r.scored_days, 40, "{:?}", r.skipped_days);
        assert_eq!((r.resolution_agreed, r.resolution_checked), (40, 40));
        assert!(r.points > 1_000, "{}", r.points);
        let all = &r.scores[0];
        let (model, market) = (row(all, 0.0), row(all, 1.0));
        assert!(market.log_loss < model.log_loss);
        assert!(model.diff_ci_low > 0.0, "{model:?}");
        assert!(market.diff_vs_market.abs() < 1e-12);
        assert_eq!(all.best_weight, Some(1.0));
        assert!(
            r.verdict
                .iter()
                .any(|v| v.contains("The market predicted better")),
            "{:?}",
            r.verdict
        );
        // The oracle is sure of the winner from the first decision on.
        let c95 = r.crossings.iter().find(|c| c.threshold == 0.95).unwrap();
        assert!(c95.market_first + c95.market_only >= c95.days - c95.same_time);
        assert!(c95.model_first == 0, "{c95:?}");
        // Calibration: the 0.90–0.98 bin (0.97 prices) wins every time.
        let bin = r
            .calibration
            .iter()
            .find(|b| b.group == "0.90–0.98")
            .unwrap();
        assert!(bin.points > 0 && (bin.win_rate - 1.0).abs() < 1e-12);
    }

    #[test]
    fn an_uninformed_market_loses_to_the_model() {
        let (obs, highs) = history();
        let mut days: Vec<MarketDay> = last_days(&highs, 40)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|_, _| 1.0 / 7.0))
            .collect();
        // One day resolved differently from the METAR high.
        let last = days.last_mut().unwrap();
        last.winner += 1;
        let r = market_study(&obs, None, &days, &cfg());
        let all = &r.scores[0];
        let model = row(all, 0.0);
        assert!(model.diff_ci_high < 0.0, "{model:?}");
        assert!(all.best_weight.unwrap() < 0.5, "{:?}", all.best_weight);
        assert!(
            r.verdict
                .iter()
                .any(|v| v.contains("The model predicted better"))
        );
        assert_eq!((r.resolution_agreed, r.resolution_checked), (39, 40));
        assert_eq!(r.resolution_mismatches.len(), 1);
        // Uniform prices never reach the trading region.
        assert_eq!(r.scores[1].points, 0);
        assert_eq!(r.scores[1].best_weight, None);
    }

    #[test]
    fn a_day_is_never_scored_with_a_model_that_has_learned_it() {
        let (obs, highs) = history();
        let (first, h) = highs.iter().next().map(|(d, h)| (*d, *h)).unwrap();
        let days = vec![market_day(first, h, &|_, _| 0.5)];
        let r = market_study(&obs, None, &days, &cfg());
        assert_eq!(r.scored_days, 0);
        assert_eq!(r.points, 0);
        assert_eq!(
            r.skipped_days,
            vec![(first, "no decision point with a market price".to_owned())]
        );
        assert!(r.verdict[0].starts_with("No market day could be scored"));
        assert!(
            !r.verdict
                .iter()
                .any(|v| v.contains("predicted") || v.contains("Best pool")),
            "{:?}",
            r.verdict
        );
    }

    #[test]
    fn unusable_market_days_are_listed_with_the_reason() {
        let (obs, highs) = history();
        let (d, h) = last_days(&highs, 1)[0];
        let mut bad = market_day(d, h, &|_, _| 0.5);
        bad.winner = 99;
        let missing = market_day(date(2030, 1, 1), 10, &|_, _| 0.5);
        let r = market_study(&obs, None, &[bad, missing], &cfg());
        assert_eq!(
            r.skipped_days,
            vec![
                (d, "market without a valid winner".to_owned()),
                (date(2030, 1, 1), "no METAR history for the day".to_owned()),
            ]
        );
    }

    fn report_obs(t: DateTime<Utc>, whole: i32) -> Observation {
        Observation {
            key: ObservationKey {
                station: st(),
                observed_at: t,
                report_type: ReportType::Metar,
            },
            version: 1,
            temperature: Some(TempC::from_whole(whole)),
            dewpoint: None,
            precision: TempPrecision::WholeDegree,
            raw_text: String::new(),
            content_hash: t.to_rfc3339(),
            provider: ProviderId::synthetic(),
            provider_receipt_at: None,
            fetched_at: t,
            parser_version: 1,
            quality: QualityFlags::default(),
        }
    }

    fn trade(
        at: DateTime<Utc>,
        bucket: usize,
        p: f64,
        buy: bool,
        shares: f64,
        who: &str,
    ) -> MarketTrade {
        MarketTrade {
            at,
            bucket,
            yes_price: p,
            taker_buys_yes: buy,
            shares,
            taker: Some(who.to_owned()),
        }
    }

    #[test]
    fn stale_quotes_on_dead_buckets_are_timed_from_the_observation() {
        let d = date(2026, 7, 1);
        let t = |h: u32, m: u32, s: i64| local(d, h, m) + Duration::seconds(s);
        let obs: Vec<Observation> = [
            (t(10, 25, 0), 17),
            (t(10, 55, 0), 18), // kills 17 (no trades there: not an event)
            (t(11, 25, 0), 18),
            (t(11, 55, 0), 19), // kills 18, still priced at ~0.39
            (t(12, 25, 0), 19),
        ]
        .iter()
        .map(|(at, v)| report_obs(*at, *v))
        .collect();
        let (buckets, labels) = buckets_around(19);
        let dead = labels.iter().position(|l| l == "18°C").unwrap();
        let trades = vec![
            trade(t(11, 50, 0), dead, 0.40, true, 5.0, "x"),
            trade(t(11, 50, 0), dead, 0.38, false, 5.0, "x"),
            trade(t(11, 55, 30), dead, 0.30, false, 100.0, "a"), // 30 s, before us
            trade(t(11, 55, 400), dead, 0.25, false, 10.0, "b"), // 400 s, after us
            trade(t(11, 55, 420), dead, 0.05, true, 10.0, "c"),  // buys the dead YES
            trade(t(11, 55, 430), dead, 0.01, false, 10.0, "d"), // below the threshold
        ];
        let md = MarketDay {
            date: d,
            event_slug: "x".into(),
            winner: labels.iter().position(|l| l == "19°C").unwrap(),
            buckets,
            labels,
            trades,
            truncated: false,
        };
        let r = market_study(&obs, None, &[md], &cfg());
        assert_eq!(r.latency_events_total, 1, "{:?}", r.latency_events);
        let e = &r.latency_events[0];
        assert_eq!((e.from_high, e.to_high), (18, 19));
        assert_eq!(e.local_time, "11:55");
        assert_eq!(e.dead_buckets, vec!["18°C".to_owned()]);
        assert_eq!(e.stale_trades, 2);
        assert_eq!(e.first_stale_after_s, Some(30));
        assert_eq!(e.distinct_takers, 2);
        let fee = |p: f64| 0.05 * p * (1.0 - p);
        assert!((e.profit_before_knowledge_usd - 100.0 * (0.30 - fee(0.30))).abs() < 1e-9);
        assert!((e.profit_after_knowledge_usd - 10.0 * (0.25 - fee(0.25))).abs() < 1e-9);
        let bin = |l: &str| r.latency_bins.iter().find(|b| b.label == l).unwrap().trades;
        assert_eq!((bin("0–1 min"), bin("5–10 min"), bin("1–2 min")), (1, 1, 0));
        // The ordinary sell five minutes before the report is only in the table.
        assert_eq!(r.latency_bins[0].trades, 1);
        assert!(
            r.verdict
                .iter()
                .any(|v| v.contains("Dead buckets after 1 new highs"))
        );
        let md = r.to_markdown();
        for section in [
            "## Verdict",
            "## Who predicts better",
            "## Where prices are wrong",
            "## Who is sure first",
            "## How fast dead buckets reprice",
            "## Resolution check",
            "| 2026-07-01 | 11:55 | 18 → 19 | 18°C | 2 | 30 s |",
        ] {
            assert!(md.contains(section), "missing {section}\n{md}");
        }
    }

    #[test]
    fn the_market_price_is_the_midpoint_of_recent_taker_buys_and_sells() {
        let t0 = local(date(2026, 7, 1), 12, 0);
        let m = |mins: i64| t0 + Duration::minutes(mins);
        let owned = [
            trade(m(-50), 0, 0.60, true, 1.0, "a"),
            trade(m(-10), 0, 0.50, false, 1.0, "b"),
            trade(m(-5), 0, 0.58, true, 1.0, "c"),
        ];
        let v: Vec<&MarketTrade> = owned.iter().collect();
        let age = Duration::minutes(30);
        assert!((market_price(&v, t0, age).unwrap() - 0.54).abs() < 1e-12);
        // At the limit the older sell drops out; only the buy remains.
        assert!((market_price(&v, m(25), age).unwrap() - 0.58).abs() < 1e-12);
        assert_eq!(market_price(&v, m(-60), age), None);
        // A trade at the decision instant counts.
        assert!((market_price(&v, m(-5), age).unwrap() - 0.54).abs() < 1e-12);
        assert_eq!(market_price(&v, m(40), age), None);
    }

    #[test]
    fn weights_always_include_model_and_market() {
        let mut c = cfg();
        c.weights = vec![0.5, 0.5, 2.0];
        assert_eq!(c.weights(), vec![0.0, 0.5, 1.0]);
    }
}
