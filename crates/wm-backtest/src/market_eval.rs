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
//! * **Both model structures** — the candidate structure
//!   ([`wm_strategy::ModelStructure::Candidate`]) is trained alongside and
//!   scored on the same decisions.
//! * **Strategies at traded prices** — A and B replayed with either
//!   structure, several confirmation windows and ask ranges
//!   ([`crate::market_sim`]), and chosen days report by report.
//! * **Strategy F** — when each season's high is first reported (the METAR
//!   history before each market day) and F's peak-slot rule with its
//!   variants at traded prices, one variant chosen out of sample
//!   ([`crate::market_peak`]).
//!
//! Market prices come from executed trades, not quotes: the midpoint of the
//! latest taker buy and taker sell of YES (ask and bid proxies), each at most
//! `max_price_age` old. Stale trades only show liquidity someone took — a
//! lower bound on what was offered.

use crate::forecast_eval::{ForecastHistory, ratio_ci};
use crate::market_gk::{self, GkDay, KnmiAccuracy, KnmiAccuracyRow, KnmiHistory};
use crate::market_makers::{self, FlowCollector, MakerTakerStudy};
use crate::market_peak;
use crate::market_sim::{
    self, DayTimeline, Decision, Flow, MarketSimConfig, Quote, SimTrade, StrategyRow,
};
use crate::research::wilson;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use wm_core::ids::StationId;
use wm_core::market::TemperatureBucket;
use wm_core::time::{Season, local_date, local_day_bounds};
use wm_core::weather::Observation;
use wm_strategy::{
    EmpiricalPeakModel, ModelStructure, PeakConfig, PeakDetectionEngine, PeakTimes,
    PeakTimesBuilder, ProbabilityModel, TemperatureStateEngine, ViewKind, log_pool,
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
    /// Strategies A and B replayed at traded prices.
    pub sim: MarketSimConfig,
    /// Minutes past each UTC hour of the station's routine reports: the
    /// maker study times trades against them, and resting orders are
    /// cancelled before them.
    pub routine_minutes: Vec<u8>,
    /// Days replayed report by report (`research market --day`).
    pub timeline_days: Vec<NaiveDate>,
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
            sim: MarketSimConfig::default(),
            routine_minutes: vec![25, 55],
            timeline_days: Vec::new(),
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
    /// The candidate structure alone and pooled at the live weight.
    #[serde(default)]
    pub candidate_rows: Vec<ScoreRow>,
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
    /// Log loss of the candidate structure minus the current one per
    /// decision, with its 95 % day-block interval (all decisions).
    #[serde(default)]
    pub candidate_diff: f64,
    #[serde(default)]
    pub candidate_diff_ci_low: f64,
    #[serde(default)]
    pub candidate_diff_ci_high: f64,
    /// The replay settings of the strategy section.
    #[serde(default)]
    pub sim: MarketSimConfig,
    /// Strategies at traded prices, per variant.
    #[serde(default)]
    pub strategies: Vec<StrategyRow>,
    #[serde(default)]
    pub strategy_verdict: Vec<String>,
    /// Every simulated trade.
    #[serde(default)]
    pub sim_trades: Vec<SimTrade>,
    /// Replayed days (`--day`).
    #[serde(default)]
    pub timelines: Vec<DayTimeline>,
    /// What resting orders earned on the other side of every trade.
    #[serde(default)]
    pub maker_taker: MakerTakerStudy,
    /// When each season's high was first reported over the whole METAR
    /// history (each market day was replayed with the days before it).
    #[serde(default)]
    pub peak_times: Option<PeakTimes>,
    /// Strategy F's rules at traded prices ([`MarketSimConfig::f`]'s shares
    /// a trade, not the $10 stake of the other rows).
    #[serde(default)]
    pub f_strategies: Vec<StrategyRow>,
    #[serde(default)]
    pub f_verdict: Vec<String>,
    /// The F rule best on the first half of the market days, judged on the
    /// second half.
    #[serde(default)]
    pub f_out_of_sample: Option<String>,
    /// Strategies G–K's rules at traded prices (their own stakes).
    #[serde(default)]
    pub gk_strategies: Vec<StrategyRow>,
    #[serde(default)]
    pub gk_verdict: Vec<String>,
    /// Per strategy G–K: the rule best on the first half, on the second.
    #[serde(default)]
    pub gk_out_of_sample: Vec<String>,
    /// How often the next METAR raised the high, by KNMI's ten-minute mean
    /// before it (strategy K's `p_new_high`).
    #[serde(default)]
    pub knmi_accuracy: Vec<KnmiAccuracyRow>,
    /// Market days with KNMI readings.
    #[serde(default)]
    pub knmi_days: u64,
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
    /// The candidate structure's probability.
    candidate: f64,
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
    quote(trades, at, max_age).mid
}

/// The latest taker buy and sell of YES at `at` (each at most `max_age`
/// old) and their midpoint — or the one side seen.
fn quote(trades: &[&MarketTrade], at: DateTime<Utc>, max_age: Duration) -> Quote {
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
    Quote {
        mid: match (buy, sell) {
            (Some(a), Some(b)) => Some((a + b) / 2.0),
            (a, b) => a.or(b),
        },
        yes_ask: buy,
        yes_bid: sell,
    }
}

/// Taker flow on one bucket's YES in `(at − lookback, at]`: shares bought at
/// or below `cap` and sold, and the ask proxy when the lookback began (the
/// latest buy then, at most `max_age` old, else the first buy after it).
fn flow(
    trades: &[&MarketTrade],
    at: DateTime<Utc>,
    lookback: Duration,
    max_age: Duration,
    cap: f64,
) -> Flow {
    let start = at - lookback;
    let lo = trades.partition_point(|t| t.at <= start);
    let hi = trades.partition_point(|t| t.at <= at).max(lo);
    let window = &trades[lo..hi];
    let mut f = Flow {
        ask_then: quote(trades, start, max_age).yes_ask.or_else(|| {
            window
                .iter()
                .find(|t| t.taker_buys_yes)
                .map(|t| t.yes_price)
        }),
        ..Flow::default()
    };
    for t in window {
        if !t.taker_buys_yes {
            f.sold += t.shares;
        } else if t.yes_price <= cap + 1e-12 {
            f.bought += t.shares;
        }
    }
    f
}

