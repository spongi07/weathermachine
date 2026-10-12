#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Strategy G's live twin in the shared session loop: paper G and G_live
//! evaluate the same rule on books of their own; G_live's proposals wait in
//! its evaluation while the live venue is not ready or its loss stop holds,
//! the compliance gate refuses a location the API may not trade from; an
//! approved live order goes out of the session (never to the simulated
//! exchange), the venue's updates build the live book, and a restored
//! G_live fill lands in the live book.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_backtest::SimulationSession;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OrderBookEvent,
    OrderUpdateEvent, ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, RunId, StationId, StrategyId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::portfolio::InstrumentRef;
use wm_core::synthetic::synthetic_book;
use wm_core::trading::{Fill, Liquidity, OrderStatus, OrderUpdate, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{EngineConfig, EngineLocation, LiveBookConfig, RestoreState, RestoredFill};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, IncrementDistribution, PeakConfig, PeakFeatures, ProbabilityModel,
    SplitUnwindConfig, TailSellerConfig, UnwindConfig,
};

const LIVE: &str = wm_strategy::tail_seller::LIVE_ID;
const PAPER_G: &str = wm_strategy::tail_seller::ID;

struct FixedModel;

impl ProbabilityModel for FixedModel {
    fn id(&self) -> &str {
        "fixed-test-model"
    }
    fn distribution(&self, _f: &PeakFeatures) -> Option<IncrementDistribution> {
        Some(IncrementDistribution {
            probs: vec![0.6, 0.3, 0.08, 0.02],
            support: 500,
            source: "fixed".into(),
        })
    }
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn loc() -> LocationId {
    LocationId::new("amsterdam").unwrap()
}

fn g() -> TailSellerConfig {
    TailSellerConfig {
        min_distance: 3,
        max_model_ratio: 1e6,
        notional: Usd::from_whole(30),
        ..TailSellerConfig::default()
    }
}

fn live_book() -> LiveBookConfig {
    let cap = Usd::from_whole(40);
    LiveBookConfig {
        tail_seller: TailSellerConfig {
            notional: Usd::from_whole(10),
            whole_cent_sizes: true,
            ..g()
        },
        risk: RiskConfig {
            position_size_usd: Usd::from_whole(10),
            global_max_exposure_usd: cap,
            max_market_exposure_usd: Some(cap),
            max_location_exposure_usd: Some(cap),
            max_strategy_exposure_usd: Some(cap),
            max_daily_new_exposure_usd: Some(cap),
            max_daily_loss_usd: Some(Usd::from_whole(20)),
            ..RiskConfig::default()
        },
        max_total_loss: Usd::from_whole(20),
    }
}

fn config(live: Option<LiveBookConfig>) -> EngineConfig {
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(12),
        locations: vec![EngineLocation {
            location: loc(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            confirmed_filter: None,
            routine_minutes: vec![25, 55],
            position: None,
            neighbours: Vec::new(),
        }],
        risk: RiskConfig {
            strategy_caps: [(
                PAPER_G.to_owned(),
                wm_risk::StrategyCaps {
                    position_size_usd: Usd::from_whole(30),
                    max_market_exposure_usd: None,
                    max_strategy_exposure_usd: Some(Usd::from_whole(120)),
                    max_spread: None,
                },
            )]
            .into_iter()
            .collect(),
            ..RiskConfig::default()
        },
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
        tail_seller: g(),
        next_degree: wm_strategy::NextDegreeConfig::absent(),
        middle_fade: wm_strategy::MiddleFadeConfig::absent(),
        morning_maker: wm_strategy::MorningMakerConfig::absent(),
        knmi_nowcast: wm_strategy::KnmiNowcastConfig::absent(),
        lab: wm_strategy::LabConfig::absent(),
        lab_forecast: None,
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: true,
        decision_log_capacity: 500,
        rejection_dedup_secs: 0,
        live,
    }
}

fn env(t: DateTime<Utc>, event: WeatherMachineEvent) -> EventEnvelope {
    EventEnvelope::new(t, EventSource::Replay, event)
}

fn report(t: DateTime<Utc>, whole: i32) -> EventEnvelope {
    let o = Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: t,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: None,
        precision: TempPrecision::WholeDegree,
        raw_text: format!(
            "EHAM {} 24012KT 9999 {whole:02}/10 Q1016",
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

/// The NO book of 18 °C: bid 0.93, ask 0.97 (G bids 0.94: YES offered at 0.06).
fn no18_book(m: &DailyTemperatureMarket, at: DateTime<Utc>) -> EventEnvelope {
    let o = m.outcome_for_value(18).unwrap();
    env(
        at,
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(&o.no_token, Some("0.93"), Some("0.97"), 500, at),
        }),
    )
}

fn session(live: Option<LiveBookConfig>) -> (SimulationSession, DailyTemperatureMarket) {
    let m = wm_core::synthetic::synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-06-30T20:00:00Z"),
    );
    let mut s = SimulationSession::new(
        config(live),
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(FixedModel),
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
    while t <= utc("2026-07-01T08:25:00Z") {
        s.push(report(t, 14));
        t += Duration::minutes(30);
    }
    (s, m)
}

fn live_lines(s: &SimulationSession, bucket: &str) -> Vec<String> {
    s.engine()
        .snapshot()
        .locations
        .iter()
        .flat_map(|l| l.evaluations.iter())
        .filter(|e| e.strategy.as_str() == LIVE && e.bucket_label.starts_with(bucket))
        .flat_map(|e| e.blockers.clone())
        .collect()
}

#[test]
fn the_live_twin_waits_while_the_venue_is_not_ready_and_never_reaches_the_simulator() {
    let (mut s, m) = session(Some(live_book()));
    s.push(no18_book(&m, utc("2026-07-01T08:30:00Z")));
    let out = s.run_until(utc("2026-07-01T08:31:00Z"));
    let approved: Vec<(String, Shares)> = out
        .approved
        .iter()
        .map(|a| (a.intent().strategy.to_string(), a.intent().shares))
        .collect();
    // paper G rests its bid at 0.94 for $30; the twin waits for the venue
    assert_eq!(approved, vec![(PAPER_G.to_owned(), Shares::from_whole(31))]);
    assert!(out.live_orders.is_empty());
    let blockers = live_lines(&s, "18");
    assert!(
        blockers
            .iter()
            .any(|b| b.contains("live venue") && b.contains("not reported")),
        "{blockers:?}"
    );
    assert!(
        !out.decisions.iter().any(|d| d.strategy.as_str() == LIVE),
        "a proposal that waits is no refusal in the log"
    );

    // A venue that is ready but places this server where the API may not
    // trade: the risk check's compliance gate refuses.
    s.engine_mut().set_live_gate(Vec::new(), false);
    s.push(no18_book(&m, utc("2026-07-01T08:31:30Z")));
    let out = s.run_until(utc("2026-07-01T08:31:45Z"));
    let refused: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.strategy.as_str() == LIVE)
        .collect();
    assert!(!refused.is_empty() && refused.iter().all(|d| !d.approved));
    assert!(
        refused[0].reasons.iter().any(|r| r.contains("Compliance")),
        "{:?}",
        refused[0].reasons
    );
    assert!(out.live_orders.is_empty());

