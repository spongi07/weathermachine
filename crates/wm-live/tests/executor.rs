//! The executor against a mock CLOB, Data API and geoblock endpoint: the
//! readiness gates, an order placed and filled, a refusal, an earlier run's
//! leftover cancelled, positions that differ, a location that may not trade.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{Duration, Utc};
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, watch};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::event::{EventEnvelope, WeatherMachineEvent};
use wm_core::ids::{ClientOrderId, TokenId};
use wm_core::market::Side;
use wm_core::trading::{Liquidity, OrderStatus};
use wm_core::units::{Price, Shares, Usd};
use wm_live::executor::{self, BookView, Channels, Command, LiveOrder, LiveStatus, Settings};
use wm_live::sign::OrderSigner;

const TOKEN: &str = "123456789";
const PROXY: &str = "0x81f4f47aa3bBcF441cDe0B7C6e2A38a706b6d77e";

fn json(v: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(v)
}

/// A venue that answers every call of a healthy start.
async fn venue(country: &str) -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/auth/derive-api-key"))
        .respond_with(json(serde_json::json!({
            "apiKey": "key-1", "secret": "dGVzdC1zZWNyZXQtYnl0ZXMtMDEyMzQ1Njc4OWFiY2Q=", "passphrase": "pp"
        })))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/version"))
        .respond_with(json(serde_json::json!({ "version": 2 })))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth/ban-status/closed-only"))
        .respond_with(json(serde_json::json!({ "closed_only": false })))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/geoblock"))
        .respond_with(json(serde_json::json!({
            "blocked": true, "ip": "192.0.2.1", "country": country, "region": "NH"
        })))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/balance-allowance/update"))
        .respond_with(json(serde_json::json!({})))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/balance-allowance"))
        .respond_with(json(
            serde_json::json!({ "balance": "50000000", "allowances": {} }),
        ))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/heartbeats"))
        .respond_with(json(serde_json::json!({ "heartbeat_id": "hb-1" })))
        .mount(&s)
        .await;
    s
}

async fn no_orders_or_positions(s: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(json(serde_json::json!([])))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/positions"))
        .respond_with(json(serde_json::json!([])))
        .mount(s)
        .await;
}

fn settings(s: &MockServer) -> Settings {
    Settings {
        clob_url: s.uri(),
        data_url: s.uri(),
        geoblock_url: format!("{}/api/geoblock", s.uri()),
        funder: PROXY.into(),
        signature_type: 1,
        api_key_nonce: 1,
        geo_close_only: None,
        require_heartbeat: true,
        max_order_usd: Usd::from_whole(10),
        user_agent: "wm-live-test".into(),
        poll_every_s: 1,
    }
}

struct Harness {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<EventEnvelope>,
    status: watch::Receiver<LiveStatus>,
    _alerts: mpsc::Receiver<(&'static str, String)>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

fn start(s: &MockServer) -> Harness {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (ev_tx, ev_rx) = mpsc::channel(64);
    let (st_tx, st_rx) = watch::channel(LiveStatus::default());
    let (al_tx, al_rx) = mpsc::channel(256);
    let (stop_tx, stop_rx) = watch::channel(false);
    let signer = OrderSigner::new(&format!("0x{}", "ab".repeat(32))).unwrap();
    let ch = Channels {
        commands: cmd_rx,
        events: ev_tx,
        status: st_tx,
        alerts: al_tx,
    };
    let task = tokio::spawn(executor::run(settings(s), signer, ch, stop_rx));
    Harness {
        commands: cmd_tx,
        events: ev_rx,
        status: st_rx,
        _alerts: al_rx,
        stop: stop_tx,
        task,
    }
}

fn book(held: &[(&str, i64)]) -> BookView {
    BookView {
        tokens: HashSet::from([TOKEN.to_owned(), "987654321".to_owned()]),
        held: held
            .iter()
            .map(|(t, n)| ((*t).to_owned(), Shares::from_whole(*n)))
            .collect::<HashMap<_, _>>(),
    }
}

fn order(id: &str) -> LiveOrder {
    LiveOrder {
        client_order_id: ClientOrderId::from_static_string(id.to_owned()),
        label: "NO 18°C on 1 Jul".to_owned(),
        token: TokenId::from_static_string(TOKEN.to_owned()),
        side: Side::Buy,
        price: Price::saturating_from_micros(920_000),
        tick: Price::saturating_from_micros(10_000),
        shares: Shares::from_whole(10),
        min_size: Shares::from_whole(5),
        neg_risk: true,
        expires_at: Utc::now() + Duration::minutes(10),
    }
}

async fn wait_status(h: &mut Harness, what: &str, f: impl Fn(&LiveStatus) -> bool) -> LiveStatus {
    let r = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if f(&h.status.borrow()) {
                return h.status.borrow().clone();
            }
            h.status.changed().await.unwrap();
        }
    })
    .await;
    r.unwrap_or_else(|_| panic!("{what}: status stayed {:?}", h.status.borrow()))
}

