//! Does a fixed-lead forecast improve the peak model? — measured, not assumed.
//!
//! The evaluation is **prequential** (walk-forward, one day at a time): every
//! decision point of day D is scored with the model as trained on the days
//! *before* D, then D is added to the training counts. No day ever informs
//! its own prediction, and a day's forecast is used only from the product's
//! ready time — the rule live trading applies. Both predictions come from the
//! same model: the "base" prediction ignores the forecast (its refinement
//! level is skipped), the "forecast" prediction uses it, so the comparison
//! isolates exactly what the forecast adds.
//!
//! Decision points are the moments the strategies act on: the first report
//! at which a daytime high has been confirmed for `window_minutes`.
//!
//! A **placebo** control guards against improvements that do not come from
//! the day's forecast (e.g. a refinement level merely changing how strongly
//! cells are smoothed, or the diurnal shape any forecast carries): a second
//! model is trained and scored identically, but with the forecast of
//! [`PLACEBO_OFFSET_DAYS`] days earlier shifted onto the day — realistic
//! shape and season, no information about that day's weather.
//!
//! Adoption rule (fixed before looking at any result): at least `min_days`
//! scored days, and 95 % day-block bootstrap intervals of the change in
//! multi-class log loss per decision point that lie entirely below zero both
//! against the prediction without a forecast and against the placebo.
//! Everything else (Brier score, calibration, the constant-price trading
//! proxy) is reported, not optimized.

use crate::bootstrap_mean_ci;
use crate::research::wilson;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use wm_core::forecast::ForecastProduct;
use wm_core::market::FeeSchedule;
use wm_core::rng::SplitMix64;
use wm_core::units::Price;
use wm_strategy::ev::ev_per_share;
use wm_strategy::probability::rise_bucket;
use wm_strategy::{EmpiricalPeakModel, ForecastDay, PeakFeatures, ProbabilityModel};

/// A station's fixed-lead forecast history: one complete series per local
/// day, each known from the product's ready time on.
#[derive(Debug, Clone)]
pub struct ForecastHistory {
    pub product: ForecastProduct,
    pub days: BTreeMap<NaiveDate, ForecastDay>,
}

impl ForecastHistory {
    /// Cut a long hourly series (valid time, tenths °C) into local days.
    /// Days the series does not cover completely are left out.
    pub fn from_hourly(product: ForecastProduct, tz: Tz, hourly: &[(DateTime<Utc>, i32)]) -> Self {
        let mut series = hourly.to_vec();
        series.sort_by_key(|p| p.0);
        series.dedup_by_key(|p| p.0);
        let mut days = BTreeMap::new();
        if let (Some(first), Some(last)) = (series.first(), series.last()) {
            let mut date = wm_core::time::local_date(first.0, tz);
            let end = wm_core::time::local_date(last.0, tz);
            while date <= end {
                let (start, stop) = wm_core::time::local_day_bounds(date, tz);
                let lo = series.partition_point(|p| p.0 < start);
                let hi = series.partition_point(|p| p.0 <= stop);
                if let Some(day) = ForecastDay::from_series(
                    date,
                    tz,
                    &series[lo..hi],
                    product.usable_from(date, tz),
                ) {
                    days.insert(date, day);
                }
                let Some(next) = date.succ_opt() else { break };
                date = next;
            }
        }
        Self { product, days }
    }

    /// Placebo for `date`: the series of `offset_days` earlier shifted onto
    /// `date`, known from `date`'s ready time. `None` when that day has no
    /// series or the shifted one does not cover `date` (DST changes).
    pub fn placebo_day(&self, date: NaiveDate, tz: Tz, offset_days: i64) -> Option<ForecastDay> {
        let source = self
            .days
            .get(&(date - chrono::Duration::days(offset_days)))?;
        let shift = chrono::Duration::days(offset_days);
        let shifted: Vec<(DateTime<Utc>, i32)> = source
            .hourly
            .iter()
            .map(|(t, v)| (*t + shift, *v))
            .collect();
        ForecastDay::from_series(date, tz, &shifted, self.product.usable_from(date, tz))
    }
}

