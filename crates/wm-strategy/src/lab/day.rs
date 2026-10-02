//! What every lab family reads off the context, and how it trades: taker
//! buys at the ask (fill-and-kill), resting NO bids (good-till-date) and
//! L3's sale of its YES at the bid.

use super::{LabConfig, LabInputs, WxReport};
use crate::ev::{break_even_probability, ev_per_share};
use crate::peak::PeakFeatures;
use crate::quoting::{next_routine_report, passive_bid};
use crate::strategy::{
    BucketEvaluation, Proposal, StrategyContext, StrategyOutput, common_high, holds_or_pending,
    max_data_age, size_for,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use wm_core::ids::{StrategyId, TokenId};
use wm_core::market::{MarketOutcome, OrderBook, OutcomeSide, Side, TakerTrade};
use wm_core::time::{local_day_bounds, local_minute_of_day};
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::{Price, Rounding, Shares, Usd, round_shares_to_lot, shares_for_notional};
use wm_core::weather::TenMinuteObservation;

/// `HH:MM` of a local minute of the day.
pub(crate) fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Tenths °C as `20.5 °C`.
pub(crate) fn c(tenths: i32) -> String {
    format!("{:.1} °C", f64::from(tenths) / 10.0)
}

/// `x` rounded half up to a whole number.
pub(crate) fn round_half_up(x: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let r = (x + 0.5).floor() as i32;
    r
}

/// The mean of `v` (0 when empty).
pub(crate) fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Whole shares for `notional` at `ask`, no more than the book offers at
/// that price, at least the market's minimum. `None` when that leaves
/// nothing to buy.
pub fn taker_size(book: &OrderBook, ask: Price, notional: Usd, min_size: Shares) -> Option<Shares> {
    let whole = Shares::from_whole(1);
    let wanted = round_shares_to_lot(
        shares_for_notional(notional, ask, Rounding::Down),
        whole,
        Rounding::Down,
    );
    let (depth, _) = book.ask_depth_up_to(ask);
    let shares = wanted.min(round_shares_to_lot(depth, whole, Rounding::Down));
    (shares.micros() > 0 && shares >= min_size).then_some(shares)
}

/// A taker buy of one outcome's token.
pub(crate) struct Buy<'x> {
    pub outcome: &'x MarketOutcome,
    pub side: OutcomeSide,
    /// The ask must lie in `[lo, hi]` (the token's own price).
    pub band: (f64, f64),
    /// `(p, min)`: the expected profit a share at the ask, after the taker
    /// fee and the slippage allowance, at win probability `p` must be at
    /// least `min`. `None`: the rule claims no probability.
    pub p_win: Option<(f64, f64)>,
    pub rationale: Vec<String>,
}

/// A resting NO bid on one outcome.
pub(crate) struct Quote<'x> {
    pub outcome: &'x MarketOutcome,
    /// The YES offered — one minus the NO bid — must lie in `[lo, hi]`.
    pub yes_band: (f64, f64),
    /// A fixed NO bid; `None`: one tick inside the NO book's spread.
    pub price: Option<Price>,
    /// When the order expires; `None` blocks it (the caller says why).
    pub expires_at: Option<DateTime<Utc>>,
    pub rationale: Vec<String>,
}

/// One taker trade in YES terms on one of today's buckets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flow<'x> {
    pub at: DateTime<Utc>,
    pub outcome: &'x MarketOutcome,
    pub yes_price: f64,
    pub taker_buys_yes: bool,
    pub shares: f64,
    pub taker: Option<&'x str>,
}

/// One evaluation's view of the day.
pub(crate) struct Day<'c, 'a> {
    pub ctx: &'c StrategyContext<'a>,
    pub cfg: &'c LabConfig,
    pub id: &'c StrategyId,
    pub lab: &'c LabInputs<'a>,
    /// The day's high (whole °C), as every view agrees.
    pub high: i32,
    /// The first view's features.
    pub f: &'c PeakFeatures,
}

