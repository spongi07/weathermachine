//! Prediction-market domain types: temperature buckets, outcomes, order books, fees.

use crate::ids::{ConditionId, EventSlug, LocationId, QuestionId, StationId, TokenId};
use crate::resolution::{ResolutionSpec, RulesText};
use crate::units::{MICROS_PER_UNIT, Price, Rounding, Shares, Usd, notional};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Temperature unit a market settles in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempUnit {
    Celsius,
    Fahrenheit,
}

impl TempUnit {
    pub fn symbol(self) -> &'static str {
        match self {
            TempUnit::Celsius => "°C",
            TempUnit::Fahrenheit => "°F",
        }
    }
}

/// Which daily extreme a market settles on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketExtreme {
    DailyMax,
    DailyMin,
}

/// Inclusive whole-degree interval; `None` bounds are open-ended.
/// `18°C` = `[18, 18]`, `13°C or below` = `(-∞, 13]`, `86-87°F` = `[86, 87]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TemperatureBucket {
    pub lower: Option<i32>,
    pub upper: Option<i32>,
    pub unit: TempUnit,
}

impl TemperatureBucket {
    pub fn exact(v: i32, unit: TempUnit) -> Self {
        Self {
            lower: Some(v),
            upper: Some(v),
            unit,
        }
    }

    pub fn at_or_below(v: i32, unit: TempUnit) -> Self {
        Self {
            lower: None,
            upper: Some(v),
            unit,
        }
    }

    pub fn at_or_above(v: i32, unit: TempUnit) -> Self {
        Self {
            lower: Some(v),
            upper: None,
            unit,
        }
    }

    pub fn range(lo: i32, hi: i32, unit: TempUnit) -> Self {
        Self {
            lower: Some(lo.min(hi)),
            upper: Some(lo.max(hi)),
            unit,
        }
    }

    pub fn contains(&self, whole: i32) -> bool {
        self.lower.is_none_or(|lo| whole >= lo) && self.upper.is_none_or(|hi| whole <= hi)
    }

    /// Sort key: buckets are ordered by their lower bound (open lower first).
    pub fn sort_key(&self) -> i64 {
        match (self.lower, self.upper) {
            (Some(lo), _) => i64::from(lo),
            (None, Some(hi)) => i64::from(hi) - 1_000_000,
            (None, None) => i64::MIN,
        }
    }

    pub fn label(&self) -> String {
        let u = self.unit.symbol();
        match (self.lower, self.upper) {
            (Some(lo), Some(hi)) if lo == hi => format!("{lo}{u}"),
            (Some(lo), Some(hi)) => format!("{lo}-{hi}{u}"),
            (None, Some(hi)) => format!("≤{hi}{u}"),
            (Some(lo), None) => format!("≥{lo}{u}"),
            (None, None) => "any".to_owned(),
        }
    }
}

impl fmt::Display for TemperatureBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// YES or NO side of a binary outcome market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OutcomeSide {
    Yes,
    No,
}

impl OutcomeSide {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeSide::Yes => "YES",
            OutcomeSide::No => "NO",
        }
    }
}

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

/// One bucket of a daily temperature event (a binary YES/NO market).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketOutcome {
    pub condition_id: ConditionId,
    pub question_id: Option<QuestionId>,
    pub market_slug: Option<String>,
    /// Label as published (e.g. `18°C`).
    pub label: String,
    pub bucket: TemperatureBucket,
    pub yes_token: TokenId,
    pub no_token: TokenId,
    pub tick_size: Price,
    pub min_order_size: Shares,
    pub accepting_orders: bool,
    pub closed: bool,
}

impl MarketOutcome {
    pub fn token(&self, side: OutcomeSide) -> &TokenId {
        match side {
            OutcomeSide::Yes => &self.yes_token,
            OutcomeSide::No => &self.no_token,
        }
    }
}

/// Error raised when an event's buckets do not partition the value space.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartitionError {
    #[error("event has no outcomes")]
    Empty,
    #[error("value {0} is covered by more than one bucket")]
    Overlap(i32),
    #[error("value {0} is not covered by any bucket")]
    Gap(i32),
    #[error("buckets mix temperature units")]
    MixedUnits,
}

