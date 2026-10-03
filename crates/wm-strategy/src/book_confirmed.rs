//! Strategy E — the day's high, confirmed by the clock, the thermometer and
//! the order book.
//!
//! Buys YES on the bucket that holds the observed high when, all at once:
//!
//! * the local time is inside a window (`start_local_minute` ..
//!   `end_local_minute`);
//! * the high was first reached at least `min_minutes_at_high` ago and the
//!   latest report is at least `min_drop_tenths` below it — the temperature
//!   has peaked and come down;
//! * the book is shrinking: the YES shares offered at or below the price cap
//!   fell by at least `min_depth_shrink` over the last `lookback_minutes`
//!   while the best ask did not fall — buyers lift the offers or sellers
//!   withdraw, i.e. the market converges on this bucket;
//! * the ask is within `[min_price, max_price]` (0.90–0.99 by default).
//!
//! It claims no model edge, but like A and B it stays silent without a loaded
//! probability model (fail closed); with `min_model_p` > 0 the model's
//! probability is also a veto. Its premise is the favourite–longshot pattern:
//! late favourites have tended to win slightly more often than their price
//! (Polymarket purchases at ≥ 90¢ earned +0.83¢ per dollar; Kalshi
//! temperature buckets the market quoted at 0.99 settled YES), while other
//! work finds short-horizon weather prices too extreme. The margin is of the
//! size of fee plus spread, so the rule is a HYPOTHESIS TO BACKTEST:
//! `research market` replays it at traded prices.
//!
//! Book depth is measured on the levels the engine keeps (the best five per
//! side). A level entering or leaving those five is not a change of the
//! book, so two states are compared only up to the highest price both show
//! (and at most `max_price`). The history of states is kept here, fed with
//! every book update the engine receives ([`Strategy::observe_book`]) and
//! with the books seen at each evaluation, coalesced to one sample per ten
//! seconds and pruned beyond the lookback, so replays reproduce it.

use crate::ev::{break_even_probability, ev_per_share};
use crate::strategy::{
    BucketEvaluation, Pooling, Proposal, Strategy, StrategyContext, StrategyOutput, common_high,
    default_market_weight, default_max_market_spread, holds_or_pending, max_data_age,
    min_p_in_bucket, size_for,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use wm_core::ids::{StrategyId, TokenId};
use wm_core::market::{OrderBook, OutcomeSide, Side};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{Price, Usd};

/// Configuration of strategy E. Every threshold is a research parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookConfirmedConfig {
    pub enabled: bool,
    /// Local time window `[start, end)`, minutes after local midnight.
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    /// The high was first reached at least this long ago (plateau-aware:
    /// counted from the first report at the high).
    pub min_minutes_at_high: i64,
    /// The latest report is at least this far below the high (tenths °C).
    pub min_drop_tenths: i32,
    /// The book is compared with its state this long ago.
    pub lookback_minutes: i64,
    /// Fraction by which the shares offered at or below `max_price` must
    /// have fallen over the lookback (0.3 = 30 %).
    pub min_depth_shrink: f64,
    /// A book offering fewer shares at the start of the lookback carries no
    /// signal.
    pub min_depth_shares: f64,
    pub min_price: Price,
    pub max_price: Price,
    /// Veto: the model's probability that the high's bucket wins must be at
    /// least this; 0 = no veto (a loaded model is still required).
    pub min_model_p: f64,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    /// Used for the EV shown with each evaluation (not a gate).
    pub slippage_allowance: Price,
    pub notional: Usd,
}

impl Default for BookConfirmedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            start_local_minute: 12 * 60,
            end_local_minute: 18 * 60,
            min_minutes_at_high: 60,
            min_drop_tenths: 10,
            lookback_minutes: 30,
            min_depth_shrink: 0.30,
            min_depth_shares: 50.0,
            min_price: Price::saturating_from_micros(900_000),
            max_price: Price::saturating_from_micros(990_000),
            min_model_p: 0.0,
            max_data_age_minutes: 40,
            max_book_age_ms: 15_000,
            slippage_allowance: Price::saturating_from_micros(5_000),
            notional: Usd::from_whole(10),
        }
    }
}

