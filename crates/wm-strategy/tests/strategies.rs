#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Strategy A/B/C and unwind behaviour on synthetic markets.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Europe::Amsterdam;
use std::collections::{HashMap, HashSet};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, StationId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::portfolio::{InstrumentRef, PositionBook};
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{Fill, IntentKind, Liquidity, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{Observation, ObservationKey, QualityFlags, ReportType, TempPrecision};
use wm_strategy::{
    BuyNoAboveHigh, BuyNoConfig, BuyYesConfig, BuyYesFinalHigh, IncrementDistribution,
    PeakDetectionEngine, SplitUnwind, SplitUnwindConfig, Strategy, StrategyContext,
    TemperatureStateEngine, UnwindConfig, UnwindEngine, UnwindStyle, ViewEvaluation, ViewKind,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
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

fn dist(p: &[f64], support: u32) -> IncrementDistribution {
    IncrementDistribution {
        probs: p.to_vec(),
        support,
        source: "test".into(),
    }
}

/// Views derived through the real state and peak engines.
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

/// High 18 at 12:00Z, declining to 17 by 13:30Z (90 minutes observed).
fn confirmed_series() -> Vec<(&'static str, i32)> {
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

fn market() -> DailyTemperatureMarket {
    synthetic_temperature_market(
        &LocationId::new("amsterdam").unwrap(),
        &eham(),
        date(),
        Amsterdam,
        13,
        24,
        utc(NOW),
    )
}

fn yes(m: &DailyTemperatureMarket, v: i32) -> TokenId {
    m.outcome_for_value(v).unwrap().yes_token.clone()
}

fn no(m: &DailyTemperatureMarket, v: i32) -> TokenId {
    m.outcome_for_value(v).unwrap().no_token.clone()
}

fn books(m: &DailyTemperatureMarket) -> HashMap<TokenId, OrderBook> {
    let now = utc(NOW);
    let mut b = HashMap::new();
    b.insert(
        yes(m, 18),
        synthetic_book(&yes(m, 18), Some("0.93"), Some("0.95"), 200, now),
    );
    b.insert(
        yes(m, 19),
        synthetic_book(&yes(m, 19), Some("0.02"), Some("0.04"), 200, now),
    );
    b.insert(
        no(m, 19),
        synthetic_book(&no(m, 19), Some("0.91"), Some("0.93"), 200, now),
    );
    b.insert(
        no(m, 20),
        synthetic_book(&no(m, 20), Some("0.96"), Some("0.97"), 200, now),
    );
    b.insert(
        no(m, 21),
        synthetic_book(&no(m, 21), Some("0.98"), Some("0.985"), 200, now),
    );
    b
}

fn ctx<'a>(
    m: &'a DailyTemperatureMarket,
    books: &'a HashMap<TokenId, OrderBook>,
    views: &'a [ViewEvaluation],
    positions: &'a PositionBook,
    pending: &'a HashSet<TokenId>,
    loc: &'a LocationId,
) -> StrategyContext<'a> {
    StrategyContext {
        now: utc(NOW),
        mode: RunMode::Paper,
        location: loc,
        market: m,
        books,
        views,
        positions,
        pending_tokens: pending,
        peak_times: None,
    }
}

fn good_dist() -> IncrementDistribution {
    dist(&[0.985, 0.012, 0.002, 0.001], 500)
}

/// Strategy A without market pooling: tests of the model's own logic.
fn model_only_yes() -> BuyYesConfig {
    BuyYesConfig {
        market_weight: 0.0,
        ..BuyYesConfig::default()
    }
}

/// Strategy B without market pooling.
fn model_only_no() -> BuyNoConfig {
    BuyNoConfig {
        market_weight: 0.0,
        ..BuyNoConfig::default()
    }
}

#[test]
fn strategy_a_signals_with_edge_after_confirmation() {
    let m = market();
    let b = books(&m);
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut a = BuyYesFinalHigh::new(model_only_yes());
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let p = &out.proposals[0];
    assert_eq!(p.bucket_label, "18°C");
    assert_eq!(
        (p.side, p.outcome_side, p.kind),
        (Side::Buy, OutcomeSide::Yes, IntentKind::Open)
    );
    assert_eq!(p.limit_price, Price::parse("0.95").unwrap());
    assert_eq!(p.shares, Shares::from_whole(10));
    assert!(p.weather_dependent);
    assert!((p.p_win - 0.985).abs() < 1e-12);
    assert!(
        p.ev_per_share > 0.02 && p.ev_per_share < 0.03,
        "{}",
        p.ev_per_share
    );
    assert_eq!(p.tif, TimeInForce::Fak);
}

#[test]
fn strategy_a_blockers() {
    let m = market();
    let b = books(&m);
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut a = BuyYesFinalHigh::new(BuyYesConfig::default());
    // Not confirmed yet (30 minutes).
    let early = views(
        &confirmed_series()[..4],
        "2026-07-01T12:31:00Z",
        Some(good_dist()),
    );
    let out = a.evaluate(&ctx(&m, &b, &early, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(
        out.evaluations[0]
            .blockers
            .iter()
            .any(|x| x.starts_with("confirmation"))
    );
    // No model ⇒ fail closed.
    let v = views(&confirmed_series(), NOW, None);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(
        out.evaluations[0]
            .blockers
            .contains(&"no probability model".to_owned())
    );
    // Insufficient edge.
    let v = views(
        &confirmed_series(),
        NOW,
        Some(dist(&[0.955, 0.03, 0.01, 0.005], 500)),
    );
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(
        out.evaluations[0]
            .blockers
            .iter()
            .any(|x| x.starts_with("edge"))
    );
    // Price outside research range.
    let mut b2 = books(&m);
    b2.insert(
        yes(&m, 18),
        synthetic_book(&yes(&m, 18), Some("0.99"), Some("0.995"), 200, utc(NOW)),
    );
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let out = a.evaluate(&ctx(&m, &b2, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    // Stale order book.
    let mut b3 = books(&m);
    b3.insert(
        yes(&m, 18),
        synthetic_book(
            &yes(&m, 18),
            Some("0.93"),
            Some("0.95"),
            200,
            utc("2026-07-01T13:00:00Z"),
        ),
    );
    let out = a.evaluate(&ctx(&m, &b3, &v, &pos, &pending, &loc));
    assert!(
        out.evaluations[0]
            .blockers
            .contains(&"order book stale".to_owned())
    );
    // Pending order on the token ⇒ no duplicate.
    let pending_yes: HashSet<TokenId> = [yes(&m, 18)].into();
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending_yes, &loc));
    assert!(out.proposals.is_empty());
    // Low model support.
    let v = views(
        &confirmed_series(),
        NOW,
        Some(dist(&[0.985, 0.012, 0.002, 0.001], 10)),
    );
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
}

#[test]
fn strategy_a_uses_most_conservative_view_and_refuses_disagreement() {
    let m = market();
    let b = books(&m);
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut a = BuyYesFinalHigh::new(model_only_yes());
    let mut v = views(&confirmed_series(), NOW, Some(good_dist()));
    let mut second = v[0].clone();
    second.distribution = Some(dist(&[0.96, 0.03, 0.007, 0.003], 500));
    v.push(second);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(
        (out.evaluations[0].p_win.unwrap() - 0.96).abs() < 1e-12,
        "min across views"
    );
    assert!(out.proposals.is_empty(), "0.96 − 0.95 − fees < min edge");
    // Views disagreeing on the high ⇒ nothing at all.
    let mut v = views(&confirmed_series(), NOW, Some(good_dist()));
    let mut other = v[0].clone();
    other.assessment.features.high_whole = 19;
    v.push(other);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty() && out.evaluations.is_empty());
}

#[test]
fn strategy_b_evaluates_each_distance_separately() {
    let m = market();
    let b = books(&m);
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut s = BuyNoAboveHigh::new(model_only_no());
    let out = s.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert_eq!(out.evaluations.len(), 3);
    let labels: Vec<_> = out
        .proposals
        .iter()
        .map(|p| p.bucket_label.as_str())
        .collect();
    assert_eq!(labels, vec!["19°C", "20°C"], "21°C edge too small at 0.985");
    let no19 = &out.proposals[0];
    assert_eq!(no19.outcome_side, OutcomeSide::No);
    assert!((no19.p_win - 0.988).abs() < 1e-12);
    // NO on the tail bucket counts the tail mass as potential loss.
    let e21 = out
        .evaluations
        .iter()
        .find(|e| e.bucket_label == "21°C")
        .unwrap();
    assert!((e21.p_win.unwrap() - 0.999).abs() < 1e-12);
}

#[test]
fn strategy_b_skips_bucket_containing_the_high() {
    let m = market();
    let b = books(&m);
    let series = vec![("2026-07-01T12:00:00Z", 250), ("2026-07-01T13:30:00Z", 240)];
    let v = views(&series, NOW, Some(good_dist()));
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut s = BuyNoAboveHigh::new(BuyNoConfig::default());
    let out = s.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(
        out.evaluations.is_empty(),
        "≥24 bucket contains the high 25"
    );
}

#[test]
fn strategy_c_is_research_only_and_disabled_by_default() {
    let m = market();
    let b = books(&m);
    let series = vec![("2026-07-01T11:30:00Z", 170), ("2026-07-01T12:00:00Z", 180)];
    let v = views(
        &series,
        "2026-07-01T12:02:00Z",
        Some(dist(&[0.60, 0.35, 0.04, 0.01], 500)),
    );
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let mut c = SplitUnwind::new(SplitUnwindConfig::default());
    assert!(c.research_only());
    assert!(
        c.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc))
            .proposals
            .is_empty()
    );
    let mut b2 = b.clone();
    b2.insert(
        yes(&m, 18),
        synthetic_book(&yes(&m, 18), Some("0.55"), Some("0.58"), 200, utc(NOW)),
    );
    b2.insert(
        yes(&m, 19),
        synthetic_book(&yes(&m, 19), Some("0.30"), Some("0.33"), 200, utc(NOW)),
    );
    let mut c = SplitUnwind::new(SplitUnwindConfig {
        enabled: true,
        ..SplitUnwindConfig::default()
    });
    let out = c.evaluate(&ctx(&m, &b2, &v, &pos, &pending, &loc));
    assert_eq!(out.proposals.len(), 2);
    assert!(out.proposals.iter().all(|p| p.research_only));
}

