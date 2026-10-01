#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Risk engine behaviour, including the fail-closed guarantees.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use proptest::prelude::*;
use std::collections::HashMap;
use wm_core::health::ProviderHealthState;
use wm_core::ids::{
    ClientOrderId, DecisionId, EventSlug, LocationId, RunId, StationId, StrategyId, TokenId,
};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::portfolio::{InstrumentRef, PositionBook};
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{Fill, IntentKind, Liquidity, RunMode, TimeInForce, TradeIntent};
use wm_core::units::{Price, Probability, Shares, Usd};
use wm_risk::{
    CheckId, OpenOrderView, PortfolioView, RiskConfig, RiskDecision, RiskEngine, RiskInputs,
    StrategyCaps, WeatherStatus,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

const NOW: &str = "2026-07-01T14:00:00Z";

fn loc() -> LocationId {
    LocationId::new("amsterdam").unwrap()
}

fn market_on(day: u32) -> DailyTemperatureMarket {
    synthetic_temperature_market(
        &loc(),
        &StationId::new("EHAM").unwrap(),
        NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc(NOW),
    )
}

fn weather(health: ProviderHealthState, age_min: i64) -> WeatherStatus {
    WeatherStatus {
        station: StationId::new("EHAM").unwrap(),
        last_observation_at: Some(utc(NOW) - Duration::minutes(age_min)),
        health,
        last_correction_at: None,
        max_gap_minutes: Some(30),
    }
}

fn intent(
    m: &DailyTemperatureMarket,
    v: i32,
    side: OutcomeSide,
    price: &str,
    shares: i64,
    decision: u64,
) -> TradeIntent {
    let o = m.outcome_for_value(v).unwrap();
    TradeIntent {
        decision_id: DecisionId(decision),
        strategy: StrategyId::new("A_buy_yes_final_high").unwrap(),
        created_at: utc(NOW),
        location: loc(),
        event_slug: m.event_slug.clone(),
        condition_id: o.condition_id.clone(),
        token: o.token(side).clone(),
        outcome_side: side,
        bucket_label: o.label.clone(),
        side: Side::Buy,
        kind: IntentKind::Open,
        weather_dependent: true,
        limit_price: Price::parse(price).unwrap(),
        shares: Shares::from_whole(shares),
        notional: Usd::ZERO,
        tif: TimeInForce::Fak,
        model_probability: Probability::new(0.98),
        expected_value_per_share: 0.02,
        break_even_probability: 0.95,
        research_only: false,
        rationale: vec![],
    }
}

fn book_for(token: &TokenId, bid: &str, ask: &str) -> OrderBook {
    synthetic_book(token, Some(bid), Some(ask), 500, utc(NOW))
}

struct World {
    market: DailyTemperatureMarket,
    markets: HashMap<EventSlug, DailyTemperatureMarket>,
    positions: PositionBook,
    orders: Vec<OpenOrderView>,
    strategies: HashMap<TokenId, StrategyId>,
}

impl World {
    fn new() -> Self {
        let market = market_on(1);
        let mut markets = HashMap::new();
        markets.insert(market.event_slug.clone(), market.clone());
        Self {
            market,
            markets,
            positions: PositionBook::new(),
            orders: vec![],
            strategies: HashMap::new(),
        }
    }

    fn inputs<'a>(
        &'a self,
        book: Option<&'a OrderBook>,
        w: Option<&'a WeatherStatus>,
        mode: RunMode,
        kill: Option<&'a str>,
    ) -> RiskInputs<'a> {
        RiskInputs {
            now: utc(NOW),
            mode,
            kill_switch: kill,
            compliance_ok: false,
            storage_ok: true,
            execution_ok: true,
            weather: w,
            market: &self.market,
            book,
            portfolio: PortfolioView {
                positions: &self.positions,
                open_orders: &self.orders,
                markets: &self.markets,
                position_strategy: &self.strategies,
            },
        }
    }
}

fn engine() -> RiskEngine {
    RiskEngine::new(RiskConfig::default(), &RunId::deterministic(1))
}

fn checks(d: &RiskDecision) -> Vec<CheckId> {
    match d {
        RiskDecision::Approved(_) => vec![],
        RiskDecision::Rejected { reasons, .. } => reasons.iter().map(|r| r.check).collect(),
    }
}

