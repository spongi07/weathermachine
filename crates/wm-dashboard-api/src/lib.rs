//! # wm-dashboard-api
//!
//! The typed contract between the Weather Machine server and its Rust/WASM
//! dashboard. Depends only on `serde` so it compiles for `wasm32`.
//! Timestamps are Unix milliseconds; money and prices are display floats
//! (all *decisions* use exact fixed-point types server-side).

use serde::{Deserialize, Serialize};

/// API version (bump on breaking changes).
pub const API_VERSION: u32 = 1;

/// Peak-confirmation windows (minutes of observed data after the last touch
/// of the high) evaluated by the kernel; mirrored from `wm-strategy` and kept
/// in sync by a server-side test.
pub const CONFIRMATION_WINDOWS: [u32; 9] = [30, 45, 60, 75, 90, 105, 120, 150, 180];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub api_version: u32,
    pub generated_at_ms: i64,
    /// Engine (knowledge) time — differs from wall time in demo/replay.
    pub engine_time_ms: i64,
    pub mode: String,
    /// Synthetic data: the UI shows a prominent banner.
    pub demo: bool,
    pub version: String,
    pub instance: String,
    pub run_id: String,
    pub model_id: String,
    /// Probability model status (no model ⇒ no weather trades).
    #[serde(default)]
    pub model: ModelDto,
    pub kill_switch: Option<String>,
    pub storage_ok: bool,
    pub execution_ok: bool,
    pub live_trading_enabled: bool,
    pub engine: EngineDto,
    pub locations: Vec<LocationDto>,
    pub providers: Vec<ProviderDto>,
    pub market_stream: Option<StreamDto>,
    pub risk: RiskDto,
    pub positions: Vec<PositionDto>,
    pub orders: Vec<OrderDto>,
    pub decisions: Vec<DecisionDto>,
    pub alerts: Vec<AlertDto>,
    pub break_even: Vec<BreakEvenDto>,
}

/// Probability model status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelDto {
    /// `loaded` | `training` | `missing` | `failed` | `invalid` | `disabled`.
    pub state: String,
    pub detail: String,
    /// Training progress in station-years: (done, total).
    pub progress: Option<(u32, u32)>,
    /// The loaded model's forecast evaluation verdict (`None`: forecasts off).
    #[serde(default)]
    pub forecast: Option<String>,
    /// Background retraining in progress; the loaded model keeps trading.
    #[serde(default)]
    pub retraining: Option<String>,
}

