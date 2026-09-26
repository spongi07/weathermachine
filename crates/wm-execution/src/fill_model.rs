//! Pure fill simulation. Never assumes mid-price fills.

use serde::{Deserialize, Serialize};
use wm_core::market::{FeeSchedule, OrderBook, Side};
use wm_core::units::{Price, Rounding, Shares, Usd, notional};

/// Result of simulating an immediately-executable (taker) order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakerFill {
    /// (price, shares) per level consumed.
    pub legs: Vec<(Price, Shares)>,
    pub filled: Shares,
    pub unfilled: Shares,
    /// Volume-weighted average price, if anything filled.
    pub avg_price: Option<Price>,
    /// Total cost (buy) or gross proceeds (sell) before fees.
    pub gross: Usd,
    pub fee: Usd,
}

/// Walk the book for a taker order: buys consume asks `≤ limit`, sells consume
/// bids `≥ limit`, best price first. `all_or_none` implements FOK.
pub fn simulate_taker(
    book: &OrderBook,
    side: Side,
    limit: Price,
    shares: Shares,
    fees: &FeeSchedule,
    all_or_none: bool,
) -> TakerFill {
    let mut remaining = shares;
    let mut legs = Vec::new();
    let levels: Vec<_> = match side {
        Side::Buy => book
            .asks
            .iter()
            .filter(|l| l.price <= limit)
            .copied()
            .collect(),
        Side::Sell => book
            .bids
            .iter()
            .filter(|l| l.price >= limit)
            .copied()
            .collect(),
    };
    for l in levels {
        if remaining.micros() <= 0 {
            break;
        }
        let take = remaining.min(l.size);
        if take.micros() > 0 {
            legs.push((l.price, take));
            remaining -= take;
        }
    }
    let filled = shares - remaining;
    if all_or_none && remaining.micros() > 0 {
        return TakerFill {
            legs: Vec::new(),
            filled: Shares::ZERO,
            unfilled: shares,
            avg_price: None,
            gross: Usd::ZERO,
            fee: Usd::ZERO,
        };
    }
    let rounding = if side == Side::Buy {
        Rounding::Up
    } else {
        Rounding::Down
    };
    let gross: Usd = legs.iter().map(|(p, s)| notional(*p, *s, rounding)).sum();
    let fee: Usd = legs.iter().map(|(p, s)| fees.taker_fee(*p, *s)).sum();
    let avg_price = if filled.micros() > 0 {
        let num = i128::from(gross.micros()) * 1_000_000;
        let den = i128::from(filled.micros());
        u32::try_from(num / den)
            .ok()
            .and_then(|m| Price::from_micros(m).ok())
    } else {
        None
    };
    TakerFill {
        legs,
        filled,
        unfilled: remaining,
        avg_price,
        gross,
        fee,
    }
}