#[test]
fn approves_a_clean_intent() {
    let w = World::new();
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&i.token, "0.94", "0.95");
    let ws = weather(ProviderHealthState::Healthy, 5);
    let d = engine().evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(d.is_approved(), "{:?}", checks(&d));
    if let RiskDecision::Approved(a) = d {
        assert!(a.client_order_id().as_str().starts_with("wm-"));
        assert_eq!(a.post_trade_global_exposure(), Usd::parse("9.5").unwrap());
    }
}

#[test]
fn each_gate_rejects() {
    let w = World::new();
    let base = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&base.token, "0.94", "0.95");
    let healthy = weather(ProviderHealthState::Healthy, 5);
    type Case<'a> = (
        Box<dyn Fn(&mut TradeIntent)>,
        Option<WeatherStatus>,
        Option<OrderBook>,
        RunMode,
        Option<&'a str>,
        CheckId,
    );
    let cases: Vec<Case<'_>> = vec![
        (
            Box::new(|_| {}),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Paper,
            Some("operator"),
            CheckId::KillSwitch,
        ),
        (
            Box::new(|_| {}),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Live,
            None,
            CheckId::Mode,
        ),
        (
            Box::new(|i| i.research_only = true),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::ResearchOnly,
        ),
        (
            Box::new(|_| {}),
            Some(weather(ProviderHealthState::Throttled, 5)),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::WeatherHealth,
        ),
        (
            Box::new(|_| {}),
            Some(weather(ProviderHealthState::Healthy, 50)),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::WeatherFreshness,
        ),
        (
            Box::new(|_| {}),
            None,
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::WeatherHealth,
        ),
        (
            Box::new(|_| {}),
            Some(WeatherStatus {
                max_gap_minutes: Some(180),
                ..healthy.clone()
            }),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::WeatherCoverage,
        ),
        (
            Box::new(|_| {}),
            Some(WeatherStatus {
                max_gap_minutes: None,
                ..healthy.clone()
            }),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::WeatherCoverage,
        ),
        (
            Box::new(|_| {}),
            Some(healthy.clone()),
            None,
            RunMode::Paper,
            None,
            CheckId::MarketData,
        ),
        (
            Box::new(|i| i.limit_price = Price::parse("0.955").unwrap()),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::Tick,
        ),
        (
            Box::new(|i| i.limit_price = Price::parse("0.995").unwrap()),
            Some(healthy.clone()),
            Some(book_for(&base.token, "0.99", "0.995")),
            RunMode::Paper,
            None,
            CheckId::PriceBounds,
        ),
        (
            Box::new(|_| {}),
            Some(healthy.clone()),
            Some(book_for(&base.token, "0.80", "0.95")),
            RunMode::Paper,
            None,
            CheckId::Spread,
        ),
        (
            Box::new(|i| i.shares = Shares::from_whole(10)),
            Some(healthy.clone()),
            Some(synthetic_book(
                &base.token,
                Some("0.94"),
                Some("0.95"),
                3,
                utc(NOW),
            )),
            RunMode::Paper,
            None,
            CheckId::Liquidity,
        ),
        (
            Box::new(|i| i.shares = Shares::from_whole(11)),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::PositionSize,
        ),
        (
            Box::new(|i| i.shares = Shares::from_whole(2)),
            Some(healthy.clone()),
            Some(book.clone()),
            RunMode::Paper,
            None,
            CheckId::MinSize,
        ),
    ];
    for (idx, (mutate, ws, b, mode, kill, expected)) in cases.into_iter().enumerate() {
        let mut i = base.clone();
        mutate(&mut i);
        let d = engine().evaluate(i, &w.inputs(b.as_ref(), ws.as_ref(), mode, kill));
        assert!(
            checks(&d).contains(&expected),
            "case {idx}: expected {expected:?} in {:?}",
            checks(&d)
        );
    }
}

#[test]
fn stale_book_blocks_opening() {
    let w = World::new();
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let mut book = book_for(&i.token, "0.94", "0.95");
    book.received_at = utc(NOW) - Duration::seconds(60);
    let ws = weather(ProviderHealthState::Healthy, 5);
    let d = engine().evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(checks(&d).contains(&CheckId::MarketData));
}

