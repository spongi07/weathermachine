#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end kernel scenarios (same code path as backtest/paper/live).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::sync::Arc;
use wm_core::event::{
    EventEnvelope, EventSource, ForecastEvent, MarketSnapshotEvent, ObservationEvent,
    OperatorCommand, OrderBookEvent, ProviderHealthEvent, TimerEvent, TimerKind,
    WeatherMachineEvent,
};
use wm_core::forecast::ForecastProduct;
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::{BookLevel, DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::resolution::ObservationFilter;
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::RunMode;
use wm_core::units::{Price, Shares, TempC, Usd};
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
        certain: wm_strategy::CertainConfig::default(),
        book_confirmed: wm_strategy::BookConfirmedConfig::default(),
        peak_slot: wm_strategy::PeakSlotConfig::default(),
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

/// Strategy E end to end: after the peak the high's YES offers thin out.
/// The engine evaluates only on weather events here: the book in force 30
/// minutes before the decision (13:20) is seen by no evaluation and reaches
/// the strategy through `observe_book`. Compared with the 12:50 book an
/// evaluation saw instead, the ask fell (0.96 → 0.95) and E would not trade.
#[test]
fn strategy_e_buys_the_high_when_its_book_shrinks_after_the_peak() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut cfg = config(RunMode::Paper);
    // E alone on the high's bucket.
    cfg.buy_yes.enabled = false;
    cfg.buy_no.enabled = false;
    cfg.certain.enabled = false;
    let mut engine = Engine::new(
        cfg.clone(),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let yes18 = m.outcome_for_value(18).unwrap().yes_token.clone();
    let book = |at: &str, bid: &str, asks: &[(&str, i64)]| {
        let mut b = synthetic_book(&yes18, Some(bid), None, 200, utc(at));
        b.asks = asks
            .iter()
            .map(|(p, s)| BookLevel {
                price: Price::parse(p).unwrap(),
                size: Shares::from_whole(*s),
            })
            .collect();
        env(
            at,
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
        )
    };
    let mut events = vec![
        health_event("2026-07-01T06:00:00Z", ProviderHealthState::Healthy),
        env(
            "2026-07-01T06:00:01Z",
            WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
        ),
    ];
    events.extend(night_and_morning());
    for (t, x) in [
        ("2026-07-01T10:25:00Z", 16),
        ("2026-07-01T10:55:00Z", 17),
        ("2026-07-01T11:25:00Z", 17),
        ("2026-07-01T11:55:00Z", 18),
        ("2026-07-01T12:25:00Z", 18),
    ] {
        events.push(obs_event(t, x));
    }
    events.push(book("2026-07-01T12:50:00Z", "0.94", &[("0.96", 300)]));
    events.push(obs_event("2026-07-01T12:55:00Z", 17)); // at 12:58: no history yet
    events.push(book(
        "2026-07-01T13:20:00Z",
        "0.92",
        &[("0.93", 200), ("0.94", 200), ("0.95", 200)],
    ));
    events.push(book(
        "2026-07-01T13:40:00Z",
        "0.93",
        &[("0.94", 150), ("0.95", 200)],
    ));
    events.push(book(
        "2026-07-01T13:57:50Z",
        "0.94",
        &[("0.95", 120), ("0.96", 100)],
    ));
    events.push(obs_event("2026-07-01T13:55:00Z", 16)); // at 13:58: 600 → 120 offered
    // Fail closed: without a model E stays silent.
    let mut no_model = Engine::new(cfg, Arc::new(NoEdgeModel));
    assert!(approvals(&run(&mut no_model, events.clone(), m.fees)).is_empty());
    let outs = run(&mut engine, events, m.fees);
    let a = approvals(&outs);
    assert_eq!(a.len(), 1, "{a:?}");
    assert!(a[0].starts_with("18°C Yes"), "{a:?}");
    let approved = outs.iter().flat_map(|o| &o.approved).next().unwrap();
    assert_eq!(approved.intent().strategy.as_str(), "E_book_confirmed_high");
    assert_eq!(approved.intent().limit_price, Price::parse("0.95").unwrap());
    assert!(
        approved
            .intent()
            .rationale
            .iter()
            .any(|r| r.contains("600 → 120 shares offered ≤ 0.95")),
        "{:?}",
        approved.intent().rationale
    );
    // The 12:58 evaluation said why it waited.
    let snap = engine.snapshot();
    assert!(
        snap.decisions.iter().any(|d| {
            let o = d.outputs.to_string();
            d.strategy.as_str() == "evaluation"
                && o.contains("E 18°C YES")
                && o.contains("book history shorter than 30m")
        }),
        "{:?}",
        snap.decisions
            .iter()
            .map(|d| &d.outputs)
            .collect::<Vec<_>>()
    );
    assert!(engine.positions().get(&yes18).is_some(), "filled in paper");
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
fn forecasts_never_change_the_observed_high_or_create_trades() {
    // A (wildly) warmer forecast is a predictive input only: for a model that
    // does not condition on forecasts, the observed high, the resolution views
    // and every decision stay exactly the same (models that adopted a forecast:
    // `a_used_forecast_changes_probabilities_only_under_the_knowledge_rule`).
    let m = market(vec![
        ObservationFilter::AllRows,
        ObservationFilter::WRH_HOURLY_NWS_FAA,
    ]);
    let model = || Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001]));
    let mut base = Engine::new(config(RunMode::Paper), model());
    let base_out = run(
        &mut base,
        scenario(&m, Some(ProviderHealthState::Healthy)),
        m.fees,
    );
    let mut with_fc = Engine::new(config(RunMode::Paper), model());
    let mut events = scenario(&m, Some(ProviderHealthState::Healthy));
    let forecast = wm_core::event::ForecastEvent {
        location: loc(),
        provider: ProviderId::new("knmi").unwrap(),
        model: "harmonie".into(),
        issued_at: utc("2026-07-01T12:00:00Z"),
        predicted_max: Some(TempC::from_whole(30)),
        hourly: vec![(utc("2026-07-01T13:00:00Z"), TempC::from_whole(30))],
        lead_days: None,
    };
    events.insert(
        10,
        env(
            "2026-07-01T12:00:00Z",
            WeatherMachineEvent::ForecastUpdate(forecast),
        ),
    );
    let fc_out = run(&mut with_fc, events, m.fees);
    assert_eq!(approvals(&base_out), approvals(&fc_out));
    let (a, b) = (base.snapshot(), with_fc.snapshot());
    assert_eq!(
        a.locations[0].views, b.locations[0].views,
        "views/high unchanged by forecasts"
    );
    assert_eq!(
        base.final_value(&m.event_slug),
        with_fc.final_value(&m.event_slug)
    );
}

