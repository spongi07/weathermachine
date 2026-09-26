#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Provider-behaviour scenarios for the station collector (blueprint §36).
//!
//! Real HTTP against a local mock server; time is driven by a `ManualClock`
//! shared by the gate and the collector so every scenario is deterministic.

use chrono::{DateTime, Duration as CDuration, Utc};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::event::{EventEnvelope, WeatherMachineEvent};
use wm_core::health::ProviderHealthState;
use wm_core::ids::{LocationId, ProviderId, StationId};
use wm_core::ingest::MemoryIngestSink;
use wm_core::time::{Clock, ManualClock};
use wm_core::weather::DedupClass;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_weather::{
    AwcMetarSource, CadenceModel, CollectorConfig, CollectorRegistry, HealthConfig,
    ObservationSource, PollOutcome, PollingHints, PollingParams, PollingPolicy, StationCollector,
    TgftpMetarSource,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn awc_json(entries: &[(&str, i64, &str)]) -> String {
    // (raw, obsTime epoch, metarType)
    let items: Vec<String> = entries
        .iter()
        .map(|(raw, t, ty)| format!(r#"{{"icaoId":"EHAM","obsTime":{t},"temp":null,"metarType":"{ty}","rawOb":"{raw}"}}"#))
        .collect();
    format!("[{}]", items.join(","))
}

const T1255: i64 = 1_790_427_300; // 2026-09-26T12:55:00Z
const T1225: i64 = 1_790_425_500; // 2026-09-26T12:25:00Z
const T1325: i64 = 1_790_429_100; // 2026-09-26T13:25:00Z
const T1355: i64 = 1_790_430_900; // 2026-09-26T13:55:00Z

struct Harness {
    collector: StationCollector,
    events: mpsc::Receiver<EventEnvelope>,
    sink: Arc<MemoryIngestSink>,
    clock: ManualClock,
    _hints: watch::Sender<PollingHints>,
}

fn policy() -> RateLimitPolicy {
    let mut p = RateLimitPolicy::local_test();
    p.min_interval = Duration::from_secs(30);
    p.backoff_base = Duration::from_secs(60);
    p.backoff_max = Duration::from_secs(600);
    p.throttle_backoff_base = Duration::from_secs(300);
    p.circuit_failure_threshold = 3;
    p.circuit_open_base = Duration::from_secs(600);
    p.circuit_open_max = Duration::from_secs(3600);
    p.timeout = Duration::from_millis(500);
    // Honour long Retry-After values like the production NWS policy does.
    p.max_retry_after = Duration::from_secs(6 * 3600);
    p
}

fn awc_source(server_uri: &str, clock: &ManualClock, provider: &str) -> Arc<dyn ObservationSource> {
    let gate = ProviderGate::new(
        ProviderId::new(provider).unwrap(),
        policy(),
        Arc::new(clock.clone()),
        11,
    );
    let fetcher =
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap());
    Arc::new(AwcMetarSource::new(fetcher, server_uri, 3))
}

fn harness(sources: Vec<Arc<dyn ObservationSource>>, clock: ManualClock) -> Harness {
    let registry = CollectorRegistry::new();
    let claim = registry.claim(&eham()).unwrap();
    let (tx, rx) = mpsc::channel(256);
    let (hints_tx, hints_rx) = watch::channel(PollingHints::default());
    let sink = Arc::new(MemoryIngestSink::new());
    let cfg = CollectorConfig {
        station: eham(),
        location: LocationId::new("amsterdam").unwrap(),
        timezone: chrono_tz::Europe::Amsterdam,
        policy: PollingPolicy::new(CadenceModel::eham(), PollingParams::default()),
        health: HealthConfig::default(),
        max_gate_wait: Duration::ZERO,
    };
    let collector = StationCollector::new(
        claim,
        cfg,
        sources,
        sink.clone(),
        tx,
        hints_rx,
        Arc::new(clock.clone()),
    );
    Harness {
        collector,
        events: rx,
        sink,
        clock,
        _hints: hints_tx,
    }
}

fn drain(rx: &mut mpsc::Receiver<EventEnvelope>) -> Vec<WeatherMachineEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e.event);
    }
    v
}

