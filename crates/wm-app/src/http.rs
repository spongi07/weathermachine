//! Dashboard/API server (axum).
//!
//! | Route                     | Purpose                                              |
//! |---------------------------|------------------------------------------------------|
//! | `GET /api/v1/snapshot`    | current [`DashboardSnapshot`] (JSON)                 |
//! | `GET /api/v1/stream`      | Server-Sent Events: a `snapshot` event per publish   |
//! | `POST /api/v1/kill-switch`| engage/release (header `X-WM-Admin-Token`)           |
//! | `GET /api/v1/report/paper`| `report paper` from the database (`?from=&to=&format=json`) |
//! | `GET /api/v1/strategies/{id}/log` | one strategy's log as Markdown (`?days=&download`) |
//! | `GET /api/v1/research`    | the research reports on the data volume (JSON)       |
//! | `GET /api/v1/research/{name}` | one of them as text (`?download`)                |
//! | `GET /healthz`            | liveness: the engine loop is publishing              |
//! | `GET /readyz`             | readiness: startup done, storage reachable           |
//! | `GET /metrics`            | Prometheus exposition                                |
//! | `GET /lite`               | server-rendered zero-JavaScript dashboard            |
//! | everything else           | the Rust/WASM dashboard (`ui/dist`)                  |
//!
//! Snapshots are serialized **once** per publish and shared by every client.
//! Optional HTTP Basic auth (`WM_DASHBOARD_USER`/`WM_DASHBOARD_PASSWORD`)
//! protects everything except the health probes.

use crate::lite;
use crate::paper_report::{self, ReportService};
use crate::strategy_log::{self, History};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use bytes::Bytes;
use metrics_exporter_prometheus::PrometheusHandle;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnFailure, TraceLayer};
use wm_core::event::OperatorCommand;
use wm_dashboard_api::{API_VERSION, DashboardSnapshot, KillSwitchRequest, ResearchReportDto};

/// Content-Security-Policy for every response. WebAssembly needs
/// `'wasm-unsafe-eval'`; no inline scripts are allowed.
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Header carrying the operator token for state-changing endpoints. A custom
/// header (not `Authorization`) so it composes with Basic auth and forces a
/// CORS preflight, which this server never grants (CSRF-safe).
pub const ADMIN_TOKEN_HEADER: &str = "x-wm-admin-token";

/// One published snapshot, serialized once.
#[derive(Debug)]
pub struct Published {
    pub seq: u64,
    pub snapshot: DashboardSnapshot,
    /// The whole snapshot: REST, and a stream client's first event.
    pub json: Bytes,
    /// The same without `decisions` (`decisions_omitted`), for a stream
    /// client that already holds this `decisions_seq`.
    pub json_without_decisions: Bytes,
    pub published_at: Instant,
}

/// Owner side of the snapshot channel.
pub struct Publisher {
    tx: watch::Sender<Arc<Published>>,
    seq: u64,
    decisions_seq: u64,
    decisions_key: u64,
}

/// Decision records never change once made, so their ids name the list.
fn decisions_key(decisions: &[wm_dashboard_api::DecisionDto]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    decisions.len().hash(&mut h);
    for d in decisions {
        d.id.hash(&mut h);
    }
    h.finish()
}

impl Publisher {
    pub fn new() -> (Self, watch::Receiver<Arc<Published>>) {
        let initial = DashboardSnapshot {
            api_version: API_VERSION,
            mode: "starting".into(),
            version: wm_core::VERSION.into(),
            ..Default::default()
        };
        let json = serde_json::to_vec(&initial)
            .map(Bytes::from)
            .unwrap_or_default();
        let (tx, rx) = watch::channel(Arc::new(Published {
            seq: 0,
            snapshot: initial,
            json_without_decisions: json.clone(),
            json,
            published_at: Instant::now(),
        }));
        let publisher = Self {
            tx,
            seq: 0,
            decisions_seq: 0,
            decisions_key: decisions_key(&[]),
        };
        (publisher, rx)
    }

