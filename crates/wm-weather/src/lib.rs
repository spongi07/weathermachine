//! # wm-weather
//!
//! Weather observation ingestion: METAR parsing, official NOAA/NWS providers,
//! normalization, deduplication/correction tracking, provider health, adaptive
//! schedule-aware polling, the single-per-station collector, the separate
//! forecast port (predictive inputs only, never resolution data) with the
//! Open-Meteo fixed-lead client, KNMI's ten-minute station readings (also
//! predictive only), and the IEM archive client used to train the
//! probability model.

pub mod collector;
pub mod forecast;
pub mod health;
pub mod history;
pub mod knmi;
pub mod ledger;
pub mod metar;
pub mod open_meteo;
pub mod polling;
pub mod providers;
pub mod registry;
pub mod source;

pub use collector::{CollectorConfig, CollectorStatus, PollOutcome, StationCollector};
pub use forecast::{ForecastError, ForecastProvider, ForecastQuery, StaticForecastProvider};
pub use health::{HealthConfig, HealthTracker};
pub use history::{HistoryError, HistoryYear, IemArchive};
pub use knmi::{KnmiError, KnmiTenMinute};
pub use ledger::{Classified, ObservationLedger};
pub use open_meteo::{ForecastSeries, OpenMeteoError, OpenMeteoPreviousRuns};
pub use polling::{
    CadenceModel, PollDecision, PollingHints, PollingMode, PollingParams, PollingPolicy,
};
pub use providers::{AwcMetarSource, NwsApiSource, TgftpMetarSource};
pub use registry::{AlreadyRunning, CollectorClaim, CollectorRegistry};
pub use source::{ObservationSource, ParsedReport, SourceError, SourceFetch};
