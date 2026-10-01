//! Carrying the paper book across a restart.
//!
//! A restart begins a new run. On its own the run would start flat: its
//! positions, the strategy that opened each of them, the exposure caps and
//! the daily limits would forget the runs before it. The application loads
//! what those runs left in the database into a [`RestoreState`] and hands it
//! to [`crate::Engine::restore`] before the first live event.

use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::DailyTemperatureMarket;
use wm_core::portfolio::InstrumentRef;
use wm_core::trading::Fill;
use wm_core::units::{Shares, Usd};

/// A fill of an earlier run with what its position needs: the instrument
/// and the strategy whose order it filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoredFill {
    pub fill: Fill,
    pub instrument: InstrumentRef,
    pub strategy: StrategyId,
}

/// The paper book of earlier runs, as stored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RestoreState {
    /// Markets settling today (UTC) or later that hold fills.
    pub markets: Vec<DailyTemperatureMarket>,
    /// Their fills, oldest first.
    pub fills: Vec<RestoredFill>,
    /// Cost at the limit of the opening orders approved today (UTC).
    pub new_exposure_today: Usd,
}

impl RestoreState {
    pub fn is_empty(&self) -> bool {
        self.fills.is_empty() && self.new_exposure_today.is_zero()
    }
}

/// What a restore rebuilt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub markets: usize,
    pub fills: usize,
    /// Open positions once the fills are applied (a finished market settles
    /// at the next step).
    pub open_positions: usize,
    pub open_shares: Shares,
    pub open_cost: Usd,
    /// Realized P&L of the restored sells made today (UTC).
    pub realized_today: Usd,
    pub new_exposure_today: Usd,
    /// Fills the position book refused, e.g. a sell larger than the holding.
    pub rejected: Vec<String>,
}
