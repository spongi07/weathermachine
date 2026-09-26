//! Probability models for the final daily high.
//!
//! Target: the distribution of the increment `k = final_high − current_high`
//! (whole degrees at resolution precision), `k ∈ {0, 1, …, K−2, ≥K−1}`.
//!
//! The baseline model is an empirical frequency table estimated from station
//! history (decades of METARs are available), with hierarchical Dirichlet
//! smoothing: each specific cell shrinks toward its parent cell, so sparse
//! cells never produce extreme probabilities. Without a trained model the
//! [`NoEdgeModel`] returns nothing and no weather-dependent trade is possible.

use crate::peak::{PeakFeatures, TrajectoryClass};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wm_core::market::TemperatureBucket;

/// Distribution over the final-high increment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncrementDistribution {
    /// `probs[k]` = P(final = high + k); the last entry is P(final ≥ high + K − 1).
    pub probs: Vec<f64>,
    /// Number of historical samples in the most specific cell used.
    pub support: u32,
    /// Human-readable provenance (model id and cell).
    pub source: String,
}

impl IncrementDistribution {
    pub fn k_max(&self) -> usize {
        self.probs.len()
    }

    pub fn p_equals(&self, k: usize) -> f64 {
        if k + 1 >= self.probs.len() {
            0.0
        } else {
            self.probs[k]
        }
    }

    fn tail_index(&self) -> usize {
        self.probs.len().saturating_sub(1)
    }

    /// Lower bound of P(final ∈ bucket): tail mass (final ≥ high+K−1) is only
    /// counted when the bucket contains *every* value from high+K−1 upward.
    /// Used for the win probability of YES positions (conservative).
    pub fn p_in_bucket_lower(&self, high_whole: i32, bucket: &TemperatureBucket) -> f64 {
        let tail = self.tail_index();
        let mut p = 0.0;
        for (k, pk) in self.probs.iter().enumerate() {
            let v = high_whole + k as i32;
            if k < tail {
                if bucket.contains(v) {
                    p += pk;
                }
            } else if bucket.contains(v) && bucket.upper.is_none() {
                p += pk;
            }
        }
        p.clamp(0.0, 1.0)
    }

    /// Upper bound of P(final ∈ bucket): tail mass counts if the bucket
    /// contains *any* value ≥ high+K−1. Used for the loss probability of NO
    /// positions (conservative).
    pub fn p_in_bucket_upper(&self, high_whole: i32, bucket: &TemperatureBucket) -> f64 {
        let tail = self.tail_index();
        let mut p = 0.0;
        for (k, pk) in self.probs.iter().enumerate() {
            let v = high_whole + k as i32;
            if k < tail {
                if bucket.contains(v) {
                    p += pk;
                }
            } else if bucket.upper.is_none_or(|hi| hi >= v) {
                p += pk;
            }
        }
        p.clamp(0.0, 1.0)
    }

    /// Normalize to sum 1 (defensive).
    pub fn normalized(mut self) -> Self {
        let s: f64 = self.probs.iter().sum();
        if s > 0.0 && s.is_finite() {
            for p in &mut self.probs {
                *p /= s;
            }
        }
        self
    }
}

/// A probability model.
pub trait ProbabilityModel: Send + Sync {
    fn id(&self) -> &str;
    fn distribution(&self, features: &PeakFeatures) -> Option<IncrementDistribution>;
}

/// Fail-closed model: never provides a distribution, so no strategy can find edge.
#[derive(Debug, Clone, Default)]
pub struct NoEdgeModel;

impl ProbabilityModel for NoEdgeModel {
    fn id(&self) -> &str {
        "no-edge"
    }

    fn distribution(&self, _f: &PeakFeatures) -> Option<IncrementDistribution> {
        None
    }
}

/// Feature dimensions used as cell keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureDim {
    Season,
    MinutesSinceHigh,
    Drop,
    LocalHour,
    Trajectory,
}

/// Bucket boundaries for each dimension.
pub fn minutes_bucket(m: i64) -> &'static str {
    match m {
        i64::MIN..=29 => "m000-029",
        30..=59 => "m030-059",
        60..=89 => "m060-089",
        90..=119 => "m090-119",
        120..=179 => "m120-179",
        _ => "m180+",
    }
}

pub fn drop_bucket(tenths: i32) -> &'static str {
    match tenths {
        i32::MIN..=4 => "d0",
        5..=14 => "d1",
        15..=24 => "d2",
        _ => "d3+",
    }
}

