//! The station collector: exactly one polling loop per station.
//!
//! ```text
//! provider(s) → gate → parse → normalize → dedup ledger → persist → events
//! ```
//!
//! Strategies never trigger requests. They receive `WeatherObservation` /
//! `WeatherCorrection` / `ProviderHealthChanged` events, and the engine may
//! publish [`PollingHints`] which the collector's [`PollingPolicy`] turns into a
//! (bounded, gate-respecting) schedule.
//!
//! The schedule belongs to the active source (the primary unless it is
//! unusable). While a window poll finds the expected report missing, a
//! standby source is asked as well when its own gate admits a request, so
//! whichever source publishes first delivers the report.

use crate::health::{HealthConfig, HealthTracker};
use crate::ledger::ObservationLedger;
use crate::polling::{PollDecision, PollingHints, PollingInputs, PollingPolicy};
use crate::registry::CollectorClaim;
use crate::source::{ObservationSource, SourceError, normalize};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use wm_core::event::{
    CorrectionEvent, EventEnvelope, EventSource, ObservationEvent, ProviderHealthEvent,
    WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{LocationId, ProviderId, StationId};
use wm_core::ingest::{IngestBatch, IngestSink};
use wm_core::time::Clock;
use wm_core::weather::{DedupClass, Observation};
use wm_net::FetchError;

/// Collector configuration.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub station: StationId,
    pub location: LocationId,
    pub timezone: Tz,
    pub policy: PollingPolicy,
    pub health: HealthConfig,
    /// Longest wait for a gate inside one poll; zero means "reschedule instead".
    pub max_gate_wait: Duration,
}

/// Live status published for the dashboard/engine (no external requests needed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectorStatus {
    pub station: StationId,
    pub location: LocationId,
    pub providers: Vec<ProviderHealthSnapshot>,
    pub active_provider: Option<ProviderId>,
    pub next_poll: Option<PollDecision>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub polls_total: u64,
    pub gate_closed_total: u64,
    pub new_observations_total: u64,
    pub duplicates_total: u64,
    pub corrections_total: u64,
    pub out_of_order_total: u64,
    pub persist_failures_total: u64,
    pub storage_ok: bool,
    pub last_observation: Option<Observation>,
    pub recent_observations: Vec<Observation>,
    pub hints: PollingHints,
}

/// What one poll did.
#[derive(Debug, Clone, PartialEq)]
pub enum PollOutcome {
    /// Request made; counts of what it produced.
    Fetched {
        provider: ProviderId,
        new: usize,
        out_of_order: usize,
        duplicates: usize,
        corrections: usize,
    },
    /// The provider gate refused (rate limit / backoff / circuit). No request made.
    GateClosed {
        provider: ProviderId,
        retry_in: Duration,
    },
    /// Request failed (network, HTTP status, malformed payload).
    Failed {
        provider: ProviderId,
        detail: String,
        throttled: bool,
    },
}

struct Slot {
    source: Arc<dyn ObservationSource>,
    health: HealthTracker,
}

/// Why a source is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// The poll the schedule planned, made to the active source.
    Scheduled,
    /// A standby asked right after a window poll found the report missing.
    /// It never moves the schedule and never waits for its gate.
    Standby,
}

/// The per-station collector.
pub struct StationCollector {
    cfg: CollectorConfig,
    slots: Vec<Slot>,
    ledger: ObservationLedger,
    sink: Arc<dyn IngestSink>,
    events: mpsc::Sender<EventEnvelope>,
    hints: watch::Receiver<PollingHints>,
    hints_open: bool,
    status_tx: watch::Sender<CollectorStatus>,
    clock: Arc<dyn Clock>,
    last_poll_at: Option<DateTime<Utc>>,
    polls_in_window: u32,
    current_expected: Option<DateTime<Utc>>,
    status: CollectorStatus,
    _claim: CollectorClaim,
}

const RECENT_KEEP: usize = 96;

