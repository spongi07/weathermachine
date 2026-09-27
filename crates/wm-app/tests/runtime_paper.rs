#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The paper runtime wired against mock NOAA AWC / Polymarket Gamma / CLOB
//! servers, without a database: market discovery, observation ingestion,
//! REST book fallback, dashboard publishing, operator kill switch, graceful
//! shutdown — and fail-closed trading because audit storage is missing.

use chrono::{Duration, Timelike, Utc};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, watch};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_app::config::{AppConfig, EnvSettings};
use wm_app::http::Publisher;
use wm_app::runtime::{self, RuntimeContext};
use wm_backtest::{StudyConfig, study, synthetic_history};
use wm_core::event::OperatorCommand;
use wm_core::ids::StationId;
use wm_core::resolution::ObservationFilter;
use wm_strategy::PeakConfig;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// AWC JSON with half-hourly EHAM METARs for the last 26 hours.
fn awc_body() -> String {
    let now = Utc::now();
    let mut t = (now - Duration::hours(26))
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    t = t - Duration::minutes(i64::from(t.minute())) + Duration::minutes(25);
    let mut entries = Vec::new();
    while t + Duration::minutes(3) < now {
        let hour = f64::from(t.hour());
        let temp =
            (14.0 + 6.0 * ((hour - 14.0) / 24.0 * std::f64::consts::TAU).cos()).round() as i32;
        let raw = format!(
            "METAR EHAM {} 24010KT 9999 FEW030 {temp:02}/08 Q1016 NOSIG",
            t.format("%d%H%MZ")
        );
        entries.push(format!(r#"{{"icaoId":"EHAM","receiptTime":"{}","obsTime":{},"temp":{temp},"metarType":"METAR","rawOb":"{raw}"}}"#, (t + Duration::minutes(2)).format("%Y-%m-%d %H:%M:%S"), t.timestamp()));
        t += Duration::minutes(30);
    }
    entries.reverse();
    format!("[{}]", entries.join(","))
}

fn gamma_body(slug: &str) -> String {
    let mut markets = Vec::new();
    let mut add = |title: &str, i: u32| {
        markets.push(format!(
            r#"{{"id":"m{i}","question":"Will the highest temperature in Amsterdam be {title}?","conditionId":"0xcond{i}","questionID":"0xq{i}","slug":"ams-{i}","groupItemTitle":"{title}","outcomes":"[\"Yes\", \"No\"]","outcomePrices":"[\"0.5\", \"0.5\"]","clobTokenIds":"[\"{yes}\", \"{no}\"]","active":true,"closed":false,"acceptingOrders":true,"orderPriceMinTickSize":0.01,"orderMinSize":5,"negRisk":true}}"#,
            yes = 1000 + i,
            no = 2000 + i
        ));
    };
    add("7°C or below", 7);
    for v in 8..=21 {
        add(&format!("{v}°C"), v);
    }
    add("22°C or higher", 22);
    let rules = r#"This market will resolve to the temperature range that contains the highest temperature recorded by NOAA at the Amsterdam Airport Schiphol Station in degrees Celsius. The resolution source for this market will be information from NOAA, specifically the highest reading under the \"Temp\" column for all times on the specified day, available here: https://www.weather.gov/wrh/timeseries?site=eham. The resolution source for this market measures temperatures to whole degrees Celsius (eg, 9°C), which is the level of precision that will be used when resolving the market."#;
    format!(
        r#"[{{"id":"e1","slug":"{slug}","title":"Highest temperature in Amsterdam today?","description":"{rules}","resolutionSource":"https://www.weather.gov/wrh/timeseries?site=eham","active":true,"closed":false,"negRisk":true,"markets":[{}]}}]"#,
        markets.join(",")
    )
}

/// Open-Meteo day-1 series: 72 hours from yesterday 00:00 UTC.
fn open_meteo_body() -> String {
    let start = (Utc::now().date_naive() - Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let times: Vec<String> = (0..72)
        .map(|h| {
            format!(
                "\"{}\"",
                (start + Duration::hours(h)).format("%Y-%m-%dT%H:%M")
            )
        })
        .collect();
    let values: Vec<String> = (0..72)
        .map(|h| {
            let hour = f64::from(h % 24);
            format!(
                "{:.1}",
                15.0 + 5.0 * ((hour - 14.0) / 24.0 * std::f64::consts::TAU).cos()
            )
        })
        .collect();
    format!(
        r#"{{"utc_offset_seconds":0,"timezone":"GMT","hourly_units":{{"temperature_2m_previous_day1":"°C"}},"hourly":{{"time":[{}],"temperature_2m_previous_day1":[{}]}}}}"#,
        times.join(","),
        values.join(",")
    )
}

fn write_model(dir: &std::path::Path) -> PathBuf {
    let st = StationId::new("EHAM").unwrap();
    let hist = synthetic_history(
        &st,
        Utc::now().date_naive() - Duration::days(100),
        90,
        5,
        Duration::minutes(4),
    );
    let cfg = StudyConfig {
        station: st,
        tz: chrono_tz::Europe::Amsterdam,
        filter: ObservationFilter::AllRows,
        peak: PeakConfig::default(),
        k_classes: 4,
        min_high_local_minute: 9 * 60,
    };
    let (_, model) = study(&hist, &cfg);
    let p = dir.join("model.json");
    std::fs::write(&p, serde_json::to_vec(&model).unwrap()).unwrap();
    p
}

struct MockEnv {
    server: MockServer,
    cfg: AppConfig,
    today_slug: String,
    tmp: PathBuf,
}

async fn mock_env(tag: &str) -> MockEnv {
    let server = MockServer::start().await;
    let mut cfg = AppConfig::load(Some(&repo().join("configs/weather-machine.toml"))).unwrap();
    let tz = chrono_tz::Europe::Amsterdam;
    let today_slug = wm_polymarket::event_slug(
        &cfg.locations[0].market.event_slug_template,
        wm_core::time::local_date(Utc::now(), tz),
    );
    Mock::given(method("GET"))
        .and(path("/api/data/metar"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(awc_body(), "application/json"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/events"))
        .and(query_param("slug", today_slug.as_str()))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(gamma_body(&today_slug), "application/json"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .and(query_param("hourly", "temperature_2m_previous_day1"))
        .and(query_param("models", "gfs_global"))
        .and(query_param("timezone", "GMT"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(open_meteo_body(), "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/book"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"bids":[{"price":"0.45","size":"200"}],"asks":[{"price":"0.47","size":"200"}],"tick_size":"0.01","min_order_size":"5"}"#, "application/json"))
        .mount(&server)
        .await;
    let tmp = std::env::temp_dir().join(format!("wm-runtime-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let p = &mut cfg.file.providers;
    p.awc.base_url = server.uri();
    p.tgftp.enabled = false;
    p.nws_api.enabled = false;
    p.polymarket_gamma.base_url = server.uri();
    p.polymarket_clob.base_url = server.uri();
    p.polymarket_ws.enabled = false;
    // Tests never contact the real IEM archive or Open-Meteo.
    p.iem.enabled = false;
    p.open_meteo.base_url = server.uri();
    p.open_meteo.policy = wm_net::RateLimitPolicy::local_test();
    p.open_meteo.policy.max_body_bytes = 4 * 1024 * 1024;
    cfg.locations[0].observation_sources.secondary.clear();
    cfg.file.app.snapshot_interval_ms = 100;
    cfg.file.app.heartbeat_secs = 5;
    cfg.file.model.path = Some(write_model(&tmp).to_string_lossy().into_owned());
    cfg.env = EnvSettings {
        contact: Some("runtime-test@example.invalid".into()),
        ..EnvSettings::default()
    };
    MockEnv {
        server,
        cfg,
        today_slug,
        tmp,
    }
}

struct Running {
    snaps: watch::Receiver<Arc<wm_app::http::Published>>,
    cmd_tx: mpsc::Sender<OperatorCommand>,
    ready: Arc<AtomicBool>,
    shutdown_tx: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(cfg: AppConfig) -> Running {
    let (publisher, snaps) = Publisher::new();
    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let ready = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(runtime::run(
        cfg,
        RuntimeContext {
            publisher,
            commands: cmd_rx,
            ready: Arc::clone(&ready),
            shutdown: shutdown_rx,
        },
    ));
    Running {
        snaps,
        cmd_tx,
        ready,
        shutdown_tx,
        handle,
    }
}

impl Running {
    /// Wait until `cond` holds for a published snapshot.
    async fn until(
        &mut self,
        what: &str,
        secs: u64,
        cond: impl Fn(&wm_dashboard_api::DashboardSnapshot) -> bool,
    ) -> Arc<wm_app::http::Published> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let changed = tokio::time::timeout_at(deadline, self.snaps.changed()).await;
            let p = Arc::clone(&self.snaps.borrow());
            if cond(&p.snapshot) {
                return p;
            }
            assert!(
                changed.is_ok() && !self.handle.is_finished(),
                "{what}: runtime did not converge; last snapshot: {:#?}",
                p.snapshot
            );
        }
    }

    async fn stop(self) {
        self.shutdown_tx.send(true).unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(15), self.handle)
            .await
            .expect("graceful shutdown")
            .unwrap();
        assert!(res.is_ok(), "{res:?}");
    }
}

fn converged(s: &wm_dashboard_api::DashboardSnapshot) -> bool {
    let loc = s.locations.first();
    let ingested = loc
        .and_then(|l| l.collector.as_ref())
        .is_some_and(|c| c.new_observations_total >= 40);
    let books = loc.and_then(|l| l.market.as_ref()).is_some_and(|m| {
        m.rows.len() == 16 && m.rows.iter().filter(|r| r.yes_bid.is_some()).count() >= 8
    });
    ingested && books
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paper_runtime_end_to_end_without_storage_fails_closed() {
    let env = mock_env("nodb").await;
    let mut run = start(env.cfg.clone());
    let snap = run.until("ingest + discovery + books", 30, converged).await;
    let s = &snap.snapshot;
    assert!(run.ready.load(Ordering::Acquire));
    assert_eq!(s.mode, "paper");
    assert!(!s.demo);
    assert!(!s.storage_ok, "no database configured");
    assert!(s.model_id.starts_with("empirical-EHAM"));
    let loc = &s.locations[0];
    let market = loc.market.as_ref().unwrap();
    assert_eq!(market.event_slug, env.today_slug);
    assert!(
        market.machine_tradable,
        "standard WRH rules text is machine-tradable: {:?}",
        market.unrecognized_clauses
    );
    assert!(s.providers.iter().any(|p| p.provider == "awc"
        && p.scope.as_deref() == Some("EHAM")
        && p.requests_today >= 1));
    assert!(
        s.risk
            .checks
            .iter()
            .any(|c| c.name == "Audit storage" && !c.ok)
    );
    // Fail closed: whatever the strategies think, nothing is approved without audit storage.
    assert_eq!(s.engine.approvals_total, 0);
    assert!(s.decisions.iter().all(|d| !d.approved));
    assert!(s.positions.is_empty());
    // Exactly one AWC request (NOAA floor: ≥ 30 s between requests), and it
    // backfilled the whole local day.
    let awc: Vec<_> = env
        .server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/api/data/metar")
        .collect();
    assert_eq!(awc.len(), 1);
    assert!(awc[0].url.query().unwrap().contains("hours=26"));
    assert!(
        awc[0]
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("runtime-test@example.invalid")
    );

    // The day-1 forecast arrives and is shown — but this model was trained
    // without it, so it cannot influence anything.
    let snap = run
        .until("forecast received", 20, |s| {
            s.locations
                .first()
                .and_then(|l| l.forecast.as_ref())
                .is_some()
        })
        .await;
    let f = snap.snapshot.locations[0].forecast.clone().unwrap();
    assert_eq!(f.product, "open_meteo/gfs_global/d1");
    assert!(
        !f.in_use && f.status.contains("does not use forecasts"),
        "{f:?}"
    );
    assert!(f.hourly.len() >= 23, "{}", f.hourly.len());
    assert!(f.day_max_c.is_some());
    assert!(
        snap.snapshot
            .providers
            .iter()
            .any(|p| p.provider == "open_meteo" && p.requests_today >= 1)
    );

    // Operator kill switch reaches the kernel.
    run.cmd_tx
        .send(OperatorCommand::KillSwitch {
            engaged: true,
            reason: "test halt".into(),
        })
        .await
        .unwrap();
    run.until("kill switch", 10, |s| {
        s.kill_switch.as_deref() == Some("test halt")
    })
    .await;
    run.stop().await;
    let _ = std::fs::remove_dir_all(&env.tmp);
}

async fn fresh_db() -> Option<(String, String, String)> {
    let Ok(admin_url) = std::env::var("WM_TEST_DATABASE_URL") else {
        eprintln!("WM_TEST_DATABASE_URL not set; skipping PostgreSQL runtime test");
        return None;
    };
    let admin = wm_storage::PgStore::connect(&admin_url, 2)
        .await
        .expect("connect admin");
    let name = format!(
        "wm_rt_{:x}{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        std::process::id()
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(admin.pool())
        .await
        .expect("create db");
    admin.pool().close().await;
    let (base, _) = admin_url.rsplit_once('/').unwrap();
    Some((format!("{base}/{name}"), admin_url, name))
}

async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paper_runtime_with_postgres_persists_and_warm_starts() {
    let Some((db_url, admin_url, db_name)) = fresh_db().await else {
        return;
    };
    let mut env = mock_env("pg").await;
    env.cfg.env.database_url = Some(db_url.clone());
    env.cfg.file.app.auto_migrate = true;

    // First run: migrate, ingest, discover, journal.
    let mut run = start(env.cfg.clone());
    let snap = run
        .until("first run", 40, |s| converged(s) && s.storage_ok)
        .await;
    assert!(snap.snapshot.storage_ok);
    // The lease is exclusive while the collector runs.
    let other = wm_storage::PgStore::connect(&db_url, 2).await.unwrap();
    assert!(
        other
            .try_station_lease(&StationId::new("EHAM").unwrap())
            .await
            .unwrap()
            .is_none(),
        "second instance must not poll EHAM"
    );
    run.stop().await;

    let pool = other.pool();
    assert_eq!(count(pool, "SELECT count(*) FROM strategy_runs").await, 1);
    assert!(
        count(
            pool,
            "SELECT count(*) FROM weather_observations WHERE station_id = 'EHAM'"
        )
        .await
            >= 40
    );
    assert!(count(pool, "SELECT count(*) FROM provider_requests").await >= 1);
    assert!(count(pool, "SELECT count(*) FROM markets").await >= 1);
    // Every engine input is journaled (replayable with `weather-machine backtest --journal`).
    assert!(
        count(
            pool,
            "SELECT count(*) FROM event_journal WHERE kind = 'weather_observation'"
        )
        .await
            >= 40
    );
    assert!(
        count(
            pool,
            "SELECT count(*) FROM event_journal WHERE kind = 'market_snapshot'"
        )
        .await
            >= 1
    );
    assert!(
        count(
            pool,
            "SELECT count(*) FROM event_journal WHERE kind = 'order_book_update'"
        )
        .await
            >= 1
    );
    assert!(
        count(
            pool,
            "SELECT count(*) FROM event_journal WHERE kind = 'provider_health_changed'"
        )
        .await
            >= 1
    );
    assert_eq!(count(pool, "SELECT count(*) FROM (SELECT seq FROM event_journal GROUP BY run_id, seq HAVING count(*) > 1) d").await, 0);
    assert!(
        count(pool, "SELECT count(*) FROM orderbook_snapshots").await >= 1,
        "books are sampled for future backtests"
    );
    let observations_before = count(pool, "SELECT count(*) FROM weather_observations").await;

    // Second run: the lease was released, the day is rebuilt from storage, and
    // the provider's repeated reports are recognised as duplicates.
    let mut run = start(env.cfg.clone());
    let snap = run
        .until("warm restart", 40, |s| {
            let c = s.locations.first().and_then(|l| l.collector.as_ref());
            c.is_some_and(|c| c.polls_total >= 1) && s.storage_ok
        })
        .await;
    let c = snap.snapshot.locations[0].collector.as_ref().unwrap();
    assert_eq!(
        c.new_observations_total, 0,
        "warm-started ledger dedups the backfill"
    );
    assert!(c.duplicates_total >= 40);
    assert!(
        !snap.snapshot.locations[0].series.is_empty()
            || wm_core::time::local_minute_of_day(Utc::now(), chrono_tz::Europe::Amsterdam) < 30,
        "engine day state rebuilt from storage"
    );
    run.stop().await;
    assert_eq!(
        count(pool, "SELECT count(*) FROM weather_observations").await,
        observations_before,
        "no duplicate rows"
    );
    assert_eq!(count(pool, "SELECT count(*) FROM strategy_runs").await, 2);

    other.pool().close().await;
    let admin = wm_storage::PgStore::connect(&admin_url, 2).await.unwrap();
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    )))
    .execute(admin.pool())
    .await;
    let _ = std::fs::remove_dir_all(&env.tmp);
}

/// No model file: the service keeps running without one (no weather trades),
/// trains from the (mock) IEM archive and forecast history, evaluates the
/// forecast, installs the model and swaps it into the running engine — no
/// restart. CSV rows and forecast values are synthetic fixtures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_model_is_trained_from_history_and_swapped_in_without_restart() {
    use chrono::Datelike;
    let mut env = mock_env("train").await;
    let st = StationId::new("EHAM").unwrap();
    let today = Utc::now().date_naive();
    let from = today - Duration::days(200);
    let mut by_year: std::collections::BTreeMap<i32, String> = Default::default();
    for o in synthetic_history(&st, from, 200, 5, Duration::minutes(4)) {
        by_year
            .entry(o.key.observed_at.year())
            .or_insert_with(|| "station,valid,metar\n".to_owned())
            .push_str(&format!(
                "EHAM,{},{}\n",
                o.key.observed_at.format("%Y-%m-%d %H:%M"),
                o.raw_text
            ));
    }
    for (y, body) in by_year {
        Mock::given(method("GET"))
            .and(path("/cgi-bin/request/asos.py"))
            .and(query_param("year1", y.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&env.server)
            .await;
    }
    let data = env.tmp.join("data");
    env.cfg.file.model.path = None;
    env.cfg.file.model.auto_train = wm_app::config::AutoTrainSection {
        enabled: true,
        data_dir: data.to_string_lossy().into_owned(),
        from_year: from.year(),
        min_days: 30,
        retry_after_secs: 3600,
        retrain_after_days: 30,
    };
    env.cfg.file.forecast.history_from = from;
    let iem = &mut env.cfg.file.providers.iem;
    iem.enabled = true;
    iem.base_url = env.server.uri();
    iem.policy = wm_net::RateLimitPolicy::local_test();
    iem.policy.max_body_bytes = 16 * 1024 * 1024;

    let mut run = start(env.cfg.clone());
    let snap = run
        .until("model trained and swapped in", 120, |s| {
            s.model.state == "loaded" && s.model_id.starts_with("empirical-EHAM-all-")
        })
        .await;
    assert!(!run.handle.is_finished(), "no restart needed");
    let model_file = data.join("models").join("eham.json");
    assert!(model_file.is_file());
    let report = std::fs::read_to_string(data.join("research").join("eham-survival.md")).unwrap();
    assert!(report.contains("## Forecast evaluation"), "{report}");
    assert!(
        snap.snapshot
            .risk
            .checks
            .iter()
            .any(|c| c.name == "Probability model" && c.ok)
    );
    // Three days of forecasts are far too few: evaluated, not adopted.
    let verdict = snap.snapshot.model.forecast.clone().unwrap_or_default();
    assert!(verdict.starts_with("not adopted"), "{verdict}");
    let m = wm_app::setup::read_model(&model_file).unwrap();
    let info = m.forecast.unwrap();
    assert!(info.evaluated && !info.adopted);
    assert!(
        !m.levels
            .iter()
            .flatten()
            .any(|d| *d == wm_strategy::probability::FeatureDim::ForecastRise)
    );
    run.stop().await;
    let _ = std::fs::remove_dir_all(&env.tmp);
}
