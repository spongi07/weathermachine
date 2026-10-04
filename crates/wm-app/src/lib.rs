//! # wm-app
//!
//! The `weather-machine` service: configuration, station collectors, market
//! discovery and streaming, the engine loop with simulated (paper) execution,
//! persistence, the dashboard/API server and the operator CLI.

pub mod cli;
pub mod config;
pub mod demo;
pub mod dto;
pub mod healthcheck;
pub mod http;
pub mod lite;
pub mod market_research;
pub mod paper_report;
pub mod restore;
pub mod runtime;
pub mod setup;
pub mod strategies;
pub mod strategy_log;
pub mod telemetry;
pub mod training;

/// The version, with the commit the image was built from when the build
/// passed one (`WM_GIT_SHA`, which CI sets): "0.1.0+bdf5668".
pub fn build_label() -> &'static str {
    static LABEL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        match option_env!("WM_GIT_SHA")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(sha) => format!(
                "{}+{}",
                wm_core::VERSION,
                sha.chars().take(7).collect::<String>()
            ),
            None => wm_core::VERSION.to_owned(),
        }
    });
    &LABEL
}
