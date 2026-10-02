#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Strategies G–K on synthetic markets: each one's signal, the order it
//! proposes and the blockers that hold it back; the unwind engine leaves
//! their positions alone.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Europe::Amsterdam;
use std::collections::{HashMap, HashSet};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, StationId, StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::portfolio::{InstrumentRef, PositionBook};
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{Fill, IntentKind, Liquidity, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    Observation, ObservationKey, QualityFlags, ReportType, TempPrecision, TenMinuteObservation,
};
use wm_strategy::{
    IncrementDistribution, KnmiNowcast, KnmiNowcastConfig, MiddleFade, MiddleFadeConfig,
    MorningMaker, MorningMakerConfig, NextDegree, NextDegreeConfig, PeakDetectionEngine, Strategy,
    StrategyContext, StrategyOutput, TailSeller, TailSellerConfig, TemperatureStateEngine,
    UnwindConfig, UnwindEngine, ViewEvaluation, ViewKind,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn p(s: &str) -> Price {
    Price::parse(s).unwrap()
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
}

fn obs(t: &str, tenths: i32) -> Observation {
    Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: utc(t),
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_tenths(tenths)),
        dewpoint: None,
        precision: TempPrecision::WholeDegree,
        raw_text: String::new(),
        content_hash: t.to_owned(),
        provider: ProviderId::awc(),
        provider_receipt_at: None,
        fetched_at: utc(t),
        parser_version: 1,
        quality: QualityFlags::default(),
    }
}

fn dist(p: &[f64]) -> IncrementDistribution {
    IncrementDistribution {
        probs: p.to_vec(),
        support: 500,
        source: "test".into(),
    }
}

fn views(
    series: &[(&str, i32)],
    now: &str,
    d: Option<IncrementDistribution>,
) -> Vec<ViewEvaluation> {
    let mut e = TemperatureStateEngine::new(5);
    e.register_station(eham(), Amsterdam);
    for (t, v) in series {
        e.apply_observation(&obs(t, *v));
    }
    let now = utc(now);
    let s = e.day_state(&eham(), date(), ViewKind::All, now).unwrap();
    let a = PeakDetectionEngine::default()
        .assess(&s, Amsterdam, now)
        .unwrap();
    vec![ViewEvaluation {
        view: ViewKind::All,
        assessment: a,
        distribution: d,
    }]
}

fn market(at: &str) -> DailyTemperatureMarket {
    synthetic_temperature_market(
        &LocationId::new("amsterdam").unwrap(),
        &eham(),
        date(),
        Amsterdam,
        13,
        24,
        utc(at),
    )
}

fn yes(m: &DailyTemperatureMarket, v: i32) -> TokenId {
    m.outcome_for_value(v).unwrap().yes_token.clone()
}

fn no(m: &DailyTemperatureMarket, v: i32) -> TokenId {
    m.outcome_for_value(v).unwrap().no_token.clone()
}

struct World {
    m: DailyTemperatureMarket,
    books: HashMap<TokenId, OrderBook>,
    positions: PositionBook,
    pending: HashSet<TokenId>,
    loc: LocationId,
    now: DateTime<Utc>,
    nowcast: Option<TenMinuteObservation>,
}

impl World {
    fn new(now: &str) -> Self {
        Self {
            m: market(now),
            books: HashMap::new(),
            positions: PositionBook::new(),
            pending: HashSet::new(),
            loc: LocationId::new("amsterdam").unwrap(),
            now: utc(now),
            nowcast: None,
        }
    }

    fn book(&mut self, token: TokenId, bid: Option<&str>, ask: Option<&str>) {
        let b = synthetic_book(&token, bid, ask, 200, self.now);
        self.books.insert(token, b);
    }

    fn run(&self, s: &mut dyn Strategy, v: &[ViewEvaluation]) -> StrategyOutput {
        let ctx = StrategyContext {
            now: self.now,
            mode: RunMode::Paper,
            location: &self.loc,
            market: &self.m,
            books: &self.books,
            views: v,
            positions: &self.positions,
            pending_tokens: &self.pending,
            peak_times: None,
            routine_minutes: &[25, 55],
            nowcast: self.nowcast.as_ref(),
            lab: &wm_strategy::LabInputs::EMPTY,
        };
        s.evaluate(&ctx)
    }
}

