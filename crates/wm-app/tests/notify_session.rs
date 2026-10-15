#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The push messages of a session's output: a live order that ended having
//! bought shares is one message, the settled market is one per strategy
//! (the live twin's by default), and nothing is sent twice, by this run or
//! by one that restarts and settles the same market again.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use wm_app::notify::{LiveContext, Notifier, Outbox, Who};
use wm_backtest::SimulationSession;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OrderBookEvent,
    OrderUpdateEvent, ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::{DailyTemperatureMarket, Side};
use wm_core::synthetic::synthetic_book;
use wm_core::trading::{Fill, Liquidity, OrderStatus, OrderUpdate, RunMode};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{EngineConfig, EngineLocation, LiveBookConfig};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, IncrementDistribution, PeakConfig, PeakFeatures, ProbabilityModel,
    SplitUnwindConfig, TailSellerConfig, UnwindConfig,
};

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

fn g() -> TailSellerConfig {
    TailSellerConfig {
        min_distance: 3,
        max_model_ratio: 1e6,
        notional: Usd::from_whole(30),
        ..TailSellerConfig::default()
    }
}

fn config() -> EngineConfig {
    let cap = Usd::from_whole(40);
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(13),
        locations: vec![EngineLocation {
            location: LocationId::new("amsterdam").unwrap(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            confirmed_filter: None,
            routine_minutes: vec![25, 55],
            position: None,
            neighbours: Vec::new(),
            upstream: Vec::new(),
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
        tail_seller: TailSellerConfig {
            enabled: false,
            ..g()
        },
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
        live: Some(LiveBookConfig {
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
        }),
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

fn session() -> (SimulationSession, DailyTemperatureMarket) {
    let m = wm_core::synthetic::synthetic_temperature_market(
        &LocationId::new("amsterdam").unwrap(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-06-30T20:00:00Z"),
    );
    let mut s = SimulationSession::new(
        config(),
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(FixedModel),
    )
    .with_event_capture(true);
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

fn outbox(sent: Vec<String>) -> Outbox {
    let (notifier, _rx) = Notifier::new(Who::Live);
    let places: HashMap<String, String> = [("amsterdam".to_owned(), "Amsterdam".to_owned())].into();
    Outbox::new(notifier, places, sent)
}

#[test]
fn a_live_fill_and_its_settlement_are_one_message_each_and_never_twice() {
    let (mut s, m) = session();
    s.engine_mut().set_live_gate(Vec::new(), true);
    let no18 = m.outcome_for_value(18).unwrap().no_token.clone();
    s.push(env(
        utc("2026-07-01T08:32:00Z"),
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(
                &no18,
                Some("0.93"),
                Some("0.97"),
                500,
                utc("2026-07-01T08:32:00Z"),
            ),
        }),
    ));
    let out = s.run_until(utc("2026-07-01T08:32:30Z"));
    assert_eq!(out.live_orders.len(), 1, "{:?}", out.decisions);
    let id = out.live_orders[0].client_order_id().clone();
    let mut ob = outbox(Vec::new());
    let live = LiveContext {
        since_live: Some(Usd::ZERO),
        stop: Some(Usd::from_whole(20)),
        balance: Some(Usd::from_whole(50)),
        open: Usd::ZERO,
        max_open: Some(Usd::from_whole(40)),
    };
    assert!(
        ob.notes(s.engine(), &out, &live).is_empty(),
        "an order handed out is no message"
    );

    // The venue's updates: resting, four shares bought, the rest expired.
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
    s.push(update(OrderStatus::Live, 0, None, "2026-07-01T08:33:00Z"));
    s.push(update(
        OrderStatus::PartiallyFilled,
        4,
        Some(4),
        "2026-07-01T08:40:00Z",
    ));
    let out = s.run_until(utc("2026-07-01T08:41:00Z"));
    assert!(
        ob.notes(s.engine(), &out, &live).is_empty(),
        "a bid that still rests is no message yet"
    );
    s.push(update(
        OrderStatus::Expired,
        4,
        None,
        "2026-07-01T08:45:00Z",
    ));
    let out = s.run_until(utc("2026-07-01T08:46:00Z"));
    let notes = ob.notes(s.engine(), &out, &live);
    assert_eq!(notes.len(), 1, "{notes:?}");
    let (key, n) = &notes[0];
    assert_eq!(key, &format!("bought|{id}"));
    assert_eq!(n.title, "G_live bought 4 NO 18°C at 0.94");
    assert!(
        n.message
            .starts_with("$3.76 on Amsterdam 1 Jul; its bid of 10 shares at 0.94 expired"),
        "{}",
        n.message
    );
    assert!(ob.notes(s.engine(), &out, &live).is_empty(), "never twice");

    // The day settles at its observed high of 14 °C: the NO of 18 °C wins.
    let out = s.run_until(utc("2026-07-02T00:00:02Z"));
    assert_eq!(out.settlements.len(), 1);
    let held = &out.settlements[0].positions;
    assert!(
        held.iter()
            .any(|h| h.live && h.strategy.as_str() == "G_live" && h.position.won()),
        "{held:?}"
    );
    let live = LiveContext {
        since_live: s.engine().live_realized_total(),
        ..live
    };
    let notes = ob.notes(s.engine(), &out, &live);
    assert_eq!(notes.len(), 1, "the live twin's, the default");
    let (key, n) = &notes[0];
    assert_eq!(key, &format!("settled|{}|G_live", m.event_slug));
    assert_eq!(n.title, "G_live won +$0.24");
    assert!(
        n.message.starts_with("Amsterdam 1 Jul settled at 14°C"),
        "{}",
        n.message
    );
    assert!(
        n.message
            .contains("NO 18°C: 4 shares at 0.94 ($3.76), won: $4.00 back."),
        "{}",
        n.message
    );
    assert!(
        n.message.contains("Since going live +$0.24"),
        "{}",
        n.message
    );
    // a restart that settles the market again, with the key in the
    // database, sends nothing
    let mut after_restart = outbox(vec![key.clone()]);
    assert!(after_restart.notes(s.engine(), &out, &live).is_empty());
}
