//! Command-line interface of the `weather-machine` binary.
#![allow(clippy::print_stdout)]

use crate::config::AppConfig;
use crate::demo::{self, DemoOptions};
use crate::http::{self, BasicAuth, Publisher, Shared};
use crate::market_research::{self, MarketResearchClients, MarketResearchPlan, ResearchProgress};
use crate::runtime::{self, RuntimeContext};
use crate::setup::Providers;
use crate::training::{self, Progress, TrainPlan};
use crate::{healthcheck, setup, telemetry};
use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{mpsc, watch};
use wm_backtest::{
    BacktestConfig, BacktestReport, BookConfirmedSim, Fidelity, MarketSimConfig, MarketStudyConfig,
    StudyConfig, import_iem_csv, run_backtest, study,
};
use wm_core::event::{EventEnvelope, WeatherMachineEvent};
use wm_core::ids::{RunId, StationId};
use wm_core::market::FeeSchedule;
use wm_core::resolution::ObservationFilter;
use wm_core::time::{Clock, SystemClock};
use wm_core::trading::RunMode;
use wm_core::units::Price;
use wm_execution::SimConfig;
use wm_polymarket::{DataApiClient, GammaClient, build_market, event_slug};
use wm_storage::PgStore;
use wm_strategy::ev::{break_even_table, research_price_grid};
use wm_weather::{
    CollectorConfig, CollectorRegistry, IemArchive, PollOutcome, PollingHints, StationCollector,
};

