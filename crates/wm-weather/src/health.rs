//! Provider health tracking (Healthy / Degraded / Throttled / Stale / Unavailable /
//! Standby).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use wm_core::event::ProviderHealthEvent;
use wm_core::health::{CircuitState, ProviderHealthSnapshot, ProviderHealthState};
use wm_core::ids::{ProviderId, StationId};
use wm_net::GateStats;

/// Thresholds for health classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthConfig {
    /// No observation newer than this ⇒ `Stale` (30-min cadence + 15-min allowance for EHAM).
    /// A source whose own last success is older than this is `Standby`: the
    /// active source is polled at least every 20 minutes, so only an idle
    /// fallback reaches it.
    pub stale_after_secs: i64,
    /// Latency EWMA *and* the latest request above this ⇒ `Degraded`.
    pub degraded_latency_ms: u64,
    /// Consecutive failures that make the provider `Unavailable`.
    pub unavailable_after_failures: u32,
    /// A throttle event keeps the provider `Throttled` at least this long.
    pub throttle_memory_secs: i64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            stale_after_secs: 45 * 60,
            degraded_latency_ms: 5_000,
            unavailable_after_failures: 3,
            throttle_memory_secs: 30 * 60,
        }
    }
}

/// Tracks the health of one provider for one station.
#[derive(Debug, Clone)]
pub struct HealthTracker {
    cfg: HealthConfig,
    snapshot: ProviderHealthSnapshot,
    parse_failures: u32,
    last_throttle_at: Option<DateTime<Utc>>,
}

impl HealthTracker {
    pub fn new(
        provider: ProviderId,
        station: StationId,
        cfg: HealthConfig,
        now: DateTime<Utc>,
    ) -> Self {
        let mut snapshot = ProviderHealthSnapshot::new(provider, Some(station), now);
        snapshot.state = ProviderHealthState::Standby;
        snapshot.reason = "standby: not contacted yet".into();
        Self {
            cfg,
            snapshot,
            parse_failures: 0,
            last_throttle_at: None,
        }
    }

    pub fn snapshot(&self) -> &ProviderHealthSnapshot {
        &self.snapshot
    }

    pub fn state(&self) -> ProviderHealthState {
        self.snapshot.state
    }

    fn apply_gate(&mut self, gate: &GateStats, blocked_until: Option<DateTime<Utc>>) {
        self.snapshot.requests_total = gate.requests_total;
        self.snapshot.requests_today = gate.requests_today;
        self.snapshot.circuit = gate.circuit;
        self.snapshot.current_backoff_ms = gate.current_backoff.as_millis() as u64;
        self.snapshot.blocked_until = blocked_until;
        self.snapshot.throttle_events = gate.throttled_total;
    }

    /// Record a successful HTTP exchange that produced parseable data.
    #[allow(clippy::too_many_arguments)]
    pub fn on_success(
        &mut self,
        now: DateTime<Utc>,
        status: u16,
        latency_ms: u64,
        newest_observation_time: Option<DateTime<Utc>>,
        new_observations: usize,
        gate: &GateStats,
        blocked_until: Option<DateTime<Utc>>,
    ) -> Option<ProviderHealthEvent> {
        self.snapshot.last_request_at = Some(now);
        self.snapshot.last_success_at = Some(now);
        self.snapshot.last_http_status = Some(status);
        self.snapshot.last_error = None;
        self.snapshot.consecutive_failures = 0;
        self.parse_failures = 0;
        self.snapshot.latency_ms_last = Some(latency_ms);
        self.snapshot.latency_ms_ewma = Some(match self.snapshot.latency_ms_ewma {
            Some(prev) => 0.8 * prev + 0.2 * latency_ms as f64,
            None => latency_ms as f64,
        });
        if new_observations > 0 {
            self.snapshot.last_new_observation_at = Some(now);
        }
        if newest_observation_time.is_some() {
            self.snapshot.last_observation_time = newest_observation_time;
        }
        self.apply_gate(gate, blocked_until);
        self.update(now)
    }

    /// HTTP succeeded but the payload was unusable.
    pub fn on_malformed(
        &mut self,
        now: DateTime<Utc>,
        detail: &str,
        gate: &GateStats,
        blocked_until: Option<DateTime<Utc>>,
    ) -> Option<ProviderHealthEvent> {
        self.snapshot.last_request_at = Some(now);
        self.snapshot.last_error = Some(format!("malformed: {detail}"));
        self.snapshot.consecutive_failures += 1;
        self.parse_failures += 1;
        self.apply_gate(gate, blocked_until);
        self.update(now)
    }

    /// Network/HTTP failure (including throttling).
    pub fn on_failure(
        &mut self,
        now: DateTime<Utc>,
        status: Option<u16>,
        throttled: bool,
        detail: &str,
        gate: &GateStats,
        blocked_until: Option<DateTime<Utc>>,
    ) -> Option<ProviderHealthEvent> {
        self.snapshot.last_request_at = Some(now);
        self.snapshot.last_http_status = status.or(self.snapshot.last_http_status);
        self.snapshot.last_error = Some(detail.to_owned());
        self.snapshot.consecutive_failures += 1;
        if throttled {
            self.last_throttle_at = Some(now);
        }
        self.apply_gate(gate, blocked_until);
        self.update(now)
    }

