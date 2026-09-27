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
use wm_core::forecast::ForecastProduct;
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
    /// The forecast series this model conditions on, if any. The engine
    /// derives the forecast rise only from events of this product, with the
    /// product's knowledge rule; any other forecast is ignored.
    fn forecast_product(&self) -> Option<&ForecastProduct> {
        None
    }
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
    /// Forecast rise bucket ([`rise_bucket`]); unavailable without a forecast.
    ForecastRise,
}

impl FeatureDim {
    /// A refinement dimension can be missing at inference time. Levels that
    /// contain one are skipped when it is missing (the distribution is then
    /// the one of the level above), and model support is counted on the
    /// levels without refinement dimensions.
    pub fn is_refinement(self) -> bool {
        matches!(self, FeatureDim::ForecastRise)
    }
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

/// Forecast rise buckets (tenths °C): strong cooling ahead (≤ −2.5 °C),
/// cooling, flat (within ±0.4 °C) and warming (≥ +0.5 °C).
pub fn rise_bucket(tenths: i32) -> &'static str {
    match tenths {
        i32::MIN..=-25 => "cool2",
        -24..=-5 => "cool",
        -4..=4 => "flat",
        _ => "warm",
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

fn dim_value(dim: FeatureDim, f: &PeakFeatures) -> Option<&'static str> {
    Some(match dim {
        FeatureDim::Season => f.season.as_str(),
        FeatureDim::MinutesSinceHigh => minutes_bucket(f.minutes_since_high),
        FeatureDim::Drop => drop_bucket(f.drop_tenths),
        FeatureDim::LocalHour => hour_bucket(f.local_minute_now),
        FeatureDim::Trajectory => match f.trajectory {
            TrajectoryClass::AtHigh => "at_high",
            TrajectoryClass::SteadyDecline => "decline",
            TrajectoryClass::Oscillating => "osc",
            TrajectoryClass::Insufficient => "insuf",
        },
        FeatureDim::ForecastRise => rise_bucket(f.forecast_rise_tenths?),
    })
}

/// Cell key for a list of dimensions, e.g. `Season=summer|MinutesSinceHigh=m060-089`.
/// `None` when a (refinement) dimension is unavailable for these features.
pub fn cell_key(dims: &[FeatureDim], f: &PeakFeatures) -> Option<String> {
    if dims.is_empty() {
        return Some("global".to_owned());
    }
    let mut parts = Vec::with_capacity(dims.len());
    for d in dims {
        parts.push(format!("{d:?}={}", dim_value(*d, f)?));
    }
    Some(parts.join("|"))
}

/// How a model relates to forecasts; written by training.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastModelInfo {
    /// The forecast series the training history was joined with.
    pub product: ForecastProduct,
    /// `true` when the out-of-sample evaluation showed a clear improvement:
    /// the model then keeps its forecast refinement level and live forecasts
    /// of `product` change its probabilities. `false`: no refinement, so
    /// forecasts cannot influence trading.
    pub adopted: bool,
    /// One-line result of the evaluation (dashboard, logs).
    pub verdict: String,
    /// Training days that had a usable forecast.
    pub days_with_forecast: u64,
    /// `false` when the forecast history could not be obtained at training
    /// (the model then has no refinement; training is retried later).
    #[serde(default)]
    pub evaluated: bool,
    pub evaluated_at: DateTime<Utc>,
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
    /// Forecast product and evaluation result (models trained with forecasts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<ForecastModelInfo>,
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
            forecast: None,
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

    /// `levels` plus a forecast refinement of their most specific level.
    pub fn with_forecast_refinement(mut levels: Vec<Vec<FeatureDim>>) -> Vec<Vec<FeatureDim>> {
        let mut last = levels.last().cloned().unwrap_or_default();
        if !last.contains(&FeatureDim::ForecastRise) {
            last.push(FeatureDim::ForecastRise);
            levels.push(last);
        }
        levels
    }

    /// Whether any level conditions on the forecast.
    pub fn uses_forecast(&self) -> bool {
        self.levels
            .iter()
            .any(|l| l.iter().any(|d| d.is_refinement()))
    }

    /// The same model without refinement levels and their cells: exactly the
    /// distributions it gives when no forecast is available.
    pub fn without_refinements(&self) -> Self {
        let mut m = self.clone();
        m.levels.retain(|l| !l.iter().any(|d| d.is_refinement()));
        let tag = format!("{:?}=", FeatureDim::ForecastRise);
        m.cells
            .retain(|key, _| !key.split('|').any(|part| part.starts_with(&tag)));
        m
    }

