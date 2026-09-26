//! Content hashing for raw payload identity and rules-text versioning.

use sha2::{Digest, Sha256};

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Normalize a raw report line before hashing so that transport artefacts
/// (trailing `=`, CR/LF, repeated spaces) do not create false "corrections".
pub fn normalize_report_text(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('=').trim();
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn normalization_removes_transport_noise() {
        let a = normalize_report_text("EHAM 261255Z 24012KT  9999 FEW030 18/12 Q1016 NOSIG=\r\n");
        let b = normalize_report_text("EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG");
        assert_eq!(a, b);
    }
}