#[test]
fn correction_cooldown_blocks_new_weather_positions() {
    let w = World::new();
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&i.token, "0.94", "0.95");
    let mut ws = weather(ProviderHealthState::Healthy, 5);
    ws.last_correction_at = Some(utc(NOW) - Duration::minutes(3));
    let d = engine().evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(checks(&d).contains(&CheckId::CorrectionCooldown));
}

#[test]
fn duplicates_are_rejected() {
    let mut w = World::new();
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&i.token, "0.94", "0.95");
    let ws = weather(ProviderHealthState::Healthy, 5);
    let mut e = engine();
    assert!(
        e.evaluate(
            i.clone(),
            &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None)
        )
        .is_approved()
    );
    // Same decision replayed ⇒ rejected even without an open order.
    let d = e.evaluate(
        i.clone(),
        &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None),
    );
    assert!(checks(&d).contains(&CheckId::Duplicate));
    // A live order on the token blocks a new decision too.
    w.orders.push(OpenOrderView {
        client_order_id: ClientOrderId::new("c").unwrap(),
        token: i.token.clone(),
        event_slug: w.market.event_slug.clone(),
        location: loc(),
        strategy: i.strategy.clone(),
        side: Side::Buy,
        outcome_side: OutcomeSide::Yes,
        bucket: w.market.outcome_for_value(18).unwrap().bucket,
        remaining: Shares::from_whole(10),
        limit_price: Price::parse("0.95").unwrap(),
    });
    let mut i2 = i;
    i2.decision_id = DecisionId(2);
    let d = e.evaluate(i2, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(checks(&d).contains(&CheckId::Duplicate));
}

fn buy_fill(w: &mut World, m: &DailyTemperatureMarket, v: i32, side: OutcomeSide, price: &str) {
    let o = m.outcome_for_value(v).unwrap();
    let inst = InstrumentRef {
        token: o.token(side).clone(),
        condition_id: o.condition_id.clone(),
        event_slug: m.event_slug.clone(),
        outcome_side: side,
        bucket: o.bucket,
    };
    let fill = Fill {
        client_order_id: ClientOrderId::new("f").unwrap(),
        token: inst.token.clone(),
        side: Side::Buy,
        price: Price::parse(price).unwrap(),
        shares: Shares::from_whole(10),
        fee: Usd::ZERO,
        liquidity: Liquidity::Taker,
        ts: utc(NOW),
    };
    w.positions.apply_fill(&fill, &inst).unwrap();
    w.markets.insert(m.event_slug.clone(), m.clone());
}

#[test]
fn global_100_usd_limit_with_ten_dollar_positions() {
    let mut w = World::new();
    // Nine independent events each with a $9.50 YES position ⇒ $85.50 worst case.
    for day in 2..=10 {
        let m = market_on(day);
        buy_fill(&mut w, &m, 18, OutcomeSide::Yes, "0.95");
    }
    let cfg = RiskConfig {
        max_daily_new_exposure_usd: None,
        max_location_exposure_usd: None,
        max_strategy_exposure_usd: None,
        ..RiskConfig::default()
    };
    let ws = weather(ProviderHealthState::Healthy, 5);
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&i.token, "0.94", "0.95");
    let d = RiskEngine::new(cfg.clone(), &RunId::deterministic(2))
        .evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(d.is_approved(), "95.00 ≤ 100: {:?}", checks(&d));
    // One more event ⇒ 104.50 > 100 ⇒ rejected.
    buy_fill(&mut w, &market_on(11), 18, OutcomeSide::Yes, "0.95");
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 3);
    let d = RiskEngine::new(cfg, &RunId::deterministic(3))
        .evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(checks(&d).contains(&CheckId::GlobalExposure));
}

#[test]
fn correlated_legs_in_one_event_use_worst_case_not_sum() {
    let mut w = World::new();
    let m = w.market.clone();
    buy_fill(&mut w, &m, 20, OutcomeSide::No, "0.97");
    buy_fill(&mut w, &m, 21, OutcomeSide::No, "0.98");
    let e = engine();
    let summary = e.exposure(&PortfolioView {
        positions: &w.positions,
        open_orders: &w.orders,
        markets: &w.markets,
        position_strategy: &w.strategies,
    });
    // Capital 19.50; worst case = 9.70 − 0.20 = 9.50 (final 20: NO20 loses, NO21 wins 0.20).
    assert_eq!(summary.capital_deployed, Usd::parse("19.5").unwrap());
    assert_eq!(summary.global_worst_case, Usd::parse("9.5").unwrap());
}

