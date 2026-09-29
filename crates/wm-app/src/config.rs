//! Application configuration: `configs/weather-machine.toml` + `configs/locations/*.toml`
//! + environment overrides for deployment-specific values (never compiled in).
//!
//! | Variable            | Meaning                                            |
//! |---------------------|----------------------------------------------------|
//! | `WM_CONFIG`         | path of the main TOML file                         |
//! | `WM_MODE`           | `paper` \| `backtest` \| `live` (live is refused)    |
//! | `WM_HTTP_BIND`      | dashboard/API bind address                         |
//! | `WM_DATABASE_URL`   | PostgreSQL URL (absent ⇒ no-db mode, trading off)  |
//! | `WM_DB_PASSWORD`    | alternative to the URL: password (+ optional       |
//! |                     | `WM_DB_HOST`/`_PORT`/`_USER`/`_NAME`), URL-encoded |
//! | `WM_CONTACT`        | contact for the NWS User-Agent (required)          |
//! | `WM_USER_AGENT`     | full User-Agent override                           |
//! | `WM_ADMIN_TOKEN`    | bearer token for operator endpoints (kill switch)  |
//! | `WM_UI_DIR`         | directory of the built dashboard assets            |
//! | `WM_MODEL_PATH`     | trained probability model JSON                     |
//! | `WM_MODEL_AUTO_TRAIN` | `false` disables training from IEM history       |
//! | `WM_DATA_DIR`       | writable data directory (models, history cache)    |
//! | `WM_JOURNAL_RETENTION_DAYS` | days of order-book updates kept in the journal |
//! | `WM_FORECAST`       | `false` disables the day-1 forecast (fetch + training) |
//! | `WM_FORECAST_MODEL` | Open-Meteo model id, e.g. `gfs_global`             |
//! | `WM_OPEN_METEO_API_KEY` | Open-Meteo subscription key (commercial host)  |
//! | `WM_LOG_FORMAT`     | `json` \| `pretty`                                   |

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use wm_core::market::TempUnit;
use wm_core::resolution::ObservationFilter;
use wm_core::trading::RunMode;
use wm_core::units::{Price, decimal_serde};
use wm_net::RateLimitPolicy;
use wm_risk::RiskConfig;
use wm_strategy::{BuyNoConfig, BuyYesConfig, SplitUnwindConfig, UnwindConfig};
use wm_weather::{HealthConfig, PollingParams};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub mode: RunMode,
    pub instance: String,
    pub http_bind: String,
    pub ui_dir: String,
    pub log_format: String,
    pub locations_dir: String,
    pub auto_migrate: bool,
    pub journal: bool,
    pub record_orderbooks: bool,
    pub heartbeat_secs: u64,
    pub snapshot_interval_ms: u64,
    /// Market history: a token's book is stored when it changed, at most once
    /// per this many seconds.
    #[serde(default = "default_book_record_interval_secs")]
    pub book_record_interval_secs: u64,
    /// Order-book updates in the replay journal are deleted after this many
    /// days (0 = keep). Other journal entries and the recorded market history
    /// are kept.
    #[serde(default = "default_journal_book_retention_days")]
    pub journal_book_retention_days: u32,
}

fn default_book_record_interval_secs() -> u64 {
    10
}

