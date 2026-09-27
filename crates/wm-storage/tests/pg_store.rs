#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
//! PostgreSQL integration tests. Each test runs in its own fresh database.
//! Set `WM_TEST_DATABASE_URL` (a role with CREATEDB), e.g.
//! `postgres://wm:wm@127.0.0.1:5432/wm_test`; tests are skipped otherwise.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use sqlx::Row;
use wm_core::event::{
    EventEnvelope, EventSource, ObservationEvent, ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{
    ClientOrderId, DecisionId, LocationId, ProviderId, RunId, StationId, StrategyId, TokenId,
};
use wm_core::ingest::{CacheOutcome, IngestBatch, ProviderRequestRecord, RawPayloadRecord};
use wm_core::market::Side;
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
        println!("WM_TEST_DATABASE_URL not set; skipping PostgreSQL integration test");
        return None;
    };
    let admin = PgStore::connect(&url, 2).await.expect("connect admin");
    let name = format!("wm_it_{}", uuid_like());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(admin.pool())
        .await
        .expect("create db");
    admin.pool().close().await;
    let (base, _) = url.rsplit_once('/').expect("url has database path");
    let db_url = format!("{base}/{name}");
    let store = PgStore::connect(&db_url, 5).await.expect("connect test db");
    store.migrate().await.expect("migrate");
    Some(TestDb {
        store,
        admin_url: url,
        name,
    })
}

fn uuid_like() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}{:x}", n, std::process::id())
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn obs(t: &str, whole: i32, version: u32, raw: &str) -> Observation {
    Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: utc(t),
            report_type: ReportType::Metar,
        },
        version,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: Some(TempC::from_whole(12)),
        precision: TempPrecision::WholeDegree,
        raw_text: raw.to_owned(),
        content_hash: wm_core::hash::sha256_hex(raw.as_bytes()),
        provider: ProviderId::awc(),
        provider_receipt_at: Some(utc(t) + Duration::minutes(2)),
        fetched_at: utc(t) + Duration::minutes(3),
        parser_version: 1,
        quality: QualityFlags {
            auto: true,
            ..QualityFlags::default()
        },
    }
}

fn batch(at: &str, body: &[u8], observations: Vec<(Observation, DedupClass)>) -> IngestBatch {
    IngestBatch {
        request: ProviderRequestRecord {
            provider: ProviderId::awc(),
            endpoint: "/api/data/metar?ids=EHAM".into(),
            station: Some(eham()),
            requested_at: utc(at),
            completed_at: utc(at) + Duration::milliseconds(150),
            status: Some(200),
            latency_ms: 150,
            bytes: body.len() as u64,
            cache: CacheOutcome::Miss,
            retry_count: 0,
            throttled: false,
            error_class: None,
            gate_wait_ms: 0,
            payload_sha256: Some(wm_core::hash::sha256_hex(body)),
        },
        raw: Some(RawPayloadRecord {
            provider: ProviderId::awc(),
            station: Some(eham()),
            endpoint: "/api/data/metar?ids=EHAM".into(),
            fetched_at: utc(at),
            status: 200,
            content_type: Some("application/json".into()),
            body: body.to_vec(),
            sha256: wm_core::hash::sha256_hex(body),
            parser_version: 1,
        }),
        observations,
        corrections: vec![],
        health: None,
    }
}

