//! Position accounting (pure). Positions are long-only outcome-token holdings.

use crate::ids::{ConditionId, EventSlug, TokenId};
use crate::market::{OutcomeSide, Side, TemperatureBucket};
use crate::trading::Fill;
use crate::units::{Rounding, Shares, Usd, notional};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Static description of the instrument a position is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentRef {
    pub token: TokenId,
    pub condition_id: ConditionId,
    pub event_slug: EventSlug,
    pub outcome_side: OutcomeSide,
    pub bucket: TemperatureBucket,
}

/// Holding in one outcome token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub instrument: InstrumentRef,
    pub shares: Shares,
    /// Total cost basis of the open shares, including fees paid.
    pub cost_basis: Usd,
    pub realized_pnl: Usd,
    pub fees_paid: Usd,
}

impl Position {
    /// Average cost per share (collateral units), or 0 for an empty position.
    pub fn avg_cost(&self) -> f64 {
        if self.shares.micros() == 0 {
            0.0
        } else {
            self.cost_basis.as_f64() / self.shares.as_f64()
        }
    }
}

/// Errors from applying fills.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortfolioError {
    #[error("sell of {requested} exceeds held {held} shares")]
    Oversell { requested: Shares, held: Shares },
    #[error("fill for unknown instrument {0}")]
    UnknownInstrument(TokenId),
}

/// All positions, keyed by token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionBook {
    positions: BTreeMap<TokenId, Position>,
}

impl PositionBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, token: &TokenId) -> Option<&Position> {
        self.positions.get(token)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Position> {
        self.positions.values()
    }

    pub fn open_positions(&self) -> impl Iterator<Item = &Position> {
        self.positions.values().filter(|p| p.shares.micros() > 0)
    }

    pub fn for_event<'a>(&'a self, slug: &'a EventSlug) -> impl Iterator<Item = &'a Position> {
        self.positions
            .values()
            .filter(move |p| &p.instrument.event_slug == slug && p.shares.micros() > 0)
    }

    pub fn total_cost_basis(&self) -> Usd {
        self.open_positions().map(|p| p.cost_basis).sum()
    }

    pub fn total_realized_pnl(&self) -> Usd {
        self.positions.values().map(|p| p.realized_pnl).sum()
    }

    /// Apply a fill. Buys add shares at cost (plus fee); sells reduce shares
    /// and realize PnL against the average cost (minus fee).
    pub fn apply_fill(
        &mut self,
        fill: &Fill,
        instrument: &InstrumentRef,
    ) -> Result<(), PortfolioError> {
        let entry = self
            .positions
            .entry(fill.token.clone())
            .or_insert_with(|| Position {
                instrument: instrument.clone(),
                shares: Shares::ZERO,
                cost_basis: Usd::ZERO,
                realized_pnl: Usd::ZERO,
                fees_paid: Usd::ZERO,
            });
        match fill.side {
            Side::Buy => {
                let cost = notional(fill.price, fill.shares, Rounding::Up) + fill.fee;
                entry.shares += fill.shares;
                entry.cost_basis += cost;
                entry.fees_paid += fill.fee;
            }
            Side::Sell => {
                if fill.shares > entry.shares {
                    return Err(PortfolioError::Oversell {
                        requested: fill.shares,
                        held: entry.shares,
                    });
                }
                let proceeds = notional(fill.price, fill.shares, Rounding::Down) - fill.fee;
                // Cost released proportionally to shares sold (rounded up: conservative PnL).
                let released = if entry.shares.micros() == 0 {
                    Usd::ZERO
                } else {
                    let num =
                        i128::from(entry.cost_basis.micros()) * i128::from(fill.shares.micros());
                    let den = i128::from(entry.shares.micros());
                    Usd::from_micros(((num + den - 1) / den) as i64)
                };
                entry.shares -= fill.shares;
                entry.cost_basis -= released;
                if entry.shares.micros() == 0 {
                    entry.cost_basis = Usd::ZERO;
                }
                entry.realized_pnl += proceeds - released;
                entry.fees_paid += fill.fee;
            }
        }
        Ok(())
    }

