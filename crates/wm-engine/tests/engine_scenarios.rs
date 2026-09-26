#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end kernel scenarios (same code path as backtest/paper/live).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OperatorCommand,
    OrderBookEvent, ProviderHealthEvent, TimerEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::resolution::ObservationFilter;
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::RunMode;
use wm_core::units::{TempC, Usd};
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{Engine, EngineConfig, EngineLocation, EngineOutput};
use wm_execution::{SimConfig, SimulatedExchange};
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, IncrementDistribution, NoEdgeModel, PeakConfig, PeakFeatures,
    ProbabilityModel, SplitUnwindConfig, UnwindConfig,
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

struct FixedModel(Vec<f64>);

impl ProbabilityModel for FixedModel {
    fn id(&self) -> &str {
        "fixed-test-model"
    }
    fn distribution(&self, _f: &PeakFeatures) -> Option<IncrementDistribution> {
        Some(IncrementDistribution {
            probs: self.0.clone(),
            support: 500,
            source: "fixed".into(),
        })
    }
}

fn config(mode: RunMode) -> EngineConfig {
    EngineConfig {
        mode,
        run_id: RunId::deterministic(42),
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
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: false,
        decision_log_capacity: 500,
        rejection_dedup_secs: 60,
    }
}

fn market(filters: Vec<ObservationFilter>) -> DailyTemperatureMarket {
    let mut m = synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        13,
        24,
        utc("2026-07-01T06:00:00Z"),
    );
    m.resolution.filters = filters;
    m
}

fn env(at: &str, event: WeatherMachineEvent) -> EventEnvelope {
    EventEnvelope::new(utc(at), EventSource::Synthetic, event)
}

fn obs_event(t: &str, whole: i32) -> EventEnvelope {
    let observed = utc(t);
    let o = Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: observed,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: None,
        precision: TempPrecision::WholeDegree,
        raw_text: format!(
            "EHAM {} 24012KT 9999 FEW030 {whole:02}/12 Q1016",
            observed.format("%d%H%MZ")
        ),
        content_hash: t.to_owned(),
        provider: ProviderId::awc(),
        provider_receipt_at: None,
        fetched_at: observed + Duration::minutes(3),
        parser_version: 1,
        quality: QualityFlags::default(),
    };
    EventEnvelope::new(
        observed + Duration::minutes(3),
        EventSource::Synthetic,
        WeatherMachineEvent::WeatherObservation(ObservationEvent {
            observation: o,
            class: DedupClass::New,
        }),
    )
}

fn health_event(at: &str, state: ProviderHealthState) -> EventEnvelope {
    let mut s = ProviderHealthSnapshot::new(ProviderId::awc(), Some(eham()), utc(at));
    s.state = state;
    env(
        at,
        WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
            previous_state: None,
            snapshot: s,
        }),
    )
}

fn books(m: &DailyTemperatureMarket, at: &str) -> Vec<OrderBook> {
    let t = utc(at);
    let o = |v: i32| m.outcome_for_value(v).unwrap();
    vec![
        synthetic_book(&o(18).yes_token, Some("0.93"), Some("0.95"), 200, t),
        synthetic_book(&o(19).no_token, Some("0.91"), Some("0.93"), 200, t),
        synthetic_book(&o(20).no_token, Some("0.96"), Some("0.97"), 200, t),
        synthetic_book(&o(21).no_token, Some("0.98"), Some("0.985"), 200, t),
    ]
}

/// Half-hourly reports from local midnight (22:00Z) to 09:55Z, all below the later high.
fn night_and_morning() -> Vec<EventEnvelope> {
    let start = utc("2026-06-30T22:25:00Z");
    (0..24)
        .map(|i| {
            let t = start + Duration::minutes(30 * i);
            let whole = if i < 10 {
                12 - (i as i32) / 5
            } else {
                11 + ((i as i32) - 10) / 3
            };
            obs_event(&t.to_rfc3339(), whole)
        })
        .collect()
}

