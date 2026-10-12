//! The CLOB's REST endpoints this venue uses, authenticated with L2 headers
//! computed over the exact bytes sent (paths as the official client names
//! them, `rs-clob-client-v2`, read 2026-10-12):
//!
//! | call | endpoint |
//! |---|---|
//! | protocol version | `GET /version` |
//! | API credentials (L1) | `GET /auth/derive-api-key`, else `POST /auth/api-key` |
//! | closed-only ban | `GET /auth/ban-status/closed-only` |
//! | place | `POST /order` |
//! | cancel | `DELETE /order` `{"orderID": …}` |
//! | an order's record | `GET /data/order/{id}` |
//! | open orders | `GET /data/orders` (paged) |
//! | collateral | `GET /balance-allowance` (and `/update`) |
//! | dead-man's switch | `POST /v1/heartbeats` |
//!
//! Plus two public reads: the geoblock (`polymarket.com/api/geoblock`) and
//! the account's positions (Data API `/positions`). Nothing here logs a key,
//! a credential or the address the geoblock answer names.

use crate::answer::{self, OpenOrder, OrderRecord};
use crate::auth::{self, ApiCreds};
use crate::geo::{self, Geo};
use crate::sign::{CHAIN_ID, OrderSigner};
use std::sync::Arc;
use std::time::Duration;
use wm_core::units::{Shares, Usd};

/// The signer and its API credentials.
#[derive(Clone)]
pub struct Auth {
    pub signer: Arc<OrderSigner>,
    pub creds: ApiCreds,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Auth({}, creds=***)", self.signer.address)
    }
}

/// One request's answer as it came: the status and body, or how the
/// transport failed (`true`: before the request was written).
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub status: Option<u16>,
    pub body: Vec<u8>,
    pub transport: Option<(bool, String)>,
}

impl Answer {
    fn of(r: Result<(u16, Vec<u8>), reqwest::Error>) -> Self {
        match r {
            Ok((status, body)) => Self {
                status: Some(status),
                body,
                transport: None,
            },
            Err(e) => Self {
                status: None,
                body: Vec::new(),
                // a connect failure never wrote the request
                transport: Some((e.is_connect(), e.without_url().to_string())),
            },
        }
    }

    fn json(&self) -> Result<serde_json::Value, String> {
        match (self.status, &self.transport) {
            (_, Some((_, why))) => Err(why.clone()),
            (Some(s), _) if (200..300).contains(&s) => {
                serde_json::from_slice(&self.body).map_err(|e| format!("json: {e}"))
            }
            (s, _) => Err(format!(
                "http {}: {}",
                s.unwrap_or(0),
                String::from_utf8_lossy(&self.body[..self.body.len().min(200)])
            )),
        }
    }
}

/// The CLOB.
#[derive(Debug, Clone)]
pub struct Clob {
    http: reqwest::Client,
    base: String,
}

fn now_s() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
}

