#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `research market` end to end against mock Gamma, Data API and IEM servers:
//! settled days only, trades converted to YES terms, reports written, and a
//! rerun served from the cache. All data are synthetic test fixtures.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;
use wiremock::matchers::{method, path, query_param, query_param_contains};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use wm_app::market_research::{self, MarketResearchClients, MarketResearchPlan};
use wm_app::training::TrainPlan;
use wm_backtest::{MarketStudyConfig, synthetic_history};
use wm_core::ids::{LocationId, ProviderId, StationId};
use wm_core::market::{FeeSchedule, TempUnit};
use wm_core::time::{SystemClock, local_date};
use wm_net::{HttpFetcher, ProviderGate, RateLimitPolicy};
use wm_polymarket::{DataApiClient, GammaClient, LocationMarketSpec, event_slug};
use wm_strategy::PeakConfig;
use wm_weather::{IemArchive, KnmiTenMinute};

const TZ: chrono_tz::Tz = chrono_tz::Europe::Amsterdam;
const TEMPLATE: &str = "highest-temperature-in-amsterdam-on-{month}-{day}-{year}";

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn fetcher(id: ProviderId, seed: u64) -> Arc<HttpFetcher> {
    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::milliseconds(1).to_std().unwrap();
    policy.max_body_bytes = 16 * 1024 * 1024;
    let gate = ProviderGate::new(id, policy, Arc::new(SystemClock::new()), seed);
    Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap())
}

fn tempdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("wm-market-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Synthetic history: IEM CSV bodies per year and each local day's high.
fn history() -> (BTreeMap<i32, String>, BTreeMap<NaiveDate, i32>) {
    let mut csv: BTreeMap<i32, String> = BTreeMap::new();
    let mut highs: BTreeMap<NaiveDate, i32> = BTreeMap::new();
    for o in synthetic_history(&eham(), d(2024, 1, 1), 474, 11, Duration::minutes(4)) {
        let body = csv
            .entry(o.key.observed_at.year())
            .or_insert_with(|| "station,valid,metar\n".to_owned());
        body.push_str(&format!(
            "EHAM,{},{}\n",
            o.key.observed_at.format("%Y-%m-%d %H:%M"),
            o.raw_text
        ));
        let t = o.temperature.unwrap().round_half_up_whole();
        let day = highs.entry(local_date(o.key.observed_at, TZ)).or_insert(t);
        *day = (*day).max(t);
    }
    (csv, highs)
}

/// Buckets ≤h−3, h−2 … h+2, ≥h+3 as (title, is_winner, condition, yes, no).
fn buckets(date: NaiveDate, high: i32) -> Vec<(String, bool, String, String, String)> {
    let mut titles = vec![(format!("{}°C or below", high - 3), false)];
    for v in (high - 2)..=(high + 2) {
        titles.push((format!("{v}°C"), v == high));
    }
    titles.push((format!("{}°C or higher", high + 3), false));
    let tag = date.format("%m%d");
    titles
        .into_iter()
        .enumerate()
        .map(|(i, (t, w))| {
            (
                t,
                w,
                format!("0xc{tag}{i}"),
                format!("1{tag}{i}"),
                format!("2{tag}{i}"),
            )
        })
        .collect()
}

fn event_json(date: NaiveDate, high: i32, settled: bool) -> String {
    let slug = event_slug(TEMPLATE, date);
    let markets: Vec<String> = buckets(date, high)
        .iter()
        .enumerate()
        .map(|(i, (title, won, cond, yes, no))| {
            let prices = if !settled {
                "[\"0.5\", \"0.5\"]"
            } else if *won {
                "[\"1\", \"0\"]"
            } else {
                "[\"0\", \"1\"]"
            };
            format!(
                r#"{{"id":"m{i}","question":"Will the highest temperature in Amsterdam be {title}?","conditionId":"{cond}","questionID":"0xq{i}","slug":"amsterdam-{i}","groupItemTitle":"{title}","outcomes":"[\"Yes\", \"No\"]","outcomePrices":{prices:?},"clobTokenIds":"[\"{yes}\", \"{no}\"]","active":{active},"closed":{settled},"acceptingOrders":false,"orderPriceMinTickSize":0.01,"orderMinSize":5,"negRisk":true}}"#,
                active = !settled
            )
        })
        .collect();
    format!(
        r#"[{{"id":"e1","slug":"{slug}","title":"Highest temperature in Amsterdam on {date}?","description":"This market will resolve to the temperature range that contains the highest temperature recorded by NOAA at the Amsterdam Airport Schiphol Station in degrees Celsius on {date}. The resolution source for this market will be information from NOAA, specifically the highest reading under the \"Temp\" column for all times on the specified day, available here: https://www.weather.gov/wrh/timeseries?site=eham. The resolution source for this market measures temperatures to whole degrees Celsius (eg, 9°C), which is the level of precision that will be used when resolving the market.","resolutionSource":"https://www.weather.gov/wrh/timeseries?site=eham","endDate":"{date}T12:00:00Z","active":{active},"closed":{settled},"negRisk":true,"markets":[{}]}}]"#,
        markets.join(","),
        active = !settled
    )
}

fn at(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
    TZ.from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc)
}

