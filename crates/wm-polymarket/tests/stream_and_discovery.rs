#![allow(clippy::unwrap_used, clippy::expect_used)]
//! WebSocket stream and Gamma discovery against local servers.

use chrono::{NaiveDate, Utc};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_core::event::WeatherMachineEvent;
use wm_core::ids::{LocationId, ProviderId, StationId, TokenId};
use wm_core::market::{FeeSchedule, TempUnit};
use wm_core::time::SystemClock;
use wm_core::units::Price;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_polymarket::{GammaClient, LocationMarketSpec, MarketStream, MarketStreamConfig, build_market, event_slug};

#[tokio::test]
async fn market_stream_builds_books_pings_and_reconnects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sub_tx, mut sub_rx) = mpsc::channel::<String>(8);
    let (ping_tx, mut ping_rx) = mpsc::channel::<()>(8);
    tokio::spawn(async move {
        for session in 0..2 {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                sub_tx.send(t.to_string()).await.unwrap();
            }
            ws.send(Message::text(r#"[{"event_type":"book","asset_id":"1018","market":"0xc","bids":[{"price":"0.93","size":"100"}],"asks":[{"price":"0.95","size":"50"}],"timestamp":"1790427300000"}]"#)).await.unwrap();
            ws.send(Message::text(r#"{"event_type":"price_change","market":"0xc","price_changes":[{"asset_id":"1018","price":"0.94","size":"20","side":"BUY"}],"timestamp":"1790427301000"}"#)).await.unwrap();
            if session == 0 {
                // Wait for the client's heartbeat, then drop the connection.
                while let Some(Ok(m)) = ws.next().await {
                    if let Message::Text(t) = m
                        && t.as_str() == "PING"
                    {
                        ping_tx.send(()).await.unwrap();
                        break;
                    }
                }
                let _ = ws.close(None).await;
            } else {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::from_millis(50);
    let gate = ProviderGate::new(ProviderId::polymarket_ws(), policy, Arc::new(SystemClock::new()), 3);
    let cfg = MarketStreamConfig { url: format!("ws://{addr}"), ping_interval: Duration::from_millis(200), book_depth: 5, max_reconnect_wait: Duration::from_secs(2) };
    let stream = MarketStream::new(cfg, gate, Arc::new(SystemClock::new()));
    let status = stream.status();
    let (assets_tx, assets_rx) = watch::channel(vec![TokenId::new("1018").unwrap()]);
    let (out_tx, mut out_rx) = mpsc::channel(64);
    let (stop_tx, stop_rx) = watch::channel(false);
    let handle = tokio::spawn(stream.run(assets_rx, out_tx, stop_rx));

    let sub = tokio::time::timeout(Duration::from_secs(3), sub_rx.recv()).await.unwrap().unwrap();
    assert!(sub.contains("\"assets_ids\":[\"1018\"]") && sub.contains("\"type\":\"market\""), "{sub}");
    let mut best_bids = Vec::new();
    while best_bids.len() < 2 {
        let env = tokio::time::timeout(Duration::from_secs(3), out_rx.recv()).await.unwrap().unwrap();
        if let WeatherMachineEvent::OrderBookUpdate(b) = env.event {
            best_bids.push(b.book.best_bid().unwrap().price);
        }
    }
    assert_eq!(best_bids, vec![Price::parse("0.93").unwrap(), Price::parse("0.94").unwrap()]);
    tokio::time::timeout(Duration::from_secs(3), ping_rx.recv()).await.unwrap().unwrap();
    // After the server drops the connection the stream reconnects and resubscribes.
    let sub2 = tokio::time::timeout(Duration::from_secs(5), sub_rx.recv()).await.unwrap().unwrap();
    assert!(sub2.contains("1018"));
    let env = tokio::time::timeout(Duration::from_secs(3), out_rx.recv()).await.unwrap().unwrap();
    assert!(matches!(env.event, WeatherMachineEvent::OrderBookUpdate(_)));
    assert!(status.borrow().reconnects_total >= 1);
    drop(assets_tx);
    stop_tx.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

#[tokio::test]
async fn gamma_discovery_maps_event() {
    let server = MockServer::start().await;
    let slug = event_slug("highest-temperature-in-amsterdam-on-{month}-{day}-{year}", NaiveDate::from_ymd_opt(2026, 9, 25).unwrap());
    let market = |title: &str, i: u32| {
        format!(r#"{{"id":"m{i}","question":"q","conditionId":"0xc{i}","groupItemTitle":"{title}","outcomes":"[\"Yes\",\"No\"]","clobTokenIds":"[\"{}\",\"{}\"]","acceptingOrders":true,"closed":false}}"#, 1000 + i, 2000 + i)
    };
    let body = format!(
        r#"[{{"id":"e","slug":"{slug}","title":"Highest temperature in Amsterdam on September 25?","description":"This market will resolve to the temperature range that contains the highest temperature recorded by NOAA at the Amsterdam Airport Schiphol Station in degrees Celsius. The resolution source for this market will be information from NOAA, available here: https://www.weather.gov/wrh/timeseries?site=eham.","negRisk":true,"active":true,"closed":false,"markets":[{},{},{}]}}]"#,
        market("17°C or below", 17),
        market("18°C", 18),
        market("19°C or higher", 19)
    );
    Mock::given(method("GET")).and(path("/events")).and(query_param("slug", slug.as_str())).respond_with(ResponseTemplate::new(200).set_body_string(body)).expect(1).mount(&server).await;
    let gate = ProviderGate::new(ProviderId::polymarket_gamma(), RateLimitPolicy::local_test(), Arc::new(SystemClock::new()), 1);
    let fetcher = Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap());
    let client = GammaClient::new(fetcher, server.uri());
    let (events, raw) = client.events_by_slug(&slug, Duration::from_secs(1)).await.unwrap();
    assert!(!raw.is_empty());
    let spec = LocationMarketSpec {
        location: LocationId::new("amsterdam").unwrap(),
        station: StationId::new("EHAM").unwrap(),
        timezone: chrono_tz::Europe::Amsterdam,
        slug_template: String::new(),
        unit: TempUnit::Celsius,
        fees: FeeSchedule::taker(50_000),
    };
    let m = build_market(&events[0], &spec, NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(), Utc::now()).unwrap();
    assert_eq!(m.outcomes.len(), 3);
    assert_eq!(m.outcome_for_value(18).unwrap().yes_token.as_str(), "1018");
    assert!(m.rules.text.contains("NOAA"));
}
