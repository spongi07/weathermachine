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
    BuyNoAboveHigh, BuyNoConfig, BuyYesConfig, BuyYesFinalHigh, IncrementDistribution, PeakDetectionEngine, SplitUnwind, SplitUnwindConfig,
    Strategy, StrategyContext, TemperatureStateEngine, UnwindConfig, UnwindEngine, UnwindStyle, ViewEvaluation, ViewKind,
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
        key: ObservationKey { station: eham(), observed_at: utc(t), report_type: ReportType::Metar },
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
    IncrementDistribution { probs: p.to_vec(), support, source: "test".into() }
}

/// Views derived through the real state and peak engines.
fn views(series: &[(&str, i32)], now: &str, d: Option<IncrementDistribution>) -> Vec<ViewEvaluation> {
    let mut e = TemperatureStateEngine::new(5);
    e.register_station(eham(), Amsterdam);
    for (t, v) in series {
        e.apply_observation(&obs(t, *v));
    }
    let now = utc(now);
    let s = e.day_state(&eham(), date(), ViewKind::All, now).unwrap();
    let a = PeakDetectionEngine::default().assess(&s, Amsterdam, now).unwrap();
    vec![ViewEvaluation { view: ViewKind::All, assessment: a, distribution: d }]
}

/// High 18 at 12:00Z, declining to 17 by 13:30Z (90 minutes observed).
fn confirmed_series() -> Vec<(&'static str, i32)> {
    vec![("2026-07-01T11:00:00Z", 170), ("2026-07-01T11:30:00Z", 175), ("2026-07-01T12:00:00Z", 180), ("2026-07-01T12:30:00Z", 178), ("2026-07-01T13:00:00Z", 175), ("2026-07-01T13:30:00Z", 170)]
}

const NOW: &str = "2026-07-01T13:32:00Z";

fn market() -> DailyTemperatureMarket {
    synthetic_temperature_market(&LocationId::new("amsterdam").unwrap(), &eham(), date(), Amsterdam, 13, 24, utc(NOW))
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
    b.insert(yes(m, 18), synthetic_book(&yes(m, 18), Some("0.93"), Some("0.95"), 200, now));
    b.insert(yes(m, 19), synthetic_book(&yes(m, 19), Some("0.02"), Some("0.04"), 200, now));
    b.insert(no(m, 19), synthetic_book(&no(m, 19), Some("0.91"), Some("0.93"), 200, now));
    b.insert(no(m, 20), synthetic_book(&no(m, 20), Some("0.96"), Some("0.97"), 200, now));
    b.insert(no(m, 21), synthetic_book(&no(m, 21), Some("0.98"), Some("0.985"), 200, now));
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
    StrategyContext { now: utc(NOW), mode: RunMode::Paper, location: loc, market: m, books, views, positions, pending_tokens: pending }
}

fn good_dist() -> IncrementDistribution {
    dist(&[0.985, 0.012, 0.002, 0.001], 500)
}

#[test]
fn strategy_a_signals_with_edge_after_confirmation() {
    let m = market();
    let b = books(&m);
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut a = BuyYesFinalHigh::new(BuyYesConfig::default(), m.fees);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert_eq!(out.proposals.len(), 1, "{:?}", out.evaluations);
    let p = &out.proposals[0];
    assert_eq!(p.bucket_label, "18°C");
    assert_eq!((p.side, p.outcome_side, p.kind), (Side::Buy, OutcomeSide::Yes, IntentKind::Open));
    assert_eq!(p.limit_price, Price::parse("0.95").unwrap());
    assert_eq!(p.shares, Shares::from_whole(10));
    assert!(p.weather_dependent);
    assert!((p.p_win - 0.985).abs() < 1e-12);
    assert!(p.ev_per_share > 0.02 && p.ev_per_share < 0.03, "{}", p.ev_per_share);
    assert_eq!(p.tif, TimeInForce::Fak);
}