fn default_journal_book_retention_days() -> u32 {
    7
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentitySection {
    pub user_agent_template: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSection {
    pub max_connections: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSection {
    pub enabled: bool,
    pub base_url: String,
    pub policy: RateLimitPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersSection {
    pub awc: ProviderSection,
    pub tgftp: ProviderSection,
    pub nws_api: ProviderSection,
    pub polymarket_gamma: ProviderSection,
    pub polymarket_clob: ProviderSection,
    pub polymarket_ws: ProviderSection,
    /// IEM ASOS/METAR archive: history for model training only.
    #[serde(default = "default_iem_provider")]
    pub iem: ProviderSection,
    /// Open-Meteo Previous Runs API: fixed-lead forecasts (predictive input).
    #[serde(default = "default_open_meteo_provider")]
    pub open_meteo: ProviderSection,
    /// Polymarket Data API: public trade history for `research market`.
    #[serde(default = "default_polymarket_data_provider")]
    pub polymarket_data: ProviderSection,
}

/// IEM throttles each IP to one request per second; we space requests 15 s
/// apart, one at a time, with a small daily cap.
fn default_iem_provider() -> ProviderSection {
    ProviderSection {
        enabled: true,
        base_url: "https://mesonet.agron.iastate.edu".into(),
        policy: RateLimitPolicy {
            min_interval: std::time::Duration::from_secs(15),
            timeout: std::time::Duration::from_secs(180),
            connect_timeout: std::time::Duration::from_secs(20),
            backoff_base: std::time::Duration::from_secs(60),
            backoff_max: std::time::Duration::from_secs(3600),
            throttle_backoff_base: std::time::Duration::from_secs(600),
            circuit_failure_threshold: 3,
            circuit_open_base: std::time::Duration::from_secs(1800),
            circuit_open_max: std::time::Duration::from_secs(6 * 3600),
            daily_budget: Some(200),
            max_body_bytes: 16 * 1024 * 1024,
            ..RateLimitPolicy::public_data_conservative()
        },
    }
}

/// Open-Meteo's free tier allows 600 calls/min, 5,000/h and 10,000/day for
/// non-commercial use. We make one call per location per refresh (hourly)
/// and one per year of history when training — one at a time, 10 s apart.
fn default_open_meteo_provider() -> ProviderSection {
    ProviderSection {
        enabled: true,
        base_url: wm_weather::open_meteo::FREE_HOST.into(),
        policy: RateLimitPolicy {
            min_interval: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(60),
            connect_timeout: std::time::Duration::from_secs(10),
            backoff_base: std::time::Duration::from_secs(60),
            backoff_max: std::time::Duration::from_secs(3600),
            throttle_backoff_base: std::time::Duration::from_secs(900),
            circuit_failure_threshold: 3,
            circuit_open_base: std::time::Duration::from_secs(1800),
            circuit_open_max: std::time::Duration::from_secs(6 * 3600),
            daily_budget: Some(500),
            max_body_bytes: 4 * 1024 * 1024,
            ..RateLimitPolicy::public_data_conservative()
        },
    }
}

/// The Data API allows 200 requests per 10 s; research reads at most two
/// per second, one at a time.
fn default_polymarket_data_provider() -> ProviderSection {
    ProviderSection {
        enabled: true,
        base_url: wm_polymarket::DataApiClient::DEFAULT_BASE.into(),
        policy: RateLimitPolicy {
            min_interval: std::time::Duration::from_millis(500),
            max_concurrency: 1,
            timeout: std::time::Duration::from_secs(30),
            daily_budget: Some(20_000),
            ..RateLimitPolicy::polymarket_rest()
        },
    }
}

/// The day-1 forecast as a model feature. It influences trading only when
/// the out-of-sample evaluation at training adopts it; otherwise it is
/// fetched and shown, nothing more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForecastSection {
    pub enabled: bool,
    /// Open-Meteo model id (Previous Runs API). `gfs_global`: 2 m
    /// temperature archived since 2021, the longest history.
    pub model: String,
    /// Lead of every value in days (1 = forecast 24 h before valid time).
    pub lead_days: u8,
    /// A local day's series is used from this local minute on (knowledge
    /// rule, identical in training and live).
    pub ready_local_minute: u16,
    /// Live refresh interval.
    pub refresh_minutes: u64,
    /// Earliest forecast history requested for training.
    pub history_from: chrono::NaiveDate,
    /// The evaluation needs at least this many scored days to adopt it.
    pub min_eval_days: u64,
}

impl Default for ForecastSection {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "gfs_global".into(),
            lead_days: 1,
            ready_local_minute: 8 * 60,
            refresh_minutes: 60,
            history_from: chrono::NaiveDate::from_ymd_opt(2021, 3, 1).unwrap_or_default(),
            min_eval_days: 365,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategiesSection {
    pub buy_yes: BuyYesConfigToml,
    pub buy_no: BuyNoConfigToml,
    pub split_unwind: SplitUnwindConfigToml,
    /// Strategy D (outcomes the observations have decided).
    #[serde(default)]
    pub certain: CertainConfigToml,
    /// Strategy E (the high's bucket, confirmed by clock, temperature and a
    /// shrinking book).
    #[serde(default)]
    pub book_confirmed: BookConfirmedConfigToml,
    /// Strategy F (the high's bucket inside the season's peak slot). Off
    /// when the section is missing: its 100 shares need their own risk caps.
    #[serde(default = "PeakSlotConfigToml::absent")]
    pub peak_slot: PeakSlotConfigToml,
    pub unwind: UnwindConfig,
}

/// A local time window as TOML: `"15:00-18:00"` (end exclusive; `24:00`
/// allowed as the end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(pub u16, pub u16);

impl Window {
    fn parse(s: &str) -> Result<Self, String> {
        let minute = |t: &str| -> Result<u16, String> {
            let (h, m) = t
                .trim()
                .split_once(':')
                .ok_or_else(|| format!("'{t}' is not HH:MM"))?;
            let (h, m): (u16, u16) = (
                h.parse().map_err(|_| format!("bad hour in '{t}'"))?,
                m.parse().map_err(|_| format!("bad minute in '{t}'"))?,
            );
            if m > 59 || h > 24 || (h == 24 && m > 0) {
                return Err(format!("'{t}' is not a time of day"));
            }
            Ok(h * 60 + m)
        };
        let (a, b) = s
            .split_once('-')
            .ok_or_else(|| format!("'{s}' is not HH:MM-HH:MM"))?;
        let (start, end) = (minute(a)?, minute(b)?);
        if start >= end {
            return Err(format!("'{s}': the start must be before the end"));
        }
        Ok(Self(start, end))
    }
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}-{:02}:{:02}",
            self.0 / 60,
            self.0 % 60,
            self.1 / 60,
            self.1 % 60
        )
    }
}

impl Serialize for Window {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Window {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Window::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Strategy F's slots per season until the history's peak times are learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeasonSlotsToml {
    pub winter: Window,
    pub spring: Window,
    pub summer: Window,
    pub autumn: Window,
}

impl From<wm_strategy::SeasonSlots> for SeasonSlotsToml {
    fn from(s: wm_strategy::SeasonSlots) -> Self {
        let w = |(a, b): (u16, u16)| Window(a, b);
        Self {
            winter: w(s.winter),
            spring: w(s.spring),
            summer: w(s.summer),
            autumn: w(s.autumn),
        }
    }
}

impl From<SeasonSlotsToml> for wm_strategy::SeasonSlots {
    fn from(s: SeasonSlotsToml) -> Self {
        let w = |x: Window| (x.0, x.1);
        Self {
            winter: w(s.winter),
            spring: w(s.spring),
            summer: w(s.summer),
            autumn: w(s.autumn),
        }
    }
}

/// TOML mirror of [`wm_strategy::PeakSlotConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PeakSlotConfigToml {
    pub enabled: bool,
    pub slot_from_quantile: f64,
    pub slot_to_quantile: f64,
    pub fallback_slots: SeasonSlotsToml,
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    pub shares: u32,
    pub min_drop_tenths: i32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
}

impl PeakSlotConfigToml {
    /// A configuration file without `[strategies.peak_slot]`: F is off.
    fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

impl Default for PeakSlotConfigToml {
    fn default() -> Self {
        let d = wm_strategy::PeakSlotConfig::default();
        Self {
            enabled: d.enabled,
            slot_from_quantile: d.slot_from_quantile,
            slot_to_quantile: d.slot_to_quantile,
            fallback_slots: d.fallback_slots.into(),
            min_price: d.min_price,
            max_price: d.max_price,
            shares: u32::try_from(d.shares.micros() / 1_000_000).unwrap_or(100),
            min_drop_tenths: d.min_drop_tenths,
            max_data_age_minutes: d.max_data_age_minutes,
            max_book_age_ms: d.max_book_age_ms,
            slippage_allowance: d.slippage_allowance,
        }
    }
}

/// TOML mirror of [`wm_strategy::BookConfirmedConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BookConfirmedConfigToml {
    pub enabled: bool,
    pub start_local_minute: u16,
    pub end_local_minute: u16,
    pub min_minutes_at_high: i64,
    pub min_drop_tenths: i32,
    pub lookback_minutes: i64,
    pub min_depth_shrink: f64,
    pub min_depth_shares: f64,
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    pub min_model_p: f64,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
}

impl Default for BookConfirmedConfigToml {
    fn default() -> Self {
        let d = wm_strategy::BookConfirmedConfig::default();
        Self {
            enabled: d.enabled,
            start_local_minute: d.start_local_minute,
            end_local_minute: d.end_local_minute,
            min_minutes_at_high: d.min_minutes_at_high,
            min_drop_tenths: d.min_drop_tenths,
            lookback_minutes: d.lookback_minutes,
            min_depth_shrink: d.min_depth_shrink,
            min_depth_shares: d.min_depth_shares,
            min_price: d.min_price,
            max_price: d.max_price,
            min_model_p: d.min_model_p,
            max_data_age_minutes: d.max_data_age_minutes,
            max_book_age_ms: d.max_book_age_ms,
            slippage_allowance: d.slippage_allowance,
        }
    }
}

/// TOML mirror of [`wm_strategy::CertainConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CertainConfigToml {
    pub enabled: bool,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    pub min_edge: f64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    pub max_jump_tenths: i32,
}

impl Default for CertainConfigToml {
    fn default() -> Self {
        let d = wm_strategy::CertainConfig::default();
        Self {
            enabled: d.enabled,
            max_price: d.max_price,
            min_edge: d.min_edge,
            slippage_allowance: d.slippage_allowance,
            max_data_age_minutes: d.max_data_age_minutes,
            max_book_age_ms: d.max_book_age_ms,
            max_jump_tenths: d.max_jump_tenths,
        }
    }
}

/// TOML-friendly mirror of [`BuyYesConfig`] (decimal strings for prices/money).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuyYesConfigToml {
    pub enabled: bool,
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    pub min_confirmation_minutes: i64,
    pub min_edge: f64,
    pub min_model_support: u32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
    /// Weight of the market-implied probability (0 = model only, 1 = market only).
    #[serde(default = "wm_strategy::strategy::default_market_weight")]
    pub market_weight: f64,
    /// Widest spread at which a book's midpoint counts as a market probability.
    #[serde(
        with = "decimal_serde::price",
        default = "wm_strategy::strategy::default_max_market_spread"
    )]
    pub max_market_spread: Price,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuyNoConfigToml {
    pub enabled: bool,
    pub distances: Vec<i32>,
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    pub min_confirmation_minutes: i64,
    pub min_edge: f64,
    pub min_model_support: u32,
    pub max_data_age_minutes: i64,
    pub max_book_age_ms: i64,
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
    /// See [`BuyYesConfigToml::market_weight`].
    #[serde(default = "wm_strategy::strategy::default_market_weight")]
    pub market_weight: f64,
    #[serde(
        with = "decimal_serde::price",
        default = "wm_strategy::strategy::default_max_market_spread"
    )]
    pub max_market_spread: Price,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitUnwindConfigToml {
    pub enabled: bool,
    pub max_confirmation_minutes: i64,
    pub min_combined_p: f64,
    #[serde(with = "decimal_serde::price")]
    pub max_combined_price: Price,
    pub max_data_age_minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSection {
    /// Trained model JSON. Unset: `<auto_train.data_dir>/models/<station>.json`
    /// while auto-training is enabled, otherwise the no-edge model (no trades).
    pub path: Option<String>,
    #[serde(default)]
    pub auto_train: AutoTrainSection,
}

/// Train the model on the host from IEM history when no model file exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AutoTrainSection {
    pub enabled: bool,
    /// Writable directory: `models/` (the model) and `research/` (cached
    /// history per year and the survival report).
    pub data_dir: String,
    /// First year of history to download.
    pub from_year: i32,
    /// Refuse to install a model built from fewer usable days.
    pub min_days: u64,
    /// Wait before retrying after a failed attempt.
    pub retry_after_secs: u64,
    /// Retrain (in the background, the current model keeps trading) when the
    /// model is older than this many days, so new history — and new forecast
    /// history for the evaluation — is used. 0 = never.
    pub retrain_after_days: u64,
}

