#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Property: whatever the interleaving of provider-health changes, reports and
//! market data, the kernel never approves a weather-dependent order unless the
//! observation source is Healthy, the latest report is fresh, and the local
//! day's series is complete. (Blueprint §36: "a provider outage cannot
//! accidentally trigger a trade".)

use chrono::{DateTime, Duration, NaiveDate, Utc};
use proptest::prelude::*;
use std::sync::Arc;
use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OrderBookEvent,
    ProviderHealthEvent, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, RunId, StationId};
use wm_core::market::DailyTemperatureMarket;
use wm_core::resolution::ObservationFilter;
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::RunMode;
use wm_core::units::TempC;
use wm_core::weather::{
    DedupClass, Observation, ObservationKey, QualityFlags, ReportType, TempPrecision,
};
use wm_engine::{Engine, EngineConfig, EngineLocation};
use wm_risk::RiskConfig;
use wm_strategy::{
    BuyNoConfig, BuyYesConfig, IncrementDistribution, PeakConfig, PeakFeatures, ProbabilityModel,
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

/// Always extremely confident: the strategies want to trade whenever allowed.
struct Confident;

impl ProbabilityModel for Confident {
    fn id(&self) -> &str {
        "confident-test-model"
    }
    fn distribution(&self, _f: &PeakFeatures) -> Option<IncrementDistribution> {
        Some(IncrementDistribution {
            probs: vec![0.995, 0.004, 0.0005, 0.0005],
            support: 10_000,
            source: "test".into(),
        })
    }
}

fn config() -> EngineConfig {
    EngineConfig {
        mode: RunMode::Paper,
        run_id: RunId::deterministic(77),
        locations: vec![EngineLocation {
            location: loc(),
            station: eham(),
            timezone: chrono_tz::Europe::Amsterdam,
            peak: PeakConfig::default(),
            confirmed_filter: Some(ObservationFilter::AllRows),
        }],
        risk: RiskConfig::default(),
        buy_yes: BuyYesConfig {
            min_confirmation_minutes: 30,
            ..BuyYesConfig::default()
        },
        buy_no: BuyNoConfig {
            min_confirmation_minutes: 30,
            ..BuyNoConfig::default()
        },
        split_unwind: SplitUnwindConfig::default(),
        unwind: UnwindConfig::default(),
        evaluate_on_book_updates: true,
        decision_log_capacity: 1000,
        rejection_dedup_secs: 0,
    }
}

fn market() -> DailyTemperatureMarket {
    let mut m = synthetic_temperature_market(
        &loc(),
        &eham(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        chrono_tz::Europe::Amsterdam,
        10,
        24,
        utc("2026-06-30T22:00:00Z"),
    );
    m.resolution.filters = vec![ObservationFilter::AllRows];
    m
}

fn observation(observed_at: DateTime<Utc>, whole: i32, delay_min: i64) -> Observation {
    Observation {
        key: ObservationKey {
            station: eham(),
            observed_at,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: None,
        precision: TempPrecision::WholeDegree,
        raw_text: format!(
            "EHAM {} 24010KT 9999 {whole:02}/10 Q1015",
            observed_at.format("%d%H%MZ")
        ),
        content_hash: observed_at.to_rfc3339(),
        provider: ProviderId::awc(),
        provider_receipt_at: None,
        fetched_at: observed_at + Duration::minutes(delay_min),
        parser_version: 1,
        quality: QualityFlags::default(),
    }
}

#[derive(Debug, Clone)]
enum Step {
    /// Deliver the next half-hourly report (temperature delta, publication delay).
    Report {
        delta: i32,
        delay_min: i64,
        skip: bool,
    },
    Health(ProviderHealthState),
    /// Re-quote every book `secs` after the previous step.
    Books {
        secs: i64,
    },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        5 => (-2i32..=2, 1i64..=60, prop::bool::weighted(0.15)).prop_map(|(delta, delay_min, skip)| Step::Report { delta, delay_min, skip }),
        2 => prop_oneof![
            Just(ProviderHealthState::Healthy),
            Just(ProviderHealthState::Degraded),
            Just(ProviderHealthState::Throttled),
            Just(ProviderHealthState::Stale),
            Just(ProviderHealthState::Unavailable),
            Just(ProviderHealthState::Standby),
        ].prop_map(Step::Health),
        3 => (1i64..600).prop_map(|secs| Step::Books { secs }),
    ]
}

/// Replay `steps` through the kernel, checking every output; returns the number of approvals.
fn simulate(start_healthy: bool, steps: Vec<Step>) -> Result<usize, TestCaseError> {
    let m = market();
    let mut engine = Engine::new(config(), Arc::new(Confident));
    let day_start = utc("2026-06-30T22:00:00Z");
    let mut now = day_start + Duration::minutes(1);
    let mut health = if start_healthy {
        ProviderHealthState::Healthy
    } else {
        ProviderHealthState::Unavailable
    };
    let push = |engine: &mut Engine, at: DateTime<Utc>, ev: WeatherMachineEvent| {
        engine.handle(&EventEnvelope::new(at, EventSource::Synthetic, ev))
    };
    let mut approvals = 0usize;

    let mut snapshot = ProviderHealthSnapshot::new(ProviderId::awc(), Some(eham()), now);
    snapshot.state = health;
    push(
        &mut engine,
        now,
        WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
            previous_state: None,
            snapshot,
        }),
    );
    push(
        &mut engine,
        now,
        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent { market: m.clone() }),
    );

    // Reports every 30 min from 22:25Z; `known` = what the engine has seen.
    let mut next_obs = day_start + Duration::minutes(25);
    let mut temp = 12;
    let mut known: Vec<DateTime<Utc>> = Vec::new();
    let mut last_known_obs: Option<DateTime<Utc>> = None;
    for s in steps {
        match s {
            Step::Report {
                delta,
                delay_min,
                skip,
            } => {
                let observed = next_obs;
                next_obs += Duration::minutes(30);
                temp = (temp + delta).clamp(8, 26);
                let avail = (observed + Duration::minutes(delay_min)).max(now);
                now = avail;
                if skip {
                    continue; // the report never arrives: a gap
                }
                known.push(observed);
                last_known_obs =
                    Some(last_known_obs.map_or(observed, |t: DateTime<Utc>| t.max(observed)));
                let out = push(
                    &mut engine,
                    now,
                    WeatherMachineEvent::WeatherObservation(ObservationEvent {
                        observation: observation(observed, temp, delay_min),
                        class: DedupClass::New,
                    }),
                );
                approvals += check(&out, now, health, last_known_obs, &known, day_start)?;
            }
            Step::Health(state) => {
                now += Duration::seconds(1);
                health = state;
                let mut snapshot =
                    ProviderHealthSnapshot::new(ProviderId::awc(), Some(eham()), now);
                snapshot.state = state;
                let out = push(
                    &mut engine,
                    now,
                    WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
                        previous_state: None,
                        snapshot,
                    }),
                );
                approvals += check(&out, now, health, last_known_obs, &known, day_start)?;
            }
            Step::Books { secs } => {
                now += Duration::seconds(secs);
                for o in &m.outcomes {
                    for (token, bid, ask) in [
                        (&o.yes_token, "0.93", "0.95"),
                        (&o.no_token, "0.95", "0.97"),
                    ] {
                        let out = push(
                            &mut engine,
                            now,
                            WeatherMachineEvent::OrderBookUpdate(OrderBookEvent {
                                book: synthetic_book(token, Some(bid), Some(ask), 500, now),
                            }),
                        );
                        approvals += check(&out, now, health, last_known_obs, &known, day_start)?;
                    }
                }
            }
        }
    }
    Ok(approvals)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, .. ProptestConfig::default() })]

    #[test]
    fn no_approval_without_healthy_fresh_complete_data(start_healthy in any::<bool>(), steps in prop::collection::vec(step(), 10..80)) {
        simulate(start_healthy, steps)?;
    }
}

