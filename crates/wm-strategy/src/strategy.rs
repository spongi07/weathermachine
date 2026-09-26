//! Strategy interface, shared helpers and strategies A (BUY YES), B (BUY NO)
//! and C (pre-confirmation two-sided exposure, research only).
//!
//! Strategies are pure: they read an immutable [`StrategyContext`] and return
//! proposals plus per-bucket evaluations (for the dashboard and audit log).
//! They cannot place orders — proposals go through the risk engine.

use crate::ev::{break_even_probability, ev_per_share};
use crate::peak::PeakAssessment;
use crate::probability::IncrementDistribution;
use crate::state::ViewKind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use wm_core::ids::{ConditionId, LocationId, StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, FeeSchedule, MarketOutcome, OrderBook, OutcomeSide, Side, TemperatureBucket};
use wm_core::portfolio::PositionBook;
use wm_core::trading::{IntentKind, RunMode, TimeInForce};
use wm_core::units::{Price, Rounding, Shares, Usd, round_shares_to_lot, shares_for_notional};

/// Assessment and model output for one resolution view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewEvaluation {
    pub view: ViewKind,
    pub assessment: PeakAssessment,
    pub distribution: Option<IncrementDistribution>,
}

/// Read-only inputs to a strategy evaluation.
pub struct StrategyContext<'a> {
    pub now: DateTime<Utc>,
    pub mode: RunMode,
    pub location: &'a LocationId,
    pub market: &'a DailyTemperatureMarket,
    pub books: &'a HashMap<TokenId, OrderBook>,
    /// One entry per candidate resolution view (≥ 1).
    pub views: &'a [ViewEvaluation],
    pub positions: &'a PositionBook,
    /// Tokens with live orders (no stacking of orders).
    pub pending_tokens: &'a HashSet<TokenId>,
}

/// A strategy's trade proposal (becomes a `TradeIntent` after id assignment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub strategy: StrategyId,
    pub bucket_label: String,
    pub bucket: TemperatureBucket,
    pub condition_id: ConditionId,
    pub token: TokenId,
    pub outcome_side: OutcomeSide,
    pub side: Side,
    pub kind: IntentKind,
    pub weather_dependent: bool,
    pub limit_price: Price,
    pub shares: Shares,
    pub tif: TimeInForce,
    pub p_win: f64,
    pub ev_per_share: f64,
    pub break_even: f64,
    pub research_only: bool,
    pub rationale: Vec<String>,
}

/// Evaluation of one bucket/side (shown in the market ladder even when no signal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BucketEvaluation {
    pub strategy: StrategyId,
    pub bucket_label: String,
    pub outcome_side: OutcomeSide,
    pub token: TokenId,
    pub ask: Option<Price>,
    pub bid: Option<Price>,
    pub p_win: Option<f64>,
    pub ev_per_share: Option<f64>,
    pub break_even: Option<f64>,
    pub signal: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrategyOutput {
    pub proposals: Vec<Proposal>,
    pub evaluations: Vec<BucketEvaluation>,
}

/// A strategy.
pub trait Strategy: Send {
    fn id(&self) -> &StrategyId;
    fn research_only(&self) -> bool {
        false
    }
    fn enabled(&self) -> bool;
    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput;
}

// ---------------------------------------------------------------------------
// Shared, conservative helpers
// ---------------------------------------------------------------------------

/// All views must agree on the current high (whole degrees).
pub fn common_high(views: &[ViewEvaluation]) -> Result<i32, String> {
    let mut high: Option<i32> = None;
    for v in views {
        let h = v.assessment.features.high_whole;
        match high {
            None => high = Some(h),
            Some(x) if x == h => {}
            Some(x) => return Err(format!("views disagree on high ({x} vs {h})")),
        }
    }
    high.ok_or_else(|| "no view has a high".to_owned())
}

/// Minimum observed minutes since the high across views.
pub fn min_minutes_since_high(views: &[ViewEvaluation]) -> i64 {
    views.iter().map(|v| v.assessment.features.minutes_since_high).min().unwrap_or(0)
}

/// Maximum data age across views.
pub fn max_data_age(views: &[ViewEvaluation]) -> i64 {
    views.iter().map(|v| v.assessment.features.data_age_minutes).max().unwrap_or(i64::MAX)
}

/// Conservative P(final ∈ bucket) as a *win* probability: minimum lower bound
/// across views, plus the minimum model support. `None` if any view lacks a model.
pub fn min_p_in_bucket(views: &[ViewEvaluation], high: i32, bucket: &TemperatureBucket) -> Option<(f64, u32)> {
    let mut p = f64::INFINITY;
    let mut support = u32::MAX;
    for v in views {
        let d = v.distribution.as_ref()?;
        p = p.min(d.p_in_bucket_lower(high, bucket));
        support = support.min(d.support);
    }
    (p.is_finite()).then_some((p, support))
}

