//! Strategy I — fade the middle: NO on buckets priced 0.30–0.70 that the
//! model also rates lower.
//!
//! **Evidence.** On EHAM's 122 settled market days (June–September 2026)
//! buckets whose YES traded at 0.30–0.70 won 45.4 % of the time at a mean
//! price of 48.5 % (95 % CI of the win rate 42.8 … 48.0 %, 1,392 decision
//! points): the middle is overpriced by about three points. When the model
//! was five or more points below the market the bucket won 51.1 % at a mean
//! price of 53.6 % (49.0 … 53.3 %). The middle of the ladder is where the
//! next report moves prices most; the market leans to "it stays here".
//!
//! **Rule.** Between `start_local_minute` and `end_local_minute`, for every
//! bucket still alive whose fresh YES book (spread ≤ `max_spread`) has its
//! midpoint in [`min_mid`, `max_mid`], and whose upper-bound model
//! probability is at least `min_model_gap` below that midpoint, I buys the
//! NO when its EV is at least `min_edge`. The EV takes the measured
//! overpricing (`calibration_bias`) off the midpoint: p(NO wins) =
//! 1 − (mid − bias), never above the model's own NO probability. Fixed cost
//! per trade, fill-and-kill, one position per bucket and day, held to
//! settlement.
//!
//! **Risk.** A three-point edge is about what the taker fee (≈ 1.25¢ at 0.50),
//! half the spread and slippage cost: I trades only on tight books, and the
//! result per trade is a coin flip with a slight tilt.
//!
//! HYPOTHESIS TO BACKTEST: `research market` replays I at traded prices.
//!
//! [`min_mid`]: MiddleFadeConfig::min_mid
//! [`max_mid`]: MiddleFadeConfig::max_mid

use crate::ev::{break_even_probability, ev_per_share};
use crate::peak_times::hm;
use crate::strategy::{
    BucketEvaluation, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    holds_or_pending, max_data_age, max_p_in_bucket,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wm_core::ids::StrategyId;
use wm_core::market::{MarketOutcome, OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{
    Price, Rounding, Shares, Usd, decimal_serde, round_shares_to_lot, shares_for_notional,
};

/// Engine id of strategy I.
pub const ID: &str = "I_middle_fade";

/// Configuration of strategy I. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MiddleFadeConfig {
    pub enabled: bool,
    /// The YES midpoint must lie in [min_mid, max_mid].
    pub min_mid: f64,
    pub max_mid: f64,
    /// Widest YES spread at which the midpoint counts.
    #[serde(with = "decimal_serde::price")]
    pub max_spread: Price,
    /// The model's upper-bound probability at least this far below the
    /// midpoint.
    pub min_model_gap: f64,
    /// The measured overpricing of these buckets, taken off the midpoint.
    pub calibration_bias: f64,
    /// EV per NO share at its ask, after the taker fee and slippage.
    pub min_edge: f64,
    /// Local window (minutes after local midnight, end exclusive).
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    /// Cost of one trade at the NO ask.
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
}

impl Default for MiddleFadeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_mid: 0.30,
            max_mid: 0.70,
            max_spread: Price::saturating_from_micros(40_000),
            min_model_gap: 0.05,
            calibration_bias: 0.03,
            min_edge: 0.0,
            start_local_minute: 9 * 60,
            end_local_minute: 18 * 60,
            notional: Usd::from_whole(10),
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
        }
    }
}

impl MiddleFadeConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Strategy I.
pub struct MiddleFade {
    id: StrategyId,
    pub config: MiddleFadeConfig,
}

impl MiddleFade {
    pub fn new(config: MiddleFadeConfig) -> Self {
        Self {
            id: StrategyId::from_static(ID),
            config,
        }
    }
}

/// Midpoint of a fresh two-sided book with a spread of at most `max_spread`.
fn mid(book: &OrderBook, max_spread: Price) -> Result<f64, String> {
    let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) else {
        return Err("YES book one-sided".into());
    };
    if ask.price <= bid.price {
        return Err("YES book crossed".into());
    }
    let spread = ask.price.saturating_sub(bid.price);
    if spread > max_spread {
        return Err(format!("YES spread {spread} > {max_spread}"));
    }
    Ok((bid.price.as_f64() + ask.price.as_f64()) / 2.0)
}

