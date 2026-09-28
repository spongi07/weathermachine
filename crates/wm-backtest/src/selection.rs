//! Which model structure predicts better? — measured walk-forward, like the
//! forecast evaluation.
//!
//! The replay of 28 September 2026 (docs/research/replay-2026-09-28.md)
//! showed two structural weaknesses of the current cells: the clock restarts
//! whenever a report repeats the high (a plateau looks like a new high), and
//! every hour before noon shares one cell (a high set at 11:55 was treated
//! like one set at 07:25). The candidate structure
//! ([`ModelStructure::Candidate`]) counts from the first time the high was
//! reached, splits the morning per hour and reads the forecast as headroom
//! above the observed high. Both structures were fixed before any result was
//! seen; nothing is tuned on the comparison.
//!
//! Both are trained in the same prequential pass: every report of day D is
//! predicted by each structure as trained on the days before D. The scored
//! reports are those within the trading hours (default 10:00–18:00 local)
//! after a burn-in, and each structure predicts with its forecast input only
//! when its own forecast evaluation (placebo-controlled) adopted it — i.e.
//! exactly as the service would use it.
//!
//! Adoption rule: at least `min_days` scored days and a 95 % day-block
//! bootstrap interval of the change in multi-class log loss per report
//! (candidate − current) entirely below zero. Otherwise the current
//! structure stays.

use crate::forecast_eval::{logloss, p_final, ratio_ci};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use wm_strategy::probability::hour_bucket_fine;
use wm_strategy::{EmpiricalPeakModel, ModelStructure, PeakFeatures, ProbabilityModel};

/// Comparison settings (engineering choices, fixed before evaluation).
#[derive(Debug, Clone)]
pub struct SelectionConfig {
    /// Reports scored: local time within `[from, to)` minutes, the hours in
    /// which the strategies trade.
    pub from_local_minute: u16,
    pub to_local_minute: u16,
    /// The first days of the history only train both structures. The
    /// service's model is trained on many years; data-starved early
    /// predictions are not what the comparison is about.
    pub burn_in_days: u64,
    /// Adoption needs at least this many scored days.
    pub min_days: u64,
    pub bootstrap_iterations: usize,
    pub seed: u64,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            from_local_minute: 10 * 60,
            to_local_minute: 18 * 60,
            burn_in_days: 365,
            min_days: 365,
            bootstrap_iterations: 2_000,
            seed: 0x5E1E_C7ED,
        }
    }
}

/// Log loss of both structures on a group of reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructureRow {
    pub group: String,
    pub points: u64,
    pub logloss_current: f64,
    pub logloss_candidate: f64,
    /// Mean predicted P(high is final) and the observed final rate.
    pub mean_p_current: f64,
    pub mean_p_candidate: f64,
    pub final_rate: f64,
}

/// Result of the structure comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructureComparison {
    pub current: String,
    pub candidate: String,
    /// Whether each side predicted with its (adopted) forecast input.
    pub current_forecast: bool,
    pub candidate_forecast: bool,
    pub from_local_minute: u16,
    pub to_local_minute: u16,
    pub burn_in_days: u64,
    pub scored_days: u64,
    pub points: u64,
    pub first_scored: Option<NaiveDate>,
    pub last_scored: Option<NaiveDate>,
    /// Mean multi-class log loss per report (lower is better).
    pub logloss_current: f64,
    pub logloss_candidate: f64,
    /// Candidate minus current, with its 95 % day-block bootstrap interval.
    pub diff: f64,
    pub diff_ci_low: f64,
    pub diff_ci_high: f64,
    /// Brier score of P(high is final).
    pub brier_current: f64,
    pub brier_candidate: f64,
    /// By local hour, and by whether the high had been repeated.
    pub rows: Vec<StructureRow>,
    pub min_days: u64,
    pub adopted: bool,
    pub verdict: String,
}

/// Log loss of the observed class and P(high is final) of one prediction.
#[derive(Debug, Clone, Copy)]
struct Pred {
    ll: f64,
    p_final: f64,
}

impl Pred {
    fn of(probs: &[f64], k: usize) -> Self {
        Self {
            ll: logloss(probs, k),
            p_final: p_final(probs),
        }
    }
}

/// One scored report: each structure's prediction without and with its
/// forecast input (the latter only when the model has a refinement level).
/// Only the scores are kept, not the distributions: a long history has
/// over a hundred thousand of these.
#[derive(Debug, Clone, Copy)]
struct Point {
    date: NaiveDate,
    is_final: bool,
    hour: &'static str,
    retested: bool,
    current: Pred,
    current_fc: Option<Pred>,
    candidate: Pred,
    candidate_fc: Option<Pred>,
}

