//! Strategy F — the high's bucket inside the season's peak slot.
//!
//! When is the day's high usually measured? That depends on the season, so
//! the station's METAR history is split per season and the local time at
//! which each day's high was first reported is collected ([`PeakTimes`]).
//! Inside the current season's *slot* — by default from the median of those
//! times to their 90th percentile — F watches the order book of the bucket
//! that holds the day's highest temperature so far, and once that bucket is
//! offered above 0.90 it buys a fixed 100 shares of its YES (walking the
//! offers, at most up to `max_price`). One position per day and bucket.
//!
//! * "The current measured temperature" is read as the day's high so far: a
//!   bucket below it has already lost, whatever the latest report says.
//! * The slot comes from the installed model's peak times; until a model
//!   carrying them is installed, `fallback_slots` apply.
//! * F claims no model edge, but like A, B and E it stays silent without a
//!   loaded probability model (fail closed: no model, no weather trades).
//!
//! The premise "after the slot the temperature cannot go higher" is not a
//! fact: by construction one day in ten (the slot's upper quantile) reports
//! its high later, and the price rule only asks the market to agree. P&L
//! depends on whether the market underprices the high's bucket inside the
//! slot. On 120 settled Amsterdam days (June–September 2026) buckets the
//! market priced 0.90–0.98 won 96.8 % of the time at a mean price of 0.949:
//! a favourite discount of about two points, the size of spread, fee and
//! slippage. E, which buys 0.90–0.99 between 12:00 and 18:00, was flat to
//! slightly negative at traded prices (76 of 79 won, −1 % per trade, mean
//! price 0.964). The default cap of 0.95 keeps F where the discount was
//! measured and away from 0.96–0.99, where one loss costs 30–220 wins.
//! HYPOTHESIS TO BACKTEST: `research market` replays F and its variants at
//! traded prices (as limit orders too) and picks among them out of sample.

use crate::ev::{break_even_probability, ev_per_share};
use crate::peak_times::{PeakTimes, hm};
use crate::strategy::{
    BucketEvaluation, Pooling, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    default_market_weight, default_max_market_spread, holds_or_pending, max_data_age,
    min_p_in_bucket,
};
use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::{OrderBook, OutcomeSide, Side};
use wm_core::time::Season;
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{Price, Shares};

/// A slot per season, `[start, end)` in minutes after local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeasonSlots {
    pub winter: (u16, u16),
    pub spring: (u16, u16),
    pub summer: (u16, u16),
    pub autumn: (u16, u16),
}

impl SeasonSlots {
    pub fn get(&self, season: Season) -> (u16, u16) {
        match season {
            Season::Winter => self.winter,
            Season::Spring => self.spring,
            Season::Summer => self.summer,
            Season::Autumn => self.autumn,
        }
    }

    pub fn all(&self) -> [(Season, (u16, u16)); 4] {
        [
            (Season::Winter, self.winter),
            (Season::Spring, self.spring),
            (Season::Summer, self.summer),
            (Season::Autumn, self.autumn),
        ]
    }
}

impl Default for SeasonSlots {
    /// Used only until the history's peak times are learned: maxima come
    /// about 2–3 hours after solar noon (KMI), the first report at the
    /// whole-degree high somewhat earlier.
    fn default() -> Self {
        Self {
            winter: (13 * 60, 16 * 60),
            spring: (14 * 60 + 30, 17 * 60 + 30),
            summer: (15 * 60, 18 * 60),
            autumn: (14 * 60, 17 * 60),
        }
    }
}

/// Configuration of strategy F. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeakSlotConfig {
    pub enabled: bool,
    /// The season's slot starts at this quantile of the local time the
    /// day's high was first reported (history) ...
    pub slot_from_quantile: f64,
    /// ... and ends after this one.
    pub slot_to_quantile: f64,
    /// Slots until the installed model carries the history's peak times.
    pub fallback_slots: SeasonSlots,
    /// Buy only once the best ask is above this (exclusive: "above 0.90").
    pub min_price: Price,
    /// ... and when all `shares` can be bought at or below this.
    pub max_price: Price,
    /// Fixed size of the buy.
    pub shares: Shares,
    /// Optional: the latest report at least this far below the high (tenths
    /// °C); 0 = no temperature condition.
    pub min_drop_tenths: i32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    /// Used for the EV shown with each evaluation (not a gate).
    pub slippage_allowance: Price,
}

impl Default for PeakSlotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            slot_from_quantile: 0.50,
            slot_to_quantile: 0.90,
            fallback_slots: SeasonSlots::default(),
            min_price: Price::saturating_from_micros(900_000),
            max_price: Price::saturating_from_micros(950_000),
            shares: Shares::from_whole(100),
            min_drop_tenths: 0,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
        }
    }
}

