//! Polymarket CLOB market-channel WebSocket (read-only).
//!
//! `wss://ws-subscriptions-clob.polymarket.com/ws/market`: subscribe with
//! `{"assets_ids":[…],"type":"market"}`; the server sends `book` snapshots,
//! `price_change` deltas (current format: `price_changes[]`; legacy:
//! `changes[]`), `tick_size_change` and `last_trade_price`. Clients send
//! `PING` every 10 s. On disconnect, books are marked stale and the stream
//! reconnects through its own rate-limit gate.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use wm_core::event::{EventEnvelope, EventSource, MarketTradeEvent, OrderBookEvent, WeatherMachineEvent};
use wm_core::ids::TokenId;
use wm_core::market::{BookLevel, OrderBook, Side, TradePrint};
use wm_core::time::Clock;
use wm_core::units::{Price, Shares};
use wm_net::{ProviderGate, RequestOutcome};

/// Parsed market-channel event.
#[derive(Debug, Clone, PartialEq)]
pub enum WsEvent {
    Book { asset: String, bids: Vec<(Price, Shares)>, asks: Vec<(Price, Shares)>, ts: Option<DateTime<Utc>>, hash: Option<String> },
    PriceChange { asset: String, side: Side, price: Price, size: Shares, ts: Option<DateTime<Utc>> },
    TickSize { asset: String, tick: Price },
    LastTrade { asset: String, price: Price, size: Shares, side: Option<Side>, ts: Option<DateTime<Utc>> },
    Pong,
    Other,
}

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(x) => Some(x.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn price(v: Option<&Value>) -> Option<Price> {
    v.and_then(s).and_then(|x| Price::parse(&x).ok())
}

fn shares(v: Option<&Value>) -> Option<Shares> {
    v.and_then(s).and_then(|x| Shares::parse(&x).ok())
}

fn side(v: Option<&Value>) -> Option<Side> {
    match v.and_then(s)?.to_ascii_uppercase().as_str() {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

fn levels(v: Option<&Value>) -> Vec<(Price, Shares)> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|l| Some((price(l.get("price"))?, shares(l.get("size"))?))).collect())
        .unwrap_or_default()
}

fn parse_one(obj: &Value, out: &mut Vec<WsEvent>) {
    let ts = obj.get("timestamp").and_then(crate::clob::parse_epoch);
    let et = obj.get("event_type").and_then(Value::as_str).unwrap_or("");
    let asset = obj.get("asset_id").and_then(s);
    match et {
        "book" => {
            if let Some(asset) = asset {
                let bids = if obj.get("bids").is_some() { levels(obj.get("bids")) } else { levels(obj.get("buys")) };
                let asks = if obj.get("asks").is_some() { levels(obj.get("asks")) } else { levels(obj.get("sells")) };
                out.push(WsEvent::Book { asset, bids, asks, ts, hash: obj.get("hash").and_then(s) });
            }
        }
        "price_change" => {
            if let Some(changes) = obj.get("price_changes").and_then(Value::as_array) {
                for c in changes {
                    if let (Some(a), Some(sd), Some(p), Some(sz)) = (c.get("asset_id").and_then(s), side(c.get("side")), price(c.get("price")), shares(c.get("size"))) {
                        out.push(WsEvent::PriceChange { asset: a, side: sd, price: p, size: sz, ts });
                    }
                }
            } else if let (Some(a), Some(changes)) = (asset, obj.get("changes").and_then(Value::as_array)) {
                for c in changes {
                    if let (Some(sd), Some(p), Some(sz)) = (side(c.get("side")), price(c.get("price")), shares(c.get("size"))) {
                        out.push(WsEvent::PriceChange { asset: a.clone(), side: sd, price: p, size: sz, ts });
                    }
                }
            }
        }
        "tick_size_change" => {
            if let (Some(a), Some(t)) = (asset, price(obj.get("new_tick_size"))) {
                out.push(WsEvent::TickSize { asset: a, tick: t });
            }
        }
        "last_trade_price" => {
            if let (Some(a), Some(p), Some(sz)) = (asset, price(obj.get("price")), shares(obj.get("size"))) {
                out.push(WsEvent::LastTrade { asset: a, price: p, size: sz, side: side(obj.get("side")), ts });
            }
        }
        _ => out.push(WsEvent::Other),
    }
}

