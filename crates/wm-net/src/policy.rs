//! Provider-specific rate-limit policies with compiled-in safety floors.
//!
//! Policies are per provider. One provider's limits are never applied to
//! another. For NOAA/NWS-operated services, compiled-in floors cannot be
//! lowered by configuration: Weather Machine never polls faster than the floor
//! and never probes for undocumented limits.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Serde helper: `Duration` as floating-point seconds in configuration files.
pub mod secs_f64 {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let v = f64::deserialize(d)?;
        if !v.is_finite() || v < 0.0 {
            return Err(serde::de::Error::custom(
                "duration must be a non-negative number of seconds",
            ));
        }
        Ok(Duration::from_secs_f64(v))
    }
}

/// Class of provider. Determines non-overridable floors and metric prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderClass {
    /// NOAA / NWS operated services (weather.gov, aviationweather.gov, tgftp).
    NoaaNws,
    /// Other public weather data providers (KNMI, Iowa Mesonet, Synoptic, …).
    PublicData,
    /// Exchange APIs (Polymarket) with documented limits.
    Exchange,
    /// Local/test endpoints.
    Local,
}

impl ProviderClass {
    /// Hard minimum spacing between requests that configuration cannot lower.
    pub fn min_interval_floor(self) -> Duration {
        match self {
            ProviderClass::NoaaNws => Duration::from_secs(30),
            ProviderClass::PublicData => Duration::from_secs(5),
            ProviderClass::Exchange => Duration::from_millis(50),
            ProviderClass::Local => Duration::ZERO,
        }
    }

    /// Hard maximum concurrency.
    pub fn max_concurrency_cap(self) -> u32 {
        match self {
            ProviderClass::NoaaNws | ProviderClass::PublicData => 1,
            ProviderClass::Exchange => 8,
            ProviderClass::Local => 64,
        }
    }

    /// Whether honouring `Retry-After` is mandatory.
    pub fn must_respect_retry_after(self) -> bool {
        !matches!(self, ProviderClass::Local)
    }

    pub fn metric_prefix(self) -> &'static str {
        match self {
            ProviderClass::NoaaNws => "nws",
            ProviderClass::PublicData => "weather_data",
            ProviderClass::Exchange => "polymarket",
            ProviderClass::Local => "local",
        }
    }
}

