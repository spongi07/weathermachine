//! CLOB authentication, both levels (ported from dailyprofit's
//! `crates/execution/src/auth.rs`, fixture-proven against the reference
//! client in `tests/golden.rs`).
//!
//! * **L1**: an EIP-712 `ClobAuth` signature over (address, timestamp,
//!   nonce, message), domain `ClobAuthDomain` version "1": derives or
//!   creates the API credentials. The nonce picks the key: dailyprofit uses
//!   0, this system 1 by default, so the two never share the credentials the
//!   venue's heartbeat switch cancels by.
//! * **L2**: HMAC-SHA256 with the url-safe base64-decoded secret over
//!   `timestamp + METHOD + path + body`, url-safe base64-encoded, on every
//!   trading request. The path excludes the query string; the body is the
//!   exact bytes that go on the wire.

use crate::sign::{OrderSigner, dec_to_u256, hex_to_b32, keccak};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

const CLOB_AUTH_TYPE: &str =
    "ClobAuth(address address,string timestamp,uint256 nonce,string message)";
const CLOB_DOMAIN_TYPE: &str = "EIP712Domain(string name,string version,uint256 chainId)";
const MSG_TO_SIGN: &str = "This message attests that I control the given wallet";

/// The L1 `ClobAuth` signature.
pub fn clob_auth_signature(
    signer: &OrderSigner,
    chain_id: u64,
    timestamp: u64,
    nonce: u64,
) -> Result<String, String> {
    let mut dom = Vec::with_capacity(128);
    dom.extend_from_slice(&keccak(CLOB_DOMAIN_TYPE.as_bytes()));
    dom.extend_from_slice(&keccak(b"ClobAuthDomain"));
    dom.extend_from_slice(&keccak(b"1"));
    dom.extend_from_slice(&dec_to_u256(&chain_id.to_string())?);
    let domain_sep = keccak(&dom);

    let mut st = Vec::with_capacity(160);
    st.extend_from_slice(&keccak(CLOB_AUTH_TYPE.as_bytes()));
    st.extend_from_slice(&hex_to_b32(&signer.address)?);
    st.extend_from_slice(&keccak(timestamp.to_string().as_bytes()));
    st.extend_from_slice(&dec_to_u256(&nonce.to_string())?);
    st.extend_from_slice(&keccak(MSG_TO_SIGN.as_bytes()));
    let struct_hash = keccak(&st);

    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(b"\x19\x01");
    msg.extend_from_slice(&domain_sep);
    msg.extend_from_slice(&struct_hash);
    signer.sign_digest(&keccak(&msg))
}

/// The L2 HMAC over exactly the serialized body that will be sent.
pub fn hmac_signature(
    secret_b64: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<String, String> {
    let secret = base64::engine::general_purpose::URL_SAFE
        .decode(secret_b64)
        .map_err(|_| "the API secret is not url-safe base64".to_owned())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(timestamp.as_bytes());
    mac.update(method.as_bytes());
    mac.update(path.as_bytes());
    if let Some(b) = body {
        mac.update(b);
    }
    Ok(base64::engine::general_purpose::URL_SAFE.encode(mac.finalize().into_bytes()))
}

/// The API credentials the L1 signature derives. Held in memory only; they
/// never print, not even in a panic.
#[derive(Clone, serde::Deserialize)]
pub struct ApiCreds {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl std::fmt::Debug for ApiCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiCreds(api_key=***, secret=***, passphrase=***)")
    }
}

/// The five L2 headers of one request.
pub fn l2_headers(
    signer: &OrderSigner,
    creds: &ApiCreds,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timestamp: u64,
) -> Result<[(&'static str, String); 5], String> {
    let ts = timestamp.to_string();
    let sig = hmac_signature(&creds.secret, &ts, method, path, body)?;
    Ok([
        ("POLY_ADDRESS", signer.address.clone()),
        ("POLY_SIGNATURE", sig),
        ("POLY_TIMESTAMP", ts),
        ("POLY_API_KEY", creds.api_key.clone()),
        ("POLY_PASSPHRASE", creds.passphrase.clone()),
    ])
}

/// The four L1 headers (derive or create the API key of `nonce`).
pub fn l1_headers(
    signer: &OrderSigner,
    chain_id: u64,
    timestamp_s: u64,
    nonce: u64,
) -> Result<Vec<(&'static str, String)>, String> {
    let sig = clob_auth_signature(signer, chain_id, timestamp_s, nonce)?;
    Ok(vec![
        ("POLY_ADDRESS", signer.address.clone()),
        ("POLY_SIGNATURE", sig),
        ("POLY_TIMESTAMP", timestamp_s.to_string()),
        ("POLY_NONCE", nonce.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn credentials_never_print() {
        let c = ApiCreds {
            api_key: "key-123".into(),
            secret: "c2VjcmV0".into(),
            passphrase: "pass-xyz".into(),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("key-123") && !shown.contains("c2VjcmV0"));
        assert!(!shown.contains("pass-xyz"));
    }

    #[test]
    fn the_nonce_is_signed_and_sent() {
        let s = OrderSigner::new(&format!("0x{}", "ab".repeat(32))).unwrap();
        let a = l1_headers(&s, 137, 1_786_090_000, 0).unwrap();
        let b = l1_headers(&s, 137, 1_786_090_000, 1).unwrap();
        assert_eq!(b[3], ("POLY_NONCE", "1".to_owned()));
        assert_ne!(a[1].1, b[1].1, "another nonce, another signature");
    }
}
