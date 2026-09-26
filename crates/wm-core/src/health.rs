//! Provider health model shared by collectors, risk engine and dashboard.

use crate::ids::{ProviderId, StationId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Health state of an external provider (optionally scoped to one station).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthState {
    /// Requests succeed and data is fresh.
    Healthy,
    /// Requests succeed but with elevated errors/latency, or a report is late.
    Degraded,
    /// The provider throttled us (HTTP 429 / Retry-After). We are backing off.
    Throttled,
    /// Requests succeed but no new data has arrived within the expected cadence.
    Stale,
    /// Circuit open / repeated failures / unreachable.
    Unavailable,
}

impl ProviderHealthState {
    /// Fail closed: only a fully healthy observation source may support a new
    /// weather-dependent position.
    pub fn allows_new_weather_positions(self) -> bool {
        matches!(self, ProviderHealthState::Healthy)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderHealthState::Healthy => "healthy",
            ProviderHealthState::Degraded => "degraded",
            ProviderHealthState::Throttled => "throttled",
            ProviderHealthState::Stale => "stale",
            ProviderHealthState::Unavailable => "unavailable",
        }
    }

    /// 0 = best … 4 = worst (for aggregation and colouring).
    pub fn severity(self) -> u8 {
        match self {
            ProviderHealthState::Healthy => 0,
            ProviderHealthState::Degraded => 1,
            ProviderHealthState::Stale => 2,
            ProviderHealthState::Throttled => 3,
            ProviderHealthState::Unavailable => 4,
        }
    }
}

impl fmt::Display for ProviderHealthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Circuit-breaker state of a provider gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Point-in-time health of a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealthSnapshot {
    pub provider: ProviderId,
    pub scope: Option<StationId>,
    pub state: ProviderHealthState,
    pub reason: String,
    pub last_request_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    /// Knowledge time of the most recent NEW observation delivered by this provider.
    pub last_new_observation_at: Option<DateTime<Utc>>,
    /// Observation time of the newest observation known for the scope.
    pub last_observation_time: Option<DateTime<Utc>>,
    pub last_http_status: Option<u16>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub current_backoff_ms: u64,
    pub blocked_until: Option<DateTime<Utc>>,
    pub latency_ms_last: Option<u64>,
    pub latency_ms_ewma: Option<f64>,
    pub throttle_events: u64,
    pub requests_total: u64,
    pub requests_today: u32,
    pub daily_budget: Option<u32>,
    pub circuit: CircuitState,
    pub updated_at: DateTime<Utc>,
}

impl ProviderHealthSnapshot {
    pub fn new(provider: ProviderId, scope: Option<StationId>, now: DateTime<Utc>) -> Self {
        Self {
            provider,
            scope,
            state: ProviderHealthState::Unavailable,
            reason: "no requests yet".to_owned(),
            last_request_at: None,
            last_success_at: None,
            last_new_observation_at: None,
            last_observation_time: None,
            last_http_status: None,
            last_error: None,
            consecutive_failures: 0,
            current_backoff_ms: 0,
            blocked_until: None,
            latency_ms_last: None,
            latency_ms_ewma: None,
            throttle_events: 0,
            requests_total: 0,
            requests_today: 0,
            daily_budget: None,
            circuit: CircuitState::Closed,
            updated_at: now,
        }
    }
}
