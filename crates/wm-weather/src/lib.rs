//! # wm-weather
//!
//! Weather observation ingestion: METAR parsing, official NOAA/NWS providers,
//! normalization, deduplication/correction tracking, provider health, adaptive
//! schedule-aware polling and the single-per-station collector.

pub mod collector;
pub mod health;
pub mod ledger;
pub mod metar;
pub mod polling;
pub mod providers;
pub mod registry;
pub mod source;

pub use collector::{CollectorConfig, CollectorStatus, PollOutcome, StationCollector};
pub use health::{HealthConfig, HealthTracker};
pub use ledger::{Classified, ObservationLedger};
pub use polling::{CadenceModel, PollDecision, PollingHints, PollingMode, PollingParams, PollingPolicy};
pub use providers::{AwcMetarSource, NwsApiSource, TgftpMetarSource};
pub use registry::{AlreadyRunning, CollectorClaim, CollectorRegistry};
pub use source::{ObservationSource, ParsedReport, SourceError, SourceFetch};