#[test]
fn reduce_intents_are_not_blocked_by_stale_weather() {
    let mut w = World::new();
    let m = w.market.clone();
    buy_fill(&mut w, &m, 18, OutcomeSide::Yes, "0.95");
    let mut i = intent(&m, 18, OutcomeSide::Yes, "0.50", 10, 9);
    i.side = Side::Sell;
    i.kind = IntentKind::Reduce;
    i.weather_dependent = false;
    let book = book_for(&i.token, "0.50", "0.52");
    let stale = weather(ProviderHealthState::Unavailable, 300);
    let d = engine().evaluate(
        i,
        &w.inputs(Some(&book), Some(&stale), RunMode::Paper, None),
    );
    assert!(d.is_approved(), "{:?}", checks(&d));
}

#[test]
fn daily_loss_limit_blocks_new_positions() {
    let w = World::new();
    let mut e = engine();
    e.record_realized_pnl(Usd::parse("-30").unwrap(), utc(NOW));
    let i = intent(&w.market, 18, OutcomeSide::Yes, "0.95", 10, 1);
    let book = book_for(&i.token, "0.94", "0.95");
    let ws = weather(ProviderHealthState::Healthy, 5);
    let d = e.evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(checks(&d).contains(&CheckId::DailyLoss));
}

#[test]
fn order_rate_limit() {
    let w = World::new();
    let cfg = RiskConfig {
        max_orders_per_minute: 2,
        max_daily_new_exposure_usd: None,
        max_market_exposure_usd: None,
        ..RiskConfig::default()
    };
    let mut e = RiskEngine::new(cfg, &RunId::deterministic(4));
    let ws = weather(ProviderHealthState::Healthy, 5);
    for (n, v) in [(1u64, 18), (2, 19), (3, 20)] {
        let i = intent(&w.market, v, OutcomeSide::Yes, "0.95", 10, n);
        let book = book_for(&i.token, "0.94", "0.95");
        let d = e.evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
        if n < 3 {
            assert!(d.is_approved(), "{:?}", checks(&d));
        } else {
            assert!(checks(&d).contains(&CheckId::OrderRate));
        }
    }
}

/// The shipped limits with strategy F's own caps: 100 shares at ≤ 0.95.
fn with_f_caps() -> RiskConfig {
    let mut cfg = RiskConfig {
        global_max_exposure_usd: Usd::from_whole(200),
        max_location_exposure_usd: Some(Usd::from_whole(150)),
        max_daily_new_exposure_usd: Some(Usd::from_whole(160)),
        ..RiskConfig::default()
    };
    cfg.strategy_caps.insert(
        "F_peak_slot".into(),
        StrategyCaps {
            position_size_usd: Usd::from_whole(100),
            max_market_exposure_usd: Some(Usd::from_whole(110)),
            max_strategy_exposure_usd: Some(Usd::from_whole(110)),
            max_spread: None,
        },
    );
    cfg
}

fn f_intent(m: &DailyTemperatureMarket, price: &str, shares: i64, decision: u64) -> TradeIntent {
    TradeIntent {
        strategy: StrategyId::new("F_peak_slot").unwrap(),
        ..intent(m, 18, OutcomeSide::Yes, price, shares, decision)
    }
}