/// A market day whose METAR history ends more than this many minutes before
/// local midnight is incomplete and not scored. The archive is read up to
/// 00:00 UTC of the current day, so the day that just ended is only partly
/// there until then.
const MAX_TAIL_GAP_MIN: i64 = 90;

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
/// Strategy K is not replayed (no KNMI readings): see [`market_study_with`].
pub fn market_study(
    observations: &[Observation],
    forecasts: Option<&ForecastHistory>,
    days: &[MarketDay],
    cfg: &MarketStudyConfig,
) -> MarketStudyReport {
    market_study_with(observations, forecasts, None, days, cfg)
}

/// [`market_study`] with KNMI's ten-minute readings for strategy K.
pub fn market_study_with(
    observations: &[Observation],
    forecasts: Option<&ForecastHistory>,
    knmi: Option<&KnmiHistory>,
    days: &[MarketDay],
    cfg: &MarketStudyConfig,
) -> MarketStudyReport {
    let view = ViewKind::All;
    let mut engine = TemperatureStateEngine::new(3);
    engine.register_station(cfg.station.clone(), cfg.tz);
    let peak = PeakDetectionEngine::new(cfg.peak.clone());
    let new_model = |structure: ModelStructure, suffix: &str| {
        let levels = if forecasts.is_some() {
            EmpiricalPeakModel::with_refinement(structure.levels(), structure.refinement())
        } else {
            structure.levels()
        };
        EmpiricalPeakModel::new(
            format!("prequential-{}{suffix}", cfg.station),
            cfg.station.to_string(),
            view.label(),
            cfg.k_classes,
            levels,
        )
    };
    let mut model = new_model(ModelStructure::Current, "");
    let mut candidate = new_model(ModelStructure::Candidate, "-first-reach");
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
        candidate_diff: 0.0,
        candidate_diff_ci_low: 0.0,
        candidate_diff_ci_high: 0.0,
        sim: cfg.sim.clone(),
        strategies: Vec::new(),
        strategy_verdict: Vec::new(),
        sim_trades: Vec::new(),
        timelines: Vec::new(),
        maker_taker: MakerTakerStudy::default(),
        peak_times: None,
        f_strategies: Vec::new(),
        f_verdict: Vec::new(),
        f_out_of_sample: None,
        gk_strategies: Vec::new(),
        gk_verdict: Vec::new(),
        gk_out_of_sample: Vec::new(),
        knmi_accuracy: Vec::new(),
        knmi_days: 0,
        verdict: Vec::new(),
    };
    let mut knmi_accuracy = KnmiAccuracy::default();
    let mut flows = FlowCollector::default();
    let mut points: Vec<Point> = Vec::new();
    let mut scored_dates: Vec<NaiveDate> = Vec::new();
    let mut seen_market: HashSet<NaiveDate> = HashSet::new();
    // Peak times of the days before the current one (strategy F's slots).
    let mut peaks = PeakTimesBuilder::new();
    let mut f_days: Vec<NaiveDate> = Vec::new();

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
            let tail_gap = times.last().map_or(i64::MAX, |t| (end - *t).num_minutes());
            if md.winner >= md.buckets.len() || md.labels.len() != md.buckets.len() {
                report
                    .skipped_days
                    .push((*date, "market without a valid winner".into()));
            } else if tail_gap > MAX_TAIL_GAP_MIN {
                report.skipped_days.push((
                    *date,
                    format!(
                        "METAR history incomplete: it ends at {} local, {tail_gap} min before the day did (the archive is read up to 00:00 UTC, so rerun after that)",
                        times
                            .last()
                            .map_or_else(|| "—".to_owned(), |t| local_hm(*t, cfg.tz))
                    ),
                ));
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
                let per_bucket = trades_by_bucket(md, cfg.min_trade_shares);
                let decisions = decisions(
                    &states,
                    [&model, &candidate],
                    &per_bucket,
                    cfg,
                    cfg.first_decision_minute,
                );
                // G–K decide from local midnight (J quotes the morning).
                let all_day =
                    decisions_from_midnight(&states, [&model, &candidate], &per_bucket, cfg);
                score_day(md, day_index, &decisions, cfg, &mut points, &mut report);
                let mut trades = market_sim::simulate_day(
                    md.date,
                    &md.buckets,
                    &md.labels,
                    md.winner,
                    &decisions,
                    &cfg.sim,
                    cfg.taker_fee_rate,
                    cfg.configured_weight,
                    cfg.min_model_support,
                    cfg.tz,
                );
                trades.extend(market_makers::simulate_makers(
                    md.date,
                    &md.buckets,
                    &md.labels,
                    md.winner,
                    &decisions,
                    &per_bucket,
                    &cfg.sim,
                    cfg.taker_fee_rate,
                    cfg.configured_weight,
                    cfg.min_model_support,
                    &cfg.routine_minutes,
                    cfg.tz,
                ));
                trades.extend(market_peak::simulate_peak_slot(
                    md.date,
                    &md.buckets,
                    &md.labels,
                    md.winner,
                    &decisions,
                    &per_bucket,
                    &peaks.build(),
                    &cfg.sim,
                    cfg.taker_fee_rate,
                    &cfg.routine_minutes,
                    cfg.tz,
                ));
                let readings = knmi.and_then(|k| k.get(date)).map(Vec::as_slice);
                let peak_before = peaks.build();
                trades.extend(market_gk::simulate_gk(
                    &GkDay {
                        date: md.date,
                        buckets: &md.buckets,
                        labels: &md.labels,
                        winner: md.winner,
                        decisions: &all_day,
                        per_bucket: &per_bucket,
                        peak: &peak_before,
                        knmi: readings.filter(|r| !r.is_empty()),
                        tz: cfg.tz,
                    },
                    &cfg.sim,
                    cfg.taker_fee_rate,
                    &cfg.routine_minutes,
                ));
                if let Some(r) = readings.filter(|r| !r.is_empty()) {
                    knmi_accuracy.add_day(&all_day, r);
                }
                f_days.push(*date);
                flows.add_day(
                    md.date,
                    &md.buckets,
                    md.winner,
                    &md.trades,
                    &states,
                    cfg.knowledge_delay,
                    &cfg.routine_minutes,
                    cfg.taker_fee_rate,
                    cfg.tz,
                );
                if cfg.timeline_days.contains(date) {
                    report.timelines.push(market_sim::timeline(
                        md.date,
                        &md.buckets,
                        &md.labels[md.winner],
                        &decisions,
                        &trades,
                        &cfg.sim,
                        cfg.tz,
                    ));
                }
                report.sim_trades.extend(trades);
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
            candidate.observe(f, *increment);
        }
        peaks.add_day(
            *date,
            Season::from_month(date.month(), cfg.peak.southern_hemisphere),
            &final_state.points,
            final_high,
        );
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
    // Candidate minus current structure, per decision, day-block interval.
    let mut per_day = vec![(0.0, 0.0); n_days];
    for p in &points {
        let d = &mut per_day[p.day];
        d.0 += log_loss(p.candidate, p.won) - log_loss(p.model, p.won);
        d.1 += 1.0;
    }
    if !points.is_empty() {
        report.candidate_diff = per_day.iter().map(|d| d.0).sum::<f64>() / points.len() as f64;
        let days: Vec<(f64, f64)> = per_day.into_iter().filter(|d| d.1 > 0.0).collect();
        (report.candidate_diff_ci_low, report.candidate_diff_ci_high) =
            ratio_ci(&days, cfg.bootstrap_iterations, cfg.seed ^ 0xCA);
    }
    let mut strategies = market_sim::strategy_rows(
        &report.sim_trades,
        &cfg.sim,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x51,
    );
    strategies.extend(market_makers::maker_rows(
        &report.sim_trades,
        &cfg.sim,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x52,
    ));
    report.strategies = strategies;
    report.strategy_verdict = market_sim::verdict(&report.strategies, &cfg.sim);
    report.peak_times = (peaks.days() > 0).then(|| peaks.build());
    report.f_strategies = market_peak::peak_rows(
        &report.sim_trades,
        &cfg.sim,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x53,
    );
    report.f_verdict = market_peak::verdict(&report.f_strategies, &cfg.sim);
    report.f_out_of_sample = market_peak::out_of_sample(
        &report.sim_trades,
        &cfg.sim,
        &f_days,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x54,
    );
    report.knmi_days = knmi_accuracy.days;
    report.knmi_accuracy = knmi_accuracy.rows();
    report.gk_strategies = market_gk::gk_rows(
        &report.sim_trades,
        &cfg.sim,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x55,
    );
    report.gk_verdict = market_gk::verdict(&report.gk_strategies, &cfg.sim, report.knmi_days);
    report.gk_out_of_sample = market_gk::out_of_sample(
        &report.sim_trades,
        &cfg.sim,
        &f_days,
        report.knmi_days,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x56,
    );
    report.maker_taker = flows.finish(
        cfg.taker_fee_rate,
        cfg.sim.maker.rebate_share,
        cfg.bootstrap_iterations,
        cfg.seed ^ 0x3A,
    );
    report.verdict = verdict(&report);
    report
}

