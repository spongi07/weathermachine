#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The strategy lab's paper strategies in the shared session loop: each
//! trades its own paper book, so a lab strategy and strategy K can buy the
//! same token at the same moment — neither blocks the other, and each
//! position, settlement and restore stays with its own strategy.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;
use wm_backtest::SimulationSession;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, NowcastEvent, ObservationEvent,
    OrderBookEvent, ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, RunId, StationId, StrategyId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::portfolio::InstrumentRef;
use wm_core::resolution::ObservationFilter;
use wm_core::synthetic::synthetic_book;
use wm_core::trading::{Fill, Liquidity, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
    TenMinuteObservation,
};
use wm_engine::{Engine, EngineConfig, EngineLocation, RestoreState, RestoredFill};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::lab::code;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, KnmiNowcastConfig, LabConfig, NoEdgeModel, PeakConfig,
    SplitUnwindConfig, UnwindConfig,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn loc() -> LocationId {
    LocationId::new("amsterdam").unwrap()
}

/// K on the main book, and of the lab only `families`.
fn config(families: &[u8]) -> EngineConfig {
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(21),
        locations: vec![EngineLocation {
            location: loc(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            confirmed_filter: Some(ObservationFilter::AllRows),
            routine_minutes: vec![25, 55],
            position: Some((52.318, 4.790)),
            neighbours: Vec::new(),
        }],
        risk: RiskConfig::default(),
        buy_yes: BuyYesConfig {
            enabled: false,
            ..BuyYesConfig::default()
        },
        buy_no: BuyNoConfig {
            enabled: false,
            ..BuyNoConfig::default()
        },
        split_unwind: SplitUnwindConfig::default(),
        certain: wm_strategy::CertainConfig {
            enabled: false,
            ..wm_strategy::CertainConfig::default()
        },
        book_confirmed: wm_strategy::BookConfirmedConfig {
            enabled: false,
            ..wm_strategy::BookConfirmedConfig::default()
        },
        peak_slot: wm_strategy::PeakSlotConfig {
            enabled: false,
            ..wm_strategy::PeakSlotConfig::default()
        },
        tail_seller: wm_strategy::TailSellerConfig::absent(),
        next_degree: wm_strategy::NextDegreeConfig::absent(),
        middle_fade: wm_strategy::MiddleFadeConfig::absent(),
        morning_maker: wm_strategy::MorningMakerConfig::absent(),
        knmi_nowcast: KnmiNowcastConfig {
            notional: Usd::from_whole(10),
            ..KnmiNowcastConfig::default()
        },
        lab: LabConfig {
            disabled: (1..=25u8)
                .filter(|f| !families.contains(f))
                .map(code)
                .collect(),
            ..LabConfig::default()
        },
        lab_forecast: None,
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: true,
        decision_log_capacity: 500,
        rejection_dedup_secs: 60,
    }
}

fn env(t: DateTime<Utc>, event: WeatherMachineEvent) -> EventEnvelope {
    EventEnvelope::new(t, EventSource::Replay, event)
}

/// A METAR at `t`, known three minutes later.
fn report(t: DateTime<Utc>, whole: i32) -> EventEnvelope {
    let o = Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: t,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: Some(TempC::from_whole(10)),
        precision: TempPrecision::WholeDegree,
        raw_text: format!(
            "EHAM {} 24012KT 9999 FEW030 {whole:02}/10 Q1016 NOSIG",
            t.format("%d%H%MZ")
        ),
        content_hash: t.to_rfc3339(),
        provider: ProviderId::awc(),
        provider_receipt_at: None,
        fetched_at: t + Duration::minutes(3),
        parser_version: 1,
        quality: QualityFlags::default(),
    };
    env(
        t + Duration::minutes(3),
        WeatherMachineEvent::WeatherObservation(ObservationEvent {
            observation: o,
            class: DedupClass::New,
        }),
    )
}

fn reading(end: &str, mean: i32, max: i32, known: &str) -> EventEnvelope {
    env(
        utc(known),
        WeatherMachineEvent::NowcastUpdate(NowcastEvent {
            observation: TenMinuteObservation {
                station: eham(),
                provider: ProviderId::knmi(),
                interval_end: utc(end),
                mean: Some(TempC::from_tenths(mean)),
                max: Some(TempC::from_tenths(max)),
                radiation: None,
                received_at: utc(known),
            },
        }),
    )
}

fn no_book(m: &DailyTemperatureMarket, at: DateTime<Utc>) -> EventEnvelope {
    let o = m.outcome_for_value(18).unwrap();
    env(
        at,
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(&o.no_token, Some("0.40"), Some("0.42"), 200, at),
        }),
    )
}