/// Parse one text frame (object or array of objects, or `PONG`).
pub fn parse_ws_message(text: &str) -> Result<Vec<WsEvent>, String> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("pong") {
        return Ok(vec![WsEvent::Pong]);
    }
    let v: Value = serde_json::from_str(t).map_err(|e| format!("invalid WS JSON: {e}"))?;
    let mut out = Vec::new();
    match &v {
        Value::Array(items) => items.iter().for_each(|i| parse_one(i, &mut out)),
        Value::Object(_) => parse_one(&v, &mut out),
        _ => return Err("unexpected WS payload".into()),
    }
    Ok(out)
}

/// Local order book maintained from snapshots and deltas.
#[derive(Debug, Clone)]
pub struct LocalBook {
    pub token: TokenId,
    bids: BTreeMap<u32, i64>,
    asks: BTreeMap<u32, i64>,
    pub tick: Price,
    pub min_size: Shares,
    pub exchange_ts: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
    pub hash: Option<String>,
    pub valid: bool,
}

impl LocalBook {
    pub fn new(token: TokenId, now: DateTime<Utc>) -> Self {
        Self {
            token,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            tick: Price::saturating_from_micros(10_000),
            min_size: Shares::from_whole(5),
            exchange_ts: None,
            received_at: now,
            hash: None,
            valid: false,
        }
    }

    pub fn apply_snapshot(&mut self, bids: &[(Price, Shares)], asks: &[(Price, Shares)], ts: Option<DateTime<Utc>>, hash: Option<String>, now: DateTime<Utc>) {
        self.bids = bids.iter().filter(|(_, s)| s.micros() > 0).map(|(p, s)| (p.micros(), s.micros())).collect();
        self.asks = asks.iter().filter(|(_, s)| s.micros() > 0).map(|(p, s)| (p.micros(), s.micros())).collect();
        self.exchange_ts = ts;
        self.hash = hash;
        self.received_at = now;
        self.valid = true;
    }

    /// Apply a level update; size 0 removes the level. `BUY` = bid side.
    pub fn apply_change(&mut self, side: Side, price: Price, size: Shares, ts: Option<DateTime<Utc>>, now: DateTime<Utc>) {
        let book = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        if size.micros() <= 0 {
            book.remove(&price.micros());
        } else {
            book.insert(price.micros(), size.micros());
        }
        self.exchange_ts = ts.or(self.exchange_ts);
        self.received_at = now;
    }

    /// Top-`depth` snapshot for the engine.
    pub fn snapshot(&self, depth: usize) -> OrderBook {
        let lvl = |(p, s): (&u32, &i64)| BookLevel { price: Price::saturating_from_micros(*p), size: Shares::from_micros(*s) };
        OrderBook {
            token: self.token.clone(),
            bids: self.bids.iter().rev().take(depth).map(lvl).collect(),
            asks: self.asks.iter().take(depth).map(lvl).collect(),
            tick_size: self.tick,
            min_order_size: self.min_size,
            exchange_ts: self.exchange_ts,
            received_at: self.received_at,
            hash: self.hash.clone(),
        }
    }
}

/// Stream status for the dashboard.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StreamStatus {
    pub connected: bool,
    pub subscribed_assets: usize,
    pub messages_total: u64,
    pub reconnects_total: u64,
    pub last_message_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Market-channel stream configuration.
#[derive(Debug, Clone)]
pub struct MarketStreamConfig {
    pub url: String,
    pub ping_interval: Duration,
    pub book_depth: usize,
    pub max_reconnect_wait: Duration,
}

impl Default for MarketStreamConfig {
    fn default() -> Self {
        Self { url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(), ping_interval: Duration::from_secs(10), book_depth: 10, max_reconnect_wait: Duration::from_secs(300) }
    }
}

/// The market stream task.
pub struct MarketStream {
    cfg: MarketStreamConfig,
    gate: Arc<ProviderGate>,
    clock: Arc<dyn Clock>,
    books: HashMap<String, LocalBook>,
    status: StreamStatus,
    status_tx: watch::Sender<StreamStatus>,
}

impl MarketStream {
    pub fn new(cfg: MarketStreamConfig, gate: Arc<ProviderGate>, clock: Arc<dyn Clock>) -> Self {
        let (status_tx, _) = watch::channel(StreamStatus::default());
        Self { cfg, gate, clock, books: HashMap::new(), status: StreamStatus::default(), status_tx }
    }

    pub fn status(&self) -> watch::Receiver<StreamStatus> {
        self.status_tx.subscribe()
    }

