//! The provider gate: one per provider, shared by every consumer of that provider.
//!
//! [`GateCore`] is a pure, synchronous state machine driven by a monotonic
//! timestamp. It enforces, in order: daily budget, circuit breaker,
//! `Retry-After`, exponential backoff with equal jitter, and the minimum
//! request spacing (escalated after throttling). [`ProviderGate`] wraps it with
//! a concurrency semaphore and a clock for async use.

use crate::policy::RateLimitPolicy;
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wm_core::health::CircuitState;
use wm_core::ids::ProviderId;
use wm_core::rng::SplitMix64;
use wm_core::time::Clock;

/// Value of a `Retry-After` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAfter {
    Delay(Duration),
    At(DateTime<Utc>),
}

impl RetryAfter {
    /// Parse `delta-seconds` or an HTTP-date (IMF-fixdate, RFC 9110 §10.2.3).
    pub fn parse(value: &str) -> Option<RetryAfter> {
        let v = value.trim();
        if let Ok(secs) = v.parse::<u64>() {
            return Some(RetryAfter::Delay(Duration::from_secs(secs)));
        }
        DateTime::parse_from_rfc2822(v)
            .ok()
            .map(|t| RetryAfter::At(t.with_timezone(&Utc)))
    }

    /// Delay relative to `now` (never negative).
    pub fn delay_from(&self, now: DateTime<Utc>) -> Duration {
        match *self {
            RetryAfter::Delay(d) => d,
            RetryAfter::At(t) => (t - now).to_std().unwrap_or(Duration::ZERO),
        }
    }
}

/// Classified result of one request, reported back to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    /// 2xx or 304.
    Success {
        status: u16,
    },
    /// 429 Too Many Requests.
    Throttled {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// 5xx (503 may carry Retry-After).
    ServerError {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// 4xx other than 429. 401/403 open the circuit immediately.
    ClientError {
        status: u16,
    },
    Timeout,
    Connect,
    /// Body read error, TLS error, oversize body, dropped permit, …
    Other(String),
}

impl RequestOutcome {
    pub fn status(&self) -> Option<u16> {
        match self {
            RequestOutcome::Success { status }
            | RequestOutcome::Throttled { status, .. }
            | RequestOutcome::ServerError { status, .. }
            | RequestOutcome::ClientError { status } => Some(*status),
            _ => None,
        }
    }

    pub fn class_label(&self) -> &'static str {
        match self {
            RequestOutcome::Success { .. } => "success",
            RequestOutcome::Throttled { .. } => "throttled",
            RequestOutcome::ServerError { .. } => "server_error",
            RequestOutcome::ClientError { .. } => "client_error",
            RequestOutcome::Timeout => "timeout",
            RequestOutcome::Connect => "connect",
            RequestOutcome::Other(_) => "other",
        }
    }
}

/// Why a request may not start yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    MinInterval,
    Backoff,
    RetryAfter,
    CircuitOpen,
    HalfOpenProbeInFlight,
    Concurrency,
    DailyBudget,
}

impl WaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitReason::MinInterval => "min_interval",
            WaitReason::Backoff => "backoff",
            WaitReason::RetryAfter => "retry_after",
            WaitReason::CircuitOpen => "circuit_open",
            WaitReason::HalfOpenProbeInFlight => "half_open_probe",
            WaitReason::Concurrency => "concurrency",
            WaitReason::DailyBudget => "daily_budget",
        }
    }
}

/// Admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Proceed,
    /// Not before the given monotonic instant.
    NotBefore {
        at: Duration,
        reason: WaitReason,
    },
}

/// Counters and state exposed for health/metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct GateStats {
    pub requests_total: u64,
    pub successes_total: u64,
    pub failures_total: u64,
    pub throttled_total: u64,
    pub requests_today: u32,
    pub requests_last_hour: u32,
    pub consecutive_failures: u32,
    pub circuit: CircuitState,
    pub circuit_opens: u32,
    pub politeness_multiplier: u32,
    pub in_flight: u32,
    pub blocked_until_mono: Option<Duration>,
    pub last_outcome: Option<RequestOutcome>,
    pub current_backoff: Duration,
}