fn blockers_of(out: &StrategyOutput, label: &str, side: OutcomeSide) -> Vec<String> {
    out.evaluations
        .iter()
        .find(|e| e.bucket_label == label && e.outcome_side == side)
        .map(|e| e.blockers.clone())
        .unwrap_or_else(|| panic!("no evaluation of {label} {side:?}: {:?}", out.evaluations))
}

/// High 18 °C at 12:00Z, 17 °C by 13:30Z (15:32 local at NOW).
fn afternoon() -> Vec<(&'static str, i32)> {
    vec![
        ("2026-07-01T11:00:00Z", 170),
        ("2026-07-01T11:30:00Z", 175),
        ("2026-07-01T12:00:00Z", 180),
        ("2026-07-01T12:30:00Z", 178),
        ("2026-07-01T13:00:00Z", 175),
        ("2026-07-01T13:30:00Z", 170),
    ]
}

const NOW: &str = "2026-07-01T13:32:00Z";

// ---------------------------------------------------------------------------
// G — tail seller
// ---------------------------------------------------------------------------

fn g_world() -> World {
    let mut w = World::new(NOW);
    let m = w.m.clone();
    w.book(no(&m, 20), Some("0.96"), Some("0.97"));
    w.book(no(&m, 21), Some("0.98"), Some("0.985"));
    w.book(no(&m, 19), Some("0.80"), Some("0.82"));
    w
}

#[test]
fn g_rests_no_bids_on_cheap_buckets_two_or_more_above_the_high() {
    let w = g_world();
    let v = views(&afternoon(), NOW, Some(dist(&[0.985, 0.012, 0.002, 0.001])));
    let out = w.run(&mut TailSeller::new(TailSellerConfig::default()), &v);
    // 19 °C is one above the high: not G's; 20–24 °C are.
    assert!(out.evaluations.iter().all(|e| e.bucket_label != "19°C"));
    assert_eq!(out.proposals.len(), 2, "{:?}", out.evaluations);
    let p20 = &out.proposals[0];
    assert_eq!(p20.bucket_label, "20°C");
    assert_eq!(
        (p20.side, p20.outcome_side, p20.kind),
        (Side::Buy, OutcomeSide::No, IntentKind::Open)
    );
    // One tick inside would meet the 0.97 ask: G joins the 0.96 bid.
    assert_eq!(p20.limit_price, p("0.96"));
    assert_eq!(p20.shares, Shares::from_whole(31), "$30 at 0.96");
    assert_eq!(
        p20.tif,
        TimeInForce::Gtd {
            expires_at: utc("2026-07-01T13:45:00Z")
        },
        "ten minutes before the 13:55 report"
    );
    assert!(p20.ev_per_share > 0.03, "{}", p20.ev_per_share);
    assert_eq!(out.proposals[1].bucket_label, "21°C");
    assert_eq!(out.proposals[1].limit_price, p("0.98"));
    // Buckets without a NO book say so.
    assert_eq!(
        blockers_of(&out, "22°C", OutcomeSide::No),
        vec!["no NO book"]
    );
}

