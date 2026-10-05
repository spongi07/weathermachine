//! Helpers shared by the strategies that rest orders (G, J) or time their
//! trades by the station's report schedule (K).
//!
//! Resting orders are good-till-date: they expire `cancel_before` ahead of
//! the next routine report, because informed takers pick off quotes in the
//! minutes before a report (in the 122-day study makers on the high's bucket
//! lost 1.34¢ a share in the last five minutes before one). A strategy posts
//! again after the report, on its next evaluation.

use chrono::{DateTime, Duration, Timelike, Utc};
use wm_core::market::{FeeSchedule, OrderBook};
use wm_core::units::Price;

/// The first routine report strictly after `t`: `minutes` past each UTC
/// hour. `None` without a schedule.
pub fn next_routine_report(t: DateTime<Utc>, minutes: &[u8]) -> Option<DateTime<Utc>> {
    let hour = t
        .with_minute(0)
        .and_then(|h| h.with_second(0))
        .and_then(|h| h.with_nanosecond(0))?;
    (0..=1)
        .flat_map(|h| {
            minutes
                .iter()
                .map(move |m| hour + Duration::hours(h) + Duration::minutes(i64::from(*m)))
        })
        .filter(|x| *x > t)
        .min()
}

/// Expiry of an order resting from `now`: `cancel_before` ahead of the
/// next routine report. `None` without a schedule, or when less than
/// `min_rest` would remain before it.
pub fn quote_expiry(
    now: DateTime<Utc>,
    minutes: &[u8],
    cancel_before: Duration,
    min_rest: Duration,
) -> Option<DateTime<Utc>> {
    let expiry = next_routine_report(now, minutes)? - cancel_before;
    (expiry - now >= min_rest).then_some(expiry)
}

/// The book's tick (0.01 when it does not say).
pub fn tick_of(book: &OrderBook) -> Price {
    if book.tick_size.micros() == 0 {
        Price::saturating_from_micros(10_000)
    } else {
        book.tick_size
    }
}

/// A passive bid: one tick above the best bid when the spread leaves room
/// (the order then leads the queue), else at the best bid. Always on the
/// tick grid and below the best ask. `None` without both sides or with a
/// crossed or locked book.
pub fn passive_bid(book: &OrderBook) -> Option<Price> {
    let bid = book.best_bid()?.price;
    let ask = book.best_ask()?.price;
    if ask <= bid {
        return None;
    }
    let tick = tick_of(book);
    let improved = bid.saturating_add(tick).floor_to_tick(tick);
    if improved < ask {
        return Some(improved);
    }
    // Join the bid, or just under it when it lies off the book's tick.
    let join = bid.floor_to_tick(tick);
    (join.micros() > 0).then_some(join)
}

/// EV per share of a resting buy filled at `price`: no taker fee and no
/// slippage, the maker fee (usually 0) paid. The rebate is left out.
pub fn maker_ev(p_win: f64, price: Price, fees: &FeeSchedule) -> f64 {
    let maker = FeeSchedule {
        taker_rate_micros: fees.maker_rate_micros,
        maker_rate_micros: fees.maker_rate_micros,
    };
    crate::ev::ev_per_share(p_win, price, &maker, Price::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::ids::TokenId;
    use wm_core::synthetic::synthetic_book;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_next_report_and_the_quote_expiry_follow_the_schedule() {
        let m = [25, 55];
        assert_eq!(
            next_routine_report(utc("2026-07-01T10:24:59Z"), &m),
            Some(utc("2026-07-01T10:25:00Z"))
        );
        assert_eq!(
            next_routine_report(utc("2026-07-01T10:25:00Z"), &m),
            Some(utc("2026-07-01T10:55:00Z")),
            "strictly after"
        );
        assert_eq!(
            next_routine_report(utc("2026-07-01T23:56:00Z"), &m),
            Some(utc("2026-07-02T00:25:00Z"))
        );
        assert_eq!(next_routine_report(utc("2026-07-01T10:00:00Z"), &[]), None);
        let ten = Duration::minutes(10);
        let three = Duration::minutes(3);
        // Posted at :28 → expires at :45.
        assert_eq!(
            quote_expiry(utc("2026-07-01T10:28:00Z"), &m, ten, three),
            Some(utc("2026-07-01T10:45:00Z"))
        );
        // At :43 only two minutes would remain: no quote.
        assert_eq!(
            quote_expiry(utc("2026-07-01T10:43:00Z"), &m, ten, three),
            None
        );
        // Inside the last ten minutes the expiry has passed: no quote.
        assert_eq!(
            quote_expiry(utc("2026-07-01T10:50:00Z"), &m, ten, three),
            None
        );
    }

    #[test]
    fn a_passive_bid_leads_the_queue_when_the_spread_allows() {
        let t = TokenId::new("t").unwrap();
        let now = utc("2026-07-01T10:30:00Z");
        let p = |s: &str| Price::parse(s).unwrap();
        let wide = synthetic_book(&t, Some("0.40"), Some("0.45"), 100, now);
        assert_eq!(passive_bid(&wide), Some(p("0.41")));
        let one_tick = synthetic_book(&t, Some("0.40"), Some("0.41"), 100, now);
        assert_eq!(passive_bid(&one_tick), Some(p("0.40")), "join the bid");
        let one_sided = synthetic_book(&t, Some("0.40"), None, 100, now);
        assert_eq!(passive_bid(&one_sided), None);
        // A bid off the book's tick (0.975 on 0.01) is never quoted as is:
        // the risk check refuses a price off the tick.
        let off_tick = synthetic_book(&t, Some("0.975"), Some("0.98"), 100, now);
        assert_eq!(off_tick.tick_size, p("0.01"));
        assert_eq!(passive_bid(&off_tick), Some(p("0.97")));
        let fine = OrderBook {
            tick_size: p("0.001"),
            ..off_tick
        };
        assert_eq!(passive_bid(&fine), Some(p("0.976")), "one tick inside");
    }
}