    /// Periodic re-evaluation (staleness can develop without any request).
    pub fn on_tick(
        &mut self,
        now: DateTime<Utc>,
        newest_observation_time: Option<DateTime<Utc>>,
        gate: &GateStats,
        blocked_until: Option<DateTime<Utc>>,
    ) -> Option<ProviderHealthEvent> {
        if newest_observation_time.is_some() {
            self.snapshot.last_observation_time = newest_observation_time;
        }
        self.apply_gate(gate, blocked_until);
        self.update(now)
    }

    fn evaluate(&self, now: DateTime<Utc>) -> (ProviderHealthState, String) {
        let s = &self.snapshot;
        if s.circuit != CircuitState::Closed {
            return (
                ProviderHealthState::Unavailable,
                format!("circuit {:?}", s.circuit).to_lowercase(),
            );
        }
        if s.consecutive_failures >= self.cfg.unavailable_after_failures {
            return (
                ProviderHealthState::Unavailable,
                format!("{} consecutive failures", s.consecutive_failures),
            );
        }
        if let Some(t) = self.last_throttle_at
            && now - t < Duration::seconds(self.cfg.throttle_memory_secs)
        {
            return (
                ProviderHealthState::Throttled,
                "throttled by provider; backing off".into(),
            );
        }
        match s.last_observation_time {
            None => return (ProviderHealthState::Stale, "no observations yet".into()),
            Some(t) if now - t > Duration::seconds(self.cfg.stale_after_secs) => {
                return (
                    ProviderHealthState::Stale,
                    format!("newest observation is {} min old", (now - t).num_minutes()),
                );
            }
            _ => {}
        }
        if s.consecutive_failures > 0 || self.parse_failures > 0 {
            return (
                ProviderHealthState::Degraded,
                "recent request failures".into(),
            );
        }
        // Health is evidence from this source's own requests. Fresh data from
        // another source says nothing about a fallback that was never asked.
        match s.last_success_at {
            None => {
                return (
                    ProviderHealthState::Standby,
                    "standby: not contacted yet".into(),
                );
            }
            Some(t) if now - t > Duration::seconds(self.cfg.stale_after_secs) => {
                return (
                    ProviderHealthState::Standby,
                    format!("standby: last success {} min ago", (now - t).num_minutes()),
                );
            }
            _ => {}
        }
        // Slow means slow on average *and* now. One slow response (say the
        // first after a restart) weighs on the average for many polls, and
        // would keep the source degraded, and every trade blocked, that long.
        let limit = self.cfg.degraded_latency_ms as f64;
        if let (Some(avg), Some(last)) = (s.latency_ms_ewma, s.latency_ms_last)
            && avg > limit
            && last as f64 > limit
        {
            return (
                ProviderHealthState::Degraded,
                format!("high latency: last {last} ms, average {avg:.0} ms"),
            );
        }
        (ProviderHealthState::Healthy, "ok".into())
    }

