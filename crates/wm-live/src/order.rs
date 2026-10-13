//! Building and signing one CLOB order, with the exact bytes to post.
//!
//! Adapted from dailyprofit's `crates/execution/src/order.rs`, with every
//! guard it kept from two adversarial audits: the signature type against
//! the maker, deposit wallets (type 3) refused by name, the market minimum,
//! no zero leg, the signed amounts re-deriving to the intended price and
//! size, a hard cap on what a BUY can pay, salts that survive a JSON double,
//! and a BUY's dollars in whole cents (the venue refuses a market buy of
//! finer dollars; a live order is held to the same rule).
//!
//! Amounts are exact: a size in whole hundredths of a share at a price on
//! the tick grid has a product with no more decimals than the venue's
//! amounts carry. For a BUY the maker leg is the dollars and the taker leg
//! the shares, both in millionths.

use crate::sign::{Domain, FastOrder, OrderSigner, Protocol, dec_to_u256, hex_to_b32, keccak};
use std::fmt::Write as _;
use wm_core::market::Side;
use wm_core::units::{Price, Shares, Usd};

pub const BYTES32_ZERO: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

/// Largest integer that survives a JSON number read as an IEEE-754 double:
/// the signed struct binds the exact salt, the body carries it as a number.
pub const MAX_SAFE_SALT: u64 = (1u64 << 53) - 1;

/// A GTD order's stated expiration lies at least this far ahead: the venue
/// takes none nearer, and ends the order a minute before the stated time
/// (docs.polymarket.com/trading/orders/overview, as dailyprofit read it on
/// 2026-10-09).
pub const GTD_MIN_AHEAD_S: i64 = 180;
/// The venue ends a GTD order this long before its stated expiration.
pub const GTD_SECURITY_S: i64 = 60;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("signature type: {0}")]
    SignatureType(String),
    #[error("maker {0:?} is not an address")]
    Maker(String),
    #[error("only buys are placed live, not sells")]
    Side,
    #[error("size {size} is below the market minimum {min}")]
    BelowMinimum { size: Shares, min: Shares },
    #[error("size {0} is not whole hundredths of a share")]
    Size(Shares),
    #[error("price {price} is not on the tick {tick} inside (0, 1)")]
    Price { price: Price, tick: Price },
    #[error("a buy of {0} is not whole cents")]
    NotWholeCents(Usd),
    #[error("a buy of {cost} is over the cap of {cap}")]
    Cap { cost: Usd, cap: Usd },
    #[error("expiration {0}: a GTD order needs one at least three minutes ahead")]
    Expiration(i64),
    #[error("salt is not representable as a JSON number")]
    Salt,
    #[error("token id: {0}")]
    Token(String),
    #[error("sign: {0}")]
    Sign(String),
}

/// The account an order is placed for.
#[derive(Clone, Debug)]
pub struct Wallet {
    /// The funder (the proxy or safe wallet that holds the money; the signer
    /// itself for an EOA).
    pub maker: String,
    /// 0 EOA, 1 POLY_PROXY, 2 POLY_GNOSIS_SAFE.
    pub signature_type: u8,
    /// The API key ("owner" on the wire).
    pub owner: String,
}

impl Wallet {
    /// Check the wallet mapping once (dailyprofit's rules).
    pub fn check(&self, signer: &OrderSigner) -> Result<(), BuildError> {
        match self.signature_type {
            0 if !self.maker.eq_ignore_ascii_case(&signer.address) => Err(
                BuildError::SignatureType("type 0 (EOA) needs the funder to be the signer".into()),
            ),
            1 | 2 if self.maker.eq_ignore_ascii_case(&signer.address) => {
                Err(BuildError::SignatureType(format!(
                    "type {} expects the funder to be the proxy or safe wallet, not the signer",
                    self.signature_type
                )))
            }
            0..=2 => {
                let m = &self.maker;
                if m.len() == 42
                    && m.starts_with("0x")
                    && m[2..].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    Ok(())
                } else {
                    Err(BuildError::Maker(m.clone()))
                }
            }
            3 => Err(BuildError::SignatureType(
                "type 3 (Deposit Wallet) needs an ERC-7739 wrapper, which is not implemented"
                    .into(),
            )),
            t => Err(BuildError::SignatureType(format!("unknown type {t}"))),
        }
    }
}

