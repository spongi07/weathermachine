//! Open-Meteo Previous Runs API: hourly 2 m temperature at a fixed lead.
//!
//! `temperature_2m_previous_day1` is, for every valid hour, the value of the
//! model run initialised 24 hours before that hour. The same product serves
//! model training (years of history) and live trading (today's series), so
//! the model is evaluated and used on identical data — unlike "historical
//! forecast" archives stitched from the first hours of each run, which carry
//! same-day information into the past (look-ahead).
//!
//! Predictive input only: never observation, resolution or settlement data.
//! Every request goes through the `open_meteo` gate (spacing, daily budget,
//! backoff, circuit breaker). A commercial API key switches to the customer
//! host; it is sent only in the request URL, and audit records carry a
//! key-free endpoint label (the HTTP layer never logs URLs).

use crate::forecast::{ForecastError, ForecastProvider, ForecastQuery};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use wm_core::event::ForecastEvent;
use wm_core::ids::ProviderId;
use wm_core::ingest::{BoxFuture, ProviderRequestRecord};
use wm_core::units::TempC;
use wm_net::{FetchError, FetchRequest, HttpFetcher, ProviderGate};

/// Free (non-commercial) host.
pub const FREE_HOST: &str = "https://previous-runs-api.open-meteo.com";
/// Host for API-key (commercial) subscriptions.
pub const CUSTOMER_HOST: &str = "https://customer-previous-runs-api.open-meteo.com";

/// Physically plausible 2 m temperatures (°C); anything else is treated as missing.
const PLAUSIBLE_C: std::ops::RangeInclusive<f64> = -90.0..=60.0;

/// Why a series could not be obtained.
#[derive(Debug, thiserror::Error)]
pub enum OpenMeteoError {
    #[error(transparent)]
    Fetch(FetchError),
    /// HTTP 400 with the API's explanation (unknown model, date out of range…).
    #[error("rejected by Open-Meteo: {reason}")]
    Rejected {
        reason: String,
        /// First date of the allowed range, when the reason states one.
        allowed_from: Option<NaiveDate>,
        record: Box<ProviderRequestRecord>,
    },
    #[error("unexpected response: {0}")]
    Malformed(String),
}

impl OpenMeteoError {
    /// Audit record of the request, when one reached the network.
    pub fn record(&self) -> Option<&ProviderRequestRecord> {
        match self {
            OpenMeteoError::Fetch(e) => e.record(),
            OpenMeteoError::Rejected { record, .. } => Some(record),
            OpenMeteoError::Malformed(_) => None,
        }
    }
}

impl From<FetchError> for OpenMeteoError {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Status {
                status: 400,
                detail,
                record,
            } => {
                let reason = detail
                    .as_deref()
                    .map(api_reason)
                    .unwrap_or_else(|| "HTTP 400".to_owned());
                OpenMeteoError::Rejected {
                    allowed_from: allowed_from(&reason),
                    reason,
                    record,
                }
            }
            other => OpenMeteoError::Fetch(other),
        }
    }
}

/// `reason` of an Open-Meteo error body `{"error":true,"reason":"…"}`.
fn api_reason(detail: &str) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        reason: String,
    }
    serde_json::from_str::<ApiError>(detail)
        .map(|e| e.reason)
        .unwrap_or_else(|_| detail.to_owned())
}

/// The date after "from " in e.g. "… out of allowed range from 2021-03-23 to 2026-10-12".
fn allowed_from(reason: &str) -> Option<NaiveDate> {
    let at = reason.find("from ")? + 5;
    NaiveDate::parse_from_str(reason.get(at..at + 10)?, "%Y-%m-%d").ok()
}

/// Hourly values by valid time (UTC); `None` where the archive has no value.
pub type HourlySeries = Vec<(DateTime<Utc>, Option<TempC>)>;

/// One downloaded series.
#[derive(Debug, Clone)]
pub struct ForecastSeries {
    /// Valid time (UTC) and value; `None` where the archive has no value.
    pub hourly: HourlySeries,
    /// Raw JSON body (cached by training for provenance and reuse).
    pub body: Vec<u8>,
    pub fetched_at: DateTime<Utc>,
    pub record: ProviderRequestRecord,
}

impl ForecastSeries {
    /// Hours that have a value.
    pub fn values(&self) -> Vec<(DateTime<Utc>, TempC)> {
        known(&self.hourly)
    }
}

/// Hours that have a value.
pub fn known(hourly: &[(DateTime<Utc>, Option<TempC>)]) -> Vec<(DateTime<Utc>, TempC)> {
    hourly
        .iter()
        .filter_map(|(t, v)| v.map(|v| (*t, v)))
        .collect()
}

/// Client for one model at one fixed lead.
pub struct OpenMeteoPreviousRuns {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
    api_key: Option<String>,
    model: String,
    lead_days: u8,
}

