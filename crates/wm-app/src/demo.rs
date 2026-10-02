//! `weather-machine demo`: the real kernel, risk engine and simulated venue
//! driven by **synthetic** data in accelerated time, so the dashboard can be
//! evaluated end to end without network access or a database.
//!
//! Every day contains, on purpose:
//! * a METAR correction (COR) in the morning → correction cooldown;
//! * a provider throttling episode (HTTP 429) in the early afternoon: the
//!   provider turns `Throttled`, reports arrive late, and every new
//!   weather-dependent position is rejected until recovery (fail closed);
//! * settlement after the local day ends, then the next day starts.
//!
//! Nothing produced here is evidence of edge; the UI shows a DEMO banner.

use crate::config::AppConfig;
use crate::dto::{self, DtoInputs};
use crate::http::Publisher;
use crate::setup;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, watch};
use wm_backtest::{
    SimulationSession, StudyConfig, SyntheticDay, study, synthetic_history, synthetic_trading_day,
};
use wm_core::event::{
    CorrectionEvent, EventEnvelope, EventSource, ForecastEvent, NowcastEvent, OperatorCommand,
    ProviderHealthEvent, StreamHeartbeatEvent, TimerEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::health::{CircuitState, ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{ProviderId, RunId};
use wm_core::resolution::ObservationFilter;
use wm_core::time::{local_date, local_day_start};
use wm_core::trading::RunMode;
use wm_core::weather::ReportType;
use wm_dashboard_api::{AlertDto, ModelDto};
use wm_execution::SimConfig;
use wm_strategy::ProbabilityModel;
use wm_weather::polling::{PollDecision, PollReason, PollingMode};
use wm_weather::{CadenceModel, CollectorStatus, PollingHints};

/// Demo options.
#[derive(Debug, Clone)]
pub struct DemoOptions {
    /// Virtual seconds per wall-clock second.
    pub speed: f64,
    pub seed: u64,
    /// Local time-of-day (minutes) the virtual clock starts at.
    pub start_local_minute: u32,
    /// Synthetic history used to train the demo model.
    pub train_days: u32,
    pub snapshot_interval: std::time::Duration,
}

impl Default for DemoOptions {
    fn default() -> Self {
        Self {
            speed: 60.0,
            seed: 7,
            start_local_minute: 8 * 60 + 30,
            train_days: 240,
            snapshot_interval: std::time::Duration::from_millis(500),
        }
    }
}

const PUBLICATION_DELAY_MIN: i64 = 4;
/// Minutes from the end of a ten-minute interval to its synthetic reading.
const TEN_MINUTE_DELAY_MIN: i64 = 5;
const HEARTBEAT_MIN: i64 = 5;

fn health_event(
    at: DateTime<Utc>,
    station: &wm_core::ids::StationId,
    state: ProviderHealthState,
    reason: &str,
    status: Option<u16>,
    blocked_until: Option<DateTime<Utc>>,
    throttles: u64,
) -> EventEnvelope {
    let mut s = ProviderHealthSnapshot::new(ProviderId::synthetic(), Some(station.clone()), at);
    s.state = state;
    s.reason = reason.to_owned();
    s.last_http_status = status;
    s.last_request_at = Some(at);
    s.blocked_until = blocked_until;
    s.throttle_events = throttles;
    s.circuit = if state == ProviderHealthState::Throttled {
        CircuitState::Open
    } else {
        CircuitState::Closed
    };
    if state == ProviderHealthState::Healthy {
        s.last_success_at = Some(at);
        s.latency_ms_last = Some(180);
        s.latency_ms_ewma = Some(185.0);
    }
    EventEnvelope::new(
        at,
        EventSource::Synthetic,
        WeatherMachineEvent::ProviderHealthChanged(ProviderHealthEvent {
            previous_state: None,
            snapshot: s,
        }),
    )
}

/// All events of one synthetic day (market, observations, books, health
/// episode, correction), in knowledge time.
pub fn day_events(
    ids: &setup::LocationIds,
    date: NaiveDate,
    seed: u64,
    day_index: u64,
) -> Vec<EventEnvelope> {
    let day = SyntheticDay {
        tz: ids.timezone,
        ..SyntheticDay::amsterdam(date, seed.wrapping_add(day_index))
    };
    let (_, mut events) = synthetic_trading_day(
        &ids.location,
        &ids.station,
        &day,
        Duration::minutes(PUBLICATION_DELAY_MIN),
    );
    let start = local_day_start(date, ids.timezone);
    let throttle_from = start + Duration::minutes(12 * 60 + 40);
    let throttle_to = start + Duration::minutes(13 * 60 + 20);
    let recovered_at = throttle_to + Duration::seconds(30);

    // Reports published during the throttling episode become known only after recovery.
    let mut correction: Option<EventEnvelope> = None;
    for e in &mut events {
        if let WeatherMachineEvent::WeatherObservation(o) = &mut e.event {
            if e.available_at >= throttle_from && e.available_at < throttle_to {
                e.available_at = recovered_at;
                e.recorded_at = recovered_at;
                o.observation.fetched_at = recovered_at;
            }
            let local = o.observation.key.observed_at.with_timezone(&ids.timezone);
            if correction.is_none()
                && chrono::Timelike::hour(&local) == 10
                && chrono::Timelike::minute(&local) == 25
            {
                let previous = o.observation.clone();
                let mut current = previous.clone();
                current.version = 2;
                current.key.report_type = ReportType::Metar;
                current.temperature = previous
                    .temperature
                    .map(|t| wm_core::units::TempC::from_tenths(t.tenths() - 10));
                let group = |t: Option<wm_core::units::TempC>| {
                    t.map(|t| t.round_half_up_whole())
                        .map(|w| {
                            if w < 0 {
                                format!(" M{:02}/", -w)
                            } else {
                                format!(" {w:02}/")
                            }
                        })
                        .unwrap_or_default()
                };
                current.raw_text = format!(
                    "METAR COR {}",
                    previous.raw_text.replacen(
                        &group(previous.temperature),
                        &group(current.temperature),
                        1
                    )
                );
                current.content_hash = wm_core::hash::sha256_hex(current.raw_text.as_bytes());
                current.quality.correction_marker = true;
                let at = previous.fetched_at + Duration::minutes(31);
                current.fetched_at = at;
                correction = Some(EventEnvelope::new(
                    at,
                    EventSource::Synthetic,
                    WeatherMachineEvent::WeatherCorrection(CorrectionEvent {
                        previous,
                        current,
                        labeled: true,
                    }),
                ));
            }
        }
    }
    events.extend(correction);
    // Ten-minute readings of the same curve (strategy K's input, KNMI-style),
    // each with a market-feed heartbeat: live, the heartbeat keeps quiet books
    // current; here it lets the books quoted after the last report count.
    for o in day.ten_minute_readings(&ids.station, Duration::minutes(TEN_MINUTE_DELAY_MIN)) {
        let at = o.received_at;
        events.push(EventEnvelope::new(
            at,
            EventSource::Synthetic,
            WeatherMachineEvent::MarketStreamHeartbeat(StreamHeartbeatEvent {
                connected_since: start,
            }),
        ));
        events.push(EventEnvelope::new(
            at,
            EventSource::Synthetic,
            WeatherMachineEvent::NowcastUpdate(NowcastEvent { observation: o }),
        ));
    }
    // Synthetic day-1 forecast: the day's underlying curve with a day-specific
    // error, known from 08:05 local (like the live product's ready time).
    let bias = (seed.wrapping_add(day_index) % 21) as f64 - 10.0;
    let hourly = (0..=24)
        .map(|h| {
            let t = start + Duration::hours(h);
            let tenths = (day.signal(h as f64) + bias).round() as i32;
            (t, wm_core::units::TempC::from_tenths(tenths))
        })
        .collect();
    events.push(EventEnvelope::new(
        start + Duration::minutes(8 * 60 + 5),
        EventSource::Synthetic,
        WeatherMachineEvent::ForecastUpdate(ForecastEvent {
            location: ids.location.clone(),
            provider: ProviderId::synthetic(),
            model: "demo".into(),
            issued_at: start + Duration::minutes(8 * 60 + 5),
            predicted_max: None,
            hourly,
            lead_days: Some(1),
        }),
    ));
    events.push(health_event(
        start - Duration::minutes(30),
        &ids.station,
        ProviderHealthState::Healthy,
        "synthetic source healthy",
        Some(200),
        None,
        0,
    ));
    events.push(health_event(
        throttle_from,
        &ids.station,
        ProviderHealthState::Throttled,
        "HTTP 429 Too Many Requests — Retry-After 2400 s honoured, polling paused (simulated)",
        Some(429),
        Some(throttle_to),
        1,
    ));
    events.push(health_event(
        recovered_at,
        &ids.station,
        ProviderHealthState::Healthy,
        "recovered after throttling (simulated)",
        Some(200),
        None,
        1,
    ));
    events
}

/// Train the demo model on synthetic history strictly before `before`.
pub fn train_model(
    ids: &setup::LocationIds,
    peak: wm_strategy::PeakConfig,
    before: NaiveDate,
    days: u32,
    seed: u64,
) -> Arc<dyn ProbabilityModel> {
    let from = before - Duration::days(i64::from(days));
    let hist = synthetic_history(
        &ids.station,
        from,
        days,
        seed ^ 0xA11CE,
        Duration::minutes(PUBLICATION_DELAY_MIN),
    );
    let cfg = StudyConfig {
        station: ids.station.clone(),
        tz: ids.timezone,
        filter: ObservationFilter::AllRows,
        peak,
        k_classes: 4,
        min_high_local_minute: 9 * 60,
    };
    let (_, mut model) = study(&hist, &cfg);
    model.id = format!("demo-synthetic-{}", model.id);
    Arc::new(model)
}

/// Collector-style status for the synthetic source, so the dashboard shows
/// the same panels (tape with raw METARs, counters, next report) as live.
struct DemoCollector {
    status: CollectorStatus,
    cadence: CadenceModel,
}

impl DemoCollector {
    fn new(ids: &setup::LocationIds, cadence: CadenceModel) -> Self {
        let status = CollectorStatus {
            station: ids.station.clone(),
            location: ids.location.clone(),
            providers: Vec::new(),
            active_provider: Some(ProviderId::synthetic()),
            next_poll: None,
            last_poll_at: None,
            polls_total: 0,
            gate_closed_total: 0,
            new_observations_total: 0,
            duplicates_total: 0,
            corrections_total: 0,
            out_of_order_total: 0,
            persist_failures_total: 0,
            storage_ok: true,
            last_observation: None,
            recent_observations: Vec::new(),
            hints: PollingHints::default(),
        };
        Self { status, cadence }
    }

    fn remember(&mut self, o: &wm_core::weather::Observation) {
        let s = &mut self.status;
        if s.last_observation
            .as_ref()
            .is_none_or(|l| o.key.observed_at >= l.key.observed_at)
        {
            s.last_observation = Some(o.clone());
        }
        s.recent_observations.retain(|x| x.key != o.key);
        s.recent_observations.push(o.clone());
        s.recent_observations.sort_by_key(|x| x.key.observed_at);
        let excess = s.recent_observations.len().saturating_sub(96);
        s.recent_observations.drain(..excess);
    }

    fn absorb(
        &mut self,
        processed: &[EventEnvelope],
        hints: Option<PollingHints>,
        now: DateTime<Utc>,
    ) {
        for env in processed {
            match &env.event {
                WeatherMachineEvent::WeatherObservation(o) => {
                    self.status.polls_total += 1;
                    self.status.new_observations_total += 1;
                    self.status.last_poll_at = Some(env.available_at);
                    self.remember(&o.observation);
                }
                WeatherMachineEvent::WeatherCorrection(c) => {
                    self.status.corrections_total += 1;
                    self.remember(&c.current);
                }
                WeatherMachineEvent::ProviderHealthChanged(h) => {
                    self.status.providers = vec![h.snapshot.clone()];
                }
                _ => {}
            }
        }
        if let Some(h) = hints {
            self.status.hints = h;
        }
        let delay = Duration::minutes(PUBLICATION_DELAY_MIN);
        let expected = self.cadence.next_report_after(now - delay);
        let peak = self.status.hints.peak_watch || self.status.hints.has_exposure;
        self.status.next_poll = expected.map(|e| PollDecision {
            at: e + delay,
            mode: if peak {
                PollingMode::Peak
            } else {
                PollingMode::Normal
            },
            reason: PollReason::ArrivalWindow,
            in_window: false,
            expected_report: Some(e),
        });
    }
}

struct VirtualClock {
    wall_start: std::time::Instant,
    virtual_start: DateTime<Utc>,
    speed: f64,
}

impl VirtualClock {
    fn now(&self) -> DateTime<Utc> {
        let elapsed = self.wall_start.elapsed().as_secs_f64() * self.speed;
        self.virtual_start + Duration::milliseconds((elapsed * 1000.0) as i64)
    }
}

/// Run the demo until shutdown.
pub async fn run(
    cfg: AppConfig,
    opts: DemoOptions,
    mut publisher: Publisher,
    mut commands: mpsc::Receiver<OperatorCommand>,
    ready: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let loc = cfg
        .locations
        .first()
        .context("demo needs one configured location")?;
    let ids = setup::location_ids(loc)?;
    let today = local_date(Utc::now(), ids.timezone);
    tracing::info!(location = %ids.location, station = %ids.station, speed = opts.speed, "demo: training synthetic model");
    let (ids2, peak, days, seed) = (
        ids.clone(),
        setup::peak_config(loc),
        opts.train_days,
        opts.seed,
    );
    let model = tokio::task::spawn_blocking(move || train_model(&ids2, peak, today, days, seed))
        .await
        .context("model training task")?;
    let model_status = ModelDto {
        state: "loaded".into(),
        detail: format!("synthetic demo model ({days} synthetic days)"),
        progress: None,
        forecast: None,
        structure: None,
        retraining: None,
    };

    let engine_cfg = setup::engine_config(&cfg, RunMode::Paper, RunId::new_v7())?;
    let strategy_catalog = crate::strategies::catalog(&cfg);
    let peak_slot_cfg = cfg.peak_slot();
    let peak_times = model.peak_times().cloned();
    let mut session =
        SimulationSession::new(engine_cfg, SimConfig::default(), Duration::hours(2), model)
            .with_event_capture(true);
    session.engine_mut().set_storage_ok(true);
    let mut collector = DemoCollector::new(&ids, setup::cadence(loc));

    let clock = VirtualClock {
        wall_start: std::time::Instant::now(),
        virtual_start: local_day_start(today, ids.timezone)
            + Duration::minutes(i64::from(opts.start_local_minute)),
        speed: opts.speed.clamp(1.0, 3600.0),
    };
    let mut next_day = today;
    let mut day_index = 0u64;
    let mut next_heartbeat = clock.now();
    let mut alerts: VecDeque<AlertDto> = VecDeque::new();
    let empty_filters = HashMap::new();
    let empty_reviews = HashMap::new();
    let mut last_publish = std::time::Instant::now() - opts.snapshot_interval;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ready.store(true, Ordering::Release);
    tracing::info!(virtual_start = %clock.virtual_start, "demo running");

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            cmd = commands.recv() => {
                if let Some(c) = cmd {
                    session.push(EventEnvelope::new(clock.now().max(session.last_time().unwrap_or(DateTime::<Utc>::MIN_UTC)), EventSource::Operator, WeatherMachineEvent::Operator(c)));
                }
            }
            r = shutdown.changed() => { if r.is_err() || *shutdown.borrow() { break; } }
        }
        let vnow = clock.now();
        // Schedule each day an hour before it starts (events are knowledge-time ordered).
        while vnow >= local_day_start(next_day, ids.timezone) - Duration::hours(1) {
            for e in day_events(&ids, next_day, opts.seed, day_index) {
                session.push(e);
            }
            next_day = next_day.succ_opt().unwrap_or(next_day);
            day_index += 1;
        }
        while next_heartbeat <= vnow {
            session.push(EventEnvelope::new(
                next_heartbeat,
                EventSource::Synthetic,
                WeatherMachineEvent::Timer(TimerEvent {
                    due_at: next_heartbeat,
                    kind: TimerKind::Heartbeat,
                }),
            ));
            next_heartbeat += Duration::minutes(HEARTBEAT_MIN);
        }
        let out = session.run_until(vnow);
        let hint = out
            .hints
            .iter()
            .rev()
            .find(|(st, _)| st == &ids.station)
            .map(|(_, h)| PollingHints {
                peak_watch: h.peak_watch,
                has_exposure: h.has_exposure,
            });
        collector.absorb(&out.processed, hint, vnow);
        metrics::counter!("wm_engine_events_total").increment(out.events);
        metrics::counter!("wm_decisions_total").increment(out.decisions.len() as u64);
        metrics::counter!("wm_orders_approved_total").increment(out.approved.len() as u64);
        for a in out.alerts {
            let level = if a.contains("KILL") {
                "critical"
            } else if a.contains("correction") {
                "warning"
            } else {
                "info"
            };
            alerts.push_back(AlertDto {
                at_ms: vnow.timestamp_millis(),
                level: level.into(),
                message: a,
            });
        }
        for t in &out.trades {
            alerts.push_back(AlertDto {
                at_ms: t.at.timestamp_millis(),
                level: "info".into(),
                message: format!(
                    "fill {} {} {} @ {} × {} ({})",
                    t.strategy, t.side, t.bucket_label, t.price, t.shares, t.event_slug
                ),
            });
        }
        for s in &out.settlements {
            alerts.push_back(AlertDto {
                at_ms: s.at.timestamp_millis(),
                level: "info".into(),
                message: format!(
                    "settled {} at {} °C: PnL {}",
                    s.event_slug, s.final_value, s.pnl
                ),
            });
        }
        while alerts.len() > 60 {
            alerts.pop_front();
        }
        if last_publish.elapsed() >= opts.snapshot_interval {
            last_publish = std::time::Instant::now();
            let snap = session.engine().snapshot();
            metrics::gauge!("wm_global_exposure_usd").set(snap.exposure.global_worst_case.as_f64());
            metrics::gauge!("wm_kill_switch").set(if snap.kill_switch.is_some() {
                1.0
            } else {
                0.0
            });
            let alerts_vec: Vec<AlertDto> = alerts.iter().cloned().collect();
            let collectors = HashMap::from([(ids.station.clone(), collector.status.clone())]);
            let inputs = DtoInputs {
                demo: true,
                instance: &cfg.file.app.instance,
                collectors: &collectors,
                stream: None,
                alerts: &alerts_vec,
                confirmed_filters: &empty_filters,
                extra_providers: &[],
                rules_review: &empty_reviews,
                model: &model_status,
                yes_pooling: cfg.buy_yes().pooling(),
                no_pooling: cfg.buy_no().pooling(),
                strategies: &strategy_catalog,
                peak_slot: &peak_slot_cfg,
                peak_times: peak_times.as_ref(),
            };
            publisher.publish(dto::build(&snap, &inputs, Utc::now()));
        }
    }
    tracing::info!("demo stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> setup::LocationIds {
        setup::LocationIds {
            location: wm_core::ids::LocationId::new("amsterdam").unwrap(),
            station: wm_core::ids::StationId::new("EHAM").unwrap(),
            timezone: chrono_tz::Europe::Amsterdam,
        }
    }

    #[test]
    fn demo_day_contains_throttle_episode_and_correction() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let ev = day_events(&ids(), d, 7, 0);
        let throttled = ev.iter().filter(|e| matches!(&e.event, WeatherMachineEvent::ProviderHealthChanged(h) if h.snapshot.state == ProviderHealthState::Throttled)).count();
        assert_eq!(throttled, 1);
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e.event, WeatherMachineEvent::WeatherCorrection(_)))
                .count(),
            1
        );
        // Nothing becomes known during the outage.
        let start = local_day_start(d, chrono_tz::Europe::Amsterdam);
        let (from, to) = (
            start + Duration::minutes(12 * 60 + 40),
            start + Duration::minutes(13 * 60 + 20),
        );
        assert!(!ev.iter().any(
            |e| matches!(e.event, WeatherMachineEvent::WeatherObservation(_))
                && e.available_at >= from
                && e.available_at < to
        ));
        // A ten-minute reading every ten minutes, each with a market-feed
        // heartbeat at the same instant.
        let at = |heartbeat: bool| {
            ev.iter()
                .filter(|e| match &e.event {
                    WeatherMachineEvent::NowcastUpdate(_) => !heartbeat,
                    WeatherMachineEvent::MarketStreamHeartbeat(_) => heartbeat,
                    _ => false,
                })
                .map(|e| e.available_at)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(false).len(), 24 * 6 - 1);
        assert_eq!(at(false), at(true));
        // Deterministic.
        assert_eq!(ev, day_events(&ids(), d, 7, 0));
    }

    #[test]
    fn demo_session_trades_and_fails_closed_during_throttling() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let loc = crate::config::AppConfig::load(Some(
            &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../configs/weather-machine.toml"),
        ))
        .unwrap();
        let l = &loc.locations[0];
        let model = train_model(&ids(), setup::peak_config(l), d, 120, 7);
        let cfg = setup::engine_config(&loc, RunMode::Paper, RunId::deterministic(9)).unwrap();
        let mut s = SimulationSession::new(cfg, SimConfig::default(), Duration::hours(2), model);
        s.engine_mut().set_storage_ok(true);
        let mut all = Vec::new();
        for i in 0..3u64 {
            for e in day_events(&ids(), d + Duration::days(i as i64), 7, i) {
                s.push(e);
            }
        }
        let out = s.run_all();
        all.extend(out.decisions);
        assert_eq!(out.settlements.len(), 3, "every demo day settles");
        assert!(
            all.iter().any(|x| x.approved),
            "the demo should show the full signal → risk → fill lifecycle"
        );
        assert!(!out.trades.is_empty());
        let start = local_day_start(d, chrono_tz::Europe::Amsterdam);
        let (from, to) = (
            start + Duration::minutes(12 * 60 + 40),
            start + Duration::minutes(13 * 60 + 20),
        );
        // No approval while the provider is throttled.
        assert!(!all.iter().any(|x| x.approved && x.at >= from && x.at < to));
        // The ten-minute readings reach the engine (strategy K's input).
        assert!(s.engine().nowcast(&ids().station).is_some());
        // Every position on the dashboard names the strategy that opened it.
        let catalog = crate::strategies::catalog(&loc);
        let (collectors, filters, reviews) = (HashMap::new(), HashMap::new(), HashMap::new());
        let model_status = ModelDto::default();
        let peak_slot = loc.peak_slot();
        let inputs = DtoInputs {
            demo: true,
            instance: "test",
            collectors: &collectors,
            stream: None,
            alerts: &[],
            confirmed_filters: &filters,
            extra_providers: &[],
            rules_review: &reviews,
            model: &model_status,
            yes_pooling: loc.buy_yes().pooling(),
            no_pooling: loc.buy_no().pooling(),
            strategies: &catalog,
            peak_slot: &peak_slot,
            peak_times: None,
        };
        let dash = dto::build(&s.engine().snapshot(), &inputs, Utc::now());
        assert!(!dash.positions.is_empty(), "the demo trades");
        for p in &dash.positions {
            assert!(
                catalog.iter().any(|c| c.id == p.opened_by),
                "{} {} opened by {:?}",
                p.bucket,
                p.side,
                p.opened_by
            );
        }
    }
}
