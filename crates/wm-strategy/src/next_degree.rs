//! Strategy H — the next degree: YES on the bucket one degree above the
//! day's high while the day can still warm.
//!
//! **Evidence.** On EHAM's 122 settled market days (June–September 2026)
//! the bucket one above the high was where takers made money: they gained
//! +0.78¢ a share before fees there, and the resting orders on the other
//! side lost 0.68¢ (95 % CI −1.27 … −0.10). Buying the NO of the buckets
//! above the high lost at every setting tried (strategy B: −$92.74 over 129
//! trades at NO asks 0.70–0.99), and cheap YES won more often than priced
//! (YES at 0.10–0.30 won 21.0 % at a mean price of 18.7 %; when the model
//! was ≥ 5 points above the market, 13.8 % at 11.9 %). The market seems to
//! underrate one further degree of warming before the peak.
//!
//! **Rule.** Between `start_local_minute` and the season's
//! `until_quantile` peak time (when that share of the history's days had
//! first reported their high; `fallback_end_local_minute` until the model
//! carries peak times), while the latest report is within
//! `max_drop_tenths` of the high, H buys the YES of the bucket holding
//! high + 1 — not the high's own bucket — when its ask lies in
//! [`min_price`, `max_price`], the whole size is offered at or below
//! `max_price`, and the model rates the bucket at least `min_model_ratio` ×
//! the ask. Fixed cost per trade, fill-and-kill; held to settlement.
//!
//! **Risk.** It loses the stake whenever the day does not warm one more
//! degree, i.e. most of the time: a few winners at 3–20× must pay for many
//! small losses, so its P&L is lumpy.
//!
//! HYPOTHESIS TO BACKTEST: `research market` replays H at traded prices.
//!
//! [`min_price`]: NextDegreeConfig::min_price
//! [`max_price`]: NextDegreeConfig::max_price

use crate::ev::{break_even_probability, ev_per_share};
use crate::peak_slot::sweep_price;
use crate::peak_times::hm;
use crate::strategy::{
    BucketEvaluation, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    holds_or_pending, max_data_age, min_p_in_bucket,
};
use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::{OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{
    Price, Rounding, Shares, Usd, decimal_serde, round_shares_to_lot, shares_for_notional,
};

/// Engine id of strategy H.
pub const ID: &str = "H_next_degree";

/// Configuration of strategy H. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NextDegreeConfig {
    pub enabled: bool,
    /// The YES ask of the bucket above the high's must be at least this …
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    /// … and the whole size offered at or below this.
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    /// From this local time (minutes after local midnight) …
    pub start_local_minute: u16,
    /// … until the season's peak time at this quantile of the history …
    pub until_quantile: f64,
    /// … or this local time until the model carries peak times.
    pub fallback_end_local_minute: u16,
    /// The latest report at most this far below the high (tenths °C).
    pub max_drop_tenths: i32,
    /// The model's probability for the bucket at least this multiple of
    /// the ask (0 = no model condition beyond a loaded model).
    pub min_model_ratio: f64,
    /// Cost of one trade at the ask.
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    /// Used for the EV shown (not a gate).
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
}

impl Default for NextDegreeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_price: Price::saturating_from_micros(50_000),
            max_price: Price::saturating_from_micros(350_000),
            start_local_minute: 10 * 60,
            until_quantile: 0.75,
            fallback_end_local_minute: 15 * 60 + 30,
            max_drop_tenths: 10,
            min_model_ratio: 1.0,
            notional: Usd::from_whole(10),
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
        }
    }
}

impl NextDegreeConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The local minute H stops at in `season`, and whether it came from
    /// the history.
    pub fn end_minute(
        &self,
        season: wm_core::time::Season,
        peak_times: Option<&crate::PeakTimes>,
    ) -> (u16, bool) {
        match peak_times
            .and_then(|pt| pt.season(season))
            .and_then(|s| s.quantile(self.until_quantile))
        {
            Some(m) => (m, true),
            None => (self.fallback_end_local_minute, false),
        }
    }
}

/// Strategy H.
pub struct NextDegree {
    id: StrategyId,
    pub config: NextDegreeConfig,
}

impl NextDegree {
    pub fn new(config: NextDegreeConfig) -> Self {
        Self {
            id: StrategyId::from_static(ID),
            config,
        }
    }
}