impl StationCollector {
    /// `sources[0]` is primary; later entries are failovers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        claim: CollectorClaim,
        cfg: CollectorConfig,
        sources: Vec<Arc<dyn ObservationSource>>,
        sink: Arc<dyn IngestSink>,
        events: mpsc::Sender<EventEnvelope>,
        hints: watch::Receiver<PollingHints>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now();
        let slots: Vec<Slot> = sources
            .into_iter()
            .map(|s| Slot {
                health: HealthTracker::new(
                    s.provider().clone(),
                    cfg.station.clone(),
                    cfg.health.clone(),
                    now,
                ),
                source: s,
            })
            .collect();
        let status = CollectorStatus {
            station: cfg.station.clone(),
            location: cfg.location.clone(),
            providers: slots.iter().map(|s| s.health.snapshot().clone()).collect(),
            active_provider: slots.first().map(|s| s.source.provider().clone()),
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
            hints: *hints.borrow(),
        };
        let (status_tx, _) = watch::channel(status.clone());
        Self {
            cfg,
            slots,
            ledger: ObservationLedger::default(),
            sink,
            events,
            hints,
            hints_open: true,
            status_tx,
            clock,
            last_poll_at: None,
            polls_in_window: 0,
            current_expected: None,
            status,
            _claim: claim,
        }
    }

    /// Seed the dedup ledger from persisted observations (restart recovery).
    pub fn warm_start(&mut self, observations: Vec<Observation>) {
        let mut sorted = observations;
        sorted.sort_by_key(|o| (o.key.observed_at, o.version));
        self.status.recent_observations = sorted
            .iter()
            .rev()
            .take(RECENT_KEEP)
            .rev()
            .cloned()
            .collect();
        self.status.last_observation = sorted.last().cloned();
        self.ledger.warm_start(sorted);
    }

    /// Subscribe to live status updates.
    pub fn status(&self) -> watch::Receiver<CollectorStatus> {
        self.status_tx.subscribe()
    }

    pub fn station(&self) -> &StationId {
        &self.cfg.station
    }

    fn usable(&self, s: &Slot) -> bool {
        !matches!(
            s.health.state(),
            ProviderHealthState::Throttled | ProviderHealthState::Unavailable
        ) && s
            .source
            .gate()
            .not_before_utc()
            .is_none_or(|t| t <= self.clock.now() + chrono::Duration::minutes(5))
    }

    fn active_index(&self) -> usize {
        if self.slots.is_empty() {
            return 0;
        }
        if self.usable(&self.slots[0]) {
            return 0;
        }
        self.slots.iter().position(|s| self.usable(s)).unwrap_or(0)
    }

    /// Is the report expected at `expected` still missing?
    fn awaiting(&self, expected: Option<DateTime<Utc>>) -> bool {
        expected.is_some_and(|e| {
            self.ledger
                .newest_observation_time(&self.cfg.station)
                .is_none_or(|t| t < e)
        })
    }

    /// Compute the next poll decision.
    pub fn plan(&mut self) -> Option<PollDecision> {
        let idx = self.active_index();
        let slot = self.slots.get(idx)?;
        let gate = slot.source.gate();
        let inputs = PollingInputs {
            now: self.clock.now(),
            tz: self.cfg.timezone,
            last_observation_time: self.ledger.newest_observation_time(&self.cfg.station),
            last_poll_at: self.last_poll_at,
            polls_in_current_window: self.polls_in_window,
            hints: *self.hints.borrow(),
            health: slot.health.state(),
            gate_not_before: gate.not_before_utc(),
            throttled_recently: gate.stats().politeness_multiplier > 1,
        };
        let d = self.cfg.policy.next_poll(&inputs);
        if d.expected_report != self.current_expected {
            self.current_expected = d.expected_report;
            self.polls_in_window = 0;
        }
        Some(d)
    }

    async fn emit(&self, available_at: DateTime<Utc>, event: WeatherMachineEvent) {
        let env = EventEnvelope::new(available_at, EventSource::Live, event);
        if self.events.send(env).await.is_err() {
            tracing::debug!(station = %self.cfg.station, "event receiver dropped");
        }
    }

    fn remember(&mut self, obs: &Observation) {
        if self
            .status
            .last_observation
            .as_ref()
            .is_none_or(|l| obs.key.observed_at >= l.key.observed_at)
        {
            self.status.last_observation = Some(obs.clone());
        }
        self.status.recent_observations.retain(|o| o.key != obs.key);
        self.status.recent_observations.push(obs.clone());
        self.status
            .recent_observations
            .sort_by_key(|o| o.key.observed_at);
        let excess = self
            .status
            .recent_observations
            .len()
            .saturating_sub(RECENT_KEEP);
        self.status.recent_observations.drain(..excess);
    }

    async fn persist(&mut self, batch: IngestBatch) {
        match self.sink.persist(batch).await {
            Ok(()) => self.status.storage_ok = true,
            Err(e) => {
                self.status.persist_failures_total += 1;
                self.status.storage_ok = false;
                metrics::counter!("wm_persist_failures_total", "station" => self.cfg.station.to_string()).increment(1);
                tracing::error!(station = %self.cfg.station, error = %e, "failed to persist ingest batch");
            }
        }
    }

    /// Perform exactly one poll (used by `run` and by `collect --once`).
    pub async fn poll_once(&mut self) -> PollOutcome {
        let idx = self.active_index();
        self.poll_slot(idx, Ask::Scheduled).await
    }

    /// Ask a standby source now, if its gate admits a request without
    /// waiting; `None` if none can be asked. A failing standby is retried at
    /// its gate's backoff and circuit-breaker pace, so it can recover; a
    /// throttled one is left alone.
    pub async fn poll_standby(&mut self) -> Option<PollOutcome> {
        let active = self.active_index();
        let idx = (0..self.slots.len()).find(|&i| {
            let s = &self.slots[i];
            i != active
                && s.health.state() != ProviderHealthState::Throttled
                && s.source.gate().not_before_utc().is_none()
        })?;
        Some(self.poll_slot(idx, Ask::Standby).await)
    }

    /// Count a request that was made; only scheduled polls move the schedule.
    fn count_request(&mut self, at: DateTime<Utc>, ask: Ask) {
        self.status.polls_total += 1;
        if ask == Ask::Scheduled {
            self.last_poll_at = Some(at);
            self.status.last_poll_at = Some(at);
        }
    }

    async fn poll_slot(&mut self, idx: usize, ask: Ask) -> PollOutcome {
        let failover = ask == Ask::Scheduled && idx != 0;
        let Some(slot) = self.slots.get(idx) else {
            return PollOutcome::Failed {
                provider: ProviderId::replay(),
                detail: "no sources configured".into(),
                throttled: false,
            };
        };
        let source = Arc::clone(&slot.source);
        let provider = source.provider().clone();
        let max_gate_wait = match ask {
            Ask::Scheduled => {
                self.status.active_provider = Some(provider.clone());
                self.cfg.max_gate_wait
            }
            Ask::Standby => Duration::ZERO,
        };
        let result = source.fetch(&self.cfg.station, max_gate_wait).await;
        let now = self.clock.now();
        let gate = Arc::clone(source.gate());

        let outcome = match result {
            Err(SourceError::Fetch(FetchError::GateClosed(w))) => {
                self.status.gate_closed_total += 1;
                PollOutcome::GateClosed {
                    provider,
                    retry_in: w.retry_in,
                }
            }
            Ok(fetch) => {
                self.count_request(now, ask);
                for w in &fetch.warnings {
                    tracing::warn!(station = %self.cfg.station, %provider, warning = %w, "provider payload warning");
                }
                let mut reports = fetch.reports.clone();
                reports.sort_by_key(|r| r.observed_at);
                let (mut new, mut ooo, mut dup, mut cor) = (0, 0, 0, 0);
                let mut persisted = Vec::new();
                let mut corrections = Vec::new();
                let mut events = Vec::new();
                for report in &reports {
                    let obs = normalize(report, &provider, fetch.request.completed_at, failover);
                    let c = self.ledger.classify(obs);
                    match c.class {
                        DedupClass::Duplicate => dup += 1,
                        DedupClass::New | DedupClass::OutOfOrder => {
                            if c.class == DedupClass::New {
                                new += 1;
                            } else {
                                ooo += 1;
                            }
                            persisted.push((c.observation.clone(), c.class));
                            events.push(WeatherMachineEvent::WeatherObservation(
                                ObservationEvent {
                                    observation: c.observation.clone(),
                                    class: c.class,
                                },
                            ));
                        }
                        DedupClass::Correction | DedupClass::Revision => {
                            cor += 1;
                            persisted.push((c.observation.clone(), c.class));
                            if let Some(prev) = c.previous.clone() {
                                let ce = CorrectionEvent {
                                    previous: prev,
                                    current: c.observation.clone(),
                                    labeled: c.class == DedupClass::Correction,
                                };
                                corrections.push(ce.clone());
                                events.push(WeatherMachineEvent::WeatherCorrection(ce));
                            }
                        }
                    }
                }
                let newest = self.ledger.newest_observation_time(&self.cfg.station);
                let blocked = gate.blocked_until_utc();
                let stats = gate.stats();
                let health = self.slots[idx].health.on_success(
                    now,
                    fetch.request.status.unwrap_or(200),
                    fetch.request.latency_ms,
                    newest,
                    new + ooo,
                    &stats,
                    blocked,
                );
                let prefix = if source.is_noaa() {
                    "nws"
                } else {
                    "weather_data"
                };
                if new > 0 {
                    metrics::counter!(format!("{prefix}_new_observations_total"), "provider" => provider.to_string(), "station" => self.cfg.station.to_string()).increment(new as u64);
                }
                if dup > 0 {
                    metrics::counter!("wm_duplicate_observations_total", "provider" => provider.to_string()).increment(dup as u64);
                }
                self.status.new_observations_total += new as u64;
                self.status.out_of_order_total += ooo as u64;
                self.status.duplicates_total += dup as u64;
                self.status.corrections_total += cor as u64;
                for (o, _) in &persisted {
                    self.remember(o);
                }
                let batch = IngestBatch {
                    request: fetch.request.clone(),
                    raw: Some(fetch.raw.clone()),
                    observations: persisted,
                    corrections,
                    health: health.clone(),
                };
                self.persist(batch).await;
                for e in events {
                    self.emit(fetch.request.completed_at, e).await;
                }
                if let Some(h) = health {
                    self.emit(now, WeatherMachineEvent::ProviderHealthChanged(h))
                        .await;
                }
                PollOutcome::Fetched {
                    provider,
                    new,
                    out_of_order: ooo,
                    duplicates: dup,
                    corrections: cor,
                }
            }
            Err(SourceError::Malformed {
                detail,
                raw,
                request,
            }) => {
                self.count_request(now, ask);
                let stats = gate.stats();
                let health = self.slots[idx].health.on_malformed(
                    now,
                    &detail,
                    &stats,
                    gate.blocked_until_utc(),
                );
                let batch = IngestBatch {
                    request: *request,
                    raw: Some(*raw),
                    observations: Vec::new(),
                    corrections: Vec::new(),
                    health: health.clone(),
                };
                self.persist(batch).await;
                if let Some(h) = health {
                    self.emit(now, WeatherMachineEvent::ProviderHealthChanged(h))
                        .await;
                }
                PollOutcome::Failed {
                    provider,
                    detail,
                    throttled: false,
                }
            }
            Err(SourceError::Fetch(err)) => {
                self.count_request(now, ask);
                let throttled = err.is_throttled();
                let status = err.record().and_then(|r| r.status);
                let stats = gate.stats();
                let detail = err.to_string();
                let health = self.slots[idx].health.on_failure(
                    now,
                    status,
                    throttled,
                    &detail,
                    &stats,
                    gate.blocked_until_utc(),
                );
                if let Some(record) = err.record().cloned() {
                    let batch = IngestBatch {
                        request: record,
                        raw: None,
                        observations: Vec::new(),
                        corrections: Vec::new(),
                        health: health.clone(),
                    };
                    self.persist(batch).await;
                }
                if let Some(h) = health {
                    self.emit(now, WeatherMachineEvent::ProviderHealthChanged(h))
                        .await;
                }
                PollOutcome::Failed {
                    provider,
                    detail,
                    throttled,
                }
            }
        };
        self.tick_health(now).await;
        self.publish();
        outcome
    }

    /// Re-evaluate staleness of every source (health can decay without requests).
    async fn tick_health(&mut self, now: DateTime<Utc>) {
        let newest = self.ledger.newest_observation_time(&self.cfg.station);
        let mut changed: Vec<ProviderHealthEvent> = Vec::new();
        for slot in &mut self.slots {
            let gate = slot.source.gate();
            if let Some(ev) =
                slot.health
                    .on_tick(now, newest, &gate.stats(), gate.blocked_until_utc())
            {
                changed.push(ev);
            }
        }
        for ev in changed {
            self.emit(now, WeatherMachineEvent::ProviderHealthChanged(ev))
                .await;
        }
    }

    fn publish(&mut self) {
        self.status.providers = self
            .slots
            .iter()
            .map(|s| {
                let mut snap = s.health.snapshot().clone();
                let st = s.source.gate().stats();
                snap.requests_today = st.requests_today;
                snap.daily_budget = s.source.gate().policy().daily_budget;
                snap
            })
            .collect();
        self.status.hints = *self.hints.borrow();
        self.status_tx.send_replace(self.status.clone());
    }

    /// Run until `shutdown` becomes `true`.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        tracing::info!(station = %self.cfg.station, sources = self.slots.len(), "station collector started");
        loop {
            if *shutdown.borrow() {
                break;
            }
            let Some(decision) = self.plan() else {
                tracing::error!(station = %self.cfg.station, "collector has no sources; stopping");
                break;
            };
            self.status.next_poll = Some(decision);
            self.publish();
            let wait = (decision.at - self.clock.now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            let hints_open = self.hints_open;
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                r = shutdown.changed() => {
                    if r.is_err() || *shutdown.borrow() { break; }
                    continue;
                }
                r = self.hints.changed(), if hints_open => {
                    if r.is_err() { self.hints_open = false; }
                    continue;
                }
            }
            if decision.in_window {
                self.polls_in_window += 1;
            }
            let outcome = self.poll_once().await;
            tracing::debug!(station = %self.cfg.station, ?outcome, reason = ?decision.reason, mode = ?decision.mode, "poll complete");
            if decision.in_window
                && self.cfg.policy.params.poll_standby_in_window
                && self.awaiting(decision.expected_report)
                && let Some(standby) = self.poll_standby().await
            {
                tracing::debug!(station = %self.cfg.station, outcome = ?standby, "standby poll complete");
            }
            if let PollOutcome::GateClosed { retry_in, .. } = outcome {
                // Never spin: honour the gate's own schedule.
                tokio::time::sleep(retry_in.max(Duration::from_secs(1))).await;
            }
        }
        tracing::info!(station = %self.cfg.station, "station collector stopped");
    }
}