/// Conservative P(final ∈ bucket) as a *loss* probability: maximum upper bound.
pub fn max_p_in_bucket(views: &[ViewEvaluation], high: i32, bucket: &TemperatureBucket) -> Option<(f64, u32)> {
    let mut p: f64 = 0.0;
    let mut support = u32::MAX;
    let mut any = false;
    for v in views {
        let d = v.distribution.as_ref()?;
        p = p.max(d.p_in_bucket_upper(high, bucket));
        support = support.min(d.support);
        any = true;
    }
    any.then_some((p, support))
}

/// Whole-share quantity for a notional at a price, respecting the market minimum.
pub fn size_for(notional: Usd, price: Price, min_size: Shares) -> Option<Shares> {
    let raw = shares_for_notional(notional, price, Rounding::Down);
    let shares = round_shares_to_lot(raw, Shares::from_whole(1), Rounding::Down);
    (shares.micros() > 0 && shares >= min_size).then_some(shares)
}

fn holds_or_pending(ctx: &StrategyContext<'_>, token: &TokenId) -> bool {
    ctx.pending_tokens.contains(token) || ctx.positions.get(token).is_some_and(|p| p.shares.micros() > 0)
}

// ---------------------------------------------------------------------------
// Strategy A — BUY YES on the observed high
// ---------------------------------------------------------------------------

/// Configuration of strategy A. Thresholds are research parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuyYesConfig {
    pub enabled: bool,
    pub min_price: Price,
    pub max_price: Price,
    pub min_confirmation_minutes: i64,
    pub min_edge: f64,
    pub min_model_support: u32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    pub slippage_allowance: Price,
    pub notional: Usd,
}

impl Default for BuyYesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_price: Price::saturating_from_micros(900_000),
            max_price: Price::saturating_from_micros(990_000),
            min_confirmation_minutes: 60,
            min_edge: 0.01,
            min_model_support: 50,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
            notional: Usd::from_whole(10),
        }
    }
}

/// Strategy A.
pub struct BuyYesFinalHigh {
    id: StrategyId,
    pub config: BuyYesConfig,
    fees: FeeSchedule,
}

impl BuyYesFinalHigh {
    pub fn new(config: BuyYesConfig, fees: FeeSchedule) -> Self {
        Self { id: StrategyId::from_static("A_buy_yes_final_high"), config, fees }
    }
}

impl Strategy for BuyYesFinalHigh {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        let high = match common_high(ctx.views) {
            Ok(h) => h,
            Err(_) => return out,
        };
        let Some(outcome) = ctx.market.outcome_for_value(high) else { return out };
        let token = &outcome.yes_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask);
        let bid = book.and_then(OrderBook::best_bid);
        let mut blockers = Vec::new();
        let p = min_p_in_bucket(ctx.views, high, &outcome.bucket);
        let price = ask.map(|a| a.price);
        let (ev, be) = match (p, price) {
            (Some((pw, _)), Some(pr)) => (
                Some(ev_per_share(pw, pr, &self.fees, self.config.slippage_allowance)),
                Some(break_even_probability(pr, &self.fees, self.config.slippage_allowance)),
            ),
            _ => (None, price.map(|pr| break_even_probability(pr, &self.fees, self.config.slippage_allowance))),
        };
        if !self.config.enabled {
            blockers.push("strategy disabled".into());
        }
        if p.is_none() {
            blockers.push("no probability model".into());
        }
        if let Some((_, support)) = p
            && support < self.config.min_model_support
        {
            blockers.push(format!("model support {support} < {}", self.config.min_model_support));
        }
        let msh = min_minutes_since_high(ctx.views);
        if msh < self.config.min_confirmation_minutes {
            blockers.push(format!("confirmation {msh}m < {}m", self.config.min_confirmation_minutes));
        }
        if max_data_age(ctx.views) > self.config.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }
        match (book, price) {
            (None, _) => blockers.push("no order book".into()),
            (Some(_), None) => blockers.push("no ask".into()),
            (Some(b), Some(pr)) => {
                if b.age_ms(ctx.now) > self.config.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if pr < self.config.min_price || pr > self.config.max_price {
                    blockers.push(format!("ask {pr} outside [{}, {}]", self.config.min_price, self.config.max_price));
                }
            }
        }
        if let Some(e) = ev
            && e < self.config.min_edge
        {
            blockers.push(format!("edge {e:.4} < {:.4}", self.config.min_edge));
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let shares = price.and_then(|pr| size_for(self.config.notional, pr, outcome.min_order_size));
        if price.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: OutcomeSide::Yes,
            token: token.clone(),
            ask: price,
            bid: bid.map(|b| b.price),
            p_win: p.map(|x| x.0),
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
        });
        if signal
            && let (Some((pw, support)), Some(pr), Some(sh), Some(e), Some(b)) = (p, price, shares, ev, be)
        {
            out.proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: outcome.label.clone(),
                bucket: outcome.bucket,
                condition_id: outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::Yes,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: pr,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: pw,
                ev_per_share: e,
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!("high {high}{} confirmed {msh}m", ctx.market.unit.symbol()),
                    format!("p_win {pw:.4} (support {support}) vs break-even {b:.4}"),
                ],
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Strategy B — BUY NO on buckets above the observed high
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuyNoConfig {
    pub enabled: bool,
    /// Distances above the high to consider (observed_high + k).
    pub distances: Vec<i32>,
    pub min_price: Price,
    pub max_price: Price,
    pub min_confirmation_minutes: i64,
    pub min_edge: f64,
    pub min_model_support: u32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    pub slippage_allowance: Price,
    pub notional: Usd,
}