impl std::fmt::Debug for OpenMeteoPreviousRuns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenMeteoPreviousRuns")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("lead_days", &self.lead_days)
            .finish()
    }
}

impl OpenMeteoPreviousRuns {
    pub fn new(
        fetcher: Arc<HttpFetcher>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        lead_days: u8,
    ) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            model: model.into(),
            lead_days,
        }
    }

    pub fn provider_id(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    pub fn lead_days(&self) -> u8 {
        self.lead_days
    }

    /// Hourly variable name for the lead (`temperature_2m_previous_dayN`).
    pub fn variable(lead_days: u8) -> String {
        if lead_days == 0 {
            "temperature_2m".to_owned()
        } else {
            format!("temperature_2m_previous_day{lead_days}")
        }
    }

    fn query(&self, latitude: f64, longitude: f64, start: NaiveDate, end: NaiveDate) -> String {
        format!(
            "/v1/forecast?latitude={latitude:.4}&longitude={longitude:.4}&hourly={}&models={}&timezone=GMT&start_date={start}&end_date={end}",
            Self::variable(self.lead_days),
            self.model
        )
    }

    /// Download `[start, end]` (UTC dates, inclusive) for a point.
    pub async fn fetch_series(
        &self,
        latitude: f64,
        longitude: f64,
        start: NaiveDate,
        end: NaiveDate,
        max_gate_wait: Duration,
    ) -> Result<ForecastSeries, OpenMeteoError> {
        let query = self.query(latitude, longitude, start, end);
        let mut url = format!("{}{query}", self.base_url);
        if let Some(key) = &self.api_key {
            url.push_str("&apikey=");
            url.push_str(key);
        }
        let req = FetchRequest::get(url, query)
            .accept("application/json")
            .max_gate_wait(max_gate_wait)
            .unconditional();
        let resp = self.fetcher.get(&req).await?;
        let hourly = parse_series(&resp.body, &Self::variable(self.lead_days))?;
        Ok(ForecastSeries {
            hourly,
            body: resp.body.to_vec(),
            fetched_at: resp.fetched_at,
            record: resp.record,
        })
    }
}

impl ForecastProvider for OpenMeteoPreviousRuns {
    fn provider(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    fn gate(&self) -> &Arc<ProviderGate> {
        self.fetcher.gate()
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// The series around `query.local_date` (one UTC day either side covers
    /// the local day in every timezone).
    fn fetch<'a>(
        &'a self,
        query: &'a ForecastQuery,
        max_gate_wait: Duration,
    ) -> BoxFuture<'a, Result<ForecastEvent, ForecastError>> {
        Box::pin(async move {
            let start = query.local_date - ChronoDuration::days(1);
            let end = query.local_date + ChronoDuration::days(1);
            let s = self
                .fetch_series(query.latitude, query.longitude, start, end, max_gate_wait)
                .await
                .map_err(|e| match e {
                    OpenMeteoError::Fetch(f) => ForecastError::Fetch(f),
                    OpenMeteoError::Rejected { reason, .. } => ForecastError::Unavailable(reason),
                    OpenMeteoError::Malformed(m) => ForecastError::Malformed(m),
                })?;
            let hourly = s.values();
            if hourly.is_empty() {
                return Err(ForecastError::Unavailable(format!(
                    "no {} values for {}",
                    Self::variable(self.lead_days),
                    query.local_date
                )));
            }
            Ok(ForecastEvent {
                location: query.location.clone(),
                provider: self.fetcher.provider().clone(),
                model: self.model.clone(),
                issued_at: s.fetched_at,
                predicted_max: None,
                hourly,
                lead_days: Some(self.lead_days),
            })
        })
    }
}

#[derive(Deserialize)]
struct Response {
    utc_offset_seconds: Option<i64>,
    #[serde(default)]
    hourly_units: HashMap<String, String>,
    hourly: Option<Hourly>,
}

#[derive(Deserialize)]
struct Hourly {
    time: Vec<String>,
    #[serde(flatten)]
    columns: HashMap<String, serde_json::Value>,
}