    pub fn publish(&mut self, mut snapshot: DashboardSnapshot) {
        self.seq += 1;
        let key = decisions_key(&snapshot.decisions);
        if key != self.decisions_key {
            self.decisions_key = key;
            self.decisions_seq += 1;
        }
        snapshot.decisions_seq = self.decisions_seq;
        // Once without the decision log, once with it; nothing is copied.
        let decisions = std::mem::take(&mut snapshot.decisions);
        snapshot.decisions_omitted = true;
        let without = serde_json::to_vec(&snapshot);
        snapshot.decisions = decisions;
        snapshot.decisions_omitted = false;
        match (serde_json::to_vec(&snapshot), without) {
            (Ok(json), Ok(without)) => {
                self.tx.send_replace(Arc::new(Published {
                    seq: self.seq,
                    snapshot,
                    json: Bytes::from(json),
                    json_without_decisions: Bytes::from(without),
                    published_at: Instant::now(),
                }));
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!(error = %e, "snapshot serialization failed");
            }
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Published>> {
        self.tx.subscribe()
    }
}

/// Optional HTTP Basic credentials for the dashboard.
#[derive(Clone)]
pub struct BasicAuth {
    expected: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BasicAuth(<redacted>)")
    }
}

impl BasicAuth {
    pub fn new(user: &str, password: &str) -> Self {
        Self {
            expected: format!(
                "Basic {}",
                base64_encode(format!("{user}:{password}").as_bytes())
            ),
        }
    }

    fn accepts(&self, header: Option<&HeaderValue>) -> bool {
        header
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| constant_time_eq(v.as_bytes(), self.expected.as_bytes()))
    }
}

/// Shared server state.
pub struct Shared {
    pub snapshots: watch::Receiver<Arc<Published>>,
    pub commands: mpsc::Sender<OperatorCommand>,
    pub admin_token: Option<String>,
    pub basic_auth: Option<BasicAuth>,
    pub prometheus: Option<PrometheusHandle>,
    pub ui_dir: Option<PathBuf>,
    /// Set once startup (migrations, collectors, engine) completed.
    pub ready: Arc<AtomicBool>,
    /// Liveness fails when no snapshot was published for this long.
    pub liveness_max_age: Duration,
    /// `report paper` over HTTP (`None`: no database).
    pub paper_report: Option<Arc<ReportService>>,
    /// Research reports served by name (`/api/v1/research/{name}`).
    pub research: Vec<ResearchFile>,
}

/// A research report the service can serve from the data volume.
#[derive(Debug, Clone)]
pub struct ResearchFile {
    /// Path segment of its URL.
    pub name: String,
    pub title: String,
    pub path: PathBuf,
    /// How to produce or refresh it.
    pub how: String,
}

impl ResearchFile {
    /// The reports of a station under `<data_dir>/research/`.
    pub fn defaults(data_dir: &std::path::Path, station: &str) -> Vec<Self> {
        let lower = station.to_ascii_lowercase();
        let dir = data_dir.join("research");
        vec![
            Self {
                name: "market".into(),
                title: "Replay at traded prices: model versus market and strategies A–F (research market)".into(),
                path: dir.join(format!("{lower}-market.md")),
                how: "Run `research market --from 2026-06-01 --print` as a one-off container on the data volume (Deployment guide → Model versus market). It writes this file.".into(),
            },
            Self {
                name: "training".into(),
                title: "Model training report: peak survival, forecast evaluation, model structure, when each season's high is first reported".into(),
                path: dir.join(format!("{lower}-survival.md")),
                how: "Written by every model training (automatic on first start and every 30 days).".into(),
            },
        ]
    }
}

/// Research files larger than this are not served.
const MAX_RESEARCH_BYTES: u64 = 16 * 1024 * 1024;

pub type AppState = Arc<Shared>;

/// Constant-time byte comparison (after a length check; lengths are not secret here).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Standard base64 (RFC 4648) encoding, used for the Basic-auth header.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn ui_index(state: &Shared) -> Option<PathBuf> {
    state
        .ui_dir
        .as_ref()
        .map(|d| d.join("index.html"))
        .filter(|p| p.is_file())
}

/// Build the router.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/stream", get(stream))
        .route("/api/v1/kill-switch", post(kill_switch))
        .route("/api/v1/report/paper", get(paper_report_page))
        .route("/api/v1/strategies/{id}/log", get(strategy_log_page))
        .route("/api/v1/research", get(research_list))
        .route("/api/v1/research/{name}", get(research_file))
        .route("/api/{*rest}", any(api_not_found))
        .route("/metrics", get(prometheus_metrics))
        .route("/lite", get(lite_page));
    let protected = match (ui_index(&state), state.ui_dir.clone()) {
        (Some(index), Some(dir)) => protected.fallback_service(
            ServeDir::new(dir)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(index)),
        ),
        _ => protected.fallback(|| async { Redirect::temporary("/lite") }),
    };
    let protected = protected.layer(middleware::from_fn_with_state(
        Arc::clone(&state),
        require_basic_auth,
    ));
    let probes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz));
    probes
        .merge(protected)
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(10),
        ))
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        // Probe 503s during startup are expected; keep them out of the error log.
        .layer(
            TraceLayer::new_for_http()
                .on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)),
        )
}

