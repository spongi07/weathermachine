//! Data API (public trade history) — read-only, rate-limited, research only.
//!
//! Base URL `https://data-api.polymarket.com`; `GET /trades` lists executed
//! trades filtered by `market` (condition ids). `takerOnly=true` (the
//! default) reports each match once, from the taker's side. `limit` and
//! `offset` are capped at 10,000 (a larger offset is rejected, not clamped);
//! `start`/`end` (epoch seconds) bound the window, so deeper history is read
//! window by window. Numbers may arrive as JSON numbers or strings.
//!
//! `research market` uses it to measure how quickly the market reprices and
//! how well its prices predict; trading never does.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ConditionId, TokenId};
use wm_core::market::Side;
use wm_net::{FetchError, FetchRequest, HttpFetcher};

/// Page size requested (the API allows up to 10,000; smaller pages keep
/// responses small).
pub const PAGE_LIMIT: u32 = 500;
/// Largest offset the API accepts.
pub const MAX_OFFSET: u32 = 10_000;
/// Upper bound on requests for one query (guards against an API that
/// ignores the window and would make splitting endless).
const MAX_REQUESTS: u32 = 100;
/// Attempts per page when the API throttles or fails transiently (the gate
/// spaces them by the server's Retry-After and the policy's backoff).
pub const PAGE_ATTEMPTS: u32 = 5;

/// One executed trade as reported (taker side).
#[derive(Debug, Clone, PartialEq)]
pub struct DataTrade {
    pub at: DateTime<Utc>,
    pub condition_id: String,
    pub asset: TokenId,
    /// The taker's side.
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub transaction_hash: String,
    /// The taker's proxy wallet (used only to count distinct traders).
    pub taker: Option<String>,
}

fn number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }))
}

fn text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTrade {
    #[serde(default, deserialize_with = "text")]
    proxy_wallet: Option<String>,
    #[serde(default, deserialize_with = "text")]
    side: Option<String>,
    #[serde(default, deserialize_with = "text")]
    asset: Option<String>,
    #[serde(default, deserialize_with = "text")]
    condition_id: Option<String>,
    #[serde(default, deserialize_with = "number")]
    size: Option<f64>,
    #[serde(default, deserialize_with = "number")]
    price: Option<f64>,
    #[serde(default, deserialize_with = "number")]
    timestamp: Option<f64>,
    #[serde(default, deserialize_with = "text")]
    transaction_hash: Option<String>,
}

impl RawTrade {
    fn into_trade(self) -> Option<DataTrade> {
        let side = match self.side.as_deref().map(str::to_ascii_uppercase).as_deref() {
            Some("BUY") => Side::Buy,
            Some("SELL") => Side::Sell,
            _ => return None,
        };
        let (price, size, ts) = (self.price?, self.size?, self.timestamp?);
        let usable = (0.0..=1.0).contains(&price)
            && size.is_finite()
            && size > 0.0
            && ts.is_finite()
            && ts > 0.0;
        if !usable {
            return None;
        }
        // Seconds; a millisecond value would be a date far in the future.
        let secs = if ts > 1e11 { ts / 1000.0 } else { ts };
        #[allow(clippy::cast_possible_truncation)]
        let at = DateTime::from_timestamp(secs.floor() as i64, 0)?;
        Some(DataTrade {
            at,
            condition_id: self.condition_id.unwrap_or_default(),
            asset: TokenId::new(self.asset?).ok()?,
            side,
            price,
            size,
            transaction_hash: self.transaction_hash.unwrap_or_default(),
            taker: self.proxy_wallet,
        })
    }
}

/// Parse a `/trades` page. Records lacking a side, asset, price, size or
/// time (or with impossible values) are skipped and counted.
pub fn parse_trades(body: &[u8]) -> Result<(Vec<DataTrade>, usize), String> {
    let raw: Vec<RawTrade> =
        serde_json::from_slice(body).map_err(|e| format!("invalid Data API trades JSON: {e}"))?;
    let total = raw.len();
    let trades: Vec<DataTrade> = raw.into_iter().filter_map(RawTrade::into_trade).collect();
    let skipped = total - trades.len();
    Ok((trades, skipped))
}

/// All trades of a set of markets in a window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeHistory {
    /// Oldest first, duplicates removed, inside the requested window.
    pub trades: Vec<DataTrade>,
    pub requests: u32,
    /// Records the API returned that could not be used.
    pub skipped: usize,
    /// The API's offset cap was reached in a window that could not be split
    /// further (or the request budget ran out): the history may be incomplete.
    pub truncated: bool,
}

/// Read-only Data API client.
pub struct DataApiClient {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
}

