#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Strategy K in the shared session loop (paper and replay): a KNMI
//! ten-minute reading reaches the engine as a `NowcastUpdate`, is kept as
//! the station's latest, and K buys the NO of the high's bucket before the
//! METAR that will kill it is published. A reading older than the last
//! METAR, or one that arrives late, changes nothing.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_backtest::SimulationSession;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, NowcastEvent, ObservationEvent,
    OrderBookEvent, ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::resolution::ObservationFilter;
use wm_core::synthetic::synthetic_book;
use wm_core::trading::{RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
    TenMinuteObservation,
};
use wm_engine::{EngineConfig, EngineLocation};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, KnmiNowcastConfig, NoEdgeModel, PeakConfig, SplitUnwindConfig,
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

fn config() -> EngineConfig {
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(12),
        locations: vec![EngineLocation {
            location: loc(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            // One resolution view, as for EHAM's Wunderground markets: the
            // synthetic market also lists the WRH hourly rows (:51–:59),
            // under which the 11:25 report would not count.
            confirmed_filter: Some(ObservationFilter::AllRows),
            routine_minutes: vec![25, 55],
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
        // The default risk limits allow $10 a position.
        knmi_nowcast: KnmiNowcastConfig {
            notional: Usd::from_whole(10),
            ..KnmiNowcastConfig::default()
        },
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

/// KNMI's reading of the ten minutes up to `end`, known at `known`.
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
                received_at: utc(known),
            },
        }),
    )
}

/// The NO of 18 °C at 0.40/0.42, quoted at `at`.
fn no_book(m: &DailyTemperatureMarket, at: DateTime<Utc>) -> EventEnvelope {
    let o = m.outcome_for_value(18).unwrap();
    env(
        at,
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(&o.no_token, Some("0.40"), Some("0.42"), 200, at),
        }),
    )
}

#[test]
fn k_buys_the_no_of_the_high_before_the_metar_that_kills_it() {
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
    // Half-hourly reports from local midnight; the 11:25Z one reaches 18 °C.
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
    let no18 = m.outcome_for_value(18).unwrap().no_token.clone();

    // 11:30: the 11:10–11:20 reading (19.0 °C) arrives after the 11:25 METAR
    // that already counted those minutes: K does nothing.
    s.push(no_book(&m, utc("2026-07-01T11:30:00Z")));
    s.push(reading(
        "2026-07-01T11:20:00Z",
        190,
        192,
        "2026-07-01T11:30:00Z",
    ));
    let out = s.run_until(utc("2026-07-01T11:30:00Z"));
    assert!(out.approved.is_empty(), "{:?}", out.approved);
    assert_eq!(
        s.engine().nowcast(&eham()).map(|n| n.interval_end),
        Some(utc("2026-07-01T11:20:00Z"))
    );

    // 11:55:20: the 11:40–11:50 reading, mean 18.9 °C, reaches the engine
    // as the 11:55 METAR is taken (published at 11:58). The high's bucket
    // dies if that METAR says 19: K buys its NO at the ask.
    s.push(no_book(&m, utc("2026-07-01T11:55:20Z")));
    s.push(reading(
        "2026-07-01T11:50:00Z",
        189,
        191,
        "2026-07-01T11:55:20Z",
    ));
    let out = s.run_until(utc("2026-07-01T11:55:20Z"));
    let orders: Vec<_> = out
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
    assert_eq!(
        orders,
        vec![(
            "K_knmi_nowcast".to_owned(),
            no18.clone(),
            Side::Buy,
            OutcomeSide::No,
            Price::parse("0.42").unwrap(),
            Shares::from_whole(23),
            TimeInForce::Fak
        )],
        "{:?}",
        out.decisions
    );
    // The fill-and-kill order takes the ask after the simulated latency.
    let out = s.run_until(utc("2026-07-01T11:55:21Z"));
    assert_eq!(out.trades.len(), 1, "{:?}", out.trades);
    assert_eq!(out.trades[0].price, "0.42");
    let snap = s.engine().snapshot();
    assert!(
        snap.positions
            .iter()
            .any(|p| p.instrument.token == no18 && p.shares == Shares::from_whole(23))
    );
    assert_eq!(
        snap.locations[0].nowcast.as_ref().map(|n| n.interval_end),
        Some(utc("2026-07-01T11:50:00Z"))
    );

    // A late copy of the 11:40 reading does not replace the newer one, and
    // K, holding the NO, does not buy it again.
    s.push(no_book(&m, utc("2026-07-01T11:56:00Z")));
    s.push(reading(
        "2026-07-01T11:40:00Z",
        195,
        197,
        "2026-07-01T11:56:00Z",
    ));
    let out = s.run_until(utc("2026-07-01T11:56:00Z"));
    assert!(out.approved.is_empty(), "{:?}", out.approved);
    assert_eq!(
        s.engine().nowcast(&eham()).map(|n| n.interval_end),
        Some(utc("2026-07-01T11:50:00Z"))
    );
}
