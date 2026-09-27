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
pub mod runtime;
pub mod setup;
pub mod telemetry;
pub mod training;