#[test]
fn g_blockers() {
    let w = g_world();
    let good = Some(dist(&[0.985, 0.012, 0.002, 0.001]));
    let v = views(&afternoon(), NOW, good.clone());
    let run = |w: &World, cfg: TailSellerConfig, v: &[ViewEvaluation]| {
        w.run(&mut TailSeller::new(cfg), v)
    };
    // The model rates 20 °C at 8 %: above the 4 % G would sell it for.
    let worried = views(&afternoon(), NOW, Some(dist(&[0.80, 0.10, 0.08, 0.02])));
    let out = run(&w, TailSellerConfig::default(), &worried);
    assert!(
        blockers_of(&out, "20°C", OutcomeSide::No)
            .iter()
            .any(|b| b.contains("the model rates the tail higher")),
        "{:?}",
        out.evaluations
    );
    // The YES offered must be cheap enough.
    let strict = TailSellerConfig {
        max_yes_price: p("0.03"),
        ..TailSellerConfig::default()
    };
    let out = run(&w, strict, &v);
    assert_eq!(
        blockers_of(&out, "20°C", OutcomeSide::No),
        vec!["YES offered at 0.04 outside [0.01, 0.03]"]
    );
    // Too close to the report: nothing rests through it.
    let mut late = g_world();
    late.now = utc("2026-07-01T13:47:00Z");
    let v_late = views(&afternoon(), "2026-07-01T13:47:00Z", good.clone());
    let out = run(&late, TailSellerConfig::default(), &v_late);
    assert!(out.proposals.is_empty());
    assert!(
        blockers_of(&out, "20°C", OutcomeSide::No)
            .iter()
            .any(|b| b.starts_with("within 10′"))
    );
    // Outside the window (09:00 local).
    let mut early = g_world();
    early.now = utc("2026-07-01T07:00:00Z");
    let morning = vec![("2026-07-01T06:30:00Z", 150)];
    let v_early = views(&morning, "2026-07-01T07:00:00Z", good.clone());
    let out = run(&early, TailSellerConfig::default(), &v_early);
    assert!(out.proposals.is_empty());
    // No model: fail closed.
    let v_none = views(&afternoon(), NOW, None);
    let out = run(&w, TailSellerConfig::default(), &v_none);
    assert!(out.proposals.is_empty());
    assert!(blockers_of(&out, "20°C", OutcomeSide::No).contains(&"no probability model".into()));
    // Already quoting the token.
    let mut busy = g_world();
    busy.pending.insert(no(&busy.m, 20));
    let out = run(&busy, TailSellerConfig::default(), &v);
    assert_eq!(out.proposals.len(), 1);
    assert_eq!(
        blockers_of(&out, "20°C", OutcomeSide::No),
        vec!["already positioned or quoting"]
    );
}

// ---------------------------------------------------------------------------
// H — next degree
// ---------------------------------------------------------------------------

const H_NOW: &str = "2026-07-01T11:32:00Z"; // 13:32 local

/// High 18 °C at 11:00Z, 17.8 °C at 11:30Z: still near the high.
fn late_morning() -> Vec<(&'static str, i32)> {
    vec![
        ("2026-07-01T10:00:00Z", 170),
        ("2026-07-01T10:30:00Z", 175),
        ("2026-07-01T11:00:00Z", 180),
        ("2026-07-01T11:30:00Z", 178),
    ]
}

#[test]
fn h_buys_the_next_degree_while_the_day_can_warm() {
    let mut w = World::new(H_NOW);
    let m = w.m.clone();
    w.book(yes(&m, 19), Some("0.10"), Some("0.12"));
    let v = views(
        &late_morning(),
        H_NOW,
        Some(dist(&[0.70, 0.25, 0.04, 0.01])),
    );
    let out = w.run(&mut NextDegree::new(NextDegreeConfig::default()), &v);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let pr = &out.proposals[0];
    assert_eq!(pr.bucket_label, "19°C");
    assert_eq!((pr.side, pr.outcome_side), (Side::Buy, OutcomeSide::Yes));
    assert_eq!(pr.limit_price, p("0.12"));
    assert_eq!(pr.shares, Shares::from_whole(83), "$10 at 0.12");
    assert_eq!(pr.tif, TimeInForce::Fak);
    assert!((pr.p_win - 0.25).abs() < 1e-12);
}

#[test]
fn h_never_pays_more_than_its_notional_when_the_sweep_goes_deeper() {
    let mut w = World::new(H_NOW);
    let m = w.m.clone();
    // 50 shares at 0.12, more at 0.13: 83 shares need 0.13, so H buys 76.
    let mut b = synthetic_book(&yes(&m, 19), Some("0.10"), Some("0.12"), 50, w.now);
    b.asks.push(wm_core::market::BookLevel {
        price: p("0.13"),
        size: Shares::from_whole(500),
    });
    w.books.insert(yes(&m, 19), b);
    let v = views(
        &late_morning(),
        H_NOW,
        Some(dist(&[0.70, 0.25, 0.04, 0.01])),
    );
    let out = w.run(&mut NextDegree::new(NextDegreeConfig::default()), &v);
    let pr = &out.proposals[0];
    assert_eq!(pr.limit_price, p("0.13"));
    assert_eq!(pr.shares, Shares::from_whole(76));
    assert!(
        wm_core::units::notional(pr.limit_price, pr.shares, wm_core::units::Rounding::Up)
            <= Usd::from_whole(10)
    );
}