impl Clob {
    pub fn new(base: &str, user_agent: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .connect_timeout(Duration::from_secs(4))
            .user_agent(user_agent)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    async fn send(
        &self,
        auth: Option<&Auth>,
        method: reqwest::Method,
        path: &str,
        query: &str,
        body: Option<Vec<u8>>,
    ) -> Answer {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}{query}", self.base));
        if let Some(a) = auth {
            // the HMAC covers the path without the query, and the exact body
            match auth::l2_headers(
                &a.signer,
                &a.creds,
                method.as_str(),
                path,
                body.as_deref(),
                now_s(),
            ) {
                Ok(headers) => {
                    for (k, v) in headers {
                        req = req.header(k, v);
                    }
                }
                Err(e) => {
                    return Answer {
                        transport: Some((true, format!("auth: {e}"))),
                        ..Answer::default()
                    };
                }
            }
        }
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").body(b);
        }
        let r = async {
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            let body = resp.bytes().await?.to_vec();
            Ok::<_, reqwest::Error>((status, body))
        }
        .await;
        Answer::of(r)
    }

    /// The protocol the CLOB runs (`{"version": 2}`).
    pub async fn version(&self) -> Result<u32, String> {
        let v = self
            .send(None, reqwest::Method::GET, "/version", "", None)
            .await
            .json()?;
        v["version"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("no version in {v}"))
    }

    /// Derive the API credentials of `nonce`, or create them when the
    /// venue has none for it. The key and the credentials never reach a log.
    pub async fn credentials(&self, signer: &OrderSigner, nonce: u64) -> Result<ApiCreds, String> {
        let l1 = auth::l1_headers(signer, CHAIN_ID, now_s(), nonce)?;
        let ask = |method: reqwest::Method, path: &str| {
            let mut req = self.http.request(method, format!("{}{path}", self.base));
            for (k, v) in &l1 {
                req = req.header(*k, v);
            }
            req
        };
        if let Ok(r) = ask(reqwest::Method::GET, "/auth/derive-api-key")
            .send()
            .await
            && r.status().is_success()
            && let Ok(c) = r.json::<ApiCreds>().await
            && !c.api_key.is_empty()
        {
            return Ok(c);
        }
        let r = ask(reqwest::Method::POST, "/auth/api-key")
            .send()
            .await
            .map_err(|e| format!("create api key: {}", e.without_url()))?;
        let status = r.status();
        if status.is_success() {
            return r
                .json::<ApiCreds>()
                .await
                .map_err(|e| format!("create api key: {}", e.without_url()));
        }
        Err(format!(
            "could not derive or create API credentials (http {status})"
        ))
    }

    /// Whether the account may only close positions.
    pub async fn closed_only(&self, a: &Auth) -> Result<bool, String> {
        let v = self
            .send(
                Some(a),
                reqwest::Method::GET,
                "/auth/ban-status/closed-only",
                "",
                None,
            )
            .await
            .json()?;
        v["closed_only"]
            .as_bool()
            .ok_or_else(|| format!("no closed_only in {v}"))
    }

    pub async fn post_order(&self, a: &Auth, body: &[u8]) -> Answer {
        self.send(
            Some(a),
            reqwest::Method::POST,
            "/order",
            "",
            Some(body.to_vec()),
        )
        .await
    }

    pub async fn cancel(&self, a: &Auth, order_id: &str) -> Answer {
        let body = serde_json::json!({ "orderID": order_id })
            .to_string()
            .into_bytes();
        self.send(Some(a), reqwest::Method::DELETE, "/order", "", Some(body))
            .await
    }

    /// The venue's record of an order (`None`: it has none).
    pub async fn record(&self, a: &Auth, order_id: &str) -> Result<Option<OrderRecord>, String> {
        let path = format!("/data/order/{order_id}");
        let ans = self
            .send(Some(a), reqwest::Method::GET, &path, "", None)
            .await;
        if let Some((_, why)) = ans.transport {
            return Err(why);
        }
        answer::parse_record(ans.status.unwrap_or(0), &ans.body)
    }

    /// Every open order of the account (at most ten pages).
    pub async fn open_orders(&self, a: &Auth) -> Result<Vec<OpenOrder>, String> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..10 {
            let query = if cursor.is_empty() {
                String::new()
            } else {
                format!("?next_cursor={cursor}")
            };
            let ans = self
                .send(Some(a), reqwest::Method::GET, "/data/orders", &query, None)
                .await;
            let v = ans.json()?;
            out.extend(answer::open_orders_of(&v)?);
            match v["next_cursor"].as_str() {
                Some(c) if !c.is_empty() && c != "LTE=" => cursor = c.to_owned(),
                _ => return Ok(out),
            }
        }
        Ok(out)
    }

    /// The account's collateral, in dollars (`refresh` first asks the venue
    /// to read the chain afresh: a deposit shows only after that).
    pub async fn balance(
        &self,
        a: &Auth,
        signature_type: u8,
        refresh: bool,
    ) -> Result<Usd, String> {
        let query = format!("?asset_type=COLLATERAL&signature_type={signature_type}");
        if refresh {
            let _ = self
                .send(
                    Some(a),
                    reqwest::Method::GET,
                    "/balance-allowance/update",
                    &query,
                    None,
                )
                .await;
        }
        let v = self
            .send(
                Some(a),
                reqwest::Method::GET,
                "/balance-allowance",
                &query,
                None,
            )
            .await
            .json()?;
        parse_balance(&v["balance"]).ok_or_else(|| format!("no balance in {v}"))
    }

    /// One heartbeat: the id to send next, or (the id the venue expects, why).
    pub async fn heartbeat(
        &self,
        a: &Auth,
        id: Option<&str>,
    ) -> Result<String, (Option<String>, String)> {
        let body = serde_json::json!({ "heartbeat_id": id })
            .to_string()
            .into_bytes();
        let ans = self
            .send(
                Some(a),
                reqwest::Method::POST,
                "/v1/heartbeats",
                "",
                Some(body),
            )
            .await;
        if let Some((_, why)) = ans.transport {
            return Err((None, why));
        }
        heartbeat_answer(ans.status.unwrap_or(0), &ans.body)
    }
}

