//! Trading intents, orders, fills and decision audit records.

use crate::ids::{ClientOrderId, ConditionId, DecisionId, EventSlug, LocationId, StrategyId, TokenId};
use crate::market::{OutcomeSide, Side};
use crate::units::{Price, Probability, Shares, Usd};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Execution environment. The strategy code is identical in all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Backtest,
    Paper,
    Live,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::Backtest => "backtest",
            RunMode::Paper => "paper",
            RunMode::Live => "live",
        }
    }
}

impl fmt::Display for RunMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opening new risk vs. reducing existing risk. Risk checks differ: stale data
/// blocks `Open` but never blocks `Reduce`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Open,
    Reduce,
}

/// Order time in force (Polymarket CLOB semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tif", rename_all = "snake_case")]
pub enum TimeInForce {
    /// Rests until filled or cancelled.
    Gtc,
    /// Rests until `expires_at`.
    Gtd { expires_at: DateTime<Utc> },
    /// Fill entirely immediately or cancel.
    Fok,
    /// Fill what is available immediately, cancel the rest.
    Fak,
}

/// A strategy's request to trade. Not an order: it must pass the risk engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeIntent {
    pub decision_id: DecisionId,
    pub strategy: StrategyId,
    pub created_at: DateTime<Utc>,
    pub location: LocationId,
    pub event_slug: EventSlug,
    pub condition_id: ConditionId,
    pub token: TokenId,
    pub outcome_side: OutcomeSide,
    pub bucket_label: String,
    pub side: Side,
    pub kind: IntentKind,
    /// Depends on weather data freshness/health (fail closed if stale).
    pub weather_dependent: bool,
    pub limit_price: Price,
    pub shares: Shares,
    /// Maximum cost (buy) or minimum proceeds (sell) at the limit price.
    pub notional: Usd,
    pub tif: TimeInForce,
    /// Model probability that this leg pays out.
    pub model_probability: Probability,
    /// Expected value per share after fees and slippage, in collateral units.
    pub expected_value_per_share: f64,
    pub break_even_probability: f64,
    /// Research-only strategies are rejected outside backtests.
    pub research_only: bool,
    pub rationale: Vec<String>,
}

/// Order lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    PendingSubmit,
    Live,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Rejected | OrderStatus::Expired
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::PendingSubmit => "pending_submit",
            OrderStatus::Live => "live",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Canceled => "canceled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Expired => "expired",
        }
    }
}

/// Maker or taker liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// A fill (execution) of one of our orders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub client_order_id: ClientOrderId,
    pub token: TokenId,
    pub side: Side,
    pub price: Price,
    pub shares: Shares,
    pub fee: Usd,
    pub liquidity: Liquidity,
    pub ts: DateTime<Utc>,
}

/// Venue-reported change to one of our orders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderUpdate {
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<String>,
    pub status: OrderStatus,
    /// Cumulative filled shares.
    pub filled: Shares,
    pub avg_price: Option<Price>,
    pub fee_paid: Usd,
    pub reason: Option<String>,
    pub ts: DateTime<Utc>,
}

/// Complete audit record of one strategy/risk decision: enough to answer
/// "why did (or didn't) we trade?" without re-running anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub decision_id: DecisionId,
    pub strategy: StrategyId,
    pub at: DateTime<Utc>,
    pub location: LocationId,
    pub event_slug: Option<EventSlug>,
    pub summary: String,
    /// Inputs: state, features, probabilities, book tops, freshness.
    pub inputs: serde_json::Value,
    /// Outputs: intent (if any) and risk outcome.
    pub outputs: serde_json::Value,
    pub approved: bool,
    pub reasons: Vec<String>,
}