    fn subscription(assets: &[TokenId]) -> String {
        serde_json::json!({ "assets_ids": assets.iter().map(|a| a.as_str()).collect::<Vec<_>>(), "type": "market" }).to_string()
    }

    async fn emit(&mut self, out: &mpsc::Sender<EventEnvelope>, events: Vec<WsEvent>) -> bool {
        let now = self.clock.now();
        let mut touched: Vec<String> = Vec::new();
        let mut trades = Vec::new();
        for e in events {
            match e {
                WsEvent::Book { asset, bids, asks, ts, hash } => {
                    let token = match TokenId::new(asset.clone()) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    let b = self.books.entry(asset.clone()).or_insert_with(|| LocalBook::new(token, now));
                    b.apply_snapshot(&bids, &asks, ts, hash, now);
                    touched.push(asset);
                }
                WsEvent::PriceChange { asset, side, price, size, ts } => {
                    if let Some(b) = self.books.get_mut(&asset)
                        && b.valid
                    {
                        b.apply_change(side, price, size, ts, now);
                        touched.push(asset);
                    }
                }
                WsEvent::TickSize { asset, tick } => {
                    if let Some(b) = self.books.get_mut(&asset) {
                        b.tick = tick;
                        touched.push(asset);
                    }
                }
                WsEvent::LastTrade { asset, price, size, side, ts } => {
                    if let Ok(token) = TokenId::new(asset) {
                        trades.push(TradePrint { token, price, size, aggressor: side, ts: ts.unwrap_or(now) });
                    }
                }
                WsEvent::Pong | WsEvent::Other => {}
            }
        }
        touched.sort();
        touched.dedup();
        for a in touched {
            if let Some(b) = self.books.get(&a) {
                let ev = WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b.snapshot(self.cfg.book_depth) });
                if out.send(EventEnvelope::new(now, EventSource::Live, ev)).await.is_err() {
                    return false;
                }
            }
        }
        for t in trades {
            let ev = WeatherMachineEvent::MarketTrade(MarketTradeEvent { trade: t });
            if out.send(EventEnvelope::new(now, EventSource::Live, ev)).await.is_err() {
                return false;
            }
        }
        true
    }

    fn publish(&self) {
        self.status_tx.send_replace(self.status.clone());
    }

    /// Run until shutdown. `assets` may change at runtime (re-subscribe).
    pub async fn run(mut self, mut assets: watch::Receiver<Vec<TokenId>>, out: mpsc::Sender<EventEnvelope>, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let current: Vec<TokenId> = assets.borrow().clone();
            if current.is_empty() {
                tokio::select! {
                    r = assets.changed() => { if r.is_err() { break; } continue; }
                    _ = shutdown.changed() => break,
                }
            }
            // Reconnects are rate limited like any other request.
            let permit = match self.gate.acquire(self.cfg.max_reconnect_wait).await {
                Ok(p) => p,
                Err(w) => {
                    tokio::select! {
                        _ = tokio::time::sleep(w.retry_in.max(Duration::from_secs(1))) => continue,
                        _ = shutdown.changed() => break,
                    }
                }
            };
            let conn = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(self.cfg.url.as_str())).await;
            let mut ws = match conn {
                Ok(Ok((ws, _))) => {
                    permit.complete(RequestOutcome::Success { status: 101 });
                    ws
                }
                Ok(Err(e)) => {
                    permit.complete(RequestOutcome::Connect);
                    self.status.last_error = Some(e.to_string());
                    self.publish();
                    continue;
                }
                Err(_) => {
                    permit.complete(RequestOutcome::Timeout);
                    self.status.last_error = Some("connect timeout".into());
                    self.publish();
                    continue;
                }
            };
            if ws.send(Message::text(Self::subscription(&current))).await.is_err() {
                continue;
            }
            self.status.connected = true;
            self.status.subscribed_assets = current.len();
            self.publish();
            let mut ping = tokio::time::interval(self.cfg.ping_interval);
            ping.tick().await;
            let reason = loop {
                tokio::select! {
                    msg = ws.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            self.status.messages_total += 1;
                            self.status.last_message_at = Some(self.clock.now());
                            match parse_ws_message(t.as_str()) {
                                Ok(events) => { if !self.emit(&out, events).await { break "receiver closed".to_owned(); } }
                                Err(e) => { tracing::warn!(error = %e, "unparseable WS message"); }
                            }
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                        Some(Ok(Message::Close(_))) | None => break "closed by server".to_owned(),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => break e.to_string(),
                    },
                    _ = ping.tick() => { if ws.send(Message::text("PING")).await.is_err() { break "ping failed".to_owned(); } }
                    r = assets.changed() => {
                        if r.is_err() { break "asset channel closed".to_owned(); }
                        let next: Vec<TokenId> = assets.borrow().clone();
                        let msg = serde_json::json!({ "assets_ids": next.iter().map(|a| a.as_str()).collect::<Vec<_>>(), "operation": "subscribe" }).to_string();
                        if ws.send(Message::text(msg)).await.is_err() { break "resubscribe failed".to_owned(); }
                        self.status.subscribed_assets = next.len();
                    }
                    _ = shutdown.changed() => { let _ = ws.close(None).await; self.status.connected = false; self.publish(); return; }
                }
                self.publish();
            };
            // Disconnected: books are no longer trustworthy until a new snapshot.
            for b in self.books.values_mut() {
                b.valid = false;
            }
            self.status.connected = false;
            self.status.reconnects_total += 1;
            self.status.last_error = Some(reason);
            self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_book_and_price_changes_both_formats() {
        let book = r#"[{"event_type":"book","asset_id":"1018","market":"0xc","bids":[{"price":"0.93","size":"100"}],"asks":[{"price":"0.95","size":"50"}],"timestamp":"1790427300000","hash":"0x1"}]"#;
        let ev = parse_ws_message(book).unwrap();
        assert!(matches!(&ev[0], WsEvent::Book { asset, bids, .. } if asset == "1018" && bids.len() == 1));
        let new_fmt = r#"{"event_type":"price_change","market":"0xc","price_changes":[{"asset_id":"1018","price":"0.94","size":"25","side":"BUY","hash":"h","best_bid":"0.94","best_ask":"0.95"}],"timestamp":"1790427301000"}"#;
        let ev = parse_ws_message(new_fmt).unwrap();
        assert_eq!(ev, vec![WsEvent::PriceChange { asset: "1018".into(), side: Side::Buy, price: Price::parse("0.94").unwrap(), size: Shares::from_whole(25), ts: crate::clob::parse_epoch(&serde_json::json!("1790427301000")) }]);
        let legacy = r#"{"event_type":"price_change","asset_id":"1018","changes":[{"price":"0.95","side":"SELL","size":"0"}]}"#;
        let ev = parse_ws_message(legacy).unwrap();
        assert!(matches!(&ev[0], WsEvent::PriceChange { side: Side::Sell, size, .. } if size.micros() == 0));
        let tick = r#"{"event_type":"tick_size_change","asset_id":"1018","old_tick_size":"0.01","new_tick_size":"0.001"}"#;
        assert_eq!(parse_ws_message(tick).unwrap(), vec![WsEvent::TickSize { asset: "1018".into(), tick: Price::parse("0.001").unwrap() }]);
        let trade = r#"{"event_type":"last_trade_price","asset_id":"1018","price":"0.95","side":"BUY","size":"10"}"#;
        assert!(matches!(parse_ws_message(trade).unwrap()[0], WsEvent::LastTrade { .. }));
        assert_eq!(parse_ws_message("PONG").unwrap(), vec![WsEvent::Pong]);
        assert!(parse_ws_message("{garbage").is_err());
    }

    #[test]
    fn local_book_applies_deltas() {
        let now = Utc::now();
        let mut b = LocalBook::new(TokenId::new("1").unwrap(), now);
        b.apply_snapshot(&[(Price::parse("0.93").unwrap(), Shares::from_whole(100))], &[(Price::parse("0.95").unwrap(), Shares::from_whole(50))], None, None, now);
        b.apply_change(Side::Buy, Price::parse("0.94").unwrap(), Shares::from_whole(10), None, now);
        b.apply_change(Side::Sell, Price::parse("0.95").unwrap(), Shares::ZERO, None, now);
        b.apply_change(Side::Sell, Price::parse("0.96").unwrap(), Shares::from_whole(5), None, now);
        let s = b.snapshot(5);
        assert_eq!(s.best_bid().unwrap().price, Price::parse("0.94").unwrap());
        assert_eq!(s.best_ask().unwrap().price, Price::parse("0.96").unwrap());
        assert_eq!(s.bids.len(), 2);
    }
}
