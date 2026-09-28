#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Model training from IEM history against a mock archive: one request per
//! station-year, finished years cached, all-or-nothing installation and the
//! minimum-data gate. The CSV rows are synthetic test fixtures.

use chrono::{Datelike, Duration, NaiveDate};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use wm_app::training::{self, ForecastPlan, Progress, TrainPlan};
use wm_backtest::synthetic_history;
use wm_core::ids::{ProviderId, StationId};
use wm_core::time::SystemClock;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_strategy::PeakConfig;
use wm_weather::{IemArchive, OpenMeteoPreviousRuns};

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

/// IEM-format CSV bodies per year (UTC), from synthetic observations.
fn csv_by_year(from: NaiveDate, days: u32) -> BTreeMap<i32, String> {
    let mut out: BTreeMap<i32, String> = BTreeMap::new();
    for o in synthetic_history(&eham(), from, days, 11, Duration::minutes(4)) {
        let body = out
            .entry(o.key.observed_at.year())
            .or_insert_with(|| "station,valid,metar\n".to_owned());
        body.push_str(&format!(
            "EHAM,{},{}\n",
            o.key.observed_at.format("%Y-%m-%d %H:%M"),
            o.raw_text
        ));
    }
    out
}

async fn mount(server: &MockServer, year: i32, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/cgi-bin/request/asos.py"))
        .and(query_param("station", "EHAM"))
        .and(query_param("year1", year.to_string().as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

fn archive(uri: &str) -> IemArchive {
    let mut policy = RateLimitPolicy::local_test();
    policy.max_body_bytes = 16 * 1024 * 1024; // a station-year is a few MB
    let gate = ProviderGate::new(ProviderId::iem(), policy, Arc::new(SystemClock::new()), 3);
    IemArchive::new(
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap()),
        uri,
    )
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("wm-train-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn plan(dir: &Path, min_days: u64) -> TrainPlan {
    TrainPlan {
        station: eham(),
        tz: chrono_tz::Europe::Amsterdam,
        peak: PeakConfig::default(),
        from_year: 2024,
        today: d(2025, 4, 19),
        cache_dir: dir.join("research/iem/EHAM"),
        model_out: dir.join("models/eham.json"),
        report_out: dir.join("research/eham-survival.md"),
        min_days,
        forecast: None,
        // Short synthetic history: a short burn-in so the comparison scores days.
        selection: Some(wm_backtest::SelectionConfig {
            burn_in_days: 30,
            min_days: 30,
            bootstrap_iterations: 200,
            ..wm_backtest::SelectionConfig::default()
        }),
    }
}

async fn run(
    archive: &IemArchive,
    plan: &TrainPlan,
) -> (anyhow::Result<training::TrainOutcome>, Vec<Progress>) {
    run_with(archive, None, plan).await
}

async fn run_with(
    archive: &IemArchive,
    forecast: Option<&OpenMeteoPreviousRuns>,
    plan: &TrainPlan,
) -> (anyhow::Result<training::TrainOutcome>, Vec<Progress>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = Arc::clone(&seen);
    let progress = move |p: Progress| s2.lock().unwrap().push(p);
    let (_stop, mut stop_rx) = watch::channel(false);
    let r = training::train(archive, forecast, plan, None, &progress, &mut stop_rx).await;
    let v = seen.lock().unwrap().clone();
    (r, v)
}

fn open_meteo(uri: &str) -> OpenMeteoPreviousRuns {
    let mut policy = RateLimitPolicy::local_test();
    policy.max_body_bytes = 4 * 1024 * 1024;
    let gate = ProviderGate::new(
        ProviderId::open_meteo(),
        policy,
        Arc::new(SystemClock::new()),
        4,
    );
    OpenMeteoPreviousRuns::new(
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap()),
        uri,
        None,
        "gfs_global",
        1,
    )
}

/// Hourly day-1 series for `[start, end]` (UTC days), a plain diurnal curve.
fn om_body(start: NaiveDate, end: NaiveDate) -> String {
    let (mut times, mut values) = (Vec::new(), Vec::new());
    let mut t = start.and_hms_opt(0, 0, 0).unwrap();
    while t.date() <= end {
        let hour = f64::from(t.format("%H").to_string().parse::<u32>().unwrap());
        times.push(format!("\"{}\"", t.format("%Y-%m-%dT%H:%M")));
        values.push(format!(
            "{:.1}",
            12.0 + 4.0 * ((hour - 13.0) / 24.0 * std::f64::consts::TAU).cos()
        ));
        t += Duration::hours(1);
    }
    format!(
        r#"{{"utc_offset_seconds":0,"timezone":"GMT","hourly_units":{{"temperature_2m_previous_day1":"°C"}},"hourly":{{"time":[{}],"temperature_2m_previous_day1":[{}]}}}}"#,
        times.join(","),
        values.join(",")
    )
}

async fn mount_om(server: &MockServer, start: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .and(query_param("start_date", start))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn forecast_history_is_fetched_newest_first_cached_and_evaluated() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    let server = MockServer::start().await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    let ok = |a: NaiveDate, b: NaiveDate| {
        ResponseTemplate::new(200).set_body_raw(om_body(a, b), "application/json")
    };
    // 2025 (up to yesterday) is served; the archive starts on 1 October 2024.
    mount_om(&server, "2025-01-01", ok(d(2025, 1, 1), d(2025, 4, 18))).await;
    mount_om(
        &server,
        "2024-01-01",
        ResponseTemplate::new(400).set_body_string(
            r#"{"error":true,"reason":"Parameter 'start_date' is out of allowed range from 2024-10-01 to 2025-04-26"}"#,
        ),
    )
    .await;
    mount_om(&server, "2024-10-01", ok(d(2024, 10, 1), d(2024, 12, 31))).await;
    let dir = tempdir("forecast");
    let mut plan = plan(&dir, 30);
    plan.forecast = Some(ForecastPlan {
        product: wm_core::forecast::ForecastProduct {
            provider: ProviderId::open_meteo(),
            model: "gfs_global".into(),
            lead_days: 1,
            ready_local_minute: 480,
        },
        latitude: 52.3156,
        longitude: 4.7903,
        from: d(2021, 3, 1),
        cache_dir: dir.join("research/open-meteo/EHAM/gfs_global-d1"),
        eval: wm_backtest::EvaluationConfig {
            min_days: 30,
            bootstrap_iterations: 200,
            ..wm_backtest::EvaluationConfig::default()
        },
    });
    let om = open_meteo(&server.uri());
    let iem = archive(&server.uri());

    let (r, progress) = run_with(&iem, Some(&om), &plan).await;
    let o = r.unwrap();
    let verdict = o.forecast_verdict.clone().unwrap();
    assert!(
        verdict.starts_with("adopted") || verdict.starts_with("not adopted"),
        "{verdict}"
    );
    assert!(
        progress
            .iter()
            .any(|p| matches!(p, Progress::Forecast { year: 2025, .. }))
    );
    let requests = |server_reqs: &[wiremock::Request], start: &str| {
        server_reqs
            .iter()
            .filter(|r| r.url.path() == "/v1/forecast")
            .filter(|r| {
                r.url
                    .query_pairs()
                    .any(|(k, v)| k == "start_date" && v == start)
            })
            .count()
    };
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        (
            requests(&reqs, "2025-01-01"),
            requests(&reqs, "2024-01-01"),
            requests(&reqs, "2024-10-01")
        ),
        (1, 1, 1)
    );
    assert!(
        reqs.iter()
            .filter(|r| r.url.path() == "/v1/forecast")
            .all(|r| {
                r.url
                    .query_pairs()
                    .any(|(k, v)| k == "hourly" && v == "temperature_2m_previous_day1")
            })
    );
    let cache = dir.join("research/open-meteo/EHAM/gfs_global-d1");
    assert!(cache.join("2024.json").is_file(), "finished year cached");
    assert!(cache.join("2025-partial.json").is_file());
    assert_eq!(
        std::fs::read_to_string(cache.join("archive-start.txt")).unwrap(),
        "2024-10-01"
    );
    let model = wm_app::setup::read_model(&plan.model_out).unwrap();
    let info = model.forecast.clone().unwrap();
    assert!(info.evaluated, "{}", info.verdict);
    assert!(info.days_with_forecast > 150, "{}", info.days_with_forecast);
    assert_eq!(model.uses_forecast(), info.adopted);
    let report = std::fs::read_to_string(&plan.report_out).unwrap();
    assert!(
        report.contains("## Forecast evaluation") && report.contains("Placebo control"),
        "{report}"
    );
    assert!(
        report.contains("open_meteo/gfs_global/d1")
            && report.contains("forecast archive starts 2024-10-01")
    );

    // Retraining: 2024 comes from the cache, years before the archive are
    // not asked for again, only the current year is refreshed.
    let (r, _) = run_with(&iem, Some(&om), &plan).await;
    r.unwrap();
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        (
            requests(&reqs, "2025-01-01"),
            requests(&reqs, "2024-01-01"),
            requests(&reqs, "2024-10-01")
        ),
        (2, 1, 1)
    );
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path() == "/v1/forecast")
            .count(),
        4
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rejected_forecast_model_is_reported_with_the_api_reason() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    let server = MockServer::start().await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":true,"reason":"Cannot initialize WeatherModel from invalid String value gfs_globl for key models"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempdir("forecast-rejected");
    let mut plan = plan(&dir, 30);
    plan.forecast = Some(ForecastPlan {
        product: wm_core::forecast::ForecastProduct {
            provider: ProviderId::open_meteo(),
            model: "gfs_globl".into(),
            lead_days: 1,
            ready_local_minute: 480,
        },
        latitude: 52.3,
        longitude: 4.8,
        from: d(2021, 3, 1),
        cache_dir: dir.join("research/open-meteo/EHAM/gfs_globl-d1"),
        eval: wm_backtest::EvaluationConfig::default(),
    });
    let (r, _) = run_with(
        &archive(&server.uri()),
        Some(&open_meteo(&server.uri())),
        &plan,
    )
    .await;
    let o = r.unwrap();
    let verdict = o.forecast_verdict.unwrap();
    assert!(
        verdict.contains("invalid String value gfs_globl"),
        "{verdict}"
    );
    let model = wm_app::setup::read_model(&plan.model_out).unwrap();
    assert!(!model.uses_forecast());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unreachable_forecast_source_leaves_the_model_without_it() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    let server = MockServer::start().await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    // Open-Meteo answers 503 (no mock for /v1/forecast would be 404).
    Mock::given(method("GET"))
        .and(path("/v1/forecast"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dir = tempdir("forecast-down");
    let mut plan = plan(&dir, 30);
    plan.selection = None;
    plan.forecast = Some(ForecastPlan {
        product: wm_core::forecast::ForecastProduct {
            provider: ProviderId::open_meteo(),
            model: "gfs_global".into(),
            lead_days: 1,
            ready_local_minute: 480,
        },
        latitude: 52.3,
        longitude: 4.8,
        from: d(2021, 3, 1),
        cache_dir: dir.join("research/open-meteo/EHAM/gfs_global-d1"),
        eval: wm_backtest::EvaluationConfig::default(),
    });
    let (r, _) = run_with(
        &archive(&server.uri()),
        Some(&open_meteo(&server.uri())),
        &plan,
    )
    .await;
    let o = r.unwrap();
    assert!(!o.forecast_adopted);
    assert!(o.structure_verdict.is_none() && !o.candidate_adopted);
    let model = wm_app::setup::read_model(&plan.model_out).unwrap();
    assert!(!model.uses_forecast());
    assert!(model.selection.is_none(), "no comparison ran");
    let info = model.forecast.unwrap();
    assert!(!info.evaluated, "retry later");
    assert!(
        info.verdict.starts_with("not evaluated"),
        "{}",
        info.verdict
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn trains_from_history_and_downloads_each_finished_year_once() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    assert_eq!(years.keys().copied().collect::<Vec<_>>(), vec![2024, 2025]);
    let server = MockServer::start().await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    let dir = tempdir("ok");
    let iem = archive(&server.uri());
    let plan = plan(&dir, 30);

    let (r, progress) = run(&iem, &plan).await;
    let o = r.unwrap();
    assert!(o.days >= 150, "{o:?}");
    assert!(o.samples > 0);
    assert_eq!((o.years_downloaded, o.years_cached), (2, 0));
    assert!(
        o.model_id.starts_with("empirical-EHAM-all-")
            && o.model_id.ends_with("-20250419")
            && o.model_id.contains("-first-reach-") == o.candidate_adopted,
        "{}",
        o.model_id
    );
    assert!(matches!(
        progress.first(),
        Some(Progress::Downloading {
            year: 2024,
            done: 0,
            total: 2
        })
    ));
    assert!(
        progress
            .iter()
            .any(|p| matches!(p, Progress::Training { .. }))
    );
    // Installed where the service looks for it, and loadable.
    let model = wm_app::setup::read_model(&plan.model_out).unwrap();
    assert_eq!(model.station, "EHAM");
    assert_eq!(model.id, o.model_id);
    let report = std::fs::read_to_string(&plan.report_out).unwrap();
    assert!(
        report.contains("## Provenance")
            && report.contains("| 2024 |")
            && report.contains("| 2025 |")
    );
    // Both structures were compared; the verdict travels with the model.
    let sel = model.selection.clone().unwrap();
    assert_eq!(Some(sel.verdict.clone()), o.structure_verdict);
    assert_eq!(sel.candidate_adopted, o.candidate_adopted);
    assert_eq!(model.structure(), sel.structure);
    assert_eq!(
        sel.structure == wm_strategy::ModelStructure::Candidate,
        o.candidate_adopted
    );
    assert!(
        sel.verdict.starts_with("candidate structure adopted")
            || sel.verdict.starts_with("current structure kept"),
        "{}",
        sel.verdict
    );
    assert!(
        report.contains("## Model structure — walk-forward comparison"),
        "{report}"
    );
    assert!(plan.cache_dir.join("2024.csv").is_file());
    assert!(plan.cache_dir.join("2025-partial.csv").is_file());
    assert!(!plan.model_out.with_extension("tmp").exists());

    // Retraining downloads only the current (partial) year again.
    let (r, _) = run(&iem, &plan).await;
    let o = r.unwrap();
    assert_eq!((o.years_downloaded, o.years_cached), (1, 1));
    let requests = server.received_requests().await.unwrap();
    let per_year = |y: &str| {
        requests
            .iter()
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "year1" && v == y))
            .count()
    };
    assert_eq!((per_year("2024"), per_year("2025")), (1, 2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_failed_download_installs_nothing_but_keeps_finished_years() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    let server = MockServer::start().await;
    mount(
        &server,
        2024,
        ResponseTemplate::new(200).set_body_string(years[&2024].clone()),
    )
    .await;
    mount(
        &server,
        2025,
        ResponseTemplate::new(429).insert_header("retry-after", "3600"),
    )
    .await;
    let dir = tempdir("fail");
    let plan = plan(&dir, 30);
    let (r, _) = run(&archive(&server.uri()), &plan).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("2025"), "{e}");
    assert!(!plan.model_out.exists(), "no model from partial history");
    assert!(
        plan.cache_dir.join("2024.csv").is_file(),
        "finished year kept"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "no retry storm"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn too_little_history_is_refused() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    let server = MockServer::start().await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    let dir = tempdir("small");
    let plan = plan(&dir, 730);
    let (r, _) = run(&archive(&server.uri()), &plan).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("not enough history"), "{e}");
    assert!(!plan.model_out.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_cache_is_downloaded_again_and_sparse_years_are_not_cached() {
    let years = csv_by_year(d(2024, 10, 1), 200);
    // 2023: a finished year with only a handful of reports (transient or truncated).
    let sparse = csv_by_year(d(2023, 6, 1), 3);
    let server = MockServer::start().await;
    mount(
        &server,
        2023,
        ResponseTemplate::new(200).set_body_string(sparse[&2023].clone()),
    )
    .await;
    for (y, body) in &years {
        mount(
            &server,
            *y,
            ResponseTemplate::new(200).set_body_string(body.clone()),
        )
        .await;
    }
    let dir = tempdir("heal");
    let mut plan = plan(&dir, 30);
    plan.from_year = 2023;
    std::fs::create_dir_all(&plan.cache_dir).unwrap();
    std::fs::write(plan.cache_dir.join("2024.csv"), b"<html>proxy error</html>").unwrap();

    let (r, _) = run(&archive(&server.uri()), &plan).await;
    let o = r.unwrap();
    assert_eq!(
        (o.years_downloaded, o.years_cached),
        (3, 0),
        "corrupt 2024 replaced"
    );
    assert!(
        !plan.cache_dir.join("2023.csv").exists(),
        "sparse year not cached for good"
    );
    assert!(plan.cache_dir.join("2023-partial.csv").is_file());
    let healed = std::fs::read_to_string(plan.cache_dir.join("2024.csv")).unwrap();
    assert!(healed.starts_with("station,valid,metar"));

    let (r, _) = run(&archive(&server.uri()), &plan).await;
    let o = r.unwrap();
    assert_eq!(
        (o.years_downloaded, o.years_cached),
        (2, 1),
        "2023 and 2025 again, 2024 cached"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
