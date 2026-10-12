//! EIP-712 signing of Polymarket CLOB orders.
//!
//! Ported from dailyprofit (`crates/execution/src/sign.rs`, itself from
//! pmdash), which reproduces the reference client bit for bit and is pinned
//! by its golden vectors (`tests/golden.rs`):
//!
//! ```text
//! Order(uint256 salt,address maker,address signer,uint256 tokenId,
//!       uint256 makerAmount,uint256 takerAmount,uint8 side,
//!       uint8 signatureType,uint256 timestamp,bytes32 metadata,
//!       bytes32 builder)
//! digest    = keccak(0x1901 || domainSeparator || structHash)  (the venue's order id)
//! signature = r || s || (27 + recid), hex with a 0x prefix
//! ```
//!
//! `expiration` is not part of the signed struct; it travels on the wire.
//!
//! **The domain follows the CLOB's protocol version** (`GET /version`), as
//! the official client selects it (`rs-clob-client-v2`, `Client::sign`, read
//! 2026-10-12): version 2 signs against the CTF Exchange V2 with domain
//! version "2", or its neg-risk twin for a neg-risk market (every weather
//! market is one); version 3 signs every market against the Exchange V3
//! with domain version "3". The struct, the wire body and the way the order
//! id is computed are the same in both.

use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use sha3::{Digest, Keccak256};
use std::sync::OnceLock;

pub const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
pub const CHAIN_ID: u64 = 137;
/// CTF Exchange V2 (Polygon).
pub const EXCHANGE_V2: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
/// Neg-risk CTF Exchange V2 (Polygon). A neg-risk market's order signed
/// against the standard exchange is refused, so it is chosen per market.
pub const EXCHANGE_V2_NEG_RISK: &str = "0xe2222d279d744050d28e00520010520000310F59";
/// Exchange V3 (Polygon): every market, neg-risk or not.
pub const EXCHANGE_V3: &str = "0xe3333700cA9d93003F00f0F71f8515005F6c00Aa";

const ORDER_TYPE_STRING: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";
const DOMAIN_TYPE_STRING: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// The CLOB protocol an order is signed for (`GET /version`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Protocol {
    V2,
    V3,
}

impl Protocol {
    /// The protocols this signer knows. Version 1 (the legacy struct with
    /// `nonce` and `feeRateBps`) is not signed here.
    pub fn from_version(v: u32) -> Option<Self> {
        match v {
            2 => Some(Self::V2),
            3 => Some(Self::V3),
            _ => None,
        }
    }

    pub fn number(self) -> u32 {
        match self {
            Self::V2 => 2,
            Self::V3 => 3,
        }
    }

    /// The exchange contract an order of a market is signed against.
    pub fn exchange(self, neg_risk: bool) -> &'static str {
        match (self, neg_risk) {
            (Self::V2, false) => EXCHANGE_V2,
            (Self::V2, true) => EXCHANGE_V2_NEG_RISK,
            (Self::V3, _) => EXCHANGE_V3,
        }
    }

    fn domain_version(self) -> &'static str {
        match self {
            Self::V2 => "2",
            Self::V3 => "3",
        }
    }
}

pub fn keccak(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// Decimal string → 32-byte big-endian uint256 (token ids overflow u128).
pub fn dec_to_u256(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("not a decimal uint: {s:?}"));
    }
    let mut out = [0u8; 32];
    for digit in s.bytes() {
        let mut carry = u32::from(digit - b'0');
        for byte in out.iter_mut().rev() {
            let v = u32::from(*byte) * 10 + carry;
            *byte = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return Err(format!("uint256 overflow: {s}"));
        }
    }
    Ok(out)
}

