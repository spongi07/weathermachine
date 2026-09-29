#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Dashboard/API server behaviour: auth, operator commands, probes, headers.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tower::ServiceExt;
use wm_app::http::{ADMIN_TOKEN_HEADER, BasicAuth, CSP, Publisher, Shared, router};
use wm_core::event::OperatorCommand;
use wm_dashboard_api::{DashboardSnapshot, KillSwitchRequest};

struct Fixture {
    publisher: Publisher,
    commands: mpsc::Receiver<OperatorCommand>,
    shared: Arc<Shared>,
    ready: Arc<AtomicBool>,
}

fn fixture(admin: Option<&str>, basic: Option<(&str, &str)>) -> Fixture {
    let (publisher, snapshots) = Publisher::new();
    let (tx, commands) = mpsc::channel(8);
    let ready = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        snapshots,
        commands: tx,
        admin_token: admin.map(str::to_owned),
        basic_auth: basic.map(|(u, p)| BasicAuth::new(u, p)),
        prometheus: None,
        ui_dir: None,
        ready: Arc::clone(&ready),
        liveness_max_age: Duration::from_secs(60),
        paper_report: None,
    });
    Fixture {
        publisher,
        commands,
        shared,
        ready,
    }
}

async fn call(
    shared: &Arc<Shared>,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = router(Arc::clone(shared)).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn kill(token: Option<&str>, engaged: bool, reason: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/v1/kill-switch")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(ADMIN_TOKEN_HEADER, t);
    }
    b.body(Body::from(
        serde_json::to_vec(&KillSwitchRequest {
            engaged,
            reason: reason.into(),
        })
        .unwrap(),
    ))
    .unwrap()
}

#[tokio::test]
async fn snapshot_is_served_with_security_headers() {
    let mut f = fixture(None, None);
    f.publisher.publish(DashboardSnapshot {
        mode: "paper".into(),
        instance: "wm-test".into(),
        ..Default::default()
    });
    let (status, headers, body) = call(&f.shared, get("/api/v1/snapshot")).await;
    assert_eq!(status, StatusCode::OK);
    let snap: DashboardSnapshot = serde_json::from_str(&body).unwrap();
    assert_eq!(snap.instance, "wm-test");
    assert_eq!(headers.get(header::CONTENT_SECURITY_POLICY).unwrap(), CSP);
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    // Unknown API routes are JSON 404s, not the SPA shell.
    let (status, _, body) = call(&f.shared, get("/api/v1/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not found"));
    // Without UI assets the root redirects to the zero-JS dashboard.
    let (status, headers, _) = call(&f.shared, get("/")).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/lite");
    let (status, _, html) = call(&f.shared, get("/lite")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("WEATHER MACHINE") && html.contains("wm-test"));
}

#[tokio::test]
async fn kill_switch_requires_the_operator_token() {
    let mut f = fixture(Some("s3cret-token"), None);
    let (status, _, _) = call(&f.shared, kill(None, true, "test")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = call(&f.shared, kill(Some("wrong"), true, "test")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        f.commands.try_recv().is_err(),
        "rejected requests never reach the engine"
    );
    let (status, _, _) = call(&f.shared, kill(Some("s3cret-token"), true, "   ")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "engaging requires a reason"
    );
    let (status, _, _) = call(&f.shared, kill(Some("s3cret-token"), true, "manual halt")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        f.commands.try_recv().unwrap(),
        OperatorCommand::KillSwitch {
            engaged: true,
            reason: "manual halt".into()
        }
    );
    let (status, _, _) = call(&f.shared, kill(Some("s3cret-token"), false, "")).await;
    assert_eq!(status, StatusCode::ACCEPTED, "releasing needs no reason");
    // Malformed body after successful auth.
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/kill-switch")
        .header(ADMIN_TOKEN_HEADER, "s3cret-token")
        .body(Body::from("{nope"))
        .unwrap();
    assert_eq!(call(&f.shared, req).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn kill_switch_is_disabled_without_a_configured_token() {
    let f = fixture(None, None);
    let (status, _, body) = call(&f.shared, kill(Some(""), true, "x")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("WM_ADMIN_TOKEN"));
}

#[tokio::test]
async fn paper_report_needs_the_database_and_basic_auth() {
    let f = fixture(None, None);
    let (status, _, body) = call(&f.shared, get("/api/v1/report/paper")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("needs the database"), "{body}");
    let f = fixture(None, Some(("op", "secret")));
    let (status, ..) = call(&f.shared, get("/api/v1/report/paper")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn basic_auth_protects_everything_but_probes() {
    let mut f = fixture(None, Some(("ops", "pa55")));
    f.publisher.publish(DashboardSnapshot::default());
    let (status, headers, _) = call(&f.shared, get("/api/v1/snapshot")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );
    for uri in ["/lite", "/metrics", "/api/v1/stream"] {
        assert_eq!(
            call(&f.shared, get(uri)).await.0,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    let authed = Request::builder()
        .uri("/api/v1/snapshot")
        .header(header::AUTHORIZATION, "Basic b3BzOnBhNTU=")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&f.shared, authed).await.0, StatusCode::OK);
    let wrong = Request::builder()
        .uri("/api/v1/snapshot")
        .header(header::AUTHORIZATION, "Basic b3BzOndyb25n")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&f.shared, wrong).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&f.shared, get("/healthz")).await.0,
        StatusCode::OK,
        "probes stay open for the orchestrator"
    );
}

#[tokio::test]
async fn probes_reflect_startup_and_engine_liveness() {
    let mut f = fixture(None, None);
    assert_eq!(call(&f.shared, get("/healthz")).await.0, StatusCode::OK);
    let (status, _, body) = call(&f.shared, get("/readyz")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    f.ready.store(true, Ordering::Release);
    f.publisher.publish(DashboardSnapshot {
        storage_ok: true,
        ..Default::default()
    });
    let (status, _, body) = call(&f.shared, get("/readyz")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"ready\":true"));
    // A stalled engine loop fails liveness.
    let (publisher, snapshots) = Publisher::new();
    let (tx, _rx) = mpsc::channel(1);
    let stalled = Arc::new(Shared {
        snapshots,
        commands: tx,
        admin_token: None,
        basic_auth: None,
        prometheus: None,
        ui_dir: None,
        ready: Arc::new(AtomicBool::new(true)),
        liveness_max_age: Duration::ZERO,
        paper_report: None,
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        call(&stalled, get("/healthz")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(publisher);
}

#[tokio::test]
async fn sse_stream_delivers_snapshots() {
    let mut f = fixture(None, None);
    f.publisher.publish(DashboardSnapshot {
        instance: "first".into(),
        ..Default::default()
    });
    let resp = router(Arc::clone(&f.shared))
        .oneshot(get("/api/v1/stream"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");
    assert!(
        resp.headers().get(header::CONTENT_ENCODING).is_none(),
        "SSE must not be compressed/buffered"
    );
    let mut body = resp.into_body().into_data_stream();
    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&first);
    assert!(
        text.contains("event: snapshot") && text.contains("\"instance\":\"first\""),
        "{text}"
    );
    f.publisher.publish(DashboardSnapshot {
        instance: "second".into(),
        ..Default::default()
    });
    let next = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&next).contains("\"instance\":\"second\""));
}