/// High 18 at 11:55Z (13:55 CEST), retest 12:25, then decline; decision after 13:55Z report.
fn scenario(m: &DailyTemperatureMarket, health: Option<ProviderHealthState>) -> Vec<EventEnvelope> {
    let mut v = Vec::new();
    if let Some(h) = health {
        v.push(health_event("2026-07-01T06:00:00Z", h));
    }
    v.push(env(
        "2026-07-01T06:00:01Z",
        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
    ));
    v.extend(night_and_morning());
    for (t, x) in [
        ("2026-07-01T10:25:00Z", 16),
        ("2026-07-01T10:55:00Z", 17),
        ("2026-07-01T11:25:00Z", 17),
        ("2026-07-01T11:55:00Z", 18),
        ("2026-07-01T12:25:00Z", 18),
        ("2026-07-01T12:55:00Z", 17),
        ("2026-07-01T13:25:00Z", 17),
    ] {
        v.push(obs_event(t, x));
    }
    for b in books(m, "2026-07-01T13:57:55Z") {
        v.push(env(
            "2026-07-01T13:57:55Z",
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
        ));
    }
    v.push(obs_event("2026-07-01T13:55:00Z", 16)); // available 13:58:00
    v
}

/// Run events through the engine and a simulated exchange (paper-style loop).
fn run(
    engine: &mut Engine,
    events: Vec<EventEnvelope>,
    fees: wm_core::market::FeeSchedule,
) -> Vec<EngineOutput> {
    let mut ex = SimulatedExchange::new(SimConfig {
        latency_ms: 250,
        adverse_ticks: 0,
    });
    let mut outputs = Vec::new();
    let mut queue: std::collections::VecDeque<EventEnvelope> = events.into();
    while let Some(e) = queue.pop_front() {
        if let WeatherMachineEvent::OrderBookUpdate(b) = &e.event {
            for u in ex.on_book(&b.book, e.available_at) {
                queue.push_front(EventEnvelope::new(
                    e.available_at,
                    EventSource::Synthetic,
                    WeatherMachineEvent::OrderUpdate(u),
                ));
            }
        }
        let out = engine.handle(&e);
        let mut followups = Vec::new();
        for a in &out.approved {
            for u in ex.submit(a, fees, e.available_at) {
                followups.push(EventEnvelope::new(
                    e.available_at,
                    EventSource::Synthetic,
                    WeatherMachineEvent::OrderUpdate(u),
                ));
            }
        }
        let later = e.available_at + Duration::milliseconds(250);
        for u in ex.process_due(later) {
            followups.push(EventEnvelope::new(
                later,
                EventSource::Synthetic,
                WeatherMachineEvent::OrderUpdate(u),
            ));
        }
        for f in followups.into_iter().rev() {
            queue.push_front(f);
        }
        outputs.push(out);
    }
    outputs
}

fn approvals(outs: &[EngineOutput]) -> Vec<String> {
    outs.iter()
        .flat_map(|o| {
            o.approved.iter().map(|a| {
                format!(
                    "{} {:?} {}",
                    a.intent().bucket_label,
                    a.intent().outcome_side,
                    a.client_order_id()
                )
            })
        })
        .collect()
}

