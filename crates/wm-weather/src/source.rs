//! The observation-source port and provider-independent normalization.

use crate::metar::{self, MetarReport, PARSER_VERSION};
use chrono::{DateTime, Utc};
use std::time::Duration;
use wm_core::hash::sha256_hex;
use wm_core::ids::{ProviderId, StationId};
use wm_core::ingest::{BoxFuture, CacheOutcome, ProviderRequestRecord, RawPayloadRecord};
use wm_core::weather::{Observation, ObservationKey, QualityFlags, ReportType, TempPrecision};
use wm_net::FetchError;

/// One report as delivered by a provider, before deduplication.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedReport {
    pub station: StationId,
    pub observed_at: DateTime<Utc>,
    pub report_type: ReportType,
    pub raw_text: String,
    pub metar: Option<MetarReport>,
    /// Temperature as decoded by the provider (tenths), for cross-checking.
    pub provider_temp_tenths: Option<i32>,
    pub provider_receipt_at: Option<DateTime<Utc>>,
}

/// Result of one successful poll.
#[derive(Debug, Clone)]
pub struct SourceFetch {
    pub reports: Vec<ParsedReport>,
    pub raw: RawPayloadRecord,
    pub request: ProviderRequestRecord,
    pub cache: CacheOutcome,
    /// Non-fatal problems (e.g. one unparseable entry among several).
    pub warnings: Vec<String>,
}

/// Failed poll.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("fetch failed: {0}")]
    Fetch(#[from] FetchError),
    /// HTTP succeeded but the payload could not be parsed. Raw data is kept.
    #[error("malformed response: {detail}")]
    Malformed { detail: String, raw: Box<RawPayloadRecord>, request: Box<ProviderRequestRecord> },
}

impl SourceError {
    pub fn request_record(&self) -> Option<&ProviderRequestRecord> {
        match self {
            SourceError::Fetch(e) => e.record(),
            SourceError::Malformed { request, .. } => Some(request),
        }
    }
}

/// A weather observation source (one provider). Implementations must route
/// every request through their provider's `wm_net::ProviderGate`.
pub trait ObservationSource: Send + Sync {
    fn provider(&self) -> &ProviderId;
    /// The rate-limit gate every request of this source goes through.
    fn gate(&self) -> &std::sync::Arc<wm_net::ProviderGate>;
    /// Fetch the latest reports for `station`, waiting at most `max_gate_wait`
    /// for the rate-limit gate.
    fn fetch<'a>(&'a self, station: &'a StationId, max_gate_wait: Duration) -> BoxFuture<'a, Result<SourceFetch, SourceError>>;
    /// Whether this source is backed by NOAA/NWS infrastructure.
    fn is_noaa(&self) -> bool {
        true
    }
}

/// Build the raw payload record for a response body.
pub fn raw_record(
    provider: &ProviderId,
    station: &StationId,
    endpoint: &str,
    fetched_at: DateTime<Utc>,
    status: u16,
    content_type: Option<String>,
    body: &[u8],
) -> RawPayloadRecord {
    RawPayloadRecord {
        provider: provider.clone(),
        station: Some(station.clone()),
        endpoint: endpoint.to_owned(),
        fetched_at,
        status,
        content_type,
        body: body.to_vec(),
        sha256: sha256_hex(body),
        parser_version: PARSER_VERSION,
    }
}

/// Convert a provider report into a normalized [`Observation`] (version 1).
/// Temperature comes from *our* parse of the raw text; the provider's decoded
/// value is only used to flag disagreements.
pub fn normalize(report: &ParsedReport, provider: &ProviderId, fetched_at: DateTime<Utc>, failover: bool) -> Observation {
    let canonical = metar::canonicalize(&report.raw_text);
    let (temperature, precision) = match report.metar.as_ref().and_then(MetarReport::temperature) {
        Some((t, p)) => (Some(t), p),
        None => (None, TempPrecision::WholeDegree),
    };
    let dewpoint = report.metar.as_ref().and_then(MetarReport::dewpoint);
    let decoded_mismatch = match (temperature, report.provider_temp_tenths) {
        (Some(ours), Some(theirs)) => (ours.tenths() - theirs).abs() >= 5,
        _ => false,
    };
    let quality = QualityFlags {
        temperature_missing: temperature.is_none(),
        decoded_mismatch,
        nil_report: report.metar.as_ref().is_some_and(|m| m.nil),
        auto: report.metar.as_ref().is_some_and(|m| m.auto),
        correction_marker: report.metar.as_ref().is_some_and(|m| m.cor),
        future_timestamp: report.observed_at > fetched_at + chrono::Duration::minutes(5),
        from_failover: failover,
    };
    Observation {
        key: ObservationKey {
            station: report.station.clone(),
            observed_at: report.observed_at,
            report_type: report.report_type,
        },
        version: 1,
        temperature,
        dewpoint,
        precision,
        raw_text: report.raw_text.trim().to_owned(),
        content_hash: sha256_hex(canonical.as_bytes()),
        provider: provider.clone(),
        provider_receipt_at: report.provider_receipt_at,
        fetched_at,
        parser_version: PARSER_VERSION,
        quality,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_prefers_raw_parse_and_flags_mismatch() {
        let raw = "METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG";
        let m = metar::parse_metar(raw).unwrap();
        let fetched = DateTime::parse_from_rfc3339("2026-09-26T12:58:00Z").unwrap().with_timezone(&Utc);
        let report = ParsedReport {
            station: StationId::new("EHAM").unwrap(),
            observed_at: m.observed_at(fetched).unwrap(),
            report_type: ReportType::Metar,
            raw_text: raw.to_owned(),
            metar: Some(m),
            provider_temp_tenths: Some(170),
            provider_receipt_at: None,
        };
        let obs = normalize(&report, &ProviderId::awc(), fetched, false);
        assert_eq!(obs.temperature.unwrap().tenths(), 180);
        assert!(obs.quality.decoded_mismatch);
        assert_eq!(obs.version, 1);
        assert_eq!(obs.knowledge_delay_secs(), 180);
        // Same report relayed without the METAR prefix hashes identically.
        let mut relay = report.clone();
        relay.raw_text = "EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG=".into();
        let obs2 = normalize(&relay, &ProviderId::tgftp(), fetched, true);
        assert_eq!(obs.content_hash, obs2.content_hash);
        assert!(obs2.quality.from_failover);
    }
}
