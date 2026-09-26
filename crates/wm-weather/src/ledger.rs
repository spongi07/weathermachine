//! Observation deduplication and correction tracking.
//!
//! Identity is the provider-independent [`ObservationKey`] (station,
//! observation time, report type); content identity is the SHA-256 of the
//! canonical report text. Versions are append-only: a correction never
//! overwrites the earlier version.

use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, HashMap};
use wm_core::ids::StationId;
use wm_core::weather::{DedupClass, Observation, ObservationKey};

/// Classification result.
#[derive(Debug, Clone, PartialEq)]
pub struct Classified {
    pub class: DedupClass,
    /// The observation with its version number assigned.
    pub observation: Observation,
    /// Previous latest version (for corrections/revisions).
    pub previous: Option<Observation>,
}

/// In-memory ledger of recently seen observations.
#[derive(Debug, Clone)]
pub struct ObservationLedger {
    versions: BTreeMap<ObservationKey, Vec<Observation>>,
    newest: HashMap<StationId, DateTime<Utc>>,
    retention: Duration,
}

impl Default for ObservationLedger {
    fn default() -> Self {
        Self::new(Duration::hours(72))
    }
}

impl ObservationLedger {
    pub fn new(retention: Duration) -> Self {
        Self { versions: BTreeMap::new(), newest: HashMap::new(), retention }
    }

    /// Seed from persisted observations after a restart, so duplicates are
    /// recognized and versions continue correctly.
    pub fn warm_start(&mut self, observations: impl IntoIterator<Item = Observation>) {
        for obs in observations {
            let newest = self.newest.entry(obs.key.station.clone()).or_insert(obs.key.observed_at);
            if obs.key.observed_at > *newest {
                *newest = obs.key.observed_at;
            }
            let versions = self.versions.entry(obs.key.clone()).or_default();
            versions.push(obs);
            versions.sort_by_key(|o| o.version);
        }
    }

    pub fn newest_observation_time(&self, station: &StationId) -> Option<DateTime<Utc>> {
        self.newest.get(station).copied()
    }

    /// Latest version of every key for `station`, in time order.
    pub fn latest_for_station(&self, station: &StationId) -> Vec<&Observation> {
        self.versions
            .iter()
            .filter(|(k, _)| &k.station == station)
            .filter_map(|(_, v)| v.last())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    /// Classify an incoming (version-1) observation and record it if it is new
    /// information.
    pub fn classify(&mut self, mut candidate: Observation) -> Classified {
        let key = candidate.key.clone();
        match self.versions.get_mut(&key) {
            None => {
                let newest = self.newest.get(&key.station).copied();
                let class = match newest {
                    Some(n) if key.observed_at <= n => DedupClass::OutOfOrder,
                    _ => DedupClass::New,
                };
                if newest.is_none_or(|n| key.observed_at > n) {
                    self.newest.insert(key.station.clone(), key.observed_at);
                }
                candidate.version = 1;
                self.versions.insert(key, vec![candidate.clone()]);
                Classified { class, observation: candidate, previous: None }
            }
            Some(versions) => {
                // Invariant: a key is only ever inserted with a non-empty version list.
                let Some(latest) = versions.last().cloned() else {
                    candidate.version = 1;
                    versions.push(candidate.clone());
                    return Classified { class: DedupClass::New, observation: candidate, previous: None };
                };
                let seen_before = versions.iter().any(|v| v.content_hash == candidate.content_hash);
                if seen_before {
                    // Identical content (possibly relayed by a different provider,
                    // or an upstream flip-flop back to an earlier version).
                    return Classified { class: DedupClass::Duplicate, observation: latest, previous: None };
                }
                candidate.version = latest.version + 1;
                let class = if candidate.quality.correction_marker {
                    DedupClass::Correction
                } else {
                    DedupClass::Revision
                };
                versions.push(candidate.clone());
                Classified { class, observation: candidate, previous: Some(latest) }
            }
        }
    }

    /// Drop keys older than the retention window relative to `now`.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - self.retention;
        self.versions.retain(|k, _| k.observed_at >= cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::hash::sha256_hex;
    use wm_core::ids::ProviderId;
    use wm_core::units::TempC;
    use wm_core::weather::{QualityFlags, ReportType, TempPrecision};

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn obs(time: &str, temp: i32, raw: &str, cor: bool) -> Observation {
        Observation {
            key: ObservationKey { station: StationId::new("EHAM").unwrap(), observed_at: utc(time), report_type: ReportType::Metar },
            version: 1,
            temperature: Some(TempC::from_whole(temp)),
            dewpoint: None,
            precision: TempPrecision::WholeDegree,
            raw_text: raw.to_owned(),
            content_hash: sha256_hex(raw.as_bytes()),
            provider: ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: utc(time) + Duration::minutes(3),
            parser_version: 1,
            quality: QualityFlags { correction_marker: cor, ..QualityFlags::default() },
        }
    }

    #[test]
    fn new_duplicate_correction_revision_out_of_order() {
        let mut l = ObservationLedger::default();
        let a = l.classify(obs("2026-09-26T12:25:00Z", 17, "A 17/12", false));
        assert_eq!(a.class, DedupClass::New);
        let b = l.classify(obs("2026-09-26T12:55:00Z", 18, "B 18/12", false));
        assert_eq!(b.class, DedupClass::New);
        // Same content again (e.g. next poll or another relay): duplicate.
        let dup = l.classify(obs("2026-09-26T12:55:00Z", 18, "B 18/12", false));
        assert_eq!(dup.class, DedupClass::Duplicate);
        assert_eq!(dup.observation.version, 1);
        // Corrected report (COR): new version, previous preserved.
        let cor = l.classify(obs("2026-09-26T12:55:00Z", 17, "B COR 17/12", true));
        assert_eq!(cor.class, DedupClass::Correction);
        assert_eq!(cor.observation.version, 2);
        assert_eq!(cor.previous.as_ref().unwrap().temperature, Some(TempC::from_whole(18)));
        // Unlabelled change: revision, version 3.
        let rev = l.classify(obs("2026-09-26T12:55:00Z", 16, "B 16/12", false));
        assert_eq!(rev.class, DedupClass::Revision);
        assert_eq!(rev.observation.version, 3);
        // Flip-flop back to an earlier version: treated as duplicate.
        let flip = l.classify(obs("2026-09-26T12:55:00Z", 18, "B 18/12", false));
        assert_eq!(flip.class, DedupClass::Duplicate);
        // Late arrival of an older report.
        let late = l.classify(obs("2026-09-26T11:55:00Z", 16, "C 16/12", false));
        assert_eq!(late.class, DedupClass::OutOfOrder);
        assert_eq!(l.newest_observation_time(&StationId::new("EHAM").unwrap()), Some(utc("2026-09-26T12:55:00Z")));
        let latest = l.latest_for_station(&StationId::new("EHAM").unwrap());
        assert_eq!(latest.len(), 3);
        assert_eq!(latest[2].version, 3);
    }

    #[test]
    fn warm_start_and_prune() {
        let mut l = ObservationLedger::new(Duration::hours(24));
        l.warm_start(vec![obs("2026-09-25T12:55:00Z", 18, "X", false)]);
        assert_eq!(l.classify(obs("2026-09-25T12:55:00Z", 18, "X", false)).class, DedupClass::Duplicate);
        l.prune(utc("2026-09-26T13:00:00Z"));
        assert!(l.is_empty());
    }
}
