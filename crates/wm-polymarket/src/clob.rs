//! CLOB REST (read-only): order books and price history.

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::TokenId;
use wm_core::market::{BookLevel, OrderBook};
use wm_core::units::{Price, Shares};
use wm_net::{FetchError, FetchRequest, HttpFetcher};

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct RawBook {
    asset_id: Option<String>,
    timestamp: Option<serde_json::Value>,
    hash: Option<String>,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
    tick_size: Option<serde_json::Value>,
    min_order_size: Option<serde_json::Value>,
}

fn value_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse a millisecond (or second) epoch timestamp given as string/number.
pub fn parse_epoch(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    let n: i64 = value_str(v)?.parse().ok()?;
    if n > 10_000_000_000 {
        Utc.timestamp_millis_opt(n).single()
    } else {
        Utc.timestamp_opt(n, 0).single()
    }
}

pub(crate) fn levels(raw: &[(String, String)]) -> Vec<BookLevel> {
    raw.iter()
        .filter_map(|(p, s)| {
            Some(BookLevel {
                price: Price::parse(p).ok()?,
                size: Shares::parse(s).ok()?,
            })
        })
        .collect()
}

/// Parse a `GET /book` body into a normalized [`OrderBook`].
pub fn parse_book(
    body: &[u8],
    token: &TokenId,
    received_at: DateTime<Utc>,
) -> Result<OrderBook, String> {
    let raw: RawBook =
        serde_json::from_slice(body).map_err(|e| format!("invalid book JSON: {e}"))?;
    if let Some(a) = &raw.asset_id
        && a != token.as_str()
    {
        return Err(format!("book asset {a} != requested {token}"));
    }
    let bids: Vec<(String, String)> = raw.bids.into_iter().map(|l| (l.price, l.size)).collect();
    let asks: Vec<(String, String)> = raw.asks.into_iter().map(|l| (l.price, l.size)).collect();
    let mut book = OrderBook {
        token: token.clone(),
        bids: levels(&bids),
        asks: levels(&asks),
        tick_size: raw
            .tick_size
            .as_ref()
            .and_then(value_str)
            .and_then(|t| Price::parse(&t).ok())
            .unwrap_or(Price::saturating_from_micros(10_000)),
        min_order_size: raw
            .min_order_size
            .as_ref()
            .and_then(value_str)
            .and_then(|t| Shares::parse(&t).ok())
            .unwrap_or(Shares::from_whole(5)),
        exchange_ts: raw.timestamp.as_ref().and_then(parse_epoch),
        received_at,
        hash: raw.hash,
        confirmed_at: None,
    };
    book.normalize();
    Ok(book)
}

/// One point of `/prices-history`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricePoint {
    pub t: DateTime<Utc>,
    pub p: Price,
}

/// Parse `{"history":[{"t":…,"p":…}]}`.
pub fn parse_price_history(body: &[u8]) -> Result<Vec<PricePoint>, String> {
    #[derive(Deserialize)]
    struct H {
        history: Vec<P>,
    }
    #[derive(Deserialize)]
    struct P {
        t: i64,
        p: f64,
    }
    let h: H =
        serde_json::from_slice(body).map_err(|e| format!("invalid prices-history JSON: {e}"))?;
    Ok(h.history
        .into_iter()
        .filter_map(|x| {
            Some(PricePoint {
                t: Utc.timestamp_opt(x.t, 0).single()?,
                p: Price::from_f64(x.p).ok()?,
            })
        })
        .collect())
}

/// Read-only CLOB client.
pub struct ClobClient {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
}

impl ClobClient {
    pub const DEFAULT_BASE: &'static str = "https://clob.polymarket.com";

    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn book(
        &self,
        token: &TokenId,
        max_gate_wait: Duration,
    ) -> Result<OrderBook, ClobError> {
        let endpoint = format!("/book?token_id={token}");
        let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint)
            .unconditional()
            .max_gate_wait(max_gate_wait);
        let resp = self.fetcher.get(&req).await?;
        parse_book(&resp.body, token, resp.fetched_at).map_err(ClobError::Malformed)
    }

    /// Historical prices (research). `fidelity` is the bucket in minutes.
    pub async fn prices_history(
        &self,
        token: &TokenId,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fidelity_minutes: u32,
        max_gate_wait: Duration,
    ) -> Result<Vec<PricePoint>, ClobError> {
        let endpoint = format!(
            "/prices-history?market={token}&startTs={}&endTs={}&fidelity={fidelity_minutes}",
            start.timestamp(),
            end.timestamp()
        );
        let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint)
            .unconditional()
            .max_gate_wait(max_gate_wait);
        let resp = self.fetcher.get(&req).await?;
        parse_price_history(&resp.body).map_err(ClobError::Malformed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClobError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("malformed response: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn book_parses_and_normalizes() {
        let body =
            br#"{"market":"0xc","asset_id":"1018","timestamp":"1790427300123","hash":"0xabc",
            "bids":[{"price":"0.90","size":"10"},{"price":"0.93","size":"120.5"}],
            "asks":[{"price":"0.99","size":"5"},{"price":"0.95","size":"80"}],
            "min_order_size":"5","tick_size":"0.01","neg_risk":true}"#;
        let t = TokenId::new("1018").unwrap();
        let b = parse_book(body, &t, Utc::now()).unwrap();
        assert_eq!(b.best_bid().unwrap().price, Price::parse("0.93").unwrap());
        assert_eq!(b.best_bid().unwrap().size, Shares::parse("120.5").unwrap());
        assert_eq!(b.best_ask().unwrap().price, Price::parse("0.95").unwrap());
        assert_eq!(b.exchange_ts.unwrap().timestamp_millis(), 1_790_427_300_123);
        assert_eq!(b.tick_size, Price::parse("0.01").unwrap());
        assert!(
            parse_book(body, &TokenId::new("999").unwrap(), Utc::now()).is_err(),
            "asset mismatch"
        );
        assert!(parse_book(b"nope", &t, Utc::now()).is_err());
    }

    #[test]
    fn price_history_parses() {
        let pts = parse_price_history(
            br#"{"history":[{"t":1790427300,"p":0.955},{"t":1790427360,"p":0.96}]}"#,
        )
        .unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].p, Price::parse("0.955").unwrap());
    }
}