impl Default for BuyNoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            distances: vec![1, 2, 3],
            min_price: Price::saturating_from_micros(900_000),
            max_price: Price::saturating_from_micros(990_000),
            min_confirmation_minutes: 60,
            min_edge: 0.01,
            min_model_support: 50,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
            notional: Usd::from_whole(10),
        }
    }
}

/// Strategy B.
pub struct BuyNoAboveHigh {
    id: StrategyId,
    pub config: BuyNoConfig,
    fees: FeeSchedule,
}

impl BuyNoAboveHigh {
    pub fn new(config: BuyNoConfig, fees: FeeSchedule) -> Self {
        Self { id: StrategyId::from_static("B_buy_no_above_high"), config, fees }
    }
}

impl Strategy for BuyNoAboveHigh {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        let Ok(high) = common_high(ctx.views) else { return out };
        let msh = min_minutes_since_high(ctx.views);
        let mut seen: HashSet<ConditionId> = HashSet::new();
        for k in &self.config.distances {
            let Some(outcome) = ctx.market.outcome_for_value(high + k) else { continue };
            if outcome.bucket.contains(high) || !seen.insert(outcome.condition_id.clone()) {
                continue; // bucket also contains the current high, or already evaluated
            }
            out.evaluations.push(self.evaluate_bucket(ctx, outcome, high, msh, *k, &mut out.proposals));
        }
        out
    }
}

impl BuyNoAboveHigh {
    fn evaluate_bucket(&self, ctx: &StrategyContext<'_>, outcome: &MarketOutcome, high: i32, msh: i64, k: i32, proposals: &mut Vec<Proposal>) -> BucketEvaluation {
        let token = &outcome.no_token;
        let book = ctx.books.get(token);
        let price = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let loss = max_p_in_bucket(ctx.views, high, &outcome.bucket);
        let p_win = loss.map(|(pl, _)| 1.0 - pl);
        let ev = match (p_win, price) {
            (Some(pw), Some(pr)) => Some(ev_per_share(pw, pr, &self.fees, self.config.slippage_allowance)),
            _ => None,
        };
        let be = price.map(|pr| break_even_probability(pr, &self.fees, self.config.slippage_allowance));
        let mut blockers = Vec::new();
        if !self.config.enabled {
            blockers.push("strategy disabled".into());
        }
        match loss {
            None => blockers.push("no probability model".into()),
            Some((_, s)) if s < self.config.min_model_support => blockers.push(format!("model support {s} < {}", self.config.min_model_support)),
            _ => {}
        }
        if msh < self.config.min_confirmation_minutes {
            blockers.push(format!("confirmation {msh}m < {}m", self.config.min_confirmation_minutes));
        }
        if max_data_age(ctx.views) > self.config.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }
        match (book, price) {
            (None, _) => blockers.push("no order book".into()),
            (Some(_), None) => blockers.push("no ask".into()),
            (Some(b), Some(pr)) => {
                if b.age_ms(ctx.now) > self.config.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if pr < self.config.min_price || pr > self.config.max_price {
                    blockers.push(format!("ask {pr} outside [{}, {}]", self.config.min_price, self.config.max_price));
                }
            }
        }
        if let Some(e) = ev
            && e < self.config.min_edge
        {
            blockers.push(format!("edge {e:.4} < {:.4}", self.config.min_edge));
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let shares = price.and_then(|pr| size_for(self.config.notional, pr, outcome.min_order_size));
        if price.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }
        let signal = blockers.is_empty();
        if signal
            && let (Some(pw), Some(pr), Some(sh), Some(e), Some(b)) = (p_win, price, shares, ev, be)
        {
            proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: outcome.label.clone(),
                bucket: outcome.bucket,
                condition_id: outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::No,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: pr,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: pw,
                ev_per_share: e,
                break_even: b,
                research_only: false,
                rationale: vec![format!("NO on high+{k} ({}) ; high {high} confirmed {msh}m", outcome.label), format!("p_win {pw:.4} vs break-even {b:.4}")],
            });
        }
        BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: OutcomeSide::No,
            token: token.clone(),
            ask: price,
            bid,
            p_win,
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy C — pre-confirmation two-sided exposure + dynamic unwind (research)
// ---------------------------------------------------------------------------