fn hold(
    book: &mut PositionBook,
    m: &DailyTemperatureMarket,
    v: i32,
    side: OutcomeSide,
    price: &str,
) -> TokenId {
    let o = m.outcome_for_value(v).unwrap();
    let token = o.token(side).clone();
    let inst = InstrumentRef {
        token: token.clone(),
        condition_id: o.condition_id.clone(),
        event_slug: m.event_slug.clone(),
        outcome_side: side,
        bucket: o.bucket,
    };
    let fill = Fill {
        client_order_id: ClientOrderId::new("x").unwrap(),
        token: token.clone(),
        side: Side::Buy,
        price: Price::parse(price).unwrap(),
        shares: Shares::from_whole(10),
        fee: Usd::ZERO,
        liquidity: Liquidity::Taker,
        ts: utc(NOW),
    };
    book.apply_fill(&fill, &inst).unwrap();
    token
}

#[test]
fn unwind_exits_yes_when_high_breaks_and_holds_certain_no() {
    let m = market();
    let mut pos = PositionBook::new();
    let yes18 = hold(&mut pos, &m, 18, OutcomeSide::Yes, "0.95");
    let no19 = hold(&mut pos, &m, 19, OutcomeSide::No, "0.93");
    // New high 20 observed: YES 18 can no longer win; NO 19 can no longer lose.
    let series = vec![("2026-07-01T12:00:00Z", 180), ("2026-07-01T13:30:00Z", 200)];
    let v = views(&series, NOW, None);
    let mut b = books(&m);
    b.insert(
        yes18.clone(),
        synthetic_book(&yes18, Some("0.01"), Some("0.02"), 200, utc(NOW)),
    );
    let mut u = UnwindEngine::new(UnwindConfig::default());
    let props = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc(NOW));
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].token, yes18);
    assert_eq!(
        (props[0].side, props[0].kind),
        (Side::Sell, IntentKind::Reduce)
    );
    assert!(
        !props[0].weather_dependent,
        "risk-reducing exits are never blocked by stale data"
    );
    assert_eq!(props[0].limit_price, Price::parse("0.01").unwrap());
    assert!(props.iter().all(|p| p.token != no19));
}

