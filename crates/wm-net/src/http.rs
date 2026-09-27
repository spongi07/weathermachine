//! Rate-limited HTTP fetching with conditional requests, body caps and auditing.

use crate::cache::{CachedResponse, ResponseCache};
use crate::gate::{GatePermit, GateWait, ProviderGate, RequestOutcome, RetryAfter};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use reqwest::header::{
    CONTENT_TYPE, ETAG, HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    LAST_MODIFIED, RETRY_AFTER,
};
use std::sync::Arc;
use std::time::Duration;
use wm_core::hash::sha256_hex;
use wm_core::ids::{ProviderId, StationId};
use wm_core::ingest::{CacheOutcome, ProviderRequestRecord};

/// Construction errors.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("HTTP client build failed: {0}")]
    Client(String),
    #[error("invalid User-Agent: {0}")]
    UserAgent(String),
}

/// One GET request.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub url: String,
    /// Audit label: path and non-secret query only.
    pub endpoint: String,
    pub station: Option<StationId>,
    /// Send `If-None-Match` / `If-Modified-Since` from the previous response.
    pub conditional: bool,
    /// Longest time to wait for the gate before giving up (caller reschedules).
    pub max_gate_wait: Duration,
    pub accept: Option<&'static str>,
}

impl FetchRequest {
    pub fn get(url: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            endpoint: endpoint.into(),
            station: None,
            conditional: true,
            max_gate_wait: Duration::ZERO,
            accept: None,
        }
    }

    pub fn station(mut self, s: StationId) -> Self {
        self.station = Some(s);
        self
    }

    pub fn max_gate_wait(mut self, d: Duration) -> Self {
        self.max_gate_wait = d;
        self
    }

    pub fn accept(mut self, a: &'static str) -> Self {
        self.accept = Some(a);
        self
    }

    pub fn unconditional(mut self) -> Self {
        self.conditional = false;
        self
    }
}

/// Successful (2xx/304) response.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub body: Bytes,
    pub cache: CacheOutcome,
    pub content_type: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    pub record: ProviderRequestRecord,
}

/// Failed fetch. Every variant that reached the network carries its audit record.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("gate closed: {0}")]
    GateClosed(GateWait),
    #[error("throttled (retry after {retry_after:?})")]
    Throttled {
        retry_after: Option<Duration>,
        record: Box<ProviderRequestRecord>,
    },
    #[error("HTTP status {status}{}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    Status {
        status: u16,
        /// Start of a 4xx response body (printable, ≤ 300 chars): APIs
        /// explain rejected parameters there. Never the request URL.
        detail: Option<String>,
        record: Box<ProviderRequestRecord>,
    },
    #[error("request timed out")]
    Timeout { record: Box<ProviderRequestRecord> },
    #[error("connection failed")]
    Connect { record: Box<ProviderRequestRecord> },
    #[error("response body exceeds {limit} bytes")]
    BodyTooLarge {
        limit: usize,
        record: Box<ProviderRequestRecord>,
    },
    #[error("transport error: {detail}")]
    Transport {
        detail: String,
        record: Box<ProviderRequestRecord>,
    },
}

impl FetchError {
    pub fn record(&self) -> Option<&ProviderRequestRecord> {
        match self {
            FetchError::GateClosed(_) => None,
            FetchError::Throttled { record, .. }
            | FetchError::Status { record, .. }
            | FetchError::Timeout { record }
            | FetchError::Connect { record }
            | FetchError::BodyTooLarge { record, .. }
            | FetchError::Transport { record, .. } => Some(record),
        }
    }

    pub fn is_gate_closed(&self) -> bool {
        matches!(self, FetchError::GateClosed(_))
    }

    pub fn is_throttled(&self) -> bool {
        matches!(self, FetchError::Throttled { .. })
    }
}

/// HTTP client bound to exactly one provider gate.
pub struct HttpFetcher {
    provider: ProviderId,
    client: reqwest::Client,
    gate: Arc<ProviderGate>,
    cache: ResponseCache,
    max_body_bytes: usize,
    metric_prefix: &'static str,
}

impl std::fmt::Debug for HttpFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpFetcher")
            .field("provider", &self.provider)
            .finish()
    }
}