    /// Settle all positions of an event given the winning whole-degree value.
    /// Returns realized PnL of the settlement.
    pub fn settle_event(&mut self, slug: &EventSlug, final_value: i32) -> Usd {
        let mut pnl = Usd::ZERO;
        for p in self
            .positions
            .values_mut()
            .filter(|p| &p.instrument.event_slug == slug)
        {
            if p.shares.micros() == 0 {
                continue;
            }
            let in_bucket = p.instrument.bucket.contains(final_value);
            let wins = match p.instrument.outcome_side {
                OutcomeSide::Yes => in_bucket,
                OutcomeSide::No => !in_bucket,
            };
            let payout = if wins {
                Usd::from_micros(p.shares.micros())
            } else {
                Usd::ZERO
            };
            let realized = payout - p.cost_basis;
            p.realized_pnl += realized;
            pnl += realized;
            p.shares = Shares::ZERO;
            p.cost_basis = Usd::ZERO;
        }
        pnl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ClientOrderId;
    use crate::market::TempUnit;
    use crate::trading::Liquidity;
    use crate::units::Price;
    use chrono::Utc;

    fn instrument(side: OutcomeSide, v: i32) -> InstrumentRef {
        InstrumentRef {
            token: TokenId::new(format!("tok-{v}-{}", side.as_str())).unwrap(),
            condition_id: ConditionId::new(format!("0xc{v}")).unwrap(),
            event_slug: EventSlug::new("ams-2026-09-26").unwrap(),
            outcome_side: side,
            bucket: TemperatureBucket::exact(v, TempUnit::Celsius),
        }
    }

    fn fill(inst: &InstrumentRef, side: Side, price: &str, shares: i64, fee: &str) -> Fill {
        Fill {
            client_order_id: ClientOrderId::new("c1").unwrap(),
            token: inst.token.clone(),
            side,
            price: Price::parse(price).unwrap(),
            shares: Shares::from_whole(shares),
            fee: Usd::parse(fee).unwrap(),
            liquidity: Liquidity::Taker,
            ts: Utc::now(),
        }
    }

    #[test]
    fn buy_then_settle_win_and_loss() {
        let yes18 = instrument(OutcomeSide::Yes, 18);
        let no19 = instrument(OutcomeSide::No, 19);
        let mut book = PositionBook::new();
        book.apply_fill(&fill(&yes18, Side::Buy, "0.95", 10, "0.02375"), &yes18)
            .unwrap();
        book.apply_fill(&fill(&no19, Side::Buy, "0.90", 10, "0"), &no19)
            .unwrap();
        assert_eq!(
            book.get(&yes18.token).unwrap().cost_basis,
            Usd::parse("9.52375").unwrap()
        );
        let pnl = book.settle_event(&yes18.event_slug, 18);
        // YES 18 wins: +10 - 9.52375 ; NO 19 wins: +10 - 9.0
        assert_eq!(pnl, Usd::parse("1.47625").unwrap());
        assert_eq!(book.total_cost_basis(), Usd::ZERO);
    }

    #[test]
    fn settle_loss_when_high_breaks() {
        let yes18 = instrument(OutcomeSide::Yes, 18);
        let mut book = PositionBook::new();
        book.apply_fill(&fill(&yes18, Side::Buy, "0.95", 10, "0"), &yes18)
            .unwrap();
        let pnl = book.settle_event(&yes18.event_slug, 19);
        assert_eq!(pnl, Usd::parse("-9.5").unwrap());
    }

    #[test]
    fn partial_sell_realizes_pnl_and_rejects_oversell() {
        let yes18 = instrument(OutcomeSide::Yes, 18);
        let mut book = PositionBook::new();
        book.apply_fill(&fill(&yes18, Side::Buy, "0.90", 10, "0"), &yes18)
            .unwrap();
        book.apply_fill(&fill(&yes18, Side::Sell, "0.95", 4, "0"), &yes18)
            .unwrap();
        let p = book.get(&yes18.token).unwrap();
        assert_eq!(p.shares, Shares::from_whole(6));
        assert_eq!(p.realized_pnl, Usd::parse("0.2").unwrap());
        assert_eq!(p.cost_basis, Usd::parse("5.4").unwrap());
        let err = book
            .apply_fill(&fill(&yes18, Side::Sell, "0.95", 7, "0"), &yes18)
            .unwrap_err();
        assert!(matches!(err, PortfolioError::Oversell { .. }));
    }
}