impl Strategy for NextDegree {
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
        let Some(outcome) = ctx.market.outcome_for_value(high + 1) else {
            return out;
        };
        if outcome.bucket.contains(high) {
            return out; // the high's own bucket also holds the next degree
        }
        let token = &outcome.yes_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let mut blockers: Vec<String> = Vec::new();
        if !cfg.enabled {
            blockers.push("strategy disabled".into());
        }
        let minute = f.local_minute_now;
        let (end, learned) = cfg.end_minute(f.season, ctx.peak_times);
        if minute < cfg.start_local_minute || minute >= end {
            blockers.push(format!(
                "{} outside {}–{} ({})",
                hm(minute),
                hm(cfg.start_local_minute),
                hm(end),
                if learned {
                    format!(
                        "{:.0}% of {} days' highs reported by then",
                        100.0 * cfg.until_quantile,
                        f.season.as_str()
                    )
                } else {
                    "fallback end: peak times not learned yet".into()
                }
            ));
        }
        let drop = ctx
            .views
            .iter()
            .map(|v| v.assessment.features.drop_tenths)
            .max()
            .unwrap_or(0);
        if drop > cfg.max_drop_tenths {
            blockers.push(format!(
                "{:.1} °C below the high > {:.1}: cooling",
                f64::from(drop) / 10.0,
                f64::from(cfg.max_drop_tenths) / 10.0
            ));
        }
        if max_data_age(ctx.views) > cfg.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }
        let size_at = |pr: Price| {
            let raw = shares_for_notional(cfg.notional, pr, Rounding::Down);
            let s = round_shares_to_lot(raw, Shares::from_whole(1), Rounding::Down);
            (s >= outcome.min_order_size && s.micros() > 0).then_some(s)
        };
        let mut shares = ask.and_then(size_at);
        let mut limit = None;
        match (book, ask) {
            (None, _) => blockers.push("no order book".into()),
            (Some(_), None) => blockers.push("no ask".into()),
            (Some(b), Some(a)) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if a < cfg.min_price || a > cfg.max_price {
                    blockers.push(format!(
                        "ask {a} outside [{}, {}]",
                        cfg.min_price, cfg.max_price
                    ));
                } else if let Some(sh) = shares {
                    limit = sweep_price(b, sh, cfg.max_price);
                    match limit {
                        // Never more than the notional: fewer shares when the
                        // sweep reaches a worse level (they fill at or below it).
                        Some(l) if l > a => shares = size_at(l),
                        Some(_) => {}
                        None => {
                            let (depth, _) = b.ask_depth_up_to(cfg.max_price);
                            blockers.push(format!(
                                "only {depth} shares offered ≤ {} (need {sh})",
                                cfg.max_price
                            ));
                        }
                    }
                } else {
                    blockers.push("size below market minimum".into());
                }
            }
        }
        let model = min_p_in_bucket(ctx.views, high, &outcome.bucket).map(|(p, _)| p);
        match (model, ask) {
            (None, _) => blockers.push("no probability model".into()),
            (Some(pm), Some(a)) if pm < cfg.min_model_ratio * a.as_f64() => {
                blockers.push(format!(
                    "model {pm:.3} < {:.2} × ask {a}",
                    cfg.min_model_ratio
                ));
            }
            _ => {}
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let fees = ctx.market.fees;
        let priced = limit.or(ask);
        let ev = model
            .zip(priced)
            .map(|(p, pr)| ev_per_share(p, pr, &fees, cfg.slippage_allowance));
        let be = priced.map(|pr| break_even_probability(pr, &fees, cfg.slippage_allowance));
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: OutcomeSide::Yes,
            token: token.clone(),
            ask,
            bid,
            p_win: model,
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
            model_p: model,
            market_p: None,
            maker_bid: None,
        });
        if signal && let (Some(lim), Some(sh), Some(b), Some(a)) = (limit, shares, be, ask) {
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
                limit_price: lim,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: model.unwrap_or(b),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!(
                        "{} local before {}; {:.1} °C below the high {high}",
                        hm(minute),
                        hm(end),
                        f64::from(drop) / 10.0
                    ),
                    format!(
                        "next degree {} offered at {a}: {sh} shares at ≤ {lim}; model {:.3}",
                        outcome.label,
                        model.unwrap_or(0.0)
                    ),
                    "the market underrates one more degree of warming (122-day replay: takers on the bucket above the high gained)".into(),
                ],
            });
        }
        out
    }
}
