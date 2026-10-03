//! Normalized weather observations and their deterministic identity.

use crate::ids::{ProviderId, StationId};
use crate::units::TempC;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// METAR (routine) or SPECI (special, unscheduled) report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ReportType {
    Metar,
    Speci,
}

impl ReportType {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportType::Metar => "METAR",
            ReportType::Speci => "SPECI",
        }
    }
}

impl fmt::Display for ReportType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Precision of the reported temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempPrecision {
    /// Whole degrees (standard METAR temperature group, e.g. `18/12`).
    WholeDegree,
    /// Tenths of a degree (US METAR `T` remark group, KNMI 10-minute data).
    Tenth,
}

/// Provider-independent identity of an observation.
///
/// Two providers relaying the same METAR produce the same key. A correction
/// (`COR`) keeps the key and increments the stored version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObservationKey {
    pub station: StationId,
    pub observed_at: DateTime<Utc>,
    pub report_type: ReportType,
}

impl ObservationKey {
    /// Stable textual fingerprint, e.g. `EHAM|2026-09-26T12:55:00Z|METAR`.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}",
            self.station,
            self.observed_at.format("%Y-%m-%dT%H:%M:%SZ"),
            self.report_type
        )
    }
}

/// Data-quality flags attached to an observation. Flags never delete data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityFlags {
    /// No usable temperature group (e.g. `/////` or NIL report).
    pub temperature_missing: bool,
    /// The provider's decoded temperature disagrees with our parse of the raw text.
    pub decoded_mismatch: bool,
    /// NIL report.
    pub nil_report: bool,
    /// Report carries the `AUTO` modifier.
    pub auto: bool,
    /// Report carries the `COR` modifier.
    pub correction_marker: bool,
    /// Observation time lies in the future relative to fetch time (clock issue upstream).
    pub future_timestamp: bool,
    /// Parsed from a secondary provider because the primary was unavailable.
    pub from_failover: bool,
}

/// A normalized observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub key: ObservationKey,
    /// 1 for the first version seen; incremented by corrections/revisions.
    pub version: u32,
    pub temperature: Option<TempC>,
    pub dewpoint: Option<TempC>,
    pub precision: TempPrecision,
    /// Raw report text exactly as received (after transport normalization for hashing only).
    pub raw_text: String,
    /// SHA-256 of the normalized raw text.
    pub content_hash: String,
    pub provider: ProviderId,
    /// Provider-side receipt time if published (AWC `receiptTime`), used to learn
    /// publication latency.
    pub provider_receipt_at: Option<DateTime<Utc>>,
    /// When Weather Machine's request that delivered this observation completed.
    /// This is the knowledge time (`available_at`) for live data.
    pub fetched_at: DateTime<Utc>,
    pub parser_version: u16,
    pub quality: QualityFlags,
}

impl Observation {
    pub fn station(&self) -> &StationId {
        &self.key.station
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.key.observed_at
    }

    /// Delay between observation time and our first knowledge of it.
    pub fn knowledge_delay_secs(&self) -> i64 {
        (self.fetched_at - self.key.observed_at).num_seconds()
    }
}

/// Result of deduplicating an incoming observation against everything seen before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DedupClass {
    /// First time this key is seen and it is the newest observation for the station.
    New,
    /// First time this key is seen but it is older than the newest known observation.
    OutOfOrder,
    /// Same key and identical content: no event is emitted.
    Duplicate,
    /// Same key, different content, report carries `COR`: stored as a new version.
    Correction,
    /// Same key, different content, no `COR` marker (unlabelled upstream revision).
    Revision,
}

impl DedupClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DedupClass::New => "new",
            DedupClass::OutOfOrder => "out_of_order",
            DedupClass::Duplicate => "duplicate",
            DedupClass::Correction => "correction",
            DedupClass::Revision => "revision",
        }
    }
}

/// A ten-minute reading of a station's automatic weather station from a
/// faster source than its METAR — KNMI's 10-minute in-situ observations at
/// Schiphol (WMO 06240). A predictive input only: it never changes the
/// observed high, the resolution views or settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenMinuteObservation {
    pub station: StationId,
    pub provider: ProviderId,
    /// End of the ten-minute interval (UTC): KNMI stamps its data so.
    pub interval_end: DateTime<Utc>,
    /// Mean 1.5 m air temperature over the interval.
    pub mean: Option<TempC>,
    /// Highest 1.5 m air temperature in the interval.
    pub max: Option<TempC>,
    /// Global radiation over the interval (W/m², KNMI `qg`), when it was
    /// requested (the strategy lab's L25 reads it); `None` otherwise.
    #[serde(default)]
    pub radiation: Option<i32>,
    /// When the bot received it.
    pub received_at: DateTime<Utc>,
}

impl TenMinuteObservation {
    /// Minutes between the end of the interval and its arrival.
    pub fn delay_minutes(&self) -> f64 {
        (self.received_at - self.interval_end).num_seconds() as f64 / 60.0
    }
}