fn observation_events(events: &[WeatherMachineEvent]) -> Vec<(DedupClass, i32)> {
    events
        .iter()
        .filter_map(|e| match e {
            WeatherMachineEvent::WeatherObservation(o) => {
                Some((o.class, o.observation.temperature.unwrap().tenths()))
            }
            _ => None,
        })
        .collect()
}

fn health_states(events: &[WeatherMachineEvent]) -> Vec<ProviderHealthState> {
    events
        .iter()
        .filter_map(|e| match e {
            WeatherMachineEvent::ProviderHealthChanged(h) => Some(h.snapshot.state),
            _ => None,
        })
        .collect()
}

async fn mount(server: &MockServer, body: String) {
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/data/metar"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "application/json"),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn http_200_new_observations_are_emitted_and_persisted_with_raw_payload() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[
            (
                "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
                T1255,
                "METAR",
            ),
            (
                "METAR EHAM 261225Z 23011KT 9999 FEW028 17/12 Q1016",
                T1225,
                "METAR",
            ),
        ]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    let out = h.collector.poll_once().await;
    assert!(
        matches!(
            out,
            PollOutcome::Fetched {
                new: 2,
                duplicates: 0,
                ..
            }
        ),
        "{out:?}"
    );
    let events = drain(&mut h.events);
    // Emitted in observation-time order.
    assert_eq!(
        observation_events(&events),
        vec![(DedupClass::New, 170), (DedupClass::New, 180)]
    );
    assert_eq!(health_states(&events), vec![ProviderHealthState::Healthy]);
    let batches = h.sink.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].observations.len(), 2);
    let raw = batches[0].raw.as_ref().unwrap();
    assert!(String::from_utf8_lossy(&raw.body).contains("261255Z"));
    assert_eq!(batches[0].request.status, Some(200));
}

#[tokio::test]
async fn duplicate_response_produces_no_new_observation_events() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    h.collector.poll_once().await;
    drain(&mut h.events);
    h.clock.advance(Duration::from_secs(60));
    let out = h.collector.poll_once().await;
    assert!(
        matches!(
            out,
            PollOutcome::Fetched {
                new: 0,
                duplicates: 1,
                ..
            }
        ),
        "{out:?}"
    );
    let events = drain(&mut h.events);
    assert!(
        observation_events(&events).is_empty(),
        "HTTP poll completed but no new observation ⇒ no WeatherEvent"
    );
}

#[tokio::test]
async fn corrected_observation_emits_correction_and_preserves_versions() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    h.collector.poll_once().await;
    drain(&mut h.events);
    mount(
        &server,
        awc_json(&[(
            "METAR COR EHAM 261255Z 24012KT 9999 FEW030 17/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    h.clock.advance(Duration::from_secs(60));
    let out = h.collector.poll_once().await;
    assert!(
        matches!(
            out,
            PollOutcome::Fetched {
                corrections: 1,
                new: 0,
                ..
            }
        ),
        "{out:?}"
    );
    let events = drain(&mut h.events);
    let corr = events
        .iter()
        .find_map(|e| match e {
            WeatherMachineEvent::WeatherCorrection(c) => Some(c.clone()),
            _ => None,
        })
        .expect("correction event");
    assert!(corr.labeled);
    assert_eq!(corr.previous.temperature.unwrap().tenths(), 180);
    assert_eq!(corr.current.temperature.unwrap().tenths(), 170);
    assert_eq!(corr.current.version, 2);
    let last = h.sink.batches().last().unwrap().clone();
    assert_eq!(last.observations[0].1, DedupClass::Correction);
    assert_eq!(last.corrections.len(), 1);
}

#[tokio::test]
async fn out_of_order_observation_is_flagged() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    h.collector.poll_once().await;
    drain(&mut h.events);
    mount(
        &server,
        awc_json(&[
            (
                "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
                T1255,
                "METAR",
            ),
            (
                "METAR EHAM 261225Z 23011KT 9999 FEW028 17/12 Q1016",
                T1225,
                "METAR",
            ),
        ]),
    )
    .await;
    h.clock.advance(Duration::from_secs(60));
    h.collector.poll_once().await;
    let events = drain(&mut h.events);
    assert_eq!(
        observation_events(&events),
        vec![(DedupClass::OutOfOrder, 170)]
    );
}

#[tokio::test]
async fn http_429_retry_after_throttles_without_retry_storm_or_observation_events() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "900"))
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    let out = h.collector.poll_once().await;
    assert!(
        matches!(
            out,
            PollOutcome::Failed {
                throttled: true,
                ..
            }
        ),
        "{out:?}"
    );
    let events = drain(&mut h.events);
    assert!(observation_events(&events).is_empty());
    assert!(health_states(&events).contains(&ProviderHealthState::Throttled));
    // Repeated attempts inside the Retry-After window never reach the server.
    for _ in 0..5 {
        h.clock.advance(Duration::from_secs(60));
        let out = h.collector.poll_once().await;
        assert!(matches!(out, PollOutcome::GateClosed { .. }), "{out:?}");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    // The plan honours the gate: next poll no earlier than Retry-After.
    let d = h.collector.plan().unwrap();
    assert!(d.at >= utc("2026-09-26T13:13:00Z"), "{:?}", d.at);
    // Audit record of the throttled request was persisted.
    assert!(h.sink.batches().iter().any(|b| b.request.throttled));
}