/// Days between a day and the forecast its placebo borrows. Two weeks is
/// beyond the predictability of day-to-day weather but keeps the season.
pub const PLACEBO_OFFSET_DAYS: i64 = 14;

/// Evaluation settings (engineering choices, fixed before evaluation).
#[derive(Debug, Clone)]
pub struct EvaluationConfig {
    /// Confirmation (minutes since the high) at which a decision is scored.
    pub window_minutes: u32,
    /// YES prices of the constant-price trading proxy.
    pub prices: Vec<Price>,
    pub fees: FeeSchedule,
    pub slippage: Price,
    /// Minimum EV per share for a proxy trade (the strategies' `min_edge`).
    pub min_edge: f64,
    pub bootstrap_iterations: usize,
    pub seed: u64,
    /// Adoption needs at least this many scored days.
    pub min_days: u64,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            window_minutes: 60,
            prices: [900_000, 930_000, 950_000, 970_000]
                .into_iter()
                .map(Price::saturating_from_micros)
                .collect(),
            fees: FeeSchedule::taker(50_000),
            slippage: Price::saturating_from_micros(5_000),
            min_edge: 0.01,
            bootstrap_iterations: 2_000,
            seed: 0x5EED_F0C4,
            min_days: 365,
        }
    }
}

/// P(final) by forecast rise bucket at the scored decision points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiseRow {
    pub bucket: String,
    pub points: u64,
    pub finals: u64,
    pub p_final: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    /// Mean predicted P(final) without and with the forecast.
    pub mean_p_base: f64,
    pub mean_p_forecast: f64,
}

/// Reliability of predicted P(final).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRow {
    pub model: String,
    pub bin: String,
    pub points: u64,
    pub mean_p: f64,
    pub final_rate: f64,
}

/// Constant-price proxy: if YES on the observed high were offered at
/// `price` at every decision point, which points would each prediction buy
/// (EV ≥ min_edge after fees and slippage) and what would they earn?
/// Real prices vary, so this compares the two predictions' trade selection;
/// it is not a forecast of real profit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyRow {
    pub price: f64,
    pub base_trades: u64,
    pub base_wins: u64,
    pub base_pnl: f64,
    pub forecast_trades: u64,
    pub forecast_wins: u64,
    pub forecast_pnl: f64,
    /// 95 % day-block bootstrap interval of `forecast_pnl − base_pnl`.
    pub diff_ci_low: f64,
    pub diff_ci_high: f64,
}

/// Result of the forecast evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastEvaluation {
    pub product: String,
    /// History days with a complete forecast series.
    pub days_with_forecast: u64,
    /// Days with at least one scored decision point.
    pub scored_days: u64,
    pub points: u64,
    pub first_scored: Option<NaiveDate>,
    pub last_scored: Option<NaiveDate>,
    pub window_minutes: u32,
    /// Mean multi-class log loss per decision point (lower is better).
    pub logloss_base: f64,
    pub logloss_forecast: f64,
    pub logloss_diff: f64,
    pub logloss_diff_ci_low: f64,
    pub logloss_diff_ci_high: f64,
    /// Brier score of P(final).
    pub brier_base: f64,
    pub brier_forecast: f64,
    /// Decision points that also have a placebo prediction.
    pub placebo_points: u64,
    /// Mean log loss with the placebo and of the real forecast on the same points.
    pub logloss_placebo: f64,
    pub logloss_forecast_on_placebo_points: f64,
    /// Real forecast minus placebo (negative = the day's forecast adds information).
    pub placebo_diff: f64,
    pub placebo_diff_ci_low: f64,
    pub placebo_diff_ci_high: f64,
    pub rise: Vec<RiseRow>,
    pub calibration: Vec<CalibrationRow>,
    pub proxy: Vec<ProxyRow>,
    pub min_days: u64,
    pub adopted: bool,
    pub verdict: String,
}

