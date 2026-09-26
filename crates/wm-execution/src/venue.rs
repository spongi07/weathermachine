//! Execution venue port. The live implementation is deliberately disabled.
//!
//! Phase 14 will add a Polymarket venue (EIP-712 V2 orders via the official
//! `polymarket_client_sdk_v2` crate, pUSD collateral, heartbeat/cancel-on-
//! disconnect, jurisdiction gate). Until then `DisabledLiveVenue` refuses
//! everything, so no code path can place a real order.

use wm_core::ids::ClientOrderId;
use wm_core::ingest::BoxFuture;
use wm_core::market::Side;
use wm_core::trading::TimeInForce;
use wm_core::units::{Price, Shares};
use wm_risk::ApprovedIntent;

/// Order as sent to a venue. Only constructible from an [`ApprovedIntent`].
#[derive(Debug, Clone, PartialEq)]
pub struct VenueOrder {
    pub client_order_id: ClientOrderId,
    pub token: String,
    pub side: Side,
    pub limit_price: Price,
    pub shares: Shares,
    pub tif: TimeInForce,
}

impl From<&ApprovedIntent> for VenueOrder {
    fn from(a: &ApprovedIntent) -> Self {
        let i = a.intent();
        Self {
            client_order_id: a.client_order_id().clone(),
            token: i.token.to_string(),
            side: i.side,
            limit_price: i.limit_price,
            shares: i.shares,
            tif: i.tif,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueAck {
    pub venue_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VenueError {
    #[error("live trading is disabled in this build (Phase 14 gate)")]
    LiveTradingDisabled,
    #[error("venue rejected order: {0}")]
    Rejected(String),
    #[error("venue unavailable: {0}")]
    Unavailable(String),
}

/// Execution venue port.
pub trait ExecutionVenue: Send + Sync {
    fn name(&self) -> &str;
    fn submit(&self, order: VenueOrder) -> BoxFuture<'_, Result<VenueAck, VenueError>>;
    fn cancel(&self, id: ClientOrderId) -> BoxFuture<'_, Result<(), VenueError>>;
}

/// Live venue placeholder: refuses every request.
#[derive(Debug, Default, Clone, Copy)]
pub struct DisabledLiveVenue;

impl ExecutionVenue for DisabledLiveVenue {
    fn name(&self) -> &str {
        "polymarket-live (disabled)"
    }

    fn submit(&self, _order: VenueOrder) -> BoxFuture<'_, Result<VenueAck, VenueError>> {
        Box::pin(async { Err(VenueError::LiveTradingDisabled) })
    }

    fn cancel(&self, _id: ClientOrderId) -> BoxFuture<'_, Result<(), VenueError>> {
        Box::pin(async { Err(VenueError::LiveTradingDisabled) })
    }
}