/// Every half hour, a taker buys YES just above the bucket's price and
/// another buys NO just above one minus it (a YES sale just below).
fn trades_json(date: NaiveDate, high: i32) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for (i, (_, won, cond, yes, no)) in buckets(date, high).iter().enumerate() {
        let p = if *won { 0.80 } else { 0.03 };
        for h in 7..24 {
            for m in [0, 30] {
                let ts = at(date, h, m).timestamp();
                out.push((ts, format!(
                    r#"{{"proxyWallet":"0xa{i}","side":"BUY","asset":"{yes}","conditionId":"{cond}","size":10,"price":{:.3},"timestamp":{ts},"outcome":"Yes","transactionHash":"0xy{i}{ts}"}}"#,
                    p + 0.01
                )));
                out.push((ts, format!(
                    r#"{{"proxyWallet":"0xb{i}","side":"BUY","asset":"{no}","conditionId":"{cond}","size":10,"price":{:.3},"timestamp":{ts},"outcome":"No","transactionHash":"0xn{i}{ts}"}}"#,
                    1.0 - p + 0.01
                )));
            }
        }
    }
    out
}

struct Servers {
    gamma: MockServer,
    data: MockServer,
    iem: MockServer,
}

async fn servers(highs: &BTreeMap<NaiveDate, i32>, csv: &BTreeMap<i32, String>) -> Servers {
    let (gamma, data, iem) = (
        MockServer::start().await,
        MockServer::start().await,
        MockServer::start().await,
    );
    for (year, body) in csv {
        Mock::given(method("GET"))
            .and(path("/cgi-bin/request/asos.py"))
            .and(query_param("year1", year.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .mount(&iem)
            .await;
    }
    // 10–12 April settled, 13 April has no event, 14 April is still open.
    let mut by_market: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    for day in [
        d(2025, 4, 10),
        d(2025, 4, 11),
        d(2025, 4, 12),
        d(2025, 4, 14),
    ] {
        let high = highs[&day];
        let settled = day != d(2025, 4, 14);
        Mock::given(method("GET"))
            .and(path("/events"))
            .and(query_param("slug", event_slug(TEMPLATE, day).as_str()))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(event_json(day, high, settled)),
            )
            .mount(&gamma)
            .await;
        let conds: Vec<String> = buckets(day, high).into_iter().map(|b| b.2).collect();
        by_market.insert(conds.join(","), trades_json(day, high));
    }
    Mock::given(method("GET"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&gamma)
        .await;
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(move |req: &Request| {
            let q: BTreeMap<String, String> = req.url.query_pairs().into_owned().collect();
            let num = |k: &str| q[k].parse::<i64>().unwrap();
            let (start, end, offset, limit) =
                (num("start"), num("end"), num("offset"), num("limit"));
            let mut rows: Vec<&(i64, String)> = by_market
                .get(&q["market"])
                .map(|v| {
                    v.iter()
                        .filter(|(ts, _)| *ts >= start && *ts <= end)
                        .collect()
                })
                .unwrap_or_default();
            rows.reverse();
            let page: Vec<&str> = rows
                .into_iter()
                .skip(usize::try_from(offset).unwrap())
                .take(usize::try_from(limit).unwrap())
                .map(|(_, r)| r.as_str())
                .collect();
            ResponseTemplate::new(200).set_body_string(format!("[{}]", page.join(",")))
        })
        .mount(&data)
        .await;
    Servers { gamma, data, iem }
}

fn plan(dir: &Path) -> MarketResearchPlan {
    MarketResearchPlan {
        train: TrainPlan {
            station: eham(),
            tz: TZ,
            peak: PeakConfig::default(),
            from_year: 2024,
            today: d(2025, 4, 19),
            cache_dir: dir.join("research/iem/EHAM"),
            model_out: dir.join("models/eham.json"),
            report_out: dir.join("research/eham-survival.md"),
            min_days: 1,
            forecast: None,
            selection: None,
            slot_quantiles: (0.5, 0.9),
        },
        spec: LocationMarketSpec {
            location: LocationId::new("amsterdam").unwrap(),
            station: eham(),
            timezone: TZ,
            slug_template: TEMPLATE.into(),
            unit: TempUnit::Celsius,
            fees: FeeSchedule::taker(50_000),
        },
        from: d(2025, 4, 10),
        to: d(2025, 4, 14),
        study: MarketStudyConfig {
            min_model_support: 10,
            bootstrap_iterations: 200,
            timeline_days: vec![d(2025, 4, 11)],
            ..MarketStudyConfig::new(eham(), TZ, PeakConfig::default())
        },
        use_forecast: false,
        knmi_location: None,
        cache_dir: dir.join("research/polymarket/EHAM"),
        report_out: dir.join("research/eham-market.md"),
    }
}

#[tokio::test]
async fn scores_settled_days_and_serves_a_rerun_from_the_cache() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    let dir = tempdir("e2e");
    let plan = plan(&dir);
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let data = DataApiClient::new(fetcher(ProviderId::polymarket_data(), 2), s.data.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: None,
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let now = at(d(2025, 4, 19), 12, 0);
    let o = market_research::run(&clients, &plan, now, &|_| {}, &mut stop_rx)
        .await
        .unwrap();
    assert_eq!(o.report.market_days, 3);
    assert_eq!((o.days_downloaded, o.days_cached), (3, 0));
    assert_eq!(
        o.unavailable,
        vec![
            (d(2025, 4, 13), "no event with this slug".to_owned()),
            (
                d(2025, 4, 14),
                "not settled yet (a bucket is still open)".to_owned()
            ),
        ]
    );
    assert_eq!(
        (o.report.resolution_agreed, o.report.resolution_checked),
        (3, 3)
    );
    assert_eq!(o.report.scored_days, 3, "{:?}", o.report.skipped_days);
    assert!(o.report.points > 50, "{}", o.report.points);
    // YES buys at p + 0.01 and NO buys at 1 − p + 0.01 average to p.
    let cal = &o.report.calibration;
    let winners = cal.iter().find(|r| r.group == "0.70–0.90").unwrap();
    assert!(
        winners.points > 0 && (winners.market_mean - 0.80).abs() < 1e-9,
        "{cal:?}"
    );
    let md = std::fs::read_to_string(&o.markdown).unwrap();
    assert!(md.contains("# Model versus market — EHAM"));
    assert!(md.contains("2025-04-14: not settled yet"));
    // Both structures, the strategies at traded prices and the replayed day.
    assert!(md.contains("| candidate model |"), "{md}");
    assert!(md.contains("## Strategies at traded prices"));
    assert!(md.contains("## Day replay — 2025-04-11 (resolved "), "{md}");
    // Per structure: A and B × 3 windows × 2 ranges, E's 3 variants and the
    // 3 maker versions of the live rules.
    assert_eq!(o.report.strategies.len(), 36);
    assert!(md.contains("## Makers and takers: the other side of every trade"));
    assert!(!o.report.maker_taker.rows.is_empty());
    assert!(
        o.report
            .strategies
            .iter()
            .any(|r| r.strategy == "E" && r.live)
    );
    assert!(md.contains("Strategy E buys YES on the bucket holding the high"));
    // Strategy F: the peak times of the whole history and its six rules
    // (100 shares a trade), apart from the $10 rows.
    assert!(
        md.contains("## When the day's high is first reported (strategy F)"),
        "{md}"
    );
    assert!(md.contains("## Strategy F at traded prices"));
    assert_eq!(o.report.f_strategies.len(), 6);
    assert!(o.report.f_strategies[0].live && o.report.f_strategies[0].strategy == "F");
    assert!(o.report.f_verdict[0].starts_with("Strategy F ("));
    // Three market days are too few to choose a rule out of sample.
    assert!(
        o.report
            .f_out_of_sample
            .as_deref()
            .is_some_and(|l| l.contains("3 replayed market days are too few")),
        "{:?}",
        o.report.f_out_of_sample
    );
    assert!(o.report.peak_times.is_some());
    assert_eq!(o.report.timelines.len(), 1);
    assert!(!o.report.timelines[0].rows.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&o.json).unwrap()).unwrap();
    assert_eq!(json["market_days"], 3);
    assert_eq!(json["timelines"][0]["date"], "2025-04-11");
    assert!(json["strategies"].as_array().is_some_and(|a| a.len() == 36));
    assert_eq!(json["sim"]["maker"]["cancel_before_report_min"], 10);
    assert_eq!(json["sim"]["e"]["lookback_minutes"], 30);
    assert_eq!(json["sim"]["f"]["shares"], 100.0);
    assert!(
        json["f_strategies"]
            .as_array()
            .is_some_and(|a| a.len() == 6)
    );
    // Settled days are cached; open or missing ones are not.
    let cache = &plan.cache_dir;
    for day in [d(2025, 4, 10), d(2025, 4, 11), d(2025, 4, 12)] {
        let slug = event_slug(TEMPLATE, day);
        assert!(cache.join(format!("{slug}.event.json")).is_file());
        let trades = std::fs::read_to_string(cache.join(format!("{slug}.trades.json"))).unwrap();
        assert!(!trades.contains("0xa1"), "wallets are stored hashed");
    }
    assert!(
        !cache
            .join(format!(
                "{}.event.json",
                event_slug(TEMPLATE, d(2025, 4, 14))
            ))
            .exists()
    );

    // A rerun asks the network only for the days that were not settled.
    let data_calls = s.data.received_requests().await.unwrap().len();
    let gamma_calls = s.gamma.received_requests().await.unwrap().len();
    let again = market_research::run(&clients, &plan, now, &|_| {}, &mut stop_rx)
        .await
        .unwrap();
    assert_eq!((again.days_downloaded, again.days_cached), (0, 3));
    assert_eq!(s.data.received_requests().await.unwrap().len(), data_calls);
    assert_eq!(
        s.gamma.received_requests().await.unwrap().len(),
        gamma_calls + 2,
        "only 13 and 14 April are asked again"
    );
    assert_eq!(again.report.points, o.report.points);
}

#[tokio::test]
async fn no_settled_market_is_an_error() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    let dir = tempdir("none");
    let mut plan = plan(&dir);
    plan.from = d(2025, 4, 13);
    plan.to = d(2025, 4, 14);
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let data = DataApiClient::new(fetcher(ProviderId::polymarket_data(), 2), s.data.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: None,
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let err = market_research::run(&clients, &plan, Utc::now(), &|_| {}, &mut stop_rx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no settled market"), "{err}");
    assert!(s.data.received_requests().await.unwrap().is_empty());
    assert!(
        s.iem.received_requests().await.unwrap().is_empty(),
        "no history needed"
    );
    plan.from = d(2025, 4, 15);
    assert!(
        market_research::run(&clients, &plan, Utc::now(), &|_| {}, &mut stop_rx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_day_whose_trades_keep_failing_is_skipped_and_the_rest_studied() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    // 11 April's trades fail on every attempt.
    let bad = d(2025, 4, 11);
    let conds: Vec<String> = buckets(bad, highs[&bad]).into_iter().map(|b| b.2).collect();
    Mock::given(method("GET"))
        .and(path("/trades"))
        .and(query_param("market", conds.join(",").as_str()))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .expect(u64::from(wm_polymarket::data::PAGE_ATTEMPTS))
        .mount(&s.data)
        .await;
    let dir = tempdir("flaky");
    let plan = plan(&dir);
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let data = DataApiClient::new(fetcher(ProviderId::polymarket_data(), 2), s.data.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: None,
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let now = at(d(2025, 4, 19), 12, 0);
    let o = market_research::run(&clients, &plan, now, &|_| {}, &mut stop_rx)
        .await
        .unwrap();
    // The other settled days (10 and 12 April) are still studied.
    assert_eq!(o.report.market_days, 2);
    assert_eq!(o.report.scored_days, 2, "{:?}", o.report.skipped_days);
    let (_, why) = o.unavailable.iter().find(|(day, _)| *day == bad).unwrap();
    assert!(
        why.starts_with("download failed (rerun to retry)") && why.contains("503"),
        "{why}"
    );
    // Nothing of the failed day is cached, so a rerun asks for it again.
    let slug = event_slug(TEMPLATE, bad);
    assert!(!plan.cache_dir.join(format!("{slug}.trades.json")).exists());
    assert!(!plan.cache_dir.join(format!("{slug}.event.json")).exists());
    let md = std::fs::read_to_string(&o.markdown).unwrap();
    assert!(
        md.contains("2025-04-11: download failed (rerun to retry)"),
        "{md}"
    );
}

#[tokio::test]
async fn downloads_stop_after_three_failed_days_in_a_row() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    Mock::given(method("GET"))
        .and(path("/trades"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&s.data)
        .await;
    let dir = tempdir("down");
    let plan = plan(&dir);
    // Backoff only (no circuit), kept short so the retries run fast.
    let mut policy = RateLimitPolicy::local_test();
    policy.min_interval = Duration::milliseconds(1).to_std().unwrap();
    policy.max_body_bytes = 16 * 1024 * 1024;
    policy.circuit_failure_threshold = 1_000;
    policy.backoff_max = Duration::milliseconds(100).to_std().unwrap();
    let gate = ProviderGate::new(
        ProviderId::polymarket_data(),
        policy,
        Arc::new(SystemClock::new()),
        2,
    );
    let data = DataApiClient::new(
        Arc::new(HttpFetcher::new(gate, "WeatherMachine-test/0 (test@example.invalid)").unwrap()),
        s.data.uri(),
    );
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: None,
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let err = market_research::run(
        &clients,
        &plan,
        at(d(2025, 4, 19), 12, 0),
        &|_| {},
        &mut stop_rx,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("could be downloaded: 3 failed even after retries"),
        "{err}"
    );
    // 10, 11 and 12 April failed; 13 and 14 April were not asked for.
    let asked: Vec<String> = s
        .gamma
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "slug")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        asked,
        [10, 11, 12]
            .map(|day| event_slug(TEMPLATE, d(2025, 4, day)))
            .to_vec()
    );
    let trade_calls = s.data.received_requests().await.unwrap().len();
    assert_eq!(trade_calls, 3 * wm_polymarket::data::PAGE_ATTEMPTS as usize);
    assert!(s.iem.received_requests().await.unwrap().is_empty());
}

/// KNMI's EDR API for one station: a reading every ten minutes of the
/// requested window, the mean 15.4 °C and the maximum 15.6 °C.
async fn knmi_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/locations/0-20000-0-06240"))
        .respond_with(|req: &Request| {
            let q: BTreeMap<String, String> = req.url.query_pairs().into_owned().collect();
            let (from, to) = q["datetime"].split_once('/').unwrap();
            let parse = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
            let (mut t, to) = (parse(from), parse(to));
            let mut times = Vec::new();
            while t <= to {
                times.push(format!("\"{}\"", t.format("%Y-%m-%dT%H:%M:%SZ")));
                t += Duration::minutes(10);
            }
            let n = times.len();
            let values = |v: &str| vec![v; n].join(",");
            ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"type":"CoverageCollection","coverages":[{{"type":"Coverage","domain":{{"axes":{{"t":{{"values":[{}]}}}}}},"ranges":{{"ta":{{"values":[{}]}},"tx":{{"values":[{}]}}}}}}]}}"#,
                times.join(","),
                values("15.4"),
                values("15.6")
            ))
        })
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn with_a_knmi_key_strategy_k_is_replayed_and_the_readings_cached() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    let knmi_srv = knmi_server().await;
    let dir = tempdir("knmi");
    let mut plan = plan(&dir);
    plan.knmi_location = Some("0-20000-0-06240".into());
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let data = DataApiClient::new(fetcher(ProviderId::polymarket_data(), 2), s.data.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let knmi = KnmiTenMinute::new(
        fetcher(ProviderId::knmi(), 4),
        knmi_srv.uri(),
        "test-key",
        "ta",
        "tx",
    );
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: Some(&knmi),
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let now = at(d(2025, 4, 19), 12, 0);
    let o = market_research::run(&clients, &plan, now, &|_| {}, &mut stop_rx)
        .await
        .unwrap();
    assert_eq!(o.report.knmi_days, 3, "the three settled days");
    let reports: u64 = o.report.knmi_accuracy.iter().map(|r| r.reports).sum();
    assert!(reports > 0, "{:?}", o.report.knmi_accuracy);
    assert!(
        !o.report
            .gk_verdict
            .iter()
            .any(|v| v.contains("not replayed")),
        "{:?}",
        o.report.gk_verdict
    );
    let md = std::fs::read_to_string(&o.markdown).unwrap();
    assert!(
        md.contains("### KNMI's ten-minute mean before the METAR (3 days)"),
        "{md}"
    );
    assert!(md.contains("| K **live** |"), "{md}");
    assert!(md.contains("KNMI Data Platform ten-minute observations"));
    // 10–14 April is one chunk. On 19 April its last day is five days old
    // and KNMI may still fill gaps, so it is not cached; on 30 April it is.
    let cached = dir.join("research/knmi/EHAM/2025-04-10_2025-04-14.json");
    assert!(!cached.exists(), "a week KNMI may still fill is not cached");
    let later = at(d(2025, 4, 30), 12, 0);
    market_research::run(&clients, &plan, later, &|_| {}, &mut stop_rx)
        .await
        .unwrap();
    assert!(cached.exists(), "a final week is cached");
    let rows: Vec<wm_core::weather::TenMinuteObservation> =
        serde_json::from_slice(&std::fs::read(&cached).unwrap()).unwrap();
    assert!(rows.len() > 5 * 144 - 10, "{}", rows.len());
    assert_eq!(rows[0].mean, Some(wm_core::units::TempC::from_tenths(154)));
}

