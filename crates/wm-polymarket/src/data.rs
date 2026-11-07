//! Data API v2 (public trade history) — read-only, rate-limited; trading
//! never uses it.
//!
//! Base URL `https://data-api.polymarket.com`; `GET /v2/trades` lists
//! executed trades filtered by `condition` (at most [`MAX_CONDITIONS`]
//! condition ids, comma-separated); `start`/`end` (epoch seconds) bound the
//! window. Every answer wraps its rows in `data`; a page's
//! `pagination.next_cursor` names the next one (sent back as `cursor`, with
//! the same filters), null on the last, and `has_more`, not the page's
//! length, says whether there is one. Rows are snake_case (`proxy_wallet`,
//! `token_id`, `condition_id`, `transaction_hash`); numbers may arrive as
//! JSON numbers or strings.
//!
//! v1 (`/trades?market=…&takerOnly=true&offset=…`, bare lists, offsets
//! capped at 10,000) was retired on 2 November 2026 and answers 410; v2
//! answers 400 to its `market` and `offset`. v1 reported each match once,
//! from the taker's side, by default; the v2 documentation as read in
//! November 2026 names no taker filter, so the rows are read as the takers'.
//!
//! `research market` uses it to measure how quickly the market reprices and
//! how well its prices predict, and the strategy lab reads today's trades.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ConditionId, TokenId};
use wm_core::market::Side;
use wm_net::{FetchError, FetchRequest, HttpFetcher};

/// Page size requested.
pub const PAGE_LIMIT: u32 = 500;
/// Condition ids one request filters by at most; more are asked for in
/// groups.
pub const MAX_CONDITIONS: usize = 20;
/// Upper bound on requests for one query (guards against a cursor that
/// never ends).
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

/// The first of `names` the row has (not null). v1's camelCase names are
/// read too.
fn field<'a>(row: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|n| row.get(*n).filter(|v| !v.is_null()))
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A row as a trade; `None` when it lacks a side, token, price, size or
/// time, or holds an impossible value.
fn trade(row: &Value) -> Option<DataTrade> {
    let side = match field(row, &["side"])
        .and_then(Value::as_str)
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("BUY") => Side::Buy,
        Some("SELL") => Side::Sell,
        _ => return None,
    };
    let price = field(row, &["price"]).and_then(number)?;
    let size = field(row, &["size"]).and_then(number)?;
    let ts = field(row, &["timestamp"]).and_then(number)?;
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
    let token = field(row, &["token_id", "asset_id", "asset"]).and_then(text)?;
    Some(DataTrade {
        at,
        condition_id: field(row, &["condition_id", "conditionId"])
            .and_then(text)
            .unwrap_or_default(),
        asset: TokenId::new(token).ok()?,
        side,
        price,
        size,
        transaction_hash: field(row, &["transaction_hash", "transactionHash"])
            .and_then(text)
            .unwrap_or_default(),
        taker: field(row, &["proxy_wallet", "proxyWallet"]).and_then(text),
    })
}

/// One `/v2/trades` page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradesPage {
    pub trades: Vec<DataTrade>,
    /// Records that could not be used (skipped and counted).
    pub skipped: usize,
    /// The next page's cursor (`None`: the last page).
    pub next: Option<String>,
}

/// Parse a `/v2/trades` page.
pub fn parse_trades(body: &[u8]) -> Result<TradesPage, String> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid Data API trades JSON: {e}"))?;
    let rows = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or("a Data API answer without a data list")?;
    let trades: Vec<DataTrade> = rows.iter().filter_map(trade).collect();
    Ok(TradesPage {
        skipped: rows.len() - trades.len(),
        trades,
        next: next_cursor(&v)?,
    })
}

