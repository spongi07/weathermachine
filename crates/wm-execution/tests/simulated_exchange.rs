#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Simulated exchange and order lifecycle behaviour.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::HashMap;
use wm_core::health::ProviderHealthState;
use wm_core::ids::{DecisionId, EventSlug, LocationId, RunId, StationId, StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, FeeSchedule, OrderBook, OutcomeSide, Side, TradePrint};
use wm_core::portfolio::PositionBook;
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{IntentKind, OrderStatus, OrderUpdate, RunMode, TimeInForce, TradeIntent};
use wm_core::units::{Price, Probability, Shares, Usd};
use wm_execution::{Applied, DisabledLiveVenue, ExecutionVenue, OrderError, OrderManager, SimConfig, SimulatedExchange, VenueError, VenueOrder};
use wm_risk::{ApprovedIntent, PortfolioView, RiskConfig, RiskDecision, RiskEngine, RiskInputs, WeatherStatus};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

const T0: &str = "2026-07-01T14:00:00Z";

fn market() -> DailyTemperatureMarket {
    synthetic_temperature_market(&LocationId::new("amsterdam").unwrap(), &StationId::new("EHAM").unwrap(), NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), chrono_tz::Europe::Amsterdam, 13, 24, utc(T0))
}

fn approve(m: &DailyTemperatureMarket, book: &OrderBook, price: &str, shares: i64, tif: TimeInForce, decision: u64) -> ApprovedIntent {
    let o = m.outcome_for_value(18).unwrap();
    let intent = TradeIntent {
        decision_id: DecisionId(decision),
        strategy: StrategyId::new("A_buy_yes_final_high").unwrap(),
        created_at: utc(T0),
        location: m.location.clone(),
        event_slug: m.event_slug.clone(),
        condition_id: o.condition_id.clone(),
        token: o.yes_token.clone(),
        outcome_side: OutcomeSide::Yes,
        bucket_label: o.label.clone(),
        side: Side::Buy,
        kind: IntentKind::Open,
        weather_dependent: true,
        limit_price: Price::parse(price).unwrap(),
        shares: Shares::from_whole(shares),
        notional: Usd::ZERO,
        tif,
        model_probability: Probability::new(0.98),
        expected_value_per_share: 0.02,
        break_even_probability: 0.95,
        research_only: false,
        rationale: vec![],
    };
    let mut markets = HashMap::new();
    markets.insert(m.event_slug.clone(), m.clone());
    let positions = PositionBook::new();
    let strategies: HashMap<TokenId, StrategyId> = HashMap::new();
    let ws = WeatherStatus { station: StationId::new("EHAM").unwrap(), last_observation_at: Some(utc(T0) - Duration::minutes(5)), health: ProviderHealthState::Healthy, last_correction_at: None };
    let inputs = RiskInputs {
        now: utc(T0),
        mode: RunMode::Paper,
        kill_switch: None,
        compliance_ok: false,
        storage_ok: true,
        execution_ok: true,
        weather: Some(&ws),
        market: m,
        book: Some(book),
        portfolio: PortfolioView { positions: &positions, open_orders: &[], markets: &markets, position_strategy: &strategies },
    };
    let cfg = RiskConfig { max_spread: Price::parse("0.10").unwrap(), ..RiskConfig::default() };
    match RiskEngine::new(cfg, &RunId::deterministic(decision)).evaluate(intent, &inputs) {
        RiskDecision::Approved(a) => a,
        RiskDecision::Rejected { reasons, .. } => panic!("not approved: {reasons:?}"),
    }
}

fn yes18(m: &DailyTemperatureMarket) -> TokenId {
    m.outcome_for_value(18).unwrap().yes_token.clone()
}