impl PeakSlotConfig {
    /// The slot of `season` and where it came from.
    pub fn slot(&self, season: Season, peak_times: Option<&PeakTimes>) -> ((u16, u16), SlotSource) {
        match peak_times.and_then(|pt| {
            pt.slot(season, self.slot_from_quantile, self.slot_to_quantile)
                .zip(pt.season(season).map(|s| s.days))
        }) {
            Some((slot, days)) => (slot, SlotSource::History { days }),
            None => (self.fallback_slots.get(season), SlotSource::Fallback),
        }
    }
}

/// Where a slot came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotSource {
    /// The installed model's peak times over this many days of the season.
    History { days: u32 },
    /// The configured fallback (no peak times installed yet).
    Fallback,
}

impl SlotSource {
    fn describe(self, from_q: f64, to_q: f64) -> String {
        match self {
            SlotSource::History { days } => format!(
                "{} → {} of {days} days' peak times",
                quantile_name(from_q),
                quantile_name(to_q)
            ),
            SlotSource::Fallback => "fallback: peak times not learned yet".into(),
        }
    }
}

fn quantile_name(q: f64) -> String {
    if (q - 0.5).abs() < 1e-9 {
        "median".into()
    } else {
        format!("{:.0}%", 100.0 * q)
    }
}

/// The price at which `shares` are all offered, walking the asks (best
/// first) no further than `cap`; `None` when fewer are offered up to it.
pub fn sweep_price(book: &OrderBook, shares: Shares, cap: Price) -> Option<Price> {
    let mut have = Shares::ZERO;
    for l in book.asks.iter().take_while(|l| l.price <= cap) {
        have += l.size;
        if have >= shares {
            return Some(l.price);
        }
    }
    None
}

/// Strategy F.
pub struct PeakSlotHigh {
    id: StrategyId,
    pub config: PeakSlotConfig,
}

impl PeakSlotHigh {
    pub fn new(config: PeakSlotConfig) -> Self {
        Self {
            id: StrategyId::from_static("F_peak_slot"),
            config,
        }
    }
}

impl Strategy for PeakSlotHigh {
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
        let Some(outcome) = ctx.market.outcome_for_value(high) else {
            return out;
        };
        let Some(features) = ctx.views.first().map(|v| &v.assessment.features) else {
            return out;
        };
        let fees = ctx.market.fees;
        let token = &outcome.yes_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let mut blockers: Vec<String> = Vec::new();
        if !cfg.enabled {
            blockers.push("strategy disabled".into());
        }

        // Inside the season's slot.
        let minute = features.local_minute_now;
        let season = features.season;
        let ((start, end), source) = cfg.slot(season, ctx.peak_times);
        if !(start..end).contains(&minute) {
            blockers.push(format!(
                "{} outside the {} slot {}–{}",
                hm(minute),
                season.as_str(),
                hm(start),
                hm(end)
            ));
        }