#[test]
fn progressive_unwind_steps_toward_bid() {
    let m = market();
    let mut pos = PositionBook::new();
    let yes18 = hold(&mut pos, &m, 18, OutcomeSide::Yes, "0.95");
    let v = views(
        &confirmed_series(),
        NOW,
        Some(dist(&[0.30, 0.50, 0.15, 0.05], 500)),
    );
    let mut b = books(&m);
    b.insert(
        yes18.clone(),
        synthetic_book(&yes18, Some("0.30"), Some("0.40"), 200, utc(NOW)),
    );
    let cfg = UnwindConfig {
        style: UnwindStyle::Progressive {
            start_offset: Price::parse("0.05").unwrap(),
            step: Price::parse("0.02").unwrap(),
            step_secs: 60,
        },
        ..UnwindConfig::default()
    };
    let mut u = UnwindEngine::new(cfg);
    let p0 = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc(NOW));
    assert_eq!(p0[0].limit_price, Price::parse("0.35").unwrap());
    assert_eq!(p0[0].tif, TimeInForce::Gtc);
    let p2 = u.evaluate(
        &m,
        &pos,
        &b,
        &v,
        &HashSet::new(),
        utc("2026-07-01T13:34:00Z"),
    );
    assert_eq!(p2[0].limit_price, Price::parse("0.31").unwrap());
    let p5 = u.evaluate(
        &m,
        &pos,
        &b,
        &v,
        &HashSet::new(),
        utc("2026-07-01T13:37:00Z"),
    );
    assert_eq!(p5[0].limit_price, Price::parse("0.30").unwrap());
    assert_eq!(p5[0].tif, TimeInForce::Fak);
}

// ---------------------------------------------------------------------------
// Strategy D — outcomes the observations have decided
// ---------------------------------------------------------------------------

use wm_strategy::{CertainConfig, CertainOutcomes};

fn book_at(token: &TokenId, bid: Option<&str>, ask: Option<&str>) -> OrderBook {
    synthetic_book(token, bid, ask, 200, utc(NOW))
}

#[test]
fn strategy_d_buys_no_on_every_bucket_below_the_observed_high() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    // High 18 (up from 17.5): buckets ≤13 … 17 are decided NO.
    let v = views(&confirmed_series(), NOW, None);
    let mut b = HashMap::new();
    b.insert(no(&m, 17), book_at(&no(&m, 17), Some("0.85"), Some("0.90"))); // stale
    b.insert(
        no(&m, 16),
        book_at(&no(&m, 16), Some("0.99"), Some("0.995")),
    ); // repriced
    let (pos, pend) = (PositionBook::new(), HashSet::new());
    let mut d = CertainOutcomes::new(CertainConfig::default());
    let out = d.evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert_eq!(out.evaluations.len(), 5, "≤13, 14, 15, 16, 17");
    assert!(
        out.evaluations
            .iter()
            .all(|e| e.outcome_side == OutcomeSide::No)
    );
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let p = &out.proposals[0];
    assert_eq!(p.bucket_label, "17°C");
    assert_eq!(p.token, no(&m, 17));
    assert_eq!(p.limit_price, Price::parse("0.90").unwrap());
    assert!((p.p_win - 1.0).abs() < 1e-12);
    // 1 − 0.90 − fee 0.05·0.9·0.1 − slippage 0.002
    assert!(
        (p.ev_per_share - (0.1 - 0.0045 - 0.002)).abs() < 1e-9,
        "{}",
        p.ev_per_share
    );
    assert!(p.weather_dependent && p.kind == IntentKind::Open && p.tif == TimeInForce::Fak);
    let blocker = |label: &str| {
        out.evaluations
            .iter()
            .find(|e| e.bucket_label == label)
            .map(|e| e.blockers.join("; "))
            .unwrap()
    };
    assert!(
        blocker("16°C").contains("ask 0.995 > 0.99"),
        "{}",
        blocker("16°C")
    );
    assert!(blocker("15°C").contains("no order book"));
    // Buckets at or above the high are never decided by the observations.
    assert!(out.evaluations.iter().all(|e| e.bucket_label != "18°C"));
    // The market's view is recorded next to the certainty, not used.
    let e = |label: &str| {
        out.evaluations
            .iter()
            .find(|e| e.bucket_label == label)
            .unwrap()
    };
    assert_eq!(e("17°C").model_p, Some(1.0));
    assert!((e("17°C").market_p.unwrap() - 0.875).abs() < 1e-12);
    assert!((e("16°C").market_p.unwrap() - 0.9925).abs() < 1e-12);
    assert_eq!(e("15°C").market_p, None);
}

#[test]
fn strategy_d_buys_yes_on_a_reached_open_top_bucket() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    let v = views(
        &[
            ("2026-07-01T12:00:00Z", 230),
            ("2026-07-01T12:30:00Z", 235),
            ("2026-07-01T13:00:00Z", 240),
        ],
        NOW,
        None,
    );
    let top = m.outcome_for_value(24).unwrap().yes_token.clone();
    let mut b = HashMap::new();
    b.insert(top.clone(), book_at(&top, Some("0.60"), Some("0.62")));
    let (pos, pend) = (PositionBook::new(), HashSet::new());
    let out = CertainOutcomes::new(CertainConfig::default())
        .evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    let yes_props: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| p.outcome_side == OutcomeSide::Yes)
        .collect();
    assert_eq!(yes_props.len(), 1);
    assert_eq!(yes_props[0].token, top);
    assert_eq!(yes_props[0].bucket_label, "24°C or higher");
}

#[test]
fn strategy_d_waits_for_a_second_report_after_an_implausible_jump() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    let mut b = HashMap::new();
    b.insert(no(&m, 18), book_at(&no(&m, 18), Some("0.80"), Some("0.85")));
    let (pos, pend) = (PositionBook::new(), HashSet::new());
    let mut d = CertainOutcomes::new(CertainConfig::default());
    // 15.0 → 19.0 in one report: not trusted yet.
    let jump = views(
        &[("2026-07-01T12:30:00Z", 150), ("2026-07-01T13:00:00Z", 190)],
        NOW,
        None,
    );
    let out = d.evaluate(&ctx(&m, &b, &jump, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
    let e = out
        .evaluations
        .iter()
        .find(|e| e.bucket_label == "18°C")
        .unwrap();
    assert!(
        e.blockers.iter().any(|x| x.contains("jumped 4.0 °C")),
        "{:?}",
        e.blockers
    );
    // The next report repeats 19: the high is confirmed.
    let repeated = views(
        &[
            ("2026-07-01T12:30:00Z", 150),
            ("2026-07-01T13:00:00Z", 190),
            ("2026-07-01T13:30:00Z", 190),
        ],
        NOW,
        None,
    );
    let out = d.evaluate(&ctx(&m, &b, &repeated, &pos, &pend, &loc));
    assert_eq!(out.proposals.len(), 1);
    assert_eq!(out.proposals[0].token, no(&m, 18));
}

#[test]
fn strategy_d_only_trusts_the_high_every_view_has_seen() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    // View 1 saw 18; view 2 (e.g. hourly rows only) has 17 so far.
    let mut v = views(&confirmed_series(), NOW, None);
    let lower = views(
        &[
            ("2026-07-01T11:00:00Z", 165),
            ("2026-07-01T11:30:00Z", 170),
            ("2026-07-01T13:30:00Z", 170),
        ],
        NOW,
        None,
    );
    v.extend(lower);
    let mut b = HashMap::new();
    b.insert(no(&m, 17), book_at(&no(&m, 17), Some("0.85"), Some("0.90")));
    b.insert(no(&m, 16), book_at(&no(&m, 16), Some("0.90"), Some("0.95")));
    let (pos, pend) = (PositionBook::new(), HashSet::new());
    let out = CertainOutcomes::new(CertainConfig::default())
        .evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert!(
        out.evaluations.iter().all(|e| e.bucket_label != "17°C"),
        "17 is not decided in view 2"
    );
    assert_eq!(out.proposals.len(), 1);
    assert_eq!(out.proposals[0].token, no(&m, 16));
}