/// One post-only GTD buy as the strategy priced it.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderSpec {
    /// The token id (decimal).
    pub token_id: String,
    pub side: Side,
    pub price: Price,
    pub tick: Price,
    pub shares: Shares,
    pub min_size: Shares,
    pub neg_risk: bool,
    /// The stated expiration, unix seconds (the venue ends the order a
    /// minute earlier: [`GTD_SECURITY_S`]).
    pub expiration_s: i64,
    /// What the order may pay at most.
    pub cap: Usd,
}

/// One order, built and signed, with the exact JSON body to post.
#[derive(Clone, Debug, PartialEq)]
pub struct SignedOrder {
    /// The EIP-712 digest, 0x-hex: the venue's `orderID`.
    pub order_id: String,
    pub salt: u64,
    pub maker_amount: u64,
    pub taker_amount: u64,
    /// What the buy pays at most.
    pub cost: Usd,
    pub timestamp_ms: u64,
    pub protocol: Protocol,
    pub body: Vec<u8>,
}

/// A deterministic salt from the order's client id (masked to 2^53 − 1).
pub fn salt_for(client_id: &str) -> u64 {
    let h = keccak(client_id.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&h[0..8]);
    u64::from_be_bytes(b) & MAX_SAFE_SALT
}

/// The largest size at or under `shares`, in whole shares, whose cost at
/// `price` is whole cents (`None` under `min`). At a 0.01 tick every whole
/// share is; at 0.001 the step is up to ten shares (dailyprofit's
/// `amounts::whole_cent_size`).
pub fn whole_cent_size(shares: Shares, price: Price, min: Shares) -> Option<Shares> {
    let s = wm_core::units::whole_cent_shares(shares, price);
    (s.micros() > 0 && s >= min).then_some(s)
}

/// Build and sign `spec` for `wallet` under `protocol`. `now_s` checks the
/// expiration.
pub fn build(
    signer: &OrderSigner,
    wallet: &Wallet,
    protocol: Protocol,
    spec: &OrderSpec,
    salt: u64,
    timestamp_ms: u64,
    now_s: i64,
) -> Result<SignedOrder, BuildError> {
    wallet.check(signer)?;
    if spec.side != Side::Buy {
        return Err(BuildError::Side);
    }
    if spec.shares < spec.min_size {
        return Err(BuildError::BelowMinimum {
            size: spec.shares,
            min: spec.min_size,
        });
    }
    if spec.shares.micros() <= 0 || spec.shares.micros() % 10_000 != 0 {
        return Err(BuildError::Size(spec.shares));
    }
    let tick = spec.tick.micros();
    let p = spec.price.micros();
    if tick == 0 || p == 0 || p >= 1_000_000 || !p.is_multiple_of(tick) || !p.is_multiple_of(100) {
        return Err(BuildError::Price {
            price: spec.price,
            tick: spec.tick,
        });
    }
    if spec.expiration_s < now_s + GTD_MIN_AHEAD_S {
        return Err(BuildError::Expiration(spec.expiration_s));
    }
    if salt > MAX_SAFE_SALT {
        return Err(BuildError::Salt);
    }
    // exact: (shares in micros, a multiple of 10^4) × (price in micros, a
    // multiple of 10^2) / 10^6 has no remainder
    let shares_micros =
        u64::try_from(spec.shares.micros()).map_err(|_| BuildError::Size(spec.shares))?;
    let usd_micros = shares_micros / 10_000 * u64::from(p / 100);
    if usd_micros == 0 {
        return Err(BuildError::Size(spec.shares));
    }
    let cost = Usd::from_micros(i64::try_from(usd_micros).unwrap_or(i64::MAX));
    if usd_micros % 10_000 != 0 {
        return Err(BuildError::NotWholeCents(cost));
    }
    if cost > spec.cap {
        return Err(BuildError::Cap {
            cost,
            cap: spec.cap,
        });
    }
    // a BUY: the dollars are the maker leg, the shares the taker leg
    let (maker_amount, taker_amount) = (usd_micros, shares_micros);
    let token_word = dec_to_u256(&spec.token_id).map_err(BuildError::Token)?;
    let fo = FastOrder {
        salt,
        maker: hex_to_b32(&wallet.maker).map_err(BuildError::Maker)?,
        signer: signer.address_word,
        token_id: token_word,
        maker_amount,
        taker_amount,
        side: 0,
        signature_type: wallet.signature_type,
        timestamp_ms,
        metadata: [0u8; 32],
        builder: [0u8; 32],
    };
    let domain = Domain::for_market(protocol, spec.neg_risk);
    let digest = fo.digest(&domain);
    let sig = signer
        .sign_digest_bytes(&digest)
        .map_err(BuildError::Sign)?;
    let body = body(
        wallet,
        &signer.address,
        &spec.token_id,
        salt,
        maker_amount,
        taker_amount,
        timestamp_ms,
        spec.expiration_s,
        &sig,
    );
    Ok(SignedOrder {
        order_id: format!("0x{}", hex::encode(digest)),
        salt,
        maker_amount,
        taker_amount,
        cost,
        timestamp_ms,
        protocol,
        body,
    })
}