impl HttpFetcher {
    pub fn new(gate: Arc<ProviderGate>, user_agent: &str) -> Result<Self, NetError> {
        let policy = gate.policy();
        let ua =
            HeaderValue::from_str(user_agent).map_err(|e| NetError::UserAgent(e.to_string()))?;
        let client = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(policy.connect_timeout)
            .timeout(policy.timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .gzip(true)
            .build()
            .map_err(|e| NetError::Client(e.to_string()))?;
        Ok(Self {
            provider: gate.provider().clone(),
            client,
            gate: Arc::clone(&gate),
            cache: ResponseCache::default(),
            max_body_bytes: policy.max_body_bytes,
            metric_prefix: policy.class.metric_prefix(),
        })
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    pub fn gate(&self) -> &Arc<ProviderGate> {
        &self.gate
    }

    /// Latest cached successful response for `url` — lets internal consumers
    /// reuse data without a new external request.
    pub fn cached(&self, url: &str) -> Option<CachedResponse> {
        self.cache.get(url)
    }

    fn metric(&self, name: &str) -> String {
        format!("{}_{}", self.metric_prefix, name)
    }

    fn count(&self, name: &str) {
        metrics::counter!(self.metric(name), "provider" => self.provider.to_string()).increment(1);
    }

    /// Perform a GET through the gate.
    pub async fn get(&self, req: &FetchRequest) -> Result<FetchResponse, FetchError> {
        let clock = Arc::clone(self.gate.clock());
        let wait_started = clock.monotonic();
        let permit = self
            .gate
            .acquire(req.max_gate_wait)
            .await
            .map_err(FetchError::GateClosed)?;
        let gate_wait = clock.monotonic().saturating_sub(wait_started);
        let requested_at = clock.now();

        let mut headers = HeaderMap::new();
        if let Some(a) = req.accept {
            headers.insert(
                HeaderName::from_static("accept"),
                HeaderValue::from_static(a),
            );
        }
        let cached = if req.conditional {
            self.cache.get(&req.url)
        } else {
            None
        };
        if let Some(c) = &cached {
            if let Some(etag) = c
                .etag
                .as_deref()
                .and_then(|e| HeaderValue::from_str(e).ok())
            {
                headers.insert(IF_NONE_MATCH, etag);
            }
            if let Some(lm) = c
                .last_modified
                .as_deref()
                .and_then(|v| HeaderValue::from_str(v).ok())
            {
                headers.insert(IF_MODIFIED_SINCE, lm);
            }
        }

        self.count("requests_total");
        let started = clock.monotonic();
        let result = self.client.get(&req.url).headers(headers).send().await;
        let mut record = ProviderRequestRecord {
            provider: self.provider.clone(),
            endpoint: req.endpoint.clone(),
            station: req.station.clone(),
            requested_at,
            completed_at: requested_at,
            status: None,
            latency_ms: 0,
            bytes: 0,
            cache: CacheOutcome::Miss,
            retry_count: 0,
            throttled: false,
            error_class: None,
            gate_wait_ms: gate_wait.as_millis() as u64,
            payload_sha256: None,
        };

        let response = match result {
            Ok(r) => r,
            Err(e) => {
                // The URL may carry an API key: never log or store it.
                let e = e.without_url();
                let outcome = if e.is_timeout() {
                    RequestOutcome::Timeout
                } else if e.is_connect() {
                    RequestOutcome::Connect
                } else {
                    RequestOutcome::Other(e.to_string())
                };
                return Err(self.fail(permit, outcome, record, &clock, started, e.to_string()));
            }
        };

        let status = response.status().as_u16();
        record.status = Some(status);
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(RetryAfter::parse)
            .map(|ra| ra.delay_from(clock.now()));

        if status == 304 {
            let Some(c) = cached else {
                return Err(self.fail(
                    permit,
                    RequestOutcome::Other("304 without cached body".into()),
                    record,
                    &clock,
                    started,
                    "304 without cached body".into(),
                ));
            };
            let latency = clock.monotonic().saturating_sub(started);
            permit.complete(RequestOutcome::Success { status });
            self.count("cache_hits");
            self.count("not_modified_total");
            record.completed_at = clock.now();
            record.latency_ms = latency.as_millis() as u64;
            record.cache = CacheOutcome::NotModified;
            record.payload_sha256 = Some(sha256_hex(&c.body));
            self.observe_latency(latency);
            return Ok(FetchResponse {
                status,
                body: c.body.clone(),
                cache: CacheOutcome::NotModified,
                content_type: c.content_type.clone(),
                requested_at,
                fetched_at: record.completed_at,
                record,
            });
        }

        if status == 429 {
            record.throttled = true;
            self.count("429_total");
            let outcome = RequestOutcome::Throttled {
                status,
                retry_after,
            };
            let err_record = self.finish_record(record, &clock, started, "throttled");
            permit.complete(outcome);
            self.count("failures_total");
            tracing::warn!(provider = %self.provider, ?retry_after, "provider throttled us; backing off");
            return Err(FetchError::Throttled {
                retry_after,
                record: Box::new(err_record),
            });
        }

        if !(200..300).contains(&status) {
            let outcome = if status >= 500 {
                RequestOutcome::ServerError {
                    status,
                    retry_after,
                }
            } else {
                RequestOutcome::ClientError { status }
            };
            let detail = if (400..500).contains(&status) {
                error_excerpt(response).await
            } else {
                None
            };
            let err_record = self.finish_record(record, &clock, started, "http_status");
            permit.complete(outcome);
            self.count("failures_total");
            return Err(FetchError::Status {
                status,
                detail,
                record: Box::new(err_record),
            });
        }

        let header_str = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header_str(ETAG);
        let last_modified = header_str(LAST_MODIFIED);
        let content_type = header_str(CONTENT_TYPE);

        let body = match self.read_capped(response).await {
            Ok(b) => b,
            Err(ReadError::TooLarge) => {
                let err_record = self.finish_record(record, &clock, started, "body_too_large");
                permit.complete(RequestOutcome::Other("body too large".into()));
                self.count("failures_total");
                return Err(FetchError::BodyTooLarge {
                    limit: self.max_body_bytes,
                    record: Box::new(err_record),
                });
            }
            Err(ReadError::Transport(detail)) => {
                return Err(self.fail(
                    permit,
                    RequestOutcome::Other(detail.clone()),
                    record,
                    &clock,
                    started,
                    detail,
                ));
            }
        };

        let latency = clock.monotonic().saturating_sub(started);
        permit.complete(RequestOutcome::Success { status });
        record.completed_at = clock.now();
        record.latency_ms = latency.as_millis() as u64;
        record.bytes = body.len() as u64;
        record.payload_sha256 = Some(sha256_hex(&body));
        self.observe_latency(latency);
        self.cache.put(
            &req.url,
            CachedResponse {
                body: body.clone(),
                etag,
                last_modified,
                content_type: content_type.clone(),
                fetched_at: record.completed_at,
            },
        );
        Ok(FetchResponse {
            status,
            body,
            cache: CacheOutcome::Miss,
            content_type,
            requested_at,
            fetched_at: record.completed_at,
            record,
        })
    }

    fn observe_latency(&self, latency: Duration) {
        metrics::histogram!(self.metric("request_latency_seconds"), "provider" => self.provider.to_string())
            .record(latency.as_secs_f64());
    }

    fn finish_record(
        &self,
        mut record: ProviderRequestRecord,
        clock: &Arc<dyn wm_core::time::Clock>,
        started: Duration,
        class: &str,
    ) -> ProviderRequestRecord {
        record.completed_at = clock.now();
        record.latency_ms = clock.monotonic().saturating_sub(started).as_millis() as u64;
        record.error_class = Some(class.to_owned());
        record
    }

    fn fail(
        &self,
        permit: GatePermit,
        outcome: RequestOutcome,
        record: ProviderRequestRecord,
        clock: &Arc<dyn wm_core::time::Clock>,
        started: Duration,
        detail: String,
    ) -> FetchError {
        let class = outcome.class_label();
        let rec = Box::new(self.finish_record(record, clock, started, class));
        let err = match &outcome {
            RequestOutcome::Timeout => FetchError::Timeout { record: rec },
            RequestOutcome::Connect => FetchError::Connect { record: rec },
            _ => FetchError::Transport {
                detail: detail.clone(),
                record: rec,
            },
        };
        permit.complete(outcome);
        self.count("failures_total");
        tracing::warn!(provider = %self.provider, class, %detail, "provider request failed");
        err
    }

    async fn read_capped(&self, mut response: reqwest::Response) -> Result<Bytes, ReadError> {
        if let Some(len) = response.content_length()
            && len as usize > self.max_body_bytes
        {
            return Err(ReadError::TooLarge);
        }
        let mut buf = BytesMut::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len() + chunk.len() > self.max_body_bytes {
                        return Err(ReadError::TooLarge);
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(buf.freeze()),
                Err(e) => return Err(ReadError::Transport(e.to_string())),
            }
        }
    }
}

enum ReadError {
    TooLarge,
    Transport(String),
}

/// First bytes of an error body as one line of printable text.
async fn error_excerpt(mut response: reqwest::Response) -> Option<String> {
    const MAX: usize = 300;
    let mut buf = Vec::new();
    while buf.len() < MAX {
        match response.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c[..c.len().min(MAX - buf.len())]),
            _ => break,
        }
    }
    let text: String = String::from_utf8_lossy(&buf)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}
