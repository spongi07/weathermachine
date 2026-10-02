#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A resting strategy in the shared session loop (paper and replay): strategy
//! J quotes both sides, its orders expire before the report and give their
//! cost back to the daily new-exposure limit, it quotes again after the
//! report, and a trade through its bid fills it.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_backtest::SimulationSession;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, MarketTradeEvent, ObservationEvent,
    OrderBookEvent, ProviderHealthEvent, TimerEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side, TradePrint};
use wm_core::synthetic::synthetic_book;
use wm_core::trading::{RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{EngineConfig, EngineLocation};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, MorningMakerConfig, NoEdgeModel, PeakConfig, SplitUnwindConfig,
    UnwindConfig,
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

fn off<T>(enabled_false: T) -> T {
    enabled_false
}

fn config() -> EngineConfig {
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(11),
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
        tail_seller: off(wm_strategy::TailSellerConfig::absent()),
        next_degree: off(wm_strategy::NextDegreeConfig::absent()),
        middle_fade: off(wm_strategy::MiddleFadeConfig::absent()),
        morning_maker: MorningMakerConfig::default(),
        knmi_nowcast: off(wm_strategy::KnmiNowcastConfig::absent()),
        lab: wm_strategy::LabConfig::absent(),
        lab_forecast: None,
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: true,
        decision_log_capacity: 200,
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

fn books(m: &DailyTemperatureMarket, at: DateTime<Utc>) -> Vec<EventEnvelope> {
    let o = m.outcome_for_value(19).unwrap();
    [
        synthetic_book(&o.yes_token, Some("0.40"), Some("0.44"), 200, at),
        synthetic_book(&o.no_token, Some("0.56"), Some("0.60"), 200, at),
    ]
    .into_iter()
    .map(|book| {
        env(
            at,
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book }),
        )
    })
    .collect()
}

fn heartbeat(t: DateTime<Utc>) -> EventEnvelope {
    env(
        t,
        WeatherMachineEvent::Timer(TimerEvent {
            due_at: t,
            kind: TimerKind::Heartbeat,
        }),
    )
}

#[test]
fn the_morning_maker_quotes_expires_requotes_and_fills() {
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
        config(),
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
    // Half-hourly reports from local midnight, 12 °C rising to 14 °C.
    let mut t = utc("2026-06-30T22:25:00Z");
    while t <= utc("2026-07-01T07:25:00Z") {
        let whole = if t < utc("2026-07-01T05:00:00Z") {
            12
        } else {
            14
        };
        s.push(report(t, whole));
        t += Duration::minutes(30);
    }
    for e in books(&m, utc("2026-07-01T06:30:00Z")) {
        s.push(e);
    }
    let yes19 = m.outcome_for_value(19).unwrap().yes_token.clone();
    let no19 = m.outcome_for_value(19).unwrap().no_token.clone();

    // 08:30 local: J rests a YES bid and a NO bid inside the 0.40/0.44 book.
    let out = s.run_until(utc("2026-07-01T06:31:00Z"));
    let quotes: Vec<_> = out
        .approved
        .iter()
        .map(|a| {
            let i = a.intent();
            (i.outcome_side, i.limit_price, i.shares, i.tif)
        })
        .collect();
    let exp = TimeInForce::Gtd {
        expires_at: utc("2026-07-01T06:45:00Z"),
    };
    assert_eq!(
        quotes,
        vec![
            (
                OutcomeSide::Yes,
                Price::parse("0.41").unwrap(),
                Shares::from_whole(24),
                exp
            ),
            (
                OutcomeSide::No,
                Price::parse("0.57").unwrap(),
                Shares::from_whole(17),
                exp
            ),
        ]
    );
    let both = Usd::from_micros(9_840_000 + 9_690_000);
    assert_eq!(s.engine().snapshot().daily_new_exposure, both);

    // Nothing trades; at 06:45 both expire and give their cost back. At
    // 06:46 the report is nine minutes away: J does not quote again.
    s.push(heartbeat(utc("2026-07-01T06:46:00Z")));
    let out = s.run_until(utc("2026-07-01T06:46:00Z"));
    assert!(out.approved.is_empty(), "{:?}", out.approved);
    let snap = s.engine().snapshot();
    assert_eq!(snap.daily_new_exposure, Usd::ZERO);
    assert!(snap.orders.iter().all(|o| o.status.is_terminal()));

    // After the 06:55 report, fresh books: J quotes again until 07:15.
    for e in books(&m, utc("2026-07-01T06:58:30Z")) {
        s.push(e);
    }
    let out = s.run_until(utc("2026-07-01T06:59:00Z"));
    assert_eq!(out.approved.len(), 2, "{:?}", out.decisions);
    assert!(out.approved.iter().all(|a| a.intent().tif
        == TimeInForce::Gtd {
            expires_at: utc("2026-07-01T07:15:00Z")
        }));

    // A taker sells YES through J's 0.41 bid: J's 24 shares fill as maker.
    s.push(env(
        utc("2026-07-01T07:02:00Z"),
        WeatherMachineEvent::MarketTrade(MarketTradeEvent {
            trade: TradePrint {
                token: yes19.clone(),
                price: Price::parse("0.40").unwrap(),
                size: Shares::from_whole(50),
                aggressor: Some(Side::Sell),
                ts: utc("2026-07-01T07:02:00Z"),
            },
        }),
    ));
    let out = s.run_until(utc("2026-07-01T07:02:00Z"));
    assert_eq!(out.trades.len(), 1, "{:?}", out.trades);
    assert_eq!(out.trades[0].price, "0.41");
    let snap = s.engine().snapshot();
    let held = snap
        .positions
        .iter()
        .find(|p| p.instrument.token == yes19)
        .unwrap();
    assert_eq!(held.shares, Shares::from_whole(24));
    assert_eq!(held.instrument.outcome_side, OutcomeSide::Yes);
    // The filled bid stays counted; the resting NO bid too.
    assert_eq!(snap.daily_new_exposure, both);

    // 07:15: the NO bid expires unfilled and gives its $9.69 back. J holds
    // its YES and does not bid for more.
    s.push(heartbeat(utc("2026-07-01T07:16:00Z")));
    s.run_until(utc("2026-07-01T07:16:00Z"));
    let snap = s.engine().snapshot();
    assert_eq!(snap.daily_new_exposure, Usd::from_micros(9_840_000));
    for e in books(&m, utc("2026-07-01T07:28:30Z")) {
        s.push(e);
    }
    let out = s.run_until(utc("2026-07-01T07:29:00Z"));
    let sides: Vec<_> = out
        .approved
        .iter()
        .map(|a| a.intent().token.clone())
        .collect();
    assert_eq!(sides, vec![no19], "only the side not yet held is quoted");
}