#[test]
fn strategy_d_leaves_the_settlement_discount_of_long_dead_buckets_alone() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    let v = views(&confirmed_series(), NOW, None);
    let mut b = HashMap::new();
    // 14 °C died hours ago; its NO is quoted where every settled bucket is.
    b.insert(no(&m, 14), book_at(&no(&m, 14), Some("0.98"), Some("0.99")));
    let (pos, pend) = (PositionBook::new(), HashSet::new());
    let out = CertainOutcomes::new(CertainConfig::default())
        .evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
    let e = out
        .evaluations
        .iter()
        .find(|e| e.bucket_label == "14°C")
        .unwrap();
    // 1 − 0.99 − fee 0.000495 − slippage 0.002 = 0.0075 < 0.02
    assert!(
        e.blockers.iter().any(|x| x == "edge 0.0075 < 0.0200"),
        "{:?}",
        e.blockers
    );
}

#[test]
fn strategy_d_respects_books_positions_data_age_and_edge() {
    let m = market();
    let loc = LocationId::new("amsterdam").unwrap();
    let v = views(&confirmed_series(), NOW, None);
    let pend = HashSet::new();
    let mut d = CertainOutcomes::new(CertainConfig::default());
    // Stale book.
    let mut b = HashMap::new();
    let mut stale = book_at(&no(&m, 17), Some("0.85"), Some("0.90"));
    stale.received_at = utc(NOW) - chrono::Duration::seconds(30);
    b.insert(no(&m, 17), stale);
    let pos = PositionBook::new();
    let out = d.evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
    // Too little edge at 0.994 (below the cap is not enough).
    let mut b = HashMap::new();
    b.insert(no(&m, 17), book_at(&no(&m, 17), Some("0.98"), Some("0.99")));
    let loose = CertainConfig {
        min_edge: 0.009,
        ..CertainConfig::default()
    };
    let out = CertainOutcomes::new(loose).evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
    assert!(
        out.evaluations
            .iter()
            .any(|e| e.blockers.iter().any(|x| x.starts_with("edge")))
    );
    // Already positioned.
    let mut b = HashMap::new();
    b.insert(no(&m, 17), book_at(&no(&m, 17), Some("0.85"), Some("0.90")));
    let mut held = PositionBook::new();
    hold(&mut held, &m, 17, OutcomeSide::No, "0.90");
    let out = d.evaluate(&ctx(&m, &b, &v, &held, &pend, &loc));
    assert!(out.proposals.is_empty());
    // Weather data older than 40 minutes.
    let old = views(&confirmed_series(), "2026-07-01T14:20:00Z", None);
    let out = d.evaluate(&ctx(&m, &b, &old, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
    assert!(
        out.evaluations
            .iter()
            .any(|e| e.blockers.iter().any(|x| x.contains("too old")))
    );
    // Disabled.
    let off = CertainConfig {
        enabled: false,
        ..CertainConfig::default()
    };
    let out = CertainOutcomes::new(off).evaluate(&ctx(&m, &b, &v, &pos, &pend, &loc));
    assert!(out.proposals.is_empty());
}

// ---------------------------------------------------------------------------
// Market pooling: the book as information
// ---------------------------------------------------------------------------

use wm_strategy::{Pooling, log_pool};

fn pooling(weight: f64) -> Pooling {
    Pooling {
        weight,
        max_spread: Price::parse("0.10").unwrap(),
        max_book_age_ms: 15_000,
    }
}

#[test]
fn log_pool_averages_log_odds() {
    // σ(½·logit 0.9 + ½·logit 0.5) = σ(½·ln 9) = σ(ln 3) = 0.75
    assert!((log_pool(0.9, Some(0.5), 0.5) - 0.75).abs() < 1e-12);
    assert!((log_pool(0.9, Some(0.5), 0.5) - log_pool(0.5, Some(0.9), 0.5)).abs() < 1e-12);
    assert_eq!(log_pool(0.9, None, 0.5), 0.9);
    assert_eq!(log_pool(0.9, Some(0.5), 0.0), 0.9);
    assert!((log_pool(0.9, Some(0.5), 1.0) - 0.5).abs() < 1e-12);
    // Weights outside 0..=1 are clamped.
    assert!((log_pool(0.9, Some(0.5), 7.0) - 0.5).abs() < 1e-12);
    assert_eq!(log_pool(0.9, Some(0.5), -1.0), 0.9);
    // Certainties are clamped, so opposite certainties meet in the middle.
    assert!((log_pool(1.0, Some(0.0), 0.5) - 0.5).abs() < 1e-9);
    assert!(log_pool(1.0, Some(0.99), 0.5).is_finite());
    // Between the inputs and increasing in both.
    let a = log_pool(0.95, Some(0.90), 0.5);
    assert!(a > 0.90 && a < 0.95);
    assert!(a < log_pool(0.95, Some(0.92), 0.5) && a < log_pool(0.96, Some(0.90), 0.5));
}

#[test]
fn pooled_win_probability_never_exceeds_the_model() {
    let p = pooling(0.5);
    assert!((p.win_probability(0.9, Some(0.5)) - 0.75).abs() < 1e-12);
    assert_eq!(
        p.win_probability(0.9, Some(0.99)),
        0.9,
        "a more confident market does not raise the model"
    );
    assert_eq!(p.win_probability(0.9, None), 0.9);
    assert_eq!(pooling(0.0).win_probability(0.9, Some(0.1)), 0.9);
}

#[test]
fn market_probability_needs_a_fresh_tight_two_sided_book() {
    let m = market();
    let now = utc(NOW);
    let (own, other) = (yes(&m, 18), no(&m, 18));
    let p = pooling(0.5);
    let tight = book_at(&own, Some("0.93"), Some("0.95"));
    let comp = book_at(&other, Some("0.04"), Some("0.08"));
    let mp = |a: Option<&OrderBook>, b: Option<&OrderBook>, t| p.market_probability(a, b, t);
    assert!((mp(Some(&tight), Some(&comp), now).unwrap() - 0.94).abs() < 1e-12);
    // A wide or one-sided own book: one minus the complement's midpoint.
    let wide = book_at(&own, Some("0.80"), Some("0.95"));
    assert!((mp(Some(&wide), Some(&comp), now).unwrap() - 0.94).abs() < 1e-12);
    assert_eq!(mp(Some(&wide), None, now), None);
    let one_sided = book_at(&own, None, Some("0.95"));
    assert!((mp(Some(&one_sided), Some(&comp), now).unwrap() - 0.94).abs() < 1e-12);
    // A spread exactly at the limit still counts.
    let at_limit = book_at(&own, Some("0.85"), Some("0.95"));
    assert!((mp(Some(&at_limit), None, now).unwrap() - 0.90).abs() < 1e-12);
    // A stale book carries no probability.
    let later = now + chrono::Duration::seconds(16);
    assert_eq!(mp(Some(&tight), Some(&comp), later), None);
    // A crossed book is not a price.
    let crossed = book_at(&own, Some("0.96"), Some("0.95"));
    assert_eq!(mp(Some(&crossed), None, now), None);
    assert_eq!(mp(None, None, now), None);
}

#[test]
fn strategy_a_lets_the_market_veto_but_not_create_a_trade() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let run = |cfg: BuyYesConfig, b: &HashMap<TokenId, OrderBook>| {
        BuyYesFinalHigh::new(cfg).evaluate(&ctx(&m, b, &v, &pos, &pending, &loc))
    };
    // Default weight ½: model 0.985 pooled with the 0.93/0.95 book (mid 0.94).
    let out = run(BuyYesConfig::default(), &books(&m));
    let e = &out.evaluations[0];
    let pooled = log_pool(0.985, Some(0.94), 0.5);
    assert!((pooled - 0.969_765).abs() < 1e-6);
    assert!((e.model_p.unwrap() - 0.985).abs() < 1e-12);
    assert!((e.market_p.unwrap() - 0.94).abs() < 1e-12);
    assert!((e.p_win.unwrap() - pooled).abs() < 1e-9);
    assert_eq!(
        out.proposals.len(),
        1,
        "0.9698 − 0.95 − fee − slippage ≥ 0.01"
    );
    let p = &out.proposals[0];
    assert!((p.p_win - pooled).abs() < 1e-9);
    assert!(
        p.rationale
            .iter()
            .any(|r| r.contains("market 0.9400") && r.contains("used 0.9698")),
        "{:?}",
        p.rationale
    );
    // A market that disagrees (mid 0.915) vetoes what the model alone would buy.
    let mut b = books(&m);
    b.insert(
        yes(&m, 18),
        book_at(&yes(&m, 18), Some("0.88"), Some("0.95")),
    );
    let out = run(BuyYesConfig::default(), &b);
    assert!(out.proposals.is_empty());
    let why = out.evaluations[0].blockers.join("; ");
    assert!(
        why.contains("market 0.915 pulls model 0.985 to 0.964"),
        "{why}"
    );
    assert_eq!(
        run(model_only_yes(), &b).proposals.len(),
        1,
        "model alone buys"
    );
    // A wide book is no probability: the model decides alone.
    let mut b = books(&m);
    b.insert(
        yes(&m, 18),
        book_at(&yes(&m, 18), Some("0.80"), Some("0.95")),
    );
    let out = run(BuyYesConfig::default(), &b);
    assert_eq!(out.evaluations[0].market_p, None);
    assert!((out.evaluations[0].p_win.unwrap() - 0.985).abs() < 1e-12);
    assert_eq!(out.proposals.len(), 1);
    assert!(
        out.proposals[0]
            .rationale
            .iter()
            .any(|r| r.starts_with("model only"))
    );
}

#[test]
fn strategy_b_pools_each_bucket_with_its_own_book() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let (pos, pending, loc) = (
        PositionBook::new(),
        HashSet::new(),
        LocationId::new("amsterdam").unwrap(),
    );
    let run = |b: &HashMap<TokenId, OrderBook>| {
        BuyNoAboveHigh::new(BuyNoConfig::default()).evaluate(&ctx(&m, b, &v, &pos, &pending, &loc))
    };
    let eval = |out: &wm_strategy::StrategyOutput, label: &str| {
        out.evaluations
            .iter()
            .find(|e| e.bucket_label == label)
            .unwrap()
            .clone()
    };
    let out = run(&books(&m));
    let labels: Vec<_> = out
        .proposals
        .iter()
        .map(|p| p.bucket_label.as_str())
        .collect();
    assert_eq!(labels, vec!["19°C", "20°C"]);
    let e19 = eval(&out, "19°C");
    assert!((e19.model_p.unwrap() - 0.988).abs() < 1e-12);
    assert!(
        (e19.market_p.unwrap() - 0.92).abs() < 1e-12,
        "the NO book's midpoint"
    );
    assert!((e19.p_win.unwrap() - log_pool(0.988, Some(0.92), 0.5)).abs() < 1e-12);
    assert!(
        eval(&out, "21°C")
            .blockers
            .iter()
            .any(|b| b.starts_with("edge"))
    );
    // A wide NO book: the YES book's midpoint (3 %) speaks for the NO side.
    let mut b = books(&m);
    b.insert(no(&m, 19), book_at(&no(&m, 19), Some("0.80"), Some("0.93")));
    let e19 = eval(&run(&b), "19°C");
    assert!((e19.market_p.unwrap() - 0.97).abs() < 1e-12);
    assert!((e19.p_win.unwrap() - log_pool(0.988, Some(0.97), 0.5)).abs() < 1e-12);
    // A YES book pricing 20 °C at 12 % vetoes the NO at 0.97.
    let mut b = books(&m);
    b.insert(no(&m, 20), book_at(&no(&m, 20), Some("0.80"), Some("0.97")));
    b.insert(
        yes(&m, 20),
        book_at(&yes(&m, 20), Some("0.10"), Some("0.14")),
    );
    let out = run(&b);
    assert!(out.proposals.iter().all(|p| p.bucket_label != "20°C"));
    let why = eval(&out, "20°C").blockers.join("; ");
    assert!(why.contains("market 0.880 pulls model 0.998"), "{why}");
    // A market more confident than the model never raises the probability.
    let mut b = books(&m);
    b.insert(no(&m, 19), book_at(&no(&m, 19), Some("0.70"), Some("0.93")));
    b.insert(
        yes(&m, 19),
        book_at(&yes(&m, 19), Some("0.001"), Some("0.003")),
    );
    let e19 = eval(&run(&b), "19°C");
    assert!((e19.market_p.unwrap() - 0.998).abs() < 1e-12);
    assert!((e19.p_win.unwrap() - 0.988).abs() < 1e-12);
}

