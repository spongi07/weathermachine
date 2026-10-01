//! # wm-strategy
//!
//! The pure decision core shared by backtest, paper and live runs:
//! temperature state, peak features, probability models, EV math,
//! strategies A–K and the unwind engine. No I/O, no clocks, no randomness.

pub mod book_confirmed;
pub mod certain;
pub mod ev;
pub mod forecast;
pub mod knmi_nowcast;
pub mod middle_fade;
pub mod morning_maker;
pub mod next_degree;
pub mod peak;
pub mod peak_slot;
pub mod peak_times;
pub mod probability;
pub mod quoting;
pub mod state;
pub mod strategy;
pub mod tail_seller;
pub mod unwind;

pub use book_confirmed::{BookConfirmedConfig, BookConfirmedHigh};
pub use certain::{CertainConfig, CertainOutcomes};
pub use forecast::ForecastDay;
pub use knmi_nowcast::{KnmiNowcast, KnmiNowcastConfig};
pub use middle_fade::{MiddleFade, MiddleFadeConfig};
pub use morning_maker::{MorningMaker, MorningMakerConfig};
pub use next_degree::{NextDegree, NextDegreeConfig};
pub use peak::{
    CONFIRMATION_WINDOWS, PeakAssessment, PeakConfig, PeakDetectionEngine, PeakFeatures,
    TrajectoryClass,
};
pub use peak_slot::{PeakSlotConfig, PeakSlotHigh, SeasonSlots};
pub use peak_times::{PeakTimes, PeakTimesBuilder, SeasonPeak};
pub use probability::{
    EmpiricalPeakModel, FeatureDim, ForecastModelInfo, IncrementDistribution, ModelStructure,
    NoEdgeModel, ProbabilityModel, StructureSelection,
};
pub use state::{DayState, HighInfo, ObsPoint, TemperatureStateEngine, ViewKind};
pub use strategy::{
    BucketEvaluation, BuyNoAboveHigh, BuyNoConfig, BuyYesConfig, BuyYesFinalHigh, Pooling,
    Proposal, SplitUnwind, SplitUnwindConfig, Strategy, StrategyContext, StrategyOutput,
    ViewEvaluation, log_pool,
};
pub use tail_seller::{TailSeller, TailSellerConfig};
pub use unwind::{UnwindConfig, UnwindEngine, UnwindStyle, default_exempt_strategies};