#[test]
fn strategy_a_blockers() {
    let m = market();
    let b = books(&m);
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut a = BuyYesFinalHigh::new(BuyYesConfig::default(), m.fees);
    // Not confirmed yet (30 minutes).
    let early = views(&confirmed_series()[..4], "2026-07-01T12:31:00Z", Some(good_dist()));
    let out = a.evaluate(&ctx(&m, &b, &early, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(out.evaluations[0].blockers.iter().any(|x| x.starts_with("confirmation")));
    // No model ⇒ fail closed.
    let v = views(&confirmed_series(), NOW, None);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(out.evaluations[0].blockers.contains(&"no probability model".to_owned()));
    // Insufficient edge.
    let v = views(&confirmed_series(), NOW, Some(dist(&[0.955, 0.03, 0.01, 0.005], 500)));
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    assert!(out.evaluations[0].blockers.iter().any(|x| x.starts_with("edge")));
    // Price outside research range.
    let mut b2 = books(&m);
    b2.insert(yes(&m, 18), synthetic_book(&yes(&m, 18), Some("0.99"), Some("0.995"), 200, utc(NOW)));
    let v = views(&confirmed_series(), NOW, Some(good_dist()));
    let out = a.evaluate(&ctx(&m, &b2, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
    // Stale order book.
    let mut b3 = books(&m);
    b3.insert(yes(&m, 18), synthetic_book(&yes(&m, 18), Some("0.93"), Some("0.95"), 200, utc("2026-07-01T13:00:00Z")));
    let out = a.evaluate(&ctx(&m, &b3, &v, &pos, &pending, &loc));
    assert!(out.evaluations[0].blockers.contains(&"order book stale".to_owned()));
    // Pending order on the token ⇒ no duplicate.
    let pending_yes: HashSet<TokenId> = [yes(&m, 18)].into();
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending_yes, &loc));
    assert!(out.proposals.is_empty());
    // Low model support.
    let v = views(&confirmed_series(), NOW, Some(dist(&[0.985, 0.012, 0.002, 0.001], 10)));
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.proposals.is_empty());
}

#[test]
fn strategy_a_uses_most_conservative_view_and_refuses_disagreement() {
    let m = market();
    let b = books(&m);
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut a = BuyYesFinalHigh::new(BuyYesConfig::default(), m.fees);
    let mut v = views(&confirmed_series(), NOW, Some(good_dist()));
    let mut second = v[0].clone();
    second.distribution = Some(dist(&[0.96, 0.03, 0.007, 0.003], 500));
    v.push(second);
    let out = a.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!((out.evaluations[0].p_win.unwrap() - 0.96).abs() < 1e-12, "min across views");
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
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut s = BuyNoAboveHigh::new(BuyNoConfig::default(), m.fees);
    let out = s.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert_eq!(out.evaluations.len(), 3);
    let labels: Vec<_> = out.proposals.iter().map(|p| p.bucket_label.as_str()).collect();
    assert_eq!(labels, vec!["19°C", "20°C"], "21°C edge too small at 0.985");
    let no19 = &out.proposals[0];
    assert_eq!(no19.outcome_side, OutcomeSide::No);
    assert!((no19.p_win - 0.988).abs() < 1e-12);
    // NO on the tail bucket counts the tail mass as potential loss.
    let e21 = out.evaluations.iter().find(|e| e.bucket_label == "21°C").unwrap();
    assert!((e21.p_win.unwrap() - 0.999).abs() < 1e-12);
}

#[test]
fn strategy_b_skips_bucket_containing_the_high() {
    let m = market();
    let b = books(&m);
    let series = vec![("2026-07-01T12:00:00Z", 250), ("2026-07-01T13:30:00Z", 240)];
    let v = views(&series, NOW, Some(good_dist()));
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut s = BuyNoAboveHigh::new(BuyNoConfig::default(), m.fees);
    let out = s.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc));
    assert!(out.evaluations.is_empty(), "≥24 bucket contains the high 25");
}