/// A v2 page's next cursor: `pagination.next_cursor`, none once `has_more`
/// is false; `has_more` without a cursor is malformed.
pub fn next_cursor(v: &Value) -> Result<Option<String>, String> {
    let pagination = v.get("pagination");
    let next = pagination
        .and_then(|p| p.get("next_cursor"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    match pagination
        .and_then(|p| p.get("has_more"))
        .and_then(Value::as_bool)
    {
        Some(false) => Ok(None),
        Some(true) if next.is_none() => Err("has_more without a next_cursor".into()),
        _ => Ok(next),
    }
}

/// `s` for a URL's query: every byte but the unreserved ones
/// percent-encoded (a cursor is opaque: base64's `+`, `/` and `=` too).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// All trades of a set of markets in a window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeHistory {
    /// Oldest first, duplicates removed, inside the requested window.
    pub trades: Vec<DataTrade>,
    pub requests: u32,
    /// Records the API returned that could not be used.
    pub skipped: usize,
    /// The request budget ran out before the last page: the history may be
    /// incomplete.
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

    /// One page of the trades of `markets` (at most [`MAX_CONDITIONS`])
    /// within `[start, end]` (seconds), from `cursor` (`None`: the first
    /// page), retried while the failure is transient (throttling, 5xx,
    /// timeouts).
    pub async fn trades_page(
        &self,
        markets: &[ConditionId],
        start: i64,
        end: i64,
        cursor: Option<&str>,
        max_gate_wait: Duration,
    ) -> Result<TradesPage, DataApiError> {
        let list = markets
            .iter()
            .map(ConditionId::as_str)
            .collect::<Vec<_>>()
            .join(",");
        let mut endpoint =
            format!("/v2/trades?condition={list}&limit={PAGE_LIMIT}&start={start}&end={end}");
        if let Some(c) = cursor {
            endpoint.push_str("&cursor=");
            endpoint.push_str(&percent_encode(c));
        }
        let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint)
            .accept("application/json")
            .unconditional()
            .max_gate_wait(max_gate_wait);
        let resp = self.fetcher.get_retrying(&req, PAGE_ATTEMPTS).await?;
        parse_trades(&resp.body).map_err(DataApiError::Malformed)
    }

    /// Every trade of `markets` (e.g. all buckets of one event) between
    /// `start` and `end`, page by page along the cursor, the markets in
    /// groups of [`MAX_CONDITIONS`].
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
        let (s, e) = (start.timestamp(), end.timestamp());
        'groups: for group in markets.chunks(MAX_CONDITIONS) {
            let mut cursor: Option<String> = None;
            loop {
                if out.requests >= MAX_REQUESTS {
                    out.truncated = true;
                    break 'groups;
                }
                let page = self
                    .trades_page(group, s, e, cursor.as_deref(), max_gate_wait)
                    .await?;
                out.requests += 1;
                out.skipped += page.skipped;
                out.trades.extend(page.trades);
                match page.next {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
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
        let body = br#"{"data": [
            {"proxy_wallet":"0xabc","side":"BUY","token_id":"1018","condition_id":"0xc1","size":12.5,"price":0.97,"timestamp":1790427300,"outcome":"No","transaction_hash":"0xt1","title":"someone"},
            {"proxy_wallet":"0xdef","side":"sell","token_id":"1017","condition_id":"0xc1","size":"4","price":"0.03","timestamp":"1790427360","transaction_hash":"0xt2"},
            {"side":"BUY","token_id":"1018","size":1,"price":1.5,"timestamp":1790427300},
            {"side":"HOLD","token_id":"1018","size":1,"price":0.5,"timestamp":1790427300},
            {"side":"BUY","token_id":"1018","size":0,"price":0.5,"timestamp":1790427300},
            {"side":"BUY","size":1,"price":0.5,"timestamp":1790427300}
        ], "pagination": {"limit": 500, "has_more": false, "next_cursor": null}}"#;
        let page = parse_trades(body).unwrap();
        assert_eq!((page.trades.len(), page.skipped, page.next), (2, 4, None));
        let t = &page.trades[0];
        assert_eq!(t.side, Side::Buy);
        assert_eq!(t.asset.as_str(), "1018");
        assert_eq!(t.condition_id, "0xc1");
        assert_eq!(t.transaction_hash, "0xt1");
        assert!((t.price - 0.97).abs() < 1e-12 && (t.size - 12.5).abs() < 1e-12);
        assert_eq!(t.at.timestamp(), 1_790_427_300);
        assert_eq!(t.taker.as_deref(), Some("0xabc"));
        assert_eq!(page.trades[1].side, Side::Sell);
        assert!((page.trades[1].price - 0.03).abs() < 1e-12);
        assert_eq!(page.trades[1].at.timestamp(), 1_790_427_360);
    }

    #[test]
    fn millisecond_timestamps_are_read_as_seconds() {
        let body = br#"{"data":[{"side":"BUY","token_id":"1","size":1,"price":0.5,"timestamp":1790427300123}]}"#;
        let page = parse_trades(body).unwrap();
        assert_eq!(page.trades[0].at.timestamp(), 1_790_427_300);
    }

    #[test]
    fn the_next_page_is_named_by_its_cursor() {
        let page = |p: serde_json::Value| {
            parse_trades(
                serde_json::json!({ "data": [], "pagination": p })
                    .to_string()
                    .as_bytes(),
            )
        };
        assert_eq!(
            page(serde_json::json!({"has_more": true, "next_cursor": "a+b/c="}))
                .unwrap()
                .next
                .as_deref(),
            Some("a+b/c=")
        );
        assert_eq!(
            page(serde_json::json!({"has_more": false, "next_cursor": "x"}))
                .unwrap()
                .next,
            None
        );
        assert_eq!(
            page(serde_json::json!({"next_cursor": null})).unwrap().next,
            None
        );
        assert!(page(serde_json::json!({"has_more": true})).is_err());
    }

    #[test]
    fn a_body_without_a_data_list_is_malformed() {
        assert!(parse_trades(br#"{"error":"bad"}"#).is_err());
        // v1's bare list.
        assert!(parse_trades(b"[]").is_err());
        assert_eq!(
            parse_trades(br#"{"data":[]}"#).unwrap(),
            TradesPage::default()
        );
    }

    #[test]
    fn cursors_are_percent_encoded() {
        assert_eq!(percent_encode("eyJ+a/b=="), "eyJ%2Ba%2Fb%3D%3D");
        assert_eq!(percent_encode("abc-._~9"), "abc-._~9");
    }
}