impl ModelDto {
    pub fn loaded(&self) -> bool {
        self.state == "loaded"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineDto {
    pub events_total: u64,
    pub evaluations_total: u64,
    pub proposals_total: u64,
    pub approvals_total: u64,
    pub rejections_total: u64,
    pub fills_total: u64,
    pub last_handle_micros: u64,
    pub max_handle_micros: u64,
    pub last_seq: u64,
    pub events_by_kind: Vec<(String, u64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PointDto {
    pub t_ms: i64,
    pub local: String,
    pub temp_c: f64,
    pub speci: bool,
    /// Counted by the primary resolution view.
    pub eligible: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewDto {
    pub label: String,
    pub observations: u32,
    pub high_c: Option<f64>,
    pub high_whole: Option<i32>,
    pub high_local: Option<String>,
    pub retests: u32,
    pub minutes_since_high: Option<i64>,
    pub drop_c: Option<f64>,
    pub lower_since_high: u32,
    pub slope_c_per_h: Option<f64>,
    pub accel: Option<f64>,
    pub trajectory: Option<String>,
    pub minutes_after_solar_noon: Option<i32>,
    pub season: Option<String>,
    pub windows_met: Vec<u32>,
    /// P(final = high + k), last entry = tail.
    pub distribution: Option<Vec<f64>>,
    pub model_support: Option<u32>,
    pub model_source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PollDto {
    pub at_ms: i64,
    pub mode: String,
    pub reason: String,
    pub expected_report_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectorDto {
    pub active_provider: Option<String>,
    pub polls_total: u64,
    pub gate_closed_total: u64,
    pub new_observations_total: u64,
    pub duplicates_total: u64,
    pub corrections_total: u64,
    pub out_of_order_total: u64,
    pub persist_failures_total: u64,
    pub storage_ok: bool,
    pub next_poll: Option<PollDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservationRowDto {
    pub t_ms: i64,
    pub local: String,
    pub temp_c: Option<f64>,
    pub report_type: String,
    pub version: u32,
    pub provider: String,
    pub raw: String,
    pub knowledge_delay_s: i64,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LadderRowDto {
    pub label: String,
    pub lower: Option<i32>,
    pub upper: Option<i32>,
    pub yes_bid: Option<f64>,
    pub yes_ask: Option<f64>,
    pub no_bid: Option<f64>,
    pub no_ask: Option<f64>,
    pub yes_spread: Option<f64>,
    pub yes_ask_depth_usd: Option<f64>,
    pub implied_p: Option<f64>,
    pub model_p: Option<f64>,
    /// P(YES) the strategies use: the model pooled with a reliable market
    /// midpoint, never above the model.
    #[serde(default)]
    pub used_p: Option<f64>,
    pub edge: Option<f64>,
    /// EVs after fee and slippage allowance, at the pooled probabilities.
    pub yes_ev: Option<f64>,
    pub no_ev: Option<f64>,
    pub yes_break_even: Option<f64>,
    pub signals: Vec<String>,
    pub blockers: Vec<String>,
    pub position_shares: f64,
    pub contains_high: bool,
    pub book_age_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketDto {
    pub event_slug: String,
    pub title: String,
    pub local_date: String,
    pub resolution_source: String,
    pub resolution_url: Option<String>,
    pub rules_sha256: String,
    pub rules_excerpt: String,
    pub filters: Vec<String>,
    pub filter_confirmed: bool,
    pub machine_tradable: bool,
    pub review_status: String,
    pub unrecognized_clauses: Vec<String>,
    pub taker_fee_rate: f64,
    pub neg_risk: bool,
    pub end_time_ms: Option<i64>,
    pub rows: Vec<LadderRowDto>,
}

/// The location's day-1 forecast (predictive input; never resolution data).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastDto {
    /// e.g. `open_meteo/gfs_global/d1`.
    pub product: String,
    pub received_ms: i64,
    /// The model conditions on it (evaluation adopted it, knowledge rule met).
    pub in_use: bool,
    /// Why it is (not) in use.
    pub status: String,
    pub day_max_c: Option<f64>,
    pub remaining_max_c: Option<f64>,
    /// Remaining-day maximum minus the maximum so far, per the forecast.
    pub rise_c: Option<f64>,
    /// Today's hourly values for the chart.
    pub hourly: Vec<ForecastPointDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastPointDto {
    pub t_ms: i64,
    /// Minutes since local midnight (0 ..= 1440; 1380/1500 on DST days).
    pub minute: i32,
    pub temp_c: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocationDto {
    pub location: String,
    pub station: String,
    pub timezone: String,
    pub local_date: String,
    pub local_time: String,
    pub current_temp_c: Option<f64>,
    pub last_observation_ms: Option<i64>,
    pub last_observation_age_s: Option<i64>,
    pub last_raw: Option<String>,
    pub peak_watch: bool,
    pub has_exposure: bool,
    pub views: Vec<ViewDto>,
    pub series: Vec<PointDto>,
    pub observations: Vec<ObservationRowDto>,
    pub collector: Option<CollectorDto>,
    pub market: Option<MarketDto>,
    #[serde(default)]
    pub forecast: Option<ForecastDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderDto {
    pub provider: String,
    pub scope: Option<String>,
    pub state: String,
    pub reason: String,
    pub last_success_ms: Option<i64>,
    pub last_new_observation_ms: Option<i64>,
    pub last_observation_ms: Option<i64>,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub backoff_s: f64,
    pub blocked_until_ms: Option<i64>,
    pub latency_ms_ewma: Option<f64>,
    pub latency_ms_last: Option<u64>,
    pub throttle_events: u64,
    pub requests_total: u64,
    pub requests_today: u32,
    pub daily_budget: Option<u32>,
    pub circuit: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamDto {
    pub connected: bool,
    pub subscribed_assets: usize,
    pub messages_total: u64,
    pub reconnects_total: u64,
    pub last_message_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckDto {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskDto {
    pub position_size_usd: f64,
    pub global_worst_case_usd: f64,
    pub global_limit_usd: f64,
    pub capital_deployed_usd: f64,
    pub daily_new_exposure_usd: f64,
    pub daily_new_limit_usd: Option<f64>,
    pub daily_realized_pnl_usd: f64,
    pub daily_loss_limit_usd: Option<f64>,
    pub realized_pnl_total_usd: f64,
    pub max_price: f64,
    pub max_spread: f64,
    pub max_weather_age_min: i64,
    pub per_event: Vec<(String, f64)>,
    pub checks: Vec<CheckDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionDto {
    pub event_slug: String,
    pub bucket: String,
    pub side: String,
    pub shares: f64,
    pub cost_usd: f64,
    pub avg_cost: f64,
    pub mark: Option<f64>,
    pub unrealized_usd: Option<f64>,
    pub realized_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderDto {
    pub client_order_id: String,
    pub strategy: String,
    pub bucket: String,
    pub outcome: String,
    pub side: String,
    pub limit: f64,
    pub shares: f64,
    pub filled: f64,
    pub avg_price: Option<f64>,
    pub fees_usd: f64,
    pub status: String,
    pub reason: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionDto {
    pub id: u64,
    pub at_ms: i64,
    pub strategy: String,
    pub summary: String,
    pub approved: bool,
    pub reasons: Vec<String>,
    /// Routine evaluations: one line per strategy and bucket with the price,
    /// the probability used, the EV and what blocked it.
    #[serde(default)]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertDto {
    pub at_ms: i64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BreakEvenDto {
    pub price: f64,
    pub fee_per_share: f64,
    pub break_even_probability: f64,
    pub wins_to_recover_one_loss: f64,
}

/// Operator command payloads (POST /api/v1/kill-switch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillSwitchRequest {
    pub engaged: bool,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrips_as_json() {
        let s = DashboardSnapshot {
            api_version: API_VERSION,
            mode: "paper".into(),
            demo: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: DashboardSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
