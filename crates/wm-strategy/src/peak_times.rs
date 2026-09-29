//! When the day's high is first reported — per season, from the station's
//! history.
//!
//! The market settles on the day's highest whole-degree METAR value, so the
//! moment that matters is the first report at that value: from then on the
//! bucket holding the high wins unless a later report beats it. Over the
//! station's history this gives, per meteorological season, the distribution
//! of that moment in local time — and with it the share of days whose high
//! was first reported after any given time.
//!
//! Strategy F ([`crate::peak_slot`]) trades inside a season's *slot*: from
//! one quantile of that distribution to another (by default from the median
//! to the 90th percentile).
//!
//! KNOWN FACT (KMI/IRM, Belgium): maxima come on average about 2–3 hours
//! after solar noon (≈ 14:30 UTC), and in winter the day's highest
//! temperature can come at night after a warm front. That is why the slot
//! is per season and why the distribution — not only its mean — is kept.

use crate::state::ObsPoint;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wm_core::time::Season;

/// Minutes of a local day.
const DAY_MINUTES: u16 = 24 * 60;

/// A day counts only if its reports cover it: the first within this many
/// minutes after local midnight, the last within this many before its end.
const EDGE_MINUTES: u16 = 90;
/// ... and no two consecutive reports further apart than this.
const MAX_GAP_MINUTES: u16 = 120;

/// The distribution of one season's peak time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeasonPeak {
    pub season: Season,
    pub days: u32,
    /// `(local minute of the day, days)` of the first report at the day's
    /// high, ascending by minute.
    pub histogram: Vec<(u16, u32)>,
}

impl SeasonPeak {
    /// Nearest-rank quantile `q` ∈ [0, 1] of the peak time (local minute).
    pub fn quantile(&self, q: f64) -> Option<u16> {
        if self.days == 0 || !q.is_finite() {
            return None;
        }
        let n = u64::from(self.days);
        let rank = ((q.clamp(0.0, 1.0) * n as f64).ceil() as u64).clamp(1, n);
        let mut seen = 0u64;
        for &(minute, count) in &self.histogram {
            seen += u64::from(count);
            if seen >= rank {
                return Some(minute);
            }
        }
        self.histogram.last().map(|(m, _)| *m)
    }

    /// Mean peak time (local minute). Night maxima pull it earlier; the
    /// median is the robust middle.
    pub fn mean(&self) -> Option<f64> {
        if self.days == 0 {
            return None;
        }
        let sum: u64 = self
            .histogram
            .iter()
            .map(|&(m, c)| u64::from(m) * u64::from(c))
            .sum();
        Some(sum as f64 / f64::from(self.days))
    }

    /// Share of days whose high was first reported after `minute`.
    pub fn share_after(&self, minute: u16) -> f64 {
        if self.days == 0 {
            return 0.0;
        }
        let later: u64 = self
            .histogram
            .iter()
            .filter(|(m, _)| *m > minute)
            .map(|(_, c)| u64::from(*c))
            .sum();
        later as f64 / f64::from(self.days)
    }
}

/// Seasons in calendar order for reports.
const SEASONS: [Season; 4] = [
    Season::Winter,
    Season::Spring,
    Season::Summer,
    Season::Autumn,
];

/// Peak times of every season with history.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeakTimes {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub seasons: Vec<SeasonPeak>,
    /// Days left out because their reports did not cover them.
    #[serde(default)]
    pub incomplete_days: u32,
}

impl PeakTimes {
    pub fn season(&self, s: Season) -> Option<&SeasonPeak> {
        self.seasons.iter().find(|p| p.season == s)
    }

    /// A season's slot `[start, end)` in local minutes: from quantile
    /// `from_q` to quantile `to_q` of its peak time. `None` without history
    /// for the season or when the quantiles leave no time between them.
    pub fn slot(&self, s: Season, from_q: f64, to_q: f64) -> Option<(u16, u16)> {
        let p = self.season(s)?;
        let start = p.quantile(from_q)?;
        // The end is exclusive: the minute of the `to_q` quantile is inside.
        let end = p.quantile(to_q)?.saturating_add(1).min(DAY_MINUTES);
        (start < end).then_some((start, end))
    }

