//! Logging (tracing) and metrics (Prometheus exposition via the `metrics` facade).

use anyhow::Result;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

/// Initialise tracing. `format`: `json` (production) or `pretty` (terminal).
pub fn init_tracing(format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,sqlx=warn,hyper=warn,reqwest=warn,tungstenite=warn")
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);
    let _ = if format.eq_ignore_ascii_case("json") {
        builder
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .try_init()
    } else {
        builder.compact().try_init()
    };
}

/// Install the Prometheus recorder and describe core metrics.
pub fn init_metrics() -> Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets(&[
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0,
        ])
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .install_recorder()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    metrics::describe_counter!("nws_requests_total", "Requests sent to NOAA/NWS providers");
    metrics::describe_counter!(
        "nws_429_total",
        "HTTP 429 responses from NOAA/NWS providers"
    );
    metrics::describe_counter!("nws_failures_total", "Failed NOAA/NWS requests (any cause)");
    metrics::describe_counter!(
        "nws_cache_hits",
        "Conditional requests answered 304 / served from cache"
    );
    metrics::describe_counter!(
        "nws_new_observations_total",
        "New observations delivered by NOAA/NWS providers"
    );
    metrics::describe_gauge!(
        "nws_requests_per_hour",
        "Requests to each NOAA/NWS provider in the last hour"
    );
    metrics::describe_counter!(
        "polymarket_requests_total",
        "Requests sent to Polymarket APIs"
    );
    metrics::describe_counter!("wm_engine_events_total", "Events processed by the engine");
    metrics::describe_histogram!("wm_engine_handle_seconds", "Engine event handling latency");
    metrics::describe_counter!("wm_decisions_total", "Decision records produced");
    metrics::describe_counter!("wm_orders_approved_total", "Risk-approved orders");
    metrics::describe_gauge!("wm_global_exposure_usd", "Worst-case global exposure");
    metrics::describe_gauge!("wm_kill_switch", "1 if the kill switch is engaged");
    let h = handle.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(5));
        loop {
            t.tick().await;
            h.run_upkeep();
        }
    });
    Ok(handle)
}

/// Install the process-wide rustls crypto provider (aws-lc-rs) once.
pub fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
