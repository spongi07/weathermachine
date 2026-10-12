//! Reading the venue's answers: a placed order, an order's record, a cancel.
//!
//! Conservative by construction (dailyprofit's `classify`): an answer that
//! does not prove where the order is counts as unknown, which keeps every
//! new live order back until the order's record settles it. The order id is
//! the EIP-712 digest computed before the order is sent, so its record can
//! always be asked for.
//!
//! | `POST /order` answer | outcome |
//! |---|---|
//! | the request never left (connect failure) | `NotSent` |
//! | 2xx, `status` live / unmatched / delayed | `Resting` |
//! | 2xx, `status` matched | `Resting` (its record says what it bought) |
//! | "crosses book" (a post-only order that would take) | `Refused` |
//! | balance, allowance, tick, size, signature, owner errors; 400 401 403 404 | `Refused` |
//! | 425 (restarting), 429, 503 (cancel-only, …) | `Refused` (never placed) |
//! | a timeout, any other 5xx, an unreadable body | `Unknown` |

use serde_json::Value;
use wm_core::units::Shares;

/// What one `POST /order` answer proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// The request did not reach the venue: nothing was placed.
    NotSent(String),
    /// The venue holds the order (in the book, or matched at once).
    Resting { order_id: Option<String> },
    /// The venue refused it: nothing was placed.
    Refused(String),
    /// Nothing is proven: the order's record decides.
    Unknown(String),
}

fn text(body: &[u8]) -> String {
    let t = String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned();
    t.replace(['\n', '\r'], " ")
}

/// Classify a `POST /order` answer (`status` is `None` when the transport
/// failed; `connect_failed` when it failed before the request was written).
pub fn classify_post(
    status: Option<u16>,
    body: &[u8],
    transport: Option<(bool, String)>,
) -> PostOutcome {
    if let Some((connect_failed, why)) = transport {
        return if connect_failed {
            PostOutcome::NotSent(why)
        } else {
            PostOutcome::Unknown(format!("no answer: {why}"))
        };
    }
    let Some(status) = status else {
        return PostOutcome::Unknown("no status".into());
    };
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let error = v["errorMsg"]
        .as_str()
        .or_else(|| v["error"].as_str())
        .filter(|e| !e.is_empty())
        .map(str::to_owned);
    let order_id = v["orderID"]
        .as_str()
        .or_else(|| v["orderId"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    match status {
        200..=299 => {
            let st = v["status"].as_str().unwrap_or("").to_ascii_lowercase();
            let ok = v["success"].as_bool().unwrap_or(true);
            match (ok, st.as_str(), &error) {
                (true, "live" | "unmatched" | "delayed" | "matched", _) => {
                    PostOutcome::Resting { order_id }
                }
                (_, _, Some(e)) => PostOutcome::Refused(e.clone()),
                (false, _, None) => PostOutcome::Refused(text(body)),
                (true, _, None) if order_id.is_some() => PostOutcome::Resting { order_id },
                _ => PostOutcome::Unknown(format!("an answer without a status: {}", text(body))),
            }
        }
        400..=404 | 425 | 429 | 503 => PostOutcome::Refused(format!(
            "http {status}: {}",
            error.unwrap_or_else(|| text(body))
        )),
        _ => PostOutcome::Unknown(format!("http {status}: {}", text(body))),
    }
}

/// The venue's record of one order (`GET /data/order/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    pub order_id: String,
    /// LIVE, MATCHED, CANCELED, INVALID, CANCELED_MARKET_RESOLVED, …
    pub status: String,
    pub size_matched: Shares,
    pub original_size: Option<Shares>,
}

impl OrderRecord {
    /// The order left the book: filled in full, cancelled, expired or invalid.
    pub fn ended(&self) -> bool {
        match self.status.as_str() {
            "LIVE" | "" => false,
            // a venue may call an order "matched" that still rests in part
            "MATCHED" => self.original_size.is_some_and(|o| self.size_matched >= o),
            _ => true,
        }
    }
}

fn shares(v: &Value) -> Option<Shares> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Shares::parse(&s).ok()
}

/// Parse `GET /data/order/{id}`. `Ok(None)`: the venue has no such order.
pub fn parse_record(status: u16, body: &[u8]) -> Result<Option<OrderRecord>, String> {
    if status == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        return Err(format!("http {status}: {}", text(body)));
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?;
    let o = if v.get("order").is_some() {
        &v["order"]
    } else {
        &v
    };
    if o.is_null() || o.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(None);
    }
    let order_id = o["id"].as_str().ok_or("a record without an id")?.to_owned();
    Ok(Some(OrderRecord {
        order_id,
        status: o["status"].as_str().unwrap_or("").to_ascii_uppercase(),
        size_matched: shares(&o["size_matched"]).unwrap_or(Shares::ZERO),
        original_size: shares(&o["original_size"]),
    }))
}

/// What the venue answered a cancel of one order (`DELETE /order`:
/// `{"canceled": [ids], "not_canceled": {id: reason}}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelAnswer {
    Cancelled,
    /// It names the order and says it is not to be cancelled (filled,
    /// cancelled already, or never there).
    NotCancelled(String),
    /// Nothing about this order: ask again.
    Unknown(String),
}

