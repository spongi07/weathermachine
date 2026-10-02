//! # wm-engine
//!
//! The deterministic Weather Machine kernel. Backtest, paper and live runs
//! all drive exactly this type with [`EventEnvelope`]s:
//!
//! ```text
//! event → state (temperature / markets / books / orders / health)
//!       → per-view peak assessment + probability model
//!       → strategies A–K, the lab's L1–L25 + unwind → proposals
//!       → risk engine → approved intents (→ execution)
//!       → decision audit records, polling hints, dashboard snapshot
//! ```
//!
//! The engine never reads a clock: time is the envelope's knowledge time.

mod engine;
mod restore;
mod snapshot;

pub use engine::{
    Engine, EngineConfig, EngineLocation, EngineOutput, EngineStats, NeighbourStation, StationHint,
    default_rejection_dedup_secs, lab_risk_config,
};
pub use restore::{RestoreState, RestoreSummary, RestoredFill};
pub use snapshot::{
    EngineSnapshot, ForecastSnapshot, LabBookSnapshot, LabInputsSnapshot, LocationSnapshot,
    NeighbourSnapshot, ViewSnapshot,
};