/// A model that conditions on the day-1 forecast: 98.5 % final unless the
/// forecast expects the rest of the day to be warmer.
struct ForecastAwareModel(ForecastProduct);

impl ProbabilityModel for ForecastAwareModel {
    fn id(&self) -> &str {
        "forecast-aware-test-model"
    }
    fn distribution(&self, f: &PeakFeatures) -> Option<IncrementDistribution> {
        let probs = match f.forecast_rise_tenths {
            Some(r) if r >= 5 => vec![0.60, 0.30, 0.08, 0.02],
            _ => vec![0.985, 0.012, 0.002, 0.001],
        };
        Some(IncrementDistribution {
            probs,
            support: 500,
            source: format!("rise={:?}", f.forecast_rise_tenths),
        })
    }
    fn forecast_product(&self) -> Option<&ForecastProduct> {
        Some(&self.0)
    }
}

fn day1_product() -> ForecastProduct {
    ForecastProduct {
        provider: ProviderId::open_meteo(),
        model: "gfs_global".into(),
        lead_days: 1,
        ready_local_minute: 480, // 08:00 CEST = 06:00Z
    }
}

/// Day-1 series for 1 July (local day = 30 Jun 22:00Z … 1 Jul 22:00Z):
/// 18.0 °C until 14:00Z, `later` tenths afterwards.
fn day1_forecast(model: &str, later: i32) -> WeatherMachineEvent {
    let start = utc("2026-06-30T22:00:00Z");
    let hourly = (0..=24)
        .map(|h| {
            let t = start + Duration::hours(h);
            let v = if t > utc("2026-07-01T14:00:00Z") {
                later
            } else {
                180
            };
            (t, TempC::from_tenths(v))
        })
        .collect();
    WeatherMachineEvent::ForecastUpdate(ForecastEvent {
        location: loc(),
        provider: ProviderId::open_meteo(),
        model: model.into(),
        issued_at: utc("2026-07-01T05:00:00Z"),
        predicted_max: None,
        hourly,
        lead_days: Some(1),
    })
}