#[tokio::test]
async fn migrations_ingest_dedup_and_corrections() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    s.migrate().await.unwrap(); // idempotent
    let body = br#"[{"rawOb":"EHAM 261255Z 18/12"}]"#;
    let o1 = obs("2026-09-26T12:25:00Z", 17, 1, "EHAM 261225Z 17/12");
    let o2 = obs("2026-09-26T12:55:00Z", 18, 1, "EHAM 261255Z 18/12");
    s.persist_ingest(&batch(
        "2026-09-26T12:58:00Z",
        body,
        vec![(o1.clone(), DedupClass::New), (o2.clone(), DedupClass::New)],
    ))
    .await
    .unwrap();
    // Same payload again (next poll): stored once, sighting counted.
    s.persist_ingest(&batch("2026-09-26T12:59:00Z", body, vec![]))
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT seen_count, first_fetched_at, last_fetched_at FROM raw_weather_payloads",
    )
    .fetch_one(s.pool())
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("seen_count").unwrap(), 2);
    assert!(
        row.try_get::<DateTime<Utc>, _>("last_fetched_at").unwrap()
            > row.try_get::<DateTime<Utc>, _>("first_fetched_at").unwrap()
    );
    let n: i64 = sqlx::query("SELECT count(*) FROM provider_requests")
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(n, 2);
    // Correction: version 2 stored alongside version 1; the current view shows v2.
    let mut c = obs("2026-09-26T12:55:00Z", 17, 2, "EHAM 261255Z COR 17/12");
    c.quality.correction_marker = true;
    let mut b = batch(
        "2026-09-26T13:01:00Z",
        b"[2]",
        vec![(c.clone(), DedupClass::Correction)],
    );
    b.corrections.push(wm_core::event::CorrectionEvent {
        previous: o2.clone(),
        current: c.clone(),
        labeled: true,
    });
    s.persist_ingest(&b).await.unwrap();
    let versions: i64 =
        sqlx::query("SELECT count(*) FROM weather_observations WHERE observed_at = $1")
            .bind(utc("2026-09-26T12:55:00Z"))
            .fetch_one(s.pool())
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
    assert_eq!(versions, 2, "raw history is never overwritten");
    let current = s
        .observations_since(&eham(), utc("2026-09-26T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[1].version, 2);
    assert_eq!(current[1].temperature, Some(TempC::from_whole(17)));
    assert!(current[1].quality.correction_marker);
    assert_eq!(current[0], o1, "round-trip preserves every field");
    let corr: i64 = sqlx::query("SELECT count(*) FROM weather_corrections")
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(corr, 1);
    let stats = s.request_stats(utc("2026-09-26T00:00:00Z")).await.unwrap();
    assert_eq!(stats[0].requests, 3);
    db.drop_db().await;
}

#[tokio::test]
async fn journal_roundtrip_is_exact() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    let run = RunId::deterministic(9);
    s.record_run(
        &run,
        RunMode::Paper,
        "test-model",
        &serde_json::json!({"k": 1}),
        utc("2026-09-26T00:00:00Z"),
    )
    .await
    .unwrap();
    let mut events = Vec::new();
    for (i, t) in ["2026-09-26T12:25:00Z", "2026-09-26T12:55:00Z"]
        .iter()
        .enumerate()
    {
        let mut e = EventEnvelope::new(
            utc(t),
            EventSource::Live,
            WeatherMachineEvent::WeatherObservation(ObservationEvent {
                observation: obs(t, 17 + i as i32, 1, t),
                class: DedupClass::New,
            }),
        );
        e.seq = i as u64 + 1;
        events.push(e);
    }
    let mut h =
        ProviderHealthSnapshot::new(ProviderId::awc(), Some(eham()), utc("2026-09-26T12:58:00Z"));
    h.state = ProviderHealthState::Healthy;
    let mut e = EventEnvelope::new(
        utc("2026-09-26T12:58:00Z"),
        EventSource::Live,
        WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
            previous_state: None,
            snapshot: h,
        }),
    );
    e.seq = 3;
    events.push(e);
    s.append_events(&run, &events).await.unwrap();
    s.append_events(&run, &events).await.unwrap(); // idempotent
    let loaded = s.load_journal(&run).await.unwrap();
    assert_eq!(loaded, events);
    assert_eq!(s.last_journal_seq(&run).await.unwrap(), 3);
    db.drop_db().await;
}

