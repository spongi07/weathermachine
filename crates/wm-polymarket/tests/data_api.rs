#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Data API trade history against a local server: paging, the offset cap,
//! window splitting, filtering and de-duplication.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use wm_core::ids::{ConditionId, ProviderId};
use wm_core::market::Side;
use wm_core::time::SystemClock;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_polymarket::DataApiClient;

const S0: i64 = 1_790_380_800; // window start (epoch seconds)
const E0: i64 = S0 + 86_400;

fn client(server: &MockServer) -> DataApiClient {
    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::from_millis(1);
    policy.max_body_bytes = 4 * 1024 * 1024;
    policy.circuit_failure_threshold = 1_000;
    let gate = ProviderGate::new(
        ProviderId::polymarket_data(),
        policy,
        Arc::new(SystemClock::new()),
        5,
    );
    let fetcher =
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap());
    DataApiClient::new(fetcher, server.uri())
}

fn record(i: usize, ts: i64) -> String {
    format!(
        r#"{{"proxyWallet":"0xw{}","side":"{}","asset":"1018","conditionId":"0xc1","size":2,"price":0.5,"timestamp":{ts},"outcome":"Yes","transactionHash":"0xh{i}"}}"#,
        i % 7,
        if i.is_multiple_of(2) { "BUY" } else { "SELL" }
    )
}

fn query(req: &Request) -> HashMap<String, String> {
    req.url.query_pairs().into_owned().collect()
}

fn num(q: &HashMap<String, String>, k: &str) -> i64 {
    q.get(k).and_then(|v| v.parse().ok()).unwrap()
}

/// A server holding `n` trades seven seconds apart that honours the
/// window, pages newest first and rejects offsets above 10,000 like the API.
async fn honest_server(n: usize) -> (MockServer, Arc<AtomicU32>) {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&calls);
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(move |req: &Request| {
            c.fetch_add(1, Ordering::SeqCst);
            let q = query(req);
            assert_eq!(q.get("takerOnly").map(String::as_str), Some("true"));
            assert_eq!(q.get("market").map(String::as_str), Some("0xc1,0xc2"));
            let (start, end, offset, limit) = (
                num(&q, "start"),
                num(&q, "end"),
                num(&q, "offset"),
                num(&q, "limit"),
            );
            if offset > 10_000 || limit > 10_000 {
                return ResponseTemplate::new(400).set_body_string("offset too large");
            }
            let mut rows: Vec<(usize, i64)> = (0..n)
                .map(|i| (i, S0 + 7 * i as i64))
                .filter(|(_, ts)| *ts >= start && *ts <= end)
                .collect();
            rows.reverse(); // newest first
            let page: Vec<String> = rows
                .into_iter()
                .skip(usize::try_from(offset).unwrap())
                .take(usize::try_from(limit).unwrap())
                .map(|(i, ts)| record(i, ts))
                .collect();
            ResponseTemplate::new(200).set_body_string(format!("[{}]", page.join(",")))
        })
        .mount(&server)
        .await;
    (server, calls)
}

fn at(ts: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(ts, 0).unwrap()
}

fn markets() -> Vec<ConditionId> {
    vec![
        ConditionId::new("0xc1").unwrap(),
        ConditionId::new("0xc2").unwrap(),
    ]
}

#[tokio::test]
async fn pages_until_a_short_page() {
    let (server, calls) = honest_server(503).await;
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.trades.len(), 503);
    assert_eq!((h.requests, calls.load(Ordering::SeqCst)), (2, 2));
    assert!(!h.truncated && h.skipped == 0);
    assert!(
        h.trades.windows(2).all(|w| w[0].at < w[1].at),
        "oldest first"
    );
    assert_eq!(h.trades[0].at.timestamp(), S0);
    assert_eq!(h.trades[0].side, Side::Buy);
    assert_eq!(h.trades[1].side, Side::Sell);
    assert_eq!(h.trades[0].taker.as_deref(), Some("0xw0"));
}

#[tokio::test]
async fn splits_the_window_at_the_offset_cap_and_misses_nothing() {
    let (server, _) = honest_server(12_000).await;
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.trades.len(), 12_000, "requests {}", h.requests);
    assert!(!h.truncated);
    // 21 pages hit the cap, then the two halves are read.
    assert!(h.requests > 21 && h.requests < 60, "{}", h.requests);
    let mut ts: Vec<i64> = h.trades.iter().map(|t| t.at.timestamp()).collect();
    ts.dedup();
    assert_eq!(ts.len(), 12_000);
}

#[tokio::test]
async fn keeps_only_the_window_and_drops_duplicates() {
    let server = MockServer::start().await;
    let inside = record(1, S0 + 10);
    let outside = record(2, E0 + 100);
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("[{inside},{inside},{outside}]")),
        )
        .mount(&server)
        .await;
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.trades.len(), 1);
    assert_eq!(h.trades[0].at.timestamp(), S0 + 10);
    assert_eq!(h.requests, 1);
}

#[tokio::test]
async fn an_api_ignoring_the_window_ends_truncated_within_budget() {
    let server = MockServer::start().await;
    let page: Vec<String> = (0..500).map(|i| record(i, S0 + 5)).collect();
    let body = format!("[{}]", page.join(","));
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(h.truncated);
    assert_eq!(h.requests, 100);
    assert_eq!(h.trades.len(), 500, "duplicates collapse");
}

#[tokio::test]
async fn nothing_to_ask_for_makes_no_request() {
    let server = MockServer::start().await;
    let h = client(&server)
        .trades(&[], at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!((h.requests, h.trades.len()), (0, 0));
    let h = client(&server)
        .trades(&markets(), at(E0), at(S0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.requests, 0);
}

#[tokio::test]
async fn a_throttled_page_is_waited_out_and_read_again() {
    // 8 June 2026: a single 429 with Retry-After 1 s ended a whole study.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let row = record(3, S0 + 60);
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("[{row}]")))
        .expect(1)
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        (h.trades.len(), h.requests),
        (1, 1),
        "one page, two attempts"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "Retry-After honoured: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_page_that_keeps_failing_gives_up_after_its_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(ResponseTemplate::new(502))
        .expect(u64::from(wm_polymarket::data::PAGE_ATTEMPTS))
        .mount(&server)
        .await;
    let err = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(30))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("502"), "{err}");
}