    // Ready and allowed: G_live's bid is approved and handed out as a live
    // order, sized to whole cents at $10, never to the simulated exchange.
    s.engine_mut().set_live_gate(Vec::new(), true);
    s.push(no18_book(&m, utc("2026-07-01T08:32:00Z")));
    let out = s.run_until(utc("2026-07-01T08:32:30Z"));
    assert_eq!(out.live_orders.len(), 1, "{:?}", out.decisions);
    let a = &out.live_orders[0];
    let i = a.intent();
    assert_eq!(i.strategy.as_str(), LIVE);
    assert_eq!(i.outcome_side, OutcomeSide::No);
    assert_eq!(i.limit_price, Price::parse("0.94").unwrap());
    assert_eq!(i.shares, Shares::from_whole(10), "$10 at 0.94, whole cents");
    assert_eq!(
        i.tif,
        TimeInForce::Gtd {
            expires_at: utc("2026-07-01T08:45:00Z")
        },
        "ten minutes before the 08:55 report"
    );
    let id = a.client_order_id().clone();
    let rec = s.engine().orders().get(&id).unwrap();
    assert_eq!(
        rec.status,
        OrderStatus::PendingSubmit,
        "the venue decides from here"
    );
    // the simulator never saw it: time passes, no fill and no update of it
    s.push(no18_book(&m, utc("2026-07-01T08:44:00Z")));
    let out = s.run_until(utc("2026-07-01T08:50:00Z"));
    assert!(out.trades.iter().all(|t| t.strategy.as_str() != LIVE));
    assert_eq!(
        s.engine().orders().get(&id).unwrap().status,
        OrderStatus::PendingSubmit
    );

    // The venue's updates, as the live executor sends them: resting, four
    // shares bought as a maker, then the rest expired.
    let no18 = m.outcome_for_value(18).unwrap().no_token.clone();
    let update = |status, filled: i64, fill: Option<i64>, at: &str| {
        env(
            utc(at),
            WeatherMachineEvent::OrderUpdate(OrderUpdateEvent {
                update: OrderUpdate {
                    client_order_id: id.clone(),
                    venue_order_id: Some("0xabc".into()),
                    status,
                    filled: Shares::from_whole(filled),
                    avg_price: (filled > 0).then(|| Price::parse("0.94").unwrap()),
                    fee_paid: Usd::ZERO,
                    reason: None,
                    ts: utc(at),
                },
                fill: fill.map(|n| Fill {
                    client_order_id: id.clone(),
                    token: no18.clone(),
                    side: Side::Buy,
                    price: Price::parse("0.94").unwrap(),
                    shares: Shares::from_whole(n),
                    fee: Usd::ZERO,
                    liquidity: Liquidity::Maker,
                    ts: utc(at),
                }),
            }),
        )
    };
    s.push(update(OrderStatus::Live, 0, None, "2026-07-01T08:50:01Z"));
    s.push(update(
        OrderStatus::PartiallyFilled,
        4,
        Some(4),
        "2026-07-01T08:50:02Z",
    ));
    s.push(update(
        OrderStatus::Expired,
        4,
        None,
        "2026-07-01T08:50:03Z",
    ));
    let out = s.run_until(utc("2026-07-01T08:50:05Z"));
    assert_eq!(out.trades.len(), 1);
    assert_eq!(out.trades[0].strategy.as_str(), LIVE);
    let e = s.engine();
    assert_eq!(
        e.live_positions().unwrap().get(&no18).unwrap().shares,
        Shares::from_whole(4)
    );
    assert!(e.positions().get(&no18).is_none(), "not the main book's");
    let snap = e.snapshot();
    let live = snap.live.as_ref().unwrap();
    assert_eq!(
        live.daily_new_exposure,
        Usd::from_micros(3_760_000),
        "the unfilled six shares gave their cost back"
    );
    assert!(
        !snap.lab_books.iter().any(|b| b.strategy.as_str() == LIVE),
        "the live book is not a lab book"
    );