#[test]
fn h_blockers() {
    let mut w = World::new(H_NOW);
    let m = w.m.clone();
    w.book(yes(&m, 19), Some("0.10"), Some("0.12"));
    let good = Some(dist(&[0.70, 0.25, 0.04, 0.01]));
    let one = |w: &World, cfg: NextDegreeConfig, v: &[ViewEvaluation]| {
        let out = w.run(&mut NextDegree::new(cfg), v);
        blockers_of(&out, "19°C", OutcomeSide::Yes)
    };
    // The model rates the next degree below its price.
    let v = views(
        &late_morning(),
        H_NOW,
        Some(dist(&[0.90, 0.08, 0.015, 0.005])),
    );
    assert_eq!(
        one(&w, NextDegreeConfig::default(), &v),
        vec!["model 0.080 < 1.00 × ask 0.12"]
    );
    // Cooling: 1.5 °C below the high.
    let cooling = vec![
        ("2026-07-01T10:30:00Z", 175),
        ("2026-07-01T11:00:00Z", 180),
        ("2026-07-01T11:30:00Z", 165),
    ];
    let v = views(&cooling, H_NOW, good.clone());
    assert_eq!(
        one(&w, NextDegreeConfig::default(), &v),
        vec!["1.5 °C below the high > 1.0: cooling"]
    );
    // After the fallback end (15:30 local) the day is not expected to warm.
    let mut late = World::new("2026-07-01T13:40:00Z");
    late.book(yes(&m, 19), Some("0.10"), Some("0.12"));
    let near = vec![("2026-07-01T13:00:00Z", 180), ("2026-07-01T13:30:00Z", 178)];
    let v = views(&near, "2026-07-01T13:40:00Z", good.clone());
    assert_eq!(
        one(&late, NextDegreeConfig::default(), &v),
        vec!["15:40 outside 10:00–15:30 (fallback end: peak times not learned yet)"]
    );
    // An ask outside the band.
    let mut dear = World::new(H_NOW);
    dear.book(yes(&m, 19), Some("0.38"), Some("0.40"));
    let v = views(
        &late_morning(),
        H_NOW,
        Some(dist(&[0.40, 0.45, 0.10, 0.05])),
    );
    assert_eq!(
        one(&dear, NextDegreeConfig::default(), &v),
        vec!["ask 0.40 outside [0.05, 0.35]"]
    );
}

// ---------------------------------------------------------------------------
// I — middle fade
// ---------------------------------------------------------------------------

fn i_world(yes_bid: &str, yes_ask: &str, no_bid: &str, no_ask: &str) -> World {
    let mut w = World::new(NOW);
    let m = w.m.clone();
    w.book(yes(&m, 19), Some(yes_bid), Some(yes_ask));
    w.book(no(&m, 19), Some(no_bid), Some(no_ask));
    w
}

#[test]
fn i_buys_the_no_of_an_overpriced_middle_bucket() {
    let w = i_world("0.44", "0.46", "0.54", "0.55");
    let v = views(&afternoon(), NOW, Some(dist(&[0.70, 0.25, 0.04, 0.01])));
    let out = w.run(&mut MiddleFade::new(MiddleFadeConfig::default()), &v);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let pr = &out.proposals[0];
    assert_eq!(pr.bucket_label, "19°C");
    assert_eq!((pr.side, pr.outcome_side), (Side::Buy, OutcomeSide::No));
    assert_eq!(pr.limit_price, p("0.55"));
    assert_eq!(pr.shares, Shares::from_whole(18), "$10 at 0.55");
    // p(NO) = 1 − (0.45 − 0.03) = 0.58, not above the model's 0.75.
    assert!((pr.p_win - 0.58).abs() < 1e-9, "{}", pr.p_win);
    let e = &out.evaluations[0];
    assert_eq!(e.market_p, Some(0.55));
}

