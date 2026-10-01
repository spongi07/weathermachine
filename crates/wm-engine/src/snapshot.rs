//! Read-only snapshot of engine state for dashboards and APIs.

use crate::engine::{EngineStats, StationHint};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use wm_core::health::ProviderHealthSnapshot;
use wm_core::ids::{LocationId, RunId, StationId};
use wm_core::market::{DailyTemperatureMarket, OrderBook};
use wm_core::portfolio::Position;
use wm_core::trading::{DecisionRecord, RunMode};
use wm_core::units::Usd;
use wm_execution::OrderRecord;
use wm_risk::{ExposureSummary, RiskConfig};
use wm_strategy::{BucketEvaluation, DayState, IncrementDistribution, ObsPoint, PeakFeatures};

/// One resolution view of a location's current day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewSnapshot {
    pub label: String,
    pub state: Option<DayState>,
    pub features: Option<PeakFeatures>,
    pub windows_met: Vec<u32>,
    pub distribution: Option<IncrementDistribution>,
}

/// The location's latest fixed-lead forecast and whether the model uses it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastSnapshot {
    /// Product label, e.g. `open_meteo/gfs_global/d1`.
    pub product: String,
    /// Knowledge time of the series (when it reached the engine).
    pub received_at: DateTime<Utc>,
    /// `true` when the model conditions on this product and today's series
    /// passed the knowledge rule and the coverage check.
    pub in_use: bool,
    /// Why the forecast is (not) in use, in words.
    pub status: String,
    pub day_max_tenths: Option<i32>,
    pub remaining_max_tenths: Option<i32>,
    pub rise_tenths: Option<i32>,
    /// Today's hourly values (valid time, tenths °C) for charts.
    pub hourly: Vec<(DateTime<Utc>, i32)>,
}

/// Everything about one location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocationSnapshot {
    pub location: LocationId,
    pub station: StationId,
    pub timezone: String,
    pub local_date: NaiveDate,
    /// All observations of the local day (unfiltered), for charts.
    pub series: Vec<ObsPoint>,
    pub views: Vec<ViewSnapshot>,
    pub market: Option<DailyTemperatureMarket>,
    pub books: Vec<OrderBook>,
    pub evaluations: Vec<BucketEvaluation>,
    pub hint: StationHint,
    #[serde(default)]
    pub forecast: Option<ForecastSnapshot>,
    /// The station's latest ten-minute reading (KNMI), if one arrived.
    #[serde(default)]
    pub nowcast: Option<wm_core::weather::TenMinuteObservation>,
}

/// Full engine snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub now: DateTime<Utc>,
    pub mode: RunMode,
    pub run_id: RunId,
    pub model_id: String,
    pub kill_switch: Option<String>,
    pub storage_ok: bool,
    pub execution_ok: bool,
    pub stats: EngineStats,
    pub locations: Vec<LocationSnapshot>,
    pub health: Vec<ProviderHealthSnapshot>,
    pub positions: Vec<Position>,
    pub orders: Vec<OrderRecord>,
    pub exposure: ExposureSummary,
    pub risk: RiskConfig,
    pub daily_new_exposure: Usd,
    pub daily_realized_pnl: Usd,
    pub realized_pnl_total: Usd,
    pub decisions: Vec<DecisionRecord>,
}
