//! Dashboard/API server (axum).
//!
//! | Route                     | Purpose                                              |
//! |---------------------------|------------------------------------------------------|
//! | `GET /api/v1/snapshot`    | current [`DashboardSnapshot`] (JSON)                 |
//! | `GET /api/v1/stream`      | Server-Sent Events: a `snapshot` event per publish   |
//! | `POST /api/v1/kill-switch`| engage/release (header `X-WM-Admin-Token`)           |
//! | `GET /api/v1/report/paper`| `report paper` from the database (`?from=&to=&format=json`) |
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
use axum::body::Body;
use axum::extract::{Query, Request, State};
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
use wm_dashboard_api::{API_VERSION, DashboardSnapshot, KillSwitchRequest};

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
    pub json: Bytes,
    pub published_at: Instant,
}

/// Owner side of the snapshot channel.
pub struct Publisher {
    tx: watch::Sender<Arc<Published>>,
    seq: u64,
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
            json,
            published_at: Instant::now(),
        }));
        (Self { tx, seq: 0 }, rx)
    }

    pub fn publish(&mut self, snapshot: DashboardSnapshot) {
        self.seq += 1;
        match serde_json::to_vec(&snapshot) {
            Ok(json) => {
                self.tx.send_replace(Arc::new(Published {
                    seq: self.seq,
                    snapshot,
                    json: Bytes::from(json),
                    published_at: Instant::now(),
                }));
            }
            Err(e) => tracing::error!(error = %e, "snapshot serialization failed"),
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
}

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
    let events = futures_util::stream::unfold((rx, true), |(mut rx, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let p = Arc::clone(&rx.borrow_and_update());
        let data = String::from_utf8_lossy(&p.json).into_owned();
        let ev = Event::default()
            .event("snapshot")
            .id(p.seq.to_string())
            .data(data);
        Some((Ok::<Event, Infallible>(ev), (rx, false)))
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