/// Parse a Previous Runs response (requested with `timezone=GMT`).
pub fn parse_series(body: &[u8], variable: &str) -> Result<HourlySeries, OpenMeteoError> {
    let r: Response = serde_json::from_slice(body)
        .map_err(|e| OpenMeteoError::Malformed(format!("not an Open-Meteo JSON response: {e}")))?;
    if r.utc_offset_seconds.unwrap_or(0) != 0 {
        return Err(OpenMeteoError::Malformed(
            "times are not UTC (timezone=GMT expected)".into(),
        ));
    }
    let hourly = r
        .hourly
        .ok_or_else(|| OpenMeteoError::Malformed("no hourly block".into()))?;
    // One model ⇒ plain name; several ⇒ `<variable>_<model>`.
    let key = if hourly.columns.contains_key(variable) {
        variable.to_owned()
    } else {
        let mut matching = hourly.columns.keys().filter(|k| k.starts_with(variable));
        match (matching.next(), matching.next()) {
            (Some(k), None) => k.clone(),
            _ => {
                return Err(OpenMeteoError::Malformed(format!(
                    "no single '{variable}' column"
                )));
            }
        }
    };
    if let Some(unit) = r.hourly_units.get(&key)
        && unit != "°C"
    {
        return Err(OpenMeteoError::Malformed(format!(
            "unit {unit:?}, expected °C"
        )));
    }
    let values = hourly.columns[&key]
        .as_array()
        .ok_or_else(|| OpenMeteoError::Malformed(format!("'{key}' is not an array")))?;
    if values.len() != hourly.time.len() {
        return Err(OpenMeteoError::Malformed(format!(
            "{} times but {} values",
            hourly.time.len(),
            values.len()
        )));
    }
    hourly
        .time
        .iter()
        .zip(values)
        .map(|(t, v)| {
            let at = NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M")
                .map_err(|_| OpenMeteoError::Malformed(format!("bad time {t:?}")))?
                .and_utc();
            let temp = v
                .as_f64()
                .filter(|c| PLAUSIBLE_C.contains(c))
                .map(|c| TempC::from_tenths((c * 10.0).round() as i32));
            Ok((at, temp))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_nulls_and_multi_model_names() {
        let body = r#"{"latitude":52.3,"longitude":4.8,"utc_offset_seconds":0,"timezone":"GMT",
            "hourly_units":{"time":"iso8601","temperature_2m_previous_day1":"°C"},
            "hourly":{"time":["2026-07-01T00:00","2026-07-01T01:00","2026-07-01T02:00"],
            "temperature_2m_previous_day1":[15.34,null,-2.05]}}"#
            .as_bytes();
        let s = parse_series(body, "temperature_2m_previous_day1").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].1, Some(TempC::from_tenths(153)));
        assert_eq!(s[1].1, None);
        assert_eq!(s[2].1, Some(TempC::from_tenths(-21)));
        assert_eq!(s[2].0.to_rfc3339(), "2026-07-01T02:00:00+00:00");
        assert_eq!(known(&s).len(), 2);

        let multi = br#"{"utc_offset_seconds":0,"hourly":{"time":["2026-07-01T00:00"],
            "temperature_2m_previous_day1_gfs_global":[12.0]}}"#;
        let s = parse_series(multi, "temperature_2m_previous_day1").unwrap();
        assert_eq!(s[0].1, Some(TempC::from_whole(12)));
    }

    #[test]
    fn rejects_what_it_cannot_trust() {
        let v = "temperature_2m_previous_day1";
        assert!(parse_series(b"<html>busy</html>", v).is_err());
        let local = br#"{"utc_offset_seconds":7200,"hourly":{"time":[],"temperature_2m_previous_day1":[]}}"#;
        assert!(parse_series(local, v).is_err(), "local times");
        let fahrenheit = r#"{"utc_offset_seconds":0,"hourly_units":{"temperature_2m_previous_day1":"°F"},"hourly":{"time":["2026-07-01T00:00"],"temperature_2m_previous_day1":[60.0]}}"#.as_bytes();
        assert!(parse_series(fahrenheit, v).is_err(), "unit");
        let short = br#"{"utc_offset_seconds":0,"hourly":{"time":["2026-07-01T00:00","2026-07-01T01:00"],"temperature_2m_previous_day1":[1.0]}}"#;
        assert!(parse_series(short, v).is_err(), "length mismatch");
        let missing = br#"{"utc_offset_seconds":0,"hourly":{"time":["2026-07-01T00:00"],"temperature_2m":[1.0]}}"#;
        assert!(parse_series(missing, v).is_err(), "wrong variable");
        let absurd = br#"{"utc_offset_seconds":0,"hourly":{"time":["2026-07-01T00:00"],"temperature_2m_previous_day1":[999.0]}}"#;
        assert_eq!(
            parse_series(absurd, v).unwrap()[0].1,
            None,
            "implausible = missing"
        );
    }

    #[test]
    fn api_reasons_and_allowed_ranges() {
        let r = api_reason(
            r#"{"error":true,"reason":"Parameter 'start_date' is out of allowed range from 2021-03-23 to 2026-10-12"}"#,
        );
        assert!(r.starts_with("Parameter 'start_date'"));
        assert_eq!(allowed_from(&r), NaiveDate::from_ymd_opt(2021, 3, 23));
        assert_eq!(
            allowed_from("Cannot initialize WeatherModel from invalid String value"),
            None
        );
        assert_eq!(api_reason("plain text"), "plain text");
    }

    #[test]
    fn variable_names() {
        assert_eq!(
            OpenMeteoPreviousRuns::variable(1),
            "temperature_2m_previous_day1"
        );
        assert_eq!(OpenMeteoPreviousRuns::variable(0), "temperature_2m");
    }
}