/// Book states closer together than this are coalesced into one sample.
pub const SAMPLE_SPACING_SECS: i64 = 10;

/// One recorded state of a token's book.
#[derive(Debug, Clone, PartialEq)]
pub struct BookSample {
    /// When the state began (receipt time of the book).
    pub at: DateTime<Utc>,
    /// Receipt time of the latest book coalesced into this sample.
    pub updated_at: DateTime<Utc>,
    pub best_ask: Option<Price>,
    /// Ask levels at or below the strategy's price cap: (price, shares).
    pub asks: Vec<(Price, f64)>,
    /// Highest ask price the book showed; nothing above it is visible.
    pub top: Option<Price>,
}

impl BookSample {
    /// The state of `book`, keeping the ask levels at or below `cap`.
    pub fn of(book: &OrderBook, cap: Price) -> Self {
        Self {
            at: book.received_at,
            updated_at: book.received_at,
            best_ask: book.best_ask().map(|l| l.price),
            asks: book
                .asks
                .iter()
                .filter(|l| l.price <= cap)
                .map(|l| (l.price, l.size.as_f64()))
                .collect(),
            top: book.asks.iter().map(|l| l.price).max(),
        }
    }

    /// Shares offered at or below `x`.
    pub fn depth_upto(&self, x: Price) -> f64 {
        // Folded from +0.0: an empty f64 `sum()` is −0.0, which prints "-0".
        self.asks
            .iter()
            .filter(|(p, _)| *p <= x)
            .fold(0.0, |total, (_, s)| total + s)
    }
}

/// Recent book states per token, oldest first.
#[derive(Debug, Clone)]
pub struct BookHistory {
    /// How far back from a token's newest state questions are answered.
    keep: Duration,
    by_token: HashMap<TokenId, VecDeque<BookSample>>,
    swept_at: Option<DateTime<Utc>>,
}

impl BookHistory {
    /// A history answering [`BookHistory::state_at`] up to `keep` before
    /// each token's newest state.
    pub fn new(keep: Duration) -> Self {
        Self {
            keep,
            by_token: HashMap::new(),
            swept_at: None,
        }
    }

    /// Record `book` (ask levels at or below `cap`). A book older than the
    /// last one recorded is ignored; one within [`SAMPLE_SPACING_SECS`] of
    /// the last sample's start replaces its values (the slot keeps its start
    /// time; with equal receipt times the later book wins, as in the
    /// engine's book map). States that ended more than `keep` before the
    /// newest are dropped, and once an hour tokens without a new state for a
    /// day are forgotten.
    pub fn record(&mut self, book: &OrderBook, cap: Price) {
        let sample = BookSample::of(book, cap);
        let at = sample.at;
        let q = self.by_token.entry(book.token.clone()).or_default();
        match q.back_mut() {
            Some(last) if at < last.updated_at => return,
            Some(last) if at - last.at < Duration::seconds(SAMPLE_SPACING_SECS) => {
                *last = BookSample {
                    at: last.at,
                    ..sample
                };
            }
            _ => q.push_back(sample),
        }
        let from = at - self.keep;
        while q.len() >= 2 && q[1].at <= from {
            q.pop_front();
        }
        if self.swept_at.is_none_or(|s| at - s >= Duration::hours(1)) {
            self.swept_at = Some(at);
            let quiet = at - Duration::days(1);
            self.by_token
                .retain(|_, q| q.back().is_some_and(|s| s.at > quiet));
        }
    }

    /// The recorded state in force at `t`: the last sample at or before it.
    pub fn state_at(&self, token: &TokenId, t: DateTime<Utc>) -> Option<&BookSample> {
        self.by_token.get(token)?.iter().rev().find(|s| s.at <= t)
    }

    /// Samples held for a token.
    pub fn len(&self, token: &TokenId) -> usize {
        self.by_token.get(token).map_or(0, VecDeque::len)
    }