    /// Record one training sample (features at time t, observed increment k).
    /// Levels whose refinement dimension is unavailable are not counted.
    pub fn observe(&mut self, f: &PeakFeatures, increment: i32) {
        let k = (increment.max(0) as usize).min(self.k_classes - 1);
        for dims in &self.levels {
            let Some(key) = cell_key(dims, f) else {
                continue;
            };
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

    fn forecast_product(&self) -> Option<&ForecastProduct> {
        self.forecast
            .as_ref()
            .filter(|f| f.adopted && self.uses_forecast())
            .map(|f| &f.product)
    }

    /// Descends the hierarchy while cells exist; each level shrinks toward
    /// its parent. `support` is the sample count of the most specific level
    /// *without* refinement dimensions, so the strategies' support gate means
    /// the same with and without a forecast; a refinement cell with few
    /// samples barely moves the parent's distribution (prior strength α).
    fn distribution(&self, f: &PeakFeatures) -> Option<IncrementDistribution> {
        let mut post: Option<Vec<f64>> = None;
        let mut support = 0u64;
        let mut refined: Option<u64> = None;
        let mut used = String::new();
        for dims in &self.levels {
            let Some(key) = cell_key(dims, f) else {
                break;
            };
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
            if dims.iter().any(|d| d.is_refinement()) {
                refined = Some(n);
            } else {
                support = n;
            }
            used = key;
        }
        post.map(|probs| {
            IncrementDistribution {
                probs,
                support: support.min(u64::from(u32::MAX)) as u32,
                source: match refined {
                    Some(n) => format!("{}:{used} (forecast cell n={n})", self.id),
                    None => format!("{}:{used}", self.id),
                },
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
            forecast_rise_tenths: None,
            high_jump_tenths: Some(5),
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
        assert!(
            !json.contains("forecast"),
            "no forecast block unless trained with one"
        );
    }

    fn with_rise(minutes: i64, drop: i32, rise: Option<i32>) -> PeakFeatures {
        PeakFeatures {
            forecast_rise_tenths: rise,
            ..features(minutes, drop)
        }
    }

    fn refined_model() -> EmpiricalPeakModel {
        let levels =
            EmpiricalPeakModel::with_forecast_refinement(EmpiricalPeakModel::default_levels());
        let mut m = EmpiricalPeakModel::new("t", "EHAM", "all", 4, levels);
        // 400 days without a forecast: 90 % final.
        for i in 0..400 {
            m.observe(&with_rise(90, 10, None), if i % 10 == 0 { 1 } else { 0 });
        }
        // Days with a forecast: cooling ahead → always final; warming → 50 %.
        for _ in 0..200 {
            m.observe(&with_rise(90, 10, Some(-30)), 0);
        }
        for i in 0..200 {
            m.observe(&with_rise(90, 10, Some(12)), i % 2);
        }
        m
    }

    #[test]
    fn forecast_refinement_moves_probabilities_but_not_support() {
        let m = refined_model();
        assert!(m.uses_forecast());
        let base = m.distribution(&with_rise(90, 10, None)).unwrap();
        let cool = m.distribution(&with_rise(90, 10, Some(-30))).unwrap();
        let warm = m.distribution(&with_rise(90, 10, Some(12))).unwrap();
        assert!(cool.probs[0] > base.probs[0] && base.probs[0] > warm.probs[0]);
        assert!(cool.probs[0] > 0.98, "{}", cool.probs[0]);
        assert!(warm.probs[0] < 0.6, "{}", warm.probs[0]);
        // Support is the base cell's count (800 samples), whatever the forecast.
        assert_eq!((base.support, cool.support, warm.support), (800, 800, 800));
        assert!(
            cool.source
                .ends_with("ForecastRise=cool2 (forecast cell n=200)"),
            "{}",
            cool.source
        );
        assert!(!base.source.contains("ForecastRise"));
        // A rise bucket never seen in training falls back to the base cell.
        let flat = m.distribution(&with_rise(90, 10, Some(0))).unwrap();
        assert_eq!(flat, base);
    }

    #[test]
    fn stripping_refinements_gives_the_no_forecast_model() {
        let m = refined_model();
        let stripped = m.without_refinements();
        assert!(!stripped.uses_forecast());
        assert_eq!(stripped.levels, EmpiricalPeakModel::default_levels());
        assert!(stripped.cells.keys().all(|k| !k.contains("ForecastRise")));
        // Same counts as a model trained without the refinement at all.
        let mut plain =
            EmpiricalPeakModel::new("t", "EHAM", "all", 4, EmpiricalPeakModel::default_levels());
        plain.created_at = m.created_at;
        plain.trained_from = m.trained_from;
        plain.trained_to = m.trained_to;
        for i in 0..400 {
            plain.observe(&with_rise(90, 10, None), if i % 10 == 0 { 1 } else { 0 });
        }
        for _ in 0..200 {
            plain.observe(&with_rise(90, 10, Some(-30)), 0);
        }
        for i in 0..200 {
            plain.observe(&with_rise(90, 10, Some(12)), i % 2);
        }
        assert_eq!(stripped, plain);
        // …and every forecast gives the no-forecast distribution.
        let a = stripped
            .distribution(&with_rise(90, 10, Some(-30)))
            .unwrap();
        let b = m.distribution(&with_rise(90, 10, None)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn forecast_product_only_when_adopted_and_refined() {
        let product = ForecastProduct {
            provider: wm_core::ids::ProviderId::open_meteo(),
            model: "gfs_global".into(),
            lead_days: 1,
            ready_local_minute: 480,
        };
        let info = |adopted| ForecastModelInfo {
            product: product.clone(),
            adopted,
            verdict: "test".into(),
            days_with_forecast: 1,
            evaluated: true,
            evaluated_at: Utc::now(),
        };
        let mut m = refined_model();
        assert_eq!(m.forecast_product(), None, "no evaluation recorded");
        m.forecast = Some(info(false));
        assert_eq!(m.forecast_product(), None, "not adopted");
        m.forecast = Some(info(true));
        assert_eq!(m.forecast_product(), Some(&product));
        let mut plain = m.without_refinements();
        plain.forecast = Some(info(true));
        assert_eq!(
            plain.forecast_product(),
            None,
            "no refinement level to use it"
        );
        // Round trip, and older model files (no `forecast` block) still load.
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            serde_json::from_str::<EmpiricalPeakModel>(&json).unwrap(),
            m
        );
        let mut old: serde_json::Value = serde_json::from_str(&json).unwrap();
        old.as_object_mut().unwrap().remove("forecast");
        let back: EmpiricalPeakModel = serde_json::from_value(old).unwrap();
        assert!(back.forecast.is_none());
    }
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;
    use wm_core::market::TempUnit;

    proptest! {
        /// For every distribution and every bucket partition of the integers,
        /// the conservative bounds bracket the truth: each lower ≤ upper,
        /// Σ lower ≤ 1 ≤ Σ upper. (YES uses the lower bound, NO the upper.)
        #[test]
        fn bucket_bounds_bracket_any_distribution(
            raw in prop::collection::vec(0.0f64..1.0, 2..7),
            high in -10i32..35,
            lo in -15i32..25,
            width in 2i32..15,
        ) {
            let total: f64 = raw.iter().sum();
            prop_assume!(total > 1e-6);
            let d = IncrementDistribution { probs: raw.iter().map(|p| p / total).collect(), support: 100, source: "prop".into() };
            let hi = lo + width;
            let mut buckets = vec![TemperatureBucket::at_or_below(lo, TempUnit::Celsius)];
            for v in lo + 1..hi {
                buckets.push(TemperatureBucket::exact(v, TempUnit::Celsius));
            }
            buckets.push(TemperatureBucket::at_or_above(hi, TempUnit::Celsius));
            let (mut sum_lower, mut sum_upper) = (0.0, 0.0);
            for b in &buckets {
                let l = d.p_in_bucket_lower(high, b);
                let u = d.p_in_bucket_upper(high, b);
                prop_assert!((0.0..=1.0).contains(&l) && (0.0..=1.0).contains(&u));
                prop_assert!(l <= u + 1e-12, "lower {l} > upper {u} for {b:?}");
                sum_lower += l;
                sum_upper += u;
            }
            prop_assert!(sum_lower <= 1.0 + 1e-9, "Σ lower = {sum_lower}");
            prop_assert!(sum_upper >= 1.0 - 1e-9, "Σ upper = {sum_upper}");
        }
    }
}
