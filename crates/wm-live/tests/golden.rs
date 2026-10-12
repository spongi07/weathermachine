//! Golden vectors from the reference client (py_clob_client_v2), with the
//! public dummy key 0xabab…ab (never funded, never registered), copied from
//! dailyprofit's `fixtures/`:
//!
//! * `orders_v2_live.json`: the orders dailyprofit sends (5 shares at ticks
//!   0.001 and 0.01, signature types 1 and 2, buys and sells);
//! * `orders_v2.json` and `auth_v2.json`: the original vectors (orders, the
//!   L1 `ClobAuth` signature, the L2 HMAC, amounts at tick 0.01).
//!
//! If one of these fails, nothing may trade live: a wrong amount or a wrong
//! signature is a refused order (cheap) or a wrong-size real order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use wm_core::market::Side;
use wm_core::units::{Price, Shares, Usd};
use wm_live::auth;
use wm_live::order::{OrderSpec, Wallet, build};
use wm_live::sign::{self, Domain, FastOrder, OrderSigner, OrderV2, Protocol};

fn load(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str()
        .unwrap_or_else(|| panic!("fixture field {k} missing"))
        .to_owned()
}

fn order_of(c: &Value) -> OrderV2 {
    OrderV2 {
        salt: s(c, "salt"),
        maker: s(c, "maker"),
        signer: s(c, "signer"),
        token_id: s(c, "tokenId"),
        maker_amount: s(c, "makerAmount"),
        taker_amount: s(c, "takerAmount"),
        side: u8::try_from(c["side"].as_u64().unwrap()).unwrap(),
        signature_type: u8::try_from(c["signatureType"].as_u64().unwrap()).unwrap(),
        timestamp: s(c, "timestamp"),
        metadata: s(c, "metadata"),
        builder: s(c, "builder"),
        expiration: s(c, "expiration"),
    }
}

#[test]
fn signatures_match_the_reference_client_for_every_case() {
    let mut n = 0;
    for name in ["orders_v2_live.json", "orders_v2.json"] {
        for c in load(name).as_array().unwrap() {
            let o = order_of(c);
            let signer = OrderSigner::new(&s(c, "private_key")).unwrap();
            assert_eq!(
                signer.address.to_lowercase(),
                s(c, "signer").to_lowercase(),
                "address derivation ({name})"
            );
            let sig = signer
                .sign_order(&o, c["chainId"].as_u64().unwrap(), &s(c, "exchange"))
                .unwrap();
            assert_eq!(
                sig.to_lowercase(),
                s(c, "signature").to_lowercase(),
                "signature of {name} salt {}",
                o.salt
            );
            n += 1;
        }
    }
    assert!(n >= 26, "{n} vectors");
}

#[test]
fn the_fast_path_digest_is_the_reference_digest() {
    for name in ["orders_v2_live.json", "orders_v2.json"] {
        for c in load(name).as_array().unwrap() {
            let o = order_of(c);
            let exchange = s(c, "exchange");
            assert!(
                exchange.eq_ignore_ascii_case(sign::EXCHANGE_V2),
                "{exchange}"
            );
            let reference =
                sign::order_digest(&o, c["chainId"].as_u64().unwrap(), &exchange).unwrap();
            let fo = FastOrder {
                salt: o.salt.parse().unwrap(),
                maker: sign::hex_to_b32(&o.maker).unwrap(),
                signer: sign::hex_to_b32(&o.signer).unwrap(),
                token_id: sign::dec_to_u256(&o.token_id).unwrap(),
                maker_amount: o.maker_amount.parse().unwrap(),
                taker_amount: o.taker_amount.parse().unwrap(),
                side: o.side,
                signature_type: o.signature_type,
                timestamp_ms: o.timestamp.parse().unwrap(),
                metadata: sign::hex_to_b32(&o.metadata).unwrap(),
                builder: sign::hex_to_b32(&o.builder).unwrap(),
            };
            let domain = Domain::for_market(Protocol::V2, false);
            assert_eq!(fo.digest(&domain), reference, "{name} salt {}", o.salt);
            let signer = OrderSigner::new(&s(c, "private_key")).unwrap();
            let sig = signer.sign_digest_bytes(&fo.digest(&domain)).unwrap();
            assert_eq!(
                format!("0x{}", hex::encode(sig)),
                s(c, "signature").to_lowercase()
            );
        }
    }
}

