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
        research: Vec::new(),
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
        research: Vec::new(),
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

    // The decision log goes out when it changes, then stays with the client.
    let decision = |id: u64| wm_dashboard_api::DecisionDto {
        id,
        summary: format!("decision {id}"),
        details: vec!["F 20°C YES · ask 0.95 — SIGNAL".into()],
        ..Default::default()
    };
    let mut next_event = async || {
        let chunk = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&chunk).into_owned()
    };
    f.publisher.publish(DashboardSnapshot {
        decisions: vec![decision(1)],
        ..Default::default()
    });
    let text = next_event().await;
    assert!(text.contains("\"summary\":\"decision 1\""), "{text}");
    assert!(!text.contains("decisions_omitted"), "{text}");
    f.publisher.publish(DashboardSnapshot {
        instance: "third".into(),
        decisions: vec![decision(1)],
        ..Default::default()
    });
    let text = next_event().await;
    assert!(
        text.contains("\"instance\":\"third\"")
            && text.contains("\"decisions\":[]")
            && text.contains("\"decisions_omitted\":true"),
        "the client keeps the log it holds: {text}"
    );
    f.publisher.publish(DashboardSnapshot {
        decisions: vec![decision(2), decision(1)],
        ..Default::default()
    });
    let text = next_event().await;
    assert!(
        text.contains("\"summary\":\"decision 2\"") && !text.contains("decisions_omitted"),
        "a new decision sends the log again: {text}"
    );
    // REST always carries the whole log.
    let (_, _, rest) = call(&f.shared, get("/api/v1/snapshot")).await;
    assert!(rest.contains("\"summary\":\"decision 2\""), "{rest}");
}

fn strategies() -> Vec<wm_dashboard_api::StrategyDto> {
    vec![
        wm_dashboard_api::StrategyDto {
            id: "E_book_confirmed_high".into(),
            letter: "E".into(),
            name: "Book-confirmed high".into(),
            enabled: true,
            summary: "E's summary.".into(),
            ..Default::default()
        },
        wm_dashboard_api::StrategyDto {
            id: "F_peak_slot".into(),
            letter: "F".into(),
            name: "Peak slot".into(),
            enabled: true,
            summary: "F's summary.".into(),
            settings: vec![("shares".into(), "100".into())],
            ..Default::default()
        },
    ]
}

#[tokio::test]
async fn each_strategy_has_a_log_to_read_or_download() {
    let mut f = fixture(None, Some(("ops", "pw")));
    f.publisher.publish(DashboardSnapshot {
        engine_time_ms: 1_790_762_400_000, // 2026-09-30 06:40 UTC
        strategies: strategies(),
        ..Default::default()
    });
    // Behind Basic auth like the rest of the dashboard.
    let (status, _, _) = call(&f.shared, get("/api/v1/strategies/F/log")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let authed = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Basic b3BzOnB3")
            .body(Body::empty())
            .unwrap()
    };
    // By letter or id, any case.
    for uri in [
        "/api/v1/strategies/F/log",
        "/api/v1/strategies/f_peak_slot/log",
    ] {
        let (status, headers, body) = call(&f.shared, authed(uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        assert!(headers.get(header::CONTENT_DISPOSITION).is_none());
        assert!(
            body.starts_with("# Strategy F — Peak slot (`F_peak_slot`)"),
            "{body}"
        );
        assert!(body.contains("| shares | 100 |"), "{body}");
        assert!(body.contains("No database"), "{body}");
        assert!(!body.contains("E's summary"), "{body}");
    }
    // As a file to save.
    let (status, headers, _) = call(&f.shared, authed("/api/v1/strategies/E/log?download=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"strategy-E-2026-09-30.md\""
    );
    // Unknown strategies name the known ones.
    let (status, _, body) = call(&f.shared, authed("/api/v1/strategies/Z/log")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("known: E_book_confirmed_high, F_peak_slot"),
        "{body}"
    );
}

#[tokio::test]
async fn research_reports_are_listed_and_served_by_name_only() {
    let dir = std::env::temp_dir().join(format!(
        "wm-research-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("research")).unwrap();
    std::fs::write(
        dir.join("research").join("eham-market.md"),
        "# Model versus market — EHAM\n\n* Strategy F out of sample: …\n",
    )
    .unwrap();
    let (publisher, snapshots) = Publisher::new();
    let (tx, _rx) = mpsc::channel(1);
    let shared = Arc::new(Shared {
        snapshots,
        commands: tx,
        admin_token: None,
        basic_auth: None,
        prometheus: None,
        ui_dir: None,
        ready: Arc::new(AtomicBool::new(true)),
        liveness_max_age: Duration::from_secs(60),
        paper_report: None,
        research: wm_app::http::ResearchFile::defaults(&dir, "EHAM"),
    });
    let (status, _, body) = call(&shared, get("/api/v1/research")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list: Vec<wm_dashboard_api::ResearchReportDto> = serde_json::from_str(&body).unwrap();
    let names: Vec<(&str, bool)> = list
        .iter()
        .map(|r| (r.name.as_str(), r.available))
        .collect();
    assert_eq!(names, [("market", true), ("training", false)]);
    assert!(list[0].bytes > 0 && list[0].modified_ms.is_some());
    assert!(list[0].how.contains("research market"));
    let (status, headers, body) = call(&shared, get("/api/v1/research/market")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
    assert!(body.contains("Strategy F out of sample"));
    let (_, headers, _) = call(&shared, get("/api/v1/research/market?download")).await;
    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"eham-market.md\""
    );
    // Not produced yet: says how.
    let (status, _, body) = call(&shared, get("/api/v1/research/training")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("does not exist yet") && body.contains("model training"),
        "{body}"
    );
    // Only the configured names, never a path.
    for uri in [
        "/api/v1/research/secrets",
        "/api/v1/research/..%2F..%2Fetc%2Fpasswd",
        "/api/v1/research/eham-market.md",
    ] {
        let (status, _, _) = call(&shared, get(uri)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    drop(publisher);
    std::fs::remove_dir_all(&dir).unwrap();
}