/// Policy violation found during validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("{field} = {value:?} is below the {class:?} floor of {floor:?}")]
    BelowFloor {
        field: &'static str,
        value: Duration,
        floor: Duration,
        class: ProviderClass,
    },
    #[error("max_concurrency {value} exceeds the {class:?} cap of {cap}")]
    ConcurrencyAboveCap {
        value: u32,
        cap: u32,
        class: ProviderClass,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Rate-limit and resilience policy of one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitPolicy {
    pub class: ProviderClass,
    /// Minimum spacing between request starts.
    #[serde(with = "secs_f64", rename = "min_interval_secs")]
    pub min_interval: Duration,
    pub max_concurrency: u32,
    /// Total request timeout.
    #[serde(with = "secs_f64", rename = "timeout_secs")]
    pub timeout: Duration,
    #[serde(with = "secs_f64", rename = "connect_timeout_secs")]
    pub connect_timeout: Duration,
    /// Base of the exponential backoff after failures (equal jitter).
    #[serde(with = "secs_f64", rename = "backoff_base_secs")]
    pub backoff_base: Duration,
    #[serde(with = "secs_f64", rename = "backoff_max_secs")]
    pub backoff_max: Duration,
    /// Backoff base used after a 429 that carried no `Retry-After`.
    #[serde(with = "secs_f64", rename = "throttle_backoff_base_secs")]
    pub throttle_backoff_base: Duration,
    pub respect_retry_after: bool,
    /// Upper bound on how long a single `Retry-After` is honoured before the
    /// provider is declared unavailable for operator attention.
    #[serde(with = "secs_f64", rename = "max_retry_after_secs")]
    pub max_retry_after: Duration,
    /// Consecutive failures that open the circuit.
    pub circuit_failure_threshold: u32,
    #[serde(with = "secs_f64", rename = "circuit_open_base_secs")]
    pub circuit_open_base: Duration,
    #[serde(with = "secs_f64", rename = "circuit_open_max_secs")]
    pub circuit_open_max: Duration,
    /// Hard cap on requests per UTC day (None = uncapped).
    pub daily_budget: Option<u32>,
    /// After a throttle event the effective minimum interval is multiplied by
    /// this factor (compounding, capped at 16×) for `politeness_decay`.
    pub politeness_factor: u32,
    #[serde(with = "secs_f64", rename = "politeness_decay_secs")]
    pub politeness_decay: Duration,
    /// Maximum accepted response body size.
    pub max_body_bytes: usize,
}

impl RateLimitPolicy {
    /// Conservative default for NOAA/NWS services.
    pub fn nws_conservative() -> Self {
        Self {
            class: ProviderClass::NoaaNws,
            min_interval: Duration::from_secs(30),
            max_concurrency: 1,
            timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(10),
            backoff_base: Duration::from_secs(60),
            backoff_max: Duration::from_secs(30 * 60),
            throttle_backoff_base: Duration::from_secs(5 * 60),
            respect_retry_after: true,
            max_retry_after: Duration::from_secs(6 * 3600),
            circuit_failure_threshold: 5,
            circuit_open_base: Duration::from_secs(10 * 60),
            circuit_open_max: Duration::from_secs(2 * 3600),
            daily_budget: Some(2_000),
            politeness_factor: 2,
            politeness_decay: Duration::from_secs(24 * 3600),
            max_body_bytes: 2 * 1024 * 1024,
        }
    }

    /// Default for public (non-NOAA) weather data services.
    pub fn public_data_conservative() -> Self {
        Self {
            class: ProviderClass::PublicData,
            min_interval: Duration::from_secs(10),
            daily_budget: Some(5_000),
            ..Self::nws_conservative()
        }
    }

    /// Default for Polymarket REST: far below documented limits.
    pub fn polymarket_rest() -> Self {
        Self {
            class: ProviderClass::Exchange,
            min_interval: Duration::from_millis(250),
            max_concurrency: 2,
            timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(5 * 60),
            throttle_backoff_base: Duration::from_secs(30),
            respect_retry_after: true,
            max_retry_after: Duration::from_secs(3600),
            circuit_failure_threshold: 8,
            circuit_open_base: Duration::from_secs(60),
            circuit_open_max: Duration::from_secs(30 * 60),
            daily_budget: Some(200_000),
            politeness_factor: 2,
            politeness_decay: Duration::from_secs(3600),
            max_body_bytes: 8 * 1024 * 1024,
        }
    }

    /// Permissive policy for tests against a local mock server.
    pub fn local_test() -> Self {
        Self {
            class: ProviderClass::Local,
            min_interval: Duration::from_millis(10),
            max_concurrency: 1,
            timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            backoff_base: Duration::from_millis(50),
            backoff_max: Duration::from_secs(1),
            throttle_backoff_base: Duration::from_millis(100),
            respect_retry_after: true,
            max_retry_after: Duration::from_secs(60),
            circuit_failure_threshold: 3,
            circuit_open_base: Duration::from_millis(500),
            circuit_open_max: Duration::from_secs(5),
            daily_budget: None,
            politeness_factor: 2,
            politeness_decay: Duration::from_secs(60),
            max_body_bytes: 64 * 1024,
        }
    }

    /// Validate against the class floors. Configuration that tries to poll a
    /// NOAA service faster than the floor is rejected at startup (fail closed).
    pub fn validate(&self) -> Result<(), PolicyError> {
        let floor = self.class.min_interval_floor();
        if self.min_interval < floor {
            return Err(PolicyError::BelowFloor {
                field: "min_interval",
                value: self.min_interval,
                floor,
                class: self.class,
            });
        }
        let cap = self.class.max_concurrency_cap();
        if self.max_concurrency == 0 {
            return Err(PolicyError::Invalid("max_concurrency must be >= 1".into()));
        }
        if self.max_concurrency > cap {
            return Err(PolicyError::ConcurrencyAboveCap {
                value: self.max_concurrency,
                cap,
                class: self.class,
            });
        }
        if self.class.must_respect_retry_after() && !self.respect_retry_after {
            return Err(PolicyError::Invalid(format!(
                "respect_retry_after cannot be disabled for {:?} providers",
                self.class
            )));
        }
        if self.timeout.is_zero() || self.connect_timeout.is_zero() {
            return Err(PolicyError::Invalid("timeouts must be positive".into()));
        }
        if self.backoff_base.is_zero() || self.backoff_max < self.backoff_base {
            return Err(PolicyError::Invalid(
                "backoff_max must be >= backoff_base > 0".into(),
            ));
        }
        if self.circuit_failure_threshold == 0 {
            return Err(PolicyError::Invalid(
                "circuit_failure_threshold must be >= 1".into(),
            ));
        }
        if self.circuit_open_max < self.circuit_open_base {
            return Err(PolicyError::Invalid(
                "circuit_open_max must be >= circuit_open_base".into(),
            ));
        }
        if self.politeness_factor == 0 {
            return Err(PolicyError::Invalid(
                "politeness_factor must be >= 1".into(),
            ));
        }
        if self.max_body_bytes == 0 {
            return Err(PolicyError::Invalid("max_body_bytes must be > 0".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        RateLimitPolicy::nws_conservative().validate().unwrap();
        RateLimitPolicy::public_data_conservative()
            .validate()
            .unwrap();
        RateLimitPolicy::polymarket_rest().validate().unwrap();
        RateLimitPolicy::local_test().validate().unwrap();
    }

    #[test]
    fn nws_floor_cannot_be_lowered() {
        let mut p = RateLimitPolicy::nws_conservative();
        p.min_interval = Duration::from_secs(5);
        assert!(matches!(p.validate(), Err(PolicyError::BelowFloor { .. })));
        let mut p = RateLimitPolicy::nws_conservative();
        p.max_concurrency = 2;
        assert!(matches!(
            p.validate(),
            Err(PolicyError::ConcurrencyAboveCap { .. })
        ));
        let mut p = RateLimitPolicy::nws_conservative();
        p.respect_retry_after = false;
        assert!(p.validate().is_err());
    }

    #[test]
    fn policy_round_trips_through_toml_like_json() {
        let p = RateLimitPolicy::nws_conservative();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"min_interval_secs\":30.0"));
        let back: RateLimitPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
