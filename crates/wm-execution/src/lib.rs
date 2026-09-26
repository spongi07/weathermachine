//! # wm-execution
//!
//! Order lifecycle, fill models and the simulated exchange used identically
//! by backtests and paper trading. The live venue is a disabled stub.

pub mod fill_model;
pub mod orders;
pub mod simulated;
pub mod venue;

pub use fill_model::{TakerFill, passive_fill_from_trade, simulate_taker};
pub use orders::{Applied, OrderError, OrderManager, OrderRecord};
pub use simulated::{SimConfig, SimulatedExchange};
pub use venue::{DisabledLiveVenue, ExecutionVenue, VenueAck, VenueError, VenueOrder};
