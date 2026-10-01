//! Strategy J — the morning maker: resting quotes on both sides before the
//! day's high is in play.
//!
//! **Evidence.** On EHAM's 122 settled market days (June–September 2026)
//! the resting orders on the other side of the morning's trades earned
//! money: +0.71¢ a share net in the evening before the day, +0.61¢ from
//! 00:00 to 09:00 local and +0.47¢ from 09:00 to 12:00 (each 95 % interval
//! spans zero; together about $14,300 for the market's makers), while the
//! afternoon, when the high is set, cost them 0.59¢ (12:00–15:00). Before a
//! report quotes are picked off (−0.49¢ in the last five minutes, −1.34¢ on
//! the high's bucket), so J's orders expire ten minutes before each one.
//! Kalshi's transaction data show the same split: makers earn more than
//! takers, at every price.
//!
//! **Rule.** Between `start_local_minute` and `end_local_minute`, for every
//! bucket whose fresh YES book has its midpoint in [`min_mid`, `max_mid`]
//! and a spread of `min_spread`–`max_spread`, J rests a YES bid and a NO
//! bid (an offer of YES), each one tick inside the best price on its own
//! book, or at it when the spread is one tick. Fixed cost per order; good
//! till `cancel_before_report_minutes` before the next routine report,
//! posted again after it. A filled side is held to settlement; when both
//! sides of a bucket fill, the pair pays one dollar whatever the weather,
//! so J keeps the spread.
//!
//! **Risk.** Inventory: a side that fills alone is a directional position at
//! a price the market set a moment ago. J needs no model, but it leaves
//! the high's bucket in the afternoon to the strategies that do.
//!
//! HYPOTHESIS TO BACKTEST: `research market` replays J at traded prices
//! (filled only when a later trade goes through the price).
//!
//! [`min_mid`]: MorningMakerConfig::min_mid
//! [`max_mid`]: MorningMakerConfig::max_mid

use crate::quoting::{maker_ev, passive_bid, quote_expiry};
use crate::strategy::{
    BucketEvaluation, Proposal, Strategy, StrategyContext, StrategyOutput, holds_or_pending,
    max_data_age,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wm_core::ids::StrategyId;
use wm_core::market::{MarketOutcome, OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{
    Price, Rounding, Shares, Usd, decimal_serde, round_shares_to_lot, shares_for_notional,
};

/// Engine id of strategy J.
pub const ID: &str = "J_morning_maker";

/// Configuration of strategy J. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MorningMakerConfig {
    pub enabled: bool,
    /// Local window (minutes after local midnight, end exclusive).
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    /// Quote buckets whose YES midpoint lies in [min_mid, max_mid] …
    pub min_mid: f64,
    pub max_mid: f64,
    /// … and whose YES spread is at least this (room to earn) …
    #[serde(with = "decimal_serde::price")]
    pub min_spread: Price,
    /// … and at most this.
    #[serde(with = "decimal_serde::price")]
    pub max_spread: Price,
    /// Cost of one order at its price.
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    /// Orders expire this long before the next routine report …
    pub cancel_before_report_minutes: i64,
    /// … and are not posted with less than this left before then.
    pub min_rest_minutes: i64,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
}

impl Default for MorningMakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start_local_minute: 0,
            end_local_minute: 11 * 60,
            min_mid: 0.10,
            max_mid: 0.90,
            min_spread: Price::saturating_from_micros(20_000),
            max_spread: Price::saturating_from_micros(50_000),
            notional: Usd::from_whole(10),
            cancel_before_report_minutes: 10,
            min_rest_minutes: 3,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
        }
    }
}

impl MorningMakerConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Strategy J.
pub struct MorningMaker {
    id: StrategyId,
    pub config: MorningMakerConfig,
}

impl MorningMaker {
    pub fn new(config: MorningMakerConfig) -> Self {
        Self {
            id: StrategyId::from_static(ID),
            config,
        }
    }
}

