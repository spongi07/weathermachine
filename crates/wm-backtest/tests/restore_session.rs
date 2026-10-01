#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A restart's paper book in the shared session loop: a restored position
//! of a finished day settles at the next step, at the replayed high, and its
//! P&L counts toward today's loss limit, as it did before the restart.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_backtest::SimulationSession;
use wm_core::event::{EventEnvelope, EventSource, ObservationEvent, WeatherMachineEvent};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, RunId, StationId, StrategyId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::portfolio::InstrumentRef;
use wm_core::trading::{Fill, Liquidity, RunMode};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{EngineConfig, EngineLocation, RestoreState, RestoredFill};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, NoEdgeModel, PeakConfig, SplitUnwindConfig, UnwindConfig,
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
        run_id: RunId::deterministic(7),
        locations: vec![EngineLocation {
            location: loc(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            confirmed_filter: None,
        }],
        risk: RiskConfig::default(),
        buy_yes: BuyYesConfig::default(),
        buy_no: BuyNoConfig::default(),
        split_unwind: SplitUnwindConfig::default(),
        certain: wm_strategy::CertainConfig::default(),
        book_confirmed: wm_strategy::BookConfirmedConfig::default(),
        peak_slot: wm_strategy::PeakSlotConfig::default(),
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: false,
        decision_log_capacity: 100,
        rejection_dedup_secs: 60,
    }
}

/// A METAR at `t`, known three minutes later (as the warm start replays it).
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
    EventEnvelope::new(
        t + Duration::minutes(3),
        EventSource::Replay,
        WeatherMachineEvent::WeatherObservation(ObservationEvent {
            observation: o,
            class: DedupClass::New,
        }),
    )
}

/// 30 June (local): 12 °C at night, 18 °C at 13:55 local, cooling after; then
/// 1 July's night and morning.
fn reports_until(end: DateTime<Utc>) -> Vec<EventEnvelope> {
    let mut t = utc("2026-06-29T22:25:00Z");
    let mut v = Vec::new();
    while t + Duration::minutes(3) <= end {
        let peak = utc("2026-06-30T11:55:00Z");
        let hours = (t - peak).num_minutes() as f64 / 60.0;
        let whole = (18.0 - 0.6 * hours.abs()).round().max(12.0) as i32;
        v.push(report(t, whole));
        t += Duration::minutes(30);
    }
    v
}

fn yes_fill(m: &DailyTemperatureMarket, value: i32, price: &str, shares: i64) -> RestoredFill {
    let o = m.outcome_for_value(value).unwrap();
    RestoredFill {
        fill: Fill {
            client_order_id: ClientOrderId::new("wm-old-7").unwrap(),
            token: o.yes_token.clone(),
            side: Side::Buy,
            price: Price::parse(price).unwrap(),
            shares: Shares::from_whole(shares),
            fee: Usd::ZERO,
            liquidity: Liquidity::Taker,
            ts: utc("2026-06-30T13:34:00Z"),
        },
        instrument: InstrumentRef {
            token: o.yes_token.clone(),
            condition_id: o.condition_id.clone(),
            event_slug: m.event_slug.clone(),
            outcome_side: OutcomeSide::Yes,
            bucket: o.bucket,
        },
        strategy: StrategyId::new("F_peak_slot").unwrap(),
    }
}

#[test]
fn a_restored_position_of_a_finished_day_settles_at_the_next_step() {
    let yesterday = wm_core::synthetic::synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-06-30T04:00:00Z"),
    );
    // Restart at 10:00 local on 1 July: 30 June settled at 00:00 UTC today.
    let now = utc("2026-07-01T08:00:00Z");
    let mut session = SimulationSession::new(
        config(),
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(NoEdgeModel),
    );
    for e in reports_until(now) {
        session.push(e);
    }
    let warm = session.run_until(now);
    assert!(warm.settlements.is_empty(), "no market before the restore");

    let state = RestoreState {
        markets: vec![yesterday.clone()],
        fills: vec![yes_fill(&yesterday, 18, "0.90", 10)],
        new_exposure_today: Usd::ZERO,
    };
    let summary = session.restore(&state, now);
    assert_eq!((summary.open_positions, summary.fills), (1, 1));
    assert_eq!(summary.open_cost, Usd::from_whole(9));

    // The next step settles it at the replayed high, without any traffic.
    let out = session.run_until(now + Duration::seconds(1));
    assert_eq!(out.settlements.len(), 1, "{:?}", out.settlements);
    let s = &out.settlements[0];
    assert_eq!(s.event_slug, yesterday.event_slug);
    assert_eq!(s.final_value, 18);
    assert_eq!(s.pnl, Usd::from_whole(1), "10 × (1 − 0.90)");
    let snap = session.engine().snapshot();
    assert_eq!(snap.daily_realized_pnl, Usd::from_whole(1), "today's limit");
    assert_eq!(
        snap.positions
            .iter()
            .filter(|p| !p.shares.is_zero())
            .count(),
        0
    );

    // Settled once: later steps leave it alone.
    let later = session.run_until(now + Duration::hours(1));
    assert!(later.settlements.is_empty());
}

#[test]
fn a_restored_position_of_today_stays_open_until_its_day_is_over() {
    let today = wm_core::synthetic::synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-07-01T04:00:00Z"),
    );
    let now = utc("2026-07-01T08:00:00Z");
    let mut session = SimulationSession::new(
        config(),
        SimConfig::default(),
        Duration::hours(2),
        Arc::new(NoEdgeModel),
    );
    for e in reports_until(now) {
        session.push(e);
    }
    session.run_until(now);
    let state = RestoreState {
        markets: vec![today.clone()],
        fills: vec![yes_fill(&today, 15, "0.40", 10)],
        new_exposure_today: Usd::from_whole(4),
    };
    session.restore(&state, now);
    let out = session.run_until(now + Duration::hours(6));
    assert!(out.settlements.is_empty(), "1 July is not over");
    let snap = session.engine().snapshot();
    assert_eq!(snap.daily_new_exposure, Usd::from_whole(4));
    assert_eq!(snap.exposure.global_worst_case, Usd::from_whole(4));
}