#[test]
fn a_strategy_with_its_own_caps_buys_100_shares_while_the_others_keep_theirs() {
    let w = World::new();
    let ws = weather(ProviderHealthState::Healthy, 5);
    let i = f_intent(&w.market, "0.94", 100, 1);
    let book = book_for(&i.token, "0.93", "0.94");
    let run = |cfg: RiskConfig, i: TradeIntent| {
        RiskEngine::new(cfg, &RunId::deterministic(7))
            .evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None))
    };
    // $94 breaks every default cap it meets.
    let c = checks(&run(RiskConfig::default(), i.clone()));
    for id in [
        CheckId::PositionSize,
        CheckId::MarketExposure,
        CheckId::LocationExposure,
        CheckId::StrategyExposure,
        CheckId::DailyNewExposure,
    ] {
        assert!(c.contains(&id), "{id:?} missing in {c:?}");
    }
    // With F's caps: approved.
    let d = run(with_f_caps(), i);
    assert!(d.is_approved(), "{:?}", checks(&d));
    // Strategy A under the same configuration keeps its $10, $30 and $60.
    let a = intent(&w.market, 18, OutcomeSide::Yes, "0.94", 100, 2);
    assert_eq!(
        checks(&run(with_f_caps(), a)),
        vec![
            CheckId::PositionSize,
            CheckId::MarketExposure,
            CheckId::StrategyExposure
        ]
    );
    // F cannot exceed its own cap: 110 shares at 0.94 cost $103.40.
    assert_eq!(
        checks(&run(with_f_caps(), f_intent(&w.market, "0.94", 110, 3))),
        vec![CheckId::PositionSize]
    );
}

#[test]
fn unfilled_cost_returns_to_the_daily_new_exposure_of_its_day() {
    let w = World::new();
    let ws = weather(ProviderHealthState::Healthy, 5);
    let mut e = RiskEngine::new(with_f_caps(), &RunId::deterministic(7));
    let i = f_intent(&w.market, "0.94", 100, 1);
    let book = book_for(&i.token, "0.93", "0.94");
    let d = e.evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None));
    assert!(d.is_approved(), "{:?}", checks(&d));
    assert_eq!(e.daily_new_exposure(), Usd::from_whole(94));
    // 40 of the 100 shares filled; the rest expired: $56.40 comes back.
    e.release_daily_new_exposure(Usd::from_micros(56_400_000), utc(NOW), utc(NOW));
    assert_eq!(e.daily_new_exposure(), Usd::from_micros(37_600_000));
    // Never below zero.
    e.release_daily_new_exposure(Usd::from_whole(500), utc(NOW), utc(NOW));
    assert_eq!(e.daily_new_exposure(), Usd::ZERO);
    // An order approved yesterday (UTC) does not touch today's counter.
    let mut e = RiskEngine::new(with_f_caps(), &RunId::deterministic(7));
    let i = f_intent(&w.market, "0.94", 100, 2);
    assert!(
        e.evaluate(i, &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None))
            .is_approved()
    );
    e.release_daily_new_exposure(Usd::from_whole(94), utc(NOW) - Duration::days(1), utc(NOW));
    assert_eq!(e.daily_new_exposure(), Usd::from_whole(94));
}

#[test]
fn a_strategy_may_have_its_own_spread_limit() {
    let w = World::new();
    let ws = weather(ProviderHealthState::Healthy, 5);
    let i = f_intent(&w.market, "0.94", 100, 1);
    // A 0.08 spread: above the default 0.05.
    let book = book_for(&i.token, "0.86", "0.94");
    let run = |cfg: RiskConfig| {
        RiskEngine::new(cfg, &RunId::deterministic(7)).evaluate(
            i.clone(),
            &w.inputs(Some(&book), Some(&ws), RunMode::Paper, None),
        )
    };
    assert_eq!(checks(&run(with_f_caps())), vec![CheckId::Spread]);
    let mut wide = with_f_caps();
    wide.strategy_caps
        .get_mut("F_peak_slot")
        .unwrap()
        .max_spread = Some(Price::parse("0.10").unwrap());
    let d = run(wide.clone());
    assert!(d.is_approved(), "{:?}", checks(&d));
    assert_eq!(
        wide.max_spread_for(&StrategyId::new("A_buy_yes_final_high").unwrap()),
        Price::parse("0.05").unwrap(),
        "the others keep the default"
    );
}

