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
use wm_app::training::{self, Progress, TrainPlan};
use wm_backtest::synthetic_history;
use wm_core::ids::{ProviderId, StationId};
use wm_core::time::SystemClock;
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_strategy::PeakConfig;
use wm_weather::IemArchive;

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
    }
}

async fn run(
    archive: &IemArchive,
    plan: &TrainPlan,
) -> (anyhow::Result<training::TrainOutcome>, Vec<Progress>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = Arc::clone(&seen);
    let progress = move |p: Progress| s2.lock().unwrap().push(p);
    let (_stop, mut stop_rx) = watch::channel(false);
    let r = training::train(archive, plan, None, &progress, &mut stop_rx).await;
    let v = seen.lock().unwrap().clone();
    (r, v)
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
        o.model_id.starts_with("empirical-EHAM-all-20250419"),
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