        // Optional temperature condition.
        let drop = ctx
            .views
            .iter()
            .map(|v| v.assessment.features.drop_tenths)
            .min()
            .unwrap_or(0);
        if drop < cfg.min_drop_tenths {
            blockers.push(format!(
                "{:.1} °C below the high < {:.1}",
                f64::from(drop) / 10.0,
                f64::from(cfg.min_drop_tenths) / 10.0
            ));
        }
        if max_data_age(ctx.views) > cfg.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }

        // Offered above min_price, all shares at or below max_price.
        let mut limit = None;
        match (book, ask) {
            (None, _) => blockers.push("no order book".into()),
            (Some(_), None) => blockers.push("no ask".into()),
            (Some(b), Some(pr)) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if pr <= cfg.min_price {
                    blockers.push(format!("ask {pr} not above {}", cfg.min_price));
                } else if pr > cfg.max_price {
                    blockers.push(format!("ask {pr} above {}", cfg.max_price));
                } else {
                    limit = sweep_price(b, cfg.shares, cfg.max_price);
                    if limit.is_none() {
                        let (depth, _) = b.ask_depth_up_to(cfg.max_price);
                        blockers.push(format!(
                            "only {depth} shares offered ≤ {} (need {})",
                            cfg.max_price, cfg.shares
                        ));
                    }
                }
            }
        }

        // A loaded model (fail closed, as for A, B and E); not a gate beyond.
        let model = min_p_in_bucket(ctx.views, high, &outcome.bucket);
        if model.is_none() {
            blockers.push("no probability model".into());
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        if cfg.shares < outcome.min_order_size {
            blockers.push("size below market minimum".into());
        }

        // Shown, not a gate: the model pooled with the market, and the EV
        // at the price the whole size costs.
        let pooling = Pooling {
            weight: default_market_weight(),
            max_spread: default_max_market_spread(),
            max_book_age_ms: cfg.max_book_age_ms,
        };
        let market_p = pooling.market_probability(book, ctx.books.get(&outcome.no_token), ctx.now);
        let p_win = model
            .map(|(pm, _)| pooling.win_probability(pm, market_p))
            .or(market_p);
        let priced = limit.or(ask);
        let ev = p_win
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
            p_win,
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
            model_p: model.map(|x| x.0),
            market_p,
            maker_bid: None,
        });
        if signal && let (Some(pr), Some(lim), Some(b)) = (ask, limit, be) {
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
                shares: cfg.shares,
                tif: TimeInForce::Fak,
                p_win: p_win.unwrap_or(b),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!(
                        "{} local inside the {} slot {}–{} ({})",
                        hm(minute),
                        season.as_str(),
                        hm(start),
                        hm(end),
                        source.describe(cfg.slot_from_quantile, cfg.slot_to_quantile)
                    ),
                    format!(
                        "high {high}{} bucket offered at {pr} (> {}): {} shares at ≤ {lim}",
                        ctx.market.unit.symbol(),
                        cfg.min_price,
                        cfg.shares
                    ),
                    "rule-based: no model edge claimed (the favourite discount inside the peak slot is the hypothesis, replayed by research market)".into(),
                ],
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peak_times::PeakTimesBuilder;
    use crate::state::ObsPoint;
    use chrono::{DateTime, Duration, NaiveDate, Utc};
    use wm_core::ids::TokenId;
    use wm_core::market::BookLevel;
    use wm_core::units::TempC;
    use wm_core::weather::ReportType;

    fn book(asks: &[(&str, i64)]) -> OrderBook {
        OrderBook {
            token: TokenId::new("yes").unwrap(),
            bids: vec![],
            asks: asks
                .iter()
                .map(|(p, s)| BookLevel {
                    price: Price::parse(p).unwrap(),
                    size: Shares::from_whole(*s),
                })
                .collect(),
            tick_size: Price::saturating_from_micros(10_000),
            min_order_size: Shares::from_whole(5),
            exchange_ts: None,
            received_at: "2026-07-02T13:00:00Z".parse().unwrap(),
            hash: None,
            confirmed_at: None,
        }
    }

    #[test]
    fn the_sweep_price_buys_every_share_or_none() {
        let cap = Price::parse("0.95").unwrap();
        let hundred = Shares::from_whole(100);
        let b = book(&[("0.92", 60), ("0.93", 30), ("0.95", 50), ("0.97", 500)]);
        assert_eq!(sweep_price(&b, hundred, cap), Price::parse("0.95").ok());
        assert_eq!(
            sweep_price(&b, Shares::from_whole(60), cap),
            Price::parse("0.92").ok(),
            "exactly the first level"
        );
        assert_eq!(
            sweep_price(&b, Shares::from_whole(141), cap),
            None,
            "0.97 is above the cap"
        );
        assert_eq!(sweep_price(&book(&[]), hundred, cap), None);
    }

    #[test]
    fn the_slot_comes_from_history_else_from_the_fallback() {
        let cfg = PeakSlotConfig::default();
        let (slot, source) = cfg.slot(Season::Summer, None);
        assert_eq!(slot, (15 * 60, 18 * 60));
        assert_eq!(source, SlotSource::Fallback);
        // Ten summer days with their high first reported at 14:25 … 18:55.
        let mut b = PeakTimesBuilder::new();
        let start: DateTime<Utc> = "2026-07-01T22:25:00Z".parse().unwrap();
        for k in 0..10 {
            let points: Vec<ObsPoint> = (0..48)
                .map(|i| {
                    let minute = (25 + 30 * i) % 1440;
                    let peak = i == 28 + k / 2;
                    ObsPoint {
                        observed_at: start + Duration::minutes(30 * i64::from(i)),
                        local_minute_of_day: minute,
                        local_minute_of_hour: (minute % 60) as u8,
                        temp: TempC::from_whole(if peak { 25 } else { 15 }),
                        report_type: ReportType::Metar,
                        version: 1,
                    }
                })
                .collect();
            b.add_day(
                NaiveDate::from_ymd_opt(2026, 7, 2).unwrap(),
                Season::Summer,
                &points,
                25,
            );
        }
        let pt = b.build();
        let (slot, source) = cfg.slot(Season::Summer, Some(&pt));
        assert_eq!(source, SlotSource::History { days: 10 });
        // Peaks at index 28..=32, two days each: 14:25, 14:55, 15:25, 15:55, 16:25.
        assert_eq!(slot, (15 * 60 + 25, 16 * 60 + 26));
        // No winter history: the fallback.
        assert_eq!(cfg.slot(Season::Winter, Some(&pt)).1, SlotSource::Fallback);
        assert_eq!(
            SlotSource::History { days: 10 }.describe(0.5, 0.9),
            "median → 90% of 10 days' peak times"
        );
    }
}