#[test]
fn a_used_forecast_changes_probabilities_only_under_the_knowledge_rule() {
    let m = market(vec![ObservationFilter::AllRows]);
    let model = || Arc::new(ForecastAwareModel(day1_product()));
    let with = |extra: Vec<(usize, EventEnvelope)>| {
        let mut e = Engine::new(config(RunMode::Paper), model());
        let mut events = scenario(&m, Some(ProviderHealthState::Healthy));
        for (i, ev) in extra {
            events.insert(i, ev);
        }
        let out = run(&mut e, events, m.fees);
        (e, out)
    };
    let (base, base_out) = with(vec![]);
    assert!(
        !approvals(&base_out).is_empty(),
        "the scenario trades without a forecast"
    );

    // Warming expected after 14:00Z (rise +2.0 °C at the 13:58Z decision), known at 06:30Z.
    let (warm, warm_out) = with(vec![(
        2,
        env("2026-07-01T06:30:00Z", day1_forecast("gfs_global", 200)),
    )]);
    assert!(
        approvals(&warm_out).is_empty(),
        "{:?}",
        approvals(&warm_out)
    );
    let fc = warm.snapshot().locations[0].forecast.clone().unwrap();
    assert!(fc.in_use, "{fc:?}");
    assert_eq!(fc.product, "open_meteo/gfs_global/d1");
    assert_eq!(fc.rise_tenths, Some(20));
    assert_eq!(fc.day_max_tenths, Some(200));

    // Cooling expected: the same trades as without a forecast.
    let (_, cool_out) = with(vec![(
        2,
        env("2026-07-01T06:30:00Z", day1_forecast("gfs_global", 150)),
    )]);
    assert_eq!(approvals(&cool_out), approvals(&base_out));

    // Retrieved at 05:30Z — before today's ready time (08:00 local): ignored all day.
    let (early, early_out) = with(vec![(
        0,
        env("2026-07-01T05:30:00Z", day1_forecast("gfs_global", 200)),
    )]);
    assert_eq!(approvals(&early_out), approvals(&base_out));
    let fc = early.snapshot().locations[0].forecast.clone().unwrap();
    assert!(!fc.in_use && fc.status.contains("ready time"), "{fc:?}");

    // Another model's series is another product: ignored.
    let (other, other_out) = with(vec![(
        2,
        env("2026-07-01T06:30:00Z", day1_forecast("ecmwf_ifs", 200)),
    )]);
    assert_eq!(approvals(&other_out), approvals(&base_out));
    assert!(
        other.snapshot().locations[0].forecast.is_none(),
        "not the model's product"
    );

    // Forecasts never change what was observed or how the market settles.
    for e in [&warm, &early, &other] {
        assert_eq!(
            e.final_value(&m.event_slug),
            base.final_value(&m.event_slug)
        );
        let (a, b) = (&e.snapshot().locations[0], &base.snapshot().locations[0]);
        assert_eq!(a.series, b.series);
        assert_eq!(
            a.views.iter().map(|v| v.state.clone()).collect::<Vec<_>>(),
            b.views.iter().map(|v| v.state.clone()).collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_swapped_model_is_used_from_the_next_evaluation() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut e = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    let mut events = scenario(&m, Some(ProviderHealthState::Healthy));
    let last = events.pop().unwrap(); // the 13:55Z report that triggers the decision
    let out = run(&mut e, events, m.fees);
    assert!(approvals(&out).is_empty(), "no model, no trades");
    e.set_model(Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])));
    assert_eq!(e.model_id(), "fixed-test-model");
    let out = run(&mut e, vec![last], m.fees);
    assert!(!approvals(&out).is_empty());
    assert_eq!(e.snapshot().model_id, "fixed-test-model");
}

