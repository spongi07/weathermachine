//! # wm-strategy
//!
//! The pure decision core shared by backtest, paper and live runs:
//! temperature state, peak features, probability models, EV math,
//! strategies A/B/C and the unwind engine. No I/O, no clocks, no randomness.

pub mod ev;
pub mod peak;
pub mod probability;
pub mod state;
pub mod strategy;
pub mod unwind;

pub use peak::{
    CONFIRMATION_WINDOWS, PeakAssessment, PeakConfig, PeakDetectionEngine, PeakFeatures,
    TrajectoryClass,
};
pub use probability::{EmpiricalPeakModel, IncrementDistribution, NoEdgeModel, ProbabilityModel};
pub use state::{DayState, HighInfo, ObsPoint, TemperatureStateEngine, ViewKind};
pub use strategy::{
    BucketEvaluation, BuyNoAboveHigh, BuyNoConfig, BuyYesConfig, BuyYesFinalHigh, Proposal,
    SplitUnwind, SplitUnwindConfig, Strategy, StrategyContext, StrategyOutput, ViewEvaluation,
};
pub use unwind::{UnwindConfig, UnwindEngine, UnwindStyle};
