//! A [`Clock`] driven by tokio's timer, so tests using
//! `#[tokio::test(start_paused = true)]` get deterministic virtual time for
//! gates and collectors alike.

use chrono::{DateTime, Utc};
use std::time::Duration;
use wm_core::time::Clock;

/// Clock whose monotonic time is `tokio::time::Instant` (pausable in tests) and
/// whose wall time is `origin_utc + monotonic`.
#[derive(Debug, Clone)]
pub struct TokioClock {
    origin_mono: tokio::time::Instant,
    origin_utc: DateTime<Utc>,
}

impl TokioClock {
    pub fn new(origin_utc: DateTime<Utc>) -> Self {
        Self {
            origin_mono: tokio::time::Instant::now(),
            origin_utc,
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        let d = chrono::Duration::from_std(self.monotonic()).unwrap_or(chrono::Duration::zero());
        self.origin_utc + d
    }

    fn monotonic(&self) -> Duration {
        tokio::time::Instant::now().saturating_duration_since(self.origin_mono)
    }
}