impl DataApiClient {
    pub const DEFAULT_BASE: &'static str = "https://data-api.polymarket.com";

    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// One page of taker trades of `markets` within `[start, end]` (seconds),
    /// retried while the failure is transient (throttling, 5xx, timeouts).
    pub async fn trades_page(
        &self,
        markets: &[ConditionId],
        start: i64,
        end: i64,
        offset: u32,
        max_gate_wait: Duration,
    ) -> Result<(Vec<DataTrade>, usize), DataApiError> {
        let list = markets
            .iter()
            .map(ConditionId::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let endpoint = format!(
            "/trades?market={list}&takerOnly=true&limit={PAGE_LIMIT}&offset={offset}&start={start}&end={end}"
        );
        let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint)
            .accept("application/json")
            .unconditional()
            .max_gate_wait(max_gate_wait);
        let resp = self.fetcher.get_retrying(&req, PAGE_ATTEMPTS).await?;
        parse_trades(&resp.body).map_err(DataApiError::Malformed)
    }

    /// Every taker trade of `markets` (e.g. all buckets of one event) between
    /// `start` and `end`, paging by offset and halving the window whenever
    /// the offset cap is reached.
    pub async fn trades(
        &self,
        markets: &[ConditionId],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        max_gate_wait: Duration,
    ) -> Result<TradeHistory, DataApiError> {
        let mut out = TradeHistory::default();
        if markets.is_empty() || end < start {
            return Ok(out);
        }
        let mut windows = vec![(start.timestamp(), end.timestamp())];
        while let Some((s, e)) = windows.pop() {
            let mut offset = 0u32;
            let mut window = Vec::new();
            loop {
                if out.requests >= MAX_REQUESTS {
                    out.truncated = true;
                    out.trades.append(&mut window);
                    windows.clear();
                    break;
                }
                let (page, skipped) = self
                    .trades_page(markets, s, e, offset, max_gate_wait)
                    .await?;
                out.requests += 1;
                out.skipped += skipped;
                let full = page.len() + skipped >= PAGE_LIMIT as usize;
                window.extend(page);
                if !full {
                    out.trades.append(&mut window);
                    break;
                }
                if offset + PAGE_LIMIT > MAX_OFFSET {
                    if e > s {
                        // Too many trades for one window: read both halves.
                        let mid = s + (e - s) / 2;
                        windows.push((mid + 1, e));
                        windows.push((s, mid));
                    } else {
                        out.truncated = true;
                        out.trades.append(&mut window);
                    }
                    break;
                }
                offset += PAGE_LIMIT;
            }
        }
        // The API may ignore the window: keep what was asked for.
        out.trades.retain(|t| t.at >= start && t.at <= end);
        out.trades.sort_by(|a, b| {
            (a.at, &a.transaction_hash, a.asset.as_str())
                .cmp(&(b.at, &b.transaction_hash, b.asset.as_str()))
                .then(a.price.total_cmp(&b.price))
                .then(a.size.total_cmp(&b.size))
        });
        out.trades.dedup_by(|a, b| {
            a.at == b.at
                && a.transaction_hash == b.transaction_hash
                && a.asset == b.asset
                && a.side == b.side
                && a.price.to_bits() == b.price.to_bits()
                && a.size.to_bits() == b.size.to_bits()
        });
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DataApiError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("malformed response: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_strings_and_skips_unusable_records() {
        let body = br#"[
            {"proxyWallet":"0xabc","side":"BUY","asset":"1018","conditionId":"0xc1","size":12.5,"price":0.97,"timestamp":1790427300,"outcome":"No","outcomeIndex":1,"transactionHash":"0xt1","name":"someone"},
            {"proxyWallet":"0xdef","side":"sell","asset":"1017","conditionId":"0xc1","size":"4","price":"0.03","timestamp":"1790427360","transactionHash":"0xt2"},
            {"side":"BUY","asset":"1018","size":1,"price":1.5,"timestamp":1790427300},
            {"side":"HOLD","asset":"1018","size":1,"price":0.5,"timestamp":1790427300},
            {"side":"BUY","asset":"1018","size":0,"price":0.5,"timestamp":1790427300},
            {"side":"BUY","size":1,"price":0.5,"timestamp":1790427300}
        ]"#;
        let (trades, skipped) = parse_trades(body).unwrap();
        assert_eq!((trades.len(), skipped), (2, 4));
        let t = &trades[0];
        assert_eq!(t.side, Side::Buy);
        assert_eq!(t.asset.as_str(), "1018");
        assert!((t.price - 0.97).abs() < 1e-12 && (t.size - 12.5).abs() < 1e-12);
        assert_eq!(t.at.timestamp(), 1_790_427_300);
        assert_eq!(t.taker.as_deref(), Some("0xabc"));
        assert_eq!(trades[1].side, Side::Sell);
        assert!((trades[1].price - 0.03).abs() < 1e-12);
        assert_eq!(trades[1].at.timestamp(), 1_790_427_360);
    }

    #[test]
    fn millisecond_timestamps_are_read_as_seconds() {
        let body =
            br#"[{"side":"BUY","asset":"1","size":1,"price":0.5,"timestamp":1790427300123}]"#;
        let (trades, _) = parse_trades(body).unwrap();
        assert_eq!(trades[0].at.timestamp(), 1_790_427_300);
    }

    #[test]
    fn a_non_array_body_is_malformed() {
        assert!(parse_trades(br#"{"error":"bad"}"#).is_err());
        assert_eq!(parse_trades(b"[]").unwrap(), (Vec::new(), 0));
    }
}
