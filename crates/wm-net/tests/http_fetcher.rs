#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Provider-behaviour integration tests against a local mock server.

use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::ids::ProviderId;
use wm_core::ingest::CacheOutcome;
use wm_core::time::SystemClock;
use wm_net::{FetchError, FetchRequest, HttpFetcher, ProviderGate, RateLimitPolicy, WaitReason};

fn fetcher(policy: RateLimitPolicy) -> HttpFetcher {
    let gate = ProviderGate::new(
        ProviderId::new("mock").unwrap(),
        policy,
        Arc::new(SystemClock::new()),
        7,
    );
    HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap()
}

fn req(server: &MockServer, p: &str) -> FetchRequest {
    FetchRequest::get(format!("{}{}", server.uri(), p), p.to_owned())
        .max_gate_wait(Duration::from_secs(2))
}

#[tokio::test]
async fn http_200_is_audited_and_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/obs"))
        .and(header(
            "user-agent",
            "WeatherMachine-test/0 (test@example.invalid)",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("EHAM 261255Z 18/12 Q1016")
                .insert_header("etag", "\"v1\""),
        )
        .expect(1)
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let r = f.get(&req(&server, "/obs")).await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.cache, CacheOutcome::Miss);
    assert_eq!(&r.body[..], b"EHAM 261255Z 18/12 Q1016");
    assert_eq!(r.record.status, Some(200));
    assert_eq!(r.record.bytes, 24);
    assert!(r.record.payload_sha256.is_some());
    assert!(f.cached(&format!("{}/obs", server.uri())).is_some());
}

#[tokio::test]
async fn conditional_get_uses_etag_and_304_reuses_cached_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/obs"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/obs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("body-v1")
                .insert_header("etag", "\"v1\""),
        )
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let first = f.get(&req(&server, "/obs")).await.unwrap();
    assert_eq!(first.cache, CacheOutcome::Miss);
    let second = f.get(&req(&server, "/obs")).await.unwrap();
    assert_eq!(second.cache, CacheOutcome::NotModified);
    assert_eq!(&second.body[..], b"body-v1");
    assert_eq!(second.record.cache, CacheOutcome::NotModified);
}

#[tokio::test]
async fn http_429_with_retry_after_closes_the_gate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
        .expect(1) // exactly one request: no retry storm
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let err = f.get(&req(&server, "/obs")).await.unwrap_err();
    match &err {
        FetchError::Throttled {
            retry_after,
            record,
        } => {
            assert_eq!(*retry_after, Some(Duration::from_secs(3)));
            assert!(record.throttled);
            assert_eq!(record.status, Some(429));
        }
        e => panic!("unexpected {e:?}"),
    }
    // An immediate second attempt is refused locally without touching the server.
    let err = f
        .get(&req(&server, "/obs").max_gate_wait(Duration::ZERO))
        .await
        .unwrap_err();
    match err {
        FetchError::GateClosed(w) => {
            assert_eq!(w.reason, WaitReason::RetryAfter);
            assert!(w.retry_in >= Duration::from_millis(2500));
        }
        e => panic!("unexpected {e:?}"),
    }
    assert_eq!(f.gate().stats().throttled_total, 1);
}

#[tokio::test]
async fn http_500_backs_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let err = f.get(&req(&server, "/obs")).await.unwrap_err();
    assert!(matches!(err, FetchError::Status { status: 500, .. }));
    let err = f
        .get(&req(&server, "/obs").max_gate_wait(Duration::ZERO))
        .await
        .unwrap_err();
    match err {
        FetchError::GateClosed(w) => assert_eq!(w.reason, WaitReason::Backoff),
        e => panic!("unexpected {e:?}"),
    }
}

#[tokio::test]
async fn timeout_is_classified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut policy = RateLimitPolicy::local_test();
    policy.timeout = Duration::from_millis(300);
    let f = fetcher(policy);
    let err = f.get(&req(&server, "/slow")).await.unwrap_err();
    assert!(matches!(err, FetchError::Timeout { .. }), "{err:?}");
    assert_eq!(
        err.record().unwrap().error_class.as_deref(),
        Some("timeout")
    );
}

