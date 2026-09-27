//! # wm-storage
//!
//! PostgreSQL persistence (SQLx, runtime-checked queries, embedded
//! migrations). Everything the collectors ingest, every engine input
//! (event journal), every decision, order and fill is stored here.
//!
//! Queries are plain `&'static str` SQL (SQLx 0.9 `SqlSafeStr`), bound with
//! typed parameters — never string-formatted.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Connection, Row};
use std::time::Duration;
use wm_core::event::{EventEnvelope, EventSource, ProviderHealthEvent};
use wm_core::ids::{ProviderId, RunId, StationId};
use wm_core::ingest::{BoxFuture, IngestBatch, IngestSink, SinkError};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side, TempUnit};
use wm_core::trading::{DecisionRecord, Fill, Liquidity, RunMode};
use wm_core::units::TempC;
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};

/// Embedded migrations (`/migrations`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid stored data: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Connection pool plus repositories.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

fn unit_str(u: TempUnit) -> &'static str {
    match u {
        TempUnit::Celsius => "celsius",
        TempUnit::Fahrenheit => "fahrenheit",
    }
}

fn source_str(s: EventSource) -> &'static str {
    match s {
        EventSource::Live => "live",
        EventSource::Replay => "replay",
        EventSource::Synthetic => "synthetic",
        EventSource::Operator => "operator",
    }
}

fn precision_str(p: TempPrecision) -> &'static str {
    match p {
        TempPrecision::WholeDegree => "whole_degree",
        TempPrecision::Tenth => "tenth",
    }
}

/// Stable 64-bit key for advisory locks (FNV-1a).
pub fn lock_key(name: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h as i64
}

/// Cross-process exclusivity for a station collector (PostgreSQL session
/// advisory lock on a dedicated, detached connection). Dropping the lease
/// closes the connection, which releases the lock server-side.
pub struct StationLease {
    conn: Option<PgConnection>,
    key: i64,
    pub name: String,
}

impl StationLease {
    /// Release explicitly (also happens on drop via connection close).
    pub async fn release(mut self) -> Result<()> {
        if let Some(mut c) = self.conn.take() {
            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(self.key)
                .execute(&mut c)
                .await?;
            c.close().await?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for StationLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StationLease")
            .field("name", &self.name)
            .finish()
    }
}

/// Request counts for budget dashboards.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RequestStats {
    pub provider: String,
    pub requests: i64,
    pub throttled: i64,
    pub failures: i64,
    pub not_modified: i64,
}