// ---------------------------------------------------------------------------
// Strategy E — the high, confirmed by clock, temperature and a shrinking book
// ---------------------------------------------------------------------------

use wm_core::market::BookLevel;
use wm_strategy::{BookConfirmedConfig, BookConfirmedHigh, StrategyOutput};

/// A book with one bid and the given asks (price, shares).
fn ladder(token: &TokenId, at: &str, bid: &str, asks: &[(&str, i64)]) -> OrderBook {
    let mut b = synthetic_book(token, Some(bid), None, 200, utc(at));
    b.asks = asks
        .iter()
        .map(|(p, s)| BookLevel {
            price: Price::parse(p).unwrap(),
            size: Shares::from_whole(*s),
        })
        .collect();
    b
}

/// The high's YES book 32 minutes before `NOW`: 300 shares from 0.94.
fn e_then(m: &DailyTemperatureMarket) -> OrderBook {
    ladder(
        &yes(m, 18),
        "2026-07-01T13:00:00Z",
        "0.93",
        &[("0.94", 100), ("0.95", 100), ("0.96", 100)],
    )
}

/// The fixture books with the high's YES book at `NOW` offering `asks`.
fn e_books(m: &DailyTemperatureMarket, asks: &[(&str, i64)]) -> HashMap<TokenId, OrderBook> {
    let mut b = books(m);
    b.insert(yes(m, 18), ladder(&yes(m, 18), NOW, "0.94", asks));
    b
}

