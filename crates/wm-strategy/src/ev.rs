//! Expected value and break-even math (blueprint §21).
//!
//! For a share bought at price `P` that pays 1 if it wins:
//!
//! ```text
//! EV = p_win·(1 − P) − (1 − p_win)·P − fee − slippage = p_win − P − fee − slippage
//! break-even p* = P + fee + slippage
//! ```
//!
//! Contracts near 0.99 need > 99 % accuracy: one loss erases ~99 wins.

use serde::{Deserialize, Serialize};
use wm_core::market::FeeSchedule;
use wm_core::units::Price;

/// EV per share (collateral units) of buying at `price` with win probability `p_win`.
pub fn ev_per_share(p_win: f64, price: Price, fees: &FeeSchedule, slippage: Price) -> f64 {
    p_win - price.as_f64() - fees.taker_fee_per_share(price) - slippage.as_f64()
}

/// Minimum win probability for non-negative EV.
pub fn break_even_probability(price: Price, fees: &FeeSchedule, slippage: Price) -> f64 {
    (price.as_f64() + fees.taker_fee_per_share(price) + slippage.as_f64()).min(1.0)
}

/// Number of winning trades needed to recover one loss at `price` (after fees).
pub fn wins_to_recover_one_loss(price: Price, fees: &FeeSchedule) -> f64 {
    let fee = fees.taker_fee_per_share(price);
    let win = 1.0 - price.as_f64() - fee;
    let loss = price.as_f64() + fee;
    if win <= 0.0 { f64::INFINITY } else { loss / win }
}

/// One row of the break-even table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakEvenRow {
    pub price: Price,
    pub fee_per_share: f64,
    pub break_even_probability: f64,
    pub wins_to_recover_one_loss: f64,
}

/// Break-even table for the research price grid (0.90 … 0.99).
pub fn break_even_table(prices: &[Price], fees: &FeeSchedule, slippage: Price) -> Vec<BreakEvenRow> {
    prices
        .iter()
        .map(|p| BreakEvenRow {
            price: *p,
            fee_per_share: fees.taker_fee_per_share(*p),
            break_even_probability: break_even_probability(*p, fees, slippage),
            wins_to_recover_one_loss: wins_to_recover_one_loss(*p, fees),
        })
        .collect()
}

/// Research price grid 0.90, 0.91, …, 0.99.
pub fn research_price_grid() -> Vec<Price> {
    (90..=99).filter_map(|c| Price::from_micros(c * 10_000).ok()).collect()
}

/// Effective entry price of a directional YES position acquired by splitting
/// one unit of collateral into YES+NO (cost 1.0) and selling the NO side at
/// `no_bid` (taker fee on the sale). Pure economics — see blueprint §18.
pub fn split_then_sell_effective_price(no_bid: Price, fees: &FeeSchedule, gas_per_share: f64) -> f64 {
    1.0 - no_bid.as_f64() + fees.taker_fee_per_share(no_bid) + gas_per_share
}

/// Effective entry price of buying YES directly at `yes_ask`.
pub fn direct_buy_effective_price(yes_ask: Price, fees: &FeeSchedule) -> f64 {
    yes_ask.as_f64() + fees.taker_fee_per_share(yes_ask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    #[test]
    fn ev_and_break_even_without_fees() {
        let f = FeeSchedule::ZERO;
        assert!((ev_per_share(0.97, p("0.95"), &f, Price::ZERO) - 0.02).abs() < 1e-12);
        assert!((break_even_probability(p("0.95"), &f, Price::ZERO) - 0.95).abs() < 1e-12);
        assert!((wins_to_recover_one_loss(p("0.99"), &f) - 99.0).abs() < 1e-9);
    }

    #[test]
    fn weather_fee_raises_break_even() {
        let f = FeeSchedule::taker(50_000);
        // 0.95 + 0.05·0.95·0.05 = 0.952375
        assert!((break_even_probability(p("0.95"), &f, Price::ZERO) - 0.952_375).abs() < 1e-12);
        // 0.99 + 0.05·0.99·0.01 = 0.990495
        assert!((break_even_probability(p("0.99"), &f, Price::ZERO) - 0.990_495).abs() < 1e-12);
        let table = break_even_table(&research_price_grid(), &f, p("0.005"));
        assert_eq!(table.len(), 10);
        assert!(table.windows(2).all(|w| w[0].break_even_probability < w[1].break_even_probability));
        assert!(table.last().unwrap().wins_to_recover_one_loss > 100.0);
    }

    #[test]
    fn split_is_equivalent_to_direct_buy_in_a_consistent_book() {
        // In a unified CLOB, YES ask ≈ 1 − NO bid. With zero fees the two entry
        // routes cost the same; fees and gas make the split route worse unless
        // the books are inconsistent.
        let f = FeeSchedule::ZERO;
        let direct = direct_buy_effective_price(p("0.95"), &f);
        let split = split_then_sell_effective_price(p("0.05"), &f, 0.0);
        assert!((direct - split).abs() < 1e-12);
        let f = FeeSchedule::taker(50_000);
        assert!(split_then_sell_effective_price(p("0.05"), &f, 0.001) > direct_buy_effective_price(p("0.95"), &f) - 1e-12);
    }
}