#[tokio::test]
async fn repeated_http_500_makes_provider_unavailable_and_opens_circuit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    let mut states = Vec::new();
    for _ in 0..3 {
        let out = h.collector.poll_once().await;
        assert!(
            matches!(
                out,
                PollOutcome::Failed {
                    throttled: false,
                    ..
                }
            ),
            "{out:?}"
        );
        states.extend(health_states(&drain(&mut h.events)));
        h.clock.advance(Duration::from_secs(700));
    }
    assert_eq!(states.last(), Some(&ProviderHealthState::Unavailable));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn timeout_and_connection_failures_are_failures_not_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("[]")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(
        vec![awc_source(&server.uri(), &clock, "awc")],
        clock.clone(),
    );
    let out = h.collector.poll_once().await;
    assert!(
        matches!(&out, PollOutcome::Failed { detail, .. } if detail.contains("timed out")),
        "{out:?}"
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut h2 = harness(
        vec![awc_source(
            &format!("http://127.0.0.1:{port}"),
            &clock,
            "awc",
        )],
        clock,
    );
    let out = h2.collector.poll_once().await;
    assert!(matches!(out, PollOutcome::Failed { .. }), "{out:?}");
    assert!(observation_events(&drain(&mut h2.events)).is_empty());
}

#[tokio::test]
async fn malformed_response_is_preserved_and_degrades_health() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    h.collector.poll_once().await;
    drain(&mut h.events);
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"))
        .mount(&server)
        .await;
    h.clock.advance(Duration::from_secs(60));
    let out = h.collector.poll_once().await;
    assert!(matches!(out, PollOutcome::Failed { .. }));
    let events = drain(&mut h.events);
    assert_eq!(health_states(&events), vec![ProviderHealthState::Degraded]);
    let last = h.sink.batches().last().unwrap().clone();
    let raw = last
        .raw
        .expect("raw payload preserved for later reprocessing");
    assert_eq!(&raw.body[..], b"<html>maintenance</html>");
}

#[tokio::test]
async fn long_outage_goes_stale_then_recovers() {
    let server = MockServer::start().await;
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016",
            T1255,
            "METAR",
        )]),
    )
    .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    h.collector.poll_once().await;
    drain(&mut h.events);
    // The feed keeps returning the same (old) report for 50 minutes.
    h.clock.advance(Duration::from_secs(50 * 60));
    h.collector.poll_once().await;
    let events = drain(&mut h.events);
    assert!(health_states(&events).contains(&ProviderHealthState::Stale));
    // A new report arrives: healthy again.
    mount(
        &server,
        awc_json(&[(
            "METAR EHAM 261325Z 24012KT 9999 FEW030 18/12 Q1016",
            T1325,
            "METAR",
        )]),
    )
    .await;
    h.clock.advance(Duration::from_secs(60));
    h.collector.poll_once().await;
    let events = drain(&mut h.events);
    assert!(health_states(&events).contains(&ProviderHealthState::Healthy));
}