pub fn cancel_answer(status: Option<u16>, body: &[u8], order_id: &str) -> CancelAnswer {
    match status {
        Some(200..=299) => {
            let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            if v["canceled"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(order_id)))
            }) {
                return CancelAnswer::Cancelled;
            }
            match v["not_canceled"]
                .as_object()
                .and_then(|m| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(order_id)))
            {
                Some((_, why)) => {
                    CancelAnswer::NotCancelled(why.as_str().unwrap_or("not cancelled").to_owned())
                }
                None => CancelAnswer::Unknown(format!(
                    "an answer that does not name the order: {}",
                    text(body)
                )),
            }
        }
        Some(s) => CancelAnswer::Unknown(format!("http {s}: {}", text(body))),
        None => CancelAnswer::Unknown("no answer".into()),
    }
}

/// One open order of the account (`GET /data/orders`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    pub order_id: String,
    pub asset_id: String,
}

/// Parse `GET /data/orders` (a list, or `{data: [...]}`).
pub fn parse_open_orders(body: &[u8]) -> Result<Vec<OpenOrder>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?;
    open_orders_of(&v)
}

/// The open orders of a decoded `GET /data/orders` answer.
pub fn open_orders_of(v: &Value) -> Result<Vec<OpenOrder>, String> {
    let list = v
        .as_array()
        .or_else(|| v["data"].as_array())
        .ok_or("an answer without a list")?;
    Ok(list
        .iter()
        .filter_map(|o| {
            Some(OpenOrder {
                order_id: o["id"].as_str()?.to_owned(),
                asset_id: o["asset_id"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn placed_orders_are_read_conservatively() {
        let live = br#"{"success":true,"orderID":"0xabc","status":"live","errorMsg":""}"#;
        assert_eq!(
            classify_post(Some(200), live, None),
            PostOutcome::Resting {
                order_id: Some("0xabc".into())
            }
        );
        let cross = br#"{"success":false,"errorMsg":"invalid post-only order: order crosses book","status":""}"#;
        assert!(
            matches!(classify_post(Some(400), cross, None), PostOutcome::Refused(e) if e.contains("crosses book"))
        );
        assert!(matches!(
            classify_post(Some(200), cross, None),
            PostOutcome::Refused(_)
        ));
        let balance = br#"{"error":"not enough balance / allowance"}"#;
        assert!(
            matches!(classify_post(Some(400), balance, None), PostOutcome::Refused(e) if e.contains("balance"))
        );
        assert!(matches!(
            classify_post(Some(425), b"", None),
            PostOutcome::Refused(_)
        ));
        assert!(matches!(
            classify_post(Some(503), b"", None),
            PostOutcome::Refused(_)
        ));
        assert!(matches!(
            classify_post(Some(500), b"order timed out", None),
            PostOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_post(Some(502), b"", None),
            PostOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_post(Some(200), b"<html>", None),
            PostOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_post(None, b"", Some((true, "connect refused".into()))),
            PostOutcome::NotSent(_)
        ));
        assert!(matches!(
            classify_post(None, b"", Some((false, "timed out".into()))),
            PostOutcome::Unknown(_)
        ));
    }

    #[test]
    fn records_say_whether_an_order_ended() {
        let b = br#"{"id":"0xabc","status":"MATCHED","original_size":"10","size_matched":"10","price":"0.92"}"#;
        let r = parse_record(200, b).unwrap().unwrap();
        assert_eq!(r.size_matched, Shares::from_whole(10));
        assert!(r.ended());
        let part = br#"{"id":"0xabc","status":"MATCHED","original_size":"10","size_matched":"4"}"#;
        assert!(
            !parse_record(200, part).unwrap().unwrap().ended(),
            "matched in part still rests"
        );
        let live = br#"{"id":"0xabc","status":"LIVE","size_matched":"0"}"#;
        assert!(!parse_record(200, live).unwrap().unwrap().ended());
        let gone = br#"{"id":"0xabc","status":"CANCELED","size_matched":"3"}"#;
        let g = parse_record(200, gone).unwrap().unwrap();
        assert!(g.ended());
        assert_eq!(g.size_matched, Shares::from_whole(3));
        assert_eq!(parse_record(404, b"").unwrap(), None);
        assert_eq!(parse_record(200, b"null").unwrap(), None);
        assert!(parse_record(500, b"").is_err());
    }

    #[test]
    fn a_cancel_answer_must_name_the_order() {
        let id = "0xabcdef";
        assert_eq!(
            cancel_answer(
                Some(200),
                br#"{"canceled":["0xABCDEF"],"not_canceled":{}}"#,
                id
            ),
            CancelAnswer::Cancelled
        );
        assert_eq!(
            cancel_answer(Some(200), br#"{"canceled":[],"not_canceled":{"0xabcdef":"Order not found or already canceled"}}"#, id),
            CancelAnswer::NotCancelled("Order not found or already canceled".into())
        );
        assert!(matches!(
            cancel_answer(Some(200), br#"{"canceled":["0x01"]}"#, id),
            CancelAnswer::Unknown(_)
        ));
        assert!(matches!(
            cancel_answer(Some(500), b"", id),
            CancelAnswer::Unknown(_)
        ));
        assert!(matches!(
            cancel_answer(None, b"", id),
            CancelAnswer::Unknown(_)
        ));
    }

    #[test]
    fn open_orders_come_as_a_list_or_wrapped() {
        let a = parse_open_orders(br#"[{"id":"0x1","asset_id":"7"}]"#).unwrap();
        assert_eq!(
            a,
            vec![OpenOrder {
                order_id: "0x1".into(),
                asset_id: "7".into()
            }]
        );
        let b =
            parse_open_orders(br#"{"data":[{"id":"0x2","asset_id":"8"}],"next_cursor":"LTE="}"#)
                .unwrap();
        assert_eq!(b.len(), 1);
        assert!(parse_open_orders(br#"{"x":1}"#).is_err());
    }
}