/// Pure gate state machine.
#[derive(Debug, Clone)]
pub struct GateCore {
    policy: RateLimitPolicy,
    last_start: Option<Duration>,
    in_flight: u32,
    consecutive_failures: u32,
    backoff_until: Option<Duration>,
    current_backoff: Duration,
    retry_after_until: Option<Duration>,
    circuit: CircuitState,
    circuit_open_until: Option<Duration>,
    circuit_opens: u32,
    half_open_probe_in_flight: bool,
    politeness_multiplier: u32,
    politeness_until: Option<Duration>,
    budget_day: Option<NaiveDate>,
    requests_today: u32,
    recent_starts: VecDeque<Duration>,
    requests_total: u64,
    successes_total: u64,
    failures_total: u64,
    throttled_total: u64,
    last_outcome: Option<RequestOutcome>,
    rng: SplitMix64,
}

const MAX_POLITENESS: u32 = 16;
const HOUR: Duration = Duration::from_secs(3600);

impl GateCore {
    pub fn new(policy: RateLimitPolicy, seed: u64) -> Self {
        Self {
            policy,
            last_start: None,
            in_flight: 0,
            consecutive_failures: 0,
            backoff_until: None,
            current_backoff: Duration::ZERO,
            retry_after_until: None,
            circuit: CircuitState::Closed,
            circuit_open_until: None,
            circuit_opens: 0,
            half_open_probe_in_flight: false,
            politeness_multiplier: 1,
            politeness_until: None,
            budget_day: None,
            requests_today: 0,
            recent_starts: VecDeque::new(),
            requests_total: 0,
            successes_total: 0,
            failures_total: 0,
            throttled_total: 0,
            last_outcome: None,
            rng: SplitMix64::new(seed),
        }
    }

    pub fn policy(&self) -> &RateLimitPolicy {
        &self.policy
    }

    /// Effective minimum spacing, including politeness escalation.
    pub fn effective_min_interval(&self, now: Duration) -> Duration {
        let mult = match self.politeness_until {
            Some(until) if now < until => self.politeness_multiplier,
            _ => 1,
        };
        self.policy
            .min_interval
            .saturating_mul(mult)
            .max(self.policy.class.min_interval_floor())
    }

    fn roll_budget_day(&mut self, today: NaiveDate) {
        if self.budget_day != Some(today) {
            self.budget_day = Some(today);
            self.requests_today = 0;
        }
    }

    /// Decide whether a request may start at monotonic time `now` (UTC date `today`
    /// is used for the daily budget). On `Proceed` the start is recorded.
    pub fn admit(
        &mut self,
        now: Duration,
        today: NaiveDate,
        until_midnight: Duration,
    ) -> Admission {
        self.roll_budget_day(today);
        if let Some(budget) = self.policy.daily_budget
            && self.requests_today >= budget
        {
            return Admission::NotBefore {
                at: now + until_midnight,
                reason: WaitReason::DailyBudget,
            };
        }

        match self.circuit {
            CircuitState::Open => {
                let until = self.circuit_open_until.unwrap_or(now);
                if now < until {
                    return Admission::NotBefore {
                        at: until,
                        reason: WaitReason::CircuitOpen,
                    };
                }
                self.circuit = CircuitState::HalfOpen;
                self.half_open_probe_in_flight = false;
            }
            CircuitState::HalfOpen if self.half_open_probe_in_flight => {
                return Admission::NotBefore {
                    at: now + self.effective_min_interval(now),
                    reason: WaitReason::HalfOpenProbeInFlight,
                };
            }
            _ => {}
        }

        if let Some(until) = self.retry_after_until
            && now < until
        {
            return Admission::NotBefore {
                at: until,
                reason: WaitReason::RetryAfter,
            };
        }
        if let Some(until) = self.backoff_until
            && now < until
        {
            return Admission::NotBefore {
                at: until,
                reason: WaitReason::Backoff,
            };
        }
        if let Some(last) = self.last_start {
            let next = last + self.effective_min_interval(now);
            if now < next {
                return Admission::NotBefore {
                    at: next,
                    reason: WaitReason::MinInterval,
                };
            }
        }
        if self.in_flight >= self.policy.max_concurrency {
            return Admission::NotBefore {
                at: now + Duration::from_millis(100),
                reason: WaitReason::Concurrency,
            };
        }

        // Admit.
        self.last_start = Some(now);
        self.in_flight += 1;
        self.requests_today += 1;
        self.requests_total += 1;
        self.recent_starts.push_back(now);
        self.trim_recent(now);
        if self.circuit == CircuitState::HalfOpen {
            self.half_open_probe_in_flight = true;
        }
        Admission::Proceed
    }