#[tokio::test]
async fn connection_refused_is_classified() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let f = fetcher(RateLimitPolicy::local_test());
    let r = FetchRequest::get(format!("http://127.0.0.1:{port}/x"), "/x")
        .max_gate_wait(Duration::from_secs(1));
    let err = f.get(&r).await.unwrap_err();
    assert!(
        matches!(
            err,
            FetchError::Connect { .. } | FetchError::Transport { .. }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 70_000]))
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test()); // 64 KiB cap
    let err = f.get(&req(&server, "/big")).await.unwrap_err();
    assert!(matches!(err, FetchError::BodyTooLarge { .. }), "{err:?}");
}

#[tokio::test]
async fn circuit_opens_after_repeated_failures_and_recovers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let mut policy = RateLimitPolicy::local_test();
    policy.backoff_base = Duration::from_millis(20);
    policy.backoff_max = Duration::from_millis(40);
    let f = fetcher(policy);
    for _ in 0..3 {
        let err = f.get(&req(&server, "/flaky")).await.unwrap_err();
        assert!(matches!(err, FetchError::Status { status: 503, .. }));
    }
    let err = f
        .get(&req(&server, "/flaky").max_gate_wait(Duration::ZERO))
        .await
        .unwrap_err();
    match err {
        FetchError::GateClosed(w) => assert_eq!(w.reason, WaitReason::CircuitOpen),
        e => panic!("unexpected {e:?}"),
    }
    // After the open period a single half-open probe succeeds and closes the circuit.
    let ok = f
        .get(&req(&server, "/flaky").max_gate_wait(Duration::from_secs(3)))
        .await
        .unwrap();
    assert_eq!(&ok.body[..], b"ok");
    assert_eq!(
        f.gate().stats().circuit,
        wm_core::health::CircuitState::Closed
    );
}

#[tokio::test]
async fn min_interval_is_enforced_between_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("a"))
        .mount(&server)
        .await;
    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::from_millis(400);
    let f = fetcher(policy);
    f.get(&req(&server, "/a").unconditional()).await.unwrap();
    let err = f
        .get(
            &req(&server, "/a")
                .unconditional()
                .max_gate_wait(Duration::ZERO),
        )
        .await
        .unwrap_err();
    match err {
        FetchError::GateClosed(w) => assert_eq!(w.reason, WaitReason::MinInterval),
        e => panic!("unexpected {e:?}"),
    }
    let started = std::time::Instant::now();
    f.get(&req(&server, "/a").unconditional()).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn retry_after_http_date_is_parsed() {
    let server = MockServer::start().await;
    let when = (chrono::Utc::now() + chrono::Duration::seconds(120))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", when.as_str()))
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    match f.get(&req(&server, "/x")).await.unwrap_err() {
        FetchError::Throttled {
            retry_after: Some(d),
            ..
        } => {
            assert!(
                d > Duration::from_secs(100) && d <= Duration::from_secs(121),
                "{d:?}"
            );
        }
        e => panic!("unexpected {e:?}"),
    }
}

