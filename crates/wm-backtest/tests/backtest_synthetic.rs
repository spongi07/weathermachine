#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Full backtests on synthetic data: plumbing, determinism and no look-ahead.
//! (Synthetic markets are not evidence of edge.)

use chrono::{Duration, NaiveDate};
use std::sync::Arc;
use wm_backtest::{BacktestConfig, Fidelity, StudyConfig, SyntheticDay, run_backtest, study, synthetic_history, synthetic_trading_day};
use wm_core::event::{EventEnvelope, EventSource, ProviderHealthEvent, WeatherMachineEvent};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::resolution::ObservationFilter;
use wm_core::trading::RunMode;
use wm_core::units::Usd;
use wm_engine::{EngineConfig, EngineLocation};
use wm_execution::SimConfig;
use wm_risk::RiskConfig;
use wm_strategy::{BuyNoConfig, BuyYesConfig, EmpiricalPeakModel, PeakConfig, ProbabilityModel, SplitUnwindConfig, UnwindConfig};

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn loc() -> LocationId {
    LocationId::new("amsterdam").unwrap()
}

fn trained_model() -> EmpiricalPeakModel {
    let hist = synthetic_history(&eham(), NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(), 150, 99, Duration::minutes(5));
    let cfg = StudyConfig { station: eham(), tz: chrono_tz::Europe::Amsterdam, filter: ObservationFilter::AllRows, peak: PeakConfig::default(), k_classes: 4, min_high_local_minute: 9 * 60 };
    study(&hist, &cfg).1
}

fn inputs(days: u32) -> Vec<EventEnvelope> {
    let start = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    let mut events = Vec::new();
    let t0 = wm_core::time::local_day_start(start, chrono_tz::Europe::Amsterdam) - Duration::hours(1);
    let mut h = ProviderHealthSnapshot::new(ProviderId::synthetic(), Some(eham()), t0);
    h.state = ProviderHealthState::Healthy;
    events.push(EventEnvelope::new(t0, EventSource::Replay, WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent { previous_state: None, snapshot: h })));
    for i in 0..days {
        let d = start + Duration::days(i64::from(i));
        let (_, ev) = synthetic_trading_day(&loc(), &eham(), &SyntheticDay::amsterdam(d, 1234), Duration::minutes(4));
        events.extend(ev);
    }
    events
}

fn config() -> BacktestConfig {
    BacktestConfig {
        engine: EngineConfig {
            mode: RunMode::Backtest,
            run_id: RunId::deterministic(5),
            locations: vec![EngineLocation { location: loc(), station: eham(), timezone: chrono_tz::Europe::Amsterdam, peak: PeakConfig::default(), confirmed_filter: Some(ObservationFilter::AllRows) }],
            risk: RiskConfig { max_spread: wm_core::units::Price::parse("0.05").unwrap(), ..RiskConfig::default() },
            buy_yes: BuyYesConfig { min_model_support: 20, ..BuyYesConfig::default() },
            buy_no: BuyNoConfig { min_model_support: 20, ..BuyNoConfig::default() },
            split_unwind: SplitUnwindConfig::default(),
            unwind: UnwindConfig::default(),
            evaluate_on_book_updates: true,
            decision_log_capacity: 1000,
        },
        sim: SimConfig { latency_ms: 500, adverse_ticks: 0 },
        fidelity: Fidelity::Synthetic,
        settle_grace: Duration::hours(2),
        heartbeat: Duration::minutes(15),
    }
}

#[test]
fn synthetic_backtest_runs_end_to_end_and_is_deterministic() {
    let model: Arc<dyn ProbabilityModel> = Arc::new(trained_model());
    let (a, engine) = run_backtest(inputs(20), &config(), model.clone());
    assert_eq!(a.fidelity, Fidelity::Synthetic);
    assert_eq!(a.settled_markets, 20, "every synthetic day settles");
    assert!(a.events > 20 * 48);
    assert!(a.decisions > 0);
    assert!(a.fills <= a.approvals * 2 + 1);
    assert!(a.alerts.iter().all(|x| !x.contains("rejected")), "{:?}", a.alerts);
    // All positions are closed after settlement and accounting is consistent.
    assert_eq!(engine.positions().open_positions().count(), 0);
    let daily_sum: Usd = a.daily_pnl.iter().map(|(_, p)| *p).sum();
    let unwind_pnl: Usd = engine.positions().iter().map(|p| p.realized_pnl).sum::<Usd>() - daily_sum;
    assert!(unwind_pnl.abs() <= Usd::from_whole(1000));
    assert!(a.max_drawdown >= Usd::ZERO);
    // Determinism: identical inputs ⇒ identical report.
    let (b, _) = run_backtest(inputs(20), &config(), model);
    assert_eq!(a, b);
}

#[test]
fn no_look_ahead_decisions_are_prefix_stable() {
    // Decisions taken before time T must not change if all events after T are removed.
    let model: Arc<dyn ProbabilityModel> = Arc::new(trained_model());
    let all = inputs(6);
    let cut = wm_core::time::local_day_start(NaiveDate::from_ymd_opt(2026, 6, 4).unwrap(), chrono_tz::Europe::Amsterdam) + Duration::hours(15);
    let prefix: Vec<EventEnvelope> = all.iter().filter(|e| e.available_at < cut).cloned().collect();
    let mut cfg = config();
    cfg.heartbeat = Duration::zero();
    let (full, _) = run_backtest(all, &cfg, model.clone());
    let (prefix_run, _) = run_backtest(prefix, &cfg, model);
    let before = |r: &wm_backtest::BacktestReport| -> Vec<String> {
        r.decision_log.iter().filter(|d| d.at < cut).map(|d| format!("{} {} {} {:?}", d.at, d.summary, d.approved, d.reasons)).collect()
    };
    let a = before(&full);
    assert!(!a.is_empty(), "the prefix contains trading decisions");
    assert_eq!(a, before(&prefix_run));
}