/// Buyers lifted 0.94 and part of 0.95: 300 → 160 shares offered.
fn e_shrunk(m: &DailyTemperatureMarket) -> HashMap<TokenId, OrderBook> {
    e_books(m, &[("0.95", 60), ("0.96", 100)])
}

/// Strategy E with its history fed `then` (as the engine does on every
/// book update), evaluated at `NOW`.
fn run_e(
    m: &DailyTemperatureMarket,
    cfg: BookConfirmedConfig,
    then: Option<&OrderBook>,
    b: &HashMap<TokenId, OrderBook>,
    v: &[ViewEvaluation],
    pending: &HashSet<TokenId>,
) -> StrategyOutput {
    let mut e = BookConfirmedHigh::new(cfg);
    if let Some(t) = then {
        e.observe_book(t);
    }
    let (pos, loc) = (PositionBook::new(), LocationId::new("amsterdam").unwrap());
    e.evaluate(&ctx(m, b, v, &pos, pending, &loc))
}

#[test]
fn strategy_e_buys_the_high_once_the_temperature_fell_and_the_book_shrank() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let out = run_e(
        &m,
        BookConfirmedConfig::default(),
        Some(&e_then(&m)),
        &e_shrunk(&m),
        &v,
        &HashSet::new(),
    );
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let p = &out.proposals[0];
    assert_eq!(p.strategy.as_str(), "E_book_confirmed_high");
    assert_eq!(p.bucket_label, "18°C");
    assert_eq!(
        (p.side, p.outcome_side, p.kind, p.tif),
        (
            Side::Buy,
            OutcomeSide::Yes,
            IntentKind::Open,
            TimeInForce::Fak
        )
    );
    assert_eq!(p.limit_price, Price::parse("0.95").unwrap());
    assert_eq!(p.shares, Shares::from_whole(10));
    assert!(p.weather_dependent && !p.research_only);
    assert!(
        p.rationale[0].starts_with("15:32 local: high 18°C first reached 90m ago")
            && p.rationale[0].contains("1.0 °C below"),
        "{:?}",
        p.rationale
    );
    assert!(
        p.rationale[1].contains("300 → 160 shares offered ≤ 0.96 in 30m (−47%)")
            && p.rationale[1].ends_with("best ask 0.94 → 0.95"),
        "{:?}",
        p.rationale
    );
    let e = &out.evaluations[0];
    assert!(e.signal && e.blockers.is_empty(), "{e:?}");
    // Shown, not a gate: the model pooled with the book's midpoint, never
    // above the model, and the EV at that probability.
    assert!((e.model_p.unwrap() - 0.985).abs() < 1e-12);
    assert!((e.market_p.unwrap() - 0.945).abs() < 1e-9);
    let p_win = e.p_win.unwrap();
    assert!(p_win > 0.945 && p_win <= 0.985, "{p_win}");
    assert!((p.p_win - p_win).abs() < 1e-12);
    assert!(e.ev_per_share.is_some() && e.break_even.is_some());
}

#[test]
fn strategy_e_blockers() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let d = BookConfirmedConfig::default();
    let then = e_then(&m);
    let shrunk = e_shrunk(&m);
    let none = HashSet::new();
    let blocked = |cfg: BookConfirmedConfig,
                   then: Option<&OrderBook>,
                   b: &HashMap<TokenId, OrderBook>,
                   pending: &HashSet<TokenId>,
                   want: &str| {
        let out = run_e(&m, cfg, then, b, &v, pending);
        assert!(out.proposals.is_empty(), "{want}: {:?}", out.proposals);
        let e = &out.evaluations[0];
        assert!(
            !e.signal && e.blockers.iter().any(|x| x.contains(want)),
            "want '{want}', got {:?}",
            e.blockers
        );
    };
    let with = |f: fn(&mut BookConfirmedConfig)| {
        let mut c = d.clone();
        f(&mut c);
        c
    };
    blocked(
        with(|c| c.enabled = false),
        Some(&then),
        &shrunk,
        &none,
        "strategy disabled",
    );
    // The clock.
    blocked(
        with(|c| c.start_local_minute = 16 * 60),
        Some(&then),
        &shrunk,
        &none,
        "15:32 outside 16:00–18:00",
    );
    blocked(
        with(|c| c.end_local_minute = 15 * 60),
        Some(&then),
        &shrunk,
        &none,
        "15:32 outside 12:00–15:00",
    );
    // The temperature: the high held long enough, and fallen far enough.
    blocked(
        with(|c| c.min_minutes_at_high = 120),
        Some(&then),
        &shrunk,
        &none,
        "high reached 90m ago < 120m",
    );
    blocked(
        with(|c| c.min_drop_tenths = 20),
        Some(&then),
        &shrunk,
        &none,
        "1.0 °C below the high < 2.0",
    );
    blocked(
        with(|c| c.max_data_age_minutes = 1),
        Some(&then),
        &shrunk,
        &none,
        "weather data too old",
    );
    // The book: history, shrink, a held ask, depth to start from.
    blocked(
        d.clone(),
        None,
        &shrunk,
        &none,
        "book history shorter than 30m",
    );
    blocked(
        d.clone(),
        Some(&then),
        &e_books(&m, &[("0.94", 100), ("0.95", 100), ("0.96", 90)]),
        &none,
        "book not shrinking: 300 → 290 shares offered ≤ 0.96 in 30m (−3%, need −30%)",
    );
    blocked(
        d.clone(),
        Some(&ladder(
            &yes(&m, 18),
            "2026-07-01T13:00:00Z",
            "0.95",
            &[("0.96", 300)],
        )),
        &shrunk,
        &none,
        "best ask fell 0.96 → 0.95",
    );
    blocked(
        d.clone(),
        Some(&ladder(
            &yes(&m, 18),
            "2026-07-01T13:00:00Z",
            "0.93",
            &[("0.94", 20), ("0.95", 20)],
        )),
        &e_books(&m, &[("0.95", 5), ("0.96", 100)]),
        &none,
        "only 40 shares offered ≤ 0.95 30m ago (< 50)",
    );
    // The price range, a fresh book, no stacking.
    blocked(
        d.clone(),
        Some(&then),
        &e_books(&m, &[("0.995", 60)]),
        &none,
        "ask 0.995 outside [0.90, 0.99]",
    );
    let mut stale = shrunk.clone();
    stale.get_mut(&yes(&m, 18)).unwrap().received_at = utc("2026-07-01T13:31:00Z");
    blocked(d.clone(), Some(&then), &stale, &none, "order book stale");
    blocked(
        d.clone(),
        Some(&then),
        &shrunk,
        &HashSet::from([yes(&m, 18)]),
        "already positioned",
    );
}

