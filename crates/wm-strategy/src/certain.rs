//! Strategy D — outcomes the observations have already decided.
//!
//! The daily high can only rise. Once every candidate resolution view has
//! observed a high of `H` whole degrees, a bucket that lies entirely below `H`
//! cannot be the answer (its NO wins), and an open-ended top bucket "≥ L" with
//! `L ≤ H` is the answer (its YES wins). No probability model is involved.
//!
//! The edge is speed: right after a report raises the high, quotes on the
//! bucket that just died can still be stale. The strategy acts on the
//! observation event itself, buys only when the ask leaves `min_edge` after
//! fee and slippage, and never pays more than `max_price`. The default edge
//! (0.02) skips long-dead buckets quoted at 0.98–0.99: a settlement discount
//! of well under a cent per share is not worth the resolution risk or the
//! day's exposure budget it would use up.
//!
//! Residual risks, handled elsewhere or by margin: an erroneous report that is
//! later corrected (the risk engine blocks trading after corrections; a high
//! that jumps implausibly from the previous report must be repeated before it
//! is trusted), and a resolution source that omits a report (only views that
//! all agree count).

use crate::ev::{break_even_probability, ev_per_share};
use crate::strategy::{
    BucketEvaluation, Pooling, Proposal, Strategy, StrategyContext, StrategyOutput, ViewEvaluation,
    default_max_market_spread, holds_or_pending, max_data_age, size_for,
};
use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::{MarketOutcome, OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{Price, Usd};

/// Configuration of strategy D.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertainConfig {
    pub enabled: bool,
    /// Highest price paid for a decided outcome.
    pub max_price: Price,
    /// Minimum profit per share after fee and slippage.
    pub min_edge: f64,
    pub slippage_allowance: Price,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    /// A high more than this above the report before it (tenths °C) is only
    /// trusted once a later report repeats it.
    pub max_jump_tenths: i32,
    pub notional: Usd,
}

impl Default for CertainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_price: Price::saturating_from_micros(990_000),
            min_edge: 0.02,
            slippage_allowance: Price::saturating_from_micros(2_000),
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            max_jump_tenths: 30,
            notional: Usd::from_whole(10),
        }
    }
}

/// The high (whole degrees) observed in *every* view, if all views have one.
pub fn decided_high(views: &[ViewEvaluation]) -> Option<i32> {
    views.iter().map(|v| v.assessment.features.high_whole).min()
}

/// Why the lowest-high view's high is not yet trusted, if it is not.
fn unconfirmed_jump(views: &[ViewEvaluation], max_jump_tenths: i32) -> Option<String> {
    views.iter().find_map(|v| {
        let f = &v.assessment.features;
        match f.high_jump_tenths {
            Some(j) if j > max_jump_tenths && f.retests == 0 => Some(format!(
                "high jumped {:.1} °C in one report — waiting for a second report",
                f64::from(j) / 10.0
            )),
            _ => None,
        }
    })
}

/// What the observations decide for one bucket, given the decided high.
fn decided_side(outcome: &MarketOutcome, high: i32) -> Option<OutcomeSide> {
    let b = &outcome.bucket;
    match (b.lower, b.upper) {
        (_, Some(upper)) if upper < high => Some(OutcomeSide::No),
        (Some(lower), None) if lower <= high => Some(OutcomeSide::Yes),
        _ => None,
    }
}

/// Strategy D.
pub struct CertainOutcomes {
    id: StrategyId,
    pub config: CertainConfig,
}

impl CertainOutcomes {
    pub fn new(config: CertainConfig) -> Self {
        Self {
            id: StrategyId::from_static("D_certain_outcome"),
            config,
        }
    }

    fn evaluate_outcome(
        &self,
        ctx: &StrategyContext<'_>,
        outcome: &MarketOutcome,
        side: OutcomeSide,
        high: i32,
        jump: Option<&str>,
        proposals: &mut Vec<Proposal>,
    ) -> BucketEvaluation {
        let fees = ctx.market.fees;
        let (token, complement) = match side {
            OutcomeSide::Yes => (&outcome.yes_token, &outcome.no_token),
            OutcomeSide::No => (&outcome.no_token, &outcome.yes_token),
        };
        let book = ctx.books.get(token);
        // Shown next to the certainty; not used to decide (a stale quote on a
        // decided outcome is the opportunity, not a warning).
        let market_p = Pooling {
            weight: 0.0,
            max_spread: default_max_market_spread(),
            max_book_age_ms: self.config.max_book_age_ms,
        }
        .market_probability(book, ctx.books.get(complement), ctx.now);
        let price = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let ev = price.map(|pr| ev_per_share(1.0, pr, &fees, self.config.slippage_allowance));
        let mut blockers = Vec::new();
        if !self.config.enabled {
            blockers.push("strategy disabled".into());
        }
        if let Some(j) = jump {
            blockers.push(j.to_owned());
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
                if pr > self.config.max_price {
                    blockers.push(format!("ask {pr} > {}", self.config.max_price));
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
        let shares =
            price.and_then(|pr| size_for(self.config.notional, pr, outcome.min_order_size));
        if price.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }
        let signal = blockers.is_empty();
        if signal && let (Some(pr), Some(sh), Some(e)) = (price, shares, ev) {
            let why = match side {
                OutcomeSide::No => format!(
                    "decided: the observed high {high}{} is above this bucket in every view",
                    ctx.market.unit.symbol()
                ),
                OutcomeSide::Yes => format!(
                    "decided: the observed high {high}{} is inside this open-ended bucket in every view",
                    ctx.market.unit.symbol()
                ),
            };
            proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: outcome.label.clone(),
                bucket: outcome.bucket,
                condition_id: outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: side,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: pr,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: 1.0,
                ev_per_share: e,
                break_even: break_even_probability(pr, &fees, self.config.slippage_allowance),
                research_only: false,
                rationale: vec![
                    why,
                    format!("ask {pr}, {e:.4} per share after fee and slippage"),
                ],
            });
        }
        BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: side,
            token: token.clone(),
            ask: price,
            bid,
            p_win: Some(1.0),
            ev_per_share: ev,
            break_even: price
                .map(|pr| break_even_probability(pr, &fees, self.config.slippage_allowance)),
            signal,
            blockers,
            model_p: Some(1.0),
            market_p,
            maker_bid: None,
        }
    }
}

impl Strategy for CertainOutcomes {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        let Some(high) = decided_high(ctx.views) else {
            return out;
        };
        let jump = unconfirmed_jump(ctx.views, self.config.max_jump_tenths);
        for outcome in &ctx.market.outcomes {
            let Some(side) = decided_side(outcome, high) else {
                continue;
            };
            out.evaluations.push(self.evaluate_outcome(
                ctx,
                outcome,
                side,
                high,
                jump.as_deref(),
                &mut out.proposals,
            ));
        }
        out
    }
}