    fn trim_recent(&mut self, now: Duration) {
        while let Some(&front) = self.recent_starts.front() {
            if now.saturating_sub(front) >= HOUR {
                self.recent_starts.pop_front();
            } else {
                break;
            }
        }
    }

    /// Exponential backoff with "equal jitter" (AWS Architecture Blog,
    /// *Exponential Backoff and Jitter*): uniform in `[step/2, step]` where
    /// `step = min(max, base × 2^(attempt-1))`. Unlike full jitter it never
    /// produces a near-zero wait, which suits a politeness-first client.
    fn equal_jitter(&mut self, base: Duration, attempt: u32) -> Duration {
        let exp = base.saturating_mul(1u32 << attempt.saturating_sub(1).min(20));
        let cap = exp.min(self.policy.backoff_max);
        let half = cap / 2;
        let jitter = (half.as_secs_f64() * self.rng.next_f64()).max(0.0);
        half + Duration::from_secs_f64(jitter)
    }

    fn open_circuit(&mut self, now: Duration, force_max: bool) {
        let dur = if force_max {
            self.policy.circuit_open_max
        } else {
            self.policy
                .circuit_open_base
                .saturating_mul(1u32 << self.circuit_opens.min(16))
                .min(self.policy.circuit_open_max)
        };
        self.circuit = CircuitState::Open;
        self.circuit_open_until = Some(now + dur);
        self.circuit_opens = self.circuit_opens.saturating_add(1);
        self.half_open_probe_in_flight = false;
    }

    /// Report the outcome of an admitted request that started earlier.
    pub fn complete(&mut self, now: Duration, outcome: RequestOutcome) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let was_half_open = self.circuit == CircuitState::HalfOpen;
        self.half_open_probe_in_flight = false;

