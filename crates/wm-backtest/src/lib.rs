//! # wm-backtest
//!
//! Replay and backtesting with the *same* kernel as live trading, historical
//! observation import, the first strategy experiment (peak survival), model
//! training with live feature code, walk-forward splits, the model-versus-
//! market study on settled markets and synthetic data for demos/tests.

pub mod forecast_eval;
pub mod historical;
pub mod market_eval;
pub mod market_sim;
pub mod replay;
pub mod research;
pub mod selection;
pub mod synthetic;

pub use forecast_eval::{
    CalibrationRow, EvaluationConfig, ForecastEvaluation, ForecastHistory, PLACEBO_OFFSET_DAYS,
    ProxyRow, RiseRow,
};
pub use historical::{ImportStats, import_iem_csv};
pub use market_eval::{MarketDay, MarketStudyConfig, MarketStudyReport, MarketTrade, market_study};
pub use market_sim::{DayTimeline, MarketSimConfig, SimTrade, StrategyRow, TimelineRow};
pub use replay::{
    BacktestConfig, BacktestReport, Fidelity, SessionOutput, Settlement, SimulationSession,
    TradeRecord, bootstrap_mean_ci, run_backtest,
};
pub use research::{
    CandidateStudy, StudyConfig, StudyOutput, SurvivalReport, SurvivalRow, date_range, study,
    study_and_select, study_with_forecasts, walk_forward_splits, wilson,
};
pub use selection::{SelectionConfig, StructureComparison, StructureRow};
pub use synthetic::{SyntheticDay, synthetic_history, synthetic_trading_day};