async fn require_basic_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match &state.basic_auth {
        Some(auth) if !auth.accepts(req.headers().get(header::AUTHORIZATION)) => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"Weather Machine\", charset=\"UTF-8\"",
            )],
            "authentication required",
        )
            .into_response(),
        _ => next.run(req).await,
    }
}

async fn snapshot(State(state): State<AppState>) -> Response {
    let p = Arc::clone(&state.snapshots.borrow());
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from(p.json.clone()),
    )
        .into_response()
}

async fn stream(State(state): State<AppState>) -> impl IntoResponse {
    let rx = state.snapshots.clone();
    // The decision log is most of a snapshot's bytes and changes a few times
    // an hour: a client gets it on connecting and whenever it changes, and
    // keeps it in between (the event says `decisions_omitted`).
    let start = (rx, true, None::<u64>);
    let events = futures_util::stream::unfold(start, |(mut rx, first, held)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let p = Arc::clone(&rx.borrow_and_update());
        let version = p.snapshot.decisions_seq;
        let json = if held == Some(version) {
            &p.json_without_decisions
        } else {
            &p.json
        };
        let data = String::from_utf8_lossy(json).into_owned();
        let ev = Event::default()
            .event("snapshot")
            .id(p.seq.to_string())
            .data(data);
        Some((Ok::<Event, Infallible>(ev), (rx, false, Some(version))))
    });
    (
        [(HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

async fn kill_switch(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = state.admin_token.as_deref() else {
        return (
            StatusCode::FORBIDDEN,
            "operator endpoints are disabled: set WM_ADMIN_TOKEN",
        )
            .into_response();
    };
    let provided = headers
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !constant_time_eq(
        wm_core::hash::sha256_hex(provided.as_bytes()).as_bytes(),
        wm_core::hash::sha256_hex(token.as_bytes()).as_bytes(),
    ) {
        tracing::warn!("kill-switch request with invalid operator token");
        return (StatusCode::UNAUTHORIZED, "invalid operator token").into_response();
    }
    let req: KillSwitchRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid request: {e}")).into_response();
        }
    };
    let reason = req.reason.trim().chars().take(200).collect::<String>();
    if req.engaged && reason.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "a reason is required to engage the kill switch",
        )
            .into_response();
    }
    tracing::warn!(engaged = req.engaged, %reason, "operator kill-switch command");
    match state
        .commands
        .send(OperatorCommand::KillSwitch {
            engaged: req.engaged,
            reason,
        })
        .await
    {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "accepted": true, "engaged": req.engaged })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "engine loop is not running",
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReportQuery {
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
    /// `json` for the JSON report; Markdown otherwise.
    format: Option<String>,
}