        match &outcome {
            RequestOutcome::Success { .. } => {
                self.successes_total += 1;
                self.consecutive_failures = 0;
                self.backoff_until = None;
                self.current_backoff = Duration::ZERO;
                if was_half_open || self.circuit == CircuitState::Open {
                    self.circuit = CircuitState::Closed;
                    self.circuit_open_until = None;
                }
                self.circuit_opens = 0;
            }
            RequestOutcome::Throttled { retry_after, .. } => {
                self.throttled_total += 1;
                self.failures_total += 1;
                self.consecutive_failures += 1;
                let ra = retry_after.map(|d| d.min(self.policy.max_retry_after));
                let own =
                    self.equal_jitter(self.policy.throttle_backoff_base, self.consecutive_failures);
                // Honour the server's Retry-After as a minimum; be at least as
                // patient as our own throttle backoff.
                let wait = match ra {
                    Some(d) if self.policy.respect_retry_after => d.max(own),
                    _ => own,
                };
                self.retry_after_until = Some(now + wait.max(self.effective_min_interval(now)));
                self.current_backoff = wait;
                // Escalate politeness for the decay period.
                self.politeness_multiplier = (self.politeness_multiplier.max(1)
                    * self.policy.politeness_factor)
                    .min(MAX_POLITENESS);
                self.politeness_until = Some(now + self.policy.politeness_decay);
                if was_half_open
                    || self.consecutive_failures >= self.policy.circuit_failure_threshold
                {
                    self.open_circuit(now, false);
                }
            }
            other => {
                self.failures_total += 1;
                self.consecutive_failures += 1;
                let backoff =
                    self.equal_jitter(self.policy.backoff_base, self.consecutive_failures);
                self.backoff_until = Some(now + backoff.max(self.effective_min_interval(now)));
                self.current_backoff = backoff;
                if let RequestOutcome::ServerError {
                    retry_after: Some(ra),
                    ..
                } = other
                    && self.policy.respect_retry_after
                {
                    let ra = (*ra).min(self.policy.max_retry_after);
                    self.retry_after_until = Some(now + ra);
                }
                let forbidden = matches!(other, RequestOutcome::ClientError { status: 401 | 403 });
                if forbidden {
                    self.open_circuit(now, true);
                } else if was_half_open
                    || self.consecutive_failures >= self.policy.circuit_failure_threshold
                {
                    self.open_circuit(now, false);
                }
            }
        }
        self.last_outcome = Some(outcome);
    }

    /// Earliest monotonic instant at which a request could be admitted,
    /// ignoring the daily budget.
    pub fn blocked_until(&self) -> Option<Duration> {
        [
            self.retry_after_until,
            self.backoff_until,
            self.circuit_open_until
                .filter(|_| self.circuit == CircuitState::Open),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    pub fn stats(&self, now: Duration) -> GateStats {
        let requests_last_hour = self
            .recent_starts
            .iter()
            .filter(|&&t| now.saturating_sub(t) < HOUR)
            .count() as u32;
        GateStats {
            requests_total: self.requests_total,
            successes_total: self.successes_total,
            failures_total: self.failures_total,
            throttled_total: self.throttled_total,
            requests_today: self.requests_today,
            requests_last_hour,
            consecutive_failures: self.consecutive_failures,
            circuit: self.circuit,
            circuit_opens: self.circuit_opens,
            politeness_multiplier: match self.politeness_until {
                Some(u) if now < u => self.politeness_multiplier,
                _ => 1,
            },
            in_flight: self.in_flight,
            blocked_until_mono: self.blocked_until().filter(|&b| b > now),
            last_outcome: self.last_outcome.clone(),
            current_backoff: self.current_backoff,
        }
    }
}

/// Wait result for async callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("provider gate closed ({reason:?}); retry in {retry_in:?}")]
pub struct GateWait {
    pub reason: WaitReason,
    pub retry_in: Duration,
}

/// Async, shareable gate for one provider.
pub struct ProviderGate {
    provider: ProviderId,
    core: Mutex<GateCore>,
    semaphore: Arc<Semaphore>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ProviderGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderGate")
            .field("provider", &self.provider)
            .finish()
    }
}

impl ProviderGate {
    pub fn new(
        provider: ProviderId,
        policy: RateLimitPolicy,
        clock: Arc<dyn Clock>,
        seed: u64,
    ) -> Arc<Self> {
        let permits = policy.max_concurrency.max(1) as usize;
        Arc::new(Self {
            provider,
            core: Mutex::new(GateCore::new(policy, seed)),
            semaphore: Arc::new(Semaphore::new(permits)),
            clock,
        })
    }

    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateCore> {
        self.core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn policy(&self) -> RateLimitPolicy {
        self.lock().policy().clone()
    }

    fn today_and_until_midnight(&self) -> (NaiveDate, Duration) {
        let now = self.clock.now();
        let today = now.date_naive();
        let midnight = today
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|n| n.and_utc())
            .unwrap_or(now);
        (today, (midnight - now).to_std().unwrap_or(Duration::ZERO))
    }