/// The collateral: whole micro-dollars as the venue writes them
/// ("50000000"), or dollars with a decimal point.
pub fn parse_balance(v: &serde_json::Value) -> Option<Usd> {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.contains('.') {
        Usd::parse(&s).ok()
    } else {
        s.parse::<i64>().ok().map(Usd::from_micros)
    }
}

/// What one heartbeat was answered (dailyprofit's `heartbeat_answer`).
pub fn heartbeat_answer(status: u16, body: &[u8]) -> Result<String, (Option<String>, String)> {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let id = v["heartbeat_id"].as_str().map(str::to_owned);
    let text = || String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned();
    match (status, id) {
        (200..=299, Some(id)) => Ok(id),
        (200..=299, None) => Err((
            None,
            format!("an answer without a heartbeat id: {}", text()),
        )),
        (400, Some(expected)) => Err((Some(expected), format!("http 400: {}", text()))),
        (s, _) => Err((None, format!("http {s}: {}", text()))),
    }
}

/// Where the venue sees this server.
pub async fn geoblock(
    http: &reqwest::Client,
    url: &str,
    close_only_override: Option<&[String]>,
) -> Result<Geo, String> {
    let r = http
        .get(url)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("http {}", r.status().as_u16()));
    }
    let body = r.bytes().await.map_err(|e| e.without_url().to_string())?;
    geo::parse(&body, close_only_override)
}

/// The account's positions (Data API): token id → shares.
pub async fn positions(
    http: &reqwest::Client,
    data_base: &str,
    funder: &str,
) -> Result<Vec<(String, Shares)>, String> {
    let url = format!(
        "{}/positions?user={funder}&sizeThreshold=0&limit=500",
        data_base.trim_end_matches('/')
    );
    let r = http
        .get(url)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !r.status().is_success() {
        return Err(format!("positions: http {}", r.status().as_u16()));
    }
    let v: serde_json::Value = r.json().await.map_err(|e| e.without_url().to_string())?;
    parse_positions(&v)
}

pub fn parse_positions(v: &serde_json::Value) -> Result<Vec<(String, Shares)>, String> {
    let list = v.as_array().ok_or("positions: an answer without a list")?;
    Ok(list
        .iter()
        .filter_map(|p| {
            let token = p["asset"].as_str()?.to_owned();
            let size = match &p["size"] {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => return None,
            };
            // the Data API writes floats: round to the micro-share
            let shares = size
                .parse::<f64>()
                .ok()
                .filter(|x| x.is_finite())
                .map(|x| Shares::from_micros((x * 1e6).round() as i64))?;
            Some((token, shares))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn balances_are_micro_dollars_or_dollars() {
        assert_eq!(
            parse_balance(&serde_json::json!("50000000")),
            Some(Usd::from_whole(50))
        );
        assert_eq!(
            parse_balance(&serde_json::json!("23.846345")),
            Some(Usd::from_micros(23_846_345))
        );
        assert_eq!(
            parse_balance(&serde_json::json!(1_000_000)),
            Some(Usd::from_whole(1))
        );
        assert_eq!(parse_balance(&serde_json::json!(null)), None);
    }

    #[test]
    fn heartbeat_answers_carry_the_next_id() {
        assert_eq!(
            heartbeat_answer(200, br#"{"heartbeat_id":"h2"}"#),
            Ok("h2".into())
        );
        assert_eq!(
            heartbeat_answer(
                400,
                br#"{"heartbeat_id":"h9","error":"Invalid Heartbeat ID"}"#
            ),
            Err((
                Some("h9".into()),
                r#"http 400: {"heartbeat_id":"h9","error":"Invalid Heartbeat ID"}"#.into()
            ))
        );
        assert!(matches!(heartbeat_answer(200, b"{}"), Err((None, _))));
        assert!(matches!(
            heartbeat_answer(404, b"not found"),
            Err((None, _))
        ));
    }

    #[test]
    fn positions_are_read_by_token() {
        let v = serde_json::json!([
            {"asset": "123", "size": 10.0, "avgPrice": 0.92, "redeemable": false},
            {"asset": "456", "size": "5.5"},
            {"size": 3}
        ]);
        assert_eq!(
            parse_positions(&v).unwrap(),
            vec![
                ("123".to_owned(), Shares::from_whole(10)),
                ("456".to_owned(), Shares::from_micros(5_500_000)),
            ]
        );
        assert!(parse_positions(&serde_json::json!({})).is_err());
    }
}