/// A report as compared: the predictions the service would have used.
#[derive(Debug, Clone, Copy)]
struct Scored {
    date: NaiveDate,
    is_final: bool,
    hour: &'static str,
    retested: bool,
    cur: Pred,
    cand: Pred,
}

/// Collects reports during the prequential pass.
#[derive(Debug)]
pub(crate) struct Comparer {
    cfg: SelectionConfig,
    points: Vec<Point>,
}

impl Comparer {
    pub(crate) fn new(cfg: SelectionConfig) -> Self {
        Self {
            cfg,
            points: Vec::new(),
        }
    }

    /// Whether a day (1-based count of days with a high) is past the burn-in.
    pub(crate) fn scores_day(&self, day_number: u64) -> bool {
        day_number > self.cfg.burn_in_days
    }

    pub(crate) fn in_window(&self, f: &PeakFeatures) -> bool {
        (self.cfg.from_local_minute..self.cfg.to_local_minute).contains(&f.local_minute_now)
    }

    /// Score one report with both models as trained on earlier days only.
    pub(crate) fn score(
        &mut self,
        date: NaiveDate,
        current: &EmpiricalPeakModel,
        candidate: &EmpiricalPeakModel,
        f: &PeakFeatures,
        increment: i32,
    ) {
        let plain = f.without_forecast();
        let (Some(c), Some(d)) = (current.distribution(&plain), candidate.distribution(&plain))
        else {
            return;
        };
        let k = (increment.max(0) as usize).min(c.probs.len().min(d.probs.len()).saturating_sub(1));
        let refined = |m: &EmpiricalPeakModel| {
            m.uses_forecast()
                .then(|| m.distribution(f))
                .flatten()
                .map(|x| Pred::of(&x.probs, k))
        };
        self.points.push(Point {
            date,
            is_final: k == 0,
            hour: hour_bucket_fine(f.local_minute_now),
            retested: f.retests > 0,
            current: Pred::of(&c.probs, k),
            current_fc: refined(current),
            candidate: Pred::of(&d.probs, k),
            candidate_fc: refined(candidate),
        });
    }

    /// Compare the predictions the service would make: with the forecast
    /// input for a structure whose forecast evaluation adopted it.
    pub(crate) fn finish(
        self,
        current_forecast: bool,
        candidate_forecast: bool,
    ) -> StructureComparison {
        let cfg = &self.cfg;
        let pick = |base: Pred, fc: Option<Pred>, use_fc: bool| match fc {
            Some(v) if use_fc => v,
            _ => base,
        };
        let scored: Vec<Scored> = self
            .points
            .iter()
            .map(|p| Scored {
                date: p.date,
                is_final: p.is_final,
                hour: p.hour,
                retested: p.retested,
                cur: pick(p.current, p.current_fc, current_forecast),
                cand: pick(p.candidate, p.candidate_fc, candidate_forecast),
            })
            .collect();
        let mut per_day: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
        let (mut ll_c, mut ll_d, mut br_c, mut br_d) = (0.0, 0.0, 0.0, 0.0);
        for s in &scored {
            ll_c += s.cur.ll;
            ll_d += s.cand.ll;
            let y = if s.is_final { 1.0 } else { 0.0 };
            br_c += (s.cur.p_final - y).powi(2);
            br_d += (s.cand.p_final - y).powi(2);
            let e = per_day.entry(s.date).or_insert((0.0, 0.0));
            e.0 += s.cand.ll - s.cur.ll;
            e.1 += 1.0;
        }
        let n = scored.len() as f64;
        let mean = |x: f64| if n > 0.0 { x / n } else { 0.0 };
        let sums: Vec<(f64, f64)> = per_day.values().copied().collect();
        let (lo, hi) = ratio_ci(&sums, cfg.bootstrap_iterations, cfg.seed);
        let scored_days = per_day.len() as u64;
        let diff = mean(ll_d - ll_c);
        let enough = scored_days >= cfg.min_days && n > 0.0;
        let adopted = enough && hi < 0.0;
        let verdict = if !enough {
            format!(
                "current structure kept: {scored_days} scored days, {} needed",
                cfg.min_days
            )
        } else if adopted {
            format!(
                "candidate structure adopted: log loss {diff:+.4} per report (95% CI {lo:+.4} … {hi:+.4}) over {scored_days} days, {} reports",
                scored.len()
            )
        } else {
            format!(
                "current structure kept: no clear improvement — log loss {diff:+.4} per report (95% CI {lo:+.4} … {hi:+.4}) over {scored_days} days"
            )
        };

        let mut rows = Vec::new();
        let mut group = |name: String, members: Vec<&Scored>| {
            if members.is_empty() {
                return;
            }
            let m = members.len() as f64;
            let avg = |f: &dyn Fn(&Scored) -> f64| members.iter().map(|s| f(s)).sum::<f64>() / m;
            rows.push(StructureRow {
                group: name,
                points: members.len() as u64,
                logloss_current: avg(&|s| s.cur.ll),
                logloss_candidate: avg(&|s| s.cand.ll),
                mean_p_current: avg(&|s| s.cur.p_final),
                mean_p_candidate: avg(&|s| s.cand.p_final),
                final_rate: avg(&|s| if s.is_final { 1.0 } else { 0.0 }),
            });
        };
        for h in [
            "h<09", "h09", "h10", "h11", "h12-13", "h14-15", "h16-17", "h18+",
        ] {
            group(
                format!("hour {h}"),
                scored.iter().filter(|s| s.hour == h).collect(),
            );
        }
        group(
            "high not repeated".to_owned(),
            scored.iter().filter(|s| !s.retested).collect(),
        );
        group(
            "high repeated (plateau)".to_owned(),
            scored.iter().filter(|s| s.retested).collect(),
        );

        StructureComparison {
            current: ModelStructure::Current.label().to_owned(),
            candidate: ModelStructure::Candidate.label().to_owned(),
            current_forecast,
            candidate_forecast,
            from_local_minute: cfg.from_local_minute,
            to_local_minute: cfg.to_local_minute,
            burn_in_days: cfg.burn_in_days,
            scored_days,
            points: scored.len() as u64,
            first_scored: per_day.keys().next().copied(),
            last_scored: per_day.keys().next_back().copied(),
            logloss_current: mean(ll_c),
            logloss_candidate: mean(ll_d),
            diff,
            diff_ci_low: lo,
            diff_ci_high: hi,
            brier_current: mean(br_c),
            brier_candidate: mean(br_d),
            rows,
            min_days: cfg.min_days,
            adopted,
            verdict,
        }
    }
}