/// One scored decision point.
#[derive(Debug, Clone)]
struct Point {
    date: NaiveDate,
    /// Observed increment class.
    k: usize,
    rise: i32,
    base: Vec<f64>,
    forecast: Vec<f64>,
    placebo: Option<Vec<f64>>,
}

impl Point {
    fn is_final(&self) -> bool {
        self.k == 0
    }
}

fn logloss(probs: &[f64], k: usize) -> f64 {
    -probs.get(k).copied().unwrap_or(0.0).max(1e-9).ln()
}

fn p_final(probs: &[f64]) -> f64 {
    probs.first().copied().unwrap_or(0.0)
}

/// Collects decision points during the prequential pass.
#[derive(Debug)]
pub(crate) struct Scorer {
    cfg: EvaluationConfig,
    points: Vec<Point>,
}

impl Scorer {
    pub(crate) fn new(cfg: EvaluationConfig) -> Self {
        Self {
            cfg,
            points: Vec::new(),
        }
    }

    pub(crate) fn window(&self) -> u32 {
        self.cfg.window_minutes
    }

    /// Score one decision point with the models as trained on earlier days
    /// only. Points without a usable forecast are not scored (both
    /// predictions would be identical).
    pub(crate) fn score(
        &mut self,
        date: NaiveDate,
        model: &EmpiricalPeakModel,
        placebo_model: &EmpiricalPeakModel,
        f: &PeakFeatures,
        placebo_rise: Option<i32>,
        increment: i32,
    ) {
        let Some(rise) = f.forecast_rise_tenths else {
            return;
        };
        let without = PeakFeatures {
            forecast_rise_tenths: None,
            ..f.clone()
        };
        let (Some(base), Some(forecast)) = (model.distribution(&without), model.distribution(f))
        else {
            return;
        };
        let placebo = placebo_rise.and_then(|r| {
            let pf = PeakFeatures {
                forecast_rise_tenths: Some(r),
                ..f.clone()
            };
            placebo_model.distribution(&pf).map(|d| d.probs)
        });
        let k = (increment.max(0) as usize).min(base.probs.len().saturating_sub(1));
        self.points.push(Point {
            date,
            k,
            rise,
            base: base.probs,
            forecast: forecast.probs,
            placebo,
        });
    }