pub fn hour_bucket(local_minute: u16) -> &'static str {
    match local_minute / 60 {
        0..=11 => "h<12",
        12..=13 => "h12-13",
        14..=15 => "h14-15",
        16..=17 => "h16-17",
        _ => "h18+",
    }
}

fn dim_value(dim: FeatureDim, f: &PeakFeatures) -> String {
    match dim {
        FeatureDim::Season => f.season.as_str().to_owned(),
        FeatureDim::MinutesSinceHigh => minutes_bucket(f.minutes_since_high).to_owned(),
        FeatureDim::Drop => drop_bucket(f.drop_tenths).to_owned(),
        FeatureDim::LocalHour => hour_bucket(f.local_minute_now).to_owned(),
        FeatureDim::Trajectory => match f.trajectory {
            TrajectoryClass::AtHigh => "at_high".to_owned(),
            TrajectoryClass::SteadyDecline => "decline".to_owned(),
            TrajectoryClass::Oscillating => "osc".to_owned(),
            TrajectoryClass::Insufficient => "insuf".to_owned(),
        },
    }
}

/// Cell key for a list of dimensions, e.g. `season=summer|msh=m060-089`.
pub fn cell_key(dims: &[FeatureDim], f: &PeakFeatures) -> String {
    if dims.is_empty() {
        return "global".to_owned();
    }
    dims.iter()
        .map(|d| format!("{d:?}={}", dim_value(*d, f)))
        .collect::<Vec<_>>()
        .join("|")
}

/// Empirical, hierarchically smoothed model (serializable artefact).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmpiricalPeakModel {
    pub id: String,
    pub station: String,
    pub view: String,
    pub trained_from: NaiveDate,
    pub trained_to: NaiveDate,
    pub created_at: DateTime<Utc>,
    /// Number of increment classes K (last = "≥ K−1").
    pub k_classes: usize,
    /// Dirichlet prior strength α toward the parent cell.
    pub prior_strength: f64,
    /// Hierarchy from least specific (index 0, usually empty = global) to most specific.
    pub levels: Vec<Vec<FeatureDim>>,
    /// Counts per cell key: `counts[k]`.
    pub cells: HashMap<String, Vec<u64>>,
}

impl EmpiricalPeakModel {
    pub fn new(
        id: impl Into<String>,
        station: impl Into<String>,
        view: impl Into<String>,
        k_classes: usize,
        levels: Vec<Vec<FeatureDim>>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            station: station.into(),
            view: view.into(),
            trained_from: now.date_naive(),
            trained_to: now.date_naive(),
            created_at: now,
            k_classes: k_classes.max(2),
            prior_strength: 20.0,
            levels,
            cells: HashMap::new(),
        }
    }

    /// Default hierarchy: global → minutes → minutes+drop → +season → +hour.
    pub fn default_levels() -> Vec<Vec<FeatureDim>> {
        use FeatureDim::*;
        vec![
            vec![],
            vec![MinutesSinceHigh],
            vec![MinutesSinceHigh, Drop],
            vec![MinutesSinceHigh, Drop, Season],
            vec![MinutesSinceHigh, Drop, Season, LocalHour],
        ]
    }

    /// Record one training sample (features at time t, observed increment k).
    pub fn observe(&mut self, f: &PeakFeatures, increment: i32) {
        let k = (increment.max(0) as usize).min(self.k_classes - 1);
        for dims in self.levels.clone() {
            let key = cell_key(&dims, f);
            let counts = self
                .cells
                .entry(key)
                .or_insert_with(|| vec![0; self.k_classes]);
            counts[k] += 1;
        }
    }

    pub fn total_samples(&self) -> u64 {
        self.cells.get("global").map_or(0, |c| c.iter().sum())
    }
}

impl ProbabilityModel for EmpiricalPeakModel {
    fn id(&self) -> &str {
        &self.id
    }