    fn update(&mut self, now: DateTime<Utc>) -> Option<ProviderHealthEvent> {
        let (state, reason) = self.evaluate(now);
        let previous = self.snapshot.state;
        self.snapshot.reason = reason;
        self.snapshot.updated_at = now;
        if state != previous {
            self.snapshot.state = state;
            metrics::counter!("wm_provider_health_changes_total", "provider" => self.snapshot.provider.to_string(), "state" => state.as_str()).increment(1);
            Some(ProviderHealthEvent {
                previous_state: Some(previous),
                snapshot: self.snapshot.clone(),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> GateStats {
        GateStats {
            requests_total: 1,
            successes_total: 1,
            failures_total: 0,
            throttled_total: 0,
            requests_today: 1,
            requests_last_hour: 1,
            consecutive_failures: 0,
            circuit: CircuitState::Closed,
            circuit_opens: 0,
            politeness_multiplier: 1,
            in_flight: 0,
            blocked_until_mono: None,
            last_outcome: None,
            current_backoff: std::time::Duration::ZERO,
        }
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tracker() -> HealthTracker {
        HealthTracker::new(
            ProviderId::awc(),
            StationId::new("EHAM").unwrap(),
            HealthConfig::default(),
            utc("2026-09-26T12:00:00Z"),
        )
    }

    #[test]
    fn healthy_then_stale_then_recovers() {
        let mut t = tracker();
        let ev = t.on_success(
            utc("2026-09-26T12:58:00Z"),
            200,
            120,
            Some(utc("2026-09-26T12:55:00Z")),
            1,
            &gate(),
            None,
        );
        assert_eq!(ev.unwrap().snapshot.state, ProviderHealthState::Healthy);
        // 50 minutes later without a new observation: stale.
        let ev = t
            .on_tick(utc("2026-09-26T13:45:00Z"), None, &gate(), None)
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Stale);
        assert_eq!(ev.previous_state, Some(ProviderHealthState::Healthy));
        let ev = t
            .on_success(
                utc("2026-09-26T13:58:00Z"),
                200,
                100,
                Some(utc("2026-09-26T13:55:00Z")),
                1,
                &gate(),
                None,
            )
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Healthy);
    }

    #[test]
    fn a_source_is_healthy_only_on_its_own_recent_success() {
        let mut t = tracker();
        // Fresh data delivered by another source says nothing about this one.
        let ev = t.on_tick(
            utc("2026-09-26T12:58:00Z"),
            Some(utc("2026-09-26T12:55:00Z")),
            &gate(),
            None,
        );
        assert!(ev.is_none());
        assert_eq!(t.state(), ProviderHealthState::Standby);
        t.on_success(
            utc("2026-09-26T13:00:00Z"),
            200,
            100,
            Some(utc("2026-09-26T12:55:00Z")),
            0,
            &gate(),
            None,
        );
        assert_eq!(t.state(), ProviderHealthState::Healthy);
        // Idle for 50 minutes while another source keeps the data fresh.
        let ev = t
            .on_tick(
                utc("2026-09-26T13:50:00Z"),
                Some(utc("2026-09-26T13:25:00Z")),
                &gate(),
                None,
            )
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Standby);
        assert_eq!(ev.snapshot.reason, "standby: last success 50 min ago");
        assert!(!ev.snapshot.state.allows_new_weather_positions());
    }

    #[test]
    fn throttle_and_failures() {
        let mut t = tracker();
        t.on_success(
            utc("2026-09-26T12:58:00Z"),
            200,
            120,
            Some(utc("2026-09-26T12:55:00Z")),
            1,
            &gate(),
            None,
        );
        let ev = t
            .on_failure(
                utc("2026-09-26T13:00:00Z"),
                Some(429),
                true,
                "throttled",
                &gate(),
                None,
            )
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Throttled);
        // Two more failures ⇒ unavailable (takes precedence over throttled).
        t.on_failure(
            utc("2026-09-26T13:01:00Z"),
            Some(500),
            false,
            "500",
            &gate(),
            None,
        );
        let ev = t
            .on_failure(
                utc("2026-09-26T13:02:00Z"),
                Some(500),
                false,
                "500",
                &gate(),
                None,
            )
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Unavailable);
    }

    #[test]
    fn open_circuit_is_unavailable_and_malformed_is_degraded() {
        let mut t = tracker();
        t.on_success(
            utc("2026-09-26T12:58:00Z"),
            200,
            120,
            Some(utc("2026-09-26T12:55:00Z")),
            1,
            &gate(),
            None,
        );
        let ev = t
            .on_malformed(utc("2026-09-26T12:59:00Z"), "bad json", &gate(), None)
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Degraded);
        let mut g = gate();
        g.circuit = CircuitState::Open;
        let ev = t
            .on_tick(utc("2026-09-26T13:00:00Z"), None, &g, None)
            .unwrap();
        assert_eq!(ev.snapshot.state, ProviderHealthState::Unavailable);
        assert!(!ev.snapshot.state.allows_new_weather_positions());
    }

    #[test]
    fn one_slow_response_does_not_keep_a_source_degraded() {
        let mut t = tracker();
        let ok = |t: &mut HealthTracker, at: &str, ms: u64| {
            t.on_success(
                utc(at),
                200,
                ms,
                Some(utc("2026-09-26T12:55:00Z")),
                0,
                &gate(),
                None,
            );
            (t.state(), t.snapshot().reason.clone())
        };
        // The first request after a restart took 19.5 s: degraded …
        let (state, reason) = ok(&mut t, "2026-09-26T12:58:00Z", 19_516);
        assert_eq!(state, ProviderHealthState::Degraded);
        assert_eq!(reason, "high latency: last 19516 ms, average 19516 ms");
        // … until the next normal-speed poll, although the average (15.7 s)
        // needs six more to fall under 5 s.
        let (state, _) = ok(&mut t, "2026-09-26T12:59:00Z", 300);
        assert_eq!(state, ProviderHealthState::Healthy);
        assert!(t.snapshot().latency_ms_ewma.unwrap() > 15_000.0);
        // A source that stays slow stays degraded.
        for (at, ms) in [
            ("2026-09-26T13:00:00Z", 9_000),
            ("2026-09-26T13:01:00Z", 8_000),
        ] {
            ok(&mut t, at, ms);
        }
        assert_eq!(t.state(), ProviderHealthState::Degraded);
        assert!(!t.state().allows_new_weather_positions());
        // One spike after fast responses, with the average fine: healthy.
        let mut t = tracker();
        for m in 0..10 {
            ok(&mut t, &format!("2026-09-26T13:{m:02}:00Z"), 300);
        }
        let (state, _) = ok(&mut t, "2026-09-26T13:10:00Z", 8_000);
        assert_eq!(state, ProviderHealthState::Healthy);
    }
}