fn market() -> DailyTemperatureMarket {
    wm_core::synthetic::synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-06-30T20:00:00Z"),
    )
}

/// A day up to the 11:25Z report (18 °C) in a fresh session.
fn session(families: &[u8], m: &DailyTemperatureMarket) -> SimulationSession {
    session_with(config(families), m)
}

fn session_with(cfg: EngineConfig, m: &DailyTemperatureMarket) -> SimulationSession {
    let mut s = SimulationSession::new(
        cfg,
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(NoEdgeModel),
    );
    let mut h =
        ProviderHealthSnapshot::new(ProviderId::awc(), Some(eham()), utc("2026-06-30T21:00:00Z"));
    h.state = ProviderHealthState::Healthy;
    s.push(env(
        utc("2026-06-30T21:00:00Z"),
        WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
            previous_state: None,
            snapshot: h,
        }),
    ));
    s.push(env(
        utc("2026-06-30T21:00:01Z"),
        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
    ));
    let mut t = utc("2026-06-30T22:25:00Z");
    while t <= utc("2026-07-01T11:25:00Z") {
        let whole = match t {
            t if t < utc("2026-07-01T06:00:00Z") => 12,
            t if t < utc("2026-07-01T09:00:00Z") => 15,
            t if t < utc("2026-07-01T11:00:00Z") => 17,
            _ => 18,
        };
        s.push(report(t, whole));
        t += Duration::minutes(30);
    }
    s
}

#[test]
fn lab_strategies_and_k_buy_the_same_token_on_their_own_books() {
    let m = market();
    let no18 = m.outcome_for_value(18).unwrap().no_token.clone();
    let mut s = session(&[2, 7], &m);
    // 11:35: the 11:20–11:30 reading (17.7 °C): no signal yet.
    s.push(no_book(&m, utc("2026-07-01T11:35:00Z")));
    s.push(reading(
        "2026-07-01T11:30:00Z",
        177,
        178,
        "2026-07-01T11:35:00Z",
    ));
    let out = s.run_until(utc("2026-07-01T11:35:00Z"));
    assert!(out.approved.is_empty(), "{:?}", out.approved);
    // 11:55:20: the 11:40–11:50 reading (18.9 °C, rising 0.6 °C in ten
    // minutes) before the 11:55 METAR. K takes the NO of 18 °C; L7 takes
    // it too (its slope reaches the edge + 0.6 °C); L2 rests a NO bid.
    s.push(no_book(&m, utc("2026-07-01T11:55:20Z")));
    s.push(reading(
        "2026-07-01T11:50:00Z",
        189,
        191,
        "2026-07-01T11:55:20Z",
    ));
    let out = s.run_until(utc("2026-07-01T11:55:20Z"));
    let mut orders: Vec<_> = out
        .approved
        .iter()
        .map(|a| {
            let i = a.intent();
            (
                i.strategy.as_str().to_owned(),
                i.token.clone(),
                i.side,
                i.outcome_side,
                i.limit_price,
                i.shares,
                i.tif,
            )
        })
        .collect();
    orders.sort_by(|a, b| a.0.cmp(&b.0));
    let p = |s: &str| Price::parse(s).unwrap();
    assert_eq!(
        orders,
        vec![
            (
                "K_knmi_nowcast".to_owned(),
                no18.clone(),
                Side::Buy,
                OutcomeSide::No,
                p("0.42"),
                Shares::from_whole(23),
                TimeInForce::Fak
            ),
            (
                "L2_informed_maker".to_owned(),
                no18.clone(),
                Side::Buy,
                OutcomeSide::No,
                p("0.41"),
                Shares::from_whole(48),
                TimeInForce::Gtd {
                    expires_at: utc("2026-07-01T11:58:00Z")
                }
            ),
            (
                "L7_knmi_slope".to_owned(),
                no18.clone(),
                Side::Buy,
                OutcomeSide::No,
                p("0.42"),
                Shares::from_whole(47),
                TimeInForce::Fak
            ),
        ],
        "{:#?}",
        out.decisions
            .iter()
            .map(|d| (d.summary.clone(), d.reasons.clone()))
            .collect::<Vec<_>>()
    );
    // The two takers fill; each position is its own strategy's.
    let out = s.run_until(utc("2026-07-01T11:55:21Z"));
    assert_eq!(out.trades.len(), 2, "{:?}", out.trades);
    let snap = s.engine().snapshot();
    let main: Vec<_> = snap
        .positions
        .iter()
        .filter(|p| p.shares.micros() > 0)
        .map(|p| (p.instrument.token.clone(), p.shares))
        .collect();
    assert_eq!(main, vec![(no18.clone(), Shares::from_whole(23))], "K only");
    assert_eq!(
        snap.position_strategy.get(&no18).map(StrategyId::as_str),
        Some("K_knmi_nowcast"),
        "the dashboard names the strategy that opened it"
    );
    let l7 = snap
        .lab_books
        .iter()
        .find(|b| b.strategy.as_str() == "L7_knmi_slope")
        .unwrap();
    assert_eq!(l7.positions.len(), 1);
    assert_eq!(l7.positions[0].shares, Shares::from_whole(47));
    assert!(l7.daily_new_exposure > Usd::from_whole(19));
    let l2 = snap
        .lab_books
        .iter()
        .find(|b| b.strategy.as_str() == "L2_informed_maker")
        .unwrap();
    assert!(l2.positions.is_empty(), "the maker's bid rests");
    assert_eq!(snap.lab_books.len(), 2);
    // The snapshot lists the main book's orders and decisions first, the
    // lab's after them: the lab never pushes K's out of the dashboard.
    let lab_id = |s: &str| wm_strategy::lab::is_lab_id(s);
    let orders: Vec<&str> = snap.orders.iter().map(|o| o.strategy.as_str()).collect();
    assert_eq!(orders.iter().filter(|s| lab_id(s)).count(), 2, "{orders:?}");
    assert!(
        orders.is_sorted_by_key(|s| lab_id(s)),
        "main first: {orders:?}"
    );
    let decisions: Vec<&str> = snap.decisions.iter().map(|d| d.strategy.as_str()).collect();
    assert!(decisions.contains(&"K_knmi_nowcast") && decisions.iter().any(|s| lab_id(s)));
    assert!(
        decisions.is_sorted_by_key(|s| lab_id(s)),
        "main first: {decisions:?}"
    );
    // Lab state on the dashboard: the readings kept, today's reports.
    let lab = snap.locations[0].lab.as_ref().unwrap();
    assert_eq!(lab.knmi_readings, 2);
    assert!(lab.reports >= 26);
    assert!(
        lab.latest_weather
            .as_deref()
            .is_some_and(|w| w.contains("240°/12 kt")),
        "{:?}",
        lab.latest_weather
    );

    // The 11:55 METAR reports 19 °C: the NO of 18 °C wins for both books.
    s.push(report(utc("2026-07-01T11:55:00Z"), 19));
    let out = s.run_until(utc("2026-07-02T00:00:02Z"));
    assert_eq!(out.settlements.len(), 1);
    let k_pnl = out.settlements[0].pnl;
    assert!(k_pnl > Usd::from_whole(13), "K's 23 shares: {k_pnl}");
    assert_eq!(
        s.engine().realized_pnl_total(),
        k_pnl,
        "the main book's alone"
    );
    let lab_pnl = s.engine().lab_realized_pnl_total();
    assert!(lab_pnl > Usd::from_whole(26), "L7's 47 shares: {lab_pnl}");
    assert_eq!(
        out.settlements[0].lab_pnl, lab_pnl,
        "the settlement names it"
    );
}