#[test]
fn i_blockers() {
    let one = |w: &World, v: &[ViewEvaluation]| {
        let out = w.run(&mut MiddleFade::new(MiddleFadeConfig::default()), v);
        out.evaluations
            .iter()
            .find(|e| e.bucket_label == "19°C")
            .map(|e| e.blockers.clone())
    };
    let good = Some(dist(&[0.70, 0.25, 0.04, 0.01]));
    // The NO costs too much for the measured three-point overpricing.
    let w = i_world("0.44", "0.46", "0.54", "0.58");
    let v = views(&afternoon(), NOW, good.clone());
    let b = one(&w, &v).unwrap();
    assert_eq!(b.len(), 1);
    assert!(b[0].starts_with("edge -0.0"), "{b:?}");
    // The model does not rate it lower.
    let w = i_world("0.44", "0.46", "0.54", "0.55");
    let v = views(&afternoon(), NOW, Some(dist(&[0.50, 0.42, 0.06, 0.02])));
    assert_eq!(
        one(&w, &v).unwrap(),
        vec!["model 0.420 not ≥ 0.05 below the midpoint 0.450"]
    );
    // A wide YES book carries no midpoint: the bucket is not considered.
    let w = i_world("0.40", "0.50", "0.54", "0.55");
    let v = views(&afternoon(), NOW, good.clone());
    assert_eq!(one(&w, &v), None);
    // A cheap bucket is not the middle.
    let w = i_world("0.10", "0.12", "0.88", "0.89");
    assert_eq!(one(&w, &v), None);
}

// ---------------------------------------------------------------------------
// J — morning maker
// ---------------------------------------------------------------------------

const J_NOW: &str = "2026-07-01T06:32:00Z"; // 08:32 local

fn morning() -> Vec<(&'static str, i32)> {
    vec![("2026-07-01T05:55:00Z", 130), ("2026-07-01T06:25:00Z", 140)]
}

#[test]
fn j_quotes_both_sides_inside_the_spread_in_the_morning() {
    let mut w = World::new(J_NOW);
    let m = w.m.clone();
    w.book(yes(&m, 19), Some("0.40"), Some("0.44"));
    w.book(no(&m, 19), Some("0.56"), Some("0.60"));
    let v = views(&morning(), J_NOW, None);
    let out = w.run(&mut MorningMaker::new(MorningMakerConfig::default()), &v);
    assert_eq!(out.proposals.len(), 2, "{:?}", out.evaluations);
    let expiry = TimeInForce::Gtd {
        expires_at: utc("2026-07-01T06:45:00Z"),
    };
    let y = &out.proposals[0];
    assert_eq!(
        (y.outcome_side, y.limit_price),
        (OutcomeSide::Yes, p("0.41"))
    );
    assert_eq!(y.shares, Shares::from_whole(24));
    assert_eq!(y.tif, expiry);
    let n = &out.proposals[1];
    assert_eq!(
        (n.outcome_side, n.limit_price),
        (OutcomeSide::No, p("0.57"))
    );
    assert_eq!(n.shares, Shares::from_whole(17));
    // The pair costs 0.98: two cents of spread if both fill.
    assert_eq!(y.limit_price.saturating_add(n.limit_price), p("0.98"));
    // Fair value is the midpoint: each side earns a cent against it.
    assert!((y.ev_per_share - 0.01).abs() < 1e-9, "{}", y.ev_per_share);
}