impl PgStore {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(2))
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Try to take the collector lease for `station`. `None` = held elsewhere.
    pub async fn try_station_lease(&self, station: &StationId) -> Result<Option<StationLease>> {
        let name = format!("wm:collector:{station}");
        let key = lock_key(&name);
        let conn = self.pool.acquire().await?;
        let mut conn = conn.detach();
        let got: bool = sqlx::query("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut conn)
            .await?
            .try_get(0)?;
        if got {
            Ok(Some(StationLease {
                conn: Some(conn),
                key,
                name,
            }))
        } else {
            conn.close().await?;
            Ok(None)
        }
    }

    // -- Ingest ------------------------------------------------------------------

    /// Persist one collector poll atomically.
    pub async fn persist_ingest(&self, b: &IngestBatch) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = &b.request;
        sqlx::query(
            "INSERT INTO provider_requests (provider, endpoint, station_id, requested_at, completed_at, status, latency_ms, bytes, cache, retry_count, throttled, error_class, gate_wait_ms, payload_sha256)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(r.provider.as_str())
        .bind(&r.endpoint)
        .bind(r.station.as_ref().map(|s| s.as_str()))
        .bind(r.requested_at)
        .bind(r.completed_at)
        .bind(r.status.map(i32::from))
        .bind(r.latency_ms as i64)
        .bind(r.bytes as i64)
        .bind(serde_json::to_value(r.cache)?.as_str().unwrap_or("miss").to_owned())
        .bind(r.retry_count as i32)
        .bind(r.throttled)
        .bind(r.error_class.as_deref())
        .bind(r.gate_wait_ms as i64)
        .bind(r.payload_sha256.as_deref())
        .execute(&mut *tx)
        .await?;

        if let Some(raw) = &b.raw {
            sqlx::query(
                "INSERT INTO raw_weather_payloads (provider, station_id, endpoint, sha256, first_fetched_at, last_fetched_at, seen_count, status, content_type, body, parser_version)
                 VALUES ($1,$2,$3,$4,$5,$5,1,$6,$7,$8,$9)
                 ON CONFLICT (provider, sha256) DO UPDATE SET last_fetched_at = EXCLUDED.last_fetched_at, seen_count = raw_weather_payloads.seen_count + 1",
            )
            .bind(raw.provider.as_str())
            .bind(raw.station.as_ref().map(|s| s.as_str()))
            .bind(&raw.endpoint)
            .bind(&raw.sha256)
            .bind(raw.fetched_at)
            .bind(i32::from(raw.status))
            .bind(raw.content_type.as_deref())
            .bind(&raw.body)
            .bind(i32::from(raw.parser_version))
            .execute(&mut *tx)
            .await?;
        }

        for (o, class) in &b.observations {
            sqlx::query(
                "INSERT INTO weather_observations (station_id, observed_at, report_type, version, temperature_dc, dewpoint_dc, precision, raw_text, content_hash, provider, provider_receipt_at, fetched_at, parser_version, dedup_class, quality)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
                 ON CONFLICT (station_id, observed_at, report_type, version) DO NOTHING",
            )
            .bind(o.key.station.as_str())
            .bind(o.key.observed_at)
            .bind(o.key.report_type.as_str())
            .bind(o.version as i32)
            .bind(o.temperature.map(TempC::tenths))
            .bind(o.dewpoint.map(TempC::tenths))
            .bind(precision_str(o.precision))
            .bind(&o.raw_text)
            .bind(&o.content_hash)
            .bind(o.provider.as_str())
            .bind(o.provider_receipt_at)
            .bind(o.fetched_at)
            .bind(i32::from(o.parser_version))
            .bind(class.as_str())
            .bind(sqlx::types::Json(&o.quality))
            .execute(&mut *tx)
            .await?;
        }

        for c in &b.corrections {
            sqlx::query(
                "INSERT INTO weather_corrections (station_id, observed_at, report_type, previous_version, current_version, previous_temperature_dc, current_temperature_dc, labeled, detected_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            )
            .bind(c.current.key.station.as_str())
            .bind(c.current.key.observed_at)
            .bind(c.current.key.report_type.as_str())
            .bind(c.previous.version as i32)
            .bind(c.current.version as i32)
            .bind(c.previous.temperature.map(TempC::tenths))
            .bind(c.current.temperature.map(TempC::tenths))
            .bind(c.labeled)
            .bind(c.current.fetched_at)
            .execute(&mut *tx)
            .await?;
        }

        if let Some(h) = &b.health {
            Self::insert_health(&mut tx, h).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn insert_health(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        h: &ProviderHealthEvent,
    ) -> Result<()> {
        sqlx::query("INSERT INTO provider_health_events (provider, station_id, state, previous_state, reason, snapshot, at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(h.snapshot.provider.as_str())
            .bind(h.snapshot.scope.as_ref().map(|s| s.as_str()))
            .bind(h.snapshot.state.as_str())
            .bind(h.previous_state.map(|s| s.as_str()))
            .bind(&h.snapshot.reason)
            .bind(sqlx::types::Json(&h.snapshot))
            .bind(h.snapshot.updated_at)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Latest version of each observation for `station` since `since` (warm start / history).
    pub async fn observations_since(
        &self,
        station: &StationId,
        since: DateTime<Utc>,
    ) -> Result<Vec<Observation>> {
        let rows = sqlx::query(
            "SELECT station_id, observed_at, report_type, version, temperature_dc, dewpoint_dc, precision, raw_text, content_hash, provider, provider_receipt_at, fetched_at, parser_version, quality
             FROM weather_observations_current WHERE station_id = $1 AND observed_at >= $2 ORDER BY observed_at",
        )
        .bind(station.as_str())
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Self::observation_from_row(&r))
            .collect()
    }

    fn observation_from_row(r: &sqlx::postgres::PgRow) -> Result<Observation> {
        let station: String = r.try_get("station_id")?;
        let report: String = r.try_get("report_type")?;
        let precision: String = r.try_get("precision")?;
        let quality: sqlx::types::Json<QualityFlags> = r.try_get("quality")?;
        Ok(Observation {
            key: ObservationKey {
                station: StationId::new(station)
                    .map_err(|e| StorageError::Invalid(e.to_string()))?,
                observed_at: r.try_get("observed_at")?,
                report_type: if report == "SPECI" {
                    ReportType::Speci
                } else {
                    ReportType::Metar
                },
            },
            version: r.try_get::<i32, _>("version")?.max(1) as u32,
            temperature: r
                .try_get::<Option<i32>, _>("temperature_dc")?
                .map(TempC::from_tenths),
            dewpoint: r
                .try_get::<Option<i32>, _>("dewpoint_dc")?
                .map(TempC::from_tenths),
            precision: if precision == "tenth" {
                TempPrecision::Tenth
            } else {
                TempPrecision::WholeDegree
            },
            raw_text: r.try_get("raw_text")?,
            content_hash: r.try_get("content_hash")?,
            provider: ProviderId::new(r.try_get::<String, _>("provider")?)
                .map_err(|e| StorageError::Invalid(e.to_string()))?,
            provider_receipt_at: r.try_get("provider_receipt_at")?,
            fetched_at: r.try_get("fetched_at")?,
            parser_version: r
                .try_get::<i32, _>("parser_version")?
                .clamp(0, i32::from(u16::MAX)) as u16,
            quality: quality.0,
        })
    }

    // -- Journal -------------------------------------------------------------------

    pub async fn record_run(
        &self,
        run: &RunId,
        mode: RunMode,
        model_id: &str,
        config: &serde_json::Value,
        started_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query("INSERT INTO strategy_runs (run_id, mode, started_at, model_id, version, config) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (run_id) DO NOTHING")
            .bind(run.0)
            .bind(mode.as_str())
            .bind(started_at)
            .bind(model_id)
            .bind(wm_core::VERSION)
            .bind(sqlx::types::Json(config))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Append sequenced events (seq must be > 0 and unique per run).
    pub async fn append_events(&self, run: &RunId, events: &[EventEnvelope]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        insert_journal(&mut tx, run, events).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Everything one engine batch produced, in a single transaction: the
    /// journal, decisions, orders, fills and sampled books. A failure rolls
    /// the whole batch back, so the caller can retry it without duplicates.
    pub async fn persist_engine_batch(&self, run: &RunId, b: EngineBatch<'_>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_journal(&mut tx, run, b.events).await?;
        for d in b.decisions {
            insert_decision(&mut tx, run, d).await?;
        }
        for o in b.orders {
            upsert_order_row(&mut tx, run, o).await?;
        }
        for f in b.fills {
            insert_fill(&mut tx, f).await?;
        }
        insert_orderbooks(&mut tx, b.books).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Delete journal order-book updates older than `before`, in chunks.
    /// Recent runs stay exactly replayable; older market history lives on in
    /// the sampled `orderbook_snapshots`. Returns the number of rows deleted.
    pub async fn prune_journal_books(&self, before: DateTime<Utc>) -> Result<u64> {
        let mut total = 0;
        loop {
            let n = sqlx::query(
                "DELETE FROM event_journal WHERE ctid IN (
                   SELECT ctid FROM event_journal
                   WHERE kind = 'order_book_update' AND available_at < $1
                   LIMIT 20000)",
            )
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected();
            total += n;
            if n < 20_000 {
                return Ok(total);
            }
        }
    }

    /// Load a run's journal in sequence order (exact replay / recovery).
    pub async fn load_journal(&self, run: &RunId) -> Result<Vec<EventEnvelope>> {
        let rows = sqlx::query("SELECT seq, available_at, recorded_at, source, payload FROM event_journal WHERE run_id = $1 ORDER BY seq")
            .bind(run.0)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|r| {
                let payload: sqlx::types::Json<wm_core::event::WeatherMachineEvent> =
                    r.try_get("payload")?;
                let source: String = r.try_get("source")?;
                Ok(EventEnvelope {
                    seq: r.try_get::<i64, _>("seq")? as u64,
                    available_at: r.try_get("available_at")?,
                    recorded_at: r.try_get("recorded_at")?,
                    source: match source.as_str() {
                        "live" => EventSource::Live,
                        "replay" => EventSource::Replay,
                        "operator" => EventSource::Operator,
                        _ => EventSource::Synthetic,
                    },
                    event: payload.0,
                })
            })
            .collect()
    }

    pub async fn last_journal_seq(&self, run: &RunId) -> Result<u64> {
        let v: Option<i64> = sqlx::query("SELECT max(seq) FROM event_journal WHERE run_id = $1")
            .bind(run.0)
            .fetch_one(&self.pool)
            .await?
            .try_get(0)?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    // -- Markets ------------------------------------------------------------------

    /// Upsert a market, its outcomes and its verbatim rules (+ raw metadata payload).
    pub async fn upsert_market(
        &self,
        m: &DailyTemperatureMarket,
        raw: Option<&[u8]>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO market_rules (rules_sha256, text, resolution_source_url, parsed_spec) VALUES ($1,$2,$3,$4) ON CONFLICT (rules_sha256) DO NOTHING")
            .bind(&m.rules.sha256)
            .bind(&m.rules.text)
            .bind(m.rules.resolution_source_url.as_deref())
            .bind(sqlx::types::Json(&m.resolution))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO markets (event_slug, event_id, location_id, station_id, local_date, extreme, unit, neg_risk, title, end_time, active, closed, rules_sha256, taker_fee_rate_micros, discovered_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
             ON CONFLICT (event_slug) DO UPDATE SET active = EXCLUDED.active, closed = EXCLUDED.closed, end_time = EXCLUDED.end_time,
               rules_sha256 = EXCLUDED.rules_sha256, title = EXCLUDED.title, updated_at = now()",
        )
        .bind(m.event_slug.as_str())
        .bind(&m.event_id)
        .bind(m.location.as_str())
        .bind(m.station.as_str())
        .bind(m.local_date)
        .bind(serde_json::to_value(m.extreme)?.as_str().unwrap_or("daily_max").to_owned())
        .bind(unit_str(m.unit))
        .bind(m.neg_risk)
        .bind(&m.title)
        .bind(m.end_time)
        .bind(m.active)
        .bind(m.closed)
        .bind(&m.rules.sha256)
        .bind(m.fees.taker_rate_micros as i32)
        .bind(m.discovered_at)
        .execute(&mut *tx)
        .await?;
        for o in &m.outcomes {
            sqlx::query(
                "INSERT INTO market_outcomes (condition_id, event_slug, question_id, label, bucket_lower, bucket_upper, unit, yes_token, no_token, tick_size_micros, min_order_size_micros, accepting_orders, closed)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                 ON CONFLICT (condition_id) DO UPDATE SET accepting_orders = EXCLUDED.accepting_orders, closed = EXCLUDED.closed, tick_size_micros = EXCLUDED.tick_size_micros",
            )
            .bind(o.condition_id.as_str())
            .bind(m.event_slug.as_str())
            .bind(o.question_id.as_ref().map(|q| q.as_str()))
            .bind(&o.label)
            .bind(o.bucket.lower)
            .bind(o.bucket.upper)
            .bind(unit_str(o.bucket.unit))
            .bind(o.yes_token.as_str())
            .bind(o.no_token.as_str())
            .bind(o.tick_size.micros() as i32)
            .bind(o.min_order_size.micros())
            .bind(o.accepting_orders)
            .bind(o.closed)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(body) = raw {
            sqlx::query("INSERT INTO market_snapshots (event_slug, captured_at, sha256, payload) VALUES ($1,$2,$3,$4) ON CONFLICT (event_slug, sha256) DO NOTHING")
                .bind(m.event_slug.as_str())
                .bind(m.discovered_at)
                .bind(wm_core::hash::sha256_hex(body))
                .bind(body)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Human approval of a rules text (by hash). Required for live trading.
    pub async fn approve_rules(&self, sha256: &str, reviewer: &str) -> Result<bool> {
        let r = sqlx::query("UPDATE market_rules SET review_status = 'approved', reviewed_by = $2, reviewed_at = now() WHERE rules_sha256 = $1")
            .bind(sha256)
            .bind(reviewer)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn rules_review_status(&self, sha256: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT review_status FROM market_rules WHERE rules_sha256 = $1")
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => Some(r.try_get("review_status")?),
            None => None,
        })
    }

    pub async fn record_orderbook(&self, b: &OrderBook) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_orderbooks(&mut conn, std::slice::from_ref(b)).await
    }

    // -- Decisions, orders, fills ---------------------------------------------------

    pub async fn record_decisions(&self, run: &RunId, records: &[DecisionRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for d in records {
            insert_decision(&mut tx, run, d).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn upsert_order(&self, run: &RunId, o: &wm_execution_record::OrderRow) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        upsert_order_row(&mut conn, run, o).await
    }

    pub async fn record_fill(&self, f: &Fill) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_fill(&mut conn, f).await
    }

    // -- Statistics -------------------------------------------------------------------

    /// Requests per provider since `since` (for budget dashboards).
    pub async fn request_stats(&self, since: DateTime<Utc>) -> Result<Vec<RequestStats>> {
        let rows = sqlx::query(
            "SELECT provider, count(*) AS requests, count(*) FILTER (WHERE throttled) AS throttled,
                    count(*) FILTER (WHERE error_class IS NOT NULL) AS failures, count(*) FILTER (WHERE cache = 'not_modified') AS not_modified
             FROM provider_requests WHERE requested_at >= $1 GROUP BY provider ORDER BY provider",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(RequestStats {
                    provider: r.try_get("provider")?,
                    requests: r.try_get("requests")?,
                    throttled: r.try_get("throttled")?,
                    failures: r.try_get("failures")?,
                    not_modified: r.try_get("not_modified")?,
                })
            })
            .collect()
    }

    /// Daily maxima per local date from stored observations (research).
    pub async fn stored_observation_count(&self, station: &StationId) -> Result<i64> {
        Ok(
            sqlx::query("SELECT count(*) FROM weather_observations_current WHERE station_id = $1")
                .bind(station.as_str())
                .fetch_one(&self.pool)
                .await?
                .try_get(0)?,
        )
    }

    pub async fn record_system_event(
        &self,
        level: &str,
        kind: &str,
        message: &str,
        details: &serde_json::Value,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO system_events (level, kind, message, details) VALUES ($1,$2,$3,$4)",
        )
        .bind(level)
        .bind(kind)
        .bind(message)
        .bind(sqlx::types::Json(details))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_backtest(
        &self,
        run: &RunId,
        from: NaiveDate,
        to: NaiveDate,
        fidelity: &str,
        parameters: &serde_json::Value,
        metrics: &serde_json::Value,
    ) -> Result<()> {
        sqlx::query("INSERT INTO backtest_runs (run_id, from_date, to_date, fidelity, parameters, metrics) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (run_id) DO UPDATE SET metrics = EXCLUDED.metrics")
            .bind(run.0)
            .bind(from)
            .bind(to)
            .bind(fidelity)
            .bind(sqlx::types::Json(parameters))
            .bind(sqlx::types::Json(metrics))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// One engine persistence unit (see [`PgStore::persist_engine_batch`]).
#[derive(Debug, Clone, Copy)]
pub struct EngineBatch<'a> {
    /// Journal entries (empty when journaling is off).
    pub events: &'a [EventEnvelope],
    pub decisions: &'a [DecisionRecord],
    pub orders: &'a [wm_execution_record::OrderRow],
    pub fills: &'a [Fill],
    pub books: &'a [OrderBook],
}

/// Rows per multi-row statement.
const INSERT_CHUNK: usize = 2_000;

async fn insert_journal(
    conn: &mut PgConnection,
    run: &RunId,
    events: &[EventEnvelope],
) -> Result<()> {
    for chunk in events.chunks(INSERT_CHUNK) {
        let seq: Vec<i64> = chunk.iter().map(|e| e.seq as i64).collect();
        let available: Vec<DateTime<Utc>> = chunk.iter().map(|e| e.available_at).collect();
        let recorded: Vec<DateTime<Utc>> = chunk.iter().map(|e| e.recorded_at).collect();
        let source: Vec<&str> = chunk.iter().map(|e| source_str(e.source)).collect();
        let kind: Vec<&str> = chunk.iter().map(|e| e.event.kind()).collect();
        let payload: Vec<sqlx::types::Json<&wm_core::event::WeatherMachineEvent>> =
            chunk.iter().map(|e| sqlx::types::Json(&e.event)).collect();
        sqlx::query(
            "INSERT INTO event_journal (run_id, seq, available_at, recorded_at, source, kind, payload)
             SELECT $1, u.seq, u.available_at, u.recorded_at, u.source, u.kind, u.payload
             FROM UNNEST($2::bigint[], $3::timestamptz[], $4::timestamptz[], $5::text[], $6::text[], $7::jsonb[])
               AS u(seq, available_at, recorded_at, source, kind, payload)
             ON CONFLICT DO NOTHING",
        )
        .bind(run.0)
        .bind(&seq)
        .bind(&available)
        .bind(&recorded)
        .bind(&source)
        .bind(&kind)
        .bind(&payload)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_orderbooks(conn: &mut PgConnection, books: &[OrderBook]) -> Result<()> {
    for chunk in books.chunks(INSERT_CHUNK) {
        let token: Vec<&str> = chunk.iter().map(|b| b.token.as_str()).collect();
        let captured: Vec<DateTime<Utc>> = chunk.iter().map(|b| b.received_at).collect();
        let exchange_ts: Vec<Option<DateTime<Utc>>> = chunk.iter().map(|b| b.exchange_ts).collect();
        let bid: Vec<Option<i32>> = chunk
            .iter()
            .map(|b| b.best_bid().map(|l| l.price.micros() as i32))
            .collect();
        let ask: Vec<Option<i32>> = chunk
            .iter()
            .map(|b| b.best_ask().map(|l| l.price.micros() as i32))
            .collect();
        let bid_size: Vec<Option<i64>> = chunk
            .iter()
            .map(|b| b.best_bid().map(|l| l.size.micros()))
            .collect();
        let ask_size: Vec<Option<i64>> = chunk
            .iter()
            .map(|b| b.best_ask().map(|l| l.size.micros()))
            .collect();
        let levels: Vec<sqlx::types::Json<serde_json::Value>> = chunk
            .iter()
            .map(|b| sqlx::types::Json(serde_json::json!({ "bids": b.bids, "asks": b.asks })))
            .collect();
        let hash: Vec<Option<&str>> = chunk.iter().map(|b| b.hash.as_deref()).collect();
        sqlx::query(
            "INSERT INTO orderbook_snapshots (token_id, captured_at, exchange_ts, best_bid_micros, best_ask_micros, bid_size_micros, ask_size_micros, levels, hash)
             SELECT * FROM UNNEST($1::text[], $2::timestamptz[], $3::timestamptz[], $4::int4[], $5::int4[], $6::int8[], $7::int8[], $8::jsonb[], $9::text[])",
        )
        .bind(&token)
        .bind(&captured)
        .bind(&exchange_ts)
        .bind(&bid)
        .bind(&ask)
        .bind(&bid_size)
        .bind(&ask_size)
        .bind(&levels)
        .bind(&hash)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_decision(conn: &mut PgConnection, run: &RunId, d: &DecisionRecord) -> Result<()> {
    sqlx::query(
        "INSERT INTO decision_snapshots (run_id, decision_id, strategy, at, location_id, event_slug, summary, inputs, outputs, approved, reasons)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT DO NOTHING",
    )
    .bind(run.0)
    .bind(d.decision_id.0 as i64)
    .bind(d.strategy.as_str())
    .bind(d.at)
    .bind(d.location.as_str())
    .bind(d.event_slug.as_ref().map(|s| s.as_str()))
    .bind(&d.summary)
    .bind(sqlx::types::Json(&d.inputs))
    .bind(sqlx::types::Json(&d.outputs))
    .bind(d.approved)
    .bind(&d.reasons)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn upsert_order_row(
    conn: &mut PgConnection,
    run: &RunId,
    o: &wm_execution_record::OrderRow,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO orders (client_order_id, run_id, decision_id, strategy, location_id, event_slug, token_id, condition_id, outcome_side, side, kind, limit_price_micros, shares_micros, tif, status, filled_micros, avg_price_micros, fees_micros, venue_order_id, reason, created_at, updated_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)
         ON CONFLICT (client_order_id) DO UPDATE SET status = EXCLUDED.status, filled_micros = EXCLUDED.filled_micros, avg_price_micros = EXCLUDED.avg_price_micros,
           fees_micros = EXCLUDED.fees_micros, venue_order_id = EXCLUDED.venue_order_id, reason = EXCLUDED.reason, updated_at = EXCLUDED.updated_at",
    )
    .bind(&o.client_order_id)
    .bind(run.0)
    .bind(o.decision_id)
    .bind(&o.strategy)
    .bind(&o.location)
    .bind(&o.event_slug)
    .bind(&o.token)
    .bind(&o.condition_id)
    .bind(&o.outcome_side)
    .bind(&o.side)
    .bind(&o.kind)
    .bind(o.limit_price_micros)
    .bind(o.shares_micros)
    .bind(sqlx::types::Json(&o.tif))
    .bind(&o.status)
    .bind(o.filled_micros)
    .bind(o.avg_price_micros)
    .bind(o.fees_micros)
    .bind(o.venue_order_id.as_deref())
    .bind(o.reason.as_deref())
    .bind(o.created_at)
    .bind(o.updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn insert_fill(conn: &mut PgConnection, f: &Fill) -> Result<()> {
    sqlx::query("INSERT INTO fills (client_order_id, token_id, side, price_micros, shares_micros, fee_micros, liquidity, ts) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(f.client_order_id.as_str())
        .bind(f.token.as_str())
        .bind(side_str(f.side))
        .bind(f.price.micros() as i32)
        .bind(f.shares.micros())
        .bind(f.fee.micros())
        .bind(match f.liquidity {
            Liquidity::Maker => "maker",
            Liquidity::Taker => "taker",
        })
        .bind(f.ts)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

impl IngestSink for PgStore {
    fn persist(&self, batch: IngestBatch) -> BoxFuture<'_, std::result::Result<(), SinkError>> {
        Box::pin(async move {
            self.persist_ingest(&batch)
                .await
                .map_err(|e| SinkError(e.to_string()))
        })
    }
}

/// Flat order row (decoupled from `wm-execution` to keep this crate's
/// dependency surface small).
pub mod wm_execution_record {
    use chrono::{DateTime, Utc};

    #[derive(Debug, Clone, PartialEq)]
    pub struct OrderRow {
        pub client_order_id: String,
        pub decision_id: i64,
        pub strategy: String,
        pub location: String,
        pub event_slug: String,
        pub token: String,
        pub condition_id: String,
        pub outcome_side: String,
        pub side: String,
        pub kind: String,
        pub limit_price_micros: i32,
        pub shares_micros: i64,
        pub tif: serde_json::Value,
        pub status: String,
        pub filled_micros: i64,
        pub avg_price_micros: Option<i32>,
        pub fees_micros: i64,
        pub venue_order_id: Option<String>,
        pub reason: Option<String>,
        pub created_at: DateTime<Utc>,
        pub updated_at: DateTime<Utc>,
    }
}

/// Helpers to map domain enums to stored text.
pub fn outcome_side_str(s: OutcomeSide) -> &'static str {
    s.as_str()
}

pub fn dedup_class_str(c: DedupClass) -> &'static str {
    c.as_str()
}