#[tokio::test]
async fn markets_rules_decisions_orders_fills() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    let m = synthetic_temperature_market(
        &LocationId::new("amsterdam").unwrap(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 9, 26).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-09-26T06:00:00Z"),
    );
    s.upsert_market(&m, Some(b"{\"raw\":true}")).await.unwrap();
    s.upsert_market(&m, Some(b"{\"raw\":true}")).await.unwrap();
    let outcomes: i64 = sqlx::query("SELECT count(*) FROM market_outcomes")
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(outcomes, m.outcomes.len() as i64);
    assert_eq!(
        s.rules_review_status(&m.rules.sha256)
            .await
            .unwrap()
            .as_deref(),
        Some("auto_parsed")
    );
    assert!(s.approve_rules(&m.rules.sha256, "operator").await.unwrap());
    assert_eq!(
        s.rules_review_status(&m.rules.sha256)
            .await
            .unwrap()
            .as_deref(),
        Some("approved")
    );

    let run = RunId::deterministic(3);
    let d = DecisionRecord {
        decision_id: DecisionId(1),
        strategy: StrategyId::new("A_buy_yes_final_high").unwrap(),
        at: utc("2026-09-26T13:58:00Z"),
        location: LocationId::new("amsterdam").unwrap(),
        event_slug: Some(m.event_slug.clone()),
        summary: "A BUY YES 18°C @ 0.95 ×10 — APPROVED".into(),
        inputs: serde_json::json!({"p_win": 0.985}),
        outputs: serde_json::json!({"approved": true}),
        approved: true,
        reasons: vec![],
    };
    s.record_decisions(&run, &[d.clone(), d]).await.unwrap();
    let row = wm_storage::wm_execution_record::OrderRow {
        client_order_id: "wm-x-1-t".into(),
        decision_id: 1,
        strategy: "A_buy_yes_final_high".into(),
        location: "amsterdam".into(),
        event_slug: m.event_slug.to_string(),
        token: "syn-token".into(),
        condition_id: "0xc".into(),
        outcome_side: "YES".into(),
        side: "BUY".into(),
        kind: "open".into(),
        limit_price_micros: 950_000,
        shares_micros: 10_000_000,
        tif: serde_json::json!({"tif": "fak"}),
        status: "filled".into(),
        filled_micros: 10_000_000,
        avg_price_micros: Some(950_000),
        fees_micros: 23_750,
        venue_order_id: Some("sim-1".into()),
        reason: None,
        created_at: utc("2026-09-26T13:58:00Z"),
        updated_at: utc("2026-09-26T13:58:01Z"),
    };
    s.upsert_order(&run, &row).await.unwrap();
    s.upsert_order(&run, &row).await.unwrap();
    s.record_fill(&Fill {
        client_order_id: ClientOrderId::new("wm-x-1-t").unwrap(),
        token: TokenId::new("syn-token").unwrap(),
        side: Side::Buy,
        price: Price::parse("0.95").unwrap(),
        shares: Shares::from_whole(10),
        fee: Usd::parse("0.02375").unwrap(),
        liquidity: Liquidity::Taker,
        ts: utc("2026-09-26T13:58:01Z"),
    })
    .await
    .unwrap();
    let n: i64 = sqlx::query("SELECT count(*) FROM decision_snapshots")
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(n, 1);
    let f: i64 = sqlx::query("SELECT fee_micros FROM fills")
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(f, 23_750);
    db.drop_db().await;
}

#[tokio::test]
async fn station_lease_is_exclusive_across_connections() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    let station = StationId::new(format!("T{}", &uuid_like()[..6].to_ascii_uppercase())).unwrap();
    let first = s
        .try_station_lease(&station)
        .await
        .unwrap()
        .expect("first lease");
    assert!(
        s.try_station_lease(&station).await.unwrap().is_none(),
        "second collector refused"
    );
    first.release().await.unwrap();
    let again = s.try_station_lease(&station).await.unwrap();
    assert!(again.is_some(), "lease available after release");
    drop(again);
    db.drop_db().await;
}

fn book_event(seq: u64, at: DateTime<Utc>, token: &str) -> EventEnvelope {
    let lv = |p: &str, s: &str| wm_core::market::BookLevel {
        price: Price::parse(p).unwrap(),
        size: Shares::parse(s).unwrap(),
    };
    let mut e = EventEnvelope::new(
        at,
        EventSource::Live,
        WeatherMachineEvent::OrderBookUpdate(wm_core::event::OrderBookEvent {
            book: wm_core::market::OrderBook {
                token: TokenId::new(token).unwrap(),
                bids: vec![lv("0.45", "100"), lv("0.44", "50")],
                asks: vec![lv("0.47", "80")],
                tick_size: Price::parse("0.01").unwrap(),
                min_order_size: Shares::parse("5").unwrap(),
                exchange_ts: None,
                received_at: at,
                hash: None,
                confirmed_at: None,
            },
        }),
    );
    e.seq = seq;
    e
}

async fn count(s: &PgStore, sql: &'static str) -> i64 {
    sqlx::query(sql)
        .fetch_one(s.pool())
        .await
        .unwrap()
        .try_get(0)
        .unwrap()
}

