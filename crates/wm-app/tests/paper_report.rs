#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `report paper` against PostgreSQL: one day of a paper run (29 Sep 2026,
//! modelled on the real one) is written with the service's own storage
//! calls, then read back into the report. Set `WM_TEST_DATABASE_URL` (a role
//! with CREATEDB); the test is skipped otherwise.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use wm_app::paper_report::{self, ReportTarget};
use wm_core::event::{EventEnvelope, EventSource, ForecastEvent, WeatherMachineEvent};
use wm_core::ids::{
    ClientOrderId, DecisionId, LocationId, ProviderId, RunId, StationId, StrategyId, TokenId,
};
use wm_core::ingest::{CacheOutcome, IngestBatch, ProviderRequestRecord};
use wm_core::market::{BookLevel, OrderBook, Side};
use wm_core::synthetic::synthetic_temperature_market;
use wm_core::trading::{DecisionRecord, Fill, Liquidity, RunMode};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_storage::PgStore;

struct TestDb {
    store: PgStore,
    admin_url: String,
    name: String,
}

impl TestDb {
    async fn drop_db(self) {
        self.store.pool().close().await;
        if let Ok(admin) = PgStore::connect(&self.admin_url, 2).await {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            )))
            .execute(admin.pool())
            .await;
        }
    }
}