#[test]
fn strategy_caps_parse_and_are_validated() {
    let cfg = with_f_caps();
    cfg.validate().unwrap();
    assert_eq!(
        cfg.position_size_for(&StrategyId::new("F_peak_slot").unwrap()),
        Usd::from_whole(100)
    );
    assert_eq!(
        cfg.position_size_for(&StrategyId::new("A_buy_yes_final_high").unwrap()),
        Usd::from_whole(10)
    );
    for bad in [Usd::ZERO, Usd::from_whole(201)] {
        let mut c = with_f_caps();
        c.strategy_caps
            .get_mut("F_peak_slot")
            .unwrap()
            .position_size_usd = bad;
        assert!(c.validate().is_err(), "{bad}");
    }
    let toml_text = r#"
        position_size_usd = "10.00"
        global_max_exposure_usd = "200.00"
        max_spread = "0.05"
        max_price = "0.99"
        min_price = "0.01"
        max_weather_age_minutes = 40
        max_book_age_ms = 15000
        correction_cooldown_minutes = 10
        max_orders_per_minute = 6
        require_approved_resolution_spec = false
        [strategy_caps.F_peak_slot]
        position_size_usd = "100.00"
        max_market_exposure_usd = "110.00"
        max_spread = "0.10"
    "#;
    let parsed: RiskConfig = toml::from_str(toml_text).unwrap();
    let f = &parsed.strategy_caps["F_peak_slot"];
    assert_eq!(f.position_size_usd, Usd::from_whole(100));
    assert_eq!(f.max_spread, Price::parse("0.10").ok());
    assert_eq!(f.max_market_exposure_usd, Some(Usd::from_whole(110)));
    assert_eq!(
        f.max_strategy_exposure_usd, None,
        "falls back to the default"
    );
    parsed.validate().unwrap();
    // Unknown keys in a strategy's caps are refused.
    let typo = toml_text.replace("max_market_exposure_usd", "max_market_exposure");
    assert!(toml::from_str::<RiskConfig>(&typo).is_err());
}

#[test]
fn config_parses_decimal_strings_from_toml() {
    let toml_text = r#"
        position_size_usd = "10.00"
        global_max_exposure_usd = "100.00"
        max_market_exposure_usd = "30.00"
        max_spread = "0.05"
        max_price = "0.99"
        min_price = "0.01"
        max_weather_age_minutes = 40
        max_book_age_ms = 15000
        correction_cooldown_minutes = 10
        max_orders_per_minute = 6
        require_approved_resolution_spec = true
    "#;
    let cfg: RiskConfig = toml::from_str(toml_text).unwrap();
    assert_eq!(cfg.position_size_usd, Usd::from_whole(10));
    assert_eq!(cfg.global_max_exposure_usd, Usd::from_whole(100));
    assert_eq!(cfg.max_location_exposure_usd, None);
    cfg.validate().unwrap();
    let mut bad = cfg.clone();
    bad.position_size_usd = Usd::from_whole(200);
    assert!(bad.validate().is_err());
}

fn any_health() -> impl Strategy<Value = ProviderHealthState> {
    prop_oneof![
        Just(ProviderHealthState::Healthy),
        Just(ProviderHealthState::Degraded),
        Just(ProviderHealthState::Throttled),
        Just(ProviderHealthState::Stale),
        Just(ProviderHealthState::Unavailable),
    ]
}

proptest! {
    /// Fail-closed guarantee: a new weather-dependent position is approved only
    /// if the source is Healthy AND the newest observation is fresh AND no kill
    /// switch is engaged. Every provider outage/throttle/staleness ⇒ rejection.
    #[test]
    fn outages_never_approve_weather_positions(
        health in any_health(),
        age in 0i64..600,
        has_weather in any::<bool>(),
        kill in any::<bool>(),
        v in 14i32..23,
        cents in 50u32..99,
    ) {
        let w = World::new();
        let price = format!("0.{cents:02}");
        let i = intent(&w.market, v, OutcomeSide::Yes, &price, 5, 7);
        let ask = Price::parse(&price).unwrap();
        let bid = ask.saturating_sub(Price::parse("0.01").unwrap());
        let book = book_for(&i.token, &bid.to_string(), &ask.to_string());
        let ws = weather(health, age);
        let d = engine().evaluate(i, &w.inputs(Some(&book), has_weather.then_some(&ws), RunMode::Paper, kill.then_some("k")));
        let healthy_and_fresh = has_weather && health == ProviderHealthState::Healthy && age <= 40;
        if d.is_approved() {
            prop_assert!(healthy_and_fresh && !kill, "approved with health={health:?} age={age} kill={kill}");
        }
        if !healthy_and_fresh || kill {
            prop_assert!(!d.is_approved());
        }
    }
}