    /// Non-blocking admission.
    pub fn try_acquire(self: &Arc<Self>) -> Result<GatePermit, GateWait> {
        let sem = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                return Err(GateWait {
                    reason: WaitReason::Concurrency,
                    retry_in: Duration::from_millis(100),
                });
            }
        };
        let now = self.clock.monotonic();
        let (today, until_midnight) = self.today_and_until_midnight();
        let admission = self.lock().admit(now, today, until_midnight);
        match admission {
            Admission::Proceed => Ok(GatePermit {
                gate: Arc::clone(self),
                _sem: sem,
                completed: false,
            }),
            Admission::NotBefore { at, reason } => Err(GateWait {
                reason,
                retry_in: at.saturating_sub(now),
            }),
        }
    }

    /// Wait (sleeping on the tokio timer) until admitted, up to `max_wait`.
    /// Gate closures longer than `max_wait` return immediately so the caller can
    /// reschedule instead of holding a task hostage.
    pub async fn acquire(self: &Arc<Self>, max_wait: Duration) -> Result<GatePermit, GateWait> {
        // The waiting budget is measured on the tokio timer (real time, or
        // virtual time in paused tests), never on the injected clock: a manual
        // clock that does not advance must not turn this into an endless loop.
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            match self.try_acquire() {
                Ok(p) => return Ok(p),
                Err(w) => {
                    let now = tokio::time::Instant::now();
                    if now + w.retry_in > deadline {
                        return Err(w);
                    }
                    tokio::time::sleep(w.retry_in.max(Duration::from_millis(1))).await;
                }
            }
        }
    }

    pub fn stats(&self) -> GateStats {
        let now = self.clock.monotonic();
        self.lock().stats(now)
    }

    /// Earliest wall-clock instant a request would be admitted, without
    /// consuming anything (`None` = admissible now).
    pub fn not_before_utc(&self) -> Option<DateTime<Utc>> {
        let now = self.clock.monotonic();
        let (today, until_midnight) = self.today_and_until_midnight();
        let mut probe = self.lock().clone();
        match probe.admit(now, today, until_midnight) {
            Admission::Proceed => None,
            Admission::NotBefore { at, .. } => {
                let delta = chrono::Duration::from_std(at.saturating_sub(now)).ok()?;
                Some(self.clock.now() + delta)
            }
        }
    }

    /// Wall-clock instant until which the gate is closed (if any).
    pub fn blocked_until_utc(&self) -> Option<DateTime<Utc>> {
        let now_mono = self.clock.monotonic();
        let blocked = self.lock().blocked_until()?;
        let delta = blocked.checked_sub(now_mono)?;
        chrono::Duration::from_std(delta)
            .ok()
            .map(|d| self.clock.now() + d)
    }

    fn complete(&self, outcome: RequestOutcome) {
        let now = self.clock.monotonic();
        self.lock().complete(now, outcome);
    }
}

/// Proof of admission. Must be completed with the request outcome; dropping it
/// uncompleted records an `Other` failure (so a panicking caller cannot leave
/// the gate believing a request is still in flight).
pub struct GatePermit {
    gate: Arc<ProviderGate>,
    _sem: OwnedSemaphorePermit,
    completed: bool,
}