    fn distribution(&self, f: &PeakFeatures) -> Option<IncrementDistribution> {
        let mut post: Option<Vec<f64>> = None;
        let mut support = 0u64;
        let mut used = String::new();
        for dims in &self.levels {
            let key = cell_key(dims, f);
            let Some(counts) = self.cells.get(&key) else {
                break;
            };
            let n: u64 = counts.iter().sum();
            if n == 0 {
                break;
            }
            let probs: Vec<f64> = match &post {
                None => counts.iter().map(|c| *c as f64 / n as f64).collect(),
                Some(prior) => counts
                    .iter()
                    .zip(prior)
                    .map(|(c, p)| {
                        (*c as f64 + self.prior_strength * p) / (n as f64 + self.prior_strength)
                    })
                    .collect(),
            };
            post = Some(probs);
            support = n;
            used = key;
        }
        post.map(|probs| {
            IncrementDistribution {
                probs,
                support: support.min(u64::from(u32::MAX)) as u32,
                source: format!("{}:{used}", self.id),
            }
            .normalized()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ViewKind;
    use wm_core::ids::StationId;
    use wm_core::market::TempUnit;
    use wm_core::time::Season;

    pub(crate) fn features(minutes: i64, drop: i32) -> PeakFeatures {
        PeakFeatures {
            station: StationId::new("EHAM").unwrap(),
            date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            view: ViewKind::All,
            high_tenths: 180,
            high_whole: 18,
            high_at: Utc::now(),
            current_tenths: 180 - drop,
            drop_tenths: drop,
            minutes_since_high: minutes,
            minutes_since_first_high: minutes,
            lower_obs_since_high: 2,
            retests: 0,
            slope_c_per_hour: Some(-0.5),
            accel_c_per_hour2: None,
            trajectory: TrajectoryClass::SteadyDecline,
            local_minute_now: 16 * 60,
            high_local_minute: 14 * 60,
            minutes_after_solar_noon: 136,
            month: 7,
            season: Season::Summer,
            observation_count: 30,
            data_age_minutes: 3,
        }
    }

    fn dist(p: &[f64]) -> IncrementDistribution {
        IncrementDistribution {
            probs: p.to_vec(),
            support: 100,
            source: "t".into(),
        }
    }

    #[test]
    fn bucket_probabilities_are_conservative_at_the_tail() {
        let d = dist(&[0.90, 0.07, 0.02, 0.01]); // k=0,1,2,≥3
        let c = TempUnit::Celsius;
        let b18 = TemperatureBucket::exact(18, c);
        assert!((d.p_in_bucket_lower(18, &b18) - 0.90).abs() < 1e-12);
        let b19 = TemperatureBucket::exact(19, c);
        assert!((d.p_in_bucket_upper(18, &b19) - 0.07).abs() < 1e-12);
        // Exact bucket at the tail value: YES lower bound excludes tail mass, NO upper bound includes it.
        let b21 = TemperatureBucket::exact(21, c);
        assert_eq!(d.p_in_bucket_lower(18, &b21), 0.0);
        assert!((d.p_in_bucket_upper(18, &b21) - 0.01).abs() < 1e-12);
        // Open upper bucket "≥ 20": lower bound includes k=2 and the tail.
        let hi = TemperatureBucket::at_or_above(20, c);
        assert!((d.p_in_bucket_lower(18, &hi) - 0.03).abs() < 1e-12);
        // "≤ 18" contains only k=0.
        let lo = TemperatureBucket::at_or_below(18, c);
        assert!((d.p_in_bucket_lower(18, &lo) - 0.90).abs() < 1e-12);
    }

    #[test]
    fn no_edge_model_returns_nothing() {
        assert!(NoEdgeModel.distribution(&features(90, 10)).is_none());
    }

    #[test]
    fn hierarchical_smoothing_shrinks_sparse_cells() {
        let mut m =
            EmpiricalPeakModel::new("t", "EHAM", "all", 4, EmpiricalPeakModel::default_levels());
        // Global: 1000 samples, 80 % final.
        for i in 0..1000 {
            let f = features(if i % 2 == 0 { 30 } else { 150 }, 0);
            m.observe(&f, if i % 5 == 0 { 1 } else { 0 });
        }
        // A sparse, very specific cell with 3 samples all "final".
        for _ in 0..3 {
            m.observe(&features(95, 20), 0);
        }
        let d = m.distribution(&features(95, 20)).unwrap();
        assert!(
            d.probs[0] < 0.97,
            "3 samples cannot justify near-certainty: {}",
            d.probs[0]
        );
        assert!(d.probs[0] > 0.80);
        assert_eq!(d.support, 3);
        let s: f64 = d.probs.iter().sum();
        assert!((s - 1.0).abs() < 1e-9);
        assert_eq!(m.total_samples(), 1003);
    }

    #[test]
    fn increments_beyond_k_go_to_tail() {
        let mut m = EmpiricalPeakModel::new("t", "EHAM", "all", 3, vec![vec![]]);
        m.observe(&features(60, 0), 7);
        m.observe(&features(60, 0), -1);
        assert_eq!(m.cells["global"], vec![1, 0, 1]);
    }

    #[test]
    fn model_serializes() {
        let mut m =
            EmpiricalPeakModel::new("t", "EHAM", "all", 3, EmpiricalPeakModel::default_levels());
        m.observe(&features(60, 0), 0);
        let json = serde_json::to_string(&m).unwrap();
        let back: EmpiricalPeakModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
