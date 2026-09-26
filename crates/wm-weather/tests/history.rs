#![allow(clippy::unwrap_used, clippy::expect_used)]
//! IEM archive client against a local mock server.

use chrono::{DateTime, NaiveDate, Utc};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::ids::{ProviderId, StationId};
use wm_core::time::ManualClock;
use wm_net::{FetchError, HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_weather::{HistoryError, IemArchive};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn archive(uri: &str, clock: &ManualClock) -> IemArchive {
    let mut p = RateLimitPolicy::local_test();
    p.min_interval = Duration::from_secs(15);
    p.throttle_backoff_base = Duration::from_secs(300);
    p.max_retry_after = Duration::from_secs(6 * 3600);
    let gate = ProviderGate::new(ProviderId::iem(), p, Arc::new(clock.clone()), 7);
    let fetcher =
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap());
    IemArchive::new(fetcher, uri)
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

const CSV: &str = "station,valid,metar\n\
EHAM,2024-07-01 12:25,EHAM 011225Z 24012KT 9999 FEW030 21/12 Q1016 NOSIG\n\
EHAM,2024-07-01 12:55,EHAM 011255Z 24012KT 9999 FEW030 22/12 Q1016 NOSIG\n";

#[tokio::test]
async fn one_station_year_per_request_with_routine_and_specials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cgi-bin/request/asos.py"))
        .and(query_param("station", "EHAM"))
        .and(query_param("data", "metar"))
        .and(query_param("year1", "2024"))
        .and(query_param("month1", "1"))
        .and(query_param("day1", "1"))
        .and(query_param("year2", "2025"))
        .and(query_param("month2", "1"))
        .and(query_param("day2", "1"))
        .and(query_param("tz", "Etc/UTC"))
        .and(query_param("format", "onlycomma"))
        .and(query_param("report_type", "3"))
        .and(query_param("report_type", "4"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CSV))
        .expect(1)
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T15:00:00Z"));
    let iem = archive(&server.uri(), &clock);
    let y = iem
        .fetch_year(
            &StationId::new("EHAM").unwrap(),
            2024,
            d(2025, 1, 1),
            Duration::ZERO,
        )
        .await
        .unwrap();
    assert_eq!(y.rows, 2);
    assert_eq!(y.csv, CSV.as_bytes());
    assert_eq!(y.record.provider, ProviderId::iem());
    assert_eq!(y.record.status, Some(200));
    // The gate spaces requests: an immediate second request is refused locally.
    let again = iem
        .fetch_year(
            &StationId::new("EHAM").unwrap(),
            2023,
            d(2024, 1, 1),
            Duration::ZERO,
        )
        .await;
    assert!(matches!(
        again,
        Err(HistoryError::Fetch(FetchError::GateClosed(_)))
    ));
}

#[tokio::test]
async fn throttling_closes_the_gate_instead_of_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .expect(1)
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T15:00:00Z"));
    let iem = archive(&server.uri(), &clock);
    let st = StationId::new("EHAM").unwrap();
    let first = iem
        .fetch_year(&st, 2024, d(2025, 1, 1), Duration::ZERO)
        .await;
    assert!(matches!(
        first,
        Err(HistoryError::Fetch(FetchError::Throttled { .. }))
    ));
    // Well past IEM's one-second floor but inside Retry-After: still closed.
    clock.advance(Duration::from_secs(60));
    let second = iem
        .fetch_year(&st, 2024, d(2025, 1, 1), Duration::ZERO)
        .await;
    assert!(matches!(
        second,
        Err(HistoryError::Fetch(FetchError::GateClosed(_)))
    ));
}

#[tokio::test]
async fn an_error_page_is_not_mistaken_for_an_empty_year() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Error: backend is busy"))
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T15:00:00Z"));
    let iem = archive(&server.uri(), &clock);
    let r = iem
        .fetch_year(
            &StationId::new("EHAM").unwrap(),
            2024,
            d(2025, 1, 1),
            Duration::ZERO,
        )
        .await;
    assert!(matches!(r, Err(HistoryError::Malformed(_))));
}
