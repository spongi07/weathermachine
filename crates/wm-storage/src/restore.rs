//! Read-only queries behind a restart's paper-book restore: the fills of the
//! markets that still settle, today's opening orders and the markets' stored
//! Gamma payloads. Nothing here writes.

use crate::{PgStore, Result};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::Row;

/// A stored fill with its order's strategy and market.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoreFillRow {
    pub client_order_id: String,
    pub strategy: String,
    pub location_id: String,
    pub event_slug: String,
    /// The market's local day.
    pub local_date: NaiveDate,
    pub token_id: String,
    pub side: String,
    pub price_micros: i32,
    pub shares_micros: i64,
    pub fee_micros: i64,
    pub liquidity: String,
    pub ts: DateTime<Utc>,
}

impl PgStore {
    /// Fills of orders on markets of local day `from_date` or later, oldest
    /// first.
    pub async fn restore_fills(&self, from_date: NaiveDate) -> Result<Vec<RestoreFillRow>> {
        let rows = sqlx::query(
            "SELECT f.client_order_id, o.strategy, o.location_id, o.event_slug, m.local_date,
                    f.token_id, f.side, f.price_micros, f.shares_micros, f.fee_micros,
                    f.liquidity, f.ts
             FROM fills f
             JOIN orders o ON o.client_order_id = f.client_order_id
             JOIN markets m ON m.event_slug = o.event_slug
             WHERE m.local_date >= $1
             ORDER BY f.ts, f.id",
        )
        .bind(from_date)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(RestoreFillRow {
                    client_order_id: r.try_get("client_order_id")?,
                    strategy: r.try_get("strategy")?,
                    location_id: r.try_get("location_id")?,
                    event_slug: r.try_get("event_slug")?,
                    local_date: r.try_get("local_date")?,
                    token_id: r.try_get("token_id")?,
                    side: r.try_get("side")?,
                    price_micros: r.try_get("price_micros")?,
                    shares_micros: r.try_get("shares_micros")?,
                    fee_micros: r.try_get("fee_micros")?,
                    liquidity: r.try_get("liquidity")?,
                    ts: r.try_get("ts")?,
                })
            })
            .collect()
    }

    /// Limit price and shares (micros) of the opening buy orders created at
    /// or after `since`: what their approvals added to the daily new
    /// exposure.
    pub async fn opening_orders_since(&self, since: DateTime<Utc>) -> Result<Vec<(i32, i64)>> {
        let rows = sqlx::query(
            "SELECT limit_price_micros, shares_micros FROM orders
             WHERE created_at >= $1 AND side = 'BUY' AND kind = 'open'",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok((
                    r.try_get("limit_price_micros")?,
                    r.try_get("shares_micros")?,
                ))
            })
            .collect()
    }

    /// The latest stored Gamma payload of a market: when it was captured and
    /// the response body.
    pub async fn latest_market_payload(
        &self,
        event_slug: &str,
    ) -> Result<Option<(DateTime<Utc>, Vec<u8>)>> {
        let row = sqlx::query(
            "SELECT captured_at, payload FROM market_snapshots
             WHERE event_slug = $1
             ORDER BY captured_at DESC, id DESC
             LIMIT 1",
        )
        .bind(event_slug)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok((r.try_get("captured_at")?, r.try_get("payload")?)))
            .transpose()
    }
}