#[test]
fn strategy_c_is_research_only_and_disabled_by_default() {
    let m = market();
    let b = books(&m);
    let series = vec![("2026-07-01T11:30:00Z", 170), ("2026-07-01T12:00:00Z", 180)];
    let v = views(&series, "2026-07-01T12:02:00Z", Some(dist(&[0.60, 0.35, 0.04, 0.01], 500)));
    let (pos, pending, loc) = (PositionBook::new(), HashSet::new(), LocationId::new("amsterdam").unwrap());
    let mut c = SplitUnwind::new(SplitUnwindConfig::default());
    assert!(c.research_only());
    assert!(c.evaluate(&ctx(&m, &b, &v, &pos, &pending, &loc)).proposals.is_empty());
    let mut b2 = b.clone();
    b2.insert(yes(&m, 18), synthetic_book(&yes(&m, 18), Some("0.55"), Some("0.58"), 200, utc(NOW)));
    b2.insert(yes(&m, 19), synthetic_book(&yes(&m, 19), Some("0.30"), Some("0.33"), 200, utc(NOW)));
    let mut c = SplitUnwind::new(SplitUnwindConfig { enabled: true, ..SplitUnwindConfig::default() });
    let out = c.evaluate(&ctx(&m, &b2, &v, &pos, &pending, &loc));
    assert_eq!(out.proposals.len(), 2);
    assert!(out.proposals.iter().all(|p| p.research_only));
}

fn hold(book: &mut PositionBook, m: &DailyTemperatureMarket, v: i32, side: OutcomeSide, price: &str) -> TokenId {
    let o = m.outcome_for_value(v).unwrap();
    let token = o.token(side).clone();
    let inst = InstrumentRef { token: token.clone(), condition_id: o.condition_id.clone(), event_slug: m.event_slug.clone(), outcome_side: side, bucket: o.bucket };
    let fill = Fill { client_order_id: ClientOrderId::new("x").unwrap(), token: token.clone(), side: Side::Buy, price: Price::parse(price).unwrap(), shares: Shares::from_whole(10), fee: Usd::ZERO, liquidity: Liquidity::Taker, ts: utc(NOW) };
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
    b.insert(yes18.clone(), synthetic_book(&yes18, Some("0.01"), Some("0.02"), 200, utc(NOW)));
    let mut u = UnwindEngine::new(UnwindConfig::default());
    let props = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc(NOW));
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].token, yes18);
    assert_eq!((props[0].side, props[0].kind), (Side::Sell, IntentKind::Reduce));
    assert!(!props[0].weather_dependent, "risk-reducing exits are never blocked by stale data");
    assert_eq!(props[0].limit_price, Price::parse("0.01").unwrap());
    assert!(props.iter().all(|p| p.token != no19));
}

#[test]
fn progressive_unwind_steps_toward_bid() {
    let m = market();
    let mut pos = PositionBook::new();
    let yes18 = hold(&mut pos, &m, 18, OutcomeSide::Yes, "0.95");
    let v = views(&confirmed_series(), NOW, Some(dist(&[0.30, 0.50, 0.15, 0.05], 500)));
    let mut b = books(&m);
    b.insert(yes18.clone(), synthetic_book(&yes18, Some("0.30"), Some("0.40"), 200, utc(NOW)));
    let cfg = UnwindConfig {
        style: UnwindStyle::Progressive { start_offset: Price::parse("0.05").unwrap(), step: Price::parse("0.02").unwrap(), step_secs: 60 },
        ..UnwindConfig::default()
    };
    let mut u = UnwindEngine::new(cfg);
    let p0 = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc(NOW));
    assert_eq!(p0[0].limit_price, Price::parse("0.35").unwrap());
    assert_eq!(p0[0].tif, TimeInForce::Gtc);
    let p2 = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc("2026-07-01T13:34:00Z"));
    assert_eq!(p2[0].limit_price, Price::parse("0.31").unwrap());
    let p5 = u.evaluate(&m, &pos, &b, &v, &HashSet::new(), utc("2026-07-01T13:37:00Z"));
    assert_eq!(p5[0].limit_price, Price::parse("0.30").unwrap());
    assert_eq!(p5[0].tif, TimeInForce::Fak);
}