    /// The table of the training and research reports: per season the
    /// mean and quantiles of the peak time, strategy F's slot between
    /// `from_q` and `to_q`, and how often the high came late or at night.
    pub fn to_markdown(&self, from_q: f64, to_q: f64) -> String {
        let days: u32 = self.seasons.iter().map(|s| s.days).sum();
        let mut md = format!(
            "\n## When the day's high is first reported (strategy F)\n\n{days} complete days{}{}. Local time of the first report at the day's whole-degree METAR high — the moment from which the bucket holding the high wins unless a later report beats it. Strategy F trades inside each season's slot, from the {} to the {} of these times; *later than* columns give the share of days whose high was first reported after the slot or after a clock time.\n\n| season | days | mean | 10% | 25% | median | 75% | 90% | slot | later than the slot | later than 17:00 | before 09:00 |\n|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|\n",
            match (self.from, self.to) {
                (Some(f), Some(t)) => format!(", {f} → {t}"),
                _ => String::new(),
            },
            if self.incomplete_days > 0 {
                format!(
                    " ({} days whose reports did not cover them left out)",
                    self.incomplete_days
                )
            } else {
                String::new()
            },
            quantile_label(from_q),
            quantile_label(to_q)
        );
        for season in SEASONS {
            let Some(p) = self.season(season) else {
                continue;
            };
            let q = |x: f64| p.quantile(x).map_or_else(|| "–".to_owned(), hm);
            let pct = |x: f64| format!("{:.0}%", 100.0 * x);
            let slot = self.slot(season, from_q, to_q);
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                season.as_str(),
                p.days,
                p.mean()
                    .map_or_else(|| "–".to_owned(), |m| hm(m.round() as u16)),
                q(0.10),
                q(0.25),
                q(0.50),
                q(0.75),
                q(0.90),
                slot.map_or_else(|| "–".to_owned(), |(a, b)| format!("{}–{}", hm(a), hm(b))),
                slot.map_or_else(
                    || "–".to_owned(),
                    |(_, b)| pct(p.share_after(b.saturating_sub(1)))
                ),
                pct(p.share_after(17 * 60 - 1)),
                pct(1.0 - p.share_after(9 * 60 - 1)),
            ));
        }
        md
    }
}

/// "the median", "the 90th percentile".
fn quantile_label(q: f64) -> String {
    let pct = (100.0 * q).round() as u32;
    match pct {
        50 => "median".into(),
        0 => "earliest".into(),
        100 => "latest".into(),
        p => format!(
            "{p}{} percentile",
            match (p % 10, p % 100) {
                (1, x) if x != 11 => "st",
                (2, x) if x != 12 => "nd",
                (3, x) if x != 13 => "rd",
                _ => "th",
            }
        ),
    }
}

/// Collects one peak time per complete day.
#[derive(Debug, Clone, Default)]
pub struct PeakTimesBuilder {
    hist: BTreeMap<Season, BTreeMap<u16, u32>>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    incomplete: u32,
}

impl PeakTimesBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a day from its reports (oldest first) and its final whole-degree
    /// high. Days the reports do not cover are counted and left out.
    pub fn add_day(&mut self, date: NaiveDate, season: Season, points: &[ObsPoint], high: i32) {
        match first_reach_minute(points, high) {
            Some(minute) => {
                *self
                    .hist
                    .entry(season)
                    .or_default()
                    .entry(minute)
                    .or_insert(0) += 1;
                self.from = Some(self.from.map_or(date, |f| f.min(date)));
                self.to = Some(self.to.map_or(date, |t| t.max(date)));
            }
            None => self.incomplete += 1,
        }
    }

    /// Complete days so far.
    pub fn days(&self) -> u32 {
        self.hist.values().flat_map(|h| h.values()).sum()
    }

    pub fn build(&self) -> PeakTimes {
        PeakTimes {
            from: self.from,
            to: self.to,
            seasons: self
                .hist
                .iter()
                .map(|(season, h)| SeasonPeak {
                    season: *season,
                    days: h.values().sum(),
                    histogram: h.iter().map(|(m, c)| (*m, *c)).collect(),
                })
                .collect(),
            incomplete_days: self.incomplete,
        }
    }
}

/// Local minute of the first report at the day's whole-degree high `high`,
/// if the reports (oldest first) cover the day.
pub fn first_reach_minute(points: &[ObsPoint], high: i32) -> Option<u16> {
    let (first, last) = (points.first()?, points.last()?);
    if first.local_minute_of_day > EDGE_MINUTES
        || last.local_minute_of_day < DAY_MINUTES - EDGE_MINUTES
    {
        return None;
    }
    if points
        .windows(2)
        .any(|w| (w[1].observed_at - w[0].observed_at).num_minutes() > i64::from(MAX_GAP_MINUTES))
    {
        return None;
    }
    points
        .iter()
        .find(|p| p.temp.round_half_up_whole() == high)
        .map(|p| p.local_minute_of_day)
}

