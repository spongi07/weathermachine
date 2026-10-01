//! Strategy G — sell the far tails: resting NO bids on buckets well above
//! the day's high.
//!
//! **Evidence.** Cheap outcomes are overpriced on prediction markets: the
//! favourite–longshot bias (on Kalshi contracts under 10¢ lose over 60 % of
//! the stake, on Polymarket buys under 10¢ about 19¢ per dollar), and the
//! side that rests orders does better than the side that takes them. EHAM's
//! 122 settled market days (June–September 2026) agree: resting orders
//! filled by takers who paid 0.00–0.02 earned +0.34¢ a share net (95 % CI
//! +0.26 … +0.41), those on buckets two above the high +1.04¢ (−0.06 …
//! +2.27) and three or more above +0.41¢ (−0.25 … +1.04); YES priced
//! 0.00–0.02 won 0.1 % of the time at a mean price of 0.4 %.
//!
//! **Rule.** For every bucket whose lowest value is at least
//! `min_distance` whole degrees above the day's high (it can only win if
//! the temperature still climbs that far), G rests a NO bid one tick above
//! the best NO bid, or at it when the spread is one tick: an offer to sell
//! that bucket's YES at one minus the bid. Only when that YES price lies in
//! [`min_yes_price`, `max_yes_price`] and the model rates the bucket at
//! most `max_model_ratio` × that price. A fixed cost per order; the order
//! expires `cancel_before_report_minutes` before the next routine report
//! and is posted again after it. One position per bucket and day: a filled
//! order is held to settlement (the unwind engine leaves it alone).
//!
//! **Risk.** A bucket that does win costs the whole NO price, 10–100 times
//! the premium. The edge is a fraction of a cent per dollar a day, so G is
//! a small, steady earner whose result rests on the rare loss.
//!
//! HYPOTHESIS TO BACKTEST: `research market` replays G at traded prices
//! (filled only when a later trade goes through the price).
//!
//! [`min_yes_price`]: TailSellerConfig::min_yes_price
//! [`max_yes_price`]: TailSellerConfig::max_yes_price

use crate::peak_times::hm;
use crate::quoting::{maker_ev, passive_bid, quote_expiry};
use crate::strategy::{
    BucketEvaluation, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    holds_or_pending, max_data_age, max_p_in_bucket,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wm_core::ids::StrategyId;
use wm_core::market::{MarketOutcome, OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{
    Price, Rounding, Usd, decimal_serde, round_shares_to_lot, shares_for_notional,
};

/// Engine id of strategy G.
pub const ID: &str = "G_tail_seller";

/// Configuration of strategy G. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TailSellerConfig {
    pub enabled: bool,
    /// Buckets whose lowest value is at least this many whole degrees above
    /// the day's high.
    pub min_distance: i32,
    /// The YES offered (one minus the NO bid) must be at least this …
    #[serde(with = "decimal_serde::price")]
    pub min_yes_price: Price,
    /// … and at most this.
    #[serde(with = "decimal_serde::price")]
    pub max_yes_price: Price,
    /// Model veto: the model's upper-bound probability that the bucket wins
    /// at most this multiple of the YES price offered.
    pub max_model_ratio: f64,
    /// Local window (minutes after local midnight, end exclusive).
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    /// Cost of one order at its NO price.
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    /// Orders expire this long before the next routine report …
    pub cancel_before_report_minutes: i64,
    /// … and are not posted with less than this left before then.
    pub min_rest_minutes: i64,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
}

impl Default for TailSellerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_distance: 2,
            min_yes_price: Price::saturating_from_micros(10_000),
            max_yes_price: Price::saturating_from_micros(80_000),
            max_model_ratio: 1.0,
            start_local_minute: 10 * 60,
            end_local_minute: 21 * 60,
            notional: Usd::from_whole(30),
            cancel_before_report_minutes: 10,
            min_rest_minutes: 3,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
        }
    }
}

impl TailSellerConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Strategy G.
pub struct TailSeller {
    id: StrategyId,
    pub config: TailSellerConfig,
}

impl TailSeller {
    pub fn new(config: TailSellerConfig) -> Self {
        Self {
            id: StrategyId::from_static(ID),
            config,
        }
    }
}