impl StructureComparison {
    pub fn to_markdown(&self) -> String {
        let hm = |m: u16| format!("{:02}:{:02}", m / 60, m % 60);
        let fc = |b: bool| {
            if b {
                "with its forecast input"
            } else {
                "without forecast"
            }
        };
        let mut s = format!(
            "\n## Model structure — walk-forward comparison\n\n**Verdict: {}**\n\nBoth structures are trained in one prequential pass; every report between {} and {} local time is predicted by each as trained on earlier days only (after a burn-in of {} days).\n\n* Current structure: {} — predicting {}.\n* Candidate structure: {} — predicting {}.\n\nAdoption needs ≥ {} scored days and a log-loss interval entirely below zero.\n\n",
            self.verdict,
            hm(self.from_local_minute),
            hm(self.to_local_minute),
            self.burn_in_days,
            self.current,
            fc(self.current_forecast),
            self.candidate,
            fc(self.candidate_forecast),
            self.min_days
        );
        let _ = writeln!(
            s,
            "Scored days: {} ({} → {}) · reports: {}\n\n| metric | current | candidate | change (95 % CI) |\n|---|---:|---:|---|\n| log loss per report | {:.4} | {:.4} | {:+.4} ({:+.4} … {:+.4}) |\n| Brier score of P(final) | {:.4} | {:.4} | {:+.4} |\n",
            self.scored_days,
            self.first_scored.map(|d| d.to_string()).unwrap_or_default(),
            self.last_scored.map(|d| d.to_string()).unwrap_or_default(),
            self.points,
            self.logloss_current,
            self.logloss_candidate,
            self.diff,
            self.diff_ci_low,
            self.diff_ci_high,
            self.brier_current,
            self.brier_candidate,
            self.brier_candidate - self.brier_current
        );
        s.push_str("| reports | n | log loss current | log loss candidate | mean P(final) current | candidate | observed final rate |\n|---|---:|---:|---:|---:|---:|---:|\n");
        for r in &self.rows {
            let _ = writeln!(
                s,
                "| {} | {} | {:.4} | {:.4} | {:.3} | {:.3} | {:.3} |",
                r.group,
                r.points,
                r.logloss_current,
                r.logloss_candidate,
                r.mean_p_current,
                r.mean_p_candidate,
                r.final_rate
            );
        }
        s
    }
}
