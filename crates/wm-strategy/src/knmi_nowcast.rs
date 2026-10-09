//! Strategy K — the KNMI nowcast: be early to a new high.
//!
//! The market resolves on the METAR, a whole-degree reading every half hour
//! (EHAM: HH:25 and HH:55 UTC) that reaches the public feeds two to five
//! minutes later. KNMI publishes the same airport's automatic weather
//! station every ten minutes, to a tenth of a degree, a few minutes after
//! each interval ends: the reading of HH:10–HH:20 is out before the HH:25
//! METAR is even observed.
//!
//! **Evidence.** On EHAM's 122 settled market days (June–September 2026)
//! 58 reports raised the high while the dead buckets were still priced;
//! in the ten minutes *before* those reports' observation times, takers
//! sold the soon-dead buckets' YES for $3,330 of profit, and on the
//! high's bucket in the last five minutes before any report takers gained
//! +1.39¢ a share before fees while the resting orders against them lost
//! 1.34¢ (95 % CI −2.05 … −0.66). Someone trades on the weather before the
//! METAR shows it.
//!
//! **Rule.** When the latest ten-minute reading is newer than the last
//! METAR, at most `max_age_minutes` old and ended at most
//! `max_lead_minutes` before the report it anticipates (the first routine
//! report after its interval: the 11:10–11:20 reading arrives about when
//! the 11:25 METAR is taken, minutes before that is published), and its
//! mean is at least
//! `mean_margin_tenths` above the high's rounding edge (high + 0.5 °C), so
//! the next METAR will most likely report high + 1 — and the ten-minute
//! maximum reached the edge as well, when `require_max_at_edge` — K buys the
//! NO of the bucket holding the high (it dies the moment any report beats
//! it) when the NO ask lies in [`min_price`, `max_price`] and the EV at
//! `p_new_high` is at least `min_edge`. Fixed cost, fill-and-kill, held to
//! settlement. A bucket that also holds high + 1 is skipped.
//!
//! **Risk.** The METAR is a single minute's value; the temperature can dip
//! back in the five to fifteen minutes between the readings. The NO then
//! still wins if any later report beats the high. `p_new_high` comes from
//! `research market` (with a KNMI API key): its table counts how often the
//! next METAR raised the high after a reading this far above the rounding
//! edge — on EHAM's 123 days to 1 October 2026, 194 of 199 readings
//! 0.3–0.5 °C above it (95 % CI 0.943 … 0.989) and all 137 further above,
//! so the shipped configuration uses 0.94. No KNMI reading (no key, the API
//! down): K does nothing.
//!
//! [`min_price`]: KnmiNowcastConfig::min_price
//! [`max_price`]: KnmiNowcastConfig::max_price

use crate::ev::{break_even_probability, ev_per_share};
use crate::quoting::next_routine_report;
use crate::strategy::{
    BucketEvaluation, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    holds_or_pending, max_data_age,
};
use chrono::Duration;
use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::{OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{
    Price, Rounding, Shares, Usd, decimal_serde, round_shares_to_lot, shares_for_notional,
};

/// Engine id of strategy K.
pub const ID: &str = "K_knmi_nowcast";

/// Configuration of strategy K. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KnmiNowcastConfig {
    pub enabled: bool,
    /// The ten-minute mean at least this far (tenths °C) above the high's
    /// rounding edge, high + 0.5 °C.
    pub mean_margin_tenths: i32,
    /// The ten-minute maximum must have reached the rounding edge too.
    pub require_max_at_edge: bool,
    /// The reading's interval ended at most this long ago …
    pub max_age_minutes: i64,
    /// … and at most this long before the routine report it anticipates
    /// (the first one after the interval).
    pub max_lead_minutes: i64,
    /// The NO of the high's bucket: its ask in [min_price, max_price].
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    /// Probability that the NO wins when the conditions hold: the next
    /// METAR's rate of new highs at this margin in `research market`'s KNMI
    /// table (the default 0.80 is a cautious placeholder; the shipped
    /// configuration uses the measured 0.94).
    pub p_new_high: f64,
    /// EV per share at the ask, after the taker fee and slippage.
    pub min_edge: f64,
    /// Cost of one trade at the ask.
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    /// METAR data age limit.
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
}

impl Default for KnmiNowcastConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mean_margin_tenths: 3,
            require_max_at_edge: true,
            max_age_minutes: 12,
            max_lead_minutes: 16,
            min_price: Price::saturating_from_micros(20_000),
            max_price: Price::saturating_from_micros(750_000),
            p_new_high: 0.80,
            min_edge: 0.05,
            notional: Usd::from_whole(25),
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(10_000),
        }
    }
}

impl KnmiNowcastConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// The ten-minute mean (tenths °C) from which the next METAR most
    /// likely reports `high + 1`.
    pub fn trigger_tenths(&self, high: i32) -> i32 {
        high * 10 + 5 + self.mean_margin_tenths
    }
}

/// Strategy K.
pub struct KnmiNowcast {
    id: StrategyId,
    pub config: KnmiNowcastConfig,
}

impl KnmiNowcast {
    pub fn new(config: KnmiNowcastConfig) -> Self {
        Self {
            id: StrategyId::from_static(ID),
            config,
        }
    }
}