#[tokio::test]
async fn a_knmi_week_that_will_not_download_costs_only_that_week() {
    let (csv, highs) = history();
    let s = servers(&highs, &csv).await;
    let knmi_srv = knmi_server().await;
    // The second week (17–18 April, local) is refused.
    Mock::given(method("GET"))
        .and(path("/locations/0-20000-0-06240"))
        .and(query_param_contains("datetime", "2025-04-16T22:00:00Z/"))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&knmi_srv)
        .await;
    let dir = tempdir("knmi-gap");
    let mut plan = plan(&dir);
    plan.knmi_location = Some("0-20000-0-06240".into());
    plan.to = d(2025, 4, 18);
    let gamma = GammaClient::new(fetcher(ProviderId::polymarket_gamma(), 1), s.gamma.uri());
    let data = DataApiClient::new(fetcher(ProviderId::polymarket_data(), 2), s.data.uri());
    let archive = IemArchive::new(fetcher(ProviderId::iem(), 3), s.iem.uri());
    let knmi = KnmiTenMinute::new(
        fetcher(ProviderId::knmi(), 4),
        knmi_srv.uri(),
        "test-key",
        "ta",
        "tx",
    );
    let clients = MarketResearchClients {
        gamma: &gamma,
        data: &data,
        archive: &archive,
        forecast: None,
        knmi: Some(&knmi),
    };
    let (_stop, mut stop_rx) = watch::channel(false);
    let o = market_research::run(
        &clients,
        &plan,
        at(d(2025, 4, 19), 12, 0),
        &|_| {},
        &mut stop_rx,
    )
    .await
    .unwrap();
    // The first week (10–16 April) holds the three settled days: K is
    // replayed on all of them.
    assert_eq!(o.report.knmi_days, 3);
    assert!(
        !o.report
            .gk_verdict
            .iter()
            .any(|v| v.contains("not replayed")),
        "{:?}",
        o.report.gk_verdict
    );
    // Every week refused: K is not replayed, the rest of the study is.
    let all_refused = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&all_refused)
        .await;
    let refused = KnmiTenMinute::new(
        fetcher(ProviderId::knmi(), 5),
        all_refused.uri(),
        "test-key",
        "ta",
        "tx",
    );
    let clients = MarketResearchClients {
        knmi: Some(&refused),
        ..clients
    };
    let o = market_research::run(
        &clients,
        &plan,
        at(d(2025, 4, 19), 12, 0),
        &|_| {},
        &mut stop_rx,
    )
    .await
    .unwrap();
    assert_eq!(o.report.knmi_days, 0);
    assert_eq!(o.report.market_days, 3);
}