    // The loss stop: $20 lost since going live stops the twin (19 °C would
    // otherwise get a bid).
    s.engine_mut()
        .set_live_realized_before(Usd::from_whole(-20));
    s.push(report(utc("2026-07-01T08:55:00Z"), 14));
    let o19 = m.outcome_for_value(19).unwrap();
    s.push(env(
        utc("2026-07-01T09:00:00Z"),
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(
                &o19.no_token,
                Some("0.93"),
                Some("0.97"),
                500,
                utc("2026-07-01T09:00:00Z"),
            ),
        }),
    ));
    let out = s.run_until(utc("2026-07-01T09:00:30Z"));
    assert!(out.live_orders.is_empty());
    let blockers = live_lines(&s, "19");
    assert!(
        blockers
            .iter()
            .any(|b| b.contains("stopped: the live book lost $20.00 since it went live")),
        "{blockers:?}"
    );
    assert!(
        s.engine()
            .snapshot()
            .live
            .unwrap()
            .blocked
            .unwrap()
            .starts_with("stopped")
    );

    // Settled at 14 °C: the NO of 18 °C wins, 4 × (1 − 0.94) = $0.24.
    s.engine_mut().settle(&m.event_slug, 14);
    assert_eq!(s.engine().live_realized_pnl(), Usd::from_micros(240_000));
    assert_eq!(
        s.engine().live_realized_total(),
        Some(Usd::from_micros(-19_760_000))
    );
}

#[test]
fn the_live_twin_sizes_to_whole_cents_and_trades_its_own_book() {
    // a 0.977 bid: ten shares cost $9.77, whole cents (eleven would not)
    let (mut s, m) = session(Some(LiveBookConfig {
        tail_seller: TailSellerConfig {
            notional: Usd::from_micros(10_900_000),
            whole_cent_sizes: true,
            ..g()
        },
        ..live_book()
    }));
    let o = m.outcome_for_value(18).unwrap();
    let at = utc("2026-07-01T08:30:00Z");
    let mut book = synthetic_book(&o.no_token, Some("0.976"), Some("0.979"), 500, at);
    book.tick_size = Price::parse("0.001").unwrap();
    s.push(env(
        at,
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book }),
    ));
    s.engine_mut().set_live_gate(Vec::new(), true);
    let out = s.run_until(utc("2026-07-01T08:31:00Z"));
    let live: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.strategy.as_str() == LIVE)
        .collect();
    assert!(!live.is_empty());
    assert!(
        live[0].summary.contains("@ 0.977 ×10"),
        "whole cents: {}",
        live[0].summary
    );
}

#[test]
fn a_restored_live_fill_lands_in_the_live_book() {
    for configured in [true, false] {
        let (mut s, m) = session(configured.then(live_book));
        let o = m.outcome_for_value(18).unwrap();
        let fill = Fill {
            client_order_id: ClientOrderId::new("wm-old-1-live").unwrap(),
            token: o.no_token.clone(),
            side: Side::Buy,
            price: Price::parse("0.94").unwrap(),
            shares: Shares::from_whole(10),
            fee: Usd::ZERO,
            liquidity: Liquidity::Maker,
            ts: utc("2026-07-01T08:00:00Z"),
        };
        let state = RestoreState {
            markets: vec![m.clone()],
            fills: vec![RestoredFill {
                fill,
                instrument: InstrumentRef {
                    token: o.no_token.clone(),
                    condition_id: o.condition_id.clone(),
                    event_slug: m.event_slug.clone(),
                    outcome_side: OutcomeSide::No,
                    bucket: o.bucket,
                },
                strategy: StrategyId::from_static(LIVE),
            }],
            ..RestoreState::default()
        };
        s.restore(&state, utc("2026-07-01T08:05:00Z"));
        let e = s.engine();
        assert!(
            e.positions().get(&o.no_token).is_none(),
            "not the main book's"
        );
        let held = e.live_positions().unwrap().get(&o.no_token).unwrap();
        assert_eq!(held.shares, Shares::from_whole(10));
        assert!(e.is_live_strategy(&StrategyId::from_static(LIVE)));
    }
}
