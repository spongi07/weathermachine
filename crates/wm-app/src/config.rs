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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategiesSection {
    pub buy_yes: BuyYesConfigToml,
    pub buy_no: BuyNoConfigToml,
    pub split_unwind: SplitUnwindConfigToml,
    pub unwind: UnwindConfig,
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
    /// Trained model JSON; absent ⇒ no-edge model (no weather trades).
    pub path: Option<String>,
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
#[derive(Debug, Clone, Default)]
pub struct EnvSettings {
    pub database_url: Option<String>,
    pub contact: Option<String>,
    pub user_agent: Option<String>,
    pub admin_token: Option<String>,
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
        ] {
            s.policy
                .validate()
                .with_context(|| format!("providers.{name}"))?;
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
        assert_eq!(
            cfg.file.risk.global_max_exposure_usd,
            wm_core::units::Usd::from_whole(100)
        );
        assert_eq!(cfg.file.app.mode, RunMode::Paper);
        assert!(cfg.file.providers.awc.policy.min_interval >= std::time::Duration::from_secs(30));
    }

    #[test]
    fn nws_floor_is_enforced_by_config_validation() {
        let mut cfg =
            AppConfig::load(Some(&repo_root().join("configs/weather-machine.toml"))).unwrap();
        cfg.file.providers.awc.policy.min_interval = std::time::Duration::from_secs(1);
        assert!(cfg.validate().is_err());
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