impl GatePermit {
    pub fn complete(mut self, outcome: RequestOutcome) {
        self.completed = true;
        self.gate.complete(outcome);
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        if !self.completed {
            self.gate.complete(RequestOutcome::Other(
                "permit dropped without outcome".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DAY: NaiveDate = match NaiveDate::from_ymd_opt(2026, 9, 26) {
        Some(d) => d,
        None => panic!("valid date"),
    };
    const FAR: Duration = Duration::from_secs(10 * 3600);

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn nws() -> GateCore {
        GateCore::new(RateLimitPolicy::nws_conservative(), 1)
    }

    #[test]
    fn enforces_min_interval() {
        let mut g = nws();
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(s(1), RequestOutcome::Success { status: 200 });
        match g.admit(s(10), DAY, FAR) {
            Admission::NotBefore { at, reason } => {
                assert_eq!(reason, WaitReason::MinInterval);
                assert_eq!(at, s(30));
            }
            a => panic!("unexpected {a:?}"),
        }
        assert_eq!(g.admit(s(30), DAY, FAR), Admission::Proceed);
    }

    #[test]
    fn retry_after_is_honoured_and_politeness_escalates() {
        let mut g = nws();
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(
            s(1),
            RequestOutcome::Throttled {
                status: 429,
                retry_after: Some(s(600)),
            },
        );
        match g.admit(s(599), DAY, FAR) {
            Admission::NotBefore {
                reason: WaitReason::RetryAfter,
                at,
            } => assert!(at >= s(601)),
            a => panic!("unexpected {a:?}"),
        }
        // Politeness doubled the minimum interval for 24h.
        assert_eq!(g.effective_min_interval(s(1000)), s(60));
        assert_eq!(g.effective_min_interval(s(1 + 24 * 3600)), s(30));
        let st = g.stats(s(2));
        assert_eq!(st.throttled_total, 1);
        assert_eq!(st.politeness_multiplier, 2);
    }

    #[test]
    fn throttle_without_retry_after_uses_throttle_backoff() {
        let mut g = nws();
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(
            s(0),
            RequestOutcome::Throttled {
                status: 429,
                retry_after: None,
            },
        );
        // throttle_backoff_base = 300s, equal jitter in [150s, 300s].
        match g.admit(s(149), DAY, FAR) {
            Admission::NotBefore {
                reason: WaitReason::RetryAfter,
                ..
            } => {}
            a => panic!("unexpected {a:?}"),
        }
        let blocked = g.blocked_until().unwrap();
        assert!(blocked >= s(150) && blocked <= s(300), "{blocked:?}");
    }

    #[test]
    fn server_errors_back_off_exponentially_then_open_circuit() {
        let mut g = nws();
        let mut t = s(0);
        let mut last_backoff = Duration::ZERO;
        for i in 1..=5u32 {
            // Wait until admitted.
            loop {
                match g.admit(t, DAY, FAR) {
                    Admission::Proceed => break,
                    Admission::NotBefore { at, .. } => t = at,
                }
            }
            g.complete(
                t,
                RequestOutcome::ServerError {
                    status: 500,
                    retry_after: None,
                },
            );
            let st = g.stats(t);
            assert_eq!(st.consecutive_failures, i);
            if i < 5 {
                assert!(
                    st.current_backoff >= last_backoff / 2,
                    "backoff should grow"
                );
                last_backoff = st.current_backoff;
                assert_eq!(st.circuit, CircuitState::Closed);
            } else {
                assert_eq!(st.circuit, CircuitState::Open);
            }
        }
        match g.admit(t + s(1), DAY, FAR) {
            Admission::NotBefore {
                reason: WaitReason::CircuitOpen,
                at,
            } => {
                assert!(at >= t + s(600));
            }
            a => panic!("unexpected {a:?}"),
        }
    }

    #[test]
    fn half_open_allows_single_probe_and_recovers() {
        let mut p = RateLimitPolicy::nws_conservative();
        p.circuit_failure_threshold = 1;
        let mut g = GateCore::new(p, 3);
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(s(0), RequestOutcome::Timeout);
        assert_eq!(g.stats(s(0)).circuit, CircuitState::Open);
        let open_until = g.blocked_until().unwrap();
        let t = open_until.max(s(3600));
        assert_eq!(g.admit(t, DAY, FAR), Admission::Proceed, "probe admitted");
        assert!(matches!(
            g.admit(t + s(31), DAY, FAR),
            Admission::NotBefore {
                reason: WaitReason::HalfOpenProbeInFlight,
                ..
            }
        ));
        g.complete(t + s(1), RequestOutcome::Success { status: 200 });
        assert_eq!(g.stats(t).circuit, CircuitState::Closed);
        assert_eq!(g.admit(t + s(40), DAY, FAR), Admission::Proceed);
    }

    #[test]
    fn half_open_failure_reopens_with_longer_duration() {
        let mut p = RateLimitPolicy::nws_conservative();
        p.circuit_failure_threshold = 1;
        let mut g = GateCore::new(p, 3);
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(s(0), RequestOutcome::Connect);
        let first_open = g.blocked_until().unwrap();
        let t = first_open.max(s(3600));
        assert_eq!(g.admit(t, DAY, FAR), Admission::Proceed);
        g.complete(t, RequestOutcome::Connect);
        let second = g.blocked_until().unwrap() - t;
        assert!(second >= s(1200), "escalated open duration {second:?}");
    }

    #[test]
    fn forbidden_opens_circuit_for_max_duration() {
        let mut g = nws();
        assert_eq!(g.admit(s(0), DAY, FAR), Admission::Proceed);
        g.complete(s(0), RequestOutcome::ClientError { status: 403 });
        assert_eq!(g.stats(s(0)).circuit, CircuitState::Open);
        assert!(g.blocked_until().unwrap() >= s(2 * 3600));
    }

    #[test]
    fn daily_budget_is_hard() {
        let mut p = RateLimitPolicy::nws_conservative();
        p.daily_budget = Some(3);
        let mut g = GateCore::new(p, 5);
        let mut t = s(0);
        for _ in 0..3 {
            assert_eq!(g.admit(t, DAY, FAR), Admission::Proceed);
            g.complete(t, RequestOutcome::Success { status: 200 });
            t += s(30);
        }
        assert!(matches!(
            g.admit(t, DAY, s(100)),
            Admission::NotBefore {
                reason: WaitReason::DailyBudget,
                ..
            }
        ));
        // New UTC day resets the budget.
        let tomorrow = DAY.succ_opt().unwrap();
        assert_eq!(g.admit(t + s(100), tomorrow, FAR), Admission::Proceed);
    }

    #[derive(Debug, Clone)]
    enum Op {
        Success,
        Throttle(Option<u64>),
        Server,
        Timeout,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => Just(Op::Success),
            1 => proptest::option::of(0u64..7200).prop_map(Op::Throttle),
            1 => Just(Op::Server),
            1 => Just(Op::Timeout),
        ]
    }

    proptest! {
        /// Whatever the provider does, admitted request starts are never closer
        /// than the class floor, never inside a Retry-After window, and the
        /// daily budget is never exceeded.
        #[test]
        fn gate_invariants(ops in proptest::collection::vec((op(), 0u64..4000), 1..200)) {
            let mut policy = RateLimitPolicy::nws_conservative();
            policy.daily_budget = Some(50);
            let floor = policy.class.min_interval_floor();
            let mut g = GateCore::new(policy, 99);
            let mut now = Duration::ZERO;
            let mut last_start: Option<Duration> = None;
            let mut retry_after_until: Option<Duration> = None;
            let mut admitted = 0u32;
            for (o, step) in ops {
                now += Duration::from_secs(step);
                if let Admission::Proceed = g.admit(now, DAY, FAR) {
                    if let Some(ls) = last_start {
                        prop_assert!(now - ls >= floor, "spacing {:?} < floor", now - ls);
                    }
                    if let Some(ra) = retry_after_until {
                        prop_assert!(now >= ra, "admitted inside Retry-After window");
                    }
                    admitted += 1;
                    prop_assert!(admitted <= 50);
                    last_start = Some(now);
                    let outcome = match o {
                        Op::Success => RequestOutcome::Success { status: 200 },
                        Op::Throttle(ra) => {
                            if let Some(secs) = ra {
                                retry_after_until = Some(now + Duration::from_secs(secs));
                            }
                            RequestOutcome::Throttled { status: 429, retry_after: ra.map(Duration::from_secs) }
                        }
                        Op::Server => RequestOutcome::ServerError { status: 503, retry_after: None },
                        Op::Timeout => RequestOutcome::Timeout,
                    };
                    g.complete(now, outcome);
                }
            }
        }
    }
}