#[test]
fn a_restart_gives_each_fill_back_to_its_own_book() {
    let m = market();
    let o = m.outcome_for_value(18).unwrap();
    let fill = |id: &str, shares: i64| Fill {
        client_order_id: ClientOrderId::from_static_string(id.into()),
        token: o.no_token.clone(),
        side: Side::Buy,
        price: Price::parse("0.42").unwrap(),
        shares: Shares::from_whole(shares),
        fee: Usd::ZERO,
        liquidity: Liquidity::Taker,
        ts: utc("2026-07-01T11:55:21Z"),
    };
    let instrument = InstrumentRef {
        token: o.no_token.clone(),
        condition_id: o.condition_id.clone(),
        event_slug: m.event_slug.clone(),
        outcome_side: OutcomeSide::No,
        bucket: o.bucket,
    };
    let state = RestoreState {
        markets: vec![m.clone()],
        fills: vec![
            RestoredFill {
                fill: fill("k1", 23),
                instrument: instrument.clone(),
                strategy: StrategyId::from_static("K_knmi_nowcast"),
            },
            RestoredFill {
                fill: fill("l7", 47),
                instrument,
                strategy: StrategyId::from_static("L7_knmi_slope"),
            },
        ],
        new_exposure_today: Usd::from_whole(10),
        lab_new_exposure_today: BTreeMap::from([(
            StrategyId::from_static("L7_knmi_slope"),
            Usd::from_whole(20),
        )]),
    };
    let mut e = Engine::new(config(&[7]), Arc::new(NoEdgeModel));
    let summary = e.restore(&state, utc("2026-07-01T12:30:00Z"));
    assert_eq!(summary.open_positions, 2);
    assert_eq!(summary.new_exposure_today, Usd::from_whole(10), "main");
    assert_eq!(summary.lab_new_exposure_today, Usd::from_whole(20), "L7");
    let k = e.positions().get(&o.no_token).unwrap();
    assert_eq!(k.shares, Shares::from_whole(23));
    let l7 = e
        .lab_positions(&StrategyId::from_static("L7_knmi_slope"))
        .unwrap()
        .get(&o.no_token)
        .unwrap();
    assert_eq!(l7.shares, Shares::from_whole(47));
    let snap = e.snapshot();
    assert_eq!(
        snap.position_strategy
            .get(&o.no_token)
            .map(StrategyId::as_str),
        Some("K_knmi_nowcast"),
        "a restored position keeps the strategy of its order"
    );
    assert_eq!(snap.daily_new_exposure, Usd::from_whole(10));
    assert_eq!(snap.lab_books[0].daily_new_exposure, Usd::from_whole(20));
}