#[derive(Debug, Parser)]
#[command(
    name = "weather-machine",
    version,
    about = "Weather Machine — automated research and paper trading of Polymarket daily-high temperature markets"
)]
pub struct Cli {
    /// Main configuration file (default: $WM_CONFIG or configs/weather-machine.toml).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the paper-trading service: collectors, market data, engine, dashboard (default).
    Run,
    /// Run the dashboard on synthetic data in accelerated time (no network, no database).
    Demo {
        /// Virtual seconds per wall-clock second.
        #[arg(long, env = "WM_DEMO_SPEED", default_value_t = 60.0)]
        speed: f64,
        #[arg(long, env = "WM_DEMO_SEED", default_value_t = 7)]
        seed: u64,
        /// Days of synthetic history used to train the demo model.
        #[arg(long, default_value_t = 240)]
        train_days: u32,
    },
    /// Phase-0 data experiment: run the station collectors only (zero trades).
    Collect {
        /// Poll each station exactly once and print the result.
        #[arg(long)]
        once: bool,
        /// Do not use PostgreSQL even if WM_DATABASE_URL is set.
        #[arg(long)]
        no_db: bool,
    },
    /// Apply database migrations and exit.
    Migrate,
    /// Research tooling.
    Research {
        #[command(subcommand)]
        command: ResearchCommand,
    },
    /// Probability model: train from real METAR history (IEM archive).
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Backtest the configured strategies (synthetic days or a recorded run journal).
    Backtest {
        /// Number of synthetic days (plumbing test — synthetic data is not evidence of edge).
        #[arg(long, conflicts_with = "journal")]
        synthetic_days: Option<u32>,
        /// Replay the event journal of a recorded paper run (run id), re-simulating execution.
        #[arg(long)]
        journal: Option<String>,
        #[arg(long, default_value_t = 7)]
        seed: u64,
        /// Trained model JSON (default: the configured model; synthetic runs train one).
        #[arg(long)]
        model: Option<PathBuf>,
        /// Write the full JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Market tooling.
    Markets {
        #[command(subcommand)]
        command: MarketsCommand,
    },
    /// Resolution-rules review.
    Rules {
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Print the break-even table (price → probability needed, fee included).
    EvTable {
        /// Taker fee rate (e.g. 0.05).
        #[arg(long, default_value = "0.05")]
        fee_rate: String,
    },
    /// Validate configuration and environment, then exit.
    CheckConfig,
    /// Container health probe (exit 0 = healthy).
    Healthcheck {
        #[arg(long)]
        url: Option<String>,
        #[arg(long, default_value_t = 3)]
        timeout_secs: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum ResearchCommand {
    /// P(high is final | N minutes observed since the high), with Wilson intervals,
    /// from an IEM ASOS CSV export (columns station,valid,metar); also trains the model.
    PeakSurvival {
        #[arg(long)]
        csv: PathBuf,
        #[arg(long, default_value = "EHAM")]
        station: String,
        #[arg(long, default_value = "Europe/Amsterdam")]
        timezone: String,
        #[arg(long, value_enum, default_value_t = FilterArg::All)]
        filter: FilterArg,
        /// Assumed publication delay (knowledge time = observation + delay).
        #[arg(long, default_value_t = 5)]
        publication_delay_min: i64,
        /// Write the trained model JSON here (use with WM_MODEL_PATH / [model].path).
        #[arg(long)]
        model_out: Option<PathBuf>,
        /// Write the Markdown report here.
        #[arg(long)]
        report_out: Option<PathBuf>,
    },
    /// Model versus market on settled markets: who predicts better, the best
    /// market_weight, who is sure of the winner first, how fast dead buckets
    /// reprice after a new high, and whether the METAR high matched the
    /// resolution. Downloads trade history from the Polymarket Data API
    /// (settled days are cached) and uses the training's METAR history.
    Market {
        /// First local date (default: 60 days before --to).
        #[arg(long)]
        from: Option<NaiveDate>,
        /// Last local date (default: yesterday).
        #[arg(long)]
        to: Option<NaiveDate>,
        /// Seconds from an observation to the bot's decision.
        #[arg(long, default_value_t = 180)]
        delay_secs: i64,
        /// Markdown report (default: <data_dir>/research/<station>-market.md;
        /// the JSON report is written beside it).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also print the whole Markdown report (for container logs).
        #[arg(long)]
        print: bool,
        /// Replay this day report by report: both models' cells and
        /// probabilities, the market and every simulated trade (repeatable;
        /// the date range is extended to include it).
        #[arg(long = "day")]
        days: Vec<NaiveDate>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FilterArg {
    All,
    HourlyNwsFaa,
    HourlyOther,
}

impl FilterArg {
    fn filter(self) -> ObservationFilter {
        match self {
            FilterArg::All => ObservationFilter::AllRows,
            FilterArg::HourlyNwsFaa => ObservationFilter::WRH_HOURLY_NWS_FAA,
            FilterArg::HourlyOther => ObservationFilter::WRH_HOURLY_OTHER,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Download the first location's METAR history from IEM (rate-limited,
    /// finished years cached) and train the model. Restart `run` to load it.
    Train {
        /// Model file (default: WM_MODEL_PATH, else <data_dir>/models/<station>.json).
        #[arg(long)]
        out: Option<PathBuf>,
        /// First year of history (default: [model.auto_train].from_year).
        #[arg(long)]
        from_year: Option<i32>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MarketsCommand {
    /// Fetch and parse the configured locations' markets for a date (read-only).
    Discover {
        /// Local date (default: today in each location's time zone).
        #[arg(long)]
        date: Option<NaiveDate>,
    },
}

#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// Record a human approval of a rules text (by SHA-256).
    Approve {
        #[arg(long)]
        sha256: String,
        #[arg(long)]
        reviewer: String,
    },
}

/// Execute a parsed command line.
pub async fn execute(cli: Cli) -> Result<()> {
    telemetry::init_crypto();
    match cli.command.unwrap_or(Command::Run) {
        Command::Healthcheck { url, timeout_secs } => {
            let url = url.unwrap_or_else(healthcheck::default_url);
            healthcheck::probe(
                &url,
                std::time::Duration::from_secs(timeout_secs.clamp(1, 60)),
            )
            .await
        }
        Command::EvTable { fee_rate } => ev_table(&fee_rate),
        Command::CheckConfig => {
            let cfg = AppConfig::load(cli.config.as_deref())?;
            println!(
                "configuration OK: {} location(s), mode {}",
                cfg.locations.len(),
                cfg.file.app.mode.as_str()
            );
            for l in &cfg.locations {
                println!(
                    "  {} → station {} ({}), sources {} + {:?}, confirmed filter {:?}",
                    l.location.id,
                    l.station.id,
                    l.location.timezone,
                    l.observation_sources.primary,
                    l.observation_sources.secondary,
                    l.market.confirmed_filter
                );
            }
            match cfg.user_agent() {
                Ok(ua) => println!("User-Agent: {ua}"),
                Err(e) => println!("User-Agent: NOT CONFIGURED ({e}) — set WM_CONTACT"),
            }
            println!(
                "database: {}",
                if cfg.env.database_url.is_some() {
                    "configured"
                } else {
                    "not configured (trading blocked)"
                }
            );
            let auto = if cfg.file.model.auto_train.enabled {
                "auto-train on"
            } else {
                "auto-train off"
            };
            match cfg.forecast_product() {
                Some(p) => println!(
                    "day-1 forecast: {} via {}{} — used only if the training evaluation adopts it",
                    p.label(),
                    cfg.open_meteo_base_url(),
                    if cfg.env.open_meteo_api_key.is_some() {
                        " (API key set)"
                    } else {
                        " (free tier: non-commercial use)"
                    }
                ),
                None => println!("day-1 forecast: off"),
            }
            match setup::find_model(&cfg) {
                setup::ModelLoad::Loaded(m) => {
                    println!(
                        "model: {} ({} samples, {} → {})",
                        m.id,
                        m.total_samples(),
                        m.trained_from,
                        m.trained_to
                    );
                    match &m.forecast {
                        Some(f) => println!("model forecast: {}", f.verdict),
                        None => println!("model forecast: not evaluated yet"),
                    }
                }
                setup::ModelLoad::Missing(p) => {
                    println!(
                        "model: not yet at {} ({auto}; no weather trades until then)",
                        p.display()
                    );
                }
                setup::ModelLoad::NotConfigured => {
                    println!("model: none ({auto}; no-edge: no weather trades)");
                }
                setup::ModelLoad::Invalid(p, e) => {
                    println!("model: UNUSABLE {} — {e}", p.display())
                }
            }
            Ok(())
        }
        Command::Model {
            command: ModelCommand::Train { out, from_year },
        } => model_train(cli.config, out, from_year).await,
        Command::Run => serve(cli.config, Mode::Run).await,
        Command::Demo {
            speed,
            seed,
            train_days,
        } => {
            serve(
                cli.config,
                Mode::Demo(DemoOptions {
                    speed,
                    seed,
                    train_days,
                    ..DemoOptions::default()
                }),
            )
            .await
        }
        Command::Collect { once, no_db } => collect(cli.config, once, no_db).await,
        Command::Migrate => {
            let cfg = AppConfig::load(cli.config.as_deref())?;
            telemetry::init_tracing(&cfg.file.app.log_format);
            let url = cfg
                .env
                .database_url
                .as_deref()
                .context("WM_DATABASE_URL is required")?;
            let store = PgStore::connect(url, 2).await?;
            store.migrate().await?;
            println!("migrations applied");
            Ok(())
        }
        Command::Research {
            command:
                ResearchCommand::PeakSurvival {
                    csv,
                    station,
                    timezone,
                    filter,
                    publication_delay_min,
                    model_out,
                    report_out,
                },
        } => peak_survival(
            cli.config,
            csv,
            &station,
            &timezone,
            filter,
            publication_delay_min,
            model_out,
            report_out,
        ),
        Command::Research {
            command:
                ResearchCommand::Market {
                    from,
                    to,
                    delay_secs,
                    out,
                    print,
                    days,
                },
        } => research_market(cli.config, from, to, delay_secs, out, print, days).await,
        Command::Backtest {
            synthetic_days,
            journal,
            seed,
            model,
            out,
        } => backtest(cli.config, synthetic_days, journal, seed, model, out).await,
        Command::Markets {
            command: MarketsCommand::Discover { date },
        } => discover(cli.config, date).await,
        Command::Rules {
            command: RulesCommand::Approve { sha256, reviewer },
        } => {
            let cfg = AppConfig::load(cli.config.as_deref())?;
            let url = cfg
                .env
                .database_url
                .as_deref()
                .context("WM_DATABASE_URL is required")?;
            let store = PgStore::connect(url, 2).await?;
            if store.approve_rules(sha256.trim(), reviewer.trim()).await? {
                println!("rules {sha256} approved by {reviewer}");
                Ok(())
            } else {
                bail!("no stored rules text with sha256 {sha256} (discover the market first)")
            }
        }
    }
}

enum Mode {
    Run,
    Demo(DemoOptions),
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// `model train`: foreground training with progress on stdout.
async fn model_train(
    config: Option<PathBuf>,
    out: Option<PathBuf>,
    from_year: Option<i32>,
) -> Result<()> {
    let mut cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    if let Some(y) = from_year {
        cfg.file.model.auto_train.from_year = y;
    }
    let path = out
        .or_else(|| {
            let mut c = cfg.clone();
            c.file.model.auto_train.enabled = true;
            setup::model_path(&c)
        })
        .context("no model path: pass --out or set WM_MODEL_PATH")?;
    let user_agent = cfg
        .user_agent()
        .context("set WM_CONTACT so requests identify Weather Machine")?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let providers = Providers::build(&cfg, Arc::clone(&clock), &user_agent)?;
    let fetcher = providers
        .fetcher("iem")
        .context("the IEM provider is disabled in the configuration")?;
    let archive = IemArchive::new(Arc::clone(fetcher), &cfg.file.providers.iem.base_url);
    let plan = TrainPlan::from_config(&cfg, path, clock.now().date_naive())?;
    let forecast = setup::forecast_client(&cfg, &providers);
    let (_stop_tx, mut stop_rx) = watch::channel(false);
    let progress = |p: Progress| match p {
        Progress::Downloading { year, done, total } => {
            println!(
                "[{done}/{total}] {year}: cached or downloading from IEM (one request at a time)"
            );
        }
        Progress::Forecast { year, done } => {
            println!(
                "forecast {year}: cached or downloading from Open-Meteo ({done} years so far)"
            );
        }
        Progress::Training { observations } => {
            println!("training and evaluating on {observations} historical reports…");
        }
    };
    let o = training::train(
        &archive,
        forecast.as_ref(),
        &plan,
        None,
        &progress,
        &mut stop_rx,
    )
    .await?;
    println!(
        "model {} written to {} — {} days ({} → {}), {} samples; {} years downloaded, {} cached",
        o.model_id,
        plan.model_out.display(),
        o.days,
        o.from.map(|d| d.to_string()).unwrap_or_default(),
        o.to.map(|d| d.to_string()).unwrap_or_default(),
        o.samples,
        o.years_downloaded,
        o.years_cached
    );
    if let Some(v) = &o.structure_verdict {
        println!("model structure: {v}");
    }
    if let Some(v) = &o.forecast_verdict {
        println!("day-1 forecast: {v}");
    }
    println!("report: {}", plan.report_out.display());
    println!("restart the service to load the model");
    Ok(())
}

/// `research market`: the model against the market on settled days.
async fn research_market(
    config: Option<PathBuf>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    delay_secs: i64,
    out: Option<PathBuf>,
    print: bool,
    days: Vec<NaiveDate>,
) -> Result<()> {
    let cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    let user_agent = cfg
        .user_agent()
        .context("set WM_CONTACT so requests identify Weather Machine")?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let providers = Providers::build(&cfg, Arc::clone(&clock), &user_agent)?;
    let loc = cfg.locations.first().context("no location configured")?;
    let spec = setup::market_spec(loc)?;
    let today = wm_core::time::local_date(clock.now(), spec.timezone);
    let to = to
        .unwrap_or(today - Duration::days(1))
        .max(days.iter().copied().max().unwrap_or(NaiveDate::MIN));
    if to >= today {
        bail!("--to and --day must be before today ({today}): only settled days can be scored");
    }
    let from = from
        .unwrap_or(to - Duration::days(59))
        .min(days.iter().copied().min().unwrap_or(NaiveDate::MAX));
    let fetcher = |name: &str| {
        providers
            .fetcher(name)
            .cloned()
            .with_context(|| format!("providers.{name} is disabled in the configuration"))
    };
    let p = &cfg.file.providers;
    let gamma = GammaClient::new(fetcher("polymarket_gamma")?, &p.polymarket_gamma.base_url);
    let data = DataApiClient::new(fetcher("polymarket_data")?, &p.polymarket_data.base_url);
    let archive = IemArchive::new(fetcher("iem")?, &p.iem.base_url);
    let model_out = {
        let mut c = cfg.clone();
        c.file.model.auto_train.enabled = true;
        setup::model_path(&c)
    }
    .context("no model path: set WM_MODEL_PATH or [model.auto_train].data_dir")?;
    let train = TrainPlan::from_config(&cfg, model_out, clock.now().date_naive())?;
    // Evaluate the model as live trading uses it: with the forecast only if
    // the installed model adopted it.
    let use_forecast = match setup::find_model(&cfg) {
        setup::ModelLoad::Loaded(m) => {
            println!(
                "installed model {}: evaluated {} the day-1 forecast",
                m.id,
                if m.uses_forecast() { "with" } else { "without" }
            );
            m.uses_forecast()
        }
        _ => {
            println!("no installed model: evaluated without the forecast");
            false
        }
    };
    let forecast = setup::forecast_client(&cfg, &providers);
    let yes = cfg.buy_yes();
    let no = cfg.buy_no();
    // Replay the live rule of strategy A (and B's distances) plus shorter
    // confirmations and a wider ask range.
    let live_window = u32::try_from(yes.min_confirmation_minutes.clamp(0, 1_440)).unwrap_or(60);
    let live_range = (yes.min_price.as_f64(), yes.max_price.as_f64());
    let mut windows = vec![0, 30, live_window];
    windows.sort_unstable();
    windows.dedup();
    let wide = (0.70_f64.min(live_range.0), live_range.1);
    let study = MarketStudyConfig {
        knowledge_delay: Duration::seconds(delay_secs.clamp(0, 3_600)),
        configured_weight: yes.market_weight,
        min_model_support: yes.min_model_support,
        taker_fee_rate: loc.market.taker_fee_rate.as_f64(),
        sim: MarketSimConfig {
            windows,
            ranges: if wide == live_range {
                vec![live_range]
            } else {
                vec![live_range, wide]
            },
            live_window,
            live_range,
            min_edge: yes.min_edge,
            slippage: yes.slippage_allowance.as_f64(),
            max_market_spread: yes.max_market_spread.as_f64(),
            no_distances: no.distances.clone(),
            stake_usd: yes.notional.as_f64(),
            e: BookConfirmedSim::from_live(&cfg.book_confirmed()),
        },
        timeline_days: days,
        ..MarketStudyConfig::new(train.station.clone(), train.tz, train.peak.clone())
    };
    let data_dir = PathBuf::from(&cfg.file.model.auto_train.data_dir);
    let lower = train.station.as_str().to_ascii_lowercase();
    let plan = MarketResearchPlan {
        cache_dir: data_dir
            .join("research")
            .join("polymarket")
            .join(train.station.as_str()),
        report_out: out
            .unwrap_or_else(|| data_dir.join("research").join(format!("{lower}-market.md"))),
        train,
        spec,
        from,
        to,
        study,
        use_forecast,
    };
    println!(
        "model versus market for {} → {} (decision = observation + {} s)",
        plan.from,
        plan.to,
        plan.study.knowledge_delay.num_seconds()
    );
    let progress = |p: ResearchProgress| match p {
        ResearchProgress::Market { date, done, total } => {
            println!(
                "[{}/{total}] {date}: event and trades (cached when settled)",
                done + 1
            );
        }
        ResearchProgress::History(Progress::Downloading { year, done, total }) => {
            println!(
                "METAR history [{}/{total}] {year}: cached or downloading from IEM",
                done + 1
            );
        }
        ResearchProgress::History(Progress::Forecast { year, .. }) => {
            println!("forecast history {year}: cached or downloading from Open-Meteo");
        }
        ResearchProgress::History(Progress::Training { .. }) => {}
        ResearchProgress::Studying { market_days } => {
            println!("replaying the history and scoring {market_days} settled market days…");
        }
    };
    let (_stop_tx, mut stop_rx) = watch::channel(false);
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: forecast.as_ref(),
    };
    let o = market_research::run(&clients, &plan, clock.now(), &progress, &mut stop_rx).await?;
    println!(
        "{} settled market days ({} cached, {} downloaded); {} dates without one",
        o.report.market_days,
        o.days_cached,
        o.days_downloaded,
        o.unavailable.len()
    );
    for line in o.report.verdict.iter().chain(&o.report.strategy_verdict) {
        println!("• {line}");
    }
    println!("report: {}", o.markdown.display());
    println!("json:   {}", o.json.display());
    if print {
        let md = std::fs::read_to_string(&o.markdown)
            .with_context(|| format!("reading {}", o.markdown.display()))?;
        println!("\n{md}");
    }
    Ok(())
}

/// `run` and `demo`: HTTP server + runtime, graceful shutdown on SIGTERM/SIGINT.
async fn serve(config: Option<PathBuf>, mode: Mode) -> Result<()> {
    let cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    let prometheus = telemetry::init_metrics()
        .map_err(|e| tracing::warn!(error = %e, "metrics recorder not installed"))
        .ok();
    let (publisher, snapshots) = Publisher::new();
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let ready = Arc::new(AtomicBool::new(false));
    let ui_dir = PathBuf::from(&cfg.file.app.ui_dir);
    let basic_auth = match (
        std::env::var("WM_DASHBOARD_USER")
            .ok()
            .filter(|s| !s.is_empty()),
        std::env::var("WM_DASHBOARD_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty()),
    ) {
        (Some(u), Some(p)) => Some(BasicAuth::new(&u, &p)),
        (None, None) => None,
        _ => bail!("set both WM_DASHBOARD_USER and WM_DASHBOARD_PASSWORD (or neither)"),
    };
    if !ui_dir.join("index.html").is_file() {
        tracing::warn!(ui_dir = %ui_dir.display(), "dashboard assets not found: serving the /lite dashboard only");
    }
    let shared = Arc::new(Shared {
        snapshots,
        commands: cmd_tx,
        admin_token: cfg.env.admin_token.clone(),
        basic_auth,
        prometheus,
        ui_dir: Some(ui_dir),
        ready: Arc::clone(&ready),
        liveness_max_age: std::time::Duration::from_secs(60),
    });
    let listener = tokio::net::TcpListener::bind(&cfg.file.app.http_bind)
        .await
        .with_context(|| format!("binding {}", cfg.file.app.http_bind))?;
    tracing::info!(addr = %cfg.file.app.http_bind, "dashboard listening");
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server = tokio::spawn(http::serve(listener, shared, shutdown_rx.clone()));
    let signal_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received");
        let _ = signal_tx.send(true);
    });
    let result = match mode {
        Mode::Run => {
            runtime::run(
                cfg,
                RuntimeContext {
                    publisher,
                    commands: cmd_rx,
                    ready,
                    shutdown: shutdown_rx,
                },
            )
            .await
        }
        Mode::Demo(opts) => {
            let opts = DemoOptions {
                snapshot_interval: std::time::Duration::from_millis(
                    cfg.file.app.snapshot_interval_ms,
                ),
                ..opts
            };
            demo::run(cfg, opts, publisher, cmd_rx, ready, shutdown_rx).await
        }
    };
    let _ = shutdown_tx.send(true);
    match tokio::time::timeout(std::time::Duration::from_secs(10), server).await {
        Ok(Ok(Err(e))) => tracing::error!(error = %e, "HTTP server error"),
        Ok(Err(e)) => tracing::error!(error = %e, "HTTP server task failed"),
        _ => {}
    }
    if let Err(e) = &result {
        tracing::error!(error = %format!("{e:#}"), "runtime stopped with an error");
    }
    result
}

fn ev_table(fee_rate: &str) -> Result<()> {
    let rate = Price::parse(fee_rate).map_err(|e| anyhow::anyhow!("invalid fee rate: {e}"))?;
    let fee = FeeSchedule::taker(rate.micros());
    println!(
        "{:>7} {:>10} {:>14} {:>22}",
        "price", "fee/share", "break-even p", "wins to recover 1 loss"
    );
    for r in break_even_table(&research_price_grid(), &fee, Price::ZERO) {
        println!(
            "{:>7} {:>10.5} {:>13.2}% {:>22.1}",
            r.price.to_string(),
            r.fee_per_share,
            r.break_even_probability * 100.0,
            r.wins_to_recover_one_loss
        );
    }
    Ok(())
}

async fn collect(config: Option<PathBuf>, once: bool, no_db: bool) -> Result<()> {
    let cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let ua = cfg
        .user_agent()
        .context("NOAA/NWS require an identifying User-Agent: set WM_CONTACT")?;
    let store = match (&cfg.env.database_url, no_db) {
        (Some(url), false) => {
            let s = PgStore::connect(url, cfg.file.database.max_connections).await?;
            if cfg.file.app.auto_migrate {
                s.migrate().await?;
            }
            Some(s)
        }
        _ => None,
    };
    let providers = setup::Providers::build(&cfg, Arc::clone(&clock), &ua)?;
    let registry = CollectorRegistry::new();
    let (events_tx, mut events_rx) = mpsc::channel::<EventEnvelope>(4096);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut collectors = Vec::new();
    let mut leases = Vec::new();
    let mut hint_senders = Vec::new();
    for l in &cfg.locations {
        let ids = setup::location_ids(l)?;
        if let Some(s) = &store {
            match s.try_station_lease(&ids.station).await? {
                Some(lease) => leases.push(lease),
                None => bail!(
                    "station {} is already being collected by another instance",
                    ids.station
                ),
            }
        }
        let claim = registry
            .claim(&ids.station)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let sink: Arc<dyn wm_core::ingest::IngestSink> = match &store {
            Some(s) => Arc::new(s.clone()),
            None => Arc::new(wm_core::ingest::MemoryIngestSink::new()),
        };
        let (hint_tx, hint_rx) = watch::channel(PollingHints::default());
        hint_senders.push(hint_tx);
        let c = StationCollector::new(
            claim,
            CollectorConfig {
                station: ids.station.clone(),
                location: ids.location.clone(),
                timezone: ids.timezone,
                policy: setup::polling_policy(&cfg, l),
                health: cfg.file.health.clone(),
                max_gate_wait: std::time::Duration::from_secs(90),
            },
            providers.observation_sources(&cfg, l)?,
            sink,
            events_tx.clone(),
            hint_rx,
            Arc::clone(&clock),
        );
        collectors.push(c);
    }
    drop(events_tx);
    let printer = tokio::spawn(async move {
        while let Some(e) = events_rx.recv().await {
            match &e.event {
                WeatherMachineEvent::WeatherObservation(o) => {
                    let ob = &o.observation;
                    println!(
                        "{} {} {:<6} {:>5} °C  {:<9} via {:<6} known after {:>4}s  {}",
                        ob.key.station,
                        ob.key.observed_at.format("%Y-%m-%d %H:%MZ"),
                        ob.key.report_type.as_str(),
                        ob.temperature
                            .map_or_else(|| "—".to_owned(), |t| format!("{:.1}", t.as_f64())),
                        o.class.as_str(),
                        ob.provider,
                        ob.knowledge_delay_secs(),
                        ob.raw_text
                    );
                }
                WeatherMachineEvent::WeatherCorrection(c) => println!(
                    "CORRECTION {} v{} → v{}",
                    c.current.key.fingerprint(),
                    c.previous.version,
                    c.current.version
                ),
                WeatherMachineEvent::ProviderHealthChanged(h) => println!(
                    "health {} [{}] → {} ({})",
                    h.snapshot.provider,
                    h.snapshot
                        .scope
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    h.snapshot.state,
                    h.snapshot.reason
                ),
                _ => {}
            }
        }
    });
    if once {
        for mut c in collectors {
            let station = c.station().clone();
            let outcome = c.poll_once().await;
            match &outcome {
                PollOutcome::Fetched {
                    provider,
                    new,
                    out_of_order,
                    duplicates,
                    corrections,
                } => println!(
                    "{station}: {provider} → {new} new, {out_of_order} late, {duplicates} duplicate, {corrections} corrected"
                ),
                PollOutcome::GateClosed { provider, retry_in } => println!(
                    "{station}: {provider} gate closed, retry in {} s",
                    retry_in.as_secs()
                ),
                PollOutcome::Failed {
                    provider,
                    detail,
                    throttled,
                } => println!(
                    "{station}: {provider} FAILED{}: {detail}",
                    if *throttled { " (throttled)" } else { "" }
                ),
            }
        }
    } else {
        let mut handles = Vec::new();
        for c in collectors {
            handles.push(tokio::spawn(c.run(shutdown_rx.clone())));
        }
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
        for h in handles {
            let _ = h.await;
        }
    }
    let _ = printer.await;
    for (name, gate) in &providers.gates {
        let s = gate.stats();
        if s.requests_total > 0 {
            println!(
                "{name}: {} request(s), {} throttled, {} failed",
                s.requests_total, s.throttled_total, s.failures_total
            );
        }
    }
    for l in leases {
        let _ = l.release().await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn peak_survival(
    config: Option<PathBuf>,
    csv: PathBuf,
    station: &str,
    timezone: &str,
    filter: FilterArg,
    delay_min: i64,
    model_out: Option<PathBuf>,
    report_out: Option<PathBuf>,
) -> Result<()> {
    let station = StationId::new(station.to_owned()).context("station")?;
    let tz: chrono_tz::Tz = timezone
        .parse()
        .map_err(|e| anyhow::anyhow!("timezone: {e}"))?;
    let file = std::fs::File::open(&csv).with_context(|| format!("opening {}", csv.display()))?;
    let (obs, stats) = import_iem_csv(BufReader::new(file), &station, Duration::minutes(delay_min))
        .map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "imported {} of {} rows (comments {}, other stations {}, unparseable {}, no temperature {}, duplicates {})",
        stats.imported,
        stats.rows,
        stats.skipped_comment,
        stats.skipped_station,
        stats.skipped_unparseable,
        stats.skipped_no_temperature,
        stats.duplicates
    );
    if obs.is_empty() {
        bail!("no observations imported");
    }
    // Station geometry from configuration when available (solar noon).
    let peak = AppConfig::load(config.as_deref())
        .ok()
        .and_then(|c| {
            c.locations
                .iter()
                .find(|l| l.station.id == station.as_str())
                .map(setup::peak_config)
        })
        .unwrap_or_default();
    let cfg = StudyConfig {
        station,
        tz,
        filter: filter.filter(),
        peak,
        k_classes: 4,
        min_high_local_minute: 9 * 60,
    };
    let (report, model) = study(&obs, &cfg);
    let md = report.to_markdown();
    println!("{md}");
    if let Some(p) = report_out {
        std::fs::write(&p, &md).with_context(|| format!("writing {}", p.display()))?;
    }
    if let Some(p) = model_out {
        std::fs::write(&p, serde_json::to_vec_pretty(&model)?)
            .with_context(|| format!("writing {}", p.display()))?;
        println!(
            "model written to {} ({} samples)",
            p.display(),
            model.total_samples()
        );
    }
    Ok(())
}

fn print_report(r: &BacktestReport) {
    println!("fidelity        {:?}", r.fidelity);
    println!("events          {}", r.events);
    println!(
        "decisions       {} (approvals {}, fills {})",
        r.decisions, r.approvals, r.fills
    );
    println!("settled markets {}", r.settled_markets);
    println!("realized PnL    {}", r.realized_pnl);
    println!("days +/−        {} / {}", r.winning_days, r.losing_days);
    println!("max drawdown    {}", r.max_drawdown);
    println!(
        "mean daily PnL  95% bootstrap CI [{:.3}, {:.3}]",
        r.mean_daily_pnl_ci.0, r.mean_daily_pnl_ci.1
    );
    for (s, p) in &r.pnl_by_strategy {
        println!("  {s:<24} {p}");
    }
}

async fn backtest(
    config: Option<PathBuf>,
    synthetic_days: Option<u32>,
    journal: Option<String>,
    seed: u64,
    model: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<()> {
    let cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    let loc = cfg.locations.first().context("no location configured")?;
    let ids = setup::location_ids(loc)?;
    let (events, fidelity, model): (
        Vec<EventEnvelope>,
        Fidelity,
        Arc<dyn wm_strategy::ProbabilityModel>,
    ) = match (synthetic_days, journal) {
        (Some(days), None) => {
            let start = wm_core::time::local_date(Utc::now(), ids.timezone)
                - Duration::days(i64::from(days.max(1)));
            let model: Arc<dyn wm_strategy::ProbabilityModel> = match &model {
                Some(p) => Arc::new(setup::read_model(p)?),
                None => demo::train_model(&ids, setup::peak_config(loc), start, 240, seed),
            };
            let events = (0..days.max(1))
                .flat_map(|i| {
                    demo::day_events(
                        &ids,
                        start + Duration::days(i64::from(i)),
                        seed,
                        u64::from(i),
                    )
                })
                .collect();
            println!(
                "synthetic backtest: {days} day(s) from {start} — plumbing test, NOT evidence of edge"
            );
            (events, Fidelity::Synthetic, model)
        }
        (None, Some(run)) => {
            let run: RunId =
                serde_json::from_value(serde_json::Value::String(run.trim().to_owned()))
                    .context("run id must be a UUID")?;
            let url = cfg
                .env
                .database_url
                .as_deref()
                .context("WM_DATABASE_URL is required to load a journal")?;
            let store = PgStore::connect(url, 2).await?;
            let mut events = store.load_journal(&run).await?;
            // Execution is re-simulated: drop recorded order updates.
            events.retain(|e| !matches!(e.event, WeatherMachineEvent::OrderUpdate(_)));
            let has_books = events
                .iter()
                .any(|e| matches!(e.event, WeatherMachineEvent::OrderBookUpdate(_)));
            let model: Arc<dyn wm_strategy::ProbabilityModel> = match &model {
                Some(p) => Arc::new(setup::read_model(p)?),
                None => setup::load_model(&cfg)?,
            };
            println!("journal backtest of run {run}: {} events", events.len());
            (
                events,
                if has_books {
                    Fidelity::TrueOrderBook
                } else {
                    Fidelity::PriceOnly
                },
                model,
            )
        }
        _ => bail!("choose exactly one of --synthetic-days N or --journal RUN_ID"),
    };
    let bt = BacktestConfig {
        engine: setup::engine_config(&cfg, RunMode::Backtest, RunId::deterministic(seed))?,
        sim: SimConfig::default(),
        fidelity,
        settle_grace: Duration::hours(2),
        heartbeat: Duration::minutes(15),
    };
    let (report, _) = tokio::task::spawn_blocking(move || run_backtest(events, &bt, model))
        .await
        .context("backtest task")?;
    print_report(&report);
    if let Some(p) = out {
        std::fs::write(&p, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("writing {}", p.display()))?;
        println!("report written to {}", p.display());
    }
    Ok(())
}

async fn discover(config: Option<PathBuf>, date: Option<NaiveDate>) -> Result<()> {
    let cfg = AppConfig::load(config.as_deref())?;
    telemetry::init_tracing(&cfg.file.app.log_format);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let ua = cfg
        .user_agent()
        .unwrap_or_else(|_| format!("WeatherMachine/{} (market-discovery)", wm_core::VERSION));
    let providers = setup::Providers::build(&cfg, Arc::clone(&clock), &ua)?;
    let fetcher = providers
        .fetcher("polymarket_gamma")
        .context("providers.polymarket_gamma is disabled")?;
    let gamma = GammaClient::new(
        Arc::clone(fetcher),
        &cfg.file.providers.polymarket_gamma.base_url,
    );
    for l in &cfg.locations {
        let spec = setup::market_spec(l)?;
        let d = date.unwrap_or_else(|| wm_core::time::local_date(clock.now(), spec.timezone));
        let slug = event_slug(&spec.slug_template, d);
        println!("== {} {} ({slug})", l.location.id, d);
        let (events, _) = gamma
            .events_by_slug(&slug, std::time::Duration::from_secs(30))
            .await?;
        let Some(ev) = events.iter().find(|e| e.slug == slug) else {
            println!("   no event with this slug");
            continue;
        };
        match build_market(ev, &spec, d, clock.now()) {
            Ok(m) => {
                println!(
                    "   {} — {} outcome(s), neg_risk {}, end {:?}",
                    m.title,
                    m.outcomes.len(),
                    m.neg_risk,
                    m.end_time
                );
                println!("   resolution: {:?}", m.resolution.source);
                println!(
                    "   filters: {:?} (certainty {:?})",
                    m.resolution
                        .filters
                        .iter()
                        .map(ObservationFilter::label)
                        .collect::<Vec<_>>(),
                    m.resolution.filter_certainty
                );
                println!(
                    "   machine tradable: {} · unrecognized clauses: {:?}",
                    m.resolution.is_machine_tradable(),
                    m.resolution.unrecognized_clauses
                );
                println!("   rules sha256: {}", m.rules.sha256);
                for o in m.sorted_outcomes() {
                    println!(
                        "   {:>12}  yes {}  no {}  tick {}  min {}",
                        o.label, o.yes_token, o.no_token, o.tick_size, o.min_order_size
                    );
                }
            }
            Err(e) => println!("   mapping failed (would not be traded): {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_market_takes_repeated_days() {
        let cli = Cli::try_parse_from([
            "weather-machine",
            "research",
            "market",
            "--from",
            "2026-09-01",
            "--day",
            "2026-09-28",
            "--day",
            "2026-09-27",
            "--print",
        ])
        .unwrap();
        let Some(Command::Research {
            command: ResearchCommand::Market {
                from, days, print, ..
            },
        }) = cli.command
        else {
            panic!("not research market");
        };
        assert_eq!(from, NaiveDate::from_ymd_opt(2026, 9, 1));
        assert_eq!(
            days,
            vec![
                NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(),
                NaiveDate::from_ymd_opt(2026, 9, 27).unwrap()
            ]
        );
        assert!(print);
        assert!(
            Cli::try_parse_from([
                "weather-machine",
                "research",
                "market",
                "--day",
                "28-09-2026"
            ])
            .is_err()
        );
    }
}