#[tokio::test]
async fn circuit_recovers_after_open_period() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        // A report that is fresh at recovery time (13:55; recovery happens ≈14:22).
        .respond_with(ResponseTemplate::new(200).set_body_string(awc_json(&[(
            "METAR EHAM 261355Z 24012KT 9999 FEW030 18/12 Q1016",
            T1355,
            "METAR",
        )])))
        .mount(&server)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let mut h = harness(vec![awc_source(&server.uri(), &clock, "awc")], clock);
    for i in 0..3 {
        if i > 0 {
            h.clock.advance(Duration::from_secs(700));
        }
        let out = h.collector.poll_once().await;
        assert!(matches!(out, PollOutcome::Failed { .. }), "{out:?}");
    }
    // Circuit is open (600 s) right after the third failure: no request is made.
    h.clock.advance(Duration::from_secs(60));
    let out = h.collector.poll_once().await;
    assert!(matches!(out, PollOutcome::GateClosed { .. }), "{out:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    // After the open period a half-open probe succeeds and the provider recovers.
    h.clock.advance(Duration::from_secs(3600));
    let out = h.collector.poll_once().await;
    assert!(
        matches!(out, PollOutcome::Fetched { new: 1, .. }),
        "{out:?}"
    );
    let events = drain(&mut h.events);
    assert!(health_states(&events).contains(&ProviderHealthState::Healthy));
}

#[tokio::test]
async fn fails_over_to_secondary_when_primary_is_throttled() {
    let primary = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .mount(&primary)
        .await;
    let secondary = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/data/observations/metar/stations/EHAM.TXT"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "2026/09/26 12:55\nEHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG\n",
        ))
        .mount(&secondary)
        .await;
    let clock = ManualClock::new(utc("2026-09-26T12:58:00Z"));
    let tg_gate = ProviderGate::new(ProviderId::tgftp(), policy(), Arc::new(clock.clone()), 12);
    let tg_fetcher = Arc::new(
        HttpFetcher::new(tg_gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap(),
    );
    let tg: Arc<dyn ObservationSource> =
        Arc::new(TgftpMetarSource::new(tg_fetcher, secondary.uri()));
    let mut h = harness(vec![awc_source(&primary.uri(), &clock, "awc"), tg], clock);
    let out = h.collector.poll_once().await;
    assert!(matches!(
        out,
        PollOutcome::Failed {
            throttled: true,
            ..
        }
    ));
    h.clock.advance(Duration::from_secs(60));
    let out = h.collector.poll_once().await;
    match out {
        PollOutcome::Fetched {
            provider, new: 1, ..
        } => assert_eq!(provider, ProviderId::tgftp()),
        o => panic!("unexpected {o:?}"),
    }
    let events = drain(&mut h.events);
    let obs = events
        .iter()
        .find_map(|e| match e {
            WeatherMachineEvent::WeatherObservation(o) => Some(o.observation.clone()),
            _ => None,
        })
        .unwrap();
    assert!(obs.quality.from_failover);
    assert_eq!(
        primary.received_requests().await.unwrap().len(),
        1,
        "throttled primary not retried"
    );
}

#[test]
fn second_collector_for_same_station_is_refused() {
    let registry = CollectorRegistry::new();
    let _first = registry.claim(&eham()).unwrap();
    assert!(registry.claim(&eham()).is_err());
}