/// 0x-hex (address or bytes32) → left-padded 32 bytes.
pub fn hex_to_b32(s: &str) -> Result<[u8; 32], String> {
    let h = s.trim().trim_start_matches("0x");
    if h.len() > 64 {
        return Err(format!("hex too long: {s}"));
    }
    let padded = format!("{h:0>64}");
    let raw = hex::decode(&padded).map_err(|e| format!("bad hex {s}: {e}"))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

fn u64_word(x: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&x.to_be_bytes());
    w
}

/// The order exactly as the reference client builds it (string fields): the
/// golden tests' path.
#[derive(Clone, Debug)]
pub struct OrderV2 {
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    /// 0 = BUY, 1 = SELL
    pub side: u8,
    /// 0 = EOA, 1 = POLY_PROXY, 2 = POLY_GNOSIS_SAFE
    pub signature_type: u8,
    pub timestamp: String,
    pub metadata: String,
    pub builder: String,
    /// Sent on the wire, not hashed.
    pub expiration: String,
}

/// The EIP-712 domain separator of `exchange` at domain `version`.
pub fn domain_separator(chain_id: u64, version: &str, exchange: &str) -> Result<[u8; 32], String> {
    let mut enc = Vec::with_capacity(160);
    enc.extend_from_slice(&keccak(DOMAIN_TYPE_STRING.as_bytes()));
    enc.extend_from_slice(&keccak(DOMAIN_NAME.as_bytes()));
    enc.extend_from_slice(&keccak(version.as_bytes()));
    enc.extend_from_slice(&dec_to_u256(&chain_id.to_string())?);
    enc.extend_from_slice(&hex_to_b32(exchange)?);
    Ok(keccak(&enc))
}

fn order_type_hash() -> [u8; 32] {
    static H: OnceLock<[u8; 32]> = OnceLock::new();
    *H.get_or_init(|| keccak(ORDER_TYPE_STRING.as_bytes()))
}

pub fn struct_hash(o: &OrderV2) -> Result<[u8; 32], String> {
    let mut enc = Vec::with_capacity(12 * 32);
    enc.extend_from_slice(&order_type_hash());
    enc.extend_from_slice(&dec_to_u256(&o.salt)?);
    enc.extend_from_slice(&hex_to_b32(&o.maker)?);
    enc.extend_from_slice(&hex_to_b32(&o.signer)?);
    enc.extend_from_slice(&dec_to_u256(&o.token_id)?);
    enc.extend_from_slice(&dec_to_u256(&o.maker_amount)?);
    enc.extend_from_slice(&dec_to_u256(&o.taker_amount)?);
    enc.extend_from_slice(&dec_to_u256(&o.side.to_string())?);
    enc.extend_from_slice(&dec_to_u256(&o.signature_type.to_string())?);
    enc.extend_from_slice(&dec_to_u256(&o.timestamp)?);
    enc.extend_from_slice(&hex_to_b32(&o.metadata)?);
    enc.extend_from_slice(&hex_to_b32(&o.builder)?);
    Ok(keccak(&enc))
}

/// The digest of the reference path, at domain version "2" (the golden
/// vectors' version).
pub fn order_digest(o: &OrderV2, chain_id: u64, exchange: &str) -> Result<[u8; 32], String> {
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(b"\x19\x01");
    msg.extend_from_slice(&domain_separator(chain_id, "2", exchange)?);
    msg.extend_from_slice(&struct_hash(o)?);
    Ok(keccak(&msg))
}

/// A cached EIP-712 domain: one per (protocol, exchange).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    pub protocol: Protocol,
    pub exchange: &'static str,
    pub separator: [u8; 32],
}

