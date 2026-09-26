//! Persistence records for ingestion and the storage port used by collectors.
//!
//! Collectors depend on the [`IngestSink`] trait, not on a database crate. The
//! PostgreSQL implementation lives in `wm-storage`; tests use [`MemoryIngestSink`].

use crate::event::{CorrectionEvent, ProviderHealthEvent};
use crate::ids::{ProviderId, StationId};
use crate::weather::{DedupClass, Observation};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// Boxed `Send` future used by object-safe async ports.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a request interacted with caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutcome {
    /// Network request, full response.
    Miss,
    /// Served from local cache without a network request.
    Hit,
    /// Conditional request answered `304 Not Modified`.
    NotModified,
}

/// Audit row for every outbound request (blueprint §34).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequestRecord {
    pub provider: ProviderId,
    /// Endpoint path and non-secret query (tokens are never recorded).
    pub endpoint: String,
    pub station: Option<StationId>,
    pub requested_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub bytes: u64,
    pub cache: CacheOutcome,
    pub retry_count: u32,
    pub throttled: bool,
    pub error_class: Option<String>,
    /// Time spent waiting for the rate-limit gate before sending.
    pub gate_wait_ms: u64,
    pub payload_sha256: Option<String>,
}

/// Raw provider response, preserved so parser bugs can be fixed retroactively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawPayloadRecord {
    pub provider: ProviderId,
    pub station: Option<StationId>,
    pub endpoint: String,
    pub fetched_at: DateTime<Utc>,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub sha256: String,
    pub parser_version: u16,
}

/// Everything produced by one collector poll, persisted atomically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestBatch {
    pub request: ProviderRequestRecord,
    pub raw: Option<RawPayloadRecord>,
    pub observations: Vec<(Observation, DedupClass)>,
    pub corrections: Vec<CorrectionEvent>,
    pub health: Option<ProviderHealthEvent>,
}

/// Storage error surfaced to collectors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("storage error: {0}")]
pub struct SinkError(pub String);

/// Port through which collectors persist what they ingest.
pub trait IngestSink: Send + Sync {
    fn persist(&self, batch: IngestBatch) -> BoxFuture<'_, Result<(), SinkError>>;
}

/// In-memory sink for tests and `--no-db` proof-of-concept runs.
#[derive(Debug, Default)]
pub struct MemoryIngestSink {
    batches: Mutex<Vec<IngestBatch>>,
}

impl MemoryIngestSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn batches(&self) -> Vec<IngestBatch> {
        self.batches.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub fn len(&self) -> usize {
        self.batches.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl IngestSink for MemoryIngestSink {
    fn persist(&self, batch: IngestBatch) -> BoxFuture<'_, Result<(), SinkError>> {
        self.batches.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(batch);
        Box::pin(async { Ok(()) })
    }
}

/// Sink that discards everything (for pure dry runs).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullIngestSink;

impl IngestSink for NullIngestSink {
    fn persist(&self, _batch: IngestBatch) -> BoxFuture<'_, Result<(), SinkError>> {
        Box::pin(async { Ok(()) })
    }
}
