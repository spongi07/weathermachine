//! Strongly typed identifiers. Newtypes prevent mixing, e.g., a token id with a condition id.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Error for invalid identifiers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} identifier '{value}'")]
pub struct IdError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    StationId,
    "Observation station identifier, e.g. ICAO `EHAM`."
);
string_id!(
    LocationId,
    "Weather Machine location slug, e.g. `amsterdam`."
);
string_id!(
    ProviderId,
    "External provider identifier, e.g. `awc`, `tgftp`, `polymarket_clob`."
);
string_id!(ConditionId, "Polymarket/CTF condition id (hex string).");
string_id!(
    TokenId,
    "Polymarket CLOB token (ERC-1155 position) id (decimal string)."
);
string_id!(QuestionId, "Polymarket question id (hex string).");
string_id!(EventSlug, "Polymarket event slug.");
string_id!(
    StrategyId,
    "Strategy identifier, e.g. `buy_yes_final_high`."
);
string_id!(
    ClientOrderId,
    "Deterministic client order id assigned by Weather Machine."
);

impl StationId {
    /// Station ids are 3–8 upper-case ASCII alphanumerics (ICAO `EHAM`, WRH `FHMC1`).
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value: String = value.into().trim().to_ascii_uppercase();
        let ok = (3..=8).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric());
        if ok {
            Ok(Self(value))
        } else {
            Err(IdError {
                kind: "station",
                value,
            })
        }
    }
}

impl LocationId {
    /// Location ids are lower-case slugs: `[a-z0-9-]{2,48}`.
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value: String = value.into();
        let ok = (2..=48).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if ok {
            Ok(Self(value))
        } else {
            Err(IdError {
                kind: "location",
                value,
            })
        }
    }
}

macro_rules! simple_new {
    ($name:ident, $kind:literal) => {
        impl $name {
            /// Construct from a compile-time literal (validated in debug builds).
            pub fn from_static(value: &'static str) -> Self {
                debug_assert!(
                    !value.is_empty() && !value.chars().any(char::is_whitespace),
                    "invalid static id"
                );
                Self(value.to_owned())
            }

            /// Construct from a string built by trusted code (validated in debug builds).
            pub fn from_static_string(value: String) -> Self {
                debug_assert!(
                    !value.is_empty() && !value.chars().any(char::is_whitespace),
                    "invalid id"
                );
                Self(value)
            }

            /// Construct from any non-empty string without whitespace.
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value: String = value.into();
                if value.is_empty() || value.chars().any(char::is_whitespace) {
                    Err(IdError { kind: $kind, value })
                } else {
                    Ok(Self(value))
                }
            }
        }
    };
}

simple_new!(ProviderId, "provider");
simple_new!(ConditionId, "condition");
simple_new!(TokenId, "token");
simple_new!(QuestionId, "question");
simple_new!(EventSlug, "event slug");
simple_new!(StrategyId, "strategy");
simple_new!(ClientOrderId, "client order");

impl ProviderId {
    /// NOAA Aviation Weather Center Data API (official, documented rate limit).
    pub fn awc() -> Self {
        Self("awc".to_owned())
    }
    /// Iowa Environmental Mesonet ASOS/METAR archive (history for model training).
    pub fn iem() -> Self {
        Self("iem".to_owned())
    }
    /// NWS Telecommunication Gateway file server (official product files).
    pub fn tgftp() -> Self {
        Self("tgftp".to_owned())
    }
    /// api.weather.gov (official NWS API).
    pub fn nws_api() -> Self {
        Self("nws_api".to_owned())
    }
    /// Synoptic Data API — the data behind weather.gov/wrh/timeseries (requires own token).
    pub fn synoptic() -> Self {
        Self("synoptic".to_owned())
    }
    pub fn polymarket_gamma() -> Self {
        Self("polymarket_gamma".to_owned())
    }
    pub fn polymarket_clob() -> Self {
        Self("polymarket_clob".to_owned())
    }
    pub fn polymarket_ws() -> Self {
        Self("polymarket_ws".to_owned())
    }
    /// Historical replay / backtest source.
    pub fn replay() -> Self {
        Self("replay".to_owned())
    }
    /// Deterministic synthetic data (demo mode, tests). Never mixed with real data.
    pub fn synthetic() -> Self {
        Self("synthetic".to_owned())
    }
}

/// Monotonic decision identifier, unique within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionId(pub u64);

impl fmt::Display for DecisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "D{:08}", self.0)
    }
}

/// Unique id of an engine run (backtest, paper session, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub uuid::Uuid);

impl RunId {
    /// Time-ordered random id for live/paper runs.
    pub fn new_v7() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// Deterministic id for reproducible backtests.
    pub fn deterministic(seed: u64) -> Self {
        Self(uuid::Uuid::from_u64_pair(0x5745_4154_4845_5200, seed))
    }

    /// Short hex prefix used inside client order ids.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_owned()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ClientOrderId {
    /// Deterministic order id from run + decision + leg; identical inputs always
    /// yield identical ids, which makes resubmission idempotent.
    pub fn derive(run: &RunId, decision: DecisionId, leg: u8) -> Self {
        Self(format!("wm-{}-{}-{}", run.short(), decision.0, leg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_ids_are_normalized_and_validated() {
        assert_eq!(StationId::new("eham").unwrap().as_str(), "EHAM");
        assert_eq!(StationId::new("FHMC1").unwrap().as_str(), "FHMC1");
        assert!(StationId::new("EH").is_err());
        assert!(StationId::new("EH AM").is_err());
        assert!(StationId::new("EHAM-").is_err());
    }

    #[test]
    fn location_ids_are_slugs() {
        assert!(LocationId::new("amsterdam").is_ok());
        assert!(LocationId::new("new-york").is_ok());
        assert!(LocationId::new("Amsterdam").is_err());
        assert!(LocationId::new("a").is_err());
    }

    #[test]
    fn client_order_ids_are_deterministic() {
        let run = RunId::deterministic(7);
        let a = ClientOrderId::derive(&run, DecisionId(42), 0);
        let b = ClientOrderId::derive(&run, DecisionId(42), 0);
        assert_eq!(a, b);
        assert_ne!(a, ClientOrderId::derive(&run, DecisionId(42), 1));
        assert!(a.as_str().starts_with("wm-"));
    }

    #[test]
    fn simple_ids_reject_whitespace() {
        assert!(TokenId::new("123 456").is_err());
        assert!(TokenId::new("").is_err());
        assert!(TokenId::new("7123456789").is_ok());
    }
}