#[test]
fn strategy_e_needs_a_model_and_can_require_its_agreement() {
    let m = market();
    let then = e_then(&m);
    let b = e_shrunk(&m);
    let none = HashSet::new();
    let veto = |p: f64| BookConfirmedConfig {
        min_model_p: p,
        ..BookConfirmedConfig::default()
    };
    let blockers = |out: &StrategyOutput| out.evaluations[0].blockers.clone();
    // Fail closed: no model, no trade — with or without a veto.
    let no_model = views(&confirmed_series(), NOW, None);
    for p in [0.0, 0.9] {
        let out = run_e(&m, veto(p), Some(&then), &b, &no_model, &none);
        assert_eq!(blockers(&out), vec!["no probability model".to_owned()]);
        assert!(out.proposals.is_empty());
    }
    // A model below the bar vetoes; at or above it the rule goes through.
    let modelled = views(&confirmed_series(), NOW, Some(good_dist()));
    let out = run_e(&m, veto(0.99), Some(&then), &b, &modelled, &none);
    assert_eq!(blockers(&out), vec!["model 0.985 < 0.990".to_owned()]);
    let out = run_e(&m, veto(0.98), Some(&then), &b, &modelled, &none);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
}

#[test]
fn strategy_e_sees_the_book_between_evaluations() {
    // The engine feeds every book update; evaluations may be sparse.
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let mut e = BookConfirmedHigh::new(BookConfirmedConfig::default());
    for (at, asks) in [
        ("2026-07-01T12:50:00Z", &[("0.93", 200), ("0.95", 200)][..]),
        ("2026-07-01T13:01:00Z", &[("0.94", 150), ("0.95", 200)][..]),
        ("2026-07-01T13:20:00Z", &[("0.95", 120)][..]),
    ] {
        e.observe_book(&ladder(&yes(&m, 18), at, "0.92", asks));
    }
    // 30 minutes before NOW (13:02) the 13:01 state was in force: 350 shares.
    let (pos, loc, none) = (
        PositionBook::new(),
        LocationId::new("amsterdam").unwrap(),
        HashSet::new(),
    );
    let b = e_books(&m, &[("0.95", 100), ("0.97", 100)]);
    let out = e.evaluate(&ctx(&m, &b, &v, &pos, &none, &loc));
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    assert!(
        out.proposals[0].rationale[1].contains("350 → 100 shares offered ≤ 0.95"),
        "{:?}",
        out.proposals[0].rationale
    );
}

// ---------------------------------------------------------------------------
// Strategy F — the high's bucket inside the season's peak slot
// ---------------------------------------------------------------------------

use wm_strategy::{PeakSlotConfig, PeakSlotHigh, PeakTimes, PeakTimesBuilder, SeasonSlots};

/// Strategy F evaluated at `NOW` (15:32 local, summer) on `b`.
fn run_f(
    m: &DailyTemperatureMarket,
    cfg: PeakSlotConfig,
    b: &HashMap<TokenId, OrderBook>,
    v: &[ViewEvaluation],
    pending: &HashSet<TokenId>,
    peak_times: Option<&PeakTimes>,
) -> StrategyOutput {
    let (pos, loc) = (PositionBook::new(), LocationId::new("amsterdam").unwrap());
    let c = StrategyContext {
        peak_times,
        ..ctx(m, b, v, &pos, pending, &loc)
    };
    PeakSlotHigh::new(cfg).evaluate(&c)
}

/// Summer peak times whose median → 90 % slot is `from`–`to` (local minutes,
/// half-hourly reports at :25 and :55).
fn summer_peaks(from: u16, to: u16) -> PeakTimes {
    let start = utc("2026-06-30T22:25:00Z");
    let mut b = PeakTimesBuilder::new();
    // Five days peaking at `from`, five at `to`: the median is `from`, the
    // 90th percentile `to`.
    for peak in std::iter::repeat_n(from, 5).chain(std::iter::repeat_n(to, 5)) {
        let points: Vec<wm_strategy::ObsPoint> = (0..48u16)
            .map(|i| {
                let minute = (25 + 30 * i) % 1440;
                wm_strategy::ObsPoint {
                    observed_at: start + chrono::Duration::minutes(30 * i64::from(i)),
                    local_minute_of_day: minute,
                    local_minute_of_hour: (minute % 60) as u8,
                    temp: TempC::from_whole(if minute == peak { 25 } else { 15 }),
                    report_type: ReportType::Metar,
                    version: 1,
                }
            })
            .collect();
        b.add_day(date(), wm_core::time::Season::Summer, &points, 25);
    }
    b.build()
}