/// `HH:MM` of a local minute.
pub fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use wm_core::units::TempC;
    use wm_core::weather::ReportType;

    /// Half-hourly reports of a summer day (CEST = UTC+2) from 00:25 local,
    /// with the given whole-degree temperatures.
    fn day(temps: &[i32]) -> Vec<ObsPoint> {
        let start: DateTime<Utc> = "2026-07-01T22:25:00Z".parse().unwrap();
        temps
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let at = start + Duration::minutes(30 * i as i64);
                let minute = (25 + 30 * i as u16) % DAY_MINUTES;
                ObsPoint {
                    observed_at: at,
                    local_minute_of_day: minute,
                    local_minute_of_hour: (minute % 60) as u8,
                    temp: TempC::from_whole(*t),
                    report_type: ReportType::Metar,
                    version: 1,
                }
            })
            .collect()
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap()
    }

    #[test]
    fn the_first_report_at_the_whole_degree_high_is_the_peak_time() {
        // 48 reports, 00:25 … 23:55, rising to 21 °C: first at 14:55
        // (index 29), again at 15:25 and 15:55.
        let mut temps = vec![15; 48];
        for (i, t) in temps.iter_mut().enumerate().take(32).skip(20) {
            *t = 18 + (i as i32 - 20) / 3;
        }
        let points = day(&temps);
        assert_eq!(temps.iter().max(), Some(&21));
        assert_eq!(first_reach_minute(&points, 21), Some(14 * 60 + 55));
        // A day whose reports stop early is not complete: no peak time.
        assert_eq!(first_reach_minute(&points[..40], 21), None);
        // A two-and-a-half-hour hole: not complete either.
        let mut holed = points.clone();
        holed.drain(10..15);
        assert_eq!(first_reach_minute(&holed, 21), None);
        // A high the reports never show (inconsistent input): none.
        assert_eq!(first_reach_minute(&points, 30), None);
    }

    #[test]
    fn quantiles_mean_and_slots_come_from_the_histogram() {
        let mut b = PeakTimesBuilder::new();
        // Ten summer days peaking at 13:55 (1), 14:25 (2), 14:55 (3),
        // 15:25 (2), 16:55 (1) and one night maximum at 00:25.
        let peaks = [
            (13, 55, 1),
            (14, 25, 2),
            (14, 55, 3),
            (15, 25, 2),
            (16, 55, 1),
            (0, 25, 1),
        ];
        for (h, m, n) in peaks {
            for _ in 0..n {
                let mut temps = vec![15; 48];
                let idx = ((h * 60 + m - 25) / 30) as usize;
                temps[idx] = 25;
                b.add_day(date(), Season::Summer, &day(&temps), 25);
            }
        }
        // An incomplete day is counted separately.
        b.add_day(date(), Season::Summer, &day(&[15; 30]), 15);
        let pt = b.build();
        assert_eq!(pt.incomplete_days, 1);
        let s = pt.season(Season::Summer).unwrap();
        assert_eq!(s.days, 10);
        assert_eq!(s.quantile(0.0), Some(25), "the night maximum");
        assert_eq!(s.quantile(0.5), Some(14 * 60 + 55));
        assert_eq!(s.quantile(0.9), Some(15 * 60 + 25));
        assert_eq!(s.quantile(1.0), Some(16 * 60 + 55));
        let mean = s.mean().unwrap();
        let expected = (13.0 * 60.0
            + 55.0
            + 2.0 * (14.0 * 60.0 + 25.0)
            + 3.0 * (14.0 * 60.0 + 55.0)
            + 2.0 * (15.0 * 60.0 + 25.0)
            + 16.0 * 60.0
            + 55.0
            + 25.0)
            / 10.0;
        assert!((mean - expected).abs() < 1e-9);
        assert!((s.share_after(15 * 60) - 0.3).abs() < 1e-12);
        assert_eq!(
            pt.slot(Season::Summer, 0.5, 0.9),
            Some((14 * 60 + 55, 15 * 60 + 26)),
            "the end quantile's minute is inside"
        );
        assert_eq!(pt.slot(Season::Winter, 0.5, 0.9), None, "no winter history");
        assert_eq!(pt.slot(Season::Summer, 0.9, 0.5), None, "empty slot");
        assert_eq!(hm(14 * 60 + 55), "14:55");
    }

    #[test]
    fn the_report_table_shows_quantiles_slot_and_late_shares() {
        let mut b = PeakTimesBuilder::new();
        // Four summer days: 14:25, 14:55, 16:55 and a night maximum at 00:25.
        for idx in [28usize, 29, 33, 0] {
            let mut temps = vec![15; 48];
            temps[idx] = 25;
            b.add_day(date(), Season::Summer, &day(&temps), 25);
        }
        let md = b.build().to_markdown(0.5, 0.9);
        assert!(
            md.contains("4 complete days, 2026-07-02 → 2026-07-02."),
            "{md}"
        );
        assert!(
            md.contains("from the median to the 90th percentile"),
            "{md}"
        );
        // Mean of 865, 895, 1015 and 25 minutes = 700 = 11:40; slot 14:25–16:56.
        assert!(
            md.contains("| summer | 4 | 11:40 | 00:25 | 00:25 | 14:25 | 14:55 | 16:55 | 14:25–16:56 | 0% | 0% | 25% |"),
            "{md}"
        );
        assert!(
            !md.contains("| winter"),
            "seasons without history are left out"
        );
        assert_eq!(quantile_label(0.9), "90th percentile");
        assert_eq!(quantile_label(0.75), "75th percentile");
        assert_eq!(quantile_label(0.21), "21st percentile");
        assert_eq!(quantile_label(0.11), "11th percentile");
    }

    #[test]
    fn peak_times_survive_serialization() {
        let mut b = PeakTimesBuilder::new();
        let mut temps = vec![10; 48];
        temps[27] = 12;
        b.add_day(date(), Season::Autumn, &day(&temps), 12);
        let pt = b.build();
        let json = serde_json::to_string(&pt).unwrap();
        let back: PeakTimes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, pt);
        assert_eq!(back.from, Some(date()));
    }
}