/// The paper-run report as Markdown (or JSON), for reading or pasting.
async fn paper_report_page(
    State(state): State<AppState>,
    Query(q): Query<ReportQuery>,
) -> Response {
    let Some(service) = state.paper_report.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the report needs the database, which is not configured\n",
        )
            .into_response();
    };
    match service.report(q.from, q.to).await {
        Ok(r) if q.format.as_deref() == Some("json") => {
            ([(header::CACHE_CONTROL, "no-store")], Json(&*r)).into_response()
        }
        Ok(r) => (
            [
                (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            paper_report::markdown(&r),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "paper report failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("report failed: {e:#}\n"),
            )
                .into_response()
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct TextQuery {
    /// Days of database history (strategy logs; default 7).
    days: Option<usize>,
    /// Present: send as a file to save.
    download: Option<String>,
}

/// Plain text to read or paste; as an attachment named `file` if given.
fn text_response(body: String, file: Option<String>) -> Response {
    let mut resp = (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response();
    if let Some(v) =
        file.and_then(|f| HeaderValue::from_str(&format!("attachment; filename=\"{f}\"")).ok())
    {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

/// One strategy's log: its live state from the latest snapshot and its
/// history from the database, as Markdown for pasting.
async fn strategy_log_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TextQuery>,
) -> Response {
    let published = Arc::clone(&state.snapshots.borrow());
    let snap = &published.snapshot;
    let Some(strategy) = snap
        .strategies
        .iter()
        .find(|s| strategy_log::matches(s, &id))
    else {
        let known: Vec<&str> = snap.strategies.iter().map(|s| s.id.as_str()).collect();
        return (
            StatusCode::NOT_FOUND,
            format!("no strategy '{id}' (known: {})\n", known.join(", ")),
        )
            .into_response();
    };
    let days = q
        .days
        .unwrap_or(7)
        .clamp(1, usize::try_from(ReportService::MAX_DAYS).unwrap_or(31));
    let report = match &state.paper_report {
        Some(service) => Some(service.latest().await),
        None => None,
    };
    let history = match &report {
        None => History::Unavailable,
        Some(Ok(r)) => History::Report { report: r, days },
        Some(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "strategy log history failed");
            History::Failed(format!("{e:#}"))
        }
    };
    let body = strategy_log::render(snap, strategy, history);
    let date = chrono::DateTime::from_timestamp_millis(snap.engine_time_ms)
        .map_or_else(String::new, |t| t.format("-%Y-%m-%d").to_string());
    let file = format!("strategy-{}{date}.md", strategy.letter);
    text_response(body, q.download.is_some().then_some(file))
}

/// The research reports and whether each exists yet.
async fn research_list(State(state): State<AppState>) -> Response {
    let mut list = Vec::with_capacity(state.research.len());
    for f in &state.research {
        let meta = tokio::fs::metadata(&f.path)
            .await
            .ok()
            .filter(std::fs::Metadata::is_file);
        list.push(ResearchReportDto {
            name: f.name.clone(),
            title: f.title.clone(),
            available: meta.is_some(),
            bytes: meta.as_ref().map_or(0, std::fs::Metadata::len),
            modified_ms: meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok()),
            how: f.how.clone(),
        });
    }
    ([(header::CACHE_CONTROL, "no-store")], Json(list)).into_response()
}

/// One research report as text (only the configured files, by name).
async fn research_file(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TextQuery>,
) -> Response {
    let Some(f) = state.research.iter().find(|f| f.name == name) else {
        return (
            StatusCode::NOT_FOUND,
            format!("no research report '{name}'\n"),
        )
            .into_response();
    };
    match tokio::fs::metadata(&f.path).await {
        Ok(m) if m.len() > MAX_RESEARCH_BYTES => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "{} is {} MB, more than the dashboard serves; copy it from the data volume\n",
                    f.path.display(),
                    m.len() / (1024 * 1024)
                ),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                StatusCode::NOT_FOUND,
                format!("{} does not exist yet. {}\n", f.path.display(), f.how),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read {}: {e}\n", f.path.display()),
            )
                .into_response();
        }
    }
    match tokio::fs::read(&f.path).await {
        Ok(bytes) => {
            let file = f.path.file_name().map(|n| n.to_string_lossy().into_owned());
            text_response(
                String::from_utf8_lossy(&bytes).into_owned(),
                q.download.is_some().then_some(file).flatten(),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot read {}: {e}\n", f.path.display()),
        )
            .into_response(),
    }
}

async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not found" })),
    )
        .into_response()
}

fn live(state: &Shared) -> Result<(), String> {
    let age = state.snapshots.borrow().published_at.elapsed();
    if age > state.liveness_max_age {
        return Err(format!(
            "engine loop has not published for {} s",
            age.as_secs()
        ));
    }
    Ok(())
}

async fn healthz(State(state): State<AppState>) -> Response {
    match live(&state) {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    }
}

async fn readyz(State(state): State<AppState>) -> Response {
    let ready = state.ready.load(Ordering::Acquire);
    let p = Arc::clone(&state.snapshots.borrow());
    let liveness = live(&state);
    let body = serde_json::json!({
        "ready": ready && liveness.is_ok(),
        "startup_complete": ready,
        "engine_loop": liveness.as_ref().map_or_else(Clone::clone, |()| "publishing".to_owned()),
        "storage_ok": p.snapshot.storage_ok,
        "kill_switch": p.snapshot.kill_switch,
        "mode": p.snapshot.mode,
    });
    let code = if ready && liveness.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}

async fn prometheus_metrics(State(state): State<AppState>) -> Response {
    match &state.prometheus {
        Some(h) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            h.render(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "metrics recorder not installed").into_response(),
    }
}

async fn lite_page(State(state): State<AppState>) -> Html<String> {
    let p = Arc::clone(&state.snapshots.borrow());
    Html(lite::render(&p.snapshot))
}

/// Serve until `shutdown` flips to true.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let app = router(state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected);
        }
        assert_eq!(
            BasicAuth::new("Aladdin", "open sesame").expected,
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
