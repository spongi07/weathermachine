#![allow(clippy::unwrap_used, clippy::expect_used)]
//! KNMI EDR client against a mock server: the request it sends and the
//! readings it returns.

use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::ids::{ProviderId, StationId};
use wm_core::time::SystemClock;
use wm_core::units::TempC;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_weather::{KnmiError, KnmiTenMinute};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn client(uri: &str) -> KnmiTenMinute {
    let gate = ProviderGate::new(
        ProviderId::knmi(),
        RateLimitPolicy::local_test(),
        Arc::new(SystemClock::new()),
        5,
    );
    let fetcher = HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap();
    KnmiTenMinute::new(Arc::new(fetcher), uri, "test-key", "ta", "tx")
}

const BODY: &str = r#"{"type":"CoverageCollection","coverages":[{"type":"Coverage",
  "domain":{"type":"Domain","domainType":"PointSeries","axes":{"x":{"values":[4.79]},"y":{"values":[52.318]},
  "t":{"values":["2026-07-01T11:30:00Z","2026-07-01T11:40:00Z"]}}},
  "ranges":{"ta":{"type":"NdArray","values":[18.62,18.91]},"tx":{"type":"NdArray","values":[18.8,19.1]}}}]}"#;

#[tokio::test]
async fn the_client_asks_for_one_station_window_with_its_key_in_the_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/locations/0-20000-0-06240"))
        .and(query_param(
            "datetime",
            "2026-07-01T11:00:00Z/2026-07-01T11:45:00Z",
        ))
        .and(query_param("parameter-name", "ta,tx"))
        .and(header("authorization", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(BODY))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server.uri());
    assert!(!format!("{c:?}").contains("test-key"));
    let got = c
        .fetch(
            &StationId::new("EHAM").unwrap(),
            "0-20000-0-06240",
            utc("2026-07-01T11:00:00Z"),
            utc("2026-07-01T11:45:00Z"),
            Duration::from_secs(5),
            1,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].interval_end, utc("2026-07-01T11:40:00Z"));
    assert_eq!(got[1].mean, Some(TempC::from_tenths(189)));
    assert_eq!(got[1].max, Some(TempC::from_tenths(191)));
}

#[tokio::test]
async fn a_rejected_key_is_an_error_not_an_empty_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"message":"Unauthorized"}"#))
        .mount(&server)
        .await;
    let err = client(&server.uri())
        .fetch(
            &StationId::new("EHAM").unwrap(),
            "0-20000-0-06240",
            utc("2026-07-01T11:00:00Z"),
            utc("2026-07-01T11:45:00Z"),
            Duration::from_secs(5),
            1,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, KnmiError::Fetch(_)), "{err}");
    assert!(err.to_string().contains("401"), "{err}");
    assert!(!err.to_string().contains("test-key"));
}

#[tokio::test]
async fn a_series_request_names_its_parameters_and_keeps_the_key_in_the_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/locations/0-20000-0-06215"))
        .and(query_param(
            "datetime",
            "2026-07-01T11:00:00Z/2026-07-01T11:45:00Z",
        ))
        .and(query_param("parameter-name", "ta,qg"))
        .and(header("authorization", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"type":"CoverageCollection","coverages":[{"domain":{"axes":{"t":{"values":["2026-07-01T11:30:00Z","2026-07-01T11:40:00Z"]}}},"ranges":{"ta":{"values":[17.4,null]},"qg":{"values":[540.0,610.0]}}}]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let got = client(&server.uri())
        .fetch_series(
            "0-20000-0-06215",
            &["ta", "qg"],
            utc("2026-07-01T11:00:00Z"),
            utc("2026-07-01T11:45:00Z"),
            Duration::from_secs(5),
            1,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].get("ta"), Some(17.4));
    assert_eq!(got[1].get("ta"), None);
    assert_eq!(got[1].get("qg"), Some(610.0));
}