/// One engine batch is one transaction (a failure leaves nothing behind, so a
/// retry cannot duplicate), large batches go in as multi-row inserts, and
/// retention removes only old order-book updates from the journal.
#[tokio::test]
async fn engine_batch_is_atomic_and_old_book_updates_are_pruned() {
    let Some(db) = fresh().await else { return };
    let s = &db.store;
    let run = RunId::deterministic(21);
    // Whole seconds: PostgreSQL stores microseconds, `Utc::now()` has nanoseconds.
    let now = chrono::SubsecRound::trunc_subsecs(Utc::now(), 0);
    s.record_run(&run, RunMode::Paper, "m", &serde_json::json!({}), now)
        .await
        .unwrap();
    let old = now - Duration::days(10);
    let mut events: Vec<EventEnvelope> = (1..=1500)
        .map(|i| book_event(i, old + Duration::seconds(i as i64), "tok-a"))
        .collect();
    let mut w = EventEnvelope::new(
        old,
        EventSource::Live,
        WeatherMachineEvent::WeatherObservation(ObservationEvent {
            observation: obs("2026-09-16T12:25:00Z", 17, 1, "2026-09-16T12:28:00Z"),
            class: DedupClass::New,
        }),
    );
    w.seq = 1501;
    events.push(w);
    events.extend((1502..=1504).map(|i| book_event(i, now, "tok-b")));
    let mut heartbeat = EventEnvelope::new(
        old,
        EventSource::Live,
        WeatherMachineEvent::MarketStreamHeartbeat(wm_core::event::StreamHeartbeatEvent {
            connected_since: old,
        }),
    );
    heartbeat.seq = 1505;
    events.push(heartbeat);
    let decision = DecisionRecord {
        decision_id: DecisionId(7),
        strategy: StrategyId::new("A_buy_yes_final_high").unwrap(),
        at: now,
        location: LocationId::new("amsterdam").unwrap(),
        event_slug: None,
        summary: "test".into(),
        inputs: serde_json::json!({}),
        outputs: serde_json::json!({}),
        approved: true,
        reasons: vec![],
    };
    let order = wm_storage::wm_execution_record::OrderRow {
        client_order_id: "wm-batch-1".into(),
        decision_id: 7,
        strategy: "A_buy_yes_final_high".into(),
        location: "amsterdam".into(),
        event_slug: "e".into(),
        token: "tok-b".into(),
        condition_id: "0xc".into(),
        outcome_side: "YES".into(),
        side: "BUY".into(),
        kind: "limit".into(),
        limit_price_micros: 950_000,
        shares_micros: 10_000_000,
        tif: serde_json::json!("fok"),
        status: "filled".into(),
        filled_micros: 10_000_000,
        avg_price_micros: Some(950_000),
        fees_micros: 2_375,
        venue_order_id: None,
        reason: None,
        created_at: now,
        updated_at: now,
    };
    let fill = |id: &str| Fill {
        client_order_id: ClientOrderId::new(id).unwrap(),
        token: TokenId::new("tok-b").unwrap(),
        side: Side::Buy,
        price: Price::parse("0.95").unwrap(),
        shares: Shares::parse("10").unwrap(),
        fee: Usd::from_micros(2_375),
        liquidity: Liquidity::Taker,
        ts: now,
    };
    let books: Vec<wm_core::market::OrderBook> = events[1501..1503]
        .iter()
        .filter_map(|e| match &e.event {
            WeatherMachineEvent::OrderBookUpdate(b) => Some(b.book.clone()),
            _ => None,
        })
        .collect();
    s.persist_engine_batch(
        &run,
        wm_storage::EngineBatch {
            events: &events,
            decisions: std::slice::from_ref(&decision),
            orders: std::slice::from_ref(&order),
            fills: &[fill("wm-batch-1")],
            books: &books,
        },
    )
    .await
    .unwrap();
    assert_eq!(count(s, "SELECT count(*) FROM event_journal").await, 1505);
    assert_eq!(count(s, "SELECT count(*) FROM decision_snapshots").await, 1);
    assert_eq!(count(s, "SELECT count(*) FROM fills").await, 1);
    assert_eq!(
        count(s, "SELECT count(*) FROM orderbook_snapshots").await,
        2
    );
    assert_eq!(
        s.load_journal(&run).await.unwrap(),
        events,
        "multi-row insert is exact"
    );

    // A batch whose last statement fails (fill for an unknown order) leaves
    // nothing behind: its journal rows are rolled back with it.
    let more: Vec<EventEnvelope> = (1506..=1511).map(|i| book_event(i, now, "tok-b")).collect();
    let failed = s
        .persist_engine_batch(
            &run,
            wm_storage::EngineBatch {
                events: &more,
                decisions: &[],
                orders: &[],
                fills: &[fill("wm-unknown")],
                books: &[],
            },
        )
        .await;
    assert!(failed.is_err());
    assert_eq!(count(s, "SELECT count(*) FROM event_journal").await, 1505);
    assert_eq!(count(s, "SELECT count(*) FROM fills").await, 1);

    // Retention: old order-book updates and stream heartbeats go, everything
    // else stays.
    let deleted = s
        .prune_journal_books(now - Duration::days(7))
        .await
        .unwrap();
    assert_eq!(deleted, 1501);
    assert_eq!(count(s, "SELECT count(*) FROM event_journal").await, 4);
    assert_eq!(
        count(
            s,
            "SELECT count(*) FROM event_journal WHERE kind = 'weather_observation'"
        )
        .await,
        1
    );
    db.drop_db().await;
}