impl<'c, 'a> Day<'c, 'a> {
    /// `None` without a high all views agree on.
    pub fn new(
        ctx: &'c StrategyContext<'a>,
        cfg: &'c LabConfig,
        id: &'c StrategyId,
    ) -> Option<Self> {
        let high = common_high(ctx.views).ok()?;
        let f = &ctx.views.first()?.assessment.features;
        Some(Self {
            ctx,
            cfg,
            id,
            lab: ctx.lab,
            high,
            f,
        })
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.ctx.now
    }

    /// Local minute of the day now.
    pub fn minute(&self) -> u16 {
        self.f.local_minute_now
    }

    pub fn minute_of(&self, t: DateTime<Utc>) -> u16 {
        local_minute_of_day(t, self.ctx.market.timezone)
    }

    pub fn date(&self) -> NaiveDate {
        self.ctx.market.local_date
    }

    /// End of the market's local day.
    pub fn day_end(&self) -> DateTime<Utc> {
        local_day_bounds(self.date(), self.ctx.market.timezone).1
    }

    /// The instant of a local minute of the market's day (`None` in a DST
    /// gap).
    pub fn local(&self, minute: u16) -> Option<DateTime<Utc>> {
        let t = self
            .date()
            .and_hms_opt(u32::from(minute / 60), u32::from(minute % 60), 0)?;
        self.ctx
            .market
            .timezone
            .from_local_datetime(&t)
            .earliest()
            .map(|x| x.with_timezone(&Utc))
    }

    /// `outside 13:00–20:00 (now 11:42)` unless `start ≤ now < end`.
    pub fn outside(&self, start: u16, end: u16) -> Option<String> {
        let m = self.minute();
        (!(start..end).contains(&m))
            .then(|| format!("outside {}–{} (now {})", hm(start), hm(end), hm(m)))
    }

    /// The high's rounding edge in tenths: above it the next report reads
    /// the next degree.
    pub fn edge(&self) -> i32 {
        self.high * 10 + 5
    }

    // -- Buckets ----------------------------------------------------------