/// The `POST /order` body of a post-only GTD buy, hand-serialised (every
/// value is a decimal or hex string made here; nothing needs escaping) so
/// the L2 HMAC covers exactly these bytes. Shape per the official client's
/// `SignedOrder` serialisation, the same for protocols 2 and 3.
#[allow(clippy::too_many_arguments)]
fn body(
    wallet: &Wallet,
    signer: &str,
    token_id: &str,
    salt: u64,
    maker_amount: u64,
    taker_amount: u64,
    ts: u64,
    expiration_s: i64,
    sig: &[u8; 65],
) -> Vec<u8> {
    let mut s = String::with_capacity(900);
    let _ = write!(
        s,
        r#"{{"order":{{"salt":{salt},"maker":"{}","signer":"{signer}","tokenId":"{token_id}","makerAmount":"{maker_amount}","takerAmount":"{taker_amount}","side":"BUY","expiration":"{expiration_s}","signatureType":{},"timestamp":"{ts}","metadata":"{BYTES32_ZERO}","builder":"{BYTES32_ZERO}","signature":"0x{}"}},"owner":"{}","orderType":"GTD","postOnly":true,"deferExec":false}}"#,
        wallet.maker,
        wallet.signature_type,
        hex::encode(sig),
        wallet.owner,
    );
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const TOKEN: &str =
        "32601456109682408575117078202194812949053807482269500276187188562276575686157";
    const PROXY: &str = "0x81f4f47aa3bBcF441cDe0B7C6e2A38a706b6d77e";

    fn signer() -> OrderSigner {
        OrderSigner::new(&format!("0x{}", "ab".repeat(32))).unwrap()
    }

    fn wallet() -> Wallet {
        Wallet {
            maker: PROXY.into(),
            signature_type: 1,
            owner: "owner-uuid".into(),
        }
    }

    fn spec(price: u32, tick: u32, shares: i64) -> OrderSpec {
        OrderSpec {
            token_id: TOKEN.into(),
            side: Side::Buy,
            price: Price::saturating_from_micros(price),
            tick: Price::saturating_from_micros(tick),
            shares: Shares::from_whole(shares),
            min_size: Shares::from_whole(5),
            neg_risk: true,
            expiration_s: 1_790_000_600,
            cap: Usd::from_whole(10),
        }
    }

    const NOW: i64 = 1_790_000_000;

    #[test]
    fn a_post_only_gtd_buy_carries_its_exact_body() {
        let so = build(
            &signer(),
            &wallet(),
            Protocol::V2,
            &spec(920_000, 10_000, 10),
            42,
            1,
            NOW,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&so.body).unwrap();
        assert_eq!(v["orderType"], "GTD");
        assert_eq!(v["postOnly"], true);
        assert_eq!(v["owner"], "owner-uuid");
        assert_eq!(v["order"]["side"], "BUY");
        assert_eq!(v["order"]["makerAmount"], "9200000", "10 × 0.92 = $9.20");
        assert_eq!(v["order"]["takerAmount"], "10000000", "10 shares");
        assert_eq!(v["order"]["expiration"], "1790000600");
        assert_eq!(v["order"]["signatureType"], 1);
        assert_eq!(v["order"]["salt"].as_u64(), Some(42));
        assert_eq!(v["order"]["tokenId"], TOKEN);
        assert_eq!(so.cost, Usd::from_micros(9_200_000));
        assert_eq!(so.order_id.len(), 66);
        // deterministic: the same inputs give the same order and id
        let again = build(
            &signer(),
            &wallet(),
            Protocol::V2,
            &spec(920_000, 10_000, 10),
            42,
            1,
            NOW,
        )
        .unwrap();
        assert_eq!(again, so);
    }

    #[test]
    fn the_protocol_and_the_market_kind_change_the_order_id() {
        let s = spec(920_000, 10_000, 10);
        let v2_neg = build(&signer(), &wallet(), Protocol::V2, &s, 42, 1, NOW).unwrap();
        let v3 = build(&signer(), &wallet(), Protocol::V3, &s, 42, 1, NOW).unwrap();
        let plain = OrderSpec {
            neg_risk: false,
            ..s
        };
        let v2 = build(&signer(), &wallet(), Protocol::V2, &plain, 42, 1, NOW).unwrap();
        assert_ne!(v2_neg.order_id, v2.order_id);
        assert_ne!(v2_neg.order_id, v3.order_id);
        assert_eq!(v2_neg.body.len(), v3.body.len(), "one wire shape");
    }

    #[test]
    fn every_guard_refuses_its_order() {
        let sg = signer();
        let w = wallet();
        let b = |s: &OrderSpec| build(&sg, &w, Protocol::V2, s, 42, 1, NOW);
        let sell = OrderSpec {
            side: Side::Sell,
            ..spec(920_000, 10_000, 10)
        };
        assert_eq!(b(&sell), Err(BuildError::Side));
        assert!(matches!(
            b(&spec(920_000, 10_000, 4)),
            Err(BuildError::BelowMinimum { .. })
        ));
        assert!(
            matches!(b(&spec(925_000, 10_000, 10)), Err(BuildError::Price { .. })),
            "off tick"
        );
        assert!(matches!(
            b(&spec(1_000_000, 10_000, 10)),
            Err(BuildError::Price { .. })
        ));
        assert!(
            matches!(
                b(&spec(977_000, 1_000, 11)),
                Err(BuildError::NotWholeCents(_))
            ),
            "11 × 0.977 = $10.747"
        );
        assert!(
            matches!(b(&spec(990_000, 10_000, 11)), Err(BuildError::Cap { .. })),
            "$10.89"
        );
        let soon = OrderSpec {
            expiration_s: NOW + 179,
            ..spec(920_000, 10_000, 10)
        };
        assert!(matches!(b(&soon), Err(BuildError::Expiration(_))));
        let eoa = Wallet {
            signature_type: 0,
            ..w.clone()
        };
        assert!(matches!(
            build(
                &sg,
                &eoa,
                Protocol::V2,
                &spec(920_000, 10_000, 10),
                42,
                1,
                NOW
            ),
            Err(BuildError::SignatureType(_))
        ));
        let deposit = Wallet {
            signature_type: 3,
            ..w.clone()
        };
        assert!(matches!(
            deposit.check(&sg),
            Err(BuildError::SignatureType(_))
        ));
        let bad = Wallet {
            maker: "0x123".into(),
            ..w
        };
        assert!(matches!(bad.check(&sg), Err(BuildError::Maker(_))));
        assert_eq!(
            build(
                &sg,
                &wallet(),
                Protocol::V2,
                &spec(920_000, 10_000, 10),
                MAX_SAFE_SALT + 1,
                1,
                NOW
            ),
            Err(BuildError::Salt)
        );
    }

    #[test]
    fn whole_cents_size_down_at_a_thousandth() {
        let p = |m| Price::saturating_from_micros(m);
        let five = Shares::from_whole(5);
        assert_eq!(
            whole_cent_size(Shares::from_whole(10), p(920_000), five),
            Some(Shares::from_whole(10))
        );
        // 0.977: only multiples of ten shares cost whole cents
        assert_eq!(
            whole_cent_size(Shares::from_whole(10), p(977_000), five),
            Some(Shares::from_whole(10))
        );
        assert_eq!(
            whole_cent_size(Shares::from_whole(19), p(977_000), five),
            Some(Shares::from_whole(10))
        );
        // 0.975: even sizes
        assert_eq!(
            whole_cent_size(Shares::from_whole(11), p(975_000), five),
            Some(Shares::from_whole(10))
        );
        assert_eq!(
            whole_cent_size(Shares::from_whole(9), p(977_000), five),
            None,
            "under the minimum"
        );
    }

    #[test]
    fn salts_are_deterministic_and_json_safe() {
        assert_eq!(salt_for("wm-a-1-123"), salt_for("wm-a-1-123"));
        assert_ne!(salt_for("wm-a-1-123"), salt_for("wm-a-2-123"));
        assert!(salt_for("x") <= MAX_SAFE_SALT);
    }
}
