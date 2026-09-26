//! # wm-backtest
//!
//! Replay and backtesting with the *same* kernel as live trading, historical
//! observation import, the first strategy experiment (peak survival), model
//! training with live feature code, walk-forward splits and synthetic data
//! for demos/tests.

pub mod historical;
pub mod replay;
pub mod research;
pub mod synthetic;

pub use historical::{ImportStats, import_iem_csv};
pub use replay::{
    BacktestConfig, BacktestReport, Fidelity, SessionOutput, Settlement, SimulationSession,
    TradeRecord, bootstrap_mean_ci, run_backtest,
};
pub use research::{
    StudyConfig, SurvivalReport, SurvivalRow, date_range, study, walk_forward_splits, wilson,
};
pub use synthetic::{SyntheticDay, synthetic_history, synthetic_trading_day};