/// A lab strategy switched off since it bought keeps its position on a
/// book of its own after a restart: it never reaches the main book (A–K's
/// caps, the unwind engine) and settles apart.
#[test]
fn a_switched_off_lab_strategy_keeps_its_restored_position_off_the_main_book() {
    let m = market();
    let o = m.outcome_for_value(18).unwrap();
    let state = RestoreState {
        markets: vec![m.clone()],
        fills: vec![RestoredFill {
            fill: Fill {
                client_order_id: ClientOrderId::from_static_string("l19".into()),
                token: o.no_token.clone(),
                side: Side::Buy,
                price: Price::parse("0.40").unwrap(),
                shares: Shares::from_whole(50),
                fee: Usd::ZERO,
                liquidity: Liquidity::Taker,
                ts: utc("2026-07-01T11:00:00Z"),
            },
            instrument: InstrumentRef {
                token: o.no_token.clone(),
                condition_id: o.condition_id.clone(),
                event_slug: m.event_slug.clone(),
                outcome_side: OutcomeSide::No,
                bucket: o.bucket,
            },
            strategy: StrategyId::from_static("L19_skill_follow"),
        }],
        ..RestoreState::default()
    };
    // Only L7 runs now; with the whole lab off it is the same.
    for cfg in [config(&[7]), {
        let mut c = config(&[7]);
        c.lab.enabled = false;
        c
    }] {
        let mut e = Engine::new(cfg, Arc::new(NoEdgeModel));
        let summary = e.restore(&state, utc("2026-07-01T12:30:00Z"));
        assert_eq!(summary.open_positions, 1);
        assert!(summary.rejected.is_empty(), "{:?}", summary.rejected);
        assert!(
            e.positions().get(&o.no_token).is_none(),
            "not on the main book"
        );
        let l19 = StrategyId::from_static("L19_skill_follow");
        assert_eq!(
            e.lab_positions(&l19)
                .and_then(|b| b.get(&o.no_token))
                .map(|p| p.shares),
            Some(Shares::from_whole(50))
        );
        let snap = e.snapshot();
        assert!(snap.positions.is_empty());
        assert!(snap.lab_books.iter().any(|b| b.strategy == l19));
        // 18 °C was the high: the NO lost, on L19's book only.
        let main = e.settle(&m.event_slug, 18);
        assert!(main.is_zero(), "{main}");
        assert_eq!(e.lab_realized_pnl_total(), Usd::from_whole(-20));
    }
}