#[test]
fn j_blockers() {
    let m = market(J_NOW);
    let v = views(&morning(), J_NOW, None);
    // A one-tick book leaves nothing to earn.
    let mut tight = World::new(J_NOW);
    tight.book(yes(&m, 19), Some("0.40"), Some("0.41"));
    tight.book(no(&m, 19), Some("0.59"), Some("0.60"));
    let out = tight.run(&mut MorningMaker::new(MorningMakerConfig::default()), &v);
    assert!(out.proposals.is_empty());
    assert_eq!(
        blockers_of(&out, "19°C", OutcomeSide::Yes),
        vec!["spread 0.01 outside [0.02, 0.05]"]
    );
    // Within the ten minutes before a report.
    let mut late = World::new("2026-07-01T06:47:00Z");
    late.book(yes(&m, 19), Some("0.40"), Some("0.44"));
    late.book(no(&m, 19), Some("0.56"), Some("0.60"));
    let v_late = views(&morning(), "2026-07-01T06:47:00Z", None);
    let out = late.run(
        &mut MorningMaker::new(MorningMakerConfig::default()),
        &v_late,
    );
    assert!(out.proposals.is_empty());
    assert!(blockers_of(&out, "19°C", OutcomeSide::No)[0].starts_with("within 10′"));
    // After 11:00 local J shows nothing at all.
    let mut noon = World::new("2026-07-01T10:00:00Z");
    noon.book(yes(&m, 19), Some("0.40"), Some("0.44"));
    let v_noon = views(&afternoon()[..1], "2026-07-01T11:01:00Z", None);
    noon.now = utc("2026-07-01T11:01:00Z");
    let out = noon.run(
        &mut MorningMaker::new(MorningMakerConfig::default()),
        &v_noon,
    );
    assert!(out.evaluations.is_empty() && out.proposals.is_empty());
}

// ---------------------------------------------------------------------------
// K — KNMI nowcast
// ---------------------------------------------------------------------------

const K_NOW: &str = "2026-07-01T11:47:00Z";

/// The 11:25Z report reached 18 °C.
fn rising() -> Vec<(&'static str, i32)> {
    vec![
        ("2026-07-01T10:25:00Z", 170),
        ("2026-07-01T10:55:00Z", 175),
        ("2026-07-01T11:25:00Z", 180),
    ]
}

fn reading(end: &str, mean: i32, max: i32) -> TenMinuteObservation {
    TenMinuteObservation {
        station: eham(),
        provider: ProviderId::knmi(),
        interval_end: utc(end),
        mean: Some(TempC::from_tenths(mean)),
        max: Some(TempC::from_tenths(max)),
        radiation: None,
        received_at: utc(end) + Duration::minutes(4),
    }
}

fn k_world(no_bid: &str, no_ask: &str, n: Option<TenMinuteObservation>) -> World {
    let mut w = World::new(K_NOW);
    let m = w.m.clone();
    w.book(no(&m, 18), Some(no_bid), Some(no_ask));
    w.nowcast = n;
    w
}

#[test]
fn k_buys_the_no_of_the_high_when_knmi_is_already_above_the_next_degree() {
    let w = k_world(
        "0.40",
        "0.42",
        Some(reading("2026-07-01T11:40:00Z", 189, 191)),
    );
    let v = views(&rising(), K_NOW, None);
    let out = w.run(&mut KnmiNowcast::new(KnmiNowcastConfig::default()), &v);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let pr = &out.proposals[0];
    assert_eq!(pr.bucket_label, "18°C");
    assert_eq!((pr.side, pr.outcome_side), (Side::Buy, OutcomeSide::No));
    assert_eq!(pr.limit_price, p("0.42"));
    assert_eq!(pr.shares, Shares::from_whole(59), "$25 at 0.42");
    assert_eq!(pr.tif, TimeInForce::Fak);
    assert!(pr.rationale[0].contains("KNMI 11:30–11:40 UTC mean 18.9 °C max 19.1 °C"));
}

/// The 11:40–11:50 reading arrives at 11:56: the 11:55 METAR has been taken
/// but not published (the last one known is 11:25), so the reading still
/// leads it by five minutes — not the 35 to the 12:25 report after `now`.
#[test]
fn k_trades_a_reading_that_arrives_after_its_report_minute() {
    let now = "2026-07-01T11:56:00Z";
    let mut w = World::new(now);
    let m = w.m.clone();
    w.book(no(&m, 18), Some("0.40"), Some("0.42"));
    w.nowcast = Some(reading("2026-07-01T11:50:00Z", 189, 191));
    let v = views(&rising(), now, None);
    let out = w.run(&mut KnmiNowcast::new(KnmiNowcastConfig::default()), &v);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    assert_eq!(out.proposals[0].bucket_label, "18°C");
}