/// [`decisions`] from local midnight.
fn decisions_from_midnight(
    states: &[(DateTime<Utc>, wm_strategy::PeakFeatures)],
    models: [&EmpiricalPeakModel; 2],
    per_bucket: &[Vec<&MarketTrade>],
    cfg: &MarketStudyConfig,
) -> Vec<Decision> {
    decisions(states, models, per_bucket, cfg, 0)
}

/// Every decision of a market day from local minute `first_minute`: each
/// structure's distribution (trained on earlier days) and every bucket's
/// quote at the decision time (observation + knowledge delay).
fn decisions(
    states: &[(DateTime<Utc>, wm_strategy::PeakFeatures)],
    models: [&EmpiricalPeakModel; 2],
    per_bucket: &[Vec<&MarketTrade>],
    cfg: &MarketStudyConfig,
    first_minute: u16,
) -> Vec<Decision> {
    let lookback = Duration::minutes(cfg.sim.e.lookback_minutes.max(1));
    states
        .iter()
        .filter(|(_, f)| f.local_minute_now >= first_minute)
        .map(|(t, f)| {
            let knowledge = *t + cfg.knowledge_delay;
            Decision {
                at: *t,
                knowledge,
                dists: models.map(|m| m.distribution(f)),
                quotes: per_bucket
                    .iter()
                    .map(|tr| quote(tr, knowledge, cfg.max_price_age))
                    .collect(),
                flows: per_bucket
                    .iter()
                    .map(|tr| {
                        flow(
                            tr,
                            knowledge,
                            lookback,
                            cfg.max_price_age,
                            cfg.sim.e.max_price,
                        )
                    })
                    .collect(),
                f: f.clone(),
            }
        })
        .collect()
}