#[test]
fn the_lab_reads_yesterdays_error_and_counts_each_taker_trade_once() {
    use wm_core::event::{ForecastEvent, TakerTradesEvent};
    use wm_core::forecast::ForecastProduct;
    use wm_core::market::TakerTrade;
    let m = market();
    let mut cfg = config(&[17, 18]);
    cfg.lab_forecast = Some(ForecastProduct {
        provider: ProviderId::open_meteo(),
        model: "test".into(),
        lead_days: 1,
        ready_local_minute: 0,
    });
    let mut s = SimulationSession::new(
        cfg,
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(NoEdgeModel),
    );
    s.push(env(
        utc("2026-06-29T12:00:00Z"),
        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
    ));
    // Yesterday (30 June, local) reached 20 °C; today starts at 14 °C.
    let mut t = utc("2026-06-29T22:25:00Z");
    while t <= utc("2026-07-01T05:25:00Z") {
        let whole = if t >= utc("2026-06-30T12:00:00Z") && t < utc("2026-06-30T15:00:00Z") {
            20
        } else {
            14
        };
        s.push(report(t, whole));
        t += Duration::minutes(30);
    }
    // The day-1 series from 29 June 22:00 UTC: 18.5 °C at its warmest on
    // 30 June, 21.0 °C on 1 July.
    let start = utc("2026-06-29T22:00:00Z");
    let hourly: Vec<(DateTime<Utc>, TempC)> = (0..=48)
        .map(|h| {
            let at = start + Duration::hours(h);
            let v = match h {
                14 => 185,
                38 => 210,
                _ => 140,
            };
            (at, TempC::from_tenths(v))
        })
        .collect();
    s.push(env(
        utc("2026-07-01T05:00:00Z"),
        WeatherMachineEvent::ForecastUpdate(ForecastEvent {
            location: loc(),
            provider: ProviderId::open_meteo(),
            model: "test".into(),
            issued_at: utc("2026-07-01T05:00:00Z"),
            predicted_max: None,
            hourly,
            lead_days: Some(1),
        }),
    ));
    // The same trade delivered by two overlapping polls counts once.
    let y20 = m.outcome_for_value(20).unwrap().yes_token.clone();
    let trade = TakerTrade {
        token: y20,
        side: Side::Buy,
        price: 0.30,
        size: 25.0,
        at: utc("2026-07-01T05:40:00Z"),
        taker: Some("abc".into()),
        id: "0xhash-1".into(),
    };
    for at in ["2026-07-01T05:41:00Z", "2026-07-01T05:41:30Z"] {
        s.push(env(
            utc(at),
            WeatherMachineEvent::TakerTrades(TakerTradesEvent {
                trades: vec![trade.clone()],
            }),
        ));
    }
    s.run_until(utc("2026-07-01T06:00:00Z"));
    let snap = s.engine().snapshot();
    let lab = snap.locations[0].lab.as_ref().unwrap();
    assert_eq!(lab.forecast_day_max_tenths, Some(210));
    assert_eq!(
        lab.yesterday_error_tenths,
        Some(15),
        "20 °C observed − 18.5 °C"
    );
    assert_eq!(lab.taker_trades, 1);
    assert_eq!(lab.wallet_days, None, "no records loaded");
}

#[test]
fn before_todays_ready_time_the_forecast_rules_say_when_the_forecast_is_usable() {
    use wm_core::event::ForecastEvent;
    use wm_core::forecast::ForecastProduct;
    let m = market();
    let mut cfg = config(&[14]);
    cfg.lab_forecast = Some(ForecastProduct {
        provider: ProviderId::open_meteo(),
        model: "test".into(),
        lead_days: 1,
        ready_local_minute: 480, // 08:00 CEST = 06:00Z
    });
    let mut s = session_with(cfg, &m);
    // The day-1 series of 1 July (local), as fetched at `at`.
    let fetched = |at: &str| {
        let start = utc("2026-06-30T22:00:00Z");
        env(
            utc(at),
            WeatherMachineEvent::ForecastUpdate(ForecastEvent {
                location: loc(),
                provider: ProviderId::open_meteo(),
                model: "test".into(),
                issued_at: utc(at),
                predicted_max: None,
                hourly: (0..=24)
                    .map(|h: i32| {
                        let at = start + Duration::hours(i64::from(h));
                        (at, TempC::from_tenths(150 + 2 * h))
                    })
                    .collect(),
                lead_days: Some(1),
            }),
        )
    };
    // The last fetch of 30 June does not count for 1 July: until 06:00Z the
    // L14 line names the ready time, not a missing forecast.
    s.push(fetched("2026-06-30T21:30:00Z"));
    let l14 = |out: &wm_backtest::SessionOutput| {
        out.decisions
            .iter()
            .rev()
            .filter(|d| d.strategy.as_str() == "evaluation")
            .find_map(|d| {
                d.outputs["evaluations"]
                    .as_array()?
                    .iter()
                    .filter_map(|l| l.as_str())
                    .find(|l| l.starts_with("L14 "))
                    .map(str::to_owned)
            })
            .unwrap()
    };
    let line = l14(&s.run_until(utc("2026-07-01T05:58:00Z")));
    assert!(
        line.contains("the forecast is usable from 06:00 UTC"),
        "{line}"
    );
    // Fetched again at 06:02, it is in use: L14 waits for its window only.
    s.push(fetched("2026-07-01T06:02:00Z"));
    let line = l14(&s.run_until(utc("2026-07-01T06:28:00Z")));
    assert!(line.contains("outside 10:00–12:30"), "{line}");
    assert!(
        !line.contains("forecast is usable") && !line.contains("no day-1 forecast"),
        "{line}"
    );
}