    pub(crate) fn finish(
        self,
        product: &ForecastProduct,
        days_with_forecast: u64,
    ) -> ForecastEvaluation {
        let cfg = &self.cfg;
        let pts = &self.points;
        // Per-day sums for day-block bootstrap: (Σ ll_f − ll_b, n).
        let mut per_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
        let (mut ll_b, mut ll_f, mut br_b, mut br_f) = (0.0, 0.0, 0.0, 0.0);
        for p in pts {
            let (b, f) = (logloss(&p.base, p.k), logloss(&p.forecast, p.k));
            ll_b += b;
            ll_f += f;
            let y = if p.is_final() { 1.0 } else { 0.0 };
            br_b += (p_final(&p.base) - y).powi(2);
            br_f += (p_final(&p.forecast) - y).powi(2);
            let e = per_day.entry(p.date).or_insert((0.0, 0.0));
            e.0 += f - b;
            e.1 += 1.0;
        }
        let n = pts.len() as f64;
        let mean = |x: f64| if n > 0.0 { x / n } else { 0.0 };
        let day_sums: Vec<(f64, f64)> = per_day.values().copied().collect();
        let (ci_low, ci_high) = ratio_ci(&day_sums, cfg.bootstrap_iterations, cfg.seed);
        let scored_days = per_day.len() as u64;
        let diff = mean(ll_f - ll_b);

        // Real forecast vs placebo, on the points that have both.
        let mut placebo_days: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
        let (mut ll_p, mut ll_fp, mut np) = (0.0, 0.0, 0.0);
        for p in pts {
            let Some(pl) = &p.placebo else { continue };
            let (f, q) = (logloss(&p.forecast, p.k), logloss(pl, p.k));
            ll_fp += f;
            ll_p += q;
            np += 1.0;
            let e = placebo_days.entry(p.date).or_insert((0.0, 0.0));
            e.0 += f - q;
            e.1 += 1.0;
        }
        let pmean = |x: f64| if np > 0.0 { x / np } else { 0.0 };
        let placebo_sums: Vec<(f64, f64)> = placebo_days.values().copied().collect();
        let (pl_low, pl_high) =
            ratio_ci(&placebo_sums, cfg.bootstrap_iterations, cfg.seed ^ 0x9E37);
        let pl_diff = pmean(ll_fp - ll_p);

        let enough = scored_days >= cfg.min_days && !pts.is_empty();
        let beats_base = ci_high < 0.0;
        let beats_placebo = np > 0.0 && pl_high < 0.0;
        let adopted = enough && beats_base && beats_placebo;
        let verdict = if !enough {
            format!(
                "not adopted: {scored_days} scored days with a forecast, {} needed",
                cfg.min_days
            )
        } else if adopted {
            format!(
                "adopted: log loss {diff:+.4} per decision (95% CI {ci_low:+.4} … {ci_high:+.4}), {pl_diff:+.4} vs placebo (95% CI {pl_low:+.4} … {pl_high:+.4}), over {scored_days} days, {} decisions",
                pts.len()
            )
        } else if beats_base {
            format!(
                "not adopted: the gain is not specific to the day's forecast — {pl_diff:+.4} vs placebo (95% CI {pl_low:+.4} … {pl_high:+.4}) over {scored_days} days"
            )
        } else {
            format!(
                "not adopted: no clear improvement — log loss {diff:+.4} per decision (95% CI {ci_low:+.4} … {ci_high:+.4}) over {scored_days} days"
            )
        };
        ForecastEvaluation {
            product: product.label(),
            days_with_forecast,
            scored_days,
            points: pts.len() as u64,
            first_scored: per_day.keys().next().copied(),
            last_scored: per_day.keys().next_back().copied(),
            window_minutes: cfg.window_minutes,
            logloss_base: mean(ll_b),
            logloss_forecast: mean(ll_f),
            logloss_diff: diff,
            logloss_diff_ci_low: ci_low,
            logloss_diff_ci_high: ci_high,
            brier_base: mean(br_b),
            brier_forecast: mean(br_f),
            placebo_points: np as u64,
            logloss_placebo: pmean(ll_p),
            logloss_forecast_on_placebo_points: pmean(ll_fp),
            placebo_diff: pl_diff,
            placebo_diff_ci_low: pl_low,
            placebo_diff_ci_high: pl_high,
            rise: rise_rows(pts),
            calibration: calibration_rows(pts),
            proxy: proxy_rows(pts, cfg),
            min_days: cfg.min_days,
            adopted,
            verdict,
        }
    }
}

/// 95 % day-block bootstrap interval of Σa / Σb.
fn ratio_ci(days: &[(f64, f64)], iterations: usize, seed: u64) -> (f64, f64) {
    if days.is_empty() || iterations == 0 {
        return (0.0, 0.0);
    }
    let mut rng = SplitMix64::new(seed);
    let len = days.len() as u64;
    let mut stats: Vec<f64> = (0..iterations)
        .map(|_| {
            let (mut a, mut b) = (0.0, 0.0);
            for _ in 0..days.len() {
                let (x, w) = days[rng.next_below(len) as usize];
                a += x;
                b += w;
            }
            if b > 0.0 { a / b } else { 0.0 }
        })
        .collect();
    stats.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let at = |q: f64| stats[((stats.len() - 1) as f64 * q).round() as usize];
    (at(0.025), at(0.975))
}