#[tokio::test]
async fn concurrent_callers_share_one_gate() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("x")
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;
    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::from_millis(150);
    let f = Arc::new(fetcher(policy));
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..3 {
        let f = Arc::clone(&f);
        let r = req(&server, "/c").unconditional();
        handles.push(tokio::spawn(async move { f.get(&r).await }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    // Three requests with ≥150 ms spacing take at least 300 ms in total.
    assert!(
        started.elapsed() >= Duration::from_millis(290),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn connection_reset_mid_response_is_a_failure_not_data() {
    // The server accepts, reads the request, starts a response and then drops
    // the connection (reset / truncated body). Nothing may be parsed or cached,
    // and the gate must record the failure.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\ncontent-type: application/json\r\n\r\n[{\"partial\":").await;
            drop(s); // abrupt close before the promised 1000 bytes
        }
    });
    let f = fetcher(RateLimitPolicy::local_test());
    let r = FetchRequest::get(format!("http://{addr}/metar"), "/metar")
        .max_gate_wait(Duration::from_secs(1));
    let err = f.get(&r).await.unwrap_err();
    assert!(
        matches!(
            err,
            FetchError::Transport { .. } | FetchError::Connect { .. }
        ),
        "{err:?}"
    );
    let rec = err.record().expect("failed requests are audited");
    // The audit keeps the status line the server really sent, classified as a failure.
    assert!(
        rec.error_class.is_some(),
        "truncated transfer must carry an error class: {rec:?}"
    );
    assert!(
        rec.payload_sha256.is_none(),
        "no payload hash for a broken body"
    );
    assert!(
        f.cached(&format!("http://{addr}/metar")).is_none(),
        "nothing cached from a broken response"
    );
    assert!(f.gate().stats().failures_total >= 1);
}

#[tokio::test]
async fn client_errors_carry_the_reason_but_never_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            "{\"error\":true,\"reason\":\"Parameter 'start_date' is out of allowed range\"}\n",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let url = format!("{}/v1/forecast?apikey=SECRET-KEY-123", server.uri());
    let err = f
        .get(&FetchRequest::get(url, "/v1/forecast").max_gate_wait(Duration::from_secs(2)))
        .await
        .unwrap_err();
    match &err {
        FetchError::Status {
            status: 400,
            detail: Some(d),
            ..
        } => {
            assert!(d.contains("out of allowed range"), "{d}");
        }
        e => panic!("unexpected {e:?}"),
    }
    let text = format!("{err} {err:?}");
    assert!(!text.contains("SECRET-KEY-123"), "{text}");
    assert_eq!(err.record().unwrap().endpoint, "/v1/forecast");

    // Transport errors: reqwest would name the URL; it is stripped.
    let dead = MockServer::start().await;
    let dead_url = format!("{}/v1/forecast?apikey=SECRET-KEY-123", dead.uri());
    drop(dead);
    let f = fetcher(RateLimitPolicy::local_test());
    let err = f
        .get(&FetchRequest::get(dead_url, "/v1/forecast").max_gate_wait(Duration::from_secs(2)))
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("SECRET-KEY-123"), "{text}");
}

#[tokio::test]
async fn retrying_waits_out_a_throttle_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/t"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/t"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let started = std::time::Instant::now();
    let r = f
        .get_retrying(&req(&server, "/t").max_gate_wait(Duration::from_secs(5)), 3)
        .await
        .unwrap();
    assert_eq!(&r.body[..], b"ok");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the gate held the retry for Retry-After: {:?}",
        started.elapsed()
    );
    assert_eq!(f.gate().stats().throttled_total, 1);
}

#[tokio::test]
async fn retrying_stops_after_its_attempts_and_never_repeats_client_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/down"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let mut policy = RateLimitPolicy::local_test();
    policy.circuit_failure_threshold = 100;
    let f = fetcher(policy);
    let wait = Duration::from_secs(10);
    let err = f
        .get_retrying(&req(&server, "/down").max_gate_wait(wait), 3)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Status { status: 503, .. }));
    assert!(err.is_transient());
    let err = f
        .get_retrying(&req(&server, "/missing").max_gate_wait(wait), 3)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Status { status: 404, .. }));
    assert!(!err.is_transient());
    // A gate that stays closed longer than the caller waits is not retried.
    let err = f
        .get_retrying(&req(&server, "/down").max_gate_wait(Duration::ZERO), 3)
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn an_api_key_goes_in_the_authorization_header_and_is_never_printed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/edr"))
        .and(header("authorization", "key-123"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(&server)
        .await;
    let f = fetcher(RateLimitPolicy::local_test());
    let r = req(&server, "/edr").authorization(wm_net::Secret::new("key-123"));
    let printed = format!("{r:?}");
    assert!(!printed.contains("key-123"), "{printed}");
    assert!(printed.contains("<secret>"));
    let resp = f.get(&r).await.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.record.endpoint, "/edr",
        "the audit label carries no key"
    );
}