/// Score every still-possible bucket at each decision of one market day.
/// Which decisions count is the current structure's call (its support and
/// tail gates, as before); the candidate is scored on the same ones.
fn score_day(
    md: &MarketDay,
    day_index: usize,
    decisions: &[Decision],
    cfg: &MarketStudyConfig,
    points: &mut Vec<Point>,
    report: &mut MarketStudyReport,
) {
    for d in decisions {
        let [Some(dist), Some(cand)] = &d.dists else {
            continue;
        };
        let high = d.f.high_whole;
        for (i, bucket) in md.buckets.iter().enumerate() {
            if bucket.upper.is_some_and(|u| u < high) {
                continue; // already decided by the observations
            }
            if dist.support < cfg.min_model_support {
                report.skipped_support += 1;
                continue;
            }
            let lower = dist.p_in_bucket_lower(high, bucket);
            let upper = dist.p_in_bucket_upper(high, bucket);
            if (upper - lower).abs() > 1e-9 {
                report.skipped_ambiguous += 1;
                continue;
            }
            let Some(market) = d.quotes[i].mid else {
                report.skipped_no_price += 1;
                continue;
            };
            points.push(Point {
                day: day_index,
                at: d.at,
                bucket: i,
                model: lower,
                candidate: cand.p_in_bucket_lower(high, bucket),
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

/// Log loss, Brier score and the difference to the market (with its
/// day-block interval unless `ci` is false) of one predictor.
fn score_row(
    name: String,
    weight: f64,
    pts: &[&Point],
    n_days: usize,
    prob: &dyn Fn(&Point) -> f64,
    ci: bool,
    cfg: &MarketStudyConfig,
) -> ScoreRow {
    let n = pts.len() as f64;
    let (mut ll, mut br) = (0.0, 0.0);
    // Per day: (Σ log loss − Σ market log loss, points).
    let mut per_day = vec![(0.0, 0.0); n_days.max(1)];
    for p in pts {
        let q = prob(p);
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
    let (lo, hi) = if ci {
        ratio_ci(&per_day, cfg.bootstrap_iterations, cfg.seed)
    } else {
        (0.0, 0.0)
    };
    ScoreRow {
        name,
        weight,
        log_loss: if n > 0.0 { ll / n } else { 0.0 },
        brier: if n > 0.0 { br / n } else { 0.0 },
        diff_vs_market: diff,
        diff_ci_low: lo,
        diff_ci_high: hi,
    }
}

fn score_table(subset: &str, pts: &[&Point], n_days: usize, cfg: &MarketStudyConfig) -> ScoreTable {
    let days_with: HashSet<usize> = pts.iter().map(|p| p.day).collect();
    let rows: Vec<ScoreRow> = cfg
        .weights()
        .into_iter()
        .map(|w| {
            let market = (w - 1.0).abs() < 1e-9;
            let name = if w == 0.0 {
                "model".to_owned()
            } else if market {
                "market".to_owned()
            } else {
                format!("pool w={w:.2}")
            };
            let pool = move |p: &Point| log_pool(p.model, Some(p.market), w);
            score_row(name, w, pts, n_days, &pool, !market, cfg)
        })
        .collect();
    let best_weight = (!pts.is_empty())
        .then(|| {
            rows.iter()
                .min_by(|a, b| a.log_loss.total_cmp(&b.log_loss))
                .map(|r| r.weight)
        })
        .flatten();
    let cw = cfg.configured_weight.clamp(0.0, 1.0);
    let mut candidate_rows = vec![score_row(
        "candidate model".to_owned(),
        0.0,
        pts,
        n_days,
        &|p| p.candidate,
        true,
        cfg,
    )];
    if cw > 0.0 && cw < 1.0 {
        candidate_rows.push(score_row(
            format!("candidate pool w={cw:.2}"),
            cw,
            pts,
            n_days,
            &|p| log_pool(p.candidate, Some(p.market), cw),
            true,
            cfg,
        ));
    }
    ScoreTable {
        subset: subset.to_owned(),
        points: pts.len() as u64,
        days: days_with.len() as u64,
        rows,
        best_weight,
        candidate_rows,
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
        if let Some(c) = all.candidate_rows.first() {
            let versus = if r.candidate_diff_ci_high < 0.0 {
                "better than"
            } else if r.candidate_diff_ci_low > 0.0 {
                "worse than"
            } else {
                "not clearly different from"
            };
            v.push(format!(
                "Candidate structure: {versus} the current one on these decisions ({:+.4} log loss per decision, 95% CI {:+.4} … {:+.4}); against the market {:+.4} (95% CI {:+.4} … {:+.4}).",
                r.candidate_diff,
                r.candidate_diff_ci_low,
                r.candidate_diff_ci_high,
                c.diff_vs_market,
                c.diff_ci_low,
                c.diff_ci_high
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
    if let Some(line) = r.maker_taker.verdict.first() {
        v.push(line.clone());
    }
    if let Some(line) = &r.f_out_of_sample {
        v.push(line.clone());
    }
    v.extend(r.gk_out_of_sample.iter().cloned());
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
            for r in &t.candidate_rows {
                let _ = writeln!(
                    s,
                    "| {} | {:.4} | {:.4} | {:+.4} | [{:+.4}, {:+.4}] |",
                    r.name, r.log_loss, r.brier, r.diff_vs_market, r.diff_ci_low, r.diff_ci_high
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
        s.push_str(&market_sim::strategies_markdown(
            &self.strategies,
            &self.sim_trades,
            &self.sim,
            &self.strategy_verdict,
        ));
        if let Some(pt) = &self.peak_times {
            s.push_str(&pt.to_markdown(self.sim.f.slot_from_quantile, self.sim.f.slot_to_quantile));
        }
        s.push_str(&market_peak::markdown(
            &self.f_strategies,
            &self.sim_trades,
            &self.sim,
            &self.f_verdict,
            self.f_out_of_sample.as_deref(),
        ));
        s.push_str(&market_gk::markdown(
            &self.gk_strategies,
            &self.sim,
            &self.gk_verdict,
            &self.gk_out_of_sample,
            &self.knmi_accuracy,
            self.knmi_days,
        ));
        s.push_str(&market_makers::maker_taker_markdown(&self.maker_taker));
        for t in &self.timelines {
            s.push_str(&market_sim::timeline_markdown(t, self.knowledge_delay_s));
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

    #[test]
    fn a_day_the_archive_holds_only_in_part_is_not_scored() {
        let (obs, highs) = history();
        // The latest day as read before 00:00 UTC: only its first two hours.
        let (last, h) = highs.iter().next_back().map(|(d, h)| (*d, *h)).unwrap();
        let cut = local(last, 2, 0);
        let obs: Vec<Observation> = obs
            .into_iter()
            .filter(|o| o.key.observed_at < cut)
            .collect();
        let (d, h2) = last_days(&highs, 1)[0];
        let days = vec![
            market_day(d, h2, &|i, w| if i == w { 0.9 } else { 0.02 }),
            market_day(last, h, &|i, w| if i == w { 0.9 } else { 0.02 }),
        ];
        let r = market_study(&obs, None, &days, &cfg());
        assert_eq!(r.skipped_days.len(), 1, "{:?}", r.skipped_days);
        let (day, why) = &r.skipped_days[0];
        assert_eq!(*day, last);
        assert!(
            why.starts_with("METAR history incomplete: it ends at 01:55 local"),
            "{why}"
        );
        // Neither scored nor counted as a resolution mismatch.
        assert_eq!((r.resolution_checked, r.resolution_agreed), (1, 1));
        assert!(r.resolution_mismatches.is_empty());
        assert_eq!(r.scored_days, 1);
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
        // A complete day: the afternoon and evening stay below the high.
        let evening = (12..24)
            .flat_map(|h| [(h, 55), (h + 1, 25)])
            .filter(|(h, _)| *h < 24);
        let obs: Vec<Observation> = [
            (t(10, 25, 0), 17),
            (t(10, 55, 0), 18), // kills 17 (no trades there: not an event)
            (t(11, 25, 0), 18),
            (t(11, 55, 0), 19), // kills 18, still priced at ~0.39
            (t(12, 25, 0), 19),
        ]
        .into_iter()
        .chain(evening.map(|(h, m)| (t(h, m, 0), 18)))
        .map(|(at, v)| report_obs(at, v))
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
    fn strategies_are_replayed_at_traded_prices() {
        let (obs, highs) = history();
        // The METAR high's bucket trades at 0.80 all day, the others at 0.02;
        // the last day resolves one bucket higher than the METAR high.
        let mut days: Vec<MarketDay> = last_days(&highs, 30)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|i, w| if i == w { 0.80 } else { 0.02 }))
            .collect();
        let lost_day = days.last().unwrap().date;
        days.last_mut().unwrap().winner += 1;
        let replay_day = days[5].date;
        let mut c = cfg();
        c.timeline_days = vec![replay_day, date(2031, 1, 1)];
        let r = market_study(&obs, None, &days, &c);
        // Per structure: A and B × 3 windows × 2 ranges, E's 3 variants and
        // the 3 maker versions of the live rules.
        assert_eq!(r.strategies.len(), 2 * (2 * 3 * 2 + 3 + 3));
        let row = |structure: &str, strategy: &str, window: u32, range: &str| {
            r.strategies
                .iter()
                .find(|x| {
                    x.structure == structure
                        && x.strategy == strategy
                        && x.window == window
                        && x.range == range
                })
                .unwrap()
        };
        // Asks of 0.802 are outside the live range: the live rule never buys YES.
        let live = row("current", "A", 60, "0.90–0.99");
        assert!(live.live && live.trades == 0, "{live:?}");
        let wide = row("current", "A", 60, "0.70–0.99");
        assert!(!wide.live);
        assert!(wide.trades >= 20, "{wide:?}");
        assert_eq!(wide.wins + 1, wide.trades, "all but the mis-resolved day");
        // P&L per trade at the stake: 10 / ask × (payout − ask − fee − slippage).
        let fee = |p: f64| 0.05 * p * (1.0 - p);
        let won = 10.0 / 0.802 * (1.0 - 0.802 - fee(0.802) - 0.005);
        let lost = 10.0 / 0.802 * (-0.802 - fee(0.802) - 0.005);
        let a_wide: Vec<&SimTrade> = r
            .sim_trades
            .iter()
            .filter(|t| {
                t.structure == "current"
                    && t.strategy == "A"
                    && t.window == 60
                    && t.range == "0.70–0.99"
            })
            .collect();
        for t in &a_wide {
            assert_eq!((t.side.as_str(), t.price), ("YES", 0.802));
            let want = if t.won { won } else { lost };
            assert!((t.pnl_usd - want).abs() < 1e-9, "{t:?}");
            assert!(t.p_used <= t.p_model + 1e-12);
            assert!(t.p_used - 0.802 - fee(0.802) - 0.005 >= 0.01 - 1e-12);
        }
        assert!(a_wide.iter().any(|t| t.date == lost_day && !t.won));
        assert!((wide.total_usd - (won * (wide.trades - 1) as f64 + lost)).abs() < 1e-6);
        // At most one trade per day and bucket in a variant.
        let mut seen = HashSet::new();
        assert!(
            a_wide
                .iter()
                .all(|t| seen.insert((t.date, t.bucket.clone())))
        );
        // A shorter confirmation trades no later than a longer one.
        assert!(row("current", "A", 0, "0.70–0.99").trades >= wide.trades);
        // Both structures are replayed and scored on the same decisions.
        assert!(row("candidate", "A", 60, "0.70–0.99").trades > 0);
        assert_eq!(r.scores[0].candidate_rows[0].name, "candidate model");
        assert!(
            r.verdict
                .iter()
                .any(|v| v.starts_with("Candidate structure:"))
        );
        assert!(
            r.strategy_verdict[0].starts_with("Live rule (60′ confirmation, asks 0.90–0.99)"),
            "{:?}",
            r.strategy_verdict
        );
        // The replayed day, report by report (a date without a market is ignored).
        assert_eq!(r.timelines.len(), 1);
        let t = &r.timelines[0];
        assert_eq!(t.date, replay_day);
        assert!(t.rows.len() >= 20 && t.rows[0].report.as_str() >= "09:00");
        assert!(t.rows.iter().all(|x| x.p_high_current.is_some()));
        assert!(t.rows.iter().all(|x| {
            x.cell_current
                .as_deref()
                .unwrap()
                .starts_with("MinutesSinceHigh=")
        }));
        assert!(t.rows.iter().all(|x| {
            x.cell_candidate
                .as_deref()
                .unwrap()
                .starts_with("MinutesSinceFirstHigh=")
        }));
        assert!(t.rows.iter().any(|x| !x.trades.is_empty()));
        let md = r.to_markdown();
        for section in [
            "## Strategies at traded prices",
            "| A **live** | current | 60′ | 0.90–0.99 | 0 |",
            &format!("## Day replay — {replay_day} (resolved "),
            "| report | current cell | candidate cell |",
            "| candidate model |",
        ] {
            assert!(md.contains(section), "missing {section}\n{md}");
        }
    }

    #[test]
    fn strategy_e_is_replayed_with_its_book_stand_in() {
        use chrono::Timelike;
        let (obs, highs) = history();
        // The high's bucket trades at 0.95 all day. On even days takers buy
        // far more than they sell from noon on (buyers lift the offers); on
        // odd days buying and selling balance (no book signal).
        let mut days: Vec<MarketDay> = last_days(&highs, 30)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|i, w| if i == w { 0.95 } else { 0.01 }))
            .collect();
        let mut lifting = HashSet::new();
        for (k, md) in days.iter_mut().enumerate() {
            if k % 2 == 1 {
                continue;
            }
            lifting.insert(md.date);
            for t in md
                .trades
                .iter_mut()
                .filter(|t| t.bucket == md.winner && t.at.with_timezone(&TZ).hour() >= 12)
            {
                t.shares = if t.taker_buys_yes { 60.0 } else { 5.0 };
            }
        }
        // One lifting day resolves a bucket higher: E's trade on it loses.
        let lost_day = days[2].date;
        days[2].winner += 1;
        let replay_day = days[0].date;
        let mut c = cfg();
        // The synthetic afternoon cools 1 °C only late: a wider window.
        c.sim.e.end_local_minute = 23 * 60;
        c.timeline_days = vec![replay_day];
        let r = market_study(&obs, None, &days, &c);
        let row = |strategy: &str| {
            r.strategies
                .iter()
                .find(|x| x.structure == "current" && x.strategy == strategy)
                .unwrap()
        };
        let (e, no_book, model) = (row("E"), row("E w/o book"), row("E + model"));
        assert!(e.live && !no_book.live && !model.live);
        assert_eq!((e.window, e.range.as_str()), (60, "0.90–0.99"));
        assert!(e.trades >= 5, "{e:?}");
        assert!(no_book.trades > e.trades, "{no_book:?} vs {e:?}");
        assert!(model.trades <= e.trades);
        let trades = |strategy: &str| -> Vec<&SimTrade> {
            r.sim_trades
                .iter()
                .filter(|t| t.structure == "current" && t.strategy == strategy)
                .collect()
        };
        // E trades only where buyers lifted the offers; each at the ask
        // proxy, on the high's bucket, after the high was 60′ old and 1 °C
        // lower, inside the window.
        let fee = |p: f64| 0.05 * p * (1.0 - p);
        let won = 10.0 / 0.952 * (1.0 - 0.952 - fee(0.952) - 0.005);
        let lost = 10.0 / 0.952 * (-0.952 - fee(0.952) - 0.005);
        for t in trades("E") {
            assert!(lifting.contains(&t.date), "{t:?}");
            assert_eq!((t.side.as_str(), t.price), ("YES", 0.952));
            assert_eq!(t.won, t.date != lost_day, "{t:?}");
            assert_eq!(t.won, t.bucket == t.resolved, "{t:?}");
            let want = if t.won { won } else { lost };
            assert!((t.pnl_usd - want).abs() < 1e-9);
            assert!(t.report.as_str() >= "12:00" && t.report.as_str() < "23:00");
            assert!(t.p_used <= t.p_model + 1e-12);
        }
        assert!(trades("E").iter().any(|t| !t.won));
        assert!(
            trades("E w/o book")
                .iter()
                .any(|t| !lifting.contains(&t.date))
        );
        assert!(trades("E + model").iter().all(|t| t.p_model >= 0.90));
        // One trade per day and bucket per variant.
        let mut seen = HashSet::new();
        assert!(
            trades("E w/o book")
                .iter()
                .all(|t| seen.insert((t.date, t.bucket.clone())))
        );
        assert!(
            r.strategy_verdict
                .iter()
                .any(|v| v.starts_with("Strategy E (12:00–23:00 local")),
            "{:?}",
            r.strategy_verdict
        );
        // The replayed day shows the stand-in: bought / sold over 30′.
        let t = &r.timelines[0];
        assert_eq!(t.flow_minutes, 30);
        let afternoon = t
            .rows
            .iter()
            .find(|x| x.report.as_str() >= "13:00")
            .unwrap();
        assert!(afternoon.bought_high.unwrap() >= 120.0, "{afternoon:?}");
        assert!(afternoon.sold_high.unwrap() <= 15.0, "{afternoon:?}");
        let md = r.to_markdown();
        let lost_row = format!("| {lost_day} | ");
        for section in [
            "Strategy E buys YES on the bucket holding the high",
            "| E **live** | current | 60′ | 0.90–0.99 |",
            "| E w/o book | current | 60′ | 0.90–0.99 |",
            "| bought / sold |",
            "Strategy E's losing trades",
            "E (current, candidate), E w/o book (current, candidate)",
            &lost_row,
        ] {
            assert!(md.contains(section), "missing {section}\n{md}");
        }
    }

    #[test]
    fn taker_flow_counts_the_lookback_and_the_ask_it_began_with() {
        let t0 = local(date(2026, 7, 1), 15, 0);
        let m = |mins: i64| t0 + Duration::minutes(mins);
        let owned = [
            trade(m(-45), 0, 0.93, true, 50.0, "a"),
            trade(m(-20), 0, 0.94, true, 30.0, "b"),
            trade(m(-10), 0, 0.92, false, 10.0, "c"),
            trade(m(-5), 0, 0.995, true, 99.0, "d"),
            trade(m(0), 0, 0.95, true, 20.0, "e"),
            trade(m(1), 0, 0.96, true, 500.0, "f"),
        ];
        let v: Vec<&MarketTrade> = owned.iter().collect();
        let f = flow(&v, t0, Duration::minutes(30), Duration::minutes(60), 0.99);
        // In (14:30, 15:00]: 30 + 20 bought at ≤ 0.99 (0.995 is above the cap,
        // 15:01 is after the decision), 10 sold; the ask at 14:30 was 0.93.
        assert!((f.bought - 50.0).abs() < 1e-9 && (f.sold - 10.0).abs() < 1e-9);
        assert_eq!(f.ask_then, Some(0.93));
        // Without a buy before the lookback, its first buy is the ask then.
        let f = flow(&v, t0, Duration::minutes(30), Duration::minutes(10), 0.99);
        assert_eq!(f.ask_then, Some(0.94));
        // Nothing traded: nothing known.
        let f = flow(
            &v,
            m(-60),
            Duration::minutes(10),
            Duration::minutes(60),
            0.99,
        );
        assert_eq!(f, Flow::default());
    }

    #[test]
    fn makers_fill_only_through_their_price_and_the_other_side_is_accounted() {
        use chrono::Timelike;
        let (obs, highs) = history();
        // The high's bucket trades at 0.95 (taker buys 0.952, sells 0.948),
        // the rest at 0.01. On every third day a taker sells the high's
        // bucket at 0.93 five minutes after each decision: through a bid at
        // 0.948, inside the order's life (cancelled 10′ before the report).
        let mut days: Vec<MarketDay> = last_days(&highs, 30)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|i, w| if i == w { 0.95 } else { 0.01 }))
            .collect();
        let mut through_days = HashSet::new();
        for md in days.iter_mut().step_by(3) {
            through_days.insert(md.date);
            let extra: Vec<MarketTrade> = md
                .trades
                .iter()
                .filter(|t| {
                    let l = t.at.with_timezone(&TZ);
                    t.bucket == md.winner
                        && !t.taker_buys_yes
                        && l.hour() >= 12
                        && (l.minute() == 0 || l.minute() == 30)
                })
                .map(|t| MarketTrade {
                    at: t.at + Duration::minutes(5),
                    yes_price: 0.93,
                    ..t.clone()
                })
                .collect();
            md.trades.extend(extra);
            md.trades.sort_by_key(|t| t.at);
        }
        let r = market_study(&obs, None, &days, &cfg());
        assert_eq!(r.strategies.len(), 2 * (2 * 3 * 2 + 3 + 3));
        let makers: Vec<&SimTrade> = r
            .sim_trades
            .iter()
            .filter(|t| t.strategy == "A maker" && t.structure == "current")
            .collect();
        assert!(!makers.is_empty(), "{:?}", r.strategy_verdict);
        let fee = |p: f64| 0.05 * p * (1.0 - p);
        for t in &makers {
            assert!(
                through_days.contains(&t.date),
                "no trade goes through 0.948 on {t:?}"
            );
            assert_eq!((t.side.as_str(), t.price), ("YES", 0.948));
            let want = 10.0 / 0.948 * (f64::from(u8::from(t.won)) - 0.948 + 0.25 * fee(0.948));
            assert!((t.pnl_usd - want).abs() < 1e-9, "{t:?}");
            assert_eq!(t.won, t.bucket == t.resolved);
        }
        let row = r
            .strategies
            .iter()
            .find(|x| x.strategy == "A maker" && x.structure == "current")
            .unwrap();
        assert!(!row.live && row.trades == makers.len() as u64);
        assert_eq!((row.window, row.range.as_str()), (60, "0.90–0.99"));
        assert!(
            r.strategy_verdict
                .iter()
                .any(|v| v.starts_with("Live rules as limit orders with the current structure")),
            "{:?}",
            r.strategy_verdict
        );

        // The other side of every trade: the study adds up to the tape.
        let (mut shares, mut pnl, mut n) = (0.0, 0.0, 0_u64);
        for md in &days {
            for t in md.trades.iter().filter(|t| t.shares > 0.0) {
                let won = (t.bucket == md.winner) == t.taker_buys_yes;
                let price = if t.taker_buys_yes {
                    t.yes_price
                } else {
                    1.0 - t.yes_price
                };
                shares += t.shares;
                pnl += t.shares * (f64::from(u8::from(won)) - price);
                n += 1;
            }
        }
        let study = &r.maker_taker;
        let all = &study.rows[0];
        assert_eq!(
            (all.family.as_str(), all.group.as_str()),
            ("all trades", "all")
        );
        assert_eq!(all.trades, n);
        assert!((all.shares - shares).abs() < 1e-6);
        assert!((all.taker_per_share - pnl / shares).abs() < 1e-12);
        assert!(all.maker_ci_low <= all.maker_net_per_share + 1e-12);
        assert!(all.maker_net_per_share <= all.maker_ci_high + 1e-12);
        // Each family splits the same trades.
        for family in [
            "the taker bought",
            "price the taker paid",
            "bucket against the reported high",
            "minutes to the next routine report",
            "local time of the trade",
        ] {
            let parts: Vec<&crate::market_makers::FlowRow> =
                study.rows.iter().filter(|x| x.family == family).collect();
            assert!(!parts.is_empty(), "{family}");
            assert_eq!(parts.iter().map(|x| x.trades).sum::<u64>(), n, "{family}");
        }
        assert!(
            r.verdict
                .iter()
                .any(|v| v.starts_with("Makers and takers:"))
        );
        let md = r.to_markdown();
        for section in [
            "## Makers and takers: the other side of every trade",
            "| **price the taker paid** |",
            "*A maker*, *B maker* and *E maker* post the live rules' orders",
            "| A maker | current | 60′ | 0.90–0.99 |",
        ] {
            assert!(md.contains(section), "missing {section}\n{md}");
        }
    }

    /// The peak times the study had on each of `dates`: those of the days
    /// before it, learned the same way.
    fn peak_times_before(
        obs: &[Observation],
        dates: &[NaiveDate],
    ) -> BTreeMap<NaiveDate, PeakTimes> {
        let mut by_day: BTreeMap<NaiveDate, Vec<&Observation>> = BTreeMap::new();
        for o in obs {
            by_day
                .entry(local_date(o.key.observed_at, TZ))
                .or_default()
                .push(o);
        }
        let mut e = TemperatureStateEngine::new(3);
        e.register_station(st(), TZ);
        let mut b = PeakTimesBuilder::new();
        let mut out = BTreeMap::new();
        for (d, os) in by_day {
            if dates.contains(&d) {
                out.insert(d, b.build());
            }
            for o in os {
                e.apply_observation(o);
            }
            let (_, end) = local_day_bounds(d, TZ);
            if let Some(s) = e.day_state(&st(), d, ViewKind::All, end)
                && let Some(h) = s.high
            {
                b.add_day(
                    d,
                    Season::from_month(d.month(), false),
                    &s.points,
                    h.value.round_half_up_whole(),
                );
            }
            e.prune(d);
        }
        out
    }

    fn minute_of(hm: &str) -> u16 {
        let (h, m) = hm.split_once(':').unwrap();
        h.parse::<u16>().unwrap() * 60 + m.parse::<u16>().unwrap()
    }

    #[test]
    fn strategy_f_buys_the_high_inside_the_slot_the_days_before_learned() {
        let (obs, highs) = history();
        // The METAR high's bucket trades at 0.93 all day, the others at 0.02;
        // the last day resolves one bucket higher than the METAR high.
        let mut days: Vec<MarketDay> = last_days(&highs, 30)
            .into_iter()
            .map(|(d, h)| market_day(d, h, &|i, w| if i == w { 0.93 } else { 0.02 }))
            .collect();
        let lost_day = days.last().unwrap().date;
        days.last_mut().unwrap().winner += 1;
        let dates: Vec<NaiveDate> = days.iter().map(|d| d.date).collect();
        assert!(dates.iter().all(|d| d.month() <= 2), "winter market days");
        let mut c = cfg();
        c.timeline_days = dates.clone();
        let r = market_study(&obs, None, &days, &c);

        // When the high is first reported, over the whole history.
        let pt = r.peak_times.as_ref().expect("peak times");
        let winter = pt.season(Season::Winter).unwrap();
        assert!(winter.days >= 100, "{winter:?}");
        assert_eq!(
            u64::from(pt.seasons.iter().map(|s| s.days).sum::<u32>() + pt.incomplete_days),
            r.history_days
        );

        // Every F trade: YES of the METAR high's bucket at its ask (0.93 +
        // 0.002), inside the winter slot of the days before it, one a day.
        let before = peak_times_before(&obs, &dates);
        let f: Vec<&SimTrade> = r.sim_trades.iter().filter(|t| t.strategy == "F").collect();
        assert!(f.len() >= 10, "{:?}", r.f_verdict);
        let fee = 0.05 * 0.932 * (1.0 - 0.932);
        let mut seen = HashSet::new();
        for t in &f {
            assert!(seen.insert(t.date), "one trade a day: {t:?}");
            assert_eq!(
                (t.side.as_str(), t.structure.as_str(), t.price),
                ("YES", "–", 0.932)
            );
            let (start, end) = before[&t.date].slot(Season::Winter, 0.5, 0.9).unwrap();
            // Filled from the tape, or at the knowledge time (report + 5′).
            let at = t
                .filled
                .as_deref()
                .map_or(minute_of(&t.report) + 5, minute_of);
            assert!((start..end).contains(&at), "{t:?} outside {start}–{end}");
            assert_eq!(t.won, t.date != lost_day, "{t:?}");
            let want = if t.won {
                100.0 * (1.0 - 0.932 - fee - 0.005)
            } else {
                -100.0 * (0.932 + fee + 0.005)
            };
            assert!((t.pnl_usd - want).abs() < 1e-9, "{t:?}");
            // The day's replay shows it at its report.
            let row = r
                .timelines
                .iter()
                .find(|x| x.date == t.date)
                .and_then(|x| x.rows.iter().find(|x| x.report == t.report))
                .unwrap();
            assert!(
                row.trades
                    .iter()
                    .any(|s| s.starts_with("F · 0.90–0.95: YES")),
                "{row:?}"
            );
        }

        // F's rows are its own, not among the $10 rows.
        assert_eq!(r.strategies.len(), 2 * (2 * 3 * 2 + 3 + 3));
        assert!(r.strategies.iter().all(|x| !x.strategy.starts_with('F')));
        assert_eq!(r.f_strategies.len(), 6);
        let row = |label: &str| r.f_strategies.iter().find(|x| x.strategy == label).unwrap();
        let live = row("F");
        assert!(live.live && r.f_strategies[0].strategy == "F");
        assert_eq!(live.trades, f.len() as u64);
        assert_eq!(live.wins, f.iter().filter(|t| t.won).count() as u64);
        // Asks of 0.932 are inside both ranges: up to 0.99 trades the same.
        assert_eq!(row("F · to 0.99").trades, live.trades);
        assert!((row("F · to 0.99").total_usd - live.total_usd).abs() < 1e-9);
        // Takers sell at the maker's bid (0.928), never through it.
        assert_eq!(row("F maker").trades, 0);
        assert!(
            r.f_verdict[0].starts_with("Strategy F (slot 50% → 90% quantile of the peak times, asks above 0.90 and ≤ 0.95; 100 shares a trade): "),
            "{:?}",
            r.f_verdict
        );
        let oos = r.f_out_of_sample.as_deref().unwrap();
        assert!(
            oos.starts_with("Strategy F out of sample: on the first 15 market days"),
            "{oos}"
        );
        assert!(r.verdict.iter().any(|v| v == oos), "{:?}", r.verdict);

        let md = r.to_markdown();
        for section in [
            "## When the day's high is first reported (strategy F)",
            "## Strategy F at traded prices",
            "| F **live** | 50% → 90% | 0.90–0.95 |",
            "| F maker | 50% → 90% | 0.90–0.95 | 0 |",
            "* Strategy F out of sample: on the first 15 market days",
        ] {
            assert!(md.contains(section), "missing {section}\n{md}");
        }
        let pos = |x: &str| md.find(x).unwrap();
        assert!(pos("## Strategies at traded prices") < pos("## When the day's high"));
        assert!(pos("## When the day's high") < pos("## Strategy F at traded prices"));
        if f.iter().any(|t| !t.won) {
            assert!(md.contains(&format!("| {lost_day} | ")), "{md}");
        }

        // The JSON keeps F; reports written before F still load.
        let json = serde_json::to_value(&r).unwrap();
        let back: MarketStudyReport = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back.peak_times, r.peak_times);
        assert_eq!(back.f_verdict, r.f_verdict);
        assert_eq!(back.f_out_of_sample, r.f_out_of_sample);
        assert_eq!(back.f_strategies.len(), 6);
        assert_eq!(back.sim.f, r.sim.f);
        let mut old = json;
        let o = old.as_object_mut().unwrap();
        for k in ["peak_times", "f_strategies", "f_verdict", "f_out_of_sample"] {
            o.remove(k);
        }
        o["sim"].as_object_mut().unwrap().remove("f");
        for t in o["sim_trades"].as_array_mut().unwrap() {
            t.as_object_mut().unwrap().remove("filled");
        }
        let old: MarketStudyReport = serde_json::from_value(old).unwrap();
        assert!(old.peak_times.is_none() && old.f_strategies.is_empty());
        assert!(old.sim_trades.iter().all(|t| t.filled.is_none()));
        assert_eq!(old.sim.f, crate::PeakSlotSim::default());

        // Strategies G–K: every rule has a row, every family a verdict, and
        // K says it was not replayed without KNMI readings.
        assert_eq!(
            r.gk_strategies.len(),
            crate::market_gk::rules(&r.sim.gk).len()
        );
        assert_eq!(r.gk_strategies.iter().filter(|x| x.live).count(), 5);
        assert_eq!(r.gk_verdict.len(), 5, "{:?}", r.gk_verdict);
        assert!(r.gk_verdict[4].starts_with("Strategy K: not replayed"));
        assert_eq!(r.knmi_days, 0);
        assert!(md.contains("## Strategies G–K at traded prices"), "{md}");
        assert!(md.contains("| G **live** | YES 0.01–0.08 |"), "{md}");
        assert!(!md.contains("| K **live**"), "no K rows without readings");
        assert!(pos("## Strategy F at traded prices") < pos("## Strategies G–K"));
        // Reports written before G–K still load.
        let mut older = serde_json::to_value(&r).unwrap();
        let o = older.as_object_mut().unwrap();
        for k in [
            "gk_strategies",
            "gk_verdict",
            "gk_out_of_sample",
            "knmi_accuracy",
            "knmi_days",
        ] {
            o.remove(k);
        }
        o["sim"].as_object_mut().unwrap().remove("gk");
        let older: MarketStudyReport = serde_json::from_value(older).unwrap();
        assert!(older.gk_strategies.is_empty() && older.knmi_days == 0);
        assert_eq!(older.sim.gk, crate::market_gk::GkSim::default());
    }

    #[test]
    fn weights_always_include_model_and_market() {
        let mut c = cfg();
        c.weights = vec![0.5, 0.5, 2.0];
        assert_eq!(c.weights(), vec![0.0, 0.5, 1.0]);
    }
}