fn c(tenths: i32) -> String {
    format!("{:.1} °C", f64::from(tenths) / 10.0)
}

impl Strategy for KnmiNowcast {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn reads_nowcast(&self) -> bool {
        true
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let cfg = &self.config;
        let mut out = StrategyOutput::default();
        let Ok(high) = common_high(ctx.views) else {
            return out;
        };
        let Some(outcome) = ctx.market.outcome_for_value(high) else {
            return out;
        };
        if outcome.bucket.contains(high + 1) {
            return out; // a new high would not kill this bucket
        }
        let token = &outcome.no_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let mut blockers: Vec<String> = Vec::new();
        if !cfg.enabled {
            blockers.push("strategy disabled".into());
        }
        let age = max_data_age(ctx.views);
        if age > cfg.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }
        let trigger = cfg.trigger_tenths(high);
        let edge = high * 10 + 5;
        let mut reading = String::from("no KNMI reading");
        match ctx.nowcast {
            None => blockers.push("no KNMI ten-minute reading".into()),
            Some(n) => {
                reading = format!(
                    "KNMI {}–{} UTC mean {} max {}",
                    (n.interval_end - Duration::minutes(10)).format("%H:%M"),
                    n.interval_end.format("%H:%M"),
                    n.mean.map_or_else(|| "—".to_owned(), |t| c(t.tenths())),
                    n.max.map_or_else(|| "—".to_owned(), |t| c(t.tenths()))
                );
                // Newer than the last METAR: `age` is whole minutes since it.
                let last_metar = ctx.now - Duration::minutes(age.clamp(0, 24 * 60));
                if n.interval_end <= last_metar {
                    blockers.push("KNMI reading not newer than the last METAR".into());
                }
                let reading_age = ctx.now - n.interval_end;
                if reading_age > Duration::minutes(cfg.max_age_minutes) {
                    blockers.push(format!(
                        "KNMI reading {} min old > {}",
                        reading_age.num_minutes(),
                        cfg.max_age_minutes
                    ));
                }
                // The report the reading anticipates: the first routine one
                // after its interval — still unpublished while the reading is
                // newer than the last METAR, even once its minute has passed
                // (the :20 reading arrives about when the :25 METAR is taken).
                match next_routine_report(n.interval_end, ctx.routine_minutes) {
                    None => blockers.push("report schedule unknown".into()),
                    Some(next) => {
                        let lead = next - n.interval_end;
                        if lead > Duration::minutes(cfg.max_lead_minutes) {
                            blockers.push(format!(
                                "reading {} min before the next report > {}",
                                lead.num_minutes(),
                                cfg.max_lead_minutes
                            ));
                        }
                    }
                }
                match n.mean.map(|t| t.tenths()) {
                    None => blockers.push("KNMI mean missing".into()),
                    Some(m) if m < trigger => blockers.push(format!(
                        "KNMI mean {} < {} (high {high} + 0.5 + {})",
                        c(m),
                        c(trigger),
                        c(cfg.mean_margin_tenths)
                    )),
                    Some(_) => {}
                }
                if cfg.require_max_at_edge && n.max.is_none_or(|t| t.tenths() < edge) {
                    blockers.push(format!("KNMI maximum below {}", c(edge)));
                }
            }
        }
        let fees = ctx.market.fees;
        let shares = ask.and_then(|a| {
            let raw = shares_for_notional(cfg.notional, a, Rounding::Down);
            let s = round_shares_to_lot(raw, Shares::from_whole(1), Rounding::Down);
            (s >= outcome.min_order_size && s.micros() > 0).then_some(s)
        });
        match (book, ask) {
            (None, _) => blockers.push("no NO book".into()),
            (Some(_), None) => blockers.push("no NO ask".into()),
            (Some(b), Some(a)) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if a < cfg.min_price || a > cfg.max_price {
                    blockers.push(format!(
                        "NO ask {a} outside [{}, {}]",
                        cfg.min_price, cfg.max_price
                    ));
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
        let ev = ask.map(|a| ev_per_share(cfg.p_new_high, a, &fees, cfg.slippage_allowance));
        if let Some(e) = ev
            && e < cfg.min_edge
        {
            blockers.push(format!("edge {e:.4} < {:.4}", cfg.min_edge));
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let be = ask.map(|a| break_even_probability(a, &fees, cfg.slippage_allowance));
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: OutcomeSide::No,
            token: token.clone(),
            ask,
            bid,
            p_win: Some(cfg.p_new_high),
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
            model_p: None,
            market_p: None,
            maker_bid: None,
        });
        if signal && let (Some(a), Some(sh), Some(b)) = (ask, shares, be) {
            out.proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: outcome.label.clone(),
                bucket: outcome.bucket,
                condition_id: outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::No,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: a,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: cfg.p_new_high,
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!("{reading}: the next METAR should report {}", high + 1),
                    format!(
                        "NO {} at {a}: p {:.2} (assumed) vs break-even {b:.3}",
                        outcome.label, cfg.p_new_high
                    ),
                    "faster data: KNMI's ten-minute reading comes before the METAR".into(),
                ],
            });
        }
        out
    }
}
