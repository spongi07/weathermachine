//! Resolution-source model.
//!
//! Strict separation (blueprint §18):
//! * `ResolutionSource` — what the *market contract* settles on (per-market rules).
//! * `WeatherObservationSource` — what Weather Machine polls to learn the weather early.
//! * `ForecastSource` — predictive inputs; never a substitute for either of the above.

use crate::hash::sha256_hex;
use crate::market::{MarketExtreme, TempUnit};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Verbatim rules text of a market plus its content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesText {
    pub text: String,
    pub resolution_source_url: Option<String>,
    pub sha256: String,
}

impl RulesText {
    pub fn new(text: impl Into<String>, resolution_source_url: Option<String>) -> Self {
        let text = text.into();
        let mut material = text.clone();
        if let Some(u) = &resolution_source_url {
            material.push('\n');
            material.push_str(u);
        }
        let sha256 = sha256_hex(material.as_bytes());
        Self {
            text,
            resolution_source_url,
            sha256,
        }
    }
}

/// Where a market resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionSourceKind {
    /// weather.gov/wrh/timeseries?site=<site> — rendered from Synoptic Data API data.
    NoaaWrhTimeseries { site: String, url: String },
    /// Weather Underground daily history page.
    WundergroundDaily { url: String },
    /// KNMI official observations.
    Knmi { description: String },
    /// Anything the parser did not recognize — never tradable without review.
    Unrecognized { description: String },
}

impl ResolutionSourceKind {
    pub fn short_name(&self) -> &'static str {
        match self {
            ResolutionSourceKind::NoaaWrhTimeseries { .. } => "NOAA WRH timeseries",
            ResolutionSourceKind::WundergroundDaily { .. } => "Weather Underground",
            ResolutionSourceKind::Knmi { .. } => "KNMI",
            ResolutionSourceKind::Unrecognized { .. } => "unrecognized",
        }
    }
}

/// Which observations count towards the settlement value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservationFilter {
    /// Every row of the source table counts.
    AllRows,
    /// Only rows whose displayed minute lies in `[start, end]` (inclusive, wrapping
    /// past 59 when `start > end`). WRH "Show Hourly Data" is reported to keep
    /// minutes 51–59 for NWS/FAA platforms and 56–04 for other platforms.
    MinuteWindow { start: u8, end: u8 },
}

impl ObservationFilter {
    pub const WRH_HOURLY_NWS_FAA: ObservationFilter =
        ObservationFilter::MinuteWindow { start: 51, end: 59 };
    pub const WRH_HOURLY_OTHER: ObservationFilter =
        ObservationFilter::MinuteWindow { start: 56, end: 4 };

    pub fn admits_minute(&self, minute: u8) -> bool {
        match *self {
            ObservationFilter::AllRows => true,
            ObservationFilter::MinuteWindow { start, end } => {
                if start <= end {
                    (start..=end).contains(&minute)
                } else {
                    minute >= start || minute <= end
                }
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            ObservationFilter::AllRows => "all".to_owned(),
            ObservationFilter::MinuteWindow { start, end } => format!(":{start:02}-:{end:02}"),
        }
    }
}

/// Whether the filter has been verified against the live resolution page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterCertainty {
    /// Verified: evaluate a single view.
    Confirmed,
    /// Not verified: evaluate every candidate view and take the most conservative.
    Unconfirmed,
}

/// Until when upstream revisions count for settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionPolicy {
    /// "Revisions … considered until the first datapoint for the following date has been published".
    UntilFirstDatapointOfNextDay,
    UntilFinalized,
    Unknown,
}

/// Human review status of a parsed specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecReviewStatus {
    AutoParsed,
    Approved,
    Rejected,
}

/// Structured, versioned interpretation of a market's rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionSpec {
    pub source: ResolutionSourceKind,
    pub fallback: Option<ResolutionSourceKind>,
    pub extreme: MarketExtreme,
    pub unit: TempUnit,
    /// Settles on whole degrees as displayed by the source.
    pub whole_degrees: bool,
    /// Time zone that defines "the specified day" and the displayed minutes.
    pub day_timezone: Tz,
    /// Candidate observation filters (one when confirmed).
    pub filters: Vec<ObservationFilter>,
    pub filter_certainty: FilterCertainty,
    pub revision_policy: RevisionPolicy,
    pub rules_sha256: String,
    pub parser_version: u16,
    pub review: SpecReviewStatus,
    /// Sentences the parser could not map to a known clause.
    pub unrecognized_clauses: Vec<String>,
    pub notes: Vec<String>,
}

impl ResolutionSpec {
    /// A spec is tradable only if the source is recognized and nothing in the
    /// rules text was left unexplained (or a human approved it).
    pub fn is_machine_tradable(&self) -> bool {
        match self.review {
            SpecReviewStatus::Approved => true,
            SpecReviewStatus::Rejected => false,
            SpecReviewStatus::AutoParsed => {
                !matches!(self.source, ResolutionSourceKind::Unrecognized { .. })
                    && self.unrecognized_clauses.is_empty()
                    && !self.filters.is_empty()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_windows() {
        let nws = ObservationFilter::WRH_HOURLY_NWS_FAA;
        assert!(nws.admits_minute(55));
        assert!(nws.admits_minute(51));
        assert!(!nws.admits_minute(25));
        assert!(!nws.admits_minute(0));
        let other = ObservationFilter::WRH_HOURLY_OTHER;
        assert!(other.admits_minute(58) && other.admits_minute(0) && other.admits_minute(4));
        assert!(!other.admits_minute(55) && !other.admits_minute(5));
        assert!(ObservationFilter::AllRows.admits_minute(25));
        assert_eq!(nws.label(), ":51-:59");
    }

    #[test]
    fn rules_hash_changes_with_text_or_url() {
        let a = RulesText::new("rules", Some("https://a".into()));
        let b = RulesText::new("rules", Some("https://b".into()));
        let c = RulesText::new("rules!", Some("https://a".into()));
        assert_ne!(a.sha256, b.sha256);
        assert_ne!(a.sha256, c.sha256);
        assert_eq!(
            a.sha256,
            RulesText::new("rules", Some("https://a".into())).sha256
        );
    }
}
