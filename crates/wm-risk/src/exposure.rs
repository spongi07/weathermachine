//! Scenario-based exposure.
//!
//! Buckets of a daily temperature event are mutually exclusive and
//! exhaustive, so a portfolio's PnL is fully described by one scenario per
//! bucket. Exposure is the **worst-case loss** across scenarios — this is what
//! makes correlated positions (e.g. NO 19 + NO 20 + NO 21) count correctly:
//! at most one of them can lose, so their risk is not the sum of their costs.

use serde::{Deserialize, Serialize};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, TemperatureBucket};
use wm_core::units::{Shares, Usd};

/// One holding (or pending buy treated as filled) in an event.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub bucket: TemperatureBucket,
    pub side: OutcomeSide,
    pub shares: Shares,
    /// Total cost including fees.
    pub cost: Usd,
}

impl Leg {
    fn payout(&self, final_value: i32) -> Usd {
        let in_bucket = self.bucket.contains(final_value);
        let wins = match self.side {
            OutcomeSide::Yes => in_bucket,
            OutcomeSide::No => !in_bucket,
        };
        if wins {
            Usd::from_micros(self.shares.micros())
        } else {
            Usd::ZERO
        }
    }
}

/// PnL of `legs` if the event settles at `final_value`.
pub fn scenario_pnl(legs: &[Leg], final_value: i32) -> Usd {
    legs.iter().map(|l| l.payout(final_value) - l.cost).sum()
}

/// A value inside each bucket (one scenario per bucket).
pub fn representative_values(market: &DailyTemperatureMarket) -> Vec<i32> {
    market
        .outcomes
        .iter()
        .map(|o| match (o.bucket.lower, o.bucket.upper) {
            (Some(lo), _) => lo,
            (None, Some(hi)) => hi,
            (None, None) => 0,
        })
        .collect()
}

/// Exposure summary of one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventExposure {
    /// `max(0, −min scenario PnL)`.
    pub worst_case_loss: Usd,
    pub best_case_pnl: Usd,
    /// Capital committed (sum of costs).
    pub capital: Usd,
    pub scenarios: Vec<(String, Usd)>,
}

pub fn event_exposure(market: &DailyTemperatureMarket, legs: &[Leg]) -> EventExposure {
    let capital: Usd = legs.iter().map(|l| l.cost).sum();
    if legs.is_empty() {
        return EventExposure {
            worst_case_loss: Usd::ZERO,
            best_case_pnl: Usd::ZERO,
            capital,
            scenarios: Vec::new(),
        };
    }
    let mut scenarios = Vec::new();
    let mut worst = Usd::from_micros(i64::MAX);
    let mut best = Usd::from_micros(i64::MIN);
    for (o, v) in market.outcomes.iter().zip(representative_values(market)) {
        let pnl = scenario_pnl(legs, v);
        worst = worst.min(pnl);
        best = best.max(pnl);
        scenarios.push((o.label.clone(), pnl));
    }
    EventExposure {
        worst_case_loss: (-worst).max(Usd::ZERO),
        best_case_pnl: best,
        capital,
        scenarios,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, Utc};
    use wm_core::ids::{LocationId, StationId};
    use wm_core::market::TempUnit;
    use wm_core::synthetic::synthetic_temperature_market;

    fn market() -> DailyTemperatureMarket {
        synthetic_temperature_market(
            &LocationId::new("amsterdam").unwrap(),
            &StationId::new("EHAM").unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            chrono_tz::Europe::Amsterdam,
            13,
            24,
            Utc::now(),
        )
    }

    fn leg(v: i32, side: OutcomeSide, shares: i64, cost: &str) -> Leg {
        Leg {
            bucket: TemperatureBucket::exact(v, TempUnit::Celsius),
            side,
            shares: Shares::from_whole(shares),
            cost: Usd::parse(cost).unwrap(),
        }
    }

    #[test]
    fn single_yes_worst_case_is_its_cost() {
        let e = event_exposure(&market(), &[leg(18, OutcomeSide::Yes, 10, "9.50")]);
        assert_eq!(e.worst_case_loss, Usd::parse("9.50").unwrap());
        assert_eq!(e.best_case_pnl, Usd::parse("0.50").unwrap());
        assert_eq!(e.capital, Usd::parse("9.50").unwrap());
    }

    #[test]
    fn correlated_nos_are_not_additive() {
        // NO 19, NO 20, NO 21 at $9.30, $9.70, $9.85 (10 shares each). At most one loses.
        let legs = [
            leg(19, OutcomeSide::No, 10, "9.30"),
            leg(20, OutcomeSide::No, 10, "9.70"),
            leg(21, OutcomeSide::No, 10, "9.85"),
        ];
        let e = event_exposure(&market(), &legs);
        // Worst: final = 21 → NO21 loses 9.85, others win +0.70 and +0.30 ⇒ −8.85.
        assert_eq!(e.worst_case_loss, Usd::parse("8.85").unwrap());
        assert!(e.worst_case_loss < e.capital);
    }

    #[test]
    fn complete_set_has_no_price_risk() {
        // YES 18 + NO 18 (a CTF split) pays exactly 10 in every scenario.
        let legs = [
            leg(18, OutcomeSide::Yes, 10, "5.00"),
            leg(18, OutcomeSide::No, 10, "5.00"),
        ];
        let e = event_exposure(&market(), &legs);
        assert_eq!(e.worst_case_loss, Usd::ZERO);
        assert_eq!(e.capital, Usd::from_whole(10));
    }

    #[test]
    fn yes_and_no_on_adjacent_buckets_hedge_partially() {
        let legs = [
            leg(18, OutcomeSide::Yes, 10, "9.50"),
            leg(19, OutcomeSide::No, 10, "9.30"),
        ];
        let e = event_exposure(&market(), &legs);
        // final 19: YES18 loses 9.50, NO19 loses 9.30 ⇒ −18.80 (both lose).
        assert_eq!(e.worst_case_loss, Usd::parse("18.80").unwrap());
    }
}