/// Every buy whose dollars are whole cents (the only kind placed live)
/// through the production builder: the same amounts and the same
/// signature as the reference client. The builder computes the amounts
/// exactly, from the fixed-point size and price.
#[test]
fn the_builder_reproduces_the_reference_buys_end_to_end() {
    let mut n = 0;
    for c in load("orders_v2_live.json").as_array().unwrap() {
        if s(c, "side_s") != "BUY" || s(c, "makerAmount").parse::<u64>().unwrap() % 10_000 != 0 {
            continue;
        }
        let signer = OrderSigner::new(&s(c, "private_key")).unwrap();
        let wallet = Wallet {
            maker: s(c, "maker"),
            signature_type: u8::try_from(c["signatureType"].as_u64().unwrap()).unwrap(),
            owner: "owner".into(),
        };
        let spec = OrderSpec {
            token_id: s(c, "tokenId"),
            side: Side::Buy,
            price: Price::parse(&c["price"].as_f64().unwrap().to_string()).unwrap(),
            tick: Price::parse(&s(c, "tick")).unwrap(),
            shares: Shares::parse(&c["size"].as_f64().unwrap().to_string()).unwrap(),
            min_size: Shares::from_whole(5),
            neg_risk: false,
            // not signed: any valid one gives the same signature
            expiration_s: 1_000_000,
            cap: Usd::from_whole(100),
        };
        let so = build(
            &signer,
            &wallet,
            Protocol::V2,
            &spec,
            s(c, "salt").parse().unwrap(),
            s(c, "timestamp").parse().unwrap(),
            0,
        )
        .unwrap();
        assert_eq!(so.maker_amount.to_string(), s(c, "makerAmount"), "{c}");
        assert_eq!(so.taker_amount.to_string(), s(c, "takerAmount"), "{c}");
        let body: Value = serde_json::from_slice(&so.body).unwrap();
        assert_eq!(
            body["order"]["signature"].as_str().unwrap().to_lowercase(),
            s(c, "signature").to_lowercase(),
            "{c}"
        );
        n += 1;
    }
    assert!(n >= 14, "exercised {n} buys");
}

#[test]
fn both_auth_levels_match_the_reference_client() {
    let f = load("auth_v2.json");
    let l1 = &f["l1"];
    let signer = OrderSigner::new(l1["key"].as_str().unwrap()).unwrap();
    assert_eq!(
        signer.address.to_lowercase(),
        l1["address"].as_str().unwrap().to_lowercase()
    );
    let sig = auth::clob_auth_signature(
        &signer,
        l1["chainId"].as_u64().unwrap(),
        l1["timestamp"].as_u64().unwrap(),
        l1["nonce"].as_u64().unwrap(),
    )
    .unwrap();
    assert_eq!(
        sig.to_lowercase(),
        l1["signature"].as_str().unwrap().to_lowercase(),
        "L1 ClobAuth signature"
    );
    let h = &f["hmac"];
    let mac = auth::hmac_signature(
        h["secret"].as_str().unwrap(),
        h["timestamp"].as_str().unwrap(),
        h["method"].as_str().unwrap(),
        h["path"].as_str().unwrap(),
        Some(h["body_json"].as_str().unwrap().as_bytes()),
    )
    .unwrap();
    assert_eq!(mac, h["signature"].as_str().unwrap(), "L2 HMAC");
}

#[test]
fn the_reference_amounts_at_a_cent_tick_are_exact() {
    for a in load("auth_v2.json")["amounts"].as_array().unwrap() {
        if a["side"].as_u64() != Some(0) {
            continue;
        }
        let shares = Shares::parse(&a["size"].as_f64().unwrap().to_string()).unwrap();
        let price = Price::parse(&a["price"].as_f64().unwrap().to_string()).unwrap();
        let shares_micros = u64::try_from(shares.micros()).unwrap();
        let usd = shares_micros / 10_000 * u64::from(price.micros() / 100);
        assert_eq!(usd, a["maker"].as_u64().unwrap(), "{a}");
        assert_eq!(shares_micros, a["taker"].as_u64().unwrap(), "{a}");
    }
}
