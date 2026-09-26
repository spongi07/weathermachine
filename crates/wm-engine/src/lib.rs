//! # wm-engine
//!
//! The deterministic Weather Machine kernel. Backtest, paper and live runs
//! all drive exactly this type with [`EventEnvelope`]s:
//!
//! ```text
//! event → state (temperature / markets / books / orders / health)
//!       → per-view peak assessment + probability model
//!       → strategies A/B/C + unwind → proposals
//!       → risk engine → approved intents (→ execution)
//!       → decision audit records, polling hints, dashboard snapshot
//! ```
//!
//! The engine never reads a clock: time is the envelope's knowledge time.

mod engine;
mod snapshot;

pub use engine::{Engine, EngineConfig, EngineLocation, EngineOutput, EngineStats, StationHint};
pub use snapshot::{EngineSnapshot, LocationSnapshot, ViewSnapshot};