#[test]
fn harness_produces_approvals_with_good_data() {
    // The property above is only meaningful if the same harness does trade
    // when every condition holds: a full, fresh, healthy day that peaks and cools.
    let mut steps = Vec::new();
    let deltas = [
        0, 0, 0, 0, -1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, -1, -1, -1, -1,
    ];
    for d in deltas {
        steps.push(Step::Report {
            delta: d,
            delay_min: 3,
            skip: false,
        });
        steps.push(Step::Books { secs: 30 });
    }
    let approvals = simulate(true, steps).expect("no invariant violation");
    assert!(
        approvals > 0,
        "harness never approves: the property would be vacuous"
    );
}

/// Every approval must coincide with healthy, fresh (≤ 40 min) and complete (no gap > 75 min) data.
fn check(
    out: &wm_engine::EngineOutput,
    now: DateTime<Utc>,
    health: ProviderHealthState,
    last_obs: Option<DateTime<Utc>>,
    known: &[DateTime<Utc>],
    day_start: DateTime<Utc>,
) -> Result<usize, TestCaseError> {
    for a in &out.approved {
        prop_assert!(a.intent().weather_dependent);
        prop_assert_eq!(
            health,
            ProviderHealthState::Healthy,
            "approved while provider {:?}",
            health
        );
        let last = last_obs.expect("approval without any observation");
        prop_assert!(
            now - last <= Duration::minutes(40),
            "approved with a {} min old report",
            (now - last).num_minutes()
        );
        let mut sorted: Vec<DateTime<Utc>> = known
            .iter()
            .copied()
            .filter(|t| *t >= day_start && *t <= now)
            .collect();
        sorted.sort();
        let mut prev = day_start;
        for t in sorted {
            prop_assert!(
                t - prev <= Duration::minutes(75),
                "approved despite a {} s gap in the day series",
                (t - prev).num_seconds()
            );
            prev = t;
        }
    }
    Ok(out.approved.len())
}
