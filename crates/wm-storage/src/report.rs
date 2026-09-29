//! Read-only queries behind `weather-machine report paper`: what the paper
//! run saw, decided and did, per local day. Every query is bounded by a time
//! range; nothing here writes.

use crate::{PgStore, Result};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::Row;
use wm_core::event::{ForecastEvent, WeatherMachineEvent};

/// One stored version of a METAR/SPECI.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportObservation {
    pub observed_at: DateTime<Utc>,
    pub report_type: String,
    pub version: i32,
    pub temperature_dc: Option<i32>,
    /// The source that delivered this version first.
    pub provider: String,
    pub fetched_at: DateTime<Utc>,
    pub from_failover: bool,
}

/// A service start (one row per run).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRun {
    pub started_at: DateTime<Utc>,
    pub version: String,
    pub model_id: String,
}

/// A decision record: an evaluation (one per weather report) or a proposal.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDecision {
    pub at: DateTime<Utc>,
    pub strategy: String,
    pub event_slug: Option<String>,
    pub summary: String,
    pub inputs: serde_json::Value,
    pub outputs: serde_json::Value,
    pub approved: bool,
    pub reasons: Vec<String>,
}

/// One bucket of a discovered market.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportOutcome {
    pub event_slug: String,
    pub local_date: NaiveDate,
    pub label: String,
    pub lower: Option<i32>,
    pub upper: Option<i32>,
    pub yes_token: String,
    pub no_token: String,
    pub taker_fee_rate_micros: i32,
}

/// Top of a recorded order book.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReportBookTop {
    pub captured_at: DateTime<Utc>,
    pub bid_micros: Option<i32>,
    pub ask_micros: Option<i32>,
}

/// A paper order as last updated.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportOrder {
    pub client_order_id: String,
    pub strategy: String,
    pub event_slug: String,
    pub token_id: String,
    pub outcome_side: String,
    pub side: String,
    pub kind: String,
    pub limit_price_micros: i32,
    pub shares_micros: i64,
    pub status: String,
    pub filled_micros: i64,
    pub avg_price_micros: Option<i32>,
    pub fees_micros: i64,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A paper fill.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportFill {
    pub client_order_id: String,
    pub token_id: String,
    pub side: String,
    pub price_micros: i32,
    pub shares_micros: i64,
    pub fee_micros: i64,
    pub liquidity: String,
    pub ts: DateTime<Utc>,
}

/// Requests to one provider on one local day.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRequestDay {
    pub provider: String,
    pub date: NaiveDate,
    pub requests: i64,
    pub failures: i64,
    pub throttled: i64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub max_ms: i64,
}

/// A provider health transition.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportHealthChange {
    pub provider: String,
    pub state: String,
    pub previous_state: Option<String>,
    pub reason: String,
    pub at: DateTime<Utc>,
}

/// A logged system event (alerts, warnings).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportSystemEvent {
    pub at: DateTime<Utc>,
    pub level: String,
    pub kind: String,
    pub message: String,
}