#[test]
fn a_new_high_is_traded_on_the_observation_event_itself() {
    // Up to 09:55Z the high is 15; 10:25Z 16; 10:55Z (known 10:58Z) 17 kills
    // the 16 °C bucket. Its NO is still quoted at 0.75 (stale).
    let m = market(vec![ObservationFilter::AllRows]);
    let mut events = vec![
        health_event("2026-06-30T22:00:00Z", ProviderHealthState::Healthy),
        env(
            "2026-06-30T22:00:01Z",
            WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
        ),
    ];
    events.extend(night_and_morning());
    events.push(obs_event("2026-07-01T10:25:00Z", 16));
    let no16 = m.outcome_for_value(16).unwrap().no_token.clone();
    events.push(env(
        "2026-07-01T10:57:30Z",
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(
                &no16,
                Some("0.70"),
                Some("0.75"),
                200,
                utc("2026-07-01T10:57:30Z"),
            ),
        }),
    ));
    // The book did not change for 30 s; the live feed confirms it at 10:57:55.
    events.push(env(
        "2026-07-01T10:57:55Z",
        WeatherMachineEvent::MarketStreamHeartbeat(wm_core::event::StreamHeartbeatEvent {
            connected_since: utc("2026-07-01T06:00:00Z"),
        }),
    ));
    events.push(obs_event("2026-07-01T10:55:00Z", 17)); // known 10:58:00
    let mut quiet = events.clone();
    let mut e = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    let outs = run(&mut e, events, m.fees);
    // The observation event that raised the high carries the approval.
    let decided = outs
        .iter()
        .find(|o| !o.approved.is_empty())
        .expect("strategy D traded");
    let a = &decided.approved[0];
    assert_eq!(a.intent().bucket_label, "16°C");
    assert_eq!(a.intent().outcome_side, OutcomeSide::No);
    assert_eq!(a.intent().strategy.as_str(), "D_certain_outcome");
    assert_eq!(
        approvals(&outs).len(),
        1,
        "one decided bucket had a book: {:?}",
        approvals(&outs)
    );
    // No model was needed, and nothing else was bought.
    let snap = e.snapshot();
    let pos: Vec<_> = snap
        .positions
        .iter()
        .filter(|p| p.shares.micros() > 0)
        .collect();
    assert_eq!(pos.len(), 1);
    assert_eq!(pos[0].instrument.token, no16);
    // The audit log says why, with the numbers: each line carries the price,
    // the probability used and the EV, and the summary names the closest call.
    let eval = decided
        .decisions
        .iter()
        .find(|d| d.strategy.as_str() == "evaluation")
        .expect("routine evaluation recorded");
    // 1 − 0.75 − fee 0.05·0.75·0.25 − slippage 0.002 = 0.2386
    let d_line = "D 16°C NO · ask 0.75 · p 1.000 (market 0.725) · EV +0.2386 — SIGNAL";
    assert!(
        eval.summary.ends_with(&format!("closest: {d_line}")),
        "{}",
        eval.summary
    );
    let lines: Vec<&str> = eval.outputs["evaluations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect();
    assert!(lines.contains(&d_line), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("D 15°C NO · no ask") && l.ends_with("— no order book")),
        "{lines:?}"
    );

    // The same book from an earlier connection (reconnected at 10:57:40) is
    // not confirmed: 30 s old, stale, no trade.
    for ev in &mut quiet {
        if let WeatherMachineEvent::MarketStreamHeartbeat(h) = &mut ev.event {
            h.connected_since = utc("2026-07-01T10:57:40Z");
        }
    }
    let mut e = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    let outs = run(&mut e, quiet, m.fees);
    assert!(approvals(&outs).is_empty(), "{:?}", approvals(&outs));
}