impl Strategy for MiddleFade {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let cfg = &self.config;
        let mut out = StrategyOutput::default();
        let Ok(high) = common_high(ctx.views) else {
            return out;
        };
        let Some(f) = ctx.views.first().map(|v| &v.assessment.features) else {
            return out;
        };
        let mut shared = Vec::new();
        if !cfg.enabled {
            shared.push("strategy disabled".to_owned());
        }
        let minute = f.local_minute_now;
        if !(cfg.start_local_minute..cfg.end_local_minute).contains(&minute) {
            shared.push(format!(
                "{} outside {}–{}",
                hm(minute),
                hm(cfg.start_local_minute),
                hm(cfg.end_local_minute)
            ));
        }
        if max_data_age(ctx.views) > cfg.max_data_age_minutes {
            shared.push("weather data too old".into());
        }
        let mut seen = HashSet::new();
        for o in &ctx.market.outcomes {
            let dead = o.bucket.upper.is_some_and(|u| u < high);
            if dead || !seen.insert(o.condition_id.clone()) {
                continue;
            }
            // Only buckets the market prices in the middle are shown.
            let Some(Ok(m)) = ctx
                .books
                .get(&o.yes_token)
                .filter(|b| b.age_ms(ctx.now) <= cfg.max_book_age_ms)
                .map(|b| mid(b, cfg.max_spread))
            else {
                continue;
            };
            if !(cfg.min_mid..=cfg.max_mid).contains(&m) {
                continue;
            }
            let (eval, proposal) = self.bucket(ctx, o, high, m, &shared);
            out.evaluations.push(eval);
            out.proposals.extend(proposal);
        }
        out
    }
}

impl MiddleFade {
    fn bucket(
        &self,
        ctx: &StrategyContext<'_>,
        o: &MarketOutcome,
        high: i32,
        m: f64,
        shared: &[String],
    ) -> (BucketEvaluation, Option<Proposal>) {
        let cfg = &self.config;
        let token = &o.no_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let mut blockers: Vec<String> = shared.to_vec();
        let model = max_p_in_bucket(ctx.views, high, &o.bucket).map(|(p, _)| p);
        match model {
            None => blockers.push("no probability model".into()),
            Some(pm) if pm > m - cfg.min_model_gap => blockers.push(format!(
                "model {pm:.3} not ≥ {:.2} below the midpoint {m:.3}",
                cfg.min_model_gap
            )),
            _ => {}
        }
        // NO wins unless the bucket does: the midpoint minus the measured
        // overpricing, never more than the model allows.
        let p_win = model.map(|pm| (1.0 - (m - cfg.calibration_bias)).min(1.0 - pm));
        let fees = ctx.market.fees;
        let shares = ask.and_then(|a| {
            let raw = shares_for_notional(cfg.notional, a, Rounding::Down);
            let s = round_shares_to_lot(raw, Shares::from_whole(1), Rounding::Down);
            (s >= o.min_order_size && s.micros() > 0).then_some(s)
        });
        match (book, ask) {
            (None, _) => blockers.push("no NO book".into()),
            (Some(_), None) => blockers.push("no NO ask".into()),
            (Some(b), Some(a)) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                match shares {
                    None => blockers.push("size below market minimum".into()),
                    Some(sh) => {
                        let (depth, _) = b.ask_depth_up_to(a);
                        if depth < sh {
                            blockers.push(format!("only {depth} NO offered at {a} (need {sh})"));
                        }
                    }
                }
            }
        }
        let ev = p_win
            .zip(ask)
            .map(|(p, a)| ev_per_share(p, a, &fees, cfg.slippage_allowance));
        if let Some(e) = ev
            && e < cfg.min_edge
        {
            blockers.push(format!("edge {e:.4} < {:.4}", cfg.min_edge));
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !o.accepting_orders || o.closed {
            blockers.push("market not accepting orders".into());
        }
        let be = ask.map(|a| break_even_probability(a, &fees, cfg.slippage_allowance));
        let signal = blockers.is_empty();
        let eval = BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: o.label.clone(),
            outcome_side: OutcomeSide::No,
            token: token.clone(),
            ask,
            bid,
            p_win,
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
            model_p: model.map(|pm| 1.0 - pm),
            market_p: Some(1.0 - m),
        };
        let proposal = match (signal, ask, shares, be) {
            (true, Some(a), Some(sh), Some(b)) => Some(Proposal {
                strategy: self.id.clone(),
                bucket_label: o.label.clone(),
                bucket: o.bucket,
                condition_id: o.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::No,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: a,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: p_win.unwrap_or(b),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!(
                        "{} YES midpoint {m:.3} in [{:.2}, {:.2}]; model {:.3}",
                        o.label,
                        cfg.min_mid,
                        cfg.max_mid,
                        model.unwrap_or(0.0)
                    ),
                    format!(
                        "NO at {a}: p {:.3} (midpoint − {:.2} measured overpricing) vs break-even {b:.3}",
                        p_win.unwrap_or(0.0),
                        cfg.calibration_bias
                    ),
                    "the middle of the ladder is overpriced (122-day replay: 45.4 % won at 48.5 %)"
                        .into(),
                ],
            }),
            _ => None,
        };
        (eval, proposal)
    }
}