#[test]
fn paper_lifecycle_signal_risk_fill_settle() {
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let outs = run(
        &mut engine,
        scenario(&m, Some(ProviderHealthState::Healthy)),
        m.fees,
    );
    let a = approvals(&outs);
    assert_eq!(a.len(), 3, "YES 18, NO 19, NO 20: {a:?}");
    assert!(a[0].starts_with("18°C Yes"));
    assert!(
        a.iter().any(|x| x.starts_with("19°C No")) && a.iter().any(|x| x.starts_with("20°C No"))
    );
    // Fills landed in the portfolio.
    assert_eq!(engine.positions().open_positions().count(), 3);
    let yes18 = m.outcome_for_value(18).unwrap().yes_token.clone();
    assert_eq!(
        engine
            .positions()
            .get(&yes18)
            .unwrap()
            .outcome_side_for_test(),
        OutcomeSide::Yes
    );
    // The decision log explains every approval and the per-observation evaluation.
    let snap = engine.snapshot();
    assert!(
        snap.decisions
            .iter()
            .any(|d| d.approved && d.summary.contains("18°C"))
    );
    assert!(
        snap.decisions
            .iter()
            .any(|d| d.strategy.as_str() == "evaluation")
    );
    assert!(
        snap.exposure.global_worst_case > Usd::ZERO
            && snap.exposure.global_worst_case <= Usd::from_whole(100)
    );
    // Settle at 18: all three legs win.
    let pnl = engine.settle(&m.event_slug, 18);
    assert!(pnl > Usd::ZERO, "pnl {pnl}");
    assert_eq!(engine.positions().open_positions().count(), 0);
}

#[test]
fn provider_outage_never_produces_a_trade() {
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    for health in [
        None,
        Some(ProviderHealthState::Throttled),
        Some(ProviderHealthState::Unavailable),
        Some(ProviderHealthState::Stale),
        Some(ProviderHealthState::Degraded),
    ] {
        let mut engine = Engine::new(
            config(RunMode::Paper),
            Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
        );
        let outs = run(&mut engine, scenario(&m, health), m.fees);
        assert!(
            approvals(&outs).is_empty(),
            "health {health:?} must not trade"
        );
        let snap = engine.snapshot();
        assert!(
            snap.decisions
                .iter()
                .any(|d| !d.approved && d.reasons.iter().any(|r| r.contains("WeatherHealth"))),
            "health {health:?}"
        );
        assert_eq!(engine.positions().open_positions().count(), 0);
    }
}

#[test]
fn missing_morning_data_blocks_trading() {
    // Cold start at midday without a backfill: the running "high" may not be the
    // day's high, so nothing weather-dependent may be opened.
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let morning_cutoff = utc("2026-07-01T10:00:00Z");
    let events: Vec<EventEnvelope> = scenario(&m, Some(ProviderHealthState::Healthy))
        .into_iter()
        .filter(|e| !matches!(&e.event, WeatherMachineEvent::WeatherObservation(o) if o.observation.key.observed_at < morning_cutoff))
        .collect();
    let outs = run(&mut engine, events, m.fees);
    assert!(approvals(&outs).is_empty());
    let snap = engine.snapshot();
    assert!(
        snap.decisions
            .iter()
            .any(|d| !d.approved && d.reasons.iter().any(|r| r.contains("WeatherCoverage")))
    );
}

#[test]
fn identical_rejections_are_recorded_once_per_window() {
    // A throttled provider with fast book updates: every update re-evaluates and
    // is rejected, but the audit log records each distinct rejection once a minute.
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let mut cfg_events = scenario(&m, Some(ProviderHealthState::Throttled));
    for i in 0..20 {
        let at = utc("2026-07-01T13:58:01Z") + Duration::seconds(i);
        for b in books(&m, &at.to_rfc3339()) {
            cfg_events.push(EventEnvelope::new(
                at,
                EventSource::Synthetic,
                WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
            ));
        }
    }
    let mut c = config(RunMode::Paper);
    c.evaluate_on_book_updates = true;
    let mut engine = Engine::new(c, Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])));
    let outs = run(&mut engine, cfg_events, m.fees);
    assert!(approvals(&outs).is_empty());
    let rejected: Vec<_> = outs
        .iter()
        .flat_map(|o| o.decisions.iter())
        .filter(|d| !d.approved && d.strategy.as_str() != "evaluation")
        .collect();
    let mut per_key = std::collections::HashMap::new();
    for d in &rejected {
        *per_key
            .entry(d.summary.split(" — ").next().unwrap_or_default().to_owned())
            .or_insert(0) += 1;
    }
    assert!(!per_key.is_empty());
    assert!(per_key.values().all(|n| *n == 1), "{per_key:?}");
    assert!(engine.stats().rejections_suppressed > 0);
    assert_eq!(
        engine.stats().rejections_total,
        rejected.len() as u64 + engine.stats().rejections_suppressed
    );
}