async fn next_update(h: &mut Harness) -> wm_core::event::OrderUpdateEvent {
    let env = tokio::time::timeout(std::time::Duration::from_secs(20), h.events.recv())
        .await
        .expect("an order update")
        .expect("the channel is open");
    match env.event {
        WeatherMachineEvent::OrderUpdate(u) => u,
        other => panic!("not an order update: {other:?}"),
    }
}

async fn stop(h: Harness) {
    let _ = h.stop.send(true);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(15), h.task).await;
}

#[tokio::test]
async fn a_ready_venue_places_a_post_only_bid_and_follows_it_to_its_fill() {
    let s = venue("NL").await;
    no_orders_or_positions(&s).await;
    Mock::given(method("POST"))
        .and(path("/order"))
        .respond_with(json(
            serde_json::json!({ "success": true, "status": "live", "errorMsg": "" }),
        ))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/data/order/0x[0-9a-f]{64}$"))
        .respond_with(json(serde_json::json!({
            "id": "0xabc", "status": "MATCHED", "original_size": "10", "size_matched": "10", "price": "0.92"
        })))
        .mount(&s)
        .await;
    let mut h = start(&s);
    let st = wait_status(&mut h, "not ready before the markets are known", |st| {
        st.blockers.iter().any(|b| b.contains("weather markets"))
    })
    .await;
    assert!(!st.ready);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    let st = wait_status(&mut h, "ready", |st| st.ready).await;
    assert_eq!(st.protocol, Some(2));
    assert_eq!(st.location.as_deref(), Some("NL-NH: API allowed"));
    assert_eq!(st.balance, Some(Usd::from_whole(50)));
    assert!(st.heartbeat_at.is_some());

    h.commands
        .send(Command::Place(order("wm-t-1-123456789")))
        .await
        .unwrap();
    let live = next_update(&mut h).await;
    assert_eq!(live.update.status, OrderStatus::Live);
    let venue_id = live.update.venue_order_id.clone().unwrap();
    assert_eq!(venue_id.len(), 66, "the EIP-712 digest is the order id");
    let filled = next_update(&mut h).await;
    assert_eq!(filled.update.status, OrderStatus::Filled);
    assert_eq!(filled.update.filled, Shares::from_whole(10));
    let fill = filled.fill.unwrap();
    assert_eq!(fill.shares, Shares::from_whole(10));
    assert_eq!(
        fill.price,
        Price::saturating_from_micros(920_000),
        "a maker fills at its own price"
    );
    assert_eq!(fill.fee, Usd::ZERO);
    assert_eq!(fill.liquidity, Liquidity::Maker);

    // the body that went out: post-only, good till date, signed
    let posted: Vec<_> = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/order")
        .collect();
    let body: serde_json::Value = serde_json::from_slice(&posted[0].body).unwrap();
    assert_eq!(body["orderType"], "GTD");
    assert_eq!(body["postOnly"], true);
    assert_eq!(body["owner"], "key-1");
    assert_eq!(body["order"]["makerAmount"], "9200000");
    assert_eq!(body["order"]["maker"], PROXY);
    assert!(
        posted[0].headers.get("POLY_SIGNATURE").is_some(),
        "L2 signed"
    );
    stop(h).await;
}

#[tokio::test]
async fn a_refusal_ends_the_order_and_nothing_goes_out_unready() {
    let s = venue("NL").await;
    no_orders_or_positions(&s).await;
    Mock::given(method("POST"))
        .and(path("/order"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "success": false, "errorMsg": "invalid post-only order: order crosses book"
        })))
        .mount(&s)
        .await;
    let mut h = start(&s);
    // before the markets are known, an order is refused without a request
    h.commands
        .send(Command::Place(order("wm-t-2-123456789")))
        .await
        .unwrap();
    let early = next_update(&mut h).await;
    assert_eq!(early.update.status, OrderStatus::Rejected);
    assert!(early.update.reason.unwrap().contains("not ready"));
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    wait_status(&mut h, "ready", |st| st.ready).await;
    h.commands
        .send(Command::Place(order("wm-t-3-123456789")))
        .await
        .unwrap();
    let refused = next_update(&mut h).await;
    assert_eq!(refused.update.status, OrderStatus::Rejected);
    assert!(refused.update.reason.unwrap().contains("crosses book"));
    stop(h).await;
}