/// A daily highest/lowest temperature event: a set of mutually exclusive buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyTemperatureMarket {
    pub event_slug: EventSlug,
    pub event_id: String,
    pub title: String,
    pub location: LocationId,
    pub station: StationId,
    pub local_date: NaiveDate,
    pub timezone: Tz,
    pub extreme: MarketExtreme,
    pub unit: TempUnit,
    pub neg_risk: bool,
    pub outcomes: Vec<MarketOutcome>,
    pub end_time: Option<DateTime<Utc>>,
    pub rules: RulesText,
    pub resolution: ResolutionSpec,
    pub fees: FeeSchedule,
    pub active: bool,
    pub closed: bool,
    pub discovered_at: DateTime<Utc>,
}

impl DailyTemperatureMarket {
    /// The bucket containing a whole-degree value.
    pub fn outcome_for_value(&self, whole: i32) -> Option<&MarketOutcome> {
        self.outcomes.iter().find(|o| o.bucket.contains(whole))
    }

    /// Outcomes sorted from coldest to warmest bucket.
    pub fn sorted_outcomes(&self) -> Vec<&MarketOutcome> {
        let mut v: Vec<&MarketOutcome> = self.outcomes.iter().collect();
        v.sort_by_key(|o| o.bucket.sort_key());
        v
    }

    /// Verify buckets form an exact partition of the integers in a wide range.
    /// Trading is refused on events that fail this check.
    pub fn validate_partition(&self) -> Result<(), PartitionError> {
        if self.outcomes.is_empty() {
            return Err(PartitionError::Empty);
        }
        if self.outcomes.iter().any(|o| o.bucket.unit != self.unit) {
            return Err(PartitionError::MixedUnits);
        }
        for v in -80..=140 {
            let n = self
                .outcomes
                .iter()
                .filter(|o| o.bucket.contains(v))
                .count();
            match n {
                0 => return Err(PartitionError::Gap(v)),
                1 => {}
                _ => return Err(PartitionError::Overlap(v)),
            }
        }
        Ok(())
    }
}

/// One price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Price,
    pub size: Shares,
}

/// Order book of a single outcome token. Bids descending, asks ascending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBook {
    pub token: TokenId,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub tick_size: Price,
    pub min_order_size: Shares,
    /// Exchange timestamp of the snapshot, if provided.
    pub exchange_ts: Option<DateTime<Utc>>,
    /// When Weather Machine received it (knowledge time).
    pub received_at: DateTime<Utc>,
    pub hash: Option<String>,
    /// Latest instant the live feed confirmed the book unchanged (stream
    /// heartbeat on the connection that delivered it). A quiet book is
    /// current, not stale; after a disconnect confirmations stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<DateTime<Utc>>,
}

/// Levels per side the engine keeps (and journals). Positions are small
/// (≤ $10), so fills and depth checks never reach beyond the top levels, and
/// deeper levels only inflate the journal.
pub const ENGINE_BOOK_DEPTH: usize = 5;

impl OrderBook {
    /// Keep the best `depth` levels per side (bids descending, asks ascending).
    pub fn truncated(mut self, depth: usize) -> Self {
        self.bids.truncate(depth);
        self.asks.truncate(depth);
        self
    }

    /// Sort levels and drop empty ones.
    pub fn normalize(&mut self) {
        self.bids.retain(|l| l.size.micros() > 0);
        self.asks.retain(|l| l.size.micros() > 0);
        self.bids.sort_by(|a, b| b.price.cmp(&a.price));
        self.asks.sort_by(|a, b| a.price.cmp(&b.price));
    }

    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks.first().copied()
    }

    /// Ask minus bid; `None` if either side is empty.
    pub fn spread(&self) -> Option<Price> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) if a.price >= b.price => Some(a.price.saturating_sub(b.price)),
            (Some(_), Some(_)) => Some(Price::ZERO),
            _ => None,
        }
    }

    /// Shares and cost available to a buyer at prices `<= limit`.
    pub fn ask_depth_up_to(&self, limit: Price) -> (Shares, Usd) {
        let mut shares = Shares::ZERO;
        let mut cost = Usd::ZERO;
        for l in self.asks.iter().take_while(|l| l.price <= limit) {
            shares += l.size;
            cost += notional(l.price, l.size, Rounding::Up);
        }
        (shares, cost)
    }

    /// Time since the book was last known current (received or confirmed),
    /// in milliseconds.
    pub fn age_ms(&self, now: DateTime<Utc>) -> i64 {
        let current = self
            .confirmed_at
            .map_or(self.received_at, |c| c.max(self.received_at));
        (now - current).num_milliseconds()
    }
}