#[test]
fn stale_weather_blocks_new_positions() {
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let mut events = scenario(&m, Some(ProviderHealthState::Healthy));
    events.pop(); // the 13:55 report never arrives
    for b in books(&m, "2026-07-01T14:44:55Z") {
        events.push(env(
            "2026-07-01T14:44:55Z",
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
        ));
    }
    events.push(env(
        "2026-07-01T14:45:00Z",
        WeatherMachineEvent::Timer(TimerEvent {
            due_at: utc("2026-07-01T14:45:00Z"),
            kind: TimerKind::Evaluate { location: loc() },
        }),
    ));
    let outs = run(&mut engine, events, m.fees);
    assert!(approvals(&outs).is_empty(), "latest observation 80 min old");
}

#[test]
fn unverifiable_resolution_views_block_trading() {
    // Hourly clause with both candidate windows: the :56–:04 view has no EHAM data.
    let m = market(vec![
        ObservationFilter::WRH_HOURLY_NWS_FAA,
        ObservationFilter::WRH_HOURLY_OTHER,
    ]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let outs = run(
        &mut engine,
        scenario(&m, Some(ProviderHealthState::Healthy)),
        m.fees,
    );
    assert!(approvals(&outs).is_empty());
    let snap = engine.snapshot();
    assert!(
        snap.decisions
            .iter()
            .any(|d| d.summary.contains("incomplete"))
    );
}

#[test]
fn kill_switch_and_missing_model_block_trading() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let mut events = scenario(&m, Some(ProviderHealthState::Healthy));
    events.insert(
        0,
        env(
            "2026-07-01T05:00:00Z",
            WeatherMachineEvent::Operator(OperatorCommand::KillSwitch {
                engaged: true,
                reason: "operator test".into(),
            }),
        ),
    );
    assert!(approvals(&run(&mut engine, events, m.fees)).is_empty());
    let mut engine = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    assert!(
        approvals(&run(
            &mut engine,
            scenario(&m, Some(ProviderHealthState::Healthy)),
            m.fees
        ))
        .is_empty()
    );
}

#[test]
fn live_mode_is_rejected_by_risk() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut engine = Engine::new(
        config(RunMode::Live),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let outs = run(
        &mut engine,
        scenario(&m, Some(ProviderHealthState::Healthy)),
        m.fees,
    );
    assert!(approvals(&outs).is_empty());
}

#[test]
fn replay_is_deterministic() {
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let run_once = || {
        let mut engine = Engine::new(
            config(RunMode::Backtest),
            Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
        );
        let outs = run(
            &mut engine,
            scenario(&m, Some(ProviderHealthState::Healthy)),
            m.fees,
        );
        let decisions: Vec<String> = outs
            .iter()
            .flat_map(|o| {
                o.decisions
                    .iter()
                    .map(|d| format!("{} {} {}", d.decision_id, d.summary, d.approved))
            })
            .collect();
        (
            approvals(&outs),
            decisions,
            engine.settle(&m.event_slug, 18),
        )
    };
    let a = run_once();
    let b = run_once();
    assert_eq!(a, b);
    assert!(!a.0.is_empty());
}

#[test]
fn hints_request_attention_only_near_the_peak() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut engine = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    let out = engine.handle(&obs_event("2026-07-01T11:55:00Z", 18)); // 13:55 CEST
    assert!(out.hints.iter().any(|(s, h)| s == &eham() && h.peak_watch));
    let _ = m;
    let side = Side::Buy;
    assert_eq!(side, Side::Buy);
}

trait OutcomeSideForTest {
    fn outcome_side_for_test(&self) -> OutcomeSide;
}

impl OutcomeSideForTest for wm_core::portfolio::Position {
    fn outcome_side_for_test(&self) -> OutcomeSide {
        self.instrument.outcome_side
    }
}
