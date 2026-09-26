//! `weather-machine` binary entry point.

use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = wm_app::cli::Cli::parse();
    match wm_app::cli::execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("weather-machine: {e:#}");
            ExitCode::FAILURE
        }
    }
}