/// A public trade print.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradePrint {
    pub token: TokenId,
    pub price: Price,
    pub size: Shares,
    pub aggressor: Option<Side>,
    pub ts: DateTime<Utc>,
}

/// A taker's trade with who made it, from the public trade history
/// (Polymarket's Data API) rather than the market feed, whose prints do not
/// say who traded. The strategy lab's flow rules (L18–L21) read it; the
/// wallet is kept only as a short hash, to tell takers apart and to look up
/// their record on earlier days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakerTrade {
    pub token: TokenId,
    /// The taker's side on `token`.
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub at: DateTime<Utc>,
    /// Short hash of the taker's wallet (`None`: not reported).
    pub taker: Option<String>,
    /// Identifies the trade across overlapping polls.
    pub id: String,
}

/// Fee schedule of a market. Polymarket (2026) charges takers
/// `fee = shares × rate × p × (1 − p)`; makers pay zero.
/// The rate is configuration/API supplied — never assumed to be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Taker fee rate in micro-units (0.05 = 50_000).
    pub taker_rate_micros: u32,
    /// Maker fee rate in micro-units (usually 0).
    pub maker_rate_micros: u32,
}

impl FeeSchedule {
    pub const ZERO: FeeSchedule = FeeSchedule {
        taker_rate_micros: 0,
        maker_rate_micros: 0,
    };

    pub fn taker(rate_micros: u32) -> Self {
        Self {
            taker_rate_micros: rate_micros,
            maker_rate_micros: 0,
        }
    }

    /// Fee charged for trading `shares` at `price` with the given rate, rounded
    /// up (fees we pay are always rounded against us).
    pub fn fee(rate_micros: u32, price: Price, shares: Shares) -> Usd {
        let p = i128::from(price.micros());
        let q = i128::from(MICROS_PER_UNIT) - p;
        let num = i128::from(shares.micros().max(0)) * i128::from(rate_micros) * p * q;
        let den = i128::from(MICROS_PER_UNIT).pow(3);
        let v = num / den + i128::from(num % den != 0);
        Usd::from_micros(i64::try_from(v).unwrap_or(i64::MAX))
    }

    pub fn taker_fee(&self, price: Price, shares: Shares) -> Usd {
        Self::fee(self.taker_rate_micros, price, shares)
    }

    pub fn maker_fee(&self, price: Price, shares: Shares) -> Usd {
        Self::fee(self.maker_rate_micros, price, shares)
    }