/// Strategy C holds YES on the current high X *and* on X+1 before the peak is
/// confirmed ("adjacent straddle"; economically the multi-bucket analogue of a
/// CTF split), then relies on the [`crate::unwind::UnwindEngine`] to exit the
/// unfavoured leg. It is research-only: the risk engine rejects its intents
/// outside backtests until the research shows it beats "wait → confirm → enter".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplitUnwindConfig {
    pub enabled: bool,
    /// Enter only while confirmation is shorter than this (pre-confirmation).
    pub max_confirmation_minutes: i64,
    pub min_combined_p: f64,
    /// Maximum combined ask of both legs.
    pub max_combined_price: Price,
    pub notional_per_leg: Usd,
    pub max_data_age_minutes: i64,
}

impl Default for SplitUnwindConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_confirmation_minutes: 30,
            min_combined_p: 0.90,
            max_combined_price: Price::saturating_from_micros(950_000),
            notional_per_leg: Usd::from_whole(5),
            max_data_age_minutes: 40,
        }
    }
}

pub struct SplitUnwind {
    id: StrategyId,
    pub config: SplitUnwindConfig,
}

impl SplitUnwind {
    pub fn new(config: SplitUnwindConfig) -> Self {
        Self { id: StrategyId::from_static("C_split_unwind"), config }
    }
}

impl Strategy for SplitUnwind {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn research_only(&self) -> bool {
        true
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        if !self.config.enabled {
            return out;
        }
        let Ok(high) = common_high(ctx.views) else { return out };
        if min_minutes_since_high(ctx.views) >= self.config.max_confirmation_minutes || max_data_age(ctx.views) > self.config.max_data_age_minutes {
            return out;
        }
        let (Some(a), Some(b)) = (ctx.market.outcome_for_value(high), ctx.market.outcome_for_value(high + 1)) else { return out };
        if a.condition_id == b.condition_id {
            return out;
        }
        let asks: Vec<Option<Price>> = [a, b].iter().map(|o| ctx.books.get(&o.yes_token).and_then(OrderBook::best_ask).map(|l| l.price)).collect();
        let (Some(pa), Some(pb)) = (asks[0], asks[1]) else { return out };
        let combined = pa.saturating_add(pb);
        let (Some((p_a, _)), Some((p_b, _))) = (min_p_in_bucket(ctx.views, high, &a.bucket), min_p_in_bucket(ctx.views, high, &b.bucket)) else { return out };
        if p_a + p_b < self.config.min_combined_p || combined > self.config.max_combined_price {
            return out;
        }
        if holds_or_pending(ctx, &a.yes_token) || holds_or_pending(ctx, &b.yes_token) {
            return out;
        }
        for (o, pr, p) in [(a, pa, p_a), (b, pb, p_b)] {
            if let Some(sh) = size_for(self.config.notional_per_leg, pr, o.min_order_size) {
                out.proposals.push(Proposal {
                    strategy: self.id.clone(),
                    bucket_label: o.label.clone(),
                    bucket: o.bucket,
                    condition_id: o.condition_id.clone(),
                    token: o.yes_token.clone(),
                    outcome_side: OutcomeSide::Yes,
                    side: Side::Buy,
                    kind: IntentKind::Open,
                    weather_dependent: true,
                    limit_price: pr,
                    shares: sh,
                    tif: TimeInForce::Fak,
                    p_win: p,
                    ev_per_share: p - pr.as_f64(),
                    break_even: pr.as_f64(),
                    research_only: true,
                    rationale: vec![format!("pre-confirmation straddle {high}/{}; combined p {:.3} ask {combined}", high + 1, p_a + p_b)],
                });
            }
        }
        out
    }
}