#[tokio::test]
async fn an_earlier_runs_leftover_on_a_weather_market_is_cancelled() {
    let s = venue("NL").await;
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(json(serde_json::json!([
            { "id": "0xleft", "asset_id": TOKEN },
            { "id": "0xother", "asset_id": "555" }
        ])))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(json(
            serde_json::json!([{ "id": "0xother", "asset_id": "555" }]),
        ))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/order"))
        .respond_with(json(
            serde_json::json!({ "canceled": ["0xleft"], "not_canceled": {} }),
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/positions"))
        .respond_with(json(serde_json::json!([])))
        .mount(&s)
        .await;
    let mut h = start(&s);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    wait_status(&mut h, "ready after the leftover is gone", |st| st.ready).await;
    let deletes: Vec<_> = s
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 1, "only the weather market's order");
    let body: serde_json::Value = serde_json::from_slice(&deletes[0].body).unwrap();
    assert_eq!(body["orderID"], "0xleft");
    stop(h).await;
}

#[tokio::test]
async fn positions_that_differ_from_the_books_keep_the_venue_closed() {
    let s = venue("NL").await;
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(json(serde_json::json!([])))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/positions"))
        .respond_with(json(serde_json::json!([
            { "asset": TOKEN, "size": 10.0 },
            { "asset": "a-btc-token", "size": 5.0 }
        ])))
        .mount(&s)
        .await;
    let mut h = start(&s);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    let st = wait_status(&mut h, "a position mismatch", |st| {
        st.blockers.iter().any(|b| b.contains("positions differ"))
    })
    .await;
    assert!(!st.ready);
    assert!(
        st.blockers.iter().any(|b| b.contains(TOKEN)),
        "{:?}",
        st.blockers
    );
    assert!(
        !st.blockers.iter().any(|b| b.contains("a-btc-token")),
        "other markets are not ours"
    );
    stop(h).await;
}

#[tokio::test]
async fn a_location_that_is_close_only_on_the_api_blocks_orders() {
    let s = venue("GB").await;
    no_orders_or_positions(&s).await;
    let mut h = start(&s);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    let st = wait_status(&mut h, "the location blocker", |st| {
        st.blockers.iter().any(|b| b.contains("close-only"))
    })
    .await;
    assert!(!st.ready && !st.location_ok);
    stop(h).await;
}

#[tokio::test]
async fn a_pulled_bid_reports_its_part_fill_then_its_cancel() {
    let s = venue("NL").await;
    no_orders_or_positions(&s).await;
    Mock::given(method("POST"))
        .and(path("/order"))
        .respond_with(json(
            serde_json::json!({ "success": true, "status": "live" }),
        ))
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/order"))
        .respond_with(json(
            serde_json::json!({ "canceled": [], "not_canceled": {} }),
        ))
        .mount(&s)
        .await;
    // resting, then (after the cancel) cancelled with 3 of 10 bought
    Mock::given(method("GET"))
        .and(path_regex(r"^/data/order/0x[0-9a-f]{64}$"))
        .respond_with(json(serde_json::json!({
            "id": "x", "status": "LIVE", "original_size": "10", "size_matched": "0"
        })))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/data/order/0x[0-9a-f]{64}$"))
        .respond_with(json(serde_json::json!({
            "id": "x", "status": "CANCELED", "original_size": "10", "size_matched": "3"
        })))
        .mount(&s)
        .await;
    let mut h = start(&s);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    wait_status(&mut h, "ready", |st| st.ready).await;
    h.commands
        .send(Command::Place(order("wm-t-4-123456789")))
        .await
        .unwrap();
    assert_eq!(next_update(&mut h).await.update.status, OrderStatus::Live);
    let st = wait_status(&mut h, "one bid resting", |st| st.resting == 1).await;
    assert_eq!(
        st.reserved,
        Usd::from_micros(9_200_000),
        "the bid reserves its cost"
    );
    h.commands
        .send(Command::CancelAll("kill switch".into()))
        .await
        .unwrap();
    let part = next_update(&mut h).await;
    assert_eq!(part.update.status, OrderStatus::PartiallyFilled);
    assert_eq!(part.fill.unwrap().shares, Shares::from_whole(3));
    let end = next_update(&mut h).await;
    assert_eq!(end.update.status, OrderStatus::Canceled);
    assert_eq!(end.update.filled, Shares::from_whole(3));
    stop(h).await;
}

#[tokio::test]
async fn an_order_of_unknown_fate_blocks_new_ones_until_its_record_is_read() {
    let s = venue("NL").await;
    no_orders_or_positions(&s).await;
    Mock::given(method("POST"))
        .and(path("/order"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/data/order/0x[0-9a-f]{64}$"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/data/order/0x[0-9a-f]{64}$"))
        .respond_with(json(serde_json::json!({
            "id": "x", "status": "LIVE", "original_size": "10", "size_matched": "0"
        })))
        .mount(&s)
        .await;
    let mut h = start(&s);
    h.commands.send(Command::Book(book(&[]))).await.unwrap();
    wait_status(&mut h, "ready", |st| st.ready).await;
    h.commands
        .send(Command::Place(order("wm-t-5-123456789")))
        .await
        .unwrap();
    let st = wait_status(&mut h, "unknown fate", |st| st.unresolved == 1).await;
    assert!(
        !st.ready,
        "nothing new goes out while an order's fate is unknown"
    );
    // its record shows it resting: the engine hears it is live
    let live = next_update(&mut h).await;
    assert_eq!(live.update.status, OrderStatus::Live);
    wait_status(&mut h, "resolved", |st| st.unresolved == 0 && st.ready).await;
    stop(h).await;
}
