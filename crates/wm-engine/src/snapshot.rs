//! Read-only snapshot of engine state for dashboards and APIs.

use crate::engine::{EngineStats, StationHint};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wm_core::health::ProviderHealthSnapshot;
use wm_core::ids::{LocationId, RunId, StationId, StrategyId, TokenId};
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
    /// What the strategy lab reads (`None` with the lab off).
    #[serde(default)]
    pub lab: Option<LabInputsSnapshot>,
}

/// A neighbouring KNMI station's latest reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighbourSnapshot {
    pub name: String,
    pub bearing_deg: f64,
    pub interval_end: Option<DateTime<Utc>>,
    pub mean_tenths: Option<i32>,
}

/// The strategy lab's inputs at a location, for the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LabInputsSnapshot {
    /// KNMI readings of the station kept (the last 36 hours).
    pub knmi_readings: usize,
    /// The latest reading's global radiation (W/m²) and its share of the
    /// clear sky.
    pub radiation_wm2: Option<i32>,
    pub clear_sky_index: Option<f64>,
    pub neighbours: Vec<NeighbourSnapshot>,
    /// Today's METARs with their weather groups, and the latest in words.
    pub reports: usize,
    pub latest_weather: Option<String>,
    /// Today's day-1 forecast maximum (tenths), when usable.
    pub forecast_day_max_tenths: Option<i32>,
    pub yesterday_error_tenths: Option<i32>,
    /// Today's market's taker trades received.
    pub taker_trades: usize,
    /// Takers' records: settled days scored (`None`: not loaded yet), and
    /// how many count as skilled (t ≥ 2) and as losing.
    pub wallet_days: Option<u32>,
    pub wallets_skilled: usize,
    pub wallets_losing: usize,
}

/// One lab strategy's own paper book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabBookSnapshot {
    pub strategy: StrategyId,
    pub positions: Vec<Position>,
    pub exposure: ExposureSummary,
    pub daily_new_exposure: Usd,
    pub daily_realized_pnl: Usd,
    pub realized_pnl_total: Usd,
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
    /// The strategy whose order opened each of the main book's positions
    /// (restored ones included); a lab book's are its own strategy's.
    #[serde(default)]
    pub position_strategy: BTreeMap<TokenId, StrategyId>,
    /// The latest 100 orders of the main book, then the latest 100 of the
    /// lab's books, each newest first.
    pub orders: Vec<OrderRecord>,
    pub exposure: ExposureSummary,
    pub risk: RiskConfig,
    pub daily_new_exposure: Usd,
    pub daily_realized_pnl: Usd,
    pub realized_pnl_total: Usd,
    /// The latest 100 decisions of the main book (with the routine
    /// evaluations), then the latest 100 of the lab's books, newest first.
    pub decisions: Vec<DecisionRecord>,
    /// Each lab strategy's own book (the fields above are the main book's).
    #[serde(default)]
    pub lab_books: Vec<LabBookSnapshot>,
}