impl PgStore {
    /// Every stored version of the station's reports observed in `[from, to)`.
    pub async fn report_observations(
        &self,
        station: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportObservation>> {
        let rows = sqlx::query(
            "SELECT observed_at, report_type, version, temperature_dc, provider, fetched_at,
                    COALESCE((quality->>'from_failover')::boolean, false) AS from_failover
             FROM weather_observations
             WHERE station_id = $1 AND observed_at >= $2 AND observed_at < $3
             ORDER BY observed_at, report_type, version",
        )
        .bind(station)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportObservation {
                    observed_at: r.try_get("observed_at")?,
                    report_type: r.try_get("report_type")?,
                    version: r.try_get("version")?,
                    temperature_dc: r.try_get("temperature_dc")?,
                    provider: r.try_get("provider")?,
                    fetched_at: r.try_get("fetched_at")?,
                    from_failover: r.try_get("from_failover")?,
                })
            })
            .collect()
    }

    /// Service starts in `[from, to)`, oldest first.
    pub async fn report_runs(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportRun>> {
        let rows = sqlx::query(
            "SELECT started_at, version, model_id FROM strategy_runs
             WHERE started_at >= $1 AND started_at < $2 AND mode <> 'backtest'
             ORDER BY started_at",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportRun {
                    started_at: r.try_get("started_at")?,
                    version: r.try_get("version")?,
                    model_id: r.try_get("model_id")?,
                })
            })
            .collect()
    }

    /// The first service start ever recorded (the start of the paper history).
    pub async fn first_run_started_at(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query("SELECT min(started_at) FROM strategy_runs WHERE mode <> 'backtest'")
                .fetch_one(&self.pool)
                .await?
                .try_get(0)?,
        )
    }

    /// Forecasts received in `[from, to)` (from the event journal), oldest first.
    pub async fn report_forecasts(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(DateTime<Utc>, ForecastEvent)>> {
        let rows = sqlx::query(
            "SELECT available_at, payload FROM event_journal
             WHERE kind = 'forecast_update' AND available_at >= $1 AND available_at < $2
             ORDER BY available_at",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: sqlx::types::Json<WeatherMachineEvent> = r.try_get("payload")?;
            if let WeatherMachineEvent::ForecastUpdate(f) = payload.0 {
                out.push((r.try_get("available_at")?, f));
            }
        }
        Ok(out)
    }

    /// Decision records of a location in `[from, to)`, oldest first.
    pub async fn report_decisions(
        &self,
        location: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportDecision>> {
        let rows = sqlx::query(
            "SELECT at, strategy, event_slug, summary, inputs, outputs, approved, reasons
             FROM decision_snapshots
             WHERE location_id = $1 AND at >= $2 AND at < $3
             ORDER BY at, decision_id",
        )
        .bind(location)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let inputs: sqlx::types::Json<serde_json::Value> = r.try_get("inputs")?;
                let outputs: sqlx::types::Json<serde_json::Value> = r.try_get("outputs")?;
                Ok(ReportDecision {
                    at: r.try_get("at")?,
                    strategy: r.try_get("strategy")?,
                    event_slug: r.try_get("event_slug")?,
                    summary: r.try_get("summary")?,
                    inputs: inputs.0,
                    outputs: outputs.0,
                    approved: r.try_get("approved")?,
                    reasons: r.try_get("reasons")?,
                })
            })
            .collect()
    }

    /// Buckets of the location's markets for local dates `[from, to]`.
    pub async fn report_outcomes(
        &self,
        location: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<ReportOutcome>> {
        let rows = sqlx::query(
            "SELECT m.event_slug, m.local_date, m.taker_fee_rate_micros, o.label,
                    o.bucket_lower, o.bucket_upper, o.yes_token, o.no_token
             FROM markets m JOIN market_outcomes o ON o.event_slug = m.event_slug
             WHERE m.location_id = $1 AND m.local_date >= $2 AND m.local_date <= $3
             ORDER BY m.local_date, o.bucket_lower NULLS FIRST",
        )
        .bind(location)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportOutcome {
                    event_slug: r.try_get("event_slug")?,
                    local_date: r.try_get("local_date")?,
                    label: r.try_get("label")?,
                    lower: r.try_get("bucket_lower")?,
                    upper: r.try_get("bucket_upper")?,
                    yes_token: r.try_get("yes_token")?,
                    no_token: r.try_get("no_token")?,
                    taker_fee_rate_micros: r.try_get("taker_fee_rate_micros")?,
                })
            })
            .collect()
    }

    /// Recorded best bid/ask of one token in `[from, to)`, oldest first.
    pub async fn report_book_tops(
        &self,
        token: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportBookTop>> {
        let rows = sqlx::query(
            "SELECT captured_at, best_bid_micros, best_ask_micros FROM orderbook_snapshots
             WHERE token_id = $1 AND captured_at >= $2 AND captured_at < $3
             ORDER BY captured_at",
        )
        .bind(token)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportBookTop {
                    captured_at: r.try_get("captured_at")?,
                    bid_micros: r.try_get("best_bid_micros")?,
                    ask_micros: r.try_get("best_ask_micros")?,
                })
            })
            .collect()
    }

    /// Paper orders of a location created in `[from, to)`.
    pub async fn report_orders(
        &self,
        location: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportOrder>> {
        let rows = sqlx::query(
            "SELECT client_order_id, strategy, event_slug, token_id, outcome_side, side, kind,
                    limit_price_micros, shares_micros, status, filled_micros, avg_price_micros,
                    fees_micros, reason, created_at
             FROM orders
             WHERE location_id = $1 AND created_at >= $2 AND created_at < $3
             ORDER BY created_at",
        )
        .bind(location)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportOrder {
                    client_order_id: r.try_get("client_order_id")?,
                    strategy: r.try_get("strategy")?,
                    event_slug: r.try_get("event_slug")?,
                    token_id: r.try_get("token_id")?,
                    outcome_side: r.try_get("outcome_side")?,
                    side: r.try_get("side")?,
                    kind: r.try_get("kind")?,
                    limit_price_micros: r.try_get("limit_price_micros")?,
                    shares_micros: r.try_get("shares_micros")?,
                    status: r.try_get("status")?,
                    filled_micros: r.try_get("filled_micros")?,
                    avg_price_micros: r.try_get("avg_price_micros")?,
                    fees_micros: r.try_get("fees_micros")?,
                    reason: r.try_get("reason")?,
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    /// Fills of the location's paper orders in `[from, to)`.
    pub async fn report_fills(
        &self,
        location: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportFill>> {
        let rows = sqlx::query(
            "SELECT f.client_order_id, f.token_id, f.side, f.price_micros, f.shares_micros,
                    f.fee_micros, f.liquidity, f.ts
             FROM fills f JOIN orders o ON o.client_order_id = f.client_order_id
             WHERE o.location_id = $1 AND f.ts >= $2 AND f.ts < $3
             ORDER BY f.ts, f.id",
        )
        .bind(location)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportFill {
                    client_order_id: r.try_get("client_order_id")?,
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

    /// Requests per provider and local day (in `timezone`) in `[from, to)`.
    pub async fn report_requests(
        &self,
        timezone: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportRequestDay>> {
        let rows = sqlx::query(
            "SELECT provider, (requested_at AT TIME ZONE $1)::date AS day,
                    count(*) AS requests,
                    count(*) FILTER (WHERE error_class IS NOT NULL) AS failures,
                    count(*) FILTER (WHERE throttled) AS throttled,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY latency_ms) AS p50,
                    percentile_cont(0.9) WITHIN GROUP (ORDER BY latency_ms) AS p90,
                    max(latency_ms) AS max_ms
             FROM provider_requests
             WHERE requested_at >= $2 AND requested_at < $3
             GROUP BY provider, day
             ORDER BY day, provider",
        )
        .bind(timezone)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportRequestDay {
                    provider: r.try_get("provider")?,
                    date: r.try_get("day")?,
                    requests: r.try_get("requests")?,
                    failures: r.try_get("failures")?,
                    throttled: r.try_get("throttled")?,
                    p50_ms: r.try_get::<Option<f64>, _>("p50")?.unwrap_or(0.0),
                    p90_ms: r.try_get::<Option<f64>, _>("p90")?.unwrap_or(0.0),
                    max_ms: r.try_get::<Option<i64>, _>("max_ms")?.unwrap_or(0),
                })
            })
            .collect()
    }

    /// Provider health transitions in `[from, to)`, oldest first.
    pub async fn report_health_changes(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportHealthChange>> {
        let rows = sqlx::query(
            "SELECT provider, state, previous_state, reason, at FROM provider_health_events
             WHERE at >= $1 AND at < $2 ORDER BY at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportHealthChange {
                    provider: r.try_get("provider")?,
                    state: r.try_get("state")?,
                    previous_state: r.try_get("previous_state")?,
                    reason: r.try_get("reason")?,
                    at: r.try_get("at")?,
                })
            })
            .collect()
    }

    /// System events in `[from, to)`, oldest first.
    pub async fn report_system_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<ReportSystemEvent>> {
        let rows = sqlx::query(
            "SELECT at, level, kind, message FROM system_events
             WHERE at >= $1 AND at < $2 ORDER BY at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(ReportSystemEvent {
                    at: r.try_get("at")?,
                    level: r.try_get("level")?,
                    kind: r.try_get("kind")?,
                    message: r.try_get("message")?,
                })
            })
            .collect()
    }
}
