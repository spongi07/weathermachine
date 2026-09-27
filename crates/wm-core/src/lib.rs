//! # wm-core
//!
//! The Weather Machine domain model. This crate is pure (no I/O, no async
//! runtime) and is shared by every other crate, including the backtester, so
//! that live, paper and backtest runs speak exactly the same language.

pub mod event;
pub mod forecast;
pub mod hash;
pub mod health;
pub mod ids;
pub mod ingest;
pub mod market;
pub mod portfolio;
pub mod resolution;
pub mod rng;
pub mod synthetic;
pub mod time;
pub mod trading;
pub mod units;
pub mod weather;

pub use event::{EventEnvelope, EventSource, WeatherMachineEvent};
pub use ids::*;
pub use units::{Price, Probability, Rounding, Shares, TempC, Usd};

/// Crate version, embedded in User-Agent strings and build info.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