#[test]
fn a_corrected_high_blocks_certain_outcome_trades() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut events = vec![
        health_event("2026-06-30T22:00:00Z", ProviderHealthState::Healthy),
        env(
            "2026-06-30T22:00:01Z",
            WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
        ),
    ];
    events.extend(night_and_morning());
    // A correction to an earlier report arrives just before the new high.
    let first = obs_event("2026-07-01T09:55:00Z", 15);
    let WeatherMachineEvent::WeatherObservation(o) = &first.event else {
        unreachable!()
    };
    let mut corrected = o.observation.clone();
    corrected.version = 2;
    corrected.temperature = Some(TempC::from_whole(14));
    events.push(env(
        "2026-07-01T10:50:00Z",
        WeatherMachineEvent::WeatherCorrection(wm_core::event::CorrectionEvent {
            previous: o.observation.clone(),
            current: corrected,
            labeled: true,
        }),
    ));
    let no16 = m.outcome_for_value(16).unwrap().no_token.clone();
    events.push(obs_event("2026-07-01T10:25:00Z", 16));
    events.push(env(
        "2026-07-01T10:57:30Z",
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
            book: synthetic_book(
                &no16,
                Some("0.70"),
                Some("0.75"),
                200,
                utc("2026-07-01T10:57:30Z"),
            ),
        }),
    ));
    events.push(obs_event("2026-07-01T10:55:00Z", 17));
    let mut e = Engine::new(config(RunMode::Paper), Arc::new(NoEdgeModel));
    let outs = run(&mut e, events, m.fees);
    assert!(
        approvals(&outs).is_empty(),
        "correction cooldown: {:?}",
        approvals(&outs)
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

/// A fixed model that also carries learned peak times (strategy F's slots).
struct PeakModel {
    probs: Vec<f64>,
    peaks: wm_strategy::PeakTimes,
}

impl ProbabilityModel for PeakModel {
    fn id(&self) -> &str {
        "fixed-test-model-with-peak-times"
    }
    fn distribution(&self, _f: &PeakFeatures) -> Option<IncrementDistribution> {
        Some(IncrementDistribution {
            probs: self.probs.clone(),
            support: 500,
            source: "fixed".into(),
        })
    }
    fn peak_times(&self) -> Option<&wm_strategy::PeakTimes> {
        Some(&self.peaks)
    }
}

/// Summer peak times with the median at `from` and the 90th percentile at
/// `to` (local minutes at :25 or :55).
fn summer_peaks(from: u16, to: u16) -> wm_strategy::PeakTimes {
    let start = utc("2026-06-30T22:25:00Z");
    let mut b = wm_strategy::PeakTimesBuilder::new();
    for peak in std::iter::repeat_n(from, 5).chain(std::iter::repeat_n(to, 5)) {
        let points: Vec<wm_strategy::ObsPoint> = (0..48u16)
            .map(|i| {
                let minute = (25 + 30 * i) % 1440;
                wm_strategy::ObsPoint {
                    observed_at: start + Duration::minutes(30 * i64::from(i)),
                    local_minute_of_day: minute,
                    local_minute_of_hour: (minute % 60) as u8,
                    temp: TempC::from_whole(if minute == peak { 25 } else { 15 }),
                    report_type: ReportType::Metar,
                    version: 1,
                }
            })
            .collect();
        b.add_day(
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            wm_core::time::Season::Summer,
            &points,
            25,
        );
    }
    b.build()
}

/// The shipped risk limits, with strategy F's own caps.
fn risk_with_f_caps() -> RiskConfig {
    let mut r = RiskConfig {
        global_max_exposure_usd: Usd::from_whole(200),
        max_location_exposure_usd: Some(Usd::from_whole(150)),
        max_daily_new_exposure_usd: Some(Usd::from_whole(160)),
        ..RiskConfig::default()
    };
    r.strategy_caps.insert(
        "F_peak_slot".into(),
        wm_risk::StrategyCaps {
            position_size_usd: Usd::from_whole(100),
            max_market_exposure_usd: Some(Usd::from_whole(110)),
            max_strategy_exposure_usd: Some(Usd::from_whole(110)),
        },
    );
    r
}

/// Strategy F end to end: high 18 °C at 13:55 local, the 15:55 report known
/// at 15:58, inside the learned summer slot; 100 shares cost at most 0.94.
#[test]
fn strategy_f_buys_100_shares_of_the_high_inside_the_learned_slot() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut cfg = config(RunMode::Paper);
    // F alone.
    cfg.buy_yes.enabled = false;
    cfg.buy_no.enabled = false;
    cfg.certain.enabled = false;
    cfg.book_confirmed.enabled = false;
    cfg.risk = risk_with_f_caps();
    let yes18 = m.outcome_for_value(18).unwrap().yes_token.clone();
    let mut events = vec![
        health_event("2026-07-01T06:00:00Z", ProviderHealthState::Healthy),
        env(
            "2026-07-01T06:00:01Z",
            WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
        ),
    ];
    events.extend(night_and_morning());
    for (t, x) in [
        ("2026-07-01T10:25:00Z", 16),
        ("2026-07-01T10:55:00Z", 17),
        ("2026-07-01T11:25:00Z", 17),
        ("2026-07-01T11:55:00Z", 18),
        ("2026-07-01T12:25:00Z", 18),
        ("2026-07-01T12:55:00Z", 17),
        ("2026-07-01T13:25:00Z", 17),
    ] {
        events.push(obs_event(t, x));
    }
    let mut b = synthetic_book(&yes18, Some("0.92"), None, 200, utc("2026-07-01T13:57:50Z"));
    b.asks = vec![
        BookLevel {
            price: Price::parse("0.93").unwrap(),
            size: Shares::from_whole(60),
        },
        BookLevel {
            price: Price::parse("0.94").unwrap(),
            size: Shares::from_whole(100),
        },
    ];
    events.push(env(
        "2026-07-01T13:57:50Z",
        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
    ));
    events.push(obs_event("2026-07-01T13:55:00Z", 17)); // known at 15:58 local
    let model = |from: u16, to: u16| {
        Arc::new(PeakModel {
            probs: vec![0.985, 0.012, 0.002, 0.001],
            peaks: summer_peaks(from, to),
        })
    };

    // Learned slot 15:25–17:26: bought, 100 shares at ≤ 0.94, filled.
    let mut engine = Engine::new(cfg.clone(), model(15 * 60 + 25, 17 * 60 + 25));
    let outs = run(&mut engine, events.clone(), m.fees);
    let a = approvals(&outs);
    assert_eq!(a.len(), 1, "{a:?}");
    let approved = outs.iter().flat_map(|o| &o.approved).next().unwrap();
    let i = approved.intent();
    assert_eq!(i.strategy.as_str(), "F_peak_slot");
    assert_eq!(i.bucket_label, "18°C");
    assert_eq!(i.shares, Shares::from_whole(100));
    assert_eq!(i.limit_price, Price::parse("0.94").unwrap());
    assert_eq!(
        i.rationale[0],
        "15:58 local inside the summer slot 15:25–17:26 (median → 90% of 10 days' peak times)"
    );
    let pos = engine.positions().get(&yes18).expect("filled in paper");
    assert_eq!(pos.shares, Shares::from_whole(100));

    // Without F's own risk caps the $10 position limit refuses it.
    let mut capped = cfg.clone();
    capped.risk = RiskConfig::default();
    let mut engine = Engine::new(capped, model(15 * 60 + 25, 17 * 60 + 25));
    let outs = run(&mut engine, events.clone(), m.fees);
    assert!(approvals(&outs).is_empty());
    assert!(
        engine.snapshot().decisions.iter().any(|d| {
            d.strategy.as_str() == "F_peak_slot"
                && !d.approved
                && d.reasons.iter().any(|r| r.starts_with("PositionSize"))
        }),
        "the rejection is recorded"
    );

    // A later slot (16:25–17:56): 15:58 is before it, and the evaluation says so.
    let mut engine = Engine::new(cfg, model(16 * 60 + 25, 17 * 60 + 55));
    let outs = run(&mut engine, events, m.fees);
    assert!(approvals(&outs).is_empty());
    assert!(
        engine.snapshot().decisions.iter().any(|d| {
            let o = d.outputs.to_string();
            d.strategy.as_str() == "evaluation"
                && o.contains("F 18°C YES")
                && o.contains("15:58 outside the summer slot 16:25–17:56")
        }),
        "{:?}",
        engine
            .snapshot()
            .decisions
            .iter()
            .map(|d| &d.outputs)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Restart: the paper book of earlier runs (Engine::restore)
// ---------------------------------------------------------------------------

use wm_core::ids::{ClientOrderId, StrategyId};
use wm_core::portfolio::InstrumentRef;
use wm_core::trading::{Fill, Liquidity};
use wm_engine::{RestoreState, RestoredFill};

fn restored(
    m: &DailyTemperatureMarket,
    value: i32,
    id: &str,
    side: Side,
    price: &str,
    shares: i64,
    ts: &str,
) -> RestoredFill {
    let o = m.outcome_for_value(value).unwrap();
    RestoredFill {
        fill: Fill {
            client_order_id: ClientOrderId::new(id).unwrap(),
            token: o.yes_token.clone(),
            side,
            price: Price::parse(price).unwrap(),
            shares: Shares::from_whole(shares),
            fee: Usd::ZERO,
            liquidity: Liquidity::Taker,
            ts: utc(ts),
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

/// The F scenario after a restart. Before it, F bought 100 × 18 °C YES at
/// 0.94 (15:40 local). Restored, the new run neither buys 18 °C again nor
/// a second bucket the same day: 19 °C becomes the high in the slot and is
/// offered at 0.94, but F's market and strategy caps ($110) and the daily
/// new exposure ($160) count the restored $94. A flat run buys it.
#[test]
fn a_restored_book_keeps_f_to_one_position_a_day() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut cfg = config(RunMode::Paper);
    cfg.buy_yes.enabled = false;
    cfg.buy_no.enabled = false;
    cfg.certain.enabled = false;
    cfg.book_confirmed.enabled = false;
    cfg.risk = risk_with_f_caps();
    let model = || {
        Arc::new(PeakModel {
            probs: vec![0.985, 0.012, 0.002, 0.001],
            peaks: summer_peaks(15 * 60 + 25, 17 * 60 + 25),
        })
    };
    let state = RestoreState {
        markets: vec![m.clone()],
        fills: vec![restored(
            &m,
            18,
            "wm-old-1",
            Side::Buy,
            "0.94",
            100,
            "2026-07-01T13:40:00Z",
        )],
        new_exposure_today: Usd::from_whole(94),
    };
    // The day up to the 13:25Z report; then a book on `value` and the 13:55Z
    // report at `last` (known 13:58Z, 15:58 local, inside the slot).
    let day = |value: i32, last: i32| {
        let mut v = vec![
            health_event("2026-07-01T06:00:00Z", ProviderHealthState::Healthy),
            env(
                "2026-07-01T06:00:01Z",
                WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
            ),
        ];
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
        let token = m.outcome_for_value(value).unwrap().yes_token.clone();
        let mut b = synthetic_book(&token, Some("0.92"), None, 200, utc("2026-07-01T13:57:50Z"));
        b.asks = vec![
            BookLevel {
                price: Price::parse("0.93").unwrap(),
                size: Shares::from_whole(60),
            },
            BookLevel {
                price: Price::parse("0.94").unwrap(),
                size: Shares::from_whole(100),
            },
        ];
        v.push(env(
            "2026-07-01T13:57:50Z",
            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book: b }),
        ));
        v.push(obs_event("2026-07-01T13:55:00Z", last));
        v
    };
    let now = utc("2026-07-01T13:45:00Z");

    // The book comes back: one position, F's strategy, today's $94.
    let mut engine = Engine::new(cfg.clone(), model());
    let s = engine.restore(&state, now);
    assert_eq!((s.markets, s.fills, s.open_positions), (1, 1, 1));
    assert_eq!(s.open_shares, Shares::from_whole(100));
    assert_eq!(s.open_cost, Usd::from_whole(94));
    assert!(s.rejected.is_empty());
    let snap = engine.snapshot();
    assert_eq!(snap.daily_new_exposure, Usd::from_whole(94));
    assert_eq!(snap.exposure.global_worst_case, Usd::from_whole(94));

    // Same bucket after the restart: already positioned, nothing bought.
    let outs = run(&mut engine, day(18, 17), m.fees);
    assert!(approvals(&outs).is_empty(), "{:?}", approvals(&outs));
    assert!(
        engine.snapshot().decisions.iter().any(|d| {
            let o = d.outputs.to_string();
            d.strategy.as_str() == "evaluation"
                && o.contains("F 18°C YES")
                && o.contains("already positioned")
        }),
        "F sees the restored position"
    );

    // Another bucket the same day: F proposes it, the caps refuse it.
    let mut engine = Engine::new(cfg.clone(), model());
    engine.restore(&state, now);
    let outs = run(&mut engine, day(19, 19), m.fees);
    assert!(approvals(&outs).is_empty(), "{:?}", approvals(&outs));
    let refused = engine
        .snapshot()
        .decisions
        .into_iter()
        .find(|d| d.strategy.as_str() == "F_peak_slot" && !d.approved)
        .expect("F proposed 19 °C and risk refused it");
    let why = refused.reasons.join("; ");
    for check in ["MarketExposure", "StrategyExposure", "DailyNewExposure"] {
        assert!(why.contains(check), "{check} missing in {why}");
    }

    // A flat run (the old restart) buys the second bucket.
    let mut flat = Engine::new(cfg, model());
    let outs = run(&mut flat, day(19, 19), m.fees);
    let a = approvals(&outs);
    assert_eq!(a.len(), 1, "{a:?}");
    assert!(a[0].starts_with("19°C Yes"), "{a:?}");
}

/// Restored sales: only today's (UTC) realized P&L counts toward the daily
/// loss limit; a sale larger than the holding is refused, not applied.
#[test]
fn restored_sales_count_toward_today_only() {
    let m = market(vec![ObservationFilter::AllRows]);
    let mut engine = Engine::new(
        config(RunMode::Paper),
        Arc::new(FixedModel(vec![0.985, 0.012, 0.002, 0.001])),
    );
    let state = RestoreState {
        markets: vec![m.clone()],
        fills: vec![
            // Yesterday (UTC): +$1.00.
            restored(
                &m,
                18,
                "wm-a",
                Side::Buy,
                "0.50",
                10,
                "2026-06-30T09:00:00Z",
            ),
            restored(
                &m,
                18,
                "wm-b",
                Side::Sell,
                "0.60",
                10,
                "2026-06-30T10:00:00Z",
            ),
            // Today: −$0.50 on half of a new 10 shares, then an oversell.
            restored(
                &m,
                18,
                "wm-c",
                Side::Buy,
                "0.50",
                10,
                "2026-07-01T08:00:00Z",
            ),
            restored(
                &m,
                18,
                "wm-d",
                Side::Sell,
                "0.40",
                5,
                "2026-07-01T09:00:00Z",
            ),
            restored(
                &m,
                18,
                "wm-e",
                Side::Sell,
                "0.40",
                50,
                "2026-07-01T09:30:00Z",
            ),
        ],
        new_exposure_today: Usd::from_whole(5),
    };
    let s = engine.restore(&state, utc("2026-07-01T12:00:00Z"));
    let minus_half = Usd::from_micros(-500_000);
    assert_eq!(s.realized_today, minus_half);
    assert_eq!(s.open_shares, Shares::from_whole(5));
    assert_eq!(s.open_cost, Usd::from_micros(2_500_000));
    assert_eq!(s.rejected.len(), 1, "{:?}", s.rejected);
    assert!(
        s.rejected[0].starts_with("wm-e on ") && s.rejected[0].contains("exceeds held"),
        "{:?}",
        s.rejected
    );
    let snap = engine.snapshot();
    assert_eq!(snap.daily_realized_pnl, minus_half);
    assert_eq!(snap.realized_pnl_total, minus_half);
    assert_eq!(snap.daily_new_exposure, Usd::from_whole(5));
}