    /// Tokens held.
    pub fn tokens(&self) -> usize {
        self.by_token.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

/// How the book changed over the lookback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shrink {
    /// Depth is compared at or below this price: the cap, or lower when
    /// either state shows no levels above it.
    pub edge: Price,
    pub depth_then: f64,
    pub depth_now: f64,
    pub ask_then: Option<Price>,
    pub ask_now: Option<Price>,
}

impl Shrink {
    /// Compare two states over the prices both show, up to `cap`.
    pub fn between(then: &BookSample, now: &BookSample, cap: Price) -> Self {
        let edge = [then.top, now.top]
            .into_iter()
            .flatten()
            .fold(cap, std::cmp::min);
        Self {
            edge,
            depth_then: then.depth_upto(edge),
            depth_now: now.depth_upto(edge),
            ask_then: then.best_ask,
            ask_now: now.best_ask,
        }
    }

    /// Fraction of the offered shares gone (negative when the book grew).
    pub fn fraction(&self) -> f64 {
        if self.depth_then > 0.0 {
            1.0 - self.depth_now / self.depth_then
        } else {
            0.0
        }
    }

    /// The best ask did not move down (thinning by buying or withdrawing,
    /// not by sellers undercutting each other).
    pub fn ask_held(&self) -> bool {
        matches!((self.ask_then, self.ask_now), (Some(a), Some(b)) if b >= a)
    }
}

fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Shares in a blocker or rationale: whole numbers bare, else at most two
/// decimals, rounded down — 49.6 offered against a minimum of 50 reads
/// "49.6", never "50".
fn share_count(x: f64) -> String {
    let cents = (x.max(0.0) * 100.0 + 1e-9).floor() / 100.0;
    format!("{cents:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

/// A change in percent with its sign: "+12%", "−3%".
fn signed_pct(x: f64) -> String {
    let r = x.round();
    if r < 0.0 {
        format!("−{:.0}%", -r)
    } else {
        format!("+{:.0}%", r.abs())
    }
}

/// Strategy E.
pub struct BookConfirmedHigh {
    id: StrategyId,
    pub config: BookConfirmedConfig,
    history: BookHistory,
}

impl BookConfirmedHigh {
    pub fn new(config: BookConfirmedConfig) -> Self {
        let keep = Duration::minutes(config.lookback_minutes.max(1));
        Self {
            id: StrategyId::from_static("E_book_confirmed_high"),
            config,
            history: BookHistory::new(keep),
        }
    }

    pub fn history(&self) -> &BookHistory {
        &self.history
    }
}

impl Strategy for BookConfirmedHigh {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn observe_book(&mut self, book: &OrderBook) {
        self.history.record(book, self.config.max_price);
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let cfg = &self.config;
        // Keep the YES books' history, whatever else holds.
        for o in &ctx.market.outcomes {
            if let Some(b) = ctx.books.get(&o.yes_token) {
                self.history.record(b, cfg.max_price);
            }
        }
        let lookback = Duration::minutes(cfg.lookback_minutes.max(1));

        let mut out = StrategyOutput::default();
        let Ok(high) = common_high(ctx.views) else {
            return out;
        };
        let Some(outcome) = ctx.market.outcome_for_value(high) else {
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

        // Inside the time window.
        let minute = ctx
            .views
            .first()
            .map_or(0, |v| v.assessment.features.local_minute_now);
        if !(cfg.start_local_minute..cfg.end_local_minute).contains(&minute) {
            blockers.push(format!(
                "{} outside {}–{}",
                hm(minute),
                hm(cfg.start_local_minute),
                hm(cfg.end_local_minute)
            ));
        }

        // The temperature has peaked: high reached a while ago, now below it.
        let held = ctx
            .views
            .iter()
            .map(|v| v.assessment.features.minutes_since_first_high)
            .min()
            .unwrap_or(0);
        if held < cfg.min_minutes_at_high {
            blockers.push(format!(
                "high reached {held}m ago < {}m",
                cfg.min_minutes_at_high
            ));
        }
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

        // A fresh book with an ask in range.
        match (book, ask) {
            (None, _) => blockers.push("no order book".into()),
            (Some(_), None) => blockers.push("no ask".into()),
            (Some(b), Some(pr)) => {
                if b.age_ms(ctx.now) > cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                if pr < cfg.min_price || pr > cfg.max_price {
                    blockers.push(format!(
                        "ask {pr} outside [{}, {}]",
                        cfg.min_price, cfg.max_price
                    ));
                }
            }
        }

        // The book is shrinking.
        let shrink = book.and_then(|b| {
            let then = self.history.state_at(token, ctx.now - lookback)?;
            Some(Shrink::between(
                then,
                &BookSample::of(b, cfg.max_price),
                cfg.max_price,
            ))
        });
        match (&shrink, book) {
            (None, Some(_)) => blockers.push(format!(
                "book history shorter than {}m",
                cfg.lookback_minutes
            )),
            (Some(s), _) => {
                if s.depth_then < cfg.min_depth_shares {
                    blockers.push(format!(
                        "only {} shares offered ≤ {} {}m ago (< {})",
                        share_count(s.depth_then),
                        s.edge,
                        cfg.lookback_minutes,
                        share_count(cfg.min_depth_shares)
                    ));
                } else if s.fraction() < cfg.min_depth_shrink {
                    blockers.push(format!(
                        "book not shrinking: {} → {} shares offered ≤ {} in {}m ({}, need −{:.0}%)",
                        share_count(s.depth_then),
                        share_count(s.depth_now),
                        s.edge,
                        cfg.lookback_minutes,
                        signed_pct(-100.0 * s.fraction()),
                        100.0 * cfg.min_depth_shrink
                    ));
                }
                // Without an ask then, the depth blockers above apply.
                if let (false, Some(a), Some(b)) = (s.ask_held(), s.ask_then, s.ask_now) {
                    blockers.push(format!("best ask fell {a} → {b}"));
                }
            }
            (None, None) => {}
        }

        // A loaded model (fail closed, as for A and B); its probability is a
        // veto only with `min_model_p` > 0.
        let model = min_p_in_bucket(ctx.views, high, &outcome.bucket);
        match model {
            None => blockers.push("no probability model".into()),
            Some((p, _)) if p < cfg.min_model_p => {
                blockers.push(format!("model {p:.3} < {:.3}", cfg.min_model_p));
            }
            Some(_) => {}
        }
        if holds_or_pending(ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let shares = ask.and_then(|pr| size_for(cfg.notional, pr, outcome.min_order_size));
        if ask.is_some() && shares.is_none() {
            blockers.push("size below market minimum".into());
        }

        // Shown, not a gate: the model pooled with the market, and its EV.
        let pooling = Pooling {
            weight: default_market_weight(),
            max_spread: default_max_market_spread(),
            max_book_age_ms: cfg.max_book_age_ms,
        };
        let market_p = pooling.market_probability(book, ctx.books.get(&outcome.no_token), ctx.now);
        let p_win = model
            .map(|(pm, _)| pooling.win_probability(pm, market_p))
            .or(market_p);
        let ev = p_win
            .zip(ask)
            .map(|(p, pr)| ev_per_share(p, pr, &fees, cfg.slippage_allowance));
        let be = ask.map(|pr| break_even_probability(pr, &fees, cfg.slippage_allowance));
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
        if signal && let (Some(pr), Some(sh), Some(b), Some(s)) = (ask, shares, be, shrink) {
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
                p_win: p_win.unwrap_or(b),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: b,
                research_only: false,
                rationale: vec![
                    format!(
                        "{} local: high {high}{} first reached {held}m ago, now {:.1} °C below it",
                        hm(minute),
                        ctx.market.unit.symbol(),
                        f64::from(drop) / 10.0
                    ),
                    format!(
                        "book shrinking: {} → {} shares offered ≤ {} in {}m (−{:.0}%), best ask {} → {pr}",
                        share_count(s.depth_then),
                        share_count(s.depth_now),
                        s.edge,
                        cfg.lookback_minutes,
                        100.0 * s.fraction(),
                        s.ask_then.map_or_else(|| "none".to_owned(), |p| p.to_string()),
                    ),
                    "rule-based: no model edge claimed (favourite–longshot hypothesis, replayed by research market)".into(),
                ],
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::market::BookLevel;
    use wm_core::units::Shares;

    #[test]
    fn share_counts_never_round_up_to_a_threshold() {
        // 1 Oct: "only 50 shares offered ≤ 0.79 30m ago (< 50)".
        assert_eq!(share_count(49.6), "49.6");
        assert_eq!(share_count(49.999), "49.99");
        assert_eq!(share_count(24.14), "24.14");
        assert_eq!(share_count(0.29), "0.29");
        assert_eq!(share_count(50.0), "50");
        assert_eq!(share_count(600.0), "600");
        assert_eq!(share_count(0.0), "0");
        assert_eq!(share_count(-0.0), "0");
        assert_eq!(share_count(-1e-9), "0");
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn book(at: &str, asks: &[(&str, i64)]) -> OrderBook {
        OrderBook {
            token: TokenId::new("7").unwrap(),
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
            received_at: t(at),
            hash: None,
            confirmed_at: None,
        }
    }

    fn cap() -> Price {
        Price::saturating_from_micros(990_000)
    }

    #[test]
    fn a_sample_keeps_the_levels_under_the_cap_and_the_visible_top() {
        let s = BookSample::of(
            &book(
                "2026-09-28T12:00:00Z",
                &[("0.95", 40), ("0.99", 60), ("0.995", 500)],
            ),
            cap(),
        );
        assert_eq!(s.asks.len(), 2);
        assert_eq!(s.top, Some(Price::parse("0.995").unwrap()));
        assert_eq!(s.best_ask, Some(Price::parse("0.95").unwrap()));
        assert!(
            (s.depth_upto(cap()) - 100.0).abs() < 1e-9,
            "0.995 is above the cap"
        );
        assert!((s.depth_upto(Price::parse("0.97").unwrap()) - 40.0).abs() < 1e-9);
        let empty = BookSample::of(&book("2026-09-28T12:00:00Z", &[]), cap());
        assert_eq!((empty.top, empty.best_ask), (None, None));
        assert_eq!(empty.depth_upto(cap()), 0.0);
        // Blocker texts print the depth: "only 0 shares", never "only -0".
        assert_eq!(format!("{:.0}", empty.depth_upto(cap())), "0");
        assert_eq!(
            format!("{:.0}", s.depth_upto(Price::parse("0.90").unwrap())),
            "0"
        );
    }

    #[test]
    fn history_coalesces_prunes_and_answers_the_state_at_a_time() {
        let mut h = BookHistory::new(Duration::minutes(30));
        let token = TokenId::new("7").unwrap();
        h.record(&book("2026-09-28T12:00:00Z", &[("0.95", 200)]), cap());
        // Within ten seconds: coalesced into the 12:00:00 slot; with the
        // same receipt time the later book wins; older books are ignored.
        h.record(&book("2026-09-28T12:00:04Z", &[("0.95", 150)]), cap());
        h.record(&book("2026-09-28T12:00:04Z", &[("0.95", 120)]), cap());
        h.record(&book("2026-09-28T12:00:02Z", &[("0.95", 1)]), cap());
        h.record(&book("2026-09-28T11:59:00Z", &[("0.95", 1)]), cap());
        h.record(&book("2026-09-28T12:20:00Z", &[("0.97", 80)]), cap());
        assert_eq!(h.len(&token), 2);
        let s = h.state_at(&token, t("2026-09-28T12:10:00Z")).unwrap();
        assert_eq!(s.at, t("2026-09-28T12:00:00Z"));
        assert_eq!(s.updated_at, t("2026-09-28T12:00:04Z"));
        assert!((s.depth_upto(cap()) - 120.0).abs() < 1e-9);
        assert_eq!(h.state_at(&token, t("2026-09-28T11:00:00Z")), None);
        let s = h.state_at(&token, t("2026-09-28T13:00:00Z")).unwrap();
        assert_eq!(s.best_ask, Some(Price::parse("0.97").unwrap()));
        // Thirty minutes kept: the state in force 30 minutes before the
        // newest survives, older ones go.
        h.record(&book("2026-09-28T12:50:00Z", &[("0.98", 10)]), cap());
        assert_eq!(h.len(&token), 2);
        let s = h.state_at(&token, t("2026-09-28T12:20:00Z")).unwrap();
        assert_eq!(s.at, t("2026-09-28T12:20:00Z"));
        // A token quiet for a day is forgotten when another one updates.
        let mut other = book("2026-09-29T13:00:00Z", &[("0.50", 10)]);
        other.token = TokenId::new("8").unwrap();
        h.record(&other, cap());
        assert_eq!((h.tokens(), h.len(&token)), (1, 0));
    }

    #[test]
    fn shrink_compares_only_the_prices_both_states_show() {
        let state = |at: &str, asks: &[(&str, i64)]| BookSample::of(&book(at, asks), cap());
        // Buyers lift 0.91 and 0.92: the five visible levels move up.
        let then = state(
            "2026-09-28T12:00:00Z",
            &[
                ("0.91", 100),
                ("0.92", 100),
                ("0.93", 100),
                ("0.94", 100),
                ("0.95", 100),
            ],
        );
        let now = state(
            "2026-09-28T12:30:00Z",
            &[
                ("0.93", 100),
                ("0.94", 100),
                ("0.95", 100),
                ("0.96", 100),
                ("0.97", 100),
            ],
        );
        let s = Shrink::between(&then, &now, cap());
        assert_eq!(s.edge, Price::parse("0.95").unwrap());
        assert!((s.depth_then - 500.0).abs() < 1e-9 && (s.depth_now - 300.0).abs() < 1e-9);
        assert!((s.fraction() - 0.4).abs() < 1e-12 && s.ask_held());

        // A seller inserts 0.955 and pushes the big 0.99 level out of the
        // five kept: counted up to 0.99 that would look like −54 %.
        let then = state(
            "2026-09-28T12:00:00Z",
            &[
                ("0.95", 100),
                ("0.96", 100),
                ("0.97", 100),
                ("0.98", 100),
                ("0.99", 500),
            ],
        );
        let now = state(
            "2026-09-28T12:30:00Z",
            &[
                ("0.95", 100),
                ("0.955", 10),
                ("0.96", 100),
                ("0.97", 100),
                ("0.98", 100),
            ],
        );
        assert!(1.0 - now.depth_upto(cap()) / then.depth_upto(cap()) > 0.5);
        let s = Shrink::between(&then, &now, cap());
        assert_eq!(s.edge, Price::parse("0.98").unwrap());
        assert!(s.fraction() < 0.0, "the book grew: {s:?}");
    }

    #[test]
    fn percentages_carry_a_real_sign() {
        assert_eq!(signed_pct(-3.3), "−3%");
        assert_eq!(signed_pct(12.6), "+13%");
        assert_eq!(signed_pct(-0.3), "+0%");
    }

    #[test]
    fn shrink_needs_a_held_ask() {
        let p = |s: &str| Some(Price::parse(s).unwrap());
        let s = Shrink {
            edge: cap(),
            depth_then: 200.0,
            depth_now: 50.0,
            ask_then: p("0.93"),
            ask_now: p("0.95"),
        };
        assert!((s.fraction() - 0.75).abs() < 1e-12 && s.ask_held());
        let fell = Shrink {
            ask_now: p("0.92"),
            ..s
        };
        assert!(!fell.ask_held());
        let grew = Shrink {
            depth_now: 300.0,
            ..s
        };
        assert!(grew.fraction() < 0.0);
        let empty = Shrink {
            depth_then: 0.0,
            ask_then: None,
            ..s
        };
        assert_eq!(empty.fraction(), 0.0);
        assert!(!empty.ask_held());
    }
}