/// Run the real collector loop for six virtual hours against a scripted source
/// and verify the request count stays within the polite budget.
#[tokio::test(start_paused = true)]
async fn run_loop_request_budget_over_six_virtual_hours() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use wm_core::ingest::BoxFuture;
    use wm_weather::{ParsedReport, SourceError, SourceFetch, metar};

    struct Scripted {
        gate: Arc<ProviderGate>,
        clock: Arc<dyn Clock>,
        calls: Arc<AtomicU32>,
    }
    impl ObservationSource for Scripted {
        fn provider(&self) -> &ProviderId {
            self.gate.provider()
        }
        fn gate(&self) -> &Arc<ProviderGate> {
            &self.gate
        }
        fn fetch<'a>(
            &'a self,
            station: &'a StationId,
            _w: Duration,
        ) -> BoxFuture<'a, Result<SourceFetch, SourceError>> {
            Box::pin(async move {
                let permit = self
                    .gate
                    .try_acquire()
                    .map_err(|w| SourceError::Fetch(wm_net::FetchError::GateClosed(w)))?;
                self.calls.fetch_add(1, Ordering::SeqCst);
                let now = self.clock.now();
                // Newest report published 3 minutes after nominal time.
                let cadence = CadenceModel::eham();
                let mut t = now - CDuration::hours(2);
                while let Some(n) = cadence.next_report_after(t) {
                    if n + CDuration::minutes(3) > now {
                        break;
                    }
                    t = n;
                }
                let raw = format!(
                    "METAR EHAM {} 24012KT 9999 FEW030 18/12 Q1016",
                    t.format("%d%H%MZ")
                );
                let m = metar::parse_metar(&raw).unwrap();
                permit.complete(wm_net::RequestOutcome::Success { status: 200 });
                let request = wm_core::ingest::ProviderRequestRecord {
                    provider: self.provider().clone(),
                    endpoint: "/scripted".into(),
                    station: Some(station.clone()),
                    requested_at: now,
                    completed_at: now,
                    status: Some(200),
                    latency_ms: 1,
                    bytes: raw.len() as u64,
                    cache: wm_core::ingest::CacheOutcome::Miss,
                    retry_count: 0,
                    throttled: false,
                    error_class: None,
                    gate_wait_ms: 0,
                    payload_sha256: None,
                };
                let rawrec = wm_weather::source::raw_record(
                    self.provider(),
                    station,
                    "/scripted",
                    now,
                    200,
                    None,
                    raw.as_bytes(),
                );
                Ok(SourceFetch {
                    reports: vec![ParsedReport {
                        station: station.clone(),
                        observed_at: t,
                        report_type: m.report_type,
                        raw_text: raw.clone(),
                        metar: Some(m),
                        provider_temp_tenths: None,
                        provider_receipt_at: None,
                    }],
                    raw: rawrec,
                    request,
                    cache: wm_core::ingest::CacheOutcome::Miss,
                    warnings: vec![],
                })
            })
        }
    }

    let clock: Arc<dyn Clock> = Arc::new(wm_net::TokioClock::new(utc("2026-09-26T08:00:00Z")));
    let gate = ProviderGate::new(
        ProviderId::awc(),
        RateLimitPolicy::nws_conservative(),
        Arc::clone(&clock),
        5,
    );
    let calls = Arc::new(AtomicU32::new(0));
    let source: Arc<dyn ObservationSource> = Arc::new(Scripted {
        gate,
        clock: Arc::clone(&clock),
        calls: Arc::clone(&calls),
    });
    let registry = CollectorRegistry::new();
    let (tx, mut rx) = mpsc::channel(4096);
    let (_hints_tx, hints_rx) = watch::channel(PollingHints {
        peak_watch: true,
        has_exposure: false,
    });
    let cfg = CollectorConfig {
        station: eham(),
        location: LocationId::new("amsterdam").unwrap(),
        timezone: chrono_tz::Europe::Amsterdam,
        policy: PollingPolicy::new(CadenceModel::eham(), PollingParams::default()),
        health: HealthConfig::default(),
        max_gate_wait: Duration::ZERO,
    };
    let collector = StationCollector::new(
        registry.claim(&eham()).unwrap(),
        cfg,
        vec![source],
        Arc::new(MemoryIngestSink::new()),
        tx,
        hints_rx,
        Arc::clone(&clock),
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = tokio::spawn(collector.run(stop_rx));
    tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    stop_tx.send(true).unwrap();
    handle.await.unwrap();
    let n = calls.load(Ordering::SeqCst);
    // 12 routine reports; peak mode: ≤ ~5 polls/window + slow background.
    assert!(n >= 12, "too few polls: {n}");
    assert!(n <= 120, "too many polls for 6 hours: {n}");
    let mut new_obs = 0;
    while let Ok(e) = rx.try_recv() {
        if let WeatherMachineEvent::WeatherObservation(o) = e.event
            && o.class == DedupClass::New
        {
            new_obs += 1;
        }
    }
    assert!(
        (11..=13).contains(&new_obs),
        "each routine report emitted once, got {new_obs}"
    );
}