impl Strategy for TailSeller {
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
            let far = o
                .bucket
                .lower
                .is_some_and(|lo| lo >= high + cfg.min_distance);
            if !far || !seen.insert(o.condition_id.clone()) {
                continue;
            }
            let (eval, proposal) = self.bucket(ctx, o, high, expiry, &shared);
            out.evaluations.push(eval);
            out.proposals.extend(proposal);
        }
        out
    }
}

impl TailSeller {
    fn bucket(
        &self,
        ctx: &StrategyContext<'_>,
        o: &MarketOutcome,
        high: i32,
        expiry: Option<DateTime<Utc>>,
        shared: &[String],
    ) -> (BucketEvaluation, Option<Proposal>) {
        let cfg = &self.config;
        let token = &o.no_token;
        let book = ctx.books.get(token);
        let mut blockers: Vec<String> = shared.to_vec();
        let mut bid = None;
        match book {
            None => blockers.push("no NO book".into()),
            Some(b) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                bid = passive_bid(b);
                if bid.is_none() {
                    blockers.push("NO book one-sided or crossed".into());
                }
            }
        }
        // The YES offered: one minus the NO bid.
        let yes_offered = bid.map(Price::complement);
        if let Some(y) = yes_offered
            && (y < cfg.min_yes_price || y > cfg.max_yes_price)
        {
            blockers.push(format!(
                "YES offered at {y} outside [{}, {}]",
                cfg.min_yes_price, cfg.max_yes_price
            ));
        }
        let model = max_p_in_bucket(ctx.views, high, &o.bucket).map(|(p, _)| p);
        match (model, yes_offered) {
            (None, _) => blockers.push("no probability model".into()),
            (Some(pm), Some(y)) if pm > cfg.max_model_ratio * y.as_f64() => blockers.push(format!(
                "model {pm:.3} > {:.2} × {y}: the model rates the tail higher",
                cfg.max_model_ratio
            )),
            _ => {}
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned or quoting".into());
        }
        if !o.accepting_orders || o.closed {
            blockers.push("market not accepting orders".into());
        }
        let shares = bid.and_then(|p| {
            let raw = shares_for_notional(cfg.notional, p, Rounding::Down);
            let s = round_shares_to_lot(raw, wm_core::units::Shares::from_whole(1), Rounding::Down);
            (s >= o.min_order_size && s.micros() > 0).then_some(s)
        });
        if bid.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }
        // NO wins unless the bucket does.
        let p_win = model.map(|pm| 1.0 - pm);
        let ev = p_win
            .zip(bid)
            .map(|(p, b)| maker_ev(p, b, &ctx.market.fees));
        let signal = blockers.is_empty();
        let eval = BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: o.label.clone(),
            outcome_side: OutcomeSide::No,
            token: token.clone(),
            ask: book.and_then(OrderBook::best_ask).map(|l| l.price),
            bid: book.and_then(OrderBook::best_bid).map(|l| l.price),
            p_win,
            ev_per_share: ev,
            break_even: bid.map(|b| b.as_f64()),
            signal,
            blockers,
            model_p: p_win,
            market_p: None,
        };
        let proposal = match (signal, bid, shares, expiry, yes_offered) {
            (true, Some(b), Some(sh), Some(exp), Some(y)) => Some(Proposal {
                strategy: self.id.clone(),
                bucket_label: o.label.clone(),
                bucket: o.bucket,
                condition_id: o.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::No,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: b,
                shares: sh,
                tif: TimeInForce::Gtd { expires_at: exp },
                p_win: p_win.unwrap_or(b.as_f64()),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b.as_f64(),
                research_only: false,
                rationale: vec![
                    format!(
                        "{} is ≥ {} °C above the high {high}: rest a NO bid at {b} (YES offered at {y})",
                        o.label, self.config.min_distance
                    ),
                    format!(
                        "model {:.4} ≤ {:.2} × {y}; expires {} UTC, before the next report",
                        model.unwrap_or(0.0),
                        self.config.max_model_ratio,
                        exp.format("%H:%M")
                    ),
                    "maker: no fee, the rebate; longshots are overpriced (favourite–longshot bias)"
                        .into(),
                ],
            }),
            _ => None,
        };
        (eval, proposal)
    }
}