fn rise_rows(pts: &[Point]) -> Vec<RiseRow> {
    let mut groups: BTreeMap<&'static str, Vec<&Point>> = BTreeMap::new();
    for p in pts {
        groups.entry(rise_bucket(p.rise)).or_default().push(p);
    }
    let order = ["cool2", "cool", "flat", "warm"];
    order
        .iter()
        .filter_map(|b| groups.get(b).map(|g| (*b, g)))
        .map(|(bucket, g)| {
            let n = g.len() as u64;
            let finals = g.iter().filter(|p| p.is_final()).count() as u64;
            let (ci_low, ci_high) = wilson(finals, n);
            let avg = |f: &dyn Fn(&Point) -> f64| g.iter().map(|p| f(p)).sum::<f64>() / n as f64;
            RiseRow {
                bucket: bucket.to_owned(),
                points: n,
                finals,
                p_final: finals as f64 / n as f64,
                ci_low,
                ci_high,
                mean_p_base: avg(&|p| p_final(&p.base)),
                mean_p_forecast: avg(&|p| p_final(&p.forecast)),
            }
        })
        .collect()
}

fn calibration_rows(pts: &[Point]) -> Vec<CalibrationRow> {
    const BINS: [(f64, f64, &str); 5] = [
        (0.0, 0.8, "< 0.80"),
        (0.8, 0.9, "0.80–0.90"),
        (0.9, 0.95, "0.90–0.95"),
        (0.95, 0.98, "0.95–0.98"),
        (0.98, 1.01, "≥ 0.98"),
    ];
    let mut rows = Vec::new();
    for (name, pick) in [
        ("base", (|p: &Point| p_final(&p.base)) as fn(&Point) -> f64),
        ("forecast", |p: &Point| p_final(&p.forecast)),
    ] {
        for (lo, hi, label) in BINS {
            let g: Vec<&Point> = pts.iter().filter(|p| (lo..hi).contains(&pick(p))).collect();
            if g.is_empty() {
                continue;
            }
            let n = g.len() as f64;
            rows.push(CalibrationRow {
                model: name.to_owned(),
                bin: label.to_owned(),
                points: g.len() as u64,
                mean_p: g.iter().map(|p| pick(p)).sum::<f64>() / n,
                final_rate: g.iter().filter(|p| p.is_final()).count() as f64 / n,
            });
        }
    }
    rows
}

fn proxy_rows(pts: &[Point], cfg: &EvaluationConfig) -> Vec<ProxyRow> {
    cfg.prices
        .iter()
        .map(|&price| {
            let trade = |p: f64| ev_per_share(p, price, &cfg.fees, cfg.slippage) >= cfg.min_edge;
            let pnl = |won: bool| {
                ev_per_share(if won { 1.0 } else { 0.0 }, price, &cfg.fees, cfg.slippage)
            };
            let mut row = ProxyRow {
                price: price.as_f64(),
                base_trades: 0,
                base_wins: 0,
                base_pnl: 0.0,
                forecast_trades: 0,
                forecast_wins: 0,
                forecast_pnl: 0.0,
                diff_ci_low: 0.0,
                diff_ci_high: 0.0,
            };
            let mut per_day: BTreeMap<NaiveDate, f64> = BTreeMap::new();
            for p in pts {
                let won = p.is_final();
                let mut d = 0.0;
                if trade(p_final(&p.base)) {
                    row.base_trades += 1;
                    row.base_wins += u64::from(won);
                    row.base_pnl += pnl(won);
                    d -= pnl(won);
                }
                if trade(p_final(&p.forecast)) {
                    row.forecast_trades += 1;
                    row.forecast_wins += u64::from(won);
                    row.forecast_pnl += pnl(won);
                    d += pnl(won);
                }
                *per_day.entry(p.date).or_insert(0.0) += d;
            }
            let diffs: Vec<f64> = per_day.values().copied().collect();
            let (lo, hi) = bootstrap_mean_ci(&diffs, cfg.bootstrap_iterations, cfg.seed ^ 0xA5);
            let days = diffs.len() as f64;
            row.diff_ci_low = lo * days;
            row.diff_ci_high = hi * days;
            row
        })
        .collect()
}

