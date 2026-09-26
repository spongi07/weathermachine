//! Conditional Token Framework economics (pure; no on-chain code).
//!
//! KNOWN FACT (Polymarket source code, `Polymarket/ctf-exchange-v2`,
//! `Polymarket/neg-risk-ctf-adapter`):
//! * `split(amount)`: `amount` collateral → `amount` YES + `amount` NO of one
//!   binary market (complete set). Since 2026-04-28 collateral is pUSD (ERC-20,
//!   6 decimals, backed by USDC) — verify before Phase 14.
//! * `merge(amount)`: burns `amount` YES + `amount` NO → `amount` collateral.
//! * `redeem`: after resolution each winning token pays 1 collateral.
//! * Neg-risk `convertPositions(marketId, indexSet, amount)`: converting
//!   `amount` NO tokens of `m` of the `n` questions yields `amount × (m − 1)`
//!   collateral plus `amount` YES of each of the other `n − m` questions
//!   (minus a market fee parameter).
//! * The CLOB matches complementary orders: a BUY YES at `p` can match a BUY
//!   NO at `1 − p` via MINT (split), and two SELLs via MERGE. Hence in a
//!   consistent book `ask(YES) ≈ 1 − bid(NO)`, and "split then sell the
//!   unfavoured side" costs the same as buying the favoured side directly
//!   (plus the sale's taker fee and gas). See `wm_strategy::ev`.

use wm_core::units::{Shares, Usd};

/// Outcome tokens received for splitting `collateral`.
pub fn split(collateral: Usd) -> (Shares, Shares) {
    let s = Shares::from_micros(collateral.micros().max(0));
    (s, s)
}

/// Collateral returned by merging complete sets (limited by the smaller side).
pub fn merge(yes: Shares, no: Shares) -> Usd {
    Usd::from_micros(yes.min(no).micros().max(0))
}

/// Payout for redeeming `shares` of a token after resolution.
pub fn redeem(shares: Shares, won: bool) -> Usd {
    if won {
        Usd::from_micros(shares.micros().max(0))
    } else {
        Usd::ZERO
    }
}

/// Neg-risk conversion of `amount` NO tokens held on `m` of `n` questions.
/// Returns (collateral released, YES tokens per remaining question, remaining question count).
pub fn neg_risk_convert(
    n_questions: usize,
    m_converted: usize,
    amount: Shares,
    fee_bips: u32,
) -> Option<(Usd, Shares, usize)> {
    if m_converted == 0 || m_converted > n_questions || amount.micros() <= 0 {
        return None;
    }
    let gross = i128::from(amount.micros()) * (m_converted as i128 - 1);
    let fee = gross * i128::from(fee_bips) / 10_000;
    let collateral = Usd::from_micros(i64::try_from(gross - fee).ok()?);
    let yes_amount =
        Shares::from_micros(amount.micros() - amount.micros() * i64::from(fee_bips) / 10_000);
    Some((collateral, yes_amount, n_questions - m_converted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_merge_roundtrip() {
        let (y, n) = split(Usd::from_whole(10));
        assert_eq!(y, Shares::from_whole(10));
        assert_eq!(merge(y, n), Usd::from_whole(10));
        assert_eq!(
            merge(Shares::from_whole(3), Shares::from_whole(10)),
            Usd::from_whole(3)
        );
        assert_eq!(redeem(Shares::from_whole(7), true), Usd::from_whole(7));
        assert_eq!(redeem(Shares::from_whole(7), false), Usd::ZERO);
    }

    #[test]
    fn neg_risk_conversion() {
        // 12 buckets; convert 10 NO shares on 3 buckets → 20 collateral + 10 YES on 9 others.
        let (c, y, k) = neg_risk_convert(12, 3, Shares::from_whole(10), 0).unwrap();
        assert_eq!(c, Usd::from_whole(20));
        assert_eq!(y, Shares::from_whole(10));
        assert_eq!(k, 9);
        assert!(neg_risk_convert(12, 0, Shares::from_whole(1), 0).is_none());
        let (c, _, _) = neg_risk_convert(12, 3, Shares::from_whole(10), 100).unwrap();
        assert_eq!(c, Usd::parse("19.8").unwrap());
    }
}
