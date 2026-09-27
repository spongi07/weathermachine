//! Builders shared by `run`, `demo`, `collect` and `backtest`: configuration →
//! engine configuration, probability model, provider gates and sources.

use crate::config::{AppConfig, LocationFile, ProviderSection};
use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use std::collections::HashMap;
use std::sync::Arc;
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::FeeSchedule;
use wm_core::time::Clock;
use wm_core::trading::RunMode;
use wm_engine::{EngineConfig, EngineLocation};
use wm_net::{HttpFetcher, ProviderGate};
use wm_polymarket::LocationMarketSpec;
use wm_strategy::{EmpiricalPeakModel, NoEdgeModel, PeakConfig, ProbabilityModel};
use wm_weather::{
    AwcMetarSource, CadenceModel, NwsApiSource, ObservationSource, OpenMeteoPreviousRuns,
    PollingPolicy, TgftpMetarSource,
};

/// Resolved identifiers of one configured location.
#[derive(Debug, Clone)]
pub struct LocationIds {
    pub location: LocationId,
    pub station: StationId,
    pub timezone: Tz,
}

pub fn location_ids(l: &LocationFile) -> Result<LocationIds> {
    Ok(LocationIds {
        location: LocationId::new(l.location.id.clone()).context("location.id")?,
        station: StationId::new(l.station.id.clone()).context("station.id")?,
        timezone: l
            .location
            .timezone
            .parse()
            .map_err(|e| anyhow::anyhow!("timezone '{}': {e}", l.location.timezone))?,
    })
}

pub fn peak_config(l: &LocationFile) -> PeakConfig {
    PeakConfig {
        longitude: l.station.longitude,
        southern_hemisphere: l.station.latitude < 0.0,
        watch_start_after_noon: l.peak.watch_start_after_noon,
        watch_end_after_noon: l.peak.watch_end_after_noon,
        watch_margin_tenths: l.peak.watch_margin_tenths,
    }
}

pub fn cadence(l: &LocationFile) -> CadenceModel {
    CadenceModel {
        routine_minutes: l.station.routine_minutes.clone(),
        first_poll_delay_secs: l.station.first_poll_delay_secs,
        arrival_window_secs: l.station.arrival_window_secs,
    }
}

pub fn polling_policy(cfg: &AppConfig, l: &LocationFile) -> PollingPolicy {
    PollingPolicy::new(cadence(l), cfg.file.polling.clone())
}

pub fn market_spec(l: &LocationFile) -> Result<LocationMarketSpec> {
    let ids = location_ids(l)?;
    Ok(LocationMarketSpec {
        location: ids.location,
        station: ids.station,
        timezone: ids.timezone,
        slug_template: l.market.event_slug_template.clone(),
        unit: l.market.unit,
        fees: FeeSchedule::taker(l.market.taker_fee_rate.micros()),
    })
}

/// Engine configuration for a run.
pub fn engine_config(cfg: &AppConfig, mode: RunMode, run_id: RunId) -> Result<EngineConfig> {
    let mut locations = Vec::new();
    for l in &cfg.locations {
        let ids = location_ids(l)?;
        locations.push(EngineLocation {
            location: ids.location,
            station: ids.station,
            timezone: ids.timezone,
            peak: peak_config(l),
            confirmed_filter: l.market.confirmed_filter.map(|f| f.filter()),
        });
    }
    Ok(EngineConfig {
        mode,
        run_id,
        locations,
        risk: cfg.file.risk.clone(),
        buy_yes: cfg.buy_yes(),
        buy_no: cfg.buy_no(),
        split_unwind: cfg.split_unwind(),
        unwind: cfg.file.strategies.unwind.clone(),
        evaluate_on_book_updates: true,
        decision_log_capacity: 2_000,
        rejection_dedup_secs: wm_engine::default_rejection_dedup_secs(),
    })
}

/// Where the model lives: the configured path, else (while auto-training is
/// on) `<data_dir>/models/<station>.json` for the first location.
pub fn model_path(cfg: &AppConfig) -> Option<std::path::PathBuf> {
    if let Some(p) = cfg.file.model.path.as_deref() {
        return Some(std::path::PathBuf::from(p));
    }
    let at = &cfg.file.model.auto_train;
    if !at.enabled {
        return None;
    }
    let station = cfg.locations.first()?.station.id.to_ascii_lowercase();
    Some(
        std::path::Path::new(&at.data_dir)
            .join("models")
            .join(format!("{station}.json")),
    )
}

/// What startup found for the probability model.
pub enum ModelLoad {
    Loaded(Arc<EmpiricalPeakModel>),
    /// No file yet; auto-training may create it.
    Missing(std::path::PathBuf),
    /// No model path and auto-training disabled.
    NotConfigured,
    /// A file exists but is unusable. The service stays up without a model
    /// (fail closed) and the operator must fix or delete the file.
    Invalid(std::path::PathBuf, String),
}

/// Look for the model without failing the service.
pub fn find_model(cfg: &AppConfig) -> ModelLoad {
    let Some(path) = model_path(cfg) else {
        return ModelLoad::NotConfigured;
    };
    if !path.exists() {
        return ModelLoad::Missing(path);
    }
    let model = match read_model(&path) {
        Ok(m) => m,
        Err(e) => return ModelLoad::Invalid(path, format!("{e:#}")),
    };
    let stations: Vec<&str> = cfg
        .locations
        .iter()
        .map(|l| l.station.id.as_str())
        .collect();
    if !stations.contains(&model.station.as_str()) {
        return ModelLoad::Invalid(
            path,
            format!(
                "model {} was trained for station {} but the configured stations are {stations:?}",
                model.id, model.station
            ),
        );
    }
    tracing::info!(model = %model.id, station = %model.station, view = %model.view, samples = model.total_samples(), from = %model.trained_from, to = %model.trained_to, "probability model loaded");
    ModelLoad::Loaded(Arc::new(model))
}