impl ForecastEvaluation {
    pub fn to_markdown(&self) -> String {
        let mut s = format!(
            "\n## Forecast evaluation — {}\n\n**Verdict: {}**\n\nPrequential (walk-forward) test: each decision point is scored with the model trained on earlier days only; a day's forecast is used only from its ready time, as in live trading. Decision point = first report with the daytime high confirmed for {} minutes.\n\n",
            self.product, self.verdict, self.window_minutes
        );
        let _ = writeln!(
            s,
            "History days with a forecast: {} · scored days: {} ({} → {}) · decision points: {} · adoption needs ≥ {} days and a log-loss interval below zero.\n",
            self.days_with_forecast,
            self.scored_days,
            self.first_scored.map(|d| d.to_string()).unwrap_or_default(),
            self.last_scored.map(|d| d.to_string()).unwrap_or_default(),
            self.points,
            self.min_days
        );
        let _ = writeln!(
            s,
            "| metric | without forecast | with forecast | change (95 % CI) |\n|---|---:|---:|---|\n| log loss per decision | {:.4} | {:.4} | {:+.4} ({:+.4} … {:+.4}) |\n| Brier score of P(final) | {:.4} | {:.4} | {:+.4} |\n",
            self.logloss_base,
            self.logloss_forecast,
            self.logloss_diff,
            self.logloss_diff_ci_low,
            self.logloss_diff_ci_high,
            self.brier_base,
            self.brier_forecast,
            self.brier_forecast - self.brier_base
        );
        let _ = writeln!(
            s,
            "Placebo control (forecast of {PLACEBO_OFFSET_DAYS} days earlier, same model and scoring), {} decision points: log loss {:.4} with the placebo vs {:.4} with the day's forecast — change {:+.4} (95 % CI {:+.4} … {:+.4}).\n",
            self.placebo_points,
            self.logloss_placebo,
            self.logloss_forecast_on_placebo_points,
            self.placebo_diff,
            self.placebo_diff_ci_low,
            self.placebo_diff_ci_high
        );
        s.push_str("### P(high is final) by forecast rise\n\n| rise | decisions | final | P(final) | 95 % CI | mean P without | mean P with |\n|---|---:|---:|---:|---|---:|---:|\n");
        for r in &self.rise {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {:.4} | [{:.4}, {:.4}] | {:.4} | {:.4} |",
                r.bucket,
                r.points,
                r.finals,
                r.p_final,
                r.ci_low,
                r.ci_high,
                r.mean_p_base,
                r.mean_p_forecast
            );
        }
        s.push_str("\n### Calibration of P(final)\n\n| prediction | bin | decisions | mean P | observed final rate |\n|---|---|---:|---:|---:|\n");
        for c in &self.calibration {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {:.4} | {:.4} |",
                c.model, c.bin, c.points, c.mean_p, c.final_rate
            );
        }
        s.push_str("\n### Constant-price proxy (YES on the observed high)\n\nIf YES were offered at this price at every decision point: trades taken (EV ≥ min edge after fees and slippage), wins and P&L per share-sum. Compares trade selection only — real prices vary.\n\n| price | trades without | wins | P&L | trades with | wins | P&L | P&L change (95 % CI) |\n|---:|---:|---:|---:|---:|---:|---:|---|\n");
        for p in &self.proxy {
            let _ = writeln!(
                s,
                "| {:.2} | {} | {} | {:+.2} | {} | {} | {:+.2} | {:+.2} ({:+.2} … {:+.2}) |",
                p.price,
                p.base_trades,
                p.base_wins,
                p.base_pnl,
                p.forecast_trades,
                p.forecast_wins,
                p.forecast_pnl,
                p.forecast_pnl - p.base_pnl,
                p.diff_ci_low,
                p.diff_ci_high
            );
        }
        s
    }
}