async fn fresh() -> Option<TestDb> {
    let Ok(url) = std::env::var("WM_TEST_DATABASE_URL") else {
        eprintln!("WM_TEST_DATABASE_URL not set; skipping PostgreSQL report test");
        return None;
    };
    let admin = PgStore::connect(&url, 2).await.expect("connect admin");
    let name = format!(
        "wm_report_{:x}{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::process::id()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(admin.pool())
        .await
        .expect("create db");
    admin.pool().close().await;
    let (base, _) = url.rsplit_once('/').expect("url has database path");
    let store = PgStore::connect(&format!("{base}/{name}"), 5)
        .await
        .expect("connect test db");
    store.migrate().await.expect("migrate");
    Some(TestDb {
        store,
        admin_url: url,
        name,
    })
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

/// A report observed at `t` (UTC), first fetched `delay_s` later from `provider`.
fn ingest(t: DateTime<Utc>, whole: i32, delay_s: i64, provider: ProviderId) -> IngestBatch {
    let raw = format!(
        "METAR EHAM {} 24010KT 9999 FEW030 {whole:02}/12 Q1016",
        t.format("%d%H%MZ")
    );
    let fetched = t + Duration::seconds(delay_s);
    IngestBatch {
        request: ProviderRequestRecord {
            provider: provider.clone(),
            endpoint: "/metar".into(),
            station: Some(eham()),
            requested_at: fetched,
            completed_at: fetched,
            status: Some(200),
            latency_ms: 120,
            bytes: raw.len() as u64,
            cache: CacheOutcome::Miss,
            retry_count: 0,
            throttled: false,
            error_class: None,
            gate_wait_ms: 0,
            payload_sha256: None,
        },
        raw: None,
        observations: vec![(
            Observation {
                key: ObservationKey {
                    station: eham(),
                    observed_at: t,
                    report_type: ReportType::Metar,
                },
                version: 1,
                temperature: Some(TempC::from_whole(whole)),
                dewpoint: Some(TempC::from_whole(12)),
                precision: TempPrecision::WholeDegree,
                content_hash: wm_core::hash::sha256_hex(raw.as_bytes()),
                raw_text: raw,
                provider,
                provider_receipt_at: None,
                fetched_at: fetched,
                parser_version: 1,
                quality: QualityFlags::default(),
            },
            DedupClass::New,
        )],
        corrections: vec![],
        health: None,
    }
}

fn evaluation(
    id: u64,
    at: &str,
    slug: &str,
    high: i32,
    p: &[f64],
    lines: &[&str],
) -> DecisionRecord {
    DecisionRecord {
        decision_id: DecisionId(id),
        strategy: StrategyId::from_static("evaluation"),
        at: utc(at),
        location: LocationId::new("amsterdam").unwrap(),
        event_slug: Some(wm_core::ids::EventSlug::new(slug).unwrap()),
        summary: "evaluated".into(),
        inputs: serde_json::json!({ "views": [
            { "view": "all", "high_whole": high, "minutes_since_high": 0, "drop_tenths": 0,
              "trajectory": "rising", "p": p }
        ]}),
        outputs: serde_json::json!({ "evaluations": lines }),
        approved: false,
        reasons: vec![],
    }
}

fn book(token: &str, at: &str, bid: &str, ask: &str) -> OrderBook {
    OrderBook {
        token: TokenId::new(token).unwrap(),
        bids: vec![BookLevel {
            price: Price::parse(bid).unwrap(),
            size: Shares::from_whole(100),
        }],
        asks: vec![BookLevel {
            price: Price::parse(ask).unwrap(),
            size: Shares::from_whole(100),
        }],
        tick_size: Price::parse("0.01").unwrap(),
        min_order_size: Shares::from_whole(5),
        exchange_ts: None,
        received_at: utc(at),
        hash: None,
        confirmed_at: None,
    }
}

/// The dashboard router serving only the report.
fn dashboard(service: paper_report::ReportService) -> axum::Router {
    let (_publisher, snapshots) = wm_app::http::Publisher::new();
    let (commands, _) = tokio::sync::mpsc::channel(1);
    wm_app::http::router(std::sync::Arc::new(wm_app::http::Shared {
        snapshots,
        commands,
        admin_token: None,
        basic_auth: None,
        prometheus: None,
        ui_dir: None,
        ready: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        liveness_max_age: std::time::Duration::from_secs(60),
        paper_report: Some(std::sync::Arc::new(service)),
    }))
}

async fn get(app: &axum::Router, uri: &str) -> (u16, String, String) {
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, ctype, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn paper_report_reads_a_day_back_from_the_database() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    let tz = chrono_tz::Europe::Amsterdam;
    let date = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
    let run = RunId::deterministic(29);
    s.record_run(
        &run,
        RunMode::Paper,
        "eham-test-model",
        &serde_json::json!({}),
        utc("2026-09-29T05:55:00Z"),
    )
    .await
    .unwrap();

    // Weather: a report from before the start (caught up), then every half
    // hour 05:55–17:55 UTC. 25 °C first at 13:55 UTC (15:55 local). The
    // 10:25 report reached us at +660 s; tgftp delivered 11:55 first.
    s.persist_ingest(&ingest(
        utc("2026-09-29T03:25:00Z"),
        14,
        2 * 3600 + 31 * 60,
        ProviderId::awc(),
    ))
    .await
    .unwrap();
    let temps = [
        17, 17, 18, 19, 19, 20, 21, 21, 22, 22, 23, 23, 23, 23, 24, 24, 25, 24, 25, 25, 25, 24, 24,
        23, 22,
    ];
    let mut t = utc("2026-09-29T05:55:00Z");
    for whole in temps {
        let (delay, provider) = match t.format("%H:%M").to_string().as_str() {
            "10:25" => (660, ProviderId::awc()),
            "11:55" => (150, ProviderId::tgftp()),
            _ => (180, ProviderId::awc()),
        };
        s.persist_ingest(&ingest(t, whole, delay, provider))
            .await
            .unwrap();
        t += Duration::minutes(30);
    }

    // The market and the recorded book of the bucket that won (25 °C).
    let m = synthetic_temperature_market(
        &LocationId::new("amsterdam").unwrap(),
        &eham(),
        date,
        tz,
        17,
        27,
        utc("2026-09-28T20:00:00Z"),
    );
    s.upsert_market(&m, None).await.unwrap();
    let slug = m.event_slug.to_string();
    let yes25 = "syn-2026-09-29-025-yes";
    for (at, bid, ask) in [
        ("2026-09-29T12:20:00Z", "0.01", "0.02"),
        ("2026-09-29T13:58:00Z", "0.73", "0.76"),
        ("2026-09-29T14:55:00Z", "0.92", "0.94"),
        ("2026-09-29T16:25:00Z", "0.98", "0.99"),
    ] {
        s.record_orderbook(&book(yes25, at, bid, ask))
            .await
            .unwrap();
    }

    // Day-1 forecast: hourly for the whole local day, maximum 26.3 °C.
    let start = utc("2026-09-28T22:00:00Z");
    let hourly: Vec<(DateTime<Utc>, TempC)> = (0..=24_i32)
        .map(|h| {
            let v = if h == 15 { 263 } else { 150 + h * 4 };
            (start + Duration::hours(i64::from(h)), TempC::from_tenths(v))
        })
        .collect();
    let mut env = EventEnvelope::new(
        utc("2026-09-29T05:00:00Z"),
        EventSource::Live,
        WeatherMachineEvent::ForecastUpdate(ForecastEvent {
            location: LocationId::new("amsterdam").unwrap(),
            provider: ProviderId::open_meteo(),
            model: "gfs_global".into(),
            issued_at: utc("2026-09-29T05:00:00Z"),
            predicted_max: None,
            hourly,
            lead_days: Some(1),
        }),
    );
    env.seq = 1;
    s.append_events(&run, &[env]).await.unwrap();

    // Evaluations (one per weather report) and one rejected proposal.
    let records = vec![
        evaluation(
            1,
            "2026-09-29T12:28:00Z",
            &slug,
            23,
            &[0.39, 0.30, 0.20, 0.11],
            &[
                "A 23°C YES · ask 0.01 · p 0.390 (market 0.015) · EV +0.3700 — confirmation 3m < 60m; ask 0.01 outside [0.90, 0.99]",
                "B 26°C NO · ask 0.60 · p 0.955 (model 0.970, market 0.400) · EV +0.3400 — confirmation 3m < 60m",
            ],
        ),
        evaluation(
            2,
            "2026-09-29T13:59:00Z",
            &slug,
            25,
            &[0.76, 0.20, 0.04, 0.0],
            &[
                "A 25°C YES · ask 0.76 · p 0.760 (market 0.745) · EV -0.0100 — confirmation 0m < 60m; ask 0.76 outside [0.90, 0.99]",
                "E 25°C YES · ask 0.76 · p 0.760 (market 0.745) · EV -0.0100 — only -0 shares offered ≤ 0.99 15m ago",
            ],
        ),
        evaluation(
            3,
            "2026-09-29T14:58:00Z",
            &slug,
            25,
            &[0.895, 0.09, 0.015, 0.0],
            &[
                "A 25°C YES · ask 0.94 · p 0.895 (market 0.925) · EV -0.0529 — confirmation 0m < 60m",
            ],
        ),
        evaluation(
            4,
            "2026-09-29T16:28:00Z",
            &slug,
            25,
            &[0.95, 0.05, 0.0, 0.0],
            &[
                "A 25°C YES · ask 0.99 · p 0.950 (market 0.985) · EV -0.0405 — ask 0.99 outside [0.90, 0.98]",
            ],
        ),
        DecisionRecord {
            decision_id: DecisionId(5),
            strategy: StrategyId::from_static("E_book_confirmed_high"),
            at: utc("2026-09-29T15:10:00Z"),
            location: LocationId::new("amsterdam").unwrap(),
            event_slug: Some(m.event_slug.clone()),
            summary: "E_book_confirmed_high BUY YES 25°C @ 0.94 ×10 — APPROVED".into(),
            inputs: serde_json::json!({}),
            outputs: serde_json::json!({ "approved": true, "reasons": [] }),
            approved: true,
            reasons: vec![],
        },
    ];
    s.record_decisions(&run, &records).await.unwrap();

    // The paper trade: 10 YES at 0.94, taker fee 0.05 × p × (1 − p).
    s.upsert_order(
        &run,
        &wm_storage::wm_execution_record::OrderRow {
            client_order_id: "wm-e-1".into(),
            decision_id: 5,
            strategy: "E_book_confirmed_high".into(),
            location: "amsterdam".into(),
            event_slug: slug.clone(),
            token: yes25.into(),
            condition_id: "0xc25".into(),
            outcome_side: "YES".into(),
            side: "BUY".into(),
            kind: "open".into(),
            limit_price_micros: 940_000,
            shares_micros: 10_000_000,
            tif: serde_json::json!({"tif": "fak"}),
            status: "filled".into(),
            filled_micros: 10_000_000,
            avg_price_micros: Some(940_000),
            fees_micros: 28_200,
            venue_order_id: None,
            reason: None,
            created_at: utc("2026-09-29T15:10:00Z"),
            updated_at: utc("2026-09-29T15:10:01Z"),
        },
    )
    .await
    .unwrap();
    s.record_fill(&Fill {
        client_order_id: ClientOrderId::new("wm-e-1").unwrap(),
        token: TokenId::new(yes25).unwrap(),
        side: Side::Buy,
        price: Price::parse("0.94").unwrap(),
        shares: Shares::from_whole(10),
        fee: Usd::parse("0.0282").unwrap(),
        liquidity: Liquidity::Taker,
        ts: utc("2026-09-29T15:10:01Z"),
    })
    .await
    .unwrap();
    s.record_system_event(
        "warning",
        "market_stream",
        "reconnected",
        &serde_json::json!({}),
    )
    .await
    .unwrap();

    // Build the report the next morning.
    let now = utc("2026-09-30T08:00:00Z");
    let target = ReportTarget {
        location: "amsterdam".into(),
        station: "EHAM".into(),
        tz,
    };
    let plan = target.plan(s, None, Some(date), None, now).await.unwrap();
    assert_eq!(plan.from, date, "starts on the first day the service ran");
    let inputs = paper_report::collect(s, &plan).await.unwrap();
    let r = paper_report::build(&inputs, &plan, now);
    assert_eq!(r.days.len(), 1);
    let d = &r.days[0];
    assert!(!d.in_progress);
    assert_eq!(d.starts.len(), 1);
    assert_eq!(d.starts[0].at, "07:55");

    // Weather and report delays.
    assert_eq!(d.weather.reports, 26);
    assert_eq!(d.weather.high_c, Some(25));
    assert_eq!(d.weather.high_first_at.as_deref(), Some("15:55"));
    assert_eq!(d.weather.high_last_at.as_deref(), Some("17:55"));
    assert_eq!(d.latency.catch_up, 1, "the report from before the start");
    assert_eq!(d.latency.timely, 25);
    assert_eq!(d.latency.median_s, Some(180));
    assert_eq!(d.latency.max_s, Some(660));
    assert_eq!(d.latency.over_5min, 1);
    assert_eq!(d.latency.late.len(), 1);
    assert_eq!(d.latency.late[0].observed_at, "12:25");
    let tg = d
        .latency
        .by_source
        .iter()
        .find(|x| x.source == "tgftp")
        .unwrap();
    assert_eq!((tg.first, tg.median_s), (1, Some(150)));

    // Forecast error.
    let f = d.forecast.as_ref().unwrap();
    assert_eq!(f.product, "open_meteo/gfs_global/d1");
    assert!((f.day_max_c - 26.3).abs() < 1e-9);
    assert!((f.error_c.unwrap() - 1.3).abs() < 1e-9);

    // Market, blockers and the closest calls with their endings.
    assert_eq!(d.market.as_ref().unwrap().winner.as_deref(), Some("25°C"));
    assert_eq!(d.evaluations, 4);
    let a = d.strategies.iter().find(|x| x.strategy == "A").unwrap();
    assert_eq!(a.lines, 4);
    let confirmation = a
        .blockers
        .iter()
        .find(|b| b.pattern == "confirmation #m < #m")
        .unwrap();
    assert_eq!(confirmation.count, 3);
    assert_eq!(
        confirmation.example, "confirmation 0m < 60m",
        "the latest one"
    );
    let best = &a.closest[0];
    assert_eq!((best.bucket.as_str(), best.won), ("23°C", Some(false)));
    let a25 = a.closest.iter().find(|c| c.bucket == "25°C").unwrap();
    assert_eq!(a25.at, "15:59", "the 25 °C evaluation with the best EV");
    assert_eq!(a25.won, Some(true));
    assert!((a25.pnl_per_share.unwrap() - (1.0 - 0.76 - 0.05 * 0.76 * 0.24)).abs() < 1e-9);
    let b = d.strategies.iter().find(|x| x.strategy == "B").unwrap();
    assert_eq!(b.closest[0].won, Some(true), "NO on 26 °C won");
    assert_eq!(b.closest[0].model, Some(0.97));
    let e = d.strategies.iter().find(|x| x.strategy == "E").unwrap();
    assert_eq!(e.blockers[0].pattern, "only # shares offered ≤ # #m ago");

    // Model against market on the winner, from the recorded books.
    let mvm = d.model_vs_market.as_ref().unwrap();
    assert_eq!(mvm.bucket, "25°C");
    assert_eq!(mvm.samples, 4);
    assert!(
        (mvm.trail[0].model - 0.20).abs() < 1e-9,
        "P(23 → 25) at 14:28"
    );
    assert!((mvm.trail[0].market - 0.015).abs() < 1e-9);
    assert_eq!(mvm.market_sure_at.as_deref(), Some("16:58"));
    assert_eq!(mvm.model_sure_at.as_deref(), Some("18:28"));
    assert_eq!(mvm.market_higher, 2);

    // Proposals, orders, fills and the settled P&L.
    assert_eq!(d.proposals.len(), 1);
    assert!(d.proposals[0].approved);
    assert_eq!(d.orders.len(), 1);
    assert_eq!(d.fills, 1);
    let pnl = d.pnl_usd.unwrap();
    assert!((pnl - (10.0 * 0.06 - 0.0282)).abs() < 1e-6, "pnl {pnl}");
    assert_eq!(d.providers.iter().map(|p| p.requests).sum::<i64>(), 26);
    assert_eq!(d.events.len(), 1);

    // Totals and the Markdown a user pastes.
    assert_eq!(r.totals.reports, 25);
    assert_eq!(r.totals.forecast_days, 1);
    assert_eq!(r.totals.approved, 1);
    let md = paper_report::markdown(&r);
    for needle in [
        "# Paper run — amsterdam (EHAM), 2026-09-29 → 2026-09-29",
        "| 2026-09-29 | 25 °C (15:55) | 26.3 (+1.3) | 26 · 180 s · 660 s | 4 | none | 1 (1) | 1 · 1 | $0.57 |",
        "Late: 12:25 +660 s (awc)",
        "Closest: 15:59 25°C YES · ask 0.76 · p 0.760 (market 0.745) · EV -0.0100",
        "→ **won** (+0.231/share at the ask)",
        "Sure (≥ 0.90) first: model 18:28, market 16:58",
        "open_meteo/gfs_global/d1 day maximum 26.3 °C, error +1.3 °C",
        "warning market_stream ×1",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in\n{md}");
    }

    // The same report from the running service's dashboard route.
    let service = paper_report::ReportService::new(
        s.clone(),
        target.clone(),
        std::sync::Arc::new(wm_core::time::ManualClock::new(now)),
    );
    let app = dashboard(service);
    let (status, ctype, body) = get(&app, "/api/v1/report/paper?from=2026-09-29").await;
    assert_eq!(status, 200);
    assert_eq!(ctype, "text/markdown; charset=utf-8");
    assert!(body.contains("| 2026-09-29 | 25 °C (15:55) |"), "{body}");
    let (status, _, body) = get(&app, "/api/v1/report/paper?from=2026-09-29&format=json").await;
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["days"][0]["weather"]["high_c"], 25);
    let (status, _, body) = get(&app, "/api/v1/report/paper?from=2026-10-05").await;
    assert_eq!(status, 500, "a first day after the last is refused: {body}");

    // A later day with nothing recorded reads as empty, not as an error.
    let plan = target
        .plan(s, Some(date), Some(date.succ_opt().unwrap()), None, now)
        .await
        .unwrap();
    let r = paper_report::build(&paper_report::collect(s, &plan).await.unwrap(), &plan, now);
    assert_eq!(r.days.len(), 2);
    assert!(r.days[1].in_progress);
    assert_eq!(r.days[1].weather.reports, 0);
    assert!(paper_report::markdown(&r).contains("## 2026-09-30 Wed — in progress"));
    db.drop_db().await;
}