#[test]
fn fak_fills_after_latency_against_current_book() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 100, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Fak, 1);
    let mut ex = SimulatedExchange::new(SimConfig { latency_ms: 250, adverse_ticks: 0 });
    ex.on_book(&book, utc(T0));
    let evs = ex.submit(&a, FeeSchedule::taker(50_000), utc(T0));
    assert_eq!(evs.len(), 1, "only the ack before latency elapses");
    assert_eq!(evs[0].update.status, OrderStatus::Live);
    let evs = ex.process_due(utc(T0) + Duration::milliseconds(250));
    assert_eq!(evs.len(), 1);
    let u = &evs[0].update;
    assert_eq!(u.status, OrderStatus::Filled);
    assert_eq!(u.filled, Shares::from_whole(10));
    let fill = evs[0].fill.as_ref().unwrap();
    assert_eq!(fill.price, Price::parse("0.95").unwrap());
    assert_eq!(fill.fee, FeeSchedule::taker(50_000).taker_fee(Price::parse("0.95").unwrap(), Shares::from_whole(10)));

    let mut om = OrderManager::new();
    om.register(&a, m.outcome_for_value(18).unwrap().bucket, utc(T0)).unwrap();
    assert_eq!(om.pending_tokens().len(), 1);
    assert!(matches!(om.apply(&u.clone()).unwrap(), Applied::Changed { newly_filled, .. } if newly_filled == Shares::from_whole(10)));
    assert_eq!(om.apply(u).unwrap(), Applied::Unchanged, "duplicate delivery is idempotent");
    assert!(om.pending_tokens().is_empty());
    let mut positions = PositionBook::new();
    let rec = om.get(a.client_order_id()).unwrap();
    positions.apply_fill(fill, &rec.instrument()).unwrap();
    assert_eq!(positions.get(&yes18(&m)).unwrap().shares, Shares::from_whole(10));
}

#[test]
fn liquidity_can_vanish_during_latency() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 100, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Fak, 2);
    let mut ex = SimulatedExchange::new(SimConfig::default());
    ex.on_book(&book, utc(T0));
    ex.submit(&a, FeeSchedule::ZERO, utc(T0));
    // The ask is lifted by someone else 100 ms later.
    let gone = synthetic_book(&yes18(&m), Some("0.94"), Some("0.97"), 100, utc(T0) + Duration::milliseconds(100));
    let evs = ex.on_book(&gone, utc(T0) + Duration::milliseconds(100));
    assert!(evs.is_empty());
    let evs = ex.process_due(utc(T0) + Duration::milliseconds(300));
    assert_eq!(evs[0].update.status, OrderStatus::Canceled);
    assert_eq!(evs[0].update.filled, Shares::ZERO);
    assert!(evs[0].fill.is_none());
}

#[test]
fn fok_is_all_or_nothing_and_missing_book_rejects() {
    let m = market();
    let thin = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 6, utc(T0));
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 100, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Fok, 3);
    let mut ex = SimulatedExchange::new(SimConfig { latency_ms: 0, adverse_ticks: 0 });
    ex.on_book(&thin, utc(T0));
    let evs = ex.submit(&a, FeeSchedule::ZERO, utc(T0));
    let last = evs.last().unwrap();
    assert_eq!(last.update.status, OrderStatus::Canceled);
    assert_eq!(last.update.filled, Shares::ZERO);
    let mut empty = SimulatedExchange::new(SimConfig { latency_ms: 0, adverse_ticks: 0 });
    let evs = empty.submit(&a, FeeSchedule::ZERO, utc(T0));
    assert_eq!(evs.last().unwrap().update.status, OrderStatus::Rejected);
}

#[test]
fn gtc_rests_then_fills_from_trades_after_queue() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 30, utc(T0));
    // Resting bid at 0.94 behind 30 shares of displayed size.
    let a = approve(&m, &book, "0.94", 10, TimeInForce::Gtc, 4);
    let mut ex = SimulatedExchange::new(SimConfig { latency_ms: 0, adverse_ticks: 0 });
    ex.on_book(&book, utc(T0));
    let evs = ex.submit(&a, FeeSchedule::taker(50_000), utc(T0));
    assert_eq!(evs.len(), 1, "ack only; nothing crossed");
    assert_eq!(ex.resting_count(), 1);
    let trade = |size: i64, price: &str| TradePrint { token: yes18(&m), price: Price::parse(price).unwrap(), size: Shares::from_whole(size), aggressor: Some(Side::Sell), ts: utc(T0) };
    assert!(ex.on_trade(&trade(25, "0.94"), utc(T0)).is_empty(), "queue ahead 30 → 5");
    let evs = ex.on_trade(&trade(8, "0.94"), utc(T0));
    assert_eq!(evs[0].update.status, OrderStatus::PartiallyFilled);
    assert_eq!(evs[0].update.filled, Shares::from_whole(3));
    assert_eq!(evs[0].fill.as_ref().unwrap().fee, Usd::ZERO, "makers pay no fee");
    let evs = ex.on_trade(&trade(1, "0.93"), utc(T0));
    assert_eq!(evs[0].update.status, OrderStatus::Filled, "trade through our price fills the rest");
    assert_eq!(ex.resting_count(), 0);
}