/// Conservative passive fill from a public trade print.
///
/// A resting order at `limit` with `queue_ahead` shares in front of it fills
/// only from volume that trades *through* its price (fully) or *at* its price
/// beyond the queue ahead. Returns (fill, new queue ahead).
pub fn passive_fill_from_trade(
    side: Side,
    limit: Price,
    remaining: Shares,
    queue_ahead: Shares,
    trade_price: Price,
    trade_size: Shares,
) -> (Shares, Shares) {
    let through = match side {
        Side::Buy => trade_price < limit,
        Side::Sell => trade_price > limit,
    };
    if through {
        return (remaining, Shares::ZERO);
    }
    if trade_price != limit {
        return (Shares::ZERO, queue_ahead);
    }
    if trade_size <= queue_ahead {
        return (Shares::ZERO, queue_ahead - trade_size);
    }
    let available = trade_size - queue_ahead;
    (available.min(remaining), Shares::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use proptest::prelude::*;
    use wm_core::ids::TokenId;
    use wm_core::market::BookLevel;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    fn book() -> OrderBook {
        let mut b = OrderBook {
            token: TokenId::new("t").unwrap(),
            bids: vec![
                BookLevel {
                    price: p("0.93"),
                    size: Shares::from_whole(10),
                },
                BookLevel {
                    price: p("0.92"),
                    size: Shares::from_whole(30),
                },
            ],
            asks: vec![
                BookLevel {
                    price: p("0.95"),
                    size: Shares::from_whole(6),
                },
                BookLevel {
                    price: p("0.96"),
                    size: Shares::from_whole(20),
                },
            ],
            tick_size: p("0.01"),
            min_order_size: Shares::from_whole(5),
            exchange_ts: None,
            received_at: Utc::now(),
            hash: None,
        };
        b.normalize();
        b
    }

    #[test]
    fn buy_walks_levels_within_limit() {
        let f = simulate_taker(
            &book(),
            Side::Buy,
            p("0.96"),
            Shares::from_whole(10),
            &FeeSchedule::ZERO,
            false,
        );
        assert_eq!(f.filled, Shares::from_whole(10));
        assert_eq!(
            f.legs,
            vec![
                (p("0.95"), Shares::from_whole(6)),
                (p("0.96"), Shares::from_whole(4))
            ]
        );
        assert_eq!(f.gross, Usd::parse("9.54").unwrap());
        assert_eq!(f.avg_price, Some(p("0.954")));
        // Limit below second level: partial fill (FAK), FOK fills nothing.
        let fak = simulate_taker(
            &book(),
            Side::Buy,
            p("0.95"),
            Shares::from_whole(10),
            &FeeSchedule::ZERO,
            false,
        );
        assert_eq!(fak.filled, Shares::from_whole(6));
        assert_eq!(fak.unfilled, Shares::from_whole(4));
        let fok = simulate_taker(
            &book(),
            Side::Buy,
            p("0.95"),
            Shares::from_whole(10),
            &FeeSchedule::ZERO,
            true,
        );
        assert_eq!(fok.filled, Shares::ZERO);
    }

    #[test]
    fn sell_hits_bids_and_pays_fees() {
        let f = simulate_taker(
            &book(),
            Side::Sell,
            p("0.92"),
            Shares::from_whole(15),
            &FeeSchedule::taker(50_000),
            false,
        );
        assert_eq!(f.filled, Shares::from_whole(15));
        assert_eq!(f.gross, Usd::parse("13.9").unwrap());
        assert!(f.fee > Usd::ZERO);
        let none = simulate_taker(
            &book(),
            Side::Sell,
            p("0.99"),
            Shares::from_whole(5),
            &FeeSchedule::ZERO,
            false,
        );
        assert_eq!(none.filled, Shares::ZERO);
        assert_eq!(none.avg_price, None);
    }

    #[test]
    fn passive_queue_model() {
        let (f, q) = passive_fill_from_trade(
            Side::Buy,
            p("0.94"),
            Shares::from_whole(10),
            Shares::from_whole(50),
            p("0.94"),
            Shares::from_whole(30),
        );
        assert_eq!((f, q), (Shares::ZERO, Shares::from_whole(20)));
        let (f, q) = passive_fill_from_trade(
            Side::Buy,
            p("0.94"),
            Shares::from_whole(10),
            Shares::from_whole(20),
            p("0.94"),
            Shares::from_whole(25),
        );
        assert_eq!((f, q), (Shares::from_whole(5), Shares::ZERO));
        let (f, _) = passive_fill_from_trade(
            Side::Buy,
            p("0.94"),
            Shares::from_whole(10),
            Shares::from_whole(1000),
            p("0.93"),
            Shares::from_whole(1),
        );
        assert_eq!(f, Shares::from_whole(10), "traded through our price");
        let (f, _) = passive_fill_from_trade(
            Side::Buy,
            p("0.94"),
            Shares::from_whole(10),
            Shares::ZERO,
            p("0.95"),
            Shares::from_whole(100),
        );
        assert_eq!(f, Shares::ZERO);
    }

    proptest! {
        #[test]
        fn fills_never_exceed_request_or_limit(req in 1i64..100, limit_c in 90u32..99, side_buy in any::<bool>()) {
            let side = if side_buy { Side::Buy } else { Side::Sell };
            let limit = Price::from_micros(limit_c * 10_000).unwrap();
            let f = simulate_taker(&book(), side, limit, Shares::from_whole(req), &FeeSchedule::taker(50_000), false);
            prop_assert!(f.filled <= Shares::from_whole(req));
            prop_assert_eq!(f.filled + f.unfilled, Shares::from_whole(req));
            for (price, _) in &f.legs {
                if side_buy { prop_assert!(*price <= limit); } else { prop_assert!(*price >= limit); }
            }
        }
    }
}
