//! Order lifecycle management (pure, idempotent).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use wm_core::ids::{ClientOrderId, ConditionId, DecisionId, EventSlug, LocationId, StrategyId, TokenId};
use wm_core::market::{OutcomeSide, Side, TemperatureBucket};
use wm_core::portfolio::InstrumentRef;
use wm_core::trading::{IntentKind, OrderStatus, OrderUpdate, TimeInForce};
use wm_core::units::{Price, Shares, Usd};
use wm_risk::{ApprovedIntent, OpenOrderView};

/// Full record of one order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub client_order_id: ClientOrderId,
    pub decision_id: DecisionId,
    pub strategy: StrategyId,
    pub location: LocationId,
    pub event_slug: EventSlug,
    pub token: TokenId,
    pub condition_id: ConditionId,
    pub outcome_side: OutcomeSide,
    pub bucket: TemperatureBucket,
    pub side: Side,
    pub kind: IntentKind,
    pub limit_price: Price,
    pub shares: Shares,
    pub tif: TimeInForce,
    pub status: OrderStatus,
    pub filled: Shares,
    pub avg_price: Option<Price>,
    pub fees: Usd,
    pub venue_order_id: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl OrderRecord {
    pub fn remaining(&self) -> Shares {
        self.shares - self.filled
    }

    pub fn instrument(&self) -> InstrumentRef {
        InstrumentRef {
            token: self.token.clone(),
            condition_id: self.condition_id.clone(),
            event_slug: self.event_slug.clone(),
            outcome_side: self.outcome_side,
            bucket: self.bucket,
        }
    }
}

/// Errors applying order updates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrderError {
    #[error("order {0} already registered")]
    Duplicate(ClientOrderId),
    #[error("unknown order {0}")]
    Unknown(ClientOrderId),
    #[error("invalid transition {from:?} → {to:?} for {id}")]
    InvalidTransition { id: ClientOrderId, from: OrderStatus, to: OrderStatus },
    #[error("filled quantity would decrease or exceed order size for {0}")]
    InvalidFill(ClientOrderId),
}

/// Result of applying an update.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    Changed { previous: OrderStatus, newly_filled: Shares },
    /// Duplicate delivery of an already applied update (idempotent).
    Unchanged,
}

fn allowed(from: OrderStatus, to: OrderStatus) -> bool {
    use OrderStatus::*;
    match from {
        PendingSubmit => to != PendingSubmit,
        Live => matches!(to, Live | PartiallyFilled | Filled | Canceled | Expired),
        PartiallyFilled => matches!(to, PartiallyFilled | Filled | Canceled | Expired),
        Filled | Canceled | Rejected | Expired => false,
    }
}

/// Order book of our own orders.
#[derive(Debug, Clone, Default)]
pub struct OrderManager {
    orders: BTreeMap<ClientOrderId, OrderRecord>,
}

impl OrderManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an approved intent as a pending order.
    pub fn register(&mut self, approved: &ApprovedIntent, bucket: TemperatureBucket, now: DateTime<Utc>) -> Result<&OrderRecord, OrderError> {
        let id = approved.client_order_id().clone();
        if self.orders.contains_key(&id) {
            return Err(OrderError::Duplicate(id));
        }
        let i = approved.intent();
        let rec = OrderRecord {
            client_order_id: id.clone(),
            decision_id: i.decision_id,
            strategy: i.strategy.clone(),
            location: i.location.clone(),
            event_slug: i.event_slug.clone(),
            token: i.token.clone(),
            condition_id: i.condition_id.clone(),
            outcome_side: i.outcome_side,
            bucket,
            side: i.side,
            kind: i.kind,
            limit_price: i.limit_price,
            shares: i.shares,
            tif: i.tif,
            status: OrderStatus::PendingSubmit,
            filled: Shares::ZERO,
            avg_price: None,
            fees: Usd::ZERO,
            venue_order_id: None,
            reason: None,
            created_at: now,
            updated_at: now,
        };
        Ok(self.orders.entry(id).or_insert(rec))
    }

    pub fn get(&self, id: &ClientOrderId) -> Option<&OrderRecord> {
        self.orders.get(id)
    }

    /// Apply a venue update. Terminal orders ignore repeated identical updates.
    pub fn apply(&mut self, u: &OrderUpdate) -> Result<Applied, OrderError> {
        let rec = self.orders.get_mut(&u.client_order_id).ok_or_else(|| OrderError::Unknown(u.client_order_id.clone()))?;
        if rec.status == u.status && rec.filled == u.filled {
            return Ok(Applied::Unchanged);
        }
        if !allowed(rec.status, u.status) {
            return Err(OrderError::InvalidTransition { id: rec.client_order_id.clone(), from: rec.status, to: u.status });
        }
        if u.filled < rec.filled || u.filled > rec.shares {
            return Err(OrderError::InvalidFill(rec.client_order_id.clone()));
        }
        let previous = rec.status;
        let newly_filled = u.filled - rec.filled;
        rec.status = u.status;
        rec.filled = u.filled;
        rec.avg_price = u.avg_price.or(rec.avg_price);
        rec.fees = u.fee_paid;
        rec.venue_order_id = u.venue_order_id.clone().or(rec.venue_order_id.take());
        rec.reason = u.reason.clone().or(rec.reason.take());
        rec.updated_at = u.ts;
        Ok(Applied::Changed { previous, newly_filled })
    }

    pub fn open_orders(&self) -> impl Iterator<Item = &OrderRecord> {
        self.orders.values().filter(|o| !o.status.is_terminal())
    }

    /// Risk-engine view of live orders.
    pub fn open_views(&self) -> Vec<OpenOrderView> {
        self.open_orders()
            .map(|o| OpenOrderView {
                client_order_id: o.client_order_id.clone(),
                token: o.token.clone(),
                event_slug: o.event_slug.clone(),
                location: o.location.clone(),
                strategy: o.strategy.clone(),
                side: o.side,
                outcome_side: o.outcome_side,
                bucket: o.bucket,
                remaining: o.remaining(),
                limit_price: o.limit_price,
            })
            .collect()
    }

    pub fn pending_tokens(&self) -> HashSet<TokenId> {
        self.open_orders().map(|o| o.token.clone()).collect()
    }

    /// Most recently updated orders first.
    pub fn recent(&self, n: usize) -> Vec<&OrderRecord> {
        let mut v: Vec<&OrderRecord> = self.orders.values().collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        v.truncate(n);
        v
    }

    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}