#[test]
fn strategy_f_buys_100_shares_of_the_high_inside_the_slot() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    // 60 + 60 shares at 0.93/0.94: 100 shares cost at most 0.94.
    let b = e_books(&m, &[("0.93", 60), ("0.94", 60), ("0.99", 500)]);
    let out = run_f(&m, PeakSlotConfig::default(), &b, &v, &HashSet::new(), None);
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let p = &out.proposals[0];
    assert_eq!(p.strategy.as_str(), "F_peak_slot");
    assert_eq!(p.bucket_label, "18°C");
    assert_eq!(
        (p.side, p.outcome_side, p.kind, p.tif),
        (
            Side::Buy,
            OutcomeSide::Yes,
            IntentKind::Open,
            TimeInForce::Fak
        )
    );
    assert_eq!(p.shares, Shares::from_whole(100));
    assert_eq!(p.limit_price, Price::parse("0.94").unwrap());
    assert!(p.weather_dependent && !p.research_only);
    assert_eq!(
        p.rationale[0],
        "15:32 local inside the summer slot 15:00–18:00 (fallback: peak times not learned yet)"
    );
    assert_eq!(
        p.rationale[1],
        "high 18°C bucket offered at 0.93 (> 0.90): 100 shares at ≤ 0.94"
    );
    let e = &out.evaluations[0];
    assert!(e.signal && e.blockers.is_empty(), "{e:?}");
    assert_eq!(e.ask, Some(Price::parse("0.93").unwrap()));
    // EV and break-even are shown at the price the whole size costs.
    let be = e.break_even.unwrap();
    assert!(be > 0.94 && be < 0.95, "{be}");
}

#[test]
fn strategy_f_takes_its_slot_from_the_learned_peak_times() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let b = e_books(&m, &[("0.93", 200)]);
    let none = HashSet::new();
    // History: summer highs first reported at 15:25 (median) … 17:25 (90 %).
    let inside = summer_peaks(15 * 60 + 25, 17 * 60 + 25);
    let out = run_f(&m, PeakSlotConfig::default(), &b, &v, &none, Some(&inside));
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    assert_eq!(
        out.proposals[0].rationale[0],
        "15:32 local inside the summer slot 15:25–17:26 (median → 90% of 10 days' peak times)"
    );
    // Later peaks: 15:32 is before the slot.
    let later = summer_peaks(15 * 60 + 55, 17 * 60 + 55);
    let out = run_f(&m, PeakSlotConfig::default(), &b, &v, &none, Some(&later));
    assert!(out.proposals.is_empty());
    assert_eq!(
        out.evaluations[0].blockers,
        vec!["15:32 outside the summer slot 15:55–17:56"]
    );
    // Earlier peaks: 15:32 is after the slot.
    let earlier = summer_peaks(12 * 60 + 25, 14 * 60 + 55);
    let out = run_f(&m, PeakSlotConfig::default(), &b, &v, &none, Some(&earlier));
    assert_eq!(
        out.evaluations[0].blockers,
        vec!["15:32 outside the summer slot 12:25–14:56"]
    );
    // Other quantiles move the slot: from the 90th percentile on.
    let cfg = PeakSlotConfig {
        slot_from_quantile: 0.9,
        slot_to_quantile: 1.0,
        ..PeakSlotConfig::default()
    };
    let out = run_f(&m, cfg, &b, &v, &none, Some(&inside));
    assert_eq!(
        out.evaluations[0].blockers,
        vec!["15:32 outside the summer slot 17:25–17:26"]
    );
}

#[test]
fn strategy_f_blockers() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let none = HashSet::new();
    let blockers = |cfg: PeakSlotConfig,
                    b: &HashMap<TokenId, OrderBook>,
                    pending: &HashSet<TokenId>,
                    v: &[ViewEvaluation]|
     -> Vec<String> {
        let out = run_f(&m, cfg, b, v, pending, None);
        assert!(out.proposals.is_empty());
        out.evaluations[0].blockers.clone()
    };
    let d = PeakSlotConfig::default;
    // Exactly 0.90 is not above 0.90.
    assert_eq!(
        blockers(d(), &e_books(&m, &[("0.90", 200)]), &none, &v),
        vec!["ask 0.90 not above 0.90"]
    );
    // Above the cap.
    assert_eq!(
        blockers(d(), &e_books(&m, &[("0.96", 200)]), &none, &v),
        vec!["ask 0.96 above 0.95"]
    );
    // Not all 100 shares at or below the cap.
    assert_eq!(
        blockers(
            d(),
            &e_books(&m, &[("0.93", 50), ("0.95", 40), ("0.96", 500)]),
            &none,
            &v
        ),
        vec!["only 90 shares offered ≤ 0.95 (need 100)"]
    );
    let ok = e_books(&m, &[("0.93", 200)]);
    // Outside a configured fallback slot.
    let late = PeakSlotConfig {
        fallback_slots: SeasonSlots {
            summer: (16 * 60, 18 * 60),
            ..SeasonSlots::default()
        },
        ..d()
    };
    assert_eq!(
        blockers(late, &ok, &none, &v),
        vec!["15:32 outside the summer slot 16:00–18:00"]
    );
    // The optional temperature condition: 1.0 °C below the high, 2.0 asked.
    let drop = PeakSlotConfig {
        min_drop_tenths: 20,
        ..d()
    };
    assert_eq!(
        blockers(drop, &ok, &none, &v),
        vec!["1.0 °C below the high < 2.0"]
    );
    // No model: fail closed.
    let nomodel = views(&confirmed_series(), NOW, None);
    assert_eq!(
        blockers(d(), &ok, &none, &nomodel),
        vec!["no probability model"]
    );
    // An order already pending on the token.
    let pending: HashSet<TokenId> = [yes(&m, 18)].into_iter().collect();
    assert_eq!(blockers(d(), &ok, &pending, &v), vec!["already positioned"]);
    // Disabled.
    let off = PeakSlotConfig {
        enabled: false,
        ..d()
    };
    assert_eq!(blockers(off, &ok, &none, &v), vec!["strategy disabled"]);
    // No book / no ask / a stale book.
    let mut nobook = books(&m);
    nobook.remove(&yes(&m, 18));
    assert_eq!(blockers(d(), &nobook, &none, &v), vec!["no order book"]);
    assert_eq!(blockers(d(), &e_books(&m, &[]), &none, &v), vec!["no ask"]);
    let mut stale = e_books(&m, &[("0.93", 200)]);
    stale.insert(
        yes(&m, 18),
        ladder(
            &yes(&m, 18),
            "2026-07-01T13:00:00Z",
            "0.92",
            &[("0.93", 200)],
        ),
    );
    assert_eq!(blockers(d(), &stale, &none, &v), vec!["order book stale"]);
}

#[test]
fn strategy_f_holds_no_second_position() {
    let m = market();
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let b = e_books(&m, &[("0.93", 200)]);
    let mut pos = PositionBook::new();
    hold(&mut pos, &m, 18, OutcomeSide::Yes, "0.93");
    let (none, loc) = (HashSet::new(), LocationId::new("amsterdam").unwrap());
    let out =
        PeakSlotHigh::new(PeakSlotConfig::default()).evaluate(&ctx(&m, &b, &v, &pos, &none, &loc));
    assert!(out.proposals.is_empty());
    assert_eq!(out.evaluations[0].blockers, vec!["already positioned"]);
}
