//! Forecast layer (blueprint §19, roadmap Phase 11).
//!
//! Forecasts are **predictive inputs**. They never replace a market's
//! resolution source (`wm_core::resolution::ResolutionSourceKind`) nor the
//! observation source (`crate::ObservationSource`): the kernel ignores
//! [`ForecastEvent`]s for the day's observed high, settlement and data-health
//! gates, and only a probability model may use them as features once a
//! backtest shows they add information (hypothesis, not assumption).
//!
//! Every provider owns a rate-limit gate like any other external source;
//! forecast freshness is tracked separately from observation freshness.

use chrono::{DateTime, NaiveDate, Utc};
use std::sync::Arc;
use std::time::Duration;
use wm_core::event::ForecastEvent;
use wm_core::ids::{LocationId, ProviderId};
use wm_core::ingest::BoxFuture;
use wm_net::{FetchError, ProviderGate};

/// What to forecast.
#[derive(Debug, Clone, PartialEq)]
pub struct ForecastQuery {
    pub location: LocationId,
    pub latitude: f64,
    pub longitude: f64,
    /// Local calendar day whose maximum is of interest.
    pub local_date: NaiveDate,
}

/// Forecast retrieval errors.
#[derive(Debug, thiserror::Error)]
pub enum ForecastError {
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    #[error("malformed forecast payload: {0}")]
    Malformed(String),
    #[error("forecast not available for {0}")]
    Unavailable(String),
}

/// A forecast source (KNMI, ECMWF, GFS, …). Implementations must route every
/// request through their own [`ProviderGate`] and must never be consulted for
/// resolution or settlement.
pub trait ForecastProvider: Send + Sync {
    fn provider(&self) -> &ProviderId;
    fn gate(&self) -> &Arc<ProviderGate>;
    /// Model/run identifier, e.g. `"harmonie-arome"` or `"gfs-0.25"`.
    fn model(&self) -> &str;
    fn fetch<'a>(
        &'a self,
        query: &'a ForecastQuery,
        max_gate_wait: Duration,
    ) -> BoxFuture<'a, Result<ForecastEvent, ForecastError>>;
}

/// Age of a forecast relative to `now` (for the forecast-freshness gate).
pub fn forecast_age(event: &ForecastEvent, now: DateTime<Utc>) -> chrono::Duration {
    now - event.issued_at
}

/// Fixed forecasts for tests and replays of archived forecast data. Performs
/// no requests; the gate exists only to satisfy the port.
pub struct StaticForecastProvider {
    provider: ProviderId,
    gate: Arc<ProviderGate>,
    model: String,
    events: Vec<ForecastEvent>,
}

impl StaticForecastProvider {
    pub fn new(
        provider: ProviderId,
        gate: Arc<ProviderGate>,
        model: impl Into<String>,
        events: Vec<ForecastEvent>,
    ) -> Self {
        Self {
            provider,
            gate,
            model: model.into(),
            events,
        }
    }
}

impl ForecastProvider for StaticForecastProvider {
    fn provider(&self) -> &ProviderId {
        &self.provider
    }

    fn gate(&self) -> &Arc<ProviderGate> {
        &self.gate
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn fetch<'a>(
        &'a self,
        query: &'a ForecastQuery,
        _max_gate_wait: Duration,
    ) -> BoxFuture<'a, Result<ForecastEvent, ForecastError>> {
        Box::pin(async move {
            self.events
                .iter()
                .filter(|e| e.location == query.location)
                .max_by_key(|e| e.issued_at)
                .cloned()
                .ok_or_else(|| {
                    ForecastError::Unavailable(format!("{} {}", query.location, query.local_date))
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::time::ManualClock;
    use wm_core::units::TempC;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn static_provider_returns_latest_issue_for_location() {
        let clock = Arc::new(ManualClock::new(utc("2026-07-01T06:00:00Z")));
        let gate = ProviderGate::new(
            ProviderId::replay(),
            wm_net::RateLimitPolicy::local_test(),
            clock,
            1,
        );
        let ams = LocationId::new("amsterdam").unwrap();
        let ev = |issued: &str, max: i32| ForecastEvent {
            location: ams.clone(),
            provider: ProviderId::replay(),
            model: "archive".into(),
            issued_at: utc(issued),
            predicted_max: Some(TempC::from_whole(max)),
            hourly: Vec::new(),
        };
        let p = StaticForecastProvider::new(
            ProviderId::replay(),
            gate,
            "archive",
            vec![
                ev("2026-07-01T00:00:00Z", 19),
                ev("2026-07-01T06:00:00Z", 21),
            ],
        );
        let q = ForecastQuery {
            location: ams.clone(),
            latitude: 52.3,
            longitude: 4.8,
            local_date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap_or_default(),
        };
        let got = p.fetch(&q, Duration::ZERO).await;
        assert!(matches!(&got, Ok(e) if e.predicted_max == Some(TempC::from_whole(21))));
        assert_eq!(
            got.map(|e| forecast_age(&e, utc("2026-07-01T07:00:00Z")).num_minutes())
                .ok(),
            Some(60)
        );
        let other = ForecastQuery {
            location: LocationId::new("london").unwrap(),
            ..q
        };
        assert!(matches!(
            p.fetch(&other, Duration::ZERO).await,
            Err(ForecastError::Unavailable(_))
        ));
    }
}