/// Load the configured probability model for batch commands. A missing file
/// ⇒ [`NoEdgeModel`], which never produces a probability and therefore never
/// a weather trade; an unusable file is an error.
pub fn load_model(cfg: &AppConfig) -> Result<Arc<dyn ProbabilityModel>> {
    match find_model(cfg) {
        ModelLoad::Loaded(m) => Ok(m),
        ModelLoad::Missing(p) => {
            tracing::warn!(path = %p.display(), "model file not found: no-edge model (no weather trades)");
            Ok(Arc::new(NoEdgeModel))
        }
        ModelLoad::NotConfigured => {
            tracing::warn!(
                "no probability model configured: strategies A/B stay silent (no-edge model)"
            );
            Ok(Arc::new(NoEdgeModel))
        }
        ModelLoad::Invalid(p, e) => bail!("model {}: {e}", p.display()),
    }
}

pub fn read_model(path: &std::path::Path) -> Result<EmpiricalPeakModel> {
    let bytes = std::fs::read(path).with_context(|| format!("reading model {}", path.display()))?;
    let model: EmpiricalPeakModel = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing model {}", path.display()))?;
    if model.total_samples() == 0 {
        bail!("model {} has no samples", path.display());
    }
    Ok(model)
}

/// Client for the configured day-1 forecast (`None`: forecasts disabled).
pub fn forecast_client(cfg: &AppConfig, providers: &Providers) -> Option<OpenMeteoPreviousRuns> {
    let product = cfg.forecast_product()?;
    let fetcher = providers.fetcher("open_meteo")?;
    Some(OpenMeteoPreviousRuns::new(
        Arc::clone(fetcher),
        cfg.open_meteo_base_url(),
        cfg.env.open_meteo_api_key.clone(),
        product.model,
        product.lead_days,
    ))
}

/// One rate-limit gate and fetcher per provider, shared by every station.
pub struct Providers {
    pub fetchers: HashMap<&'static str, Arc<HttpFetcher>>,
    pub gates: HashMap<&'static str, Arc<ProviderGate>>,
}

fn provider_id(name: &str) -> ProviderId {
    match name {
        "awc" => ProviderId::awc(),
        "tgftp" => ProviderId::tgftp(),
        "nws_api" => ProviderId::nws_api(),
        "polymarket_gamma" => ProviderId::polymarket_gamma(),
        "polymarket_clob" => ProviderId::polymarket_clob(),
        "iem" => ProviderId::iem(),
        "open_meteo" => ProviderId::open_meteo(),
        _ => ProviderId::polymarket_ws(),
    }
}

impl Providers {
    pub fn build(cfg: &AppConfig, clock: Arc<dyn Clock>, user_agent: &str) -> Result<Self> {
        let p = &cfg.file.providers;
        let sections: [(&'static str, &ProviderSection); 8] = [
            ("awc", &p.awc),
            ("tgftp", &p.tgftp),
            ("nws_api", &p.nws_api),
            ("polymarket_gamma", &p.polymarket_gamma),
            ("polymarket_clob", &p.polymarket_clob),
            ("polymarket_ws", &p.polymarket_ws),
            ("iem", &p.iem),
            ("open_meteo", &p.open_meteo),
        ];
        let mut fetchers = HashMap::new();
        let mut gates = HashMap::new();
        for (i, (name, section)) in sections.into_iter().enumerate() {
            if !section.enabled {
                continue;
            }
            let gate = ProviderGate::new(
                provider_id(name),
                section.policy.clone(),
                Arc::clone(&clock),
                0x5EED_0000 + i as u64,
            );
            if name != "polymarket_ws" {
                let fetcher = HttpFetcher::new(Arc::clone(&gate), user_agent)
                    .map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
                fetchers.insert(name, Arc::new(fetcher));
            }
            gates.insert(name, gate);
        }
        Ok(Self { fetchers, gates })
    }

    pub fn fetcher(&self, name: &str) -> Option<&Arc<HttpFetcher>> {
        self.fetchers.get(name)
    }

    pub fn gate(&self, name: &str) -> Option<&Arc<ProviderGate>> {
        self.gates.get(name)
    }

    /// Observation sources for a location: primary first, then failovers.
    pub fn observation_sources(
        &self,
        cfg: &AppConfig,
        l: &LocationFile,
    ) -> Result<Vec<Arc<dyn ObservationSource>>> {
        let mut out: Vec<Arc<dyn ObservationSource>> = Vec::new();
        let names = std::iter::once(&l.observation_sources.primary)
            .chain(l.observation_sources.secondary.iter());
        for name in names {
            let Some(f) = self.fetcher(name) else {
                tracing::warn!(source = %name, location = %l.location.id, "observation source disabled or unknown; skipped");
                continue;
            };
            let p = &cfg.file.providers;
            let s: Arc<dyn ObservationSource> = match name.as_str() {
                "awc" => Arc::new(AwcMetarSource::new(Arc::clone(f), &p.awc.base_url, 3)),
                "tgftp" => Arc::new(TgftpMetarSource::new(Arc::clone(f), &p.tgftp.base_url)),
                "nws_api" => Arc::new(NwsApiSource::new(Arc::clone(f), &p.nws_api.base_url, 12)),
                other => bail!("unknown observation source '{other}' for {}", l.location.id),
            };
            out.push(s);
        }
        if out.is_empty() {
            bail!(
                "location {} has no enabled observation source",
                l.location.id
            );
        }
        Ok(out)
    }
}