impl Domain {
    pub fn for_market(protocol: Protocol, neg_risk: bool) -> Domain {
        static CACHE: OnceLock<[Domain; 4]> = OnceLock::new();
        let all = CACHE.get_or_init(|| {
            let make = |protocol: Protocol, neg_risk: bool| {
                let exchange = protocol.exchange(neg_risk);
                Domain {
                    protocol,
                    exchange,
                    separator: domain_separator(CHAIN_ID, protocol.domain_version(), exchange)
                        .unwrap_or([0; 32]),
                }
            };
            [
                make(Protocol::V2, false),
                make(Protocol::V2, true),
                make(Protocol::V3, false),
                make(Protocol::V3, true),
            ]
        });
        let i = match (protocol, neg_risk) {
            (Protocol::V2, false) => 0,
            (Protocol::V2, true) => 1,
            (Protocol::V3, false) => 2,
            (Protocol::V3, true) => 3,
        };
        all[i]
    }
}

/// Every signed field pre-encoded as its ABI word: what the order builder
/// signs. The golden tests prove it gives the reference path's digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FastOrder {
    pub salt: u64,
    pub maker: [u8; 32],
    pub signer: [u8; 32],
    pub token_id: [u8; 32],
    pub maker_amount: u64,
    pub taker_amount: u64,
    pub side: u8,
    pub signature_type: u8,
    pub timestamp_ms: u64,
    pub metadata: [u8; 32],
    pub builder: [u8; 32],
}

impl FastOrder {
    pub fn struct_hash(&self) -> [u8; 32] {
        let mut enc = [0u8; 12 * 32];
        enc[0..32].copy_from_slice(&order_type_hash());
        enc[32..64].copy_from_slice(&u64_word(self.salt));
        enc[64..96].copy_from_slice(&self.maker);
        enc[96..128].copy_from_slice(&self.signer);
        enc[128..160].copy_from_slice(&self.token_id);
        enc[160..192].copy_from_slice(&u64_word(self.maker_amount));
        enc[192..224].copy_from_slice(&u64_word(self.taker_amount));
        enc[224..256].copy_from_slice(&u64_word(u64::from(self.side)));
        enc[256..288].copy_from_slice(&u64_word(u64::from(self.signature_type)));
        enc[288..320].copy_from_slice(&u64_word(self.timestamp_ms));
        enc[320..352].copy_from_slice(&self.metadata);
        enc[352..384].copy_from_slice(&self.builder);
        keccak(&enc)
    }

    /// The EIP-712 digest: what is signed, and the venue's order id.
    pub fn digest(&self, domain: &Domain) -> [u8; 32] {
        let mut msg = [0u8; 66];
        msg[0] = 0x19;
        msg[1] = 0x01;
        msg[2..34].copy_from_slice(&domain.separator);
        msg[34..66].copy_from_slice(&self.struct_hash());
        keccak(&msg)
    }
}

/// The wallet key that signs orders and authenticates to the CLOB. It never
/// prints: `Debug` shows the address only.
pub struct OrderSigner {
    key: SigningKey,
    /// EIP-55 checksummed.
    pub address: String,
    pub address_word: [u8; 32],
}

impl std::fmt::Debug for OrderSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OrderSigner({})", self.address)
    }
}

impl OrderSigner {
    pub fn new(private_key_hex: &str) -> Result<Self, String> {
        let raw = hex::decode(private_key_hex.trim().trim_start_matches("0x"))
            .map_err(|_| "the private key is not hex".to_owned())?;
        let key = SigningKey::from_slice(&raw).map_err(|_| "not a valid private key".to_owned())?;
        Ok(Self::from_key(key))
    }

    pub fn from_key(key: SigningKey) -> Self {
        let pubkey = key.verifying_key().to_encoded_point(false);
        let hash = keccak(&pubkey.as_bytes()[1..]);
        let address = eip55(&hash[12..]);
        let address_word = hex_to_b32(&address).unwrap_or([0; 32]);
        Self {
            key,
            address,
            address_word,
        }
    }

    /// Read the key from an environment variable. The value never reaches a
    /// log or an error message.
    pub fn from_env(var: &str) -> Result<Self, String> {
        let v = std::env::var(var).map_err(|_| format!("{var} is not set"))?;
        // (a stack file written on Windows leaves a trailing CR)
        Self::new(v.trim()).map_err(|e| format!("{var}: {e}"))
    }