impl Strategy for MorningMaker {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let cfg = &self.config;
        let mut out = StrategyOutput::default();
        let Some(f) = ctx.views.first().map(|v| &v.assessment.features) else {
            return out;
        };
        let minute = f.local_minute_now;
        // Outside the window J shows nothing: it has no bucket to watch.
        if !(cfg.start_local_minute..cfg.end_local_minute).contains(&minute) {
            return out;
        }
        let mut shared = Vec::new();
        if !cfg.enabled {
            shared.push("strategy disabled".to_owned());
        }
        if max_data_age(ctx.views) > cfg.max_data_age_minutes {
            shared.push("weather data too old".into());
        }
        let expiry = quote_expiry(
            ctx.now,
            ctx.routine_minutes,
            Duration::minutes(cfg.cancel_before_report_minutes),
            Duration::minutes(cfg.min_rest_minutes),
        );
        if expiry.is_none() {
            shared.push(format!(
                "within {}′ (+{}′ to rest) of the next report",
                cfg.cancel_before_report_minutes, cfg.min_rest_minutes
            ));
        }
        let mut seen = HashSet::new();
        for o in &ctx.market.outcomes {
            if !seen.insert(o.condition_id.clone()) {
                continue;
            }
            let Some(yes_book) = ctx
                .books
                .get(&o.yes_token)
                .filter(|b| b.age_ms(ctx.now) <= cfg.max_book_age_ms)
            else {
                continue;
            };
            let (Some(b), Some(a)) = (yes_book.best_bid(), yes_book.best_ask()) else {
                continue;
            };
            if a.price <= b.price {
                continue;
            }
            let m = (a.price.as_f64() + b.price.as_f64()) / 2.0;
            if !(cfg.min_mid..=cfg.max_mid).contains(&m) {
                continue;
            }
            let spread = a.price.saturating_sub(b.price);
            let mut blockers = shared.clone();
            if spread < cfg.min_spread || spread > cfg.max_spread {
                blockers.push(format!(
                    "spread {spread} outside [{}, {}]",
                    cfg.min_spread, cfg.max_spread
                ));
            }
            for side in [OutcomeSide::Yes, OutcomeSide::No] {
                let p_win = match side {
                    OutcomeSide::Yes => m,
                    OutcomeSide::No => 1.0 - m,
                };
                let (eval, proposal) = self.quote(ctx, o, side, p_win, expiry, &blockers);
                out.evaluations.push(eval);
                out.proposals.extend(proposal);
            }
        }
        out
    }
}

impl MorningMaker {
    fn quote(
        &self,
        ctx: &StrategyContext<'_>,
        o: &MarketOutcome,
        side: OutcomeSide,
        p_mid: f64,
        expiry: Option<DateTime<Utc>>,
        shared: &[String],
    ) -> (BucketEvaluation, Option<Proposal>) {
        let cfg = &self.config;
        let token = o.token(side);
        let book = ctx.books.get(token);
        let mut blockers = shared.to_vec();
        let price = match book {
            None => {
                blockers.push(format!("no {} book", side.as_str()));
                None
            }
            Some(b) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                let p = passive_bid(b);
                if p.is_none() {
                    blockers.push(format!("{} book one-sided or crossed", side.as_str()));
                }
                p
            }
        };
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned or quoting".into());
        }
        if !o.accepting_orders || o.closed {
            blockers.push("market not accepting orders".into());
        }
        let shares = price.and_then(|p| {
            let raw = shares_for_notional(cfg.notional, p, Rounding::Down);
            let s = round_shares_to_lot(raw, Shares::from_whole(1), Rounding::Down);
            (s >= o.min_order_size && s.micros() > 0).then_some(s)
        });
        if price.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }
        // The market's own midpoint is the fair value: J earns the distance
        // to it (plus the rebate), not a forecast.
        let ev = price.map(|p| maker_ev(p_mid, p, &ctx.market.fees));
        let signal = blockers.is_empty();
        let eval = BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: o.label.clone(),
            outcome_side: side,
            token: token.clone(),
            ask: book.and_then(OrderBook::best_ask).map(|l| l.price),
            bid: book.and_then(OrderBook::best_bid).map(|l| l.price),
            p_win: Some(p_mid),
            ev_per_share: ev,
            break_even: price.map(|p| p.as_f64()),
            signal,
            blockers,
            model_p: None,
            market_p: Some(p_mid),
        };
        let proposal = match (signal, price, shares, expiry) {
            (true, Some(p), Some(sh), Some(exp)) => Some(Proposal {
                strategy: self.id.clone(),
                bucket_label: o.label.clone(),
                bucket: o.bucket,
                condition_id: o.condition_id.clone(),
                token: token.clone(),
                outcome_side: side,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: p,
                shares: sh,
                tif: TimeInForce::Gtd { expires_at: exp },
                p_win: p_mid,
                ev_per_share: ev.unwrap_or(0.0),
                break_even: p.as_f64(),
                research_only: false,
                rationale: vec![
                    format!(
                        "morning quote: {} bid at {p} on {} (midpoint {p_mid:.3})",
                        side.as_str(),
                        o.label
                    ),
                    format!(
                        "expires {} UTC, {}′ before the next report",
                        exp.format("%H:%M"),
                        cfg.cancel_before_report_minutes
                    ),
                    "maker: no fee, the rebate; morning makers earned in the 122-day replay".into(),
                ],
            }),
            _ => None,
        };
        (eval, proposal)
    }
}
