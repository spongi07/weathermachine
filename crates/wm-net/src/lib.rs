//! # wm-net
//!
//! Every outbound request Weather Machine makes goes through this crate:
//!
//! * [`ProviderGate`] — one per provider, shared by all consumers, enforcing a
//!   provider-specific [`RateLimitPolicy`] (minimum spacing, concurrency,
//!   `Retry-After`, equal-jitter exponential backoff, circuit breaker, daily
//!   budget, politeness escalation after throttling).
//! * [`HttpFetcher`] — `reqwest` client bound to a gate: descriptive
//!   User-Agent, timeouts, conditional GET, body size cap, audit records.
//! * Caches for latest responses and slow-changing metadata.

pub mod cache;
pub mod clock;
pub mod gate;
pub mod http;
pub mod policy;
pub mod user_agent;

pub use cache::{CachedResponse, ResponseCache, TtlCache};
pub use clock::TokioClock;
pub use gate::{Admission, GateCore, GatePermit, GateStats, GateWait, ProviderGate, RequestOutcome, RetryAfter, WaitReason};
pub use http::{FetchError, FetchRequest, FetchResponse, HttpFetcher, NetError};
pub use policy::{PolicyError, ProviderClass, RateLimitPolicy};
pub use user_agent::build_user_agent;
