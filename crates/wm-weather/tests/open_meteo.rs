#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Open-Meteo Previous Runs client against a mock server.

use chrono::NaiveDate;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::ids::{LocationId, ProviderId};
use wm_core::time::SystemClock;
use wm_core::units::TempC;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_weather::forecast::{ForecastError, ForecastProvider, ForecastQuery};
use wm_weather::{OpenMeteoError, OpenMeteoPreviousRuns};

fn client(uri: &str, key: Option<&str>) -> OpenMeteoPreviousRuns {
    let gate = ProviderGate::new(
        ProviderId::open_meteo(),
        RateLimitPolicy::local_test(),
        Arc::new(SystemClock::new()),
        5,
    );
    let fetcher = HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap();
    OpenMeteoPreviousRuns::new(
        Arc::new(fetcher),
        uri,
        key.map(str::to_owned),
        "gfs_global",
        1,
    )
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn body(times: &[&str], values: &[&str]) -> String {
    format!(
        r#"{{"utc_offset_seconds":0,"timezone":"GMT","hourly_units":{{"time":"iso8601","temperature_2m_previous_day1":"°C"}},"hourly":{{"time":[{}],"temperature_2m_previous_day1":[{}]}}}}"#,
        times
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(","),
        values.join(",")
    )
}

#[tokio::test]
async fn requests_the_fixed_lead_series_in_utc() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .and(query_param("latitude", "52.3156"))
        .and(query_param("longitude", "4.7903"))
        .and(query_param("hourly", "temperature_2m_previous_day1"))
        .and(query_param("models", "gfs_global"))
        .and(query_param("timezone", "GMT"))
        .and(query_param("start_date", "2026-06-30"))
        .and(query_param("end_date", "2026-07-02"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body(
            &["2026-07-01T12:00", "2026-07-01T13:00", "2026-07-01T14:00"],
            &["18.2", "null", "19.46"],
        )))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server.uri(), None);
    let q = ForecastQuery {
        location: LocationId::new("amsterdam").unwrap(),
        latitude: 52.3156,
        longitude: 4.7903,
        local_date: d(2026, 7, 1),
    };
    let e = c.fetch(&q, Duration::from_secs(2)).await.unwrap();
    assert_eq!(e.provider, ProviderId::open_meteo());
    assert_eq!(e.model, "gfs_global");
    assert_eq!(e.lead_days, Some(1));
    assert_eq!(
        e.hourly.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
        vec![TempC::from_tenths(182), TempC::from_tenths(195)],
        "nulls are gaps, never zeros"
    );
    assert_eq!(e.location.as_str(), "amsterdam");
}

#[tokio::test]
async fn an_api_key_goes_to_the_request_but_never_to_the_audit_record() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .and(query_param("apikey", "k3y-SECRET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(body(&["2026-07-01T00:00"], &["10.0"])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server.uri(), Some("k3y-SECRET"));
    let s = c
        .fetch_series(
            52.3,
            4.8,
            d(2026, 7, 1),
            d(2026, 7, 1),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(
        !s.record.endpoint.contains("k3y-SECRET"),
        "{}",
        s.record.endpoint
    );
    assert!(
        s.record
            .endpoint
            .starts_with("/v1/forecast?latitude=52.3000")
    );
    assert!(!format!("{c:?}").contains("k3y-SECRET"));
    // A blank key is no key.
    let blank = client(&server.uri(), Some("  "));
    assert!(format!("{blank:?}").contains("api_key: None"));
}

#[tokio::test]
async fn rejections_explain_themselves() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":true,"reason":"Parameter 'start_date' is out of allowed range from 2021-03-23 to 2026-10-12"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server.uri(), None);
    let err = c
        .fetch_series(
            52.3,
            4.8,
            d(2020, 1, 1),
            d(2020, 12, 31),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
    match &err {
        OpenMeteoError::Rejected {
            reason,
            allowed_from,
            record,
        } => {
            assert!(reason.contains("out of allowed range"));
            assert_eq!(*allowed_from, Some(d(2021, 3, 23)));
            assert_eq!(record.status, Some(400));
        }
        e => panic!("unexpected {e:?}"),
    }
}

#[tokio::test]
async fn an_empty_series_is_unavailable_not_a_forecast() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(body(&["2026-07-01T00:00"], &["null"])),
        )
        .mount(&server)
        .await;
    let c = client(&server.uri(), None);
    let q = ForecastQuery {
        location: LocationId::new("amsterdam").unwrap(),
        latitude: 52.3,
        longitude: 4.8,
        local_date: d(2026, 7, 1),
    };
    assert!(matches!(
        c.fetch(&q, Duration::from_secs(2)).await,
        Err(ForecastError::Unavailable(_))
    ));
}