    pub fn outcome_of(&self, v: i32) -> Option<&'c MarketOutcome> {
        self.ctx.market.outcome_for_value(v)
    }

    /// The high's bucket when it does not also hold the next degree (a new
    /// high kills it).
    pub fn high_outcome(&self) -> Option<&'c MarketOutcome> {
        self.outcome_of(self.high)
            .filter(|o| !o.bucket.contains(self.high + 1))
    }

    /// The next degree's bucket when it is not the high's.
    pub fn next_outcome(&self) -> Option<&'c MarketOutcome> {
        self.outcome_of(self.high + 1)
            .filter(|o| !o.bucket.contains(self.high))
    }

    /// Today's buckets from the coldest up.
    pub fn ordered(&self) -> Vec<&'c MarketOutcome> {
        let mut v: Vec<&MarketOutcome> = self.ctx.market.outcomes.iter().collect();
        v.sort_by_key(|o| o.bucket.sort_key());
        v
    }

    /// A YES price of a bucket from its fresh book: the midpoint, else the
    /// ask, else the bid.
    pub fn yes_price(&self, o: &MarketOutcome) -> Option<f64> {
        let b = self
            .ctx
            .books
            .get(&o.yes_token)
            .filter(|b| b.age_ms(self.now()) <= self.cfg.max_book_age_ms)?;
        match (b.best_bid(), b.best_ask()) {
            (Some(bid), Some(ask)) if ask.price >= bid.price => {
                Some((bid.price.as_f64() + ask.price.as_f64()) / 2.0)
            }
            (_, Some(ask)) => Some(ask.price.as_f64()),
            (Some(bid), None) => Some(bid.price.as_f64()),
            (None, None) => None,
        }
    }

    /// The bucket the market favours: the highest YES price.
    pub fn favourite(&self) -> Option<(&'c MarketOutcome, f64)> {
        self.ctx
            .market
            .outcomes
            .iter()
            .filter_map(|o| self.yes_price(o).map(|p| (o, p)))
            .fold(
                None,
                |best: Option<(&MarketOutcome, f64)>, (o, p)| match best {
                    Some((_, b)) if b >= p => best,
                    _ => Some((o, p)),
                },
            )
    }

    /// Whether this strategy's own book already traded today's market (a
    /// position, open or closed, or an order still live): the rules that
    /// take one trade a day.
    pub fn traded_today(&self) -> bool {
        let slug = &self.ctx.market.event_slug;
        self.ctx
            .positions
            .iter()
            .any(|p| &p.instrument.event_slug == slug)
            || self.ctx.market.outcomes.iter().any(|o| {
                self.ctx.pending_tokens.contains(&o.yes_token)
                    || self.ctx.pending_tokens.contains(&o.no_token)
            })
    }

    // -- KNMI -------------------------------------------------------------

    /// The station's readings up to now, oldest first.
    pub fn readings(&self) -> &'a [TenMinuteObservation] {
        let k = self.lab.knmi;
        &k[..k.partition_point(|r| r.interval_end <= self.now())]
    }

    /// The latest reading, if fresh enough to act on.
    pub fn latest_reading(&self) -> Result<&'a TenMinuteObservation, String> {
        let Some(r) = self.readings().last() else {
            return Err("no KNMI ten-minute reading".into());
        };
        let age = self.now() - r.interval_end;
        if age > Duration::minutes(self.cfg.max_reading_age_minutes) {
            return Err(format!(
                "latest KNMI reading {} min old > {}",
                age.num_minutes(),
                self.cfg.max_reading_age_minutes
            ));
        }
        Ok(r)
    }

    pub fn reading_at(&self, t: DateTime<Utc>) -> Option<&'a TenMinuteObservation> {
        let rs = self.readings();
        rs.binary_search_by_key(&t, |r| r.interval_end)
            .ok()
            .map(|i| &rs[i])
    }

    /// The mean (tenths) of the reading whose interval ends at `t`.
    pub fn mean_at(&self, t: DateTime<Utc>) -> Option<i32> {
        self.reading_at(t)?.mean.map(|m| m.tenths())
    }

    /// Readings whose intervals end in `(end − minutes, end]`.
    pub fn window(&self, end: DateTime<Utc>, minutes: i64) -> &'a [TenMinuteObservation] {
        let rs = self.readings();
        let lo = rs.partition_point(|r| r.interval_end <= end - Duration::minutes(minutes));
        let hi = rs.partition_point(|r| r.interval_end <= end);
        &rs[lo..hi.max(lo)]
    }

    /// `KNMI 11:10–11:20 UTC mean 19.6 °C max 19.8 °C`.
    pub fn describe(r: &TenMinuteObservation) -> String {
        format!(
            "KNMI {}–{} UTC mean {} max {}",
            (r.interval_end - Duration::minutes(10)).format("%H:%M"),
            r.interval_end.format("%H:%M"),
            r.mean.map_or_else(|| "—".to_owned(), |t| c(t.tenths())),
            r.max.map_or_else(|| "—".to_owned(), |t| c(t.tenths()))
        )
    }

    /// The checks K makes of a reading that anticipates the next METAR:
    /// newer than the last one and at most 16 minutes before the routine
    /// report after it. `Ok(next report)`.
    pub fn ahead_of_metar(
        &self,
        r: &TenMinuteObservation,
        blockers: &mut Vec<String>,
    ) -> Option<DateTime<Utc>> {
        if r.interval_end <= self.last_metar_at() {
            blockers.push("KNMI reading not newer than the last METAR".into());
        }
        match next_routine_report(r.interval_end, self.ctx.routine_minutes) {
            None => {
                blockers.push("report schedule unknown".into());
                None
            }
            Some(n) => {
                let lead = n - r.interval_end;
                if lead > Duration::minutes(16) {
                    blockers.push(format!(
                        "reading {} min before the next report > 16",
                        lead.num_minutes()
                    ));
                }
                Some(n)
            }
        }
    }

    // -- METAR ------------------------------------------------------------

    /// Today's reports up to now, oldest first.
    pub fn reports(&self) -> &'a [WxReport] {
        let r = self.lab.reports;
        &r[..r.partition_point(|x| x.observed_at <= self.now())]
    }

    pub fn latest_report(&self) -> Option<&'a WxReport> {
        self.reports().last()
    }

    pub fn report_observed(&self, at: DateTime<Utc>) -> Option<&'a WxReport> {
        self.reports().iter().rev().find(|r| r.observed_at == at)
    }

    /// When the latest METAR was observed (from the reports, else the
    /// views' data age).
    pub fn last_metar_at(&self) -> DateTime<Utc> {
        self.latest_report().map_or_else(
            || self.now() - Duration::minutes(self.f.data_age_minutes.clamp(0, 24 * 60)),
            |r| r.observed_at,
        )
    }

    /// The season's median local time of first reaching the high (14:00
    /// without history).
    pub fn season_median(&self) -> u16 {
        self.ctx
            .peak_times
            .and_then(|p| p.season(self.f.season))
            .and_then(|s| s.quantile(0.5))
            .unwrap_or(14 * 60)
    }

    // -- Taker trades -----------------------------------------------------

    /// Today's taker trades up to now in YES terms, oldest first.
    pub fn flow(&self) -> Vec<Flow<'c>> {
        let lab: &'c LabInputs<'a> = self.lab;
        let market = self.ctx.market;
        lab.takers
            .iter()
            .filter(|t| t.at <= self.now())
            .filter_map(|t: &'c TakerTrade| {
                let (outcome, yes) = market.outcomes.iter().find_map(|o| {
                    if o.yes_token == t.token {
                        Some((o, true))
                    } else if o.no_token == t.token {
                        Some((o, false))
                    } else {
                        None
                    }
                })?;
                Some(Flow {
                    at: t.at,
                    outcome,
                    yes_price: if yes { t.price } else { 1.0 - t.price },
                    taker_buys_yes: yes == (t.side == Side::Buy),
                    shares: t.size,
                    taker: t.taker.as_deref(),
                })
            })
            .collect()
    }

    // -- Orders -----------------------------------------------------------

    fn token(o: &MarketOutcome, side: OutcomeSide) -> &TokenId {
        match side {
            OutcomeSide::Yes => &o.yes_token,
            OutcomeSide::No => &o.no_token,
        }
    }

    /// An evaluation that cannot trade (no rule target to price, or a
    /// missing input), shown on `outcome`'s `side`.
    pub fn note(
        &self,
        outcome: &MarketOutcome,
        side: OutcomeSide,
        blockers: Vec<String>,
        out: &mut StrategyOutput,
    ) {
        let token = Self::token(outcome, side);
        let book = self.ctx.books.get(token);
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: side,
            token: token.clone(),
            ask: book.and_then(OrderBook::best_ask).map(|l| l.price),
            bid: book.and_then(OrderBook::best_bid).map(|l| l.price),
            p_win: None,
            ev_per_share: None,
            break_even: None,
            signal: false,
            blockers,
            model_p: None,
            market_p: None,
        });
    }

    fn common_blockers(
        &self,
        outcome: &MarketOutcome,
        token: &TokenId,
        blockers: &mut Vec<String>,
    ) {
        if max_data_age(self.ctx.views) > self.cfg.max_data_age_minutes {
            blockers.push("weather data too old".into());
        }
        if holds_or_pending(self.ctx, token) {
            blockers.push("already positioned".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
    }

    /// A taker buy at the ask, fill-and-kill.
    pub fn taker(&self, b: Buy<'_>, mut blockers: Vec<String>, out: &mut StrategyOutput) {
        let ctx = self.ctx;
        let token = Self::token(b.outcome, b.side);
        let side = b.side.as_str();
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        match (book, ask) {
            (None, _) => blockers.push(format!("no {side} book")),
            (Some(_), None) => blockers.push(format!("no {side} ask")),
            (Some(bk), Some(a)) => {
                if bk.age_ms(ctx.now) > self.cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
                let p = a.as_f64();
                if p < b.band.0 - 1e-9 || p > b.band.1 + 1e-9 {
                    blockers.push(format!(
                        "{side} ask {a} outside [{:.2}, {:.2}]",
                        b.band.0, b.band.1
                    ));
                }
            }
        }
        let fees = ctx.market.fees;
        let slip = self.cfg.slippage_allowance;
        let ev = match (b.p_win, ask) {
            (Some((p, _)), Some(a)) => Some(ev_per_share(p, a, &fees, slip)),
            _ => None,
        };
        if let (Some((p, min)), Some(e)) = (b.p_win, ev)
            && e < min
        {
            blockers.push(format!("edge {e:.4} < {min:.4} at p {p:.2}"));
        }
        let shares = book
            .zip(ask)
            .and_then(|(bk, a)| taker_size(bk, a, self.cfg.notional, b.outcome.min_order_size));
        if ask.is_some() && shares.is_none() {
            blockers.push("size below the market minimum (stake or depth at the ask)".into());
        }
        self.common_blockers(b.outcome, token, &mut blockers);
        let be = ask.map(|a| break_even_probability(a, &fees, slip));
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: b.outcome.label.clone(),
            outcome_side: b.side,
            token: token.clone(),
            ask,
            bid,
            p_win: b.p_win.map(|x| x.0),
            ev_per_share: ev,
            break_even: be,
            signal,
            blockers,
            model_p: None,
            market_p: None,
        });
        if signal && let (Some(a), Some(sh)) = (ask, shares) {
            let mut rationale = b.rationale;
            rationale.push(match b.p_win {
                Some((p, _)) => format!(
                    "{side} {} at {a}: p {p:.2} (the rule's) vs break-even {:.3}",
                    b.outcome.label,
                    be.unwrap_or(0.0)
                ),
                None => format!(
                    "{side} {} at {a}, inside [{:.2}, {:.2}] (no probability claimed)",
                    b.outcome.label, b.band.0, b.band.1
                ),
            });
            out.proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: b.outcome.label.clone(),
                bucket: b.outcome.bucket,
                condition_id: b.outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: b.side,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: a,
                shares: sh,
                tif: TimeInForce::Fak,
                p_win: b.p_win.map_or(a.as_f64(), |x| x.0),
                ev_per_share: ev.unwrap_or(0.0),
                break_even: be.unwrap_or(a.as_f64()),
                research_only: false,
                rationale,
            });
        }
    }

    /// A resting NO bid (good-till-date), held to settlement once filled.
    pub fn maker_no(&self, q: Quote<'_>, mut blockers: Vec<String>, out: &mut StrategyOutput) {
        let ctx = self.ctx;
        let token = &q.outcome.no_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        let price = q.price.or_else(|| book.and_then(passive_bid));
        match book {
            None => blockers.push("no NO book".into()),
            Some(bk) => {
                if bk.age_ms(ctx.now) > self.cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
            }
        }
        match price {
            None => blockers.push("no NO bid to join (one-sided or crossed book)".into()),
            Some(p) => {
                let offered = 1.0 - p.as_f64();
                if offered < q.yes_band.0 - 1e-9 || offered > q.yes_band.1 + 1e-9 {
                    blockers.push(format!(
                        "YES offered at {offered:.3} outside [{:.2}, {:.2}]",
                        q.yes_band.0, q.yes_band.1
                    ));
                }
                if ask.is_some_and(|a| p >= a) {
                    blockers.push(format!(
                        "NO bid {p} would take the ask {} (not a resting order)",
                        ask.unwrap_or(p)
                    ));
                }
            }
        }
        match q.expires_at {
            None => {}
            Some(e) if e - ctx.now < Duration::minutes(1) => {
                blockers.push(format!(
                    "would rest less than a minute (until {} UTC)",
                    e.format("%H:%M")
                ));
            }
            Some(_) => {}
        }
        if q.expires_at.is_none() && blockers.is_empty() {
            blockers.push("no time to rest before the next report".into());
        }
        let shares = price.and_then(|p| size_for(self.cfg.notional, p, q.outcome.min_order_size));
        if price.is_some() && shares.is_none() {
            blockers.push("size below the market minimum".into());
        }
        self.common_blockers(q.outcome, token, &mut blockers);
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: q.outcome.label.clone(),
            outcome_side: OutcomeSide::No,
            token: token.clone(),
            ask,
            bid,
            p_win: None,
            ev_per_share: None,
            break_even: price.map(Price::as_f64),
            signal,
            blockers,
            model_p: None,
            market_p: None,
        });
        if signal && let (Some(p), Some(sh), Some(e)) = (price, shares, q.expires_at) {
            let mut rationale = q.rationale;
            rationale.push(format!(
                "NO bid {p} on {} (YES offered at {:.3}) until {} UTC; a maker pays no fee",
                q.outcome.label,
                1.0 - p.as_f64(),
                e.format("%H:%M")
            ));
            out.proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: q.outcome.label.clone(),
                bucket: q.outcome.bucket,
                condition_id: q.outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::No,
                side: Side::Buy,
                kind: IntentKind::Open,
                weather_dependent: true,
                limit_price: p,
                shares: sh,
                tif: TimeInForce::Gtd { expires_at: e },
                p_win: p.as_f64(),
                ev_per_share: 0.0,
                break_even: p.as_f64(),
                research_only: false,
                rationale,
            });
        }
    }

    /// Sell `held` YES shares at the bid, fill-and-kill (L3's exit).
    pub fn sell_yes(
        &self,
        outcome: &MarketOutcome,
        held: Shares,
        rationale: Vec<String>,
        mut blockers: Vec<String>,
        out: &mut StrategyOutput,
    ) {
        let ctx = self.ctx;
        let token = &outcome.yes_token;
        let book = ctx.books.get(token);
        let ask = book.and_then(OrderBook::best_ask).map(|l| l.price);
        let bid = book.and_then(OrderBook::best_bid).map(|l| l.price);
        match (book, bid) {
            (None, _) => blockers.push("no YES book".into()),
            (Some(_), None) => blockers.push("no YES bid to sell into".into()),
            (Some(bk), Some(_)) => {
                if bk.age_ms(ctx.now) > self.cfg.max_book_age_ms {
                    blockers.push("order book stale".into());
                }
            }
        }
        if ctx.pending_tokens.contains(token) {
            blockers.push("an order on this YES is still live".into());
        }
        if !outcome.accepting_orders || outcome.closed {
            blockers.push("market not accepting orders".into());
        }
        let whole = round_shares_to_lot(held, Shares::from_whole(1), Rounding::Down);
        if whole.micros() == 0 {
            blockers.push("nothing to sell".into());
        }
        let signal = blockers.is_empty();
        out.evaluations.push(BucketEvaluation {
            strategy: self.id.clone(),
            bucket_label: outcome.label.clone(),
            outcome_side: OutcomeSide::Yes,
            token: token.clone(),
            ask,
            bid,
            p_win: None,
            ev_per_share: None,
            break_even: None,
            signal,
            blockers,
            model_p: None,
            market_p: None,
        });
        if signal && let Some(b) = bid {
            let mut rationale = rationale;
            rationale.push(format!("sell {whole} YES {} at the bid {b}", outcome.label));
            out.proposals.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: outcome.label.clone(),
                bucket: outcome.bucket,
                condition_id: outcome.condition_id.clone(),
                token: token.clone(),
                outcome_side: OutcomeSide::Yes,
                side: Side::Sell,
                kind: IntentKind::Reduce,
                weather_dependent: false,
                limit_price: b,
                shares: whole,
                tif: TimeInForce::Fak,
                p_win: 0.0,
                ev_per_share: 0.0,
                break_even: b.as_f64(),
                research_only: false,
                rationale,
            });
        }
    }
}