impl Default for AutoTrainSection {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: "/data".into(),
            from_year: 2005,
            min_days: 730,
            retry_after_secs: 6 * 3600,
            retrain_after_days: 30,
        }
    }
}

/// Main configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfigFile {
    pub app: AppSection,
    pub identity: IdentitySection,
    pub database: DatabaseSection,
    pub providers: ProvidersSection,
    pub polling: PollingParams,
    pub health: HealthConfig,
    pub risk: RiskConfig,
    pub strategies: StrategiesSection,
    pub model: ModelSection,
    #[serde(default)]
    pub forecast: ForecastSection,
}

/// Per-location file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationFile {
    pub location: LocationMeta,
    pub station: StationMeta,
    pub market: MarketMeta,
    pub observation_sources: SourcesMeta,
    pub peak: PeakMeta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocationMeta {
    pub id: String,
    pub name: String,
    pub timezone: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StationMeta {
    pub id: String,
    pub name: String,
    pub wmo_id: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    /// Minutes past the hour (UTC) of routine reports.
    pub routine_minutes: Vec<u8>,
    pub first_poll_delay_secs: i64,
    pub arrival_window_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketMeta {
    pub event_slug_template: String,
    pub unit: TempUnit,
    #[serde(with = "decimal_serde::price")]
    pub taker_fee_rate: Price,
    /// Set only after the Phase-0 verification of the resolution page.
    pub confirmed_filter: Option<ConfirmedFilter>,
    pub expected_resolution_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmedFilter {
    All,
    HourlyNwsFaa,
    HourlyOther,
}

impl ConfirmedFilter {
    pub fn filter(self) -> ObservationFilter {
        match self {
            ConfirmedFilter::All => ObservationFilter::AllRows,
            ConfirmedFilter::HourlyNwsFaa => ObservationFilter::WRH_HOURLY_NWS_FAA,
            ConfirmedFilter::HourlyOther => ObservationFilter::WRH_HOURLY_OTHER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcesMeta {
    pub primary: String,
    pub secondary: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeakMeta {
    pub watch_start_after_noon: i32,
    pub watch_end_after_noon: i32,
    pub watch_margin_tenths: i32,
}

/// Environment-supplied deployment settings.
#[derive(Clone, Default)]
pub struct EnvSettings {
    pub database_url: Option<String>,
    pub contact: Option<String>,
    pub user_agent: Option<String>,
    pub admin_token: Option<String>,
    /// Open-Meteo subscription key (never logged or stored).
    pub open_meteo_api_key: Option<String>,
}

/// Secrets (database URL with its password, tokens, keys) are never printed.
impl std::fmt::Debug for EnvSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |v: &Option<String>| v.as_ref().map(|_| "<set>");
        f.debug_struct("EnvSettings")
            .field("database_url", &set(&self.database_url))
            .field("contact", &self.contact)
            .field("user_agent", &self.user_agent)
            .field("admin_token", &set(&self.admin_token))
            .field("open_meteo_api_key", &set(&self.open_meteo_api_key))
            .finish()
    }
}

/// Fully loaded configuration.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub file: AppConfigFile,
    pub locations: Vec<LocationFile>,
    pub env: EnvSettings,
    pub source_path: PathBuf,
}

/// Percent-encode a URL component (RFC 3986 unreserved characters pass).
pub fn url_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Build the PostgreSQL URL from separate variables (container deployments),
/// so passwords with special characters need no manual URL encoding.
fn database_url_from_parts() -> Option<String> {
    let password = env_nonempty("WM_DB_PASSWORD")?;
    let user = env_nonempty("WM_DB_USER").unwrap_or_else(|| "wm".to_owned());
    let host = env_nonempty("WM_DB_HOST").unwrap_or_else(|| "postgres".to_owned());
    let port = env_nonempty("WM_DB_PORT").unwrap_or_else(|| "5432".to_owned());
    let name = env_nonempty("WM_DB_NAME").unwrap_or_else(|| "weather_machine".to_owned());
    Some(format!(
        "postgres://{}:{}@{host}:{port}/{}",
        url_component(&user),
        url_component(&password),
        url_component(&name)
    ))
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn parse_bool(name: &str, v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => bail!("{name} must be true or false, got '{other}'"),
    }
}

impl AppConfig {
    /// Load and validate from `path` (or `WM_CONFIG`), applying env overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(|| env_nonempty("WM_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("configs/weather-machine.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut file: AppConfigFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(m) = env_nonempty("WM_MODE") {
            file.app.mode = match m.as_str() {
                "paper" => RunMode::Paper,
                "backtest" => RunMode::Backtest,
                "live" => RunMode::Live,
                other => bail!("WM_MODE must be paper|backtest|live, got '{other}'"),
            };
        }
        if let Some(v) = env_nonempty("WM_HTTP_BIND") {
            file.app.http_bind = v;
        }
        if let Some(v) = env_nonempty("WM_UI_DIR") {
            file.app.ui_dir = v;
        }
        if let Some(v) = env_nonempty("WM_LOG_FORMAT") {
            file.app.log_format = v;
        }
        if let Some(v) = env_nonempty("WM_MODEL_PATH") {
            file.model.path = Some(v);
        }
        if let Some(v) = env_nonempty("WM_JOURNAL_RETENTION_DAYS") {
            file.app.journal_book_retention_days = v.parse().with_context(|| {
                format!("WM_JOURNAL_RETENTION_DAYS must be a whole number of days, got '{v}'")
            })?;
        }
        if let Some(v) = env_nonempty("WM_DATA_DIR") {
            file.model.auto_train.data_dir = v;
        }
        if let Some(v) = env_nonempty("WM_MODEL_AUTO_TRAIN") {
            file.model.auto_train.enabled = parse_bool("WM_MODEL_AUTO_TRAIN", &v)?;
        }
        if let Some(v) = env_nonempty("WM_FORECAST") {
            file.forecast.enabled = parse_bool("WM_FORECAST", &v)?;
        }
        if let Some(v) = env_nonempty("WM_FORECAST_MODEL") {
            file.forecast.model = v;
        }
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let loc_dir = {
            let p = PathBuf::from(&file.app.locations_dir);
            if p.is_absolute() || p.exists() {
                p
            } else {
                base.join(p.file_name().unwrap_or_default())
            }
        };
        let mut locations = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&loc_dir)
            .with_context(|| format!("reading locations dir {}", loc_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();
        for p in entries {
            let t =
                std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            let l: LocationFile =
                toml::from_str(&t).with_context(|| format!("parsing {}", p.display()))?;
            if l.location.enabled {
                locations.push(l);
            }
        }
        let env = EnvSettings {
            database_url: env_nonempty("WM_DATABASE_URL").or_else(database_url_from_parts),
            contact: env_nonempty("WM_CONTACT"),
            user_agent: env_nonempty("WM_USER_AGENT"),
            admin_token: env_nonempty("WM_ADMIN_TOKEN"),
            open_meteo_api_key: env_nonempty("WM_OPEN_METEO_API_KEY"),
        };
        let cfg = Self {
            file,
            locations,
            env,
            source_path: path,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Structural validation (fail closed at startup).
    pub fn validate(&self) -> Result<()> {
        let p = &self.file.providers;
        for (name, s) in [
            ("awc", &p.awc),
            ("tgftp", &p.tgftp),
            ("nws_api", &p.nws_api),
            ("polymarket_gamma", &p.polymarket_gamma),
            ("polymarket_clob", &p.polymarket_clob),
            ("polymarket_ws", &p.polymarket_ws),
            ("iem", &p.iem),
            ("open_meteo", &p.open_meteo),
            ("polymarket_data", &p.polymarket_data),
        ] {
            s.policy
                .validate()
                .with_context(|| format!("providers.{name}"))?;
        }
        let f = &self.file.forecast;
        if !(1..=7).contains(&f.lead_days) {
            bail!("forecast.lead_days must be 1..=7 (a same-day series is not knowledge-safe)");
        }
        if f.ready_local_minute >= 24 * 60 {
            bail!("forecast.ready_local_minute must be < 1440");
        }
        if f.refresh_minutes < 15 {
            bail!("forecast.refresh_minutes must be ≥ 15");
        }
        if f.min_eval_days < 30 {
            bail!("forecast.min_eval_days must be ≥ 30");
        }
        if f.model.is_empty()
            || !f
                .model
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            bail!(
                "forecast.model must be an Open-Meteo model id (a-z, 0-9, _), got '{}'",
                f.model
            );
        }
        let st = &self.file.strategies;
        for (name, weight, spread) in [
            (
                "buy_yes",
                st.buy_yes.market_weight,
                st.buy_yes.max_market_spread,
            ),
            (
                "buy_no",
                st.buy_no.market_weight,
                st.buy_no.max_market_spread,
            ),
        ] {
            if !(0.0..=1.0).contains(&weight) {
                bail!("strategies.{name}.market_weight must be 0..=1, got {weight}");
            }
            if spread <= Price::ZERO {
                bail!("strategies.{name}.max_market_spread must be > 0");
            }
        }
        let e = &st.book_confirmed;
        if !(e.start_local_minute < e.end_local_minute && e.end_local_minute <= 24 * 60) {
            bail!(
                "strategies.book_confirmed: need start_local_minute < end_local_minute ≤ 1440, got {}..{}",
                e.start_local_minute,
                e.end_local_minute
            );
        }
        if !(Price::ZERO < e.min_price && e.min_price <= e.max_price && e.max_price < Price::ONE) {
            bail!(
                "strategies.book_confirmed: need 0 < min_price ≤ max_price < 1, got {}..{}",
                e.min_price,
                e.max_price
            );
        }
        if e.lookback_minutes < 1 || e.lookback_minutes > 24 * 60 {
            bail!("strategies.book_confirmed.lookback_minutes must be 1..=1440");
        }
        if !(e.min_depth_shrink > 0.0 && e.min_depth_shrink <= 1.0) {
            bail!(
                "strategies.book_confirmed.min_depth_shrink must be in (0, 1], got {}",
                e.min_depth_shrink
            );
        }
        if !(e.min_depth_shares >= 0.0 && e.min_depth_shares.is_finite()) {
            bail!("strategies.book_confirmed.min_depth_shares must be ≥ 0");
        }
        if !(0.0..1.0).contains(&e.min_model_p) {
            bail!(
                "strategies.book_confirmed.min_model_p must be in [0, 1) (0 = no model veto), got {}",
                e.min_model_p
            );
        }
        if e.min_minutes_at_high < 0 || e.min_drop_tenths < 0 {
            bail!("strategies.book_confirmed: min_minutes_at_high and min_drop_tenths must be ≥ 0");
        }
        let f = &st.peak_slot;
        if !(0.0..=1.0).contains(&f.slot_from_quantile)
            || !(0.0..=1.0).contains(&f.slot_to_quantile)
            || f.slot_from_quantile > f.slot_to_quantile
        {
            bail!(
                "strategies.peak_slot: need 0 ≤ slot_from_quantile ≤ slot_to_quantile ≤ 1, got {} … {}",
                f.slot_from_quantile,
                f.slot_to_quantile
            );
        }
        if !(Price::ZERO < f.min_price && f.min_price < f.max_price && f.max_price < Price::ONE) {
            bail!(
                "strategies.peak_slot: need 0 < min_price < max_price < 1, got {} … {}",
                f.min_price,
                f.max_price
            );
        }
        if f.shares == 0 {
            bail!("strategies.peak_slot.shares must be ≥ 1");
        }
        if f.min_drop_tenths < 0 {
            bail!("strategies.peak_slot.min_drop_tenths must be ≥ 0");
        }
        if f.enabled {
            // One position must fit every pre-trade cap, or F never trades.
            let cost = wm_core::units::notional(
                f.max_price,
                wm_core::units::Shares::from_whole(i64::from(f.shares)),
                wm_core::units::Rounding::Up,
            );
            let risk = &self.file.risk;
            let id = wm_core::ids::StrategyId::from_static("F_peak_slot");
            let caps = [
                ("its position cap", Some(risk.position_size_for(&id))),
                ("its per-market cap", risk.market_cap_for(&id)),
                ("its per-strategy cap", risk.strategy_cap_for(&id)),
                (
                    "global_max_exposure_usd",
                    Some(risk.global_max_exposure_usd),
                ),
                ("max_location_exposure_usd", risk.max_location_exposure_usd),
                (
                    "max_daily_new_exposure_usd",
                    risk.max_daily_new_exposure_usd,
                ),
            ];
            for (name, cap) in caps {
                if let Some(cap) = cap
                    && cost > cap
                {
                    bail!(
                        "strategies.peak_slot: {} shares at up to {} cost {cost}, above {name} {cap}, so F could never trade — see [risk.strategy_caps.F_peak_slot] and the [risk] caps of the shipped configuration",
                        f.shares,
                        f.max_price
                    );
                }
            }
        }
        self.file.risk.validate().context("risk")?;
        if self.locations.is_empty() {
            bail!("no enabled locations configured");
        }
        for l in &self.locations {
            wm_core::ids::LocationId::new(l.location.id.clone()).context("location.id")?;
            wm_core::ids::StationId::new(l.station.id.clone()).context("station.id")?;
            l.location
                .timezone
                .parse::<chrono_tz::Tz>()
                .map_err(|e| anyhow::anyhow!("invalid timezone '{}': {e}", l.location.timezone))?;
            if l.station.routine_minutes.is_empty()
                || l.station.routine_minutes.iter().any(|m| *m > 59)
            {
                bail!(
                    "station.routine_minutes must be 0..=59 for {}",
                    l.location.id
                );
            }
        }
        if self.file.app.snapshot_interval_ms < 100 {
            bail!("app.snapshot_interval_ms must be ≥ 100");
        }
        Ok(())
    }

    /// User-Agent for outbound requests. NOAA/NWS require identification, so a
    /// contact is mandatory unless a full override is provided.
    pub fn user_agent(&self) -> Result<String> {
        if let Some(ua) = &self.env.user_agent {
            return wm_net::build_user_agent(ua, None).map_err(|e| anyhow::anyhow!(e.to_string()));
        }
        wm_net::build_user_agent(
            &self.file.identity.user_agent_template,
            self.env.contact.as_deref(),
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// The forecast product fetched live and used in training; `None` when
    /// forecasts are disabled.
    pub fn forecast_product(&self) -> Option<wm_core::forecast::ForecastProduct> {
        let f = &self.file.forecast;
        (f.enabled && self.file.providers.open_meteo.enabled).then(|| {
            wm_core::forecast::ForecastProduct {
                provider: wm_core::ids::ProviderId::open_meteo(),
                model: f.model.clone(),
                lead_days: f.lead_days,
                ready_local_minute: f.ready_local_minute,
            }
        })
    }

    /// Open-Meteo host: the customer host when a subscription key is set and
    /// the configured host is the free one.
    pub fn open_meteo_base_url(&self) -> String {
        let configured = self
            .file
            .providers
            .open_meteo
            .base_url
            .trim_end_matches('/');
        if self.env.open_meteo_api_key.is_some() && configured == wm_weather::open_meteo::FREE_HOST
        {
            wm_weather::open_meteo::CUSTOMER_HOST.to_owned()
        } else {
            configured.to_owned()
        }
    }

    pub fn buy_yes(&self) -> BuyYesConfig {
        let c = &self.file.strategies.buy_yes;
        BuyYesConfig {
            enabled: c.enabled,
            min_price: c.min_price,
            max_price: c.max_price,
            min_confirmation_minutes: c.min_confirmation_minutes,
            min_edge: c.min_edge,
            min_model_support: c.min_model_support,
            max_data_age_minutes: c.max_data_age_minutes,
            max_book_age_ms: c.max_book_age_ms,
            slippage_allowance: c.slippage_allowance,
            notional: self.file.risk.position_size_usd,
            market_weight: c.market_weight,
            max_market_spread: c.max_market_spread,
        }
    }

    pub fn buy_no(&self) -> BuyNoConfig {
        let c = &self.file.strategies.buy_no;
        BuyNoConfig {
            enabled: c.enabled,
            distances: c.distances.clone(),
            min_price: c.min_price,
            max_price: c.max_price,
            min_confirmation_minutes: c.min_confirmation_minutes,
            min_edge: c.min_edge,
            min_model_support: c.min_model_support,
            max_data_age_minutes: c.max_data_age_minutes,
            max_book_age_ms: c.max_book_age_ms,
            slippage_allowance: c.slippage_allowance,
            notional: self.file.risk.position_size_usd,
            market_weight: c.market_weight,
            max_market_spread: c.max_market_spread,
        }
    }

    pub fn certain(&self) -> wm_strategy::CertainConfig {
        let c = &self.file.strategies.certain;
        wm_strategy::CertainConfig {
            enabled: c.enabled,
            max_price: c.max_price,
            min_edge: c.min_edge,
            slippage_allowance: c.slippage_allowance,
            max_data_age_minutes: c.max_data_age_minutes,
            max_book_age_ms: c.max_book_age_ms,
            max_jump_tenths: c.max_jump_tenths,
            notional: self.file.risk.position_size_usd,
        }
    }

    pub fn book_confirmed(&self) -> wm_strategy::BookConfirmedConfig {
        let c = &self.file.strategies.book_confirmed;
        wm_strategy::BookConfirmedConfig {
            enabled: c.enabled,
            start_local_minute: c.start_local_minute,
            end_local_minute: c.end_local_minute,
            min_minutes_at_high: c.min_minutes_at_high,
            min_drop_tenths: c.min_drop_tenths,
            lookback_minutes: c.lookback_minutes,
            min_depth_shrink: c.min_depth_shrink,
            min_depth_shares: c.min_depth_shares,
            min_price: c.min_price,
            max_price: c.max_price,
            min_model_p: c.min_model_p,
            max_data_age_minutes: c.max_data_age_minutes,
            max_book_age_ms: c.max_book_age_ms,
            slippage_allowance: c.slippage_allowance,
            notional: self.file.risk.position_size_usd,
        }
    }

    pub fn peak_slot(&self) -> wm_strategy::PeakSlotConfig {
        let c = &self.file.strategies.peak_slot;
        wm_strategy::PeakSlotConfig {
            enabled: c.enabled,
            slot_from_quantile: c.slot_from_quantile,
            slot_to_quantile: c.slot_to_quantile,
            fallback_slots: c.fallback_slots.into(),
            min_price: c.min_price,
            max_price: c.max_price,
            shares: wm_core::units::Shares::from_whole(i64::from(c.shares)),
            min_drop_tenths: c.min_drop_tenths,
            max_data_age_minutes: c.max_data_age_minutes,
            max_book_age_ms: c.max_book_age_ms,
            slippage_allowance: c.slippage_allowance,
        }
    }

    pub fn split_unwind(&self) -> SplitUnwindConfig {
        let c = &self.file.strategies.split_unwind;
        SplitUnwindConfig {
            enabled: c.enabled,
            max_confirmation_minutes: c.max_confirmation_minutes,
            min_combined_p: c.min_combined_p,
            max_combined_price: c.max_combined_price,
            notional_per_leg: wm_core::units::Usd::from_micros(
                self.file.risk.position_size_usd.micros() / 2,
            ),
            max_data_age_minutes: c.max_data_age_minutes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn shipped_configuration_is_valid() {
        let cfg = AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        assert_eq!(cfg.locations.len(), 1);
        let ams = &cfg.locations[0];
        assert_eq!(ams.station.id, "EHAM");
        assert_eq!(ams.location.timezone, "Europe/Amsterdam");
        assert_eq!(ams.station.routine_minutes, vec![25, 55]);
        assert!(
            ams.market.confirmed_filter.is_none(),
            "must stay unset until Phase 0 verifies the WRH page"
        );
        assert_eq!(
            cfg.file.risk.position_size_usd,
            wm_core::units::Usd::from_whole(10)
        );
        // $100 for the $10 strategies plus one $100 position of strategy F,
        // which alone may exceed the $10 per position.
        let usd = wm_core::units::Usd::from_whole;
        assert_eq!(cfg.file.risk.global_max_exposure_usd, usd(200));
        let f_id = wm_core::ids::StrategyId::from_static("F_peak_slot");
        assert_eq!(cfg.file.risk.position_size_for(&f_id), usd(100));
        assert_eq!(
            cfg.file
                .risk
                .position_size_for(&wm_core::ids::StrategyId::from_static(
                    "E_book_confirmed_high"
                )),
            usd(10)
        );
        assert_eq!(cfg.file.risk.max_daily_loss_usd, Some(usd(30)));
        assert_eq!(
            cfg.peak_slot(),
            wm_strategy::PeakSlotConfig::default(),
            "the shipped [strategies.peak_slot] and the documented defaults must agree"
        );
        assert_eq!(cfg.file.app.mode, RunMode::Paper);
        assert!(cfg.file.providers.awc.policy.min_interval >= std::time::Duration::from_secs(30));
        assert_eq!(
            cfg.file.polling,
            PollingParams::default(),
            "the shipped polling section and the documented defaults must agree"
        );
    }

    #[test]
    fn nws_floor_is_enforced_by_config_validation() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        cfg.file.providers.awc.policy.min_interval = std::time::Duration::from_secs(1);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn forecast_defaults_and_host_selection() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        let p = cfg.forecast_product().unwrap();
        assert_eq!(p.label(), "open_meteo/gfs_global/d1");
        assert_eq!(p.ready_local_minute, 480);
        assert_eq!(cfg.open_meteo_base_url(), wm_weather::open_meteo::FREE_HOST);
        cfg.env.open_meteo_api_key = Some("k".into());
        assert_eq!(
            cfg.open_meteo_base_url(),
            wm_weather::open_meteo::CUSTOMER_HOST
        );
        cfg.file.forecast.enabled = false;
        assert!(cfg.forecast_product().is_none());
        cfg.file.forecast.enabled = true;
        cfg.file.forecast.lead_days = 0;
        assert!(cfg.validate().is_err(), "lead 0 would leak same-day runs");
        cfg.file.forecast.lead_days = 1;
        cfg.file.forecast.model = "gfs&x=1".into();
        assert!(cfg.validate().is_err(), "model id goes into a URL");
        cfg.file.forecast.model = "ecmwf_ifs".into();
        assert!(cfg.validate().is_ok());
    }

    /// The shipped file without the given sections and keys.
    fn shipped_without(sections: &[&str], keys: &[&str]) -> String {
        let text =
            std::fs::read_to_string(repo_root().join("configs/weather-machine.toml")).unwrap();
        let mut out = Vec::new();
        let mut skipping = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                skipping = sections
                    .iter()
                    .any(|s| t == format!("[{s}]") || t.starts_with(&format!("[{s}.")));
            }
            if skipping || keys.iter().any(|k| t.starts_with(&format!("{k} ="))) {
                continue;
            }
            out.push(line);
        }
        out.join("\n")
    }

    #[test]
    fn older_config_files_get_the_new_defaults() {
        let text = shipped_without(
            &[
                "providers.polymarket_data",
                "strategies.certain",
                "strategies.book_confirmed",
                "strategies.peak_slot",
                "strategies.peak_slot.fallback_slots",
                "risk.strategy_caps.F_peak_slot",
            ],
            &["market_weight", "max_market_spread"],
        );
        assert!(
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .all(|l| !l.contains("polymarket_data") && !l.contains("market_weight"))
        );
        let file: AppConfigFile = toml::from_str(&text).unwrap();
        let st = &file.strategies;
        for (w, spread) in [
            (st.buy_yes.market_weight, st.buy_yes.max_market_spread),
            (st.buy_no.market_weight, st.buy_no.max_market_spread),
        ] {
            assert!((w - 0.5).abs() < 1e-12);
            assert_eq!(spread, Price::saturating_from_micros(100_000));
        }
        assert!(st.certain.enabled);
        assert_eq!(st.book_confirmed, BookConfirmedConfigToml::default());
        // F needs its own risk caps: without its section it is off.
        assert!(!st.peak_slot.enabled);
        assert!(file.risk.strategy_caps.is_empty());
        let data = &file.providers.polymarket_data;
        assert!(data.enabled);
        assert_eq!(data.base_url, "https://data-api.polymarket.com");
        assert!(data.policy.min_interval >= std::time::Duration::from_millis(500));
        assert!(data.policy.validate().is_ok());
    }

    #[test]
    fn peak_slot_settings_map_and_are_validated() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        let f = cfg.peak_slot();
        assert!(f.enabled);
        assert_eq!(f.shares, wm_core::units::Shares::from_whole(100));
        assert_eq!(f.fallback_slots.summer, (15 * 60, 18 * 60));
        let ok = cfg.file.strategies.peak_slot.clone();
        let bad: [fn(&mut PeakSlotConfigToml); 7] = [
            |f| f.slot_from_quantile = -0.1,
            |f| f.slot_to_quantile = 1.1,
            |f| (f.slot_from_quantile, f.slot_to_quantile) = (0.9, 0.5),
            |f| f.min_price = f.max_price,
            |f| f.max_price = Price::ONE,
            |f| f.shares = 0,
            |f| f.min_drop_tenths = -1,
        ];
        for (i, f) in bad.into_iter().enumerate() {
            cfg.file.strategies.peak_slot = ok.clone();
            f(&mut cfg.file.strategies.peak_slot);
            assert!(cfg.validate().is_err(), "case {i} must be refused");
        }
        cfg.file.strategies.peak_slot = ok.clone();
        cfg.validate().unwrap();
        // 100 shares at up to 0.95 need F's own position cap …
        let risk = cfg.file.risk.clone();
        cfg.file.risk.strategy_caps.clear();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("above its position cap $10.00"), "{err}");
        assert!(err.contains("[risk.strategy_caps.F_peak_slot]"), "{err}");
        // … and room under every other cap an order of F meets.
        let short: [fn(&mut wm_risk::RiskConfig); 5] = [
            |r| {
                r.strategy_caps
                    .get_mut("F_peak_slot")
                    .unwrap()
                    .max_market_exposure_usd = None;
            },
            |r| {
                r.strategy_caps
                    .get_mut("F_peak_slot")
                    .unwrap()
                    .max_strategy_exposure_usd = None;
            },
            |r| r.global_max_exposure_usd = wm_core::units::Usd::from_whole(90),
            |r| r.max_location_exposure_usd = Some(wm_core::units::Usd::from_whole(50)),
            |r| r.max_daily_new_exposure_usd = Some(wm_core::units::Usd::from_whole(60)),
        ];
        for (i, f) in short.into_iter().enumerate() {
            cfg.file.risk = risk.clone();
            f(&mut cfg.file.risk);
            let err = format!("{:#}", cfg.validate().unwrap_err());
            assert!(err.contains("so F could never trade"), "case {i}: {err}");
        }
        // A disabled F needs none of it.
        cfg.file.risk.strategy_caps.clear();
        cfg.file.strategies.peak_slot.enabled = false;
        cfg.validate().unwrap();
        cfg.file.risk = risk;
        // Fallback slots are written as local times.
        let slots: SeasonSlotsToml = toml::from_str(
            "winter = \"13:00-16:00\"\nspring = \"14:30-17:30\"\nsummer = \"15:00-24:00\"\nautumn = \"00:00-17:00\"",
        )
        .unwrap();
        assert_eq!(slots.summer, Window(15 * 60, 24 * 60));
        assert_eq!(slots.autumn.to_string(), "00:00-17:00");
        for bad in [
            "\"25:00-26:00\"",
            "\"16:00-15:00\"",
            "\"1500-1800\"",
            "\"15:60-16:00\"",
            "\"24:30-24:40\"",
        ] {
            let text = format!(
                "winter = {bad}\nspring = \"14:30-17:30\"\nsummer = \"15:00-18:00\"\nautumn = \"14:00-17:00\""
            );
            assert!(toml::from_str::<SeasonSlotsToml>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn book_confirmed_settings_map_and_are_validated() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        let e = cfg.book_confirmed();
        let d = wm_strategy::BookConfirmedConfig::default();
        assert_eq!(
            wm_strategy::BookConfirmedConfig {
                notional: d.notional,
                ..e.clone()
            },
            d,
            "the shipped file states the defaults"
        );
        assert_eq!(e.notional, cfg.file.risk.position_size_usd);
        let ok = cfg.file.strategies.book_confirmed.clone();
        let bad: [fn(&mut BookConfirmedConfigToml); 8] = [
            |e| e.end_local_minute = e.start_local_minute,
            |e| e.end_local_minute = 24 * 60 + 1,
            |e| e.min_price = Price::parse("0.995").unwrap(),
            |e| e.max_price = Price::ONE,
            |e| e.lookback_minutes = 0,
            |e| e.min_depth_shrink = 0.0,
            |e| e.min_model_p = 1.0,
            |e| e.min_depth_shares = f64::NAN,
        ];
        for (i, f) in bad.iter().enumerate() {
            cfg.file.strategies.book_confirmed = ok.clone();
            f(&mut cfg.file.strategies.book_confirmed);
            assert!(cfg.validate().is_err(), "case {i} should be rejected");
        }
        cfg.file.strategies.book_confirmed = ok;
        cfg.file.strategies.book_confirmed.min_model_p = 0.9;
        assert!(cfg.validate().is_ok(), "a model veto is allowed");
    }

    #[test]
    fn market_pooling_settings_are_validated() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        assert!((cfg.buy_yes().market_weight - 0.5).abs() < 1e-12);
        assert!((cfg.buy_no().pooling().weight - 0.5).abs() < 1e-12);
        assert_eq!(cfg.buy_no().pooling().max_book_age_ms, 15_000);
        cfg.file.strategies.buy_yes.market_weight = 1.5;
        assert!(cfg.validate().is_err());
        cfg.file.strategies.buy_yes.market_weight = 0.0;
        assert!(cfg.validate().is_ok(), "0 = model only");
        cfg.file.strategies.buy_no.market_weight = f64::NAN;
        assert!(cfg.validate().is_err());
        cfg.file.strategies.buy_no.market_weight = 1.0;
        cfg.file.strategies.buy_no.max_market_spread = Price::ZERO;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let env = EnvSettings {
            database_url: Some("postgres://wm:hunter2@db/wm".into()),
            contact: Some("ops@example.org".into()),
            user_agent: None,
            admin_token: Some("tok-secret".into()),
            open_meteo_api_key: Some("key-secret".into()),
        };
        let text = format!("{env:?}");
        for secret in ["hunter2", "tok-secret", "key-secret"] {
            assert!(!text.contains(secret), "{text}");
        }
        assert!(text.contains("ops@example.org"));
    }

    #[test]
    fn url_components_are_encoded() {
        assert_eq!(url_component("wm"), "wm");
        assert_eq!(url_component("p@ss:w/rd#1 %"), "p%40ss%3Aw%2Frd%231%20%25");
        assert_eq!(url_component("a-b.c_d~"), "a-b.c_d~");
    }

    #[test]
    fn user_agent_requires_contact() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        cfg.env.contact = None;
        cfg.env.user_agent = None;
        assert!(cfg.user_agent().is_err());
        cfg.env.contact = Some("ops@example.org".into());
        assert!(cfg.user_agent().unwrap().contains("ops@example.org"));
    }
}