#[test]
fn gtc_crossed_by_book_fills_as_maker_and_gtd_expires() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.96"), 30, utc(T0));
    let mut ex = SimulatedExchange::new(SimConfig { latency_ms: 0, adverse_ticks: 0 });
    ex.on_book(&book, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Gtc, 5);
    ex.submit(&a, FeeSchedule::ZERO, utc(T0));
    let crossing = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 4, utc(T0) + Duration::seconds(5));
    let evs = ex.on_book(&crossing, utc(T0) + Duration::seconds(5));
    assert_eq!(evs[0].update.filled, Shares::from_whole(4));
    assert_eq!(evs[0].fill.as_ref().unwrap().price, Price::parse("0.95").unwrap());

    let b2 = approve(&m, &book, "0.94", 5, TimeInForce::Gtd { expires_at: utc(T0) + Duration::minutes(1) }, 6);
    ex.submit(&b2, FeeSchedule::ZERO, utc(T0));
    let evs = ex.expire(utc(T0) + Duration::minutes(2));
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].update.status, OrderStatus::Expired);
    let evs = ex.cancel(a.client_order_id(), utc(T0) + Duration::minutes(3));
    assert_eq!(evs[0].update.status, OrderStatus::Canceled);
}

#[test]
fn invalid_transitions_and_fills_are_rejected() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 100, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Fak, 7);
    let mut om = OrderManager::new();
    om.register(&a, m.outcome_for_value(18).unwrap().bucket, utc(T0)).unwrap();
    assert!(matches!(om.register(&a, m.outcome_for_value(18).unwrap().bucket, utc(T0)), Err(OrderError::Duplicate(_))));
    let mk = |status, filled: i64| OrderUpdate { client_order_id: a.client_order_id().clone(), venue_order_id: None, status, filled: Shares::from_whole(filled), avg_price: None, fee_paid: Usd::ZERO, reason: None, ts: utc(T0) };
    om.apply(&mk(OrderStatus::PartiallyFilled, 5)).unwrap();
    assert!(matches!(om.apply(&mk(OrderStatus::PartiallyFilled, 3)), Err(OrderError::InvalidFill(_))));
    assert!(matches!(om.apply(&mk(OrderStatus::Filled, 11)), Err(OrderError::InvalidFill(_))));
    om.apply(&mk(OrderStatus::Filled, 10)).unwrap();
    assert!(matches!(om.apply(&mk(OrderStatus::Live, 10)), Err(OrderError::InvalidTransition { .. })));
    let unknown = OrderUpdate { client_order_id: wm_core::ids::ClientOrderId::new("nope").unwrap(), ..mk(OrderStatus::Live, 0) };
    assert!(matches!(om.apply(&unknown), Err(OrderError::Unknown(_))));
    let _ = EventSlug::new("x");
}

#[tokio::test]
async fn live_venue_is_disabled() {
    let m = market();
    let book = synthetic_book(&yes18(&m), Some("0.94"), Some("0.95"), 100, utc(T0));
    let a = approve(&m, &book, "0.95", 10, TimeInForce::Fak, 8);
    let venue = DisabledLiveVenue;
    assert_eq!(venue.submit(VenueOrder::from(&a)).await, Err(VenueError::LiveTradingDisabled));
    assert_eq!(venue.cancel(a.client_order_id().clone()).await, Err(VenueError::LiveTradingDisabled));
}