    /// Sign a 32-byte digest: r || s || v (v = 27 + recovery id).
    pub fn sign_digest_bytes(&self, digest: &[u8; 32]) -> Result<[u8; 65], String> {
        let (sig, recid): (Signature, RecoveryId) = self
            .key
            .sign_prehash_recoverable(digest)
            .map_err(|e| format!("sign failed: {e}"))?;
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = 27 + recid.to_byte();
        Ok(out)
    }

    /// 0x-hex signature (the golden tests' path).
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Result<String, String> {
        Ok(format!(
            "0x{}",
            hex::encode(self.sign_digest_bytes(digest)?)
        ))
    }

    /// Sign the reference path's order against `exchange` at domain version "2".
    pub fn sign_order(&self, o: &OrderV2, chain_id: u64, exchange: &str) -> Result<String, String> {
        let digest = order_digest(o, chain_id, exchange)?;
        self.sign_digest(&digest)
    }
}

fn eip55(addr20: &[u8]) -> String {
    let lower = hex::encode(addr20);
    let hash = keccak(lower.as_bytes());
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = (hash[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0xf;
        if c.is_ascii_alphabetic() && nibble >= 8 {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn each_protocol_and_market_kind_has_its_own_domain() {
        let d = |p, n| Domain::for_market(p, n);
        assert_eq!(d(Protocol::V2, false).exchange, EXCHANGE_V2);
        assert_eq!(d(Protocol::V2, true).exchange, EXCHANGE_V2_NEG_RISK);
        assert_eq!(d(Protocol::V3, true).exchange, EXCHANGE_V3);
        assert_eq!(d(Protocol::V3, false).exchange, EXCHANGE_V3);
        let seps = [
            d(Protocol::V2, false).separator,
            d(Protocol::V2, true).separator,
            d(Protocol::V3, true).separator,
        ];
        assert!(seps[0] != seps[1] && seps[1] != seps[2] && seps[0] != seps[2]);
        // V3 is one domain for every market
        assert_eq!(
            d(Protocol::V3, false).separator,
            d(Protocol::V3, true).separator
        );
        // the separator is the EIP-712 encoding of the domain, field by field
        let mut enc = Vec::new();
        enc.extend_from_slice(&keccak(DOMAIN_TYPE_STRING.as_bytes()));
        enc.extend_from_slice(&keccak(b"Polymarket CTF Exchange"));
        enc.extend_from_slice(&keccak(b"3"));
        let mut chain = [0u8; 32];
        chain[31] = 137;
        enc.extend_from_slice(&chain);
        let mut contract = [0u8; 32];
        contract[12..].copy_from_slice(&hex::decode(&EXCHANGE_V3[2..]).unwrap());
        enc.extend_from_slice(&contract);
        assert_eq!(d(Protocol::V3, true).separator, keccak(&enc));
        assert_eq!(
            Protocol::from_version(1),
            None,
            "the legacy struct is not signed"
        );
        assert_eq!(Protocol::from_version(3), Some(Protocol::V3));
    }

    #[test]
    fn the_key_never_prints() {
        let s = OrderSigner::new(&format!("0x{}", "ab".repeat(32))).unwrap();
        assert!(!format!("{s:?}").contains("abab"));
        assert!(format!("{s:?}").contains(&s.address));
    }

    #[test]
    fn decimals_and_hex_become_words() {
        let w = dec_to_u256("258").unwrap();
        assert_eq!((w[30], w[31]), (1, 2));
        assert!(dec_to_u256("12a").is_err());
        assert!(dec_to_u256("").is_err());
        assert!(dec_to_u256(&"9".repeat(80)).is_err(), "overflow");
        assert_eq!(hex_to_b32("0x01").unwrap()[31], 1);
        assert!(hex_to_b32(&format!("0x{}", "0".repeat(65))).is_err());
    }
}