#[test]
fn k_blockers() {
    let v = views(&rising(), K_NOW, None);
    let one = |w: &World| {
        let out = w.run(&mut KnmiNowcast::new(KnmiNowcastConfig::default()), &v);
        blockers_of(&out, "18°C", OutcomeSide::No)
    };
    // Not warm enough: 18.7 < 18.8.
    let w = k_world(
        "0.40",
        "0.42",
        Some(reading("2026-07-01T11:40:00Z", 187, 191)),
    );
    assert_eq!(
        one(&w),
        vec!["KNMI mean 18.7 °C < 18.8 °C (high 18 + 0.5 + 0.3 °C)"]
    );
    // The reading ended before the last METAR (11:25): stale.
    let w = k_world(
        "0.40",
        "0.42",
        Some(reading("2026-07-01T11:20:00Z", 189, 191)),
    );
    let b = one(&w);
    assert!(
        b.contains(&"KNMI reading not newer than the last METAR".into()),
        "{b:?}"
    );
    // Too far ahead of the next report (11:30 reading, report at 11:55).
    let mut early = k_world(
        "0.40",
        "0.42",
        Some(reading("2026-07-01T11:30:00Z", 189, 191)),
    );
    early.now = utc("2026-07-01T11:34:00Z");
    let v_early = views(&rising(), "2026-07-01T11:34:00Z", None);
    let out = early.run(
        &mut KnmiNowcast::new(KnmiNowcastConfig::default()),
        &v_early,
    );
    assert_eq!(
        blockers_of(&out, "18°C", OutcomeSide::No),
        vec!["reading 25 min before the next report > 16"]
    );
    // The market already knows: the NO costs 0.80.
    let w = k_world(
        "0.78",
        "0.80",
        Some(reading("2026-07-01T11:40:00Z", 189, 191)),
    );
    let b = one(&w);
    assert!(
        b.contains(&"NO ask 0.80 outside [0.02, 0.75]".into()),
        "{b:?}"
    );
    // No KNMI reading at all: K does nothing.
    let w = k_world("0.40", "0.42", None);
    assert_eq!(one(&w), vec!["no KNMI ten-minute reading"]);
}

// ---------------------------------------------------------------------------
// The unwind engine leaves G–K's positions alone
// ---------------------------------------------------------------------------

#[test]
fn unwind_exits_f_but_not_the_strategies_that_hold_to_settlement() {
    let m = market(NOW);
    let o = m.outcome_for_value(19).unwrap();
    let inst = InstrumentRef {
        token: o.yes_token.clone(),
        condition_id: o.condition_id.clone(),
        event_slug: m.event_slug.clone(),
        outcome_side: OutcomeSide::Yes,
        bucket: o.bucket,
    };
    let mut pos = PositionBook::new();
    pos.apply_fill(
        &Fill {
            client_order_id: ClientOrderId::new("h1").unwrap(),
            token: inst.token.clone(),
            side: Side::Buy,
            price: p("0.12"),
            shares: Shares::from_whole(83),
            fee: Usd::ZERO,
            liquidity: Liquidity::Taker,
            ts: utc(NOW),
        },
        &inst,
    )
    .unwrap();
    // The model gives 19 °C 2 %: below the 50 % exit level.
    let v = views(&afternoon(), NOW, Some(dist(&[0.975, 0.02, 0.004, 0.001])));
    let mut books = HashMap::new();
    books.insert(
        inst.token.clone(),
        synthetic_book(&inst.token, Some("0.05"), Some("0.07"), 200, utc(NOW)),
    );
    let exits = |owner: &str| {
        let mut u = UnwindEngine::new(UnwindConfig::default());
        let owners: HashMap<TokenId, StrategyId> = [(
            inst.token.clone(),
            StrategyId::new(owner.to_owned()).unwrap(),
        )]
        .into_iter()
        .collect();
        u.evaluate(&m, &pos, &books, &v, &HashSet::new(), &owners, utc(NOW))
            .len()
    };
    assert_eq!(exits("F_peak_slot"), 1, "F's position is exited");
    for s in [
        "G_tail_seller",
        "H_next_degree",
        "I_middle_fade",
        "J_morning_maker",
        "K_knmi_nowcast",
    ] {
        assert_eq!(exits(s), 0, "{s} holds to settlement");
    }
}