    /// Taker fee per share as a float fraction of 1.0 (for EV math).
    pub fn taker_fee_per_share(&self, price: Price) -> f64 {
        let p = price.as_f64();
        f64::from(self.taker_rate_micros) / 1_000_000.0 * p * (1.0 - p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> TokenId {
        TokenId::new(s).unwrap()
    }

    #[test]
    fn bucket_contains_and_labels() {
        let b = TemperatureBucket::exact(18, TempUnit::Celsius);
        assert!(b.contains(18) && !b.contains(17) && !b.contains(19));
        assert_eq!(b.label(), "18°C");
        let lo = TemperatureBucket::at_or_below(13, TempUnit::Celsius);
        assert!(lo.contains(-20) && lo.contains(13) && !lo.contains(14));
        assert_eq!(lo.label(), "≤13°C");
        let hi = TemperatureBucket::at_or_above(24, TempUnit::Celsius);
        assert!(hi.contains(40) && !hi.contains(23));
        let r = TemperatureBucket::range(87, 86, TempUnit::Fahrenheit);
        assert_eq!(r.label(), "86-87°F");
        assert!(r.contains(86) && r.contains(87) && !r.contains(88));
    }

    #[test]
    fn a_confirmed_quiet_book_is_current() {
        let t = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_default()
        };
        let mut b = OrderBook {
            token: tok("1"),
            bids: vec![],
            asks: vec![],
            tick_size: Price::saturating_from_micros(10_000),
            min_order_size: Shares::from_whole(5),
            exchange_ts: None,
            received_at: t("2026-07-01T12:00:00Z"),
            hash: None,
            confirmed_at: None,
        };
        let now = t("2026-07-01T12:01:00Z");
        assert_eq!(b.age_ms(now), 60_000);
        b.confirmed_at = Some(t("2026-07-01T12:00:55Z"));
        assert_eq!(b.age_ms(now), 5_000);
        // A confirmation older than the content never makes it older.
        b.confirmed_at = Some(t("2026-07-01T11:00:00Z"));
        assert_eq!(b.age_ms(now), 60_000);
        let json = serde_json::to_string(&OrderBook {
            confirmed_at: None,
            ..b.clone()
        })
        .unwrap_or_default();
        assert!(
            !json.contains("confirmed_at"),
            "journal format unchanged without it"
        );
    }

    #[test]
    fn book_best_prices_depth_and_spread() {
        let mut book = OrderBook {
            token: tok("1"),
            bids: vec![
                BookLevel {
                    price: Price::parse("0.93").unwrap(),
                    size: Shares::from_whole(50),
                },
                BookLevel {
                    price: Price::parse("0.94").unwrap(),
                    size: Shares::from_whole(10),
                },
                BookLevel {
                    price: Price::parse("0.90").unwrap(),
                    size: Shares::ZERO,
                },
            ],
            asks: vec![
                BookLevel {
                    price: Price::parse("0.97").unwrap(),
                    size: Shares::from_whole(100),
                },
                BookLevel {
                    price: Price::parse("0.96").unwrap(),
                    size: Shares::from_whole(20),
                },
            ],
            tick_size: Price::parse("0.01").unwrap(),
            min_order_size: Shares::from_whole(5),
            exchange_ts: None,
            received_at: Utc::now(),
            hash: None,
            confirmed_at: None,
        };
        book.normalize();
        assert_eq!(
            book.best_bid().unwrap().price,
            Price::parse("0.94").unwrap()
        );
        assert_eq!(
            book.best_ask().unwrap().price,
            Price::parse("0.96").unwrap()
        );
        assert_eq!(book.spread(), Some(Price::parse("0.02").unwrap()));
        assert_eq!(book.bids.len(), 2, "zero-size level dropped");
        let (sh, cost) = book.ask_depth_up_to(Price::parse("0.96").unwrap());
        assert_eq!(sh, Shares::from_whole(20));
        assert_eq!(cost, Usd::parse("19.2").unwrap());
        let (sh, _) = book.ask_depth_up_to(Price::parse("0.99").unwrap());
        assert_eq!(sh, Shares::from_whole(120));
    }

    #[test]
    fn polymarket_fee_formula() {
        let fees = FeeSchedule::taker(50_000);
        // At p = 0.50 the fee is 0.05 × 0.25 = $0.0125/share → $1.25 per 100 shares.
        assert_eq!(
            fees.taker_fee(Price::parse("0.5").unwrap(), Shares::from_whole(100)),
            Usd::parse("1.25").unwrap()
        );
        // At p = 0.95: 0.05 × 0.95 × 0.05 = 0.002375/share.
        assert_eq!(
            fees.taker_fee(Price::parse("0.95").unwrap(), Shares::from_whole(100)),
            Usd::parse("0.2375").unwrap()
        );
        assert!((fees.taker_fee_per_share(Price::parse("0.95").unwrap()) - 0.002375).abs() < 1e-12);
        assert_eq!(
            fees.maker_fee(Price::parse("0.95").unwrap(), Shares::from_whole(100)),
            Usd::ZERO
        );
        assert_eq!(
            FeeSchedule::ZERO.taker_fee(Price::parse("0.5").unwrap(), Shares::from_whole(100)),
            Usd::ZERO
        );
    }
}
