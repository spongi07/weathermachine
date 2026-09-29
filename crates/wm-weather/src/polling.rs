//! Adaptive, schedule-aware polling policy (pure).
//!
//! HTTP polling frequency is not observation frequency. EHAM issues routine
//! METARs at HH:25 and HH:55 (Dutch AIP GEN 3.5 — verify in Phase 0), so
//! polling every 30 s all day would waste ~98 % of requests. Instead:
//!
//! * inside an **arrival window** after each expected report, poll at the
//!   mode's `window_interval` (a few quick polls at most), then at the slower
//!   `late_interval` until the window closes, so a late report is still seen
//!   within a minute or two instead of at the next background poll;
//! * outside windows, poll slowly (`background_interval`) to catch SPECIs;
//! * when a report is overdue, do **not** speed up — slow down after a while;
//! * throttling or gate closure always wins (never poll faster than the gate).

use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use wm_core::health::ProviderHealthState;
use wm_core::time::local_minute_of_day;

/// Expected reporting cadence of a station.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CadenceModel {
    /// Minutes past the hour of routine reports (UTC minutes; EHAM: 25, 55).
    pub routine_minutes: Vec<u8>,
    /// Earliest expected availability after the nominal report time.
    pub first_poll_delay_secs: i64,
    /// Length of the arrival window after the nominal time.
    pub arrival_window_secs: i64,
}

impl CadenceModel {
    pub fn eham() -> Self {
        Self {
            routine_minutes: vec![25, 55],
            first_poll_delay_secs: 90,
            arrival_window_secs: 12 * 60,
        }
    }

    /// Next nominal routine report time strictly after `after`.
    pub fn next_report_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.routine_minutes.is_empty() {
            return None;
        }
        let base = after.with_second(0)?.with_nanosecond(0)?;
        let hour_start = base.with_minute(0)?;
        for h in 0..=2 {
            let hs = hour_start + Duration::hours(h);
            let mut mins: Vec<u8> = self.routine_minutes.clone();
            mins.sort_unstable();
            for m in mins {
                let t = hs + Duration::minutes(i64::from(m));
                if t > after {
                    return Some(t);
                }
            }
        }
        None
    }

    /// Nominal interval between routine reports.
    pub fn nominal_interval(&self) -> Duration {
        match self.routine_minutes.len() {
            0 => Duration::hours(1),
            n => Duration::minutes(60 / n as i64),
        }
    }
}

/// Intervals for one polling mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeParams {
    /// Spacing of the quick polls that open each arrival window.
    pub window_interval_secs: i64,
    /// Spacing once the quick polls are used up, until the window closes.
    pub late_interval_secs: i64,
    pub background_interval_secs: i64,
    /// Number of quick polls per window.
    pub max_polls_per_window: u32,
}

/// Polling configuration (engineering starting points, not optimized values).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollingParams {
    pub low: ModeParams,
    pub normal: ModeParams,
    pub peak: ModeParams,
    /// Local night window (minutes after midnight) during which, absent
    /// exposure, the low mode is used.
    pub night_start_minute: u16,
    pub night_end_minute: u16,
    /// After a report is overdue by this long, poll only every `stale_interval`.
    pub stale_slowdown_after_secs: i64,
    pub stale_interval_secs: i64,
    /// While a window poll finds the expected report missing, ask the standby
    /// source (TGFTP) as well, whenever its own gate admits a request.
    pub poll_standby_in_window: bool,
}

impl Default for PollingParams {
    fn default() -> Self {
        Self {
            low: ModeParams {
                window_interval_secs: 120,
                late_interval_secs: 180,
                background_interval_secs: 15 * 60,
                max_polls_per_window: 3,
            },
            normal: ModeParams {
                window_interval_secs: 60,
                late_interval_secs: 90,
                background_interval_secs: 10 * 60,
                max_polls_per_window: 6,
            },
            peak: ModeParams {
                window_interval_secs: 30,
                late_interval_secs: 60,
                background_interval_secs: 5 * 60,
                max_polls_per_window: 10,
            },
            night_start_minute: 22 * 60,
            night_end_minute: 5 * 60,
            stale_slowdown_after_secs: 2 * 3600,
            stale_interval_secs: 20 * 60,
            poll_standby_in_window: true,
        }
    }
}

