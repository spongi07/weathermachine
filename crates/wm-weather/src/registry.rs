//! Guarantees one collector per station inside a process. Cross-process
//! exclusivity is provided by a PostgreSQL advisory-lock lease (`wm-storage`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use wm_core::ids::StationId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a collector for station {0} is already running")]
pub struct AlreadyRunning(pub StationId);

/// Registry of stations with an active collector.
#[derive(Debug, Clone, Default)]
pub struct CollectorRegistry {
    inner: Arc<Mutex<HashSet<StationId>>>,
}

impl CollectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim a station. Fails if already claimed; the claim is released on drop.
    pub fn claim(&self, station: &StationId) -> Result<CollectorClaim, AlreadyRunning> {
        let mut set = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !set.insert(station.clone()) {
            return Err(AlreadyRunning(station.clone()));
        }
        Ok(CollectorClaim {
            station: station.clone(),
            registry: self.clone(),
        })
    }

    pub fn is_claimed(&self, station: &StationId) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(station)
    }
}

/// Proof that the holder is the only collector for `station` in this process.
#[derive(Debug)]
pub struct CollectorClaim {
    station: StationId,
    registry: CollectorRegistry,
}

impl CollectorClaim {
    pub fn station(&self) -> &StationId {
        &self.station
    }
}

impl Drop for CollectorClaim {
    fn drop(&mut self) {
        self.registry
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.station);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_claim_per_station() {
        let r = CollectorRegistry::new();
        let eham = StationId::new("EHAM").unwrap();
        let c = r.claim(&eham).unwrap();
        assert!(r.claim(&eham).is_err());
        assert!(r.claim(&StationId::new("EGLL").unwrap()).is_ok());
        drop(c);
        assert!(r.claim(&eham).is_ok());
    }
}
