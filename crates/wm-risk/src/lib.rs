//! # wm-risk
//!
//! Pre-trade risk engine. Every intent from every strategy passes through
//! [`RiskEngine::evaluate`]; only approved intents ([`ApprovedIntent`], which
//! this crate alone can construct) can reach execution. Fail closed.

pub mod engine;
pub mod exposure;

pub use engine::{
    ApprovedIntent, CheckId, ExposureSummary, OpenOrderView, PortfolioView, RiskConfig, RiskConfigError, RiskDecision, RiskEngine,
    RiskInputs, RiskRejection, WeatherStatus,
};
pub use exposure::{EventExposure, Leg, event_exposure, scenario_pnl};