/// Hints published by the engine (never by strategies directly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingHints {
    /// A possible daily peak is forming (engine judgement).
    pub peak_watch: bool,
    /// Weather Machine holds exposure depending on this station.
    pub has_exposure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollingMode {
    Low,
    Normal,
    Peak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollReason {
    Initial,
    ArrivalWindow,
    /// Quick polls used up, window still open: slower polls until it closes.
    LateWindow,
    Background,
    Overdue,
    StaleSlowdown,
    GateBlocked,
}

/// Inputs to one scheduling decision.
#[derive(Debug, Clone)]
pub struct PollingInputs {
    pub now: DateTime<Utc>,
    pub tz: Tz,
    pub last_observation_time: Option<DateTime<Utc>>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub polls_in_current_window: u32,
    pub hints: PollingHints,
    pub health: ProviderHealthState,
    /// Earliest instant the provider gate will admit a request.
    pub gate_not_before: Option<DateTime<Utc>>,
    /// Throttled within the politeness period.
    pub throttled_recently: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollDecision {
    pub at: DateTime<Utc>,
    pub mode: PollingMode,
    pub reason: PollReason,
    pub in_window: bool,
    /// Nominal time of the report being waited for.
    pub expected_report: Option<DateTime<Utc>>,
}

/// The policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollingPolicy {
    pub cadence: CadenceModel,
    pub params: PollingParams,
}

impl PollingPolicy {
    pub fn new(cadence: CadenceModel, params: PollingParams) -> Self {
        Self { cadence, params }
    }

    pub fn mode(&self, i: &PollingInputs) -> PollingMode {
        let minute = local_minute_of_day(i.now, i.tz);
        let (s, e) = (self.params.night_start_minute, self.params.night_end_minute);
        let night = if s <= e {
            minute >= s && minute < e
        } else {
            minute >= s || minute < e
        };
        let mut mode = if i.hints.has_exposure || i.hints.peak_watch {
            PollingMode::Peak
        } else if night {
            PollingMode::Low
        } else {
            PollingMode::Normal
        };
        // Previous throttling: step down one level for the politeness period.
        if i.throttled_recently {
            mode = match mode {
                PollingMode::Peak => PollingMode::Normal,
                _ => PollingMode::Low,
            };
        }
        mode
    }

    fn params_for(&self, mode: PollingMode) -> &ModeParams {
        match mode {
            PollingMode::Low => &self.params.low,
            PollingMode::Normal => &self.params.normal,
            PollingMode::Peak => &self.params.peak,
        }
    }

    /// When should the next request be made?
    pub fn next_poll(&self, i: &PollingInputs) -> PollDecision {
        let mode = self.mode(i);
        let p = self.params_for(mode);
        let window_iv = Duration::seconds(p.window_interval_secs);
        let late_iv = Duration::seconds(p.late_interval_secs);
        let background_iv = Duration::seconds(p.background_interval_secs);

        let window = Duration::seconds(self.cadence.arrival_window_secs);
        // The report we are waiting for: the one after the newest observation,
        // unless its window has already closed (report missed), in which case
        // we wait for the next scheduled report instead.
        let expected = match i
            .last_observation_time
            .and_then(|t| self.cadence.next_report_after(t))
        {
            Some(e) if i.now <= e + window => Some(e),
            _ => self.cadence.next_report_after(i.now - window),
        };
        let long_outage = i
            .last_observation_time
            .is_some_and(|t| i.now - t > Duration::seconds(self.params.stale_slowdown_after_secs));

        let (mut at, mut reason, mut in_window) = match (i.last_poll_at, expected) {
            (None, _) => (i.now, PollReason::Initial, false),
            (Some(last), _) if long_outage => {
                // Never hammer a broken feed: poll rarely until data returns.
                (
                    last + Duration::seconds(self.params.stale_interval_secs),
                    PollReason::StaleSlowdown,
                    false,
                )
            }
            (Some(last), None) => (last + background_iv, PollReason::Background, false),
            (Some(last), Some(exp)) => {
                let w_start = exp + Duration::seconds(self.cadence.first_poll_delay_secs);
                let w_end = exp + window;
                if i.now < w_start {
                    let bg = last + background_iv;
                    if bg < w_start {
                        (bg, PollReason::Background, false)
                    } else {
                        (w_start, PollReason::ArrivalWindow, true)
                    }
                } else if i.now <= w_end && i.polls_in_current_window < p.max_polls_per_window {
                    (
                        (last + window_iv).max(w_start),
                        PollReason::ArrivalWindow,
                        true,
                    )
                } else if i.now <= w_end && last + late_iv <= w_end {
                    // Quick polls used up but the window is still open: keep
                    // looking, more slowly, until it closes.
                    (last + late_iv, PollReason::LateWindow, true)
                } else {
                    // Window closed without the report: fall back to the
                    // background cadence (never faster).
                    (last + background_iv, PollReason::Overdue, false)
                }
            }
        };

        if let Some(gnb) = i.gate_not_before
            && gnb > at
        {
            at = gnb;
            reason = PollReason::GateBlocked;
            in_window = false;
        }
        if at < i.now {
            at = i.now;
        }
        PollDecision {
            at,
            mode,
            reason,
            in_window,
            expected_report: expected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn inputs(now: &str, last_obs: Option<&str>, last_poll: Option<&str>) -> PollingInputs {
        PollingInputs {
            now: utc(now),
            tz: Amsterdam,
            last_observation_time: last_obs.map(utc),
            last_poll_at: last_poll.map(utc),
            polls_in_current_window: 0,
            hints: PollingHints::default(),
            health: ProviderHealthState::Healthy,
            gate_not_before: None,
            throttled_recently: false,
        }
    }

    fn policy() -> PollingPolicy {
        PollingPolicy::new(CadenceModel::eham(), PollingParams::default())
    }

    #[test]
    fn next_report_times() {
        let c = CadenceModel::eham();
        assert_eq!(
            c.next_report_after(utc("2026-09-26T12:55:00Z")),
            Some(utc("2026-09-26T13:25:00Z"))
        );
        assert_eq!(
            c.next_report_after(utc("2026-09-26T13:25:00Z")),
            Some(utc("2026-09-26T13:55:00Z"))
        );
        assert_eq!(
            c.next_report_after(utc("2026-09-26T23:56:00Z")),
            Some(utc("2026-09-27T00:25:00Z"))
        );
        assert_eq!(c.nominal_interval(), Duration::minutes(30));
    }

    #[test]
    fn initial_poll_is_immediate() {
        let d = policy().next_poll(&inputs("2026-09-26T12:00:00Z", None, None));
        assert_eq!(d.reason, PollReason::Initial);
        assert_eq!(d.at, utc("2026-09-26T12:00:00Z"));
    }

    #[test]
    fn waits_for_arrival_window_instead_of_hammering() {
        // Last obs 12:55, last poll 12:58, now 13:00 (local 15:00 CEST, normal mode).
        let d = policy().next_poll(&inputs(
            "2026-09-26T13:00:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T12:58:00Z"),
        ));
        assert_eq!(d.mode, PollingMode::Normal);
        assert_eq!(d.reason, PollReason::Background);
        assert_eq!(
            d.at,
            utc("2026-09-26T13:08:00Z"),
            "background poll 10 min after last"
        );
        // Next background would be 13:18 → still before window start 13:26:30.
        let d = policy().next_poll(&inputs(
            "2026-09-26T13:10:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:08:00Z"),
        ));
        assert_eq!(d.at, utc("2026-09-26T13:18:00Z"));
        // From 13:18 the next background (13:28) is after the window start → window start wins.
        let d = policy().next_poll(&inputs(
            "2026-09-26T13:19:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:18:00Z"),
        ));
        assert_eq!(d.reason, PollReason::ArrivalWindow);
        assert_eq!(d.at, utc("2026-09-26T13:26:30Z"));
        assert_eq!(d.expected_report, Some(utc("2026-09-26T13:25:00Z")));
    }

    #[test]
    fn inside_window_polls_at_window_interval_and_caps_polls() {
        let mut i = inputs(
            "2026-09-26T13:27:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:26:30Z"),
        );
        i.polls_in_current_window = 1;
        let d = policy().next_poll(&i);
        assert!(d.in_window);
        assert_eq!(d.at, utc("2026-09-26T13:27:30Z"));
        // Quick polls used up: the window (to 13:37) is still watched, more slowly.
        i.polls_in_current_window = 6;
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::LateWindow);
        assert!(d.in_window);
        assert_eq!(d.at, utc("2026-09-26T13:28:00Z"));
        // The last late poll may land on the window's end...
        i.now = utc("2026-09-26T13:35:40Z");
        i.last_poll_at = Some(utc("2026-09-26T13:35:30Z"));
        i.polls_in_current_window = 9;
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::LateWindow);
        assert_eq!(d.at, utc("2026-09-26T13:37:00Z"));
        // ...and after it the background cadence takes over.
        i.now = utc("2026-09-26T13:37:00Z");
        i.last_poll_at = Some(utc("2026-09-26T13:37:00Z"));
        i.polls_in_current_window = 10;
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::Overdue);
        assert!(!d.in_window);
        assert_eq!(d.at, utc("2026-09-26T13:47:00Z"));
        // Once the window has closed, the wait is for the next report.
        i.now = utc("2026-09-26T13:37:05Z");
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::Background);
        assert_eq!(d.expected_report, Some(utc("2026-09-26T13:55:00Z")));
        assert_eq!(d.at, utc("2026-09-26T13:47:00Z"));
    }

    /// 29 Sep 2026: the 12:25 report was over six minutes late at AWC. Peak
    /// mode spent its ten quick polls by +360 s and then waited the 5-minute
    /// background interval, so the report was only seen at +660 s. The late
    /// polls now cover the rest of the window.
    #[test]
    fn late_report_is_seen_within_a_late_interval() {
        let p = policy();
        let nominal = utc("2026-09-29T12:25:00Z");
        let published = nominal + Duration::seconds(400);
        let mut i = inputs(
            "2026-09-29T12:24:00Z",
            Some("2026-09-29T11:55:00Z"),
            Some("2026-09-29T12:21:00Z"),
        );
        i.hints.peak_watch = true;
        let mut polls = Vec::new();
        let seen = loop {
            let d = p.next_poll(&i);
            assert_eq!(d.expected_report, Some(nominal));
            i.now = d.at;
            i.last_poll_at = Some(d.at);
            if d.in_window {
                i.polls_in_current_window += 1;
            }
            polls.push((d.at - nominal, d.reason));
            if d.at >= published {
                break d.at;
            }
            assert!(polls.len() < 30, "runaway polling: {polls:?}");
        };
        assert_eq!(seen - nominal, Duration::seconds(420), "{polls:?}");
        let quick = polls
            .iter()
            .filter(|(_, r)| *r == PollReason::ArrivalWindow)
            .count();
        assert_eq!(quick, 10);
        assert_eq!(polls.last().map(|(_, r)| *r), Some(PollReason::LateWindow));
    }

    #[test]
    fn peak_mode_polls_faster_but_only_in_window() {
        let mut i = inputs(
            "2026-09-26T13:27:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:26:30Z"),
        );
        i.hints.peak_watch = true;
        i.polls_in_current_window = 1;
        let d = policy().next_poll(&i);
        assert_eq!(d.mode, PollingMode::Peak);
        assert_eq!(d.at, utc("2026-09-26T13:27:00Z"));
        // Outside the window peak mode still uses the slow background interval.
        let mut i = inputs(
            "2026-09-26T13:05:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T12:58:00Z"),
        );
        i.hints.peak_watch = true;
        let d = policy().next_poll(&i);
        assert_eq!(d.at, utc("2026-09-26T13:03:00Z").max(i.now));
        assert_eq!(d.reason, PollReason::Background);
    }

    #[test]
    fn missed_reports_do_not_speed_up_polling() {
        // 13:25 and 13:55 never arrived; at 14:10 we wait for the 14:25 window
        // at the normal background cadence — no faster.
        let d = policy().next_poll(&inputs(
            "2026-09-26T14:10:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T14:08:00Z"),
        ));
        assert_eq!(d.expected_report, Some(utc("2026-09-26T14:25:00Z")));
        assert_eq!(d.reason, PollReason::Background);
        assert_eq!(d.at, utc("2026-09-26T14:18:00Z"));
        // Window of a missed report closed: back to background cadence.
        let mut i = inputs(
            "2026-09-26T13:36:10Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:36:00Z"),
        );
        i.polls_in_current_window = 8;
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::Overdue);
        assert_eq!(d.at, utc("2026-09-26T13:46:00Z"));
        // Long outage (> 2 h since the newest observation): slow down further.
        let d = policy().next_poll(&inputs(
            "2026-09-26T17:00:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T16:58:00Z"),
        ));
        assert_eq!(d.reason, PollReason::StaleSlowdown);
        assert_eq!(d.at, utc("2026-09-26T17:18:00Z"));
    }

    #[test]
    fn gate_and_throttling_win() {
        let mut i = inputs(
            "2026-09-26T13:27:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:26:30Z"),
        );
        i.gate_not_before = Some(utc("2026-09-26T13:40:00Z"));
        let d = policy().next_poll(&i);
        assert_eq!(d.reason, PollReason::GateBlocked);
        assert_eq!(d.at, utc("2026-09-26T13:40:00Z"));
        let mut i = inputs(
            "2026-09-26T13:27:00Z",
            Some("2026-09-26T12:55:00Z"),
            Some("2026-09-26T13:26:30Z"),
        );
        i.hints.has_exposure = true;
        i.throttled_recently = true;
        assert_eq!(policy().mode(&i), PollingMode::Normal);
    }

    #[test]
    fn night_uses_low_mode() {
        // 00:30 CEST = 22:30 UTC previous day.
        let i = inputs(
            "2026-09-25T22:30:00Z",
            Some("2026-09-25T22:25:00Z"),
            Some("2026-09-25T22:27:00Z"),
        );
        assert_eq!(policy().mode(&i), PollingMode::Low);
    }

    /// Simulate a full day of polling with every report published `delay`
    /// after its nominal time. Returns the requests made and the longest wait
    /// between a report's publication and the poll that saw it.
    fn simulate_day(delay: Duration) -> (u32, Duration) {
        let p = policy();
        let start = utc("2026-09-26T00:00:00Z");
        let end = start + Duration::days(1);
        let mut now = start;
        let mut last_obs: Option<DateTime<Utc>> = Some(start - Duration::minutes(5));
        let mut last_poll: Option<DateTime<Utc>> = None;
        let mut polls_in_window = 0;
        let mut requests = 0;
        let mut worst_wait = Duration::zero();
        let mut current_expected = None;
        while now < end {
            let mut i = inputs("2026-09-26T00:00:00Z", None, None);
            i.now = now;
            i.last_observation_time = last_obs;
            i.last_poll_at = last_poll;
            i.polls_in_current_window = polls_in_window;
            i.hints.peak_watch = (11..=16).contains(&now.hour());
            let d = p.next_poll(&i);
            if d.expected_report != current_expected {
                current_expected = d.expected_report;
                polls_in_window = 0;
            }
            now = d.at;
            if now >= end {
                break;
            }
            requests += 1;
            last_poll = Some(now);
            if d.in_window {
                polls_in_window += 1;
            }
            // Newest report available at this moment.
            let mut t = start - Duration::minutes(5);
            while let Some(n) = p.cadence.next_report_after(t) {
                if n + delay > now {
                    break;
                }
                t = n;
            }
            if Some(t) > last_obs {
                // Every report published since the previous poll is seen now.
                let mut r = last_obs.unwrap_or(t);
                while let Some(n) = p.cadence.next_report_after(r) {
                    if n > t {
                        break;
                    }
                    worst_wait = worst_wait.max(now - (n + delay));
                    r = n;
                }
                last_obs = Some(t);
            }
        }
        (requests, worst_wait)
    }

    /// Reports arriving on time (3 min after nominal): the budget stays modest.
    #[test]
    fn simulated_day_request_budget() {
        let (requests, worst_wait) = simulate_day(Duration::minutes(3));
        assert!(requests < 400, "requests per day {requests}");
        assert!(requests > 100, "requests per day {requests}");
        assert!(
            worst_wait <= Duration::minutes(2),
            "worst wait {worst_wait}"
        );
    }

    /// Every report 11 minutes late: the late polls still see each one within
    /// a few minutes of publication at a cost far inside AWC's daily budget
    /// (2,000 requests).
    #[test]
    fn simulated_late_day_stays_timely_and_in_budget() {
        let (requests, worst_wait) = simulate_day(Duration::minutes(11));
        assert!(requests < 800, "requests per day {requests}");
        assert!(
            worst_wait <= Duration::minutes(3),
            "worst wait {worst_wait}"
        );
    }
}
