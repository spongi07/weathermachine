//! `weather-machine` binary entry point.

use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = wm_app::cli::Cli::parse();
    match wm_app::cli::execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e)
            if e.downcast_ref::<wm_app::runtime::RestartRequested>()
                .is_some() =>
        {
            eprintln!("weather-machine: {e}; exiting for the restart policy");
            ExitCode::from(wm_app::runtime::RESTART_EXIT_CODE)
        }
        Err(e) => {
            eprintln!("weather-machine: {e:#}");
            ExitCode::FAILURE
        }
    }
}
