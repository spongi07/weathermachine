#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Data API v2 trade history against a local server: paging along the
//! cursor, the request's filters, groups of conditions, filtering and
//! de-duplication.

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
        r#"{{"proxy_wallet":"0xw{}","side":"{}","token_id":"1018","condition_id":"0xc1","size":2,"price":0.5,"timestamp":{ts},"outcome":"Yes","transaction_hash":"0xh{i}"}}"#,
        i % 7,
        if i.is_multiple_of(2) { "BUY" } else { "SELL" }
    )
}

/// A v2 page: the rows in `data`, the next page's cursor in `pagination`.
fn page(rows: &[String], next: Option<&str>) -> String {
    format!(
        r#"{{"data":[{}],"pagination":{{"limit":500,"has_more":{},"next_cursor":{}}}}}"#,
        rows.join(","),
        next.is_some(),
        next.map_or_else(|| "null".to_owned(), |c| format!("\"{c}\""))
    )
}

fn query(req: &Request) -> HashMap<String, String> {
    req.url.query_pairs().into_owned().collect()
}

fn num(q: &HashMap<String, String>, k: &str) -> i64 {
    q.get(k).and_then(|v| v.parse().ok()).unwrap()
}

/// A server holding `n` trades seven seconds apart that honours the
/// window and pages newest first along an opaque cursor (base64's `+`, `/`
/// and `=` in it), and answers 400 to v1's parameters like the API.
async fn honest_server(n: usize) -> (MockServer, Arc<AtomicU32>) {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&calls);
    Mock::given(method("GET"))
        .and(path("/v2/trades"))
        .respond_with(move |req: &Request| {
            c.fetch_add(1, Ordering::SeqCst);
            let q = query(req);
            if ["market", "offset", "takerOnly"]
                .iter()
                .any(|k| q.contains_key(*k))
            {
                return ResponseTemplate::new(400).set_body_string("obsolete parameter");
            }
            assert_eq!(q.get("condition").map(String::as_str), Some("0xc1,0xc2"));
            let (start, end, limit) = (num(&q, "start"), num(&q, "end"), num(&q, "limit"));
            let skip: usize = q.get("cursor").map_or(0, |c| {
                c.strip_prefix("p+")
                    .and_then(|c| c.strip_suffix("/=="))
                    .and_then(|c| c.parse().ok())
                    .expect("a cursor this server made")
            });
            let mut rows: Vec<(usize, i64)> = (0..n)
                .map(|i| (i, S0 + 7 * i as i64))
                .filter(|(_, ts)| *ts >= start && *ts <= end)
                .collect();
            rows.reverse(); // newest first
            let total = rows.len();
            let take = usize::try_from(limit).unwrap();
            let rows: Vec<String> = rows
                .into_iter()
                .skip(skip)
                .take(take)
                .map(|(i, ts)| record(i, ts))
                .collect();
            let next = (skip + take < total).then(|| format!("p+{}/==", skip + take));
            ResponseTemplate::new(200).set_body_string(page(&rows, next.as_deref()))
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
async fn pages_along_the_cursor_until_the_last() {
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

/// v1 stopped at an offset of 10,000; a cursor reads on.
#[tokio::test]
async fn reads_past_ten_thousand_along_the_cursor_and_misses_nothing() {
    let (server, _) = honest_server(12_000).await;
    let h = client(&server)
        .trades(&markets(), at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(h.trades.len(), 12_000, "requests {}", h.requests);
    assert!(!h.truncated);
    assert_eq!(h.requests, 24);
    let mut ts: Vec<i64> = h.trades.iter().map(|t| t.at.timestamp()).collect();
    ts.dedup();
    assert_eq!(ts.len(), 12_000);
}

/// At most 20 condition ids a request: more are asked for in groups, each
/// group along its own cursor.
#[tokio::test]
async fn more_than_twenty_markets_are_asked_for_in_groups() {
    let server = MockServer::start().await;
    let groups = Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = Arc::clone(&groups);
    Mock::given(method("GET"))
        .and(path("/v2/trades"))
        .respond_with(move |req: &Request| {
            let q = query(req);
            let ids: Vec<String> = q["condition"].split(',').map(str::to_owned).collect();
            g.lock().unwrap().push(ids.len());
            let row = record(ids.len(), S0 + i64::try_from(ids.len()).unwrap());
            ResponseTemplate::new(200).set_body_string(page(&[row], None))
        })
        .mount(&server)
        .await;
    let many: Vec<ConditionId> = (0..25)
        .map(|i| ConditionId::new(format!("0xc{i}")).unwrap())
        .collect();
    let h = client(&server)
        .trades(&many, at(S0), at(E0), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(*groups.lock().unwrap(), vec![20, 5]);
    assert_eq!((h.requests, h.trades.len()), (2, 2));
}

#[tokio::test]
async fn keeps_only_the_window_and_drops_duplicates() {
    let server = MockServer::start().await;
    let inside = record(1, S0 + 10);
    let outside = record(2, E0 + 100);
    Mock::given(method("GET"))
        .and(path("/v2/trades"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(page(&[inside.clone(), inside, outside], None)),
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
async fn a_cursor_that_never_ends_ends_truncated_within_budget() {
    let server = MockServer::start().await;
    let rows: Vec<String> = (0..500).map(|i| record(i, S0 + 5)).collect();
    let body = page(&rows, Some("again"));
    Mock::given(method("GET"))
        .and(path("/v2/trades"))
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
        .and(path("/v2/trades"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let row = record(3, S0 + 60);
    Mock::given(method("GET"))
        .and(path("/v2/trades"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page(&[row], None)))
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
        .and(path("/v2/trades"))
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
