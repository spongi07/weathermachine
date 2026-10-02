//! L18–L22: who trades — bursts of flow, takers' track records, the jump
//! after a new high, and the tails of the market's own favourite.

use super::day::{Buy, Day, Flow, Quote, c, hm};
use crate::quoting::{next_routine_report, quote_expiry, tick_of};
use crate::strategy::StrategyOutput;
use chrono::Duration;
use std::collections::{BTreeMap, HashSet};
use wm_core::market::{MarketOutcome, OutcomeSide};
use wm_core::units::Price;

/// Shown on the high's bucket when a flow rule has nothing to trade on.
fn idle(day: &Day<'_, '_>, why: String, out: &mut StrategyOutput) {
    if let Some(h) = day.outcome_of(day.high) {
        day.note(h, OutcomeSide::Yes, vec![why], out);
    }
}

/// L18 (`control`: without KNMI). A burst of informed flow before a report
/// — ≥ 40 shares from ≥ 2 takers within 3 minutes selling the high's
/// bucket or buying the next degree, from 6 minutes before a routine report
/// until it is known — followed only when KNMI's reading is ≥ the edge +
/// 0.3 °C: NO of the high's bucket at 0.05–0.85.
pub(crate) fn burst_follow(day: &Day<'_, '_>, control: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    let last = day.last_metar_at();
    match next_routine_report(last, day.ctx.routine_minutes) {
        None => blockers.push("report schedule unknown".into()),
        Some(report) => {
            let from = report - Duration::minutes(6);
            let to = report + Duration::minutes(day.cfg.report_known_minutes);
            if day.now() < from || day.now() >= to {
                blockers.push(format!(
                    "outside the burst window {}–{} UTC (6 min before the {} UTC report until it is known)",
                    from.format("%H:%M"),
                    to.format("%H:%M"),
                    report.format("%H:%M")
                ));
            }
            let next = day.next_outcome();
            let mut events: Vec<Flow<'_>> = day
                .flow()
                .into_iter()
                .filter(|t| t.at >= from && t.at < to)
                .filter(|t| {
                    (t.outcome.condition_id == h.condition_id && !t.taker_buys_yes)
                        || next.is_some_and(|j| {
                            t.outcome.condition_id == j.condition_id && t.taker_buys_yes
                        })
                })
                .collect();
            events.sort_by_key(|t| t.at);
            let burst = events.iter().find_map(|e| {
                let span: Vec<&Flow<'_>> = events
                    .iter()
                    .filter(|t| t.at >= e.at - Duration::minutes(3) && t.at <= e.at)
                    .collect();
                let shares: f64 = span.iter().map(|t| t.shares).sum();
                let takers: HashSet<&str> = span.iter().filter_map(|t| t.taker).collect();
                (shares >= 40.0 && takers.len() >= 2).then_some((e.at, shares, takers.len()))
            });
            match burst {
                None if day.lab.takers.is_empty() => {
                    blockers.push("no taker trades received (the Data API poll)".into());
                }
                None => blockers.push(format!(
                    "no burst: {} rise-side trade(s) in the window, none reaching 40 shares from 2 takers in 3 min",
                    events.len()
                )),
                Some((at, shares, takers)) => {
                    if day.now() < at + Duration::seconds(10) {
                        blockers.push("following 10 s after the burst".into());
                    }
                    rationale.push(format!(
                        "burst at {} UTC: {shares:.0} shares from {takers} takers selling {} or buying the next degree",
                        at.format("%H:%M:%S"),
                        h.label
                    ));
                }
            }
        }
    }
    if !control {
        match day.latest_reading() {
            Err(why) => blockers.push(why),
            Ok(r) => {
                rationale.push(Day::describe(r));
                if r.interval_end <= last {
                    blockers.push("KNMI reading not newer than the last METAR".into());
                }
                match r.mean.map(|m| m.tenths()) {
                    Some(m) if m >= day.edge() + 3 => {
                        rationale.push("KNMI confirms: the next METAR should rise".into());
                    }
                    Some(m) => blockers.push(format!(
                        "KNMI mean {} < {} (the edge + 0.3)",
                        c(m),
                        c(day.edge() + 3)
                    )),
                    None => blockers.push("KNMI mean missing".into()),
                }
            }
        }
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::No,
            band: (0.05, 0.85),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L19 (`variant`: t ≥ 3). Follow the takers whose record on settled days
/// shows skill (≥ 30 trades, ≥ +0.02 a share, t ≥ 2): their side from 30
/// seconds after their trade for 10 minutes, at ≤ their price + 0.03
/// (their price 0.05–0.90).
pub(crate) fn skill_follow(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_t = if variant { 3.0 } else { 2.0 };
    let Some(wallets) = day.lab.wallets else {
        idle(
            day,
            "no taker records yet (loaded from settled days after the start)".into(),
            out,
        );
        return;
    };
    let now = day.now();
    // Per bucket and side: the highest price to pay, from each skilled
    // trade of the last 10 minutes that is at least 30 seconds old.
    let mut follow: BTreeMap<(String, bool), (&MarketOutcome, f64, String)> = BTreeMap::new();
    let mut waiting = 0;
    for t in day.flow() {
        let Some(w) = t.taker else { continue };
        if t.at < now - Duration::minutes(10) || !wallets.skilled(w, min_t) {
            continue;
        }
        let yes = t.taker_buys_yes;
        let theirs = if yes { t.yes_price } else { 1.0 - t.yes_price };
        if !(0.05..=0.90).contains(&theirs) {
            continue;
        }
        if now < t.at + Duration::seconds(30) {
            waiting += 1;
            continue;
        }
        let s = wallets.get(w);
        let note = format!(
            "skilled taker {w} ({} trades, {:+.3} a share, t {:.1}) bought {} {} at {theirs:.3}, {} UTC",
            s.map_or(0, |s| s.trades),
            s.map_or(0.0, |s| s.mean),
            s.map_or(0.0, |s| s.t),
            if yes { "YES" } else { "NO" },
            t.outcome.label,
            t.at.format("%H:%M:%S")
        );
        let e = follow.entry((t.outcome.label.clone(), yes)).or_insert((
            t.outcome,
            theirs + 0.03,
            note.clone(),
        ));
        if theirs + 0.03 > e.1 {
            *e = (t.outcome, theirs + 0.03, note);
        }
    }
    if follow.is_empty() {
        let (skilled, _) = wallets.counts(min_t);
        idle(
            day,
            if waiting > 0 {
                "following 30 s after a skilled taker's trade".into()
            } else {
                format!(
                    "no trade by one of the {skilled} skilled takers (t ≥ {min_t:.0}) in the last 10 min"
                )
            },
            out,
        );
        return;
    }
    for ((_, yes), (outcome, max_price, note)) in follow {
        day.taker(
            Buy {
                outcome,
                side: if yes {
                    OutcomeSide::Yes
                } else {
                    OutcomeSide::No
                },
                band: (0.01, max_price),
                p_win: None,
                rationale: vec![note],
            },
            Vec::new(),
            out,
        );
    }
}

/// L20 (`taker`: take NO at 0.80–0.98 within 10 minutes). Longshots are
/// overpriced and some takers keep buying them: when a taker whose ≥ 30
/// trades on settled days lost ≥ 0.05 a share (t ≤ −2) buys YES at
/// 0.02–0.15, rest a NO bid one tick under their price for 30 minutes.
pub(crate) fn longshot_fade(day: &Day<'_, '_>, taker: bool, out: &mut StrategyOutput) {
    let Some(wallets) = day.lab.wallets else {
        idle(
            day,
            "no taker records yet (loaded from settled days after the start)".into(),
            out,
        );
        return;
    };
    let now = day.now();
    let window = Duration::minutes(if taker { 10 } else { 30 });
    // Per bucket, the latest qualifying buy.
    let mut latest: BTreeMap<String, Flow<'_>> = BTreeMap::new();
    let mut waiting = false;
    for t in day.flow() {
        let Some(w) = t.taker else { continue };
        if !t.taker_buys_yes
            || !(0.02..=0.15).contains(&t.yes_price)
            || t.at < now - window
            || !wallets.losing(w)
        {
            continue;
        }
        if now < t.at + Duration::seconds(30) {
            waiting = true;
            continue;
        }
        latest.insert(t.outcome.label.clone(), t);
    }
    if latest.is_empty() {
        let (_, losing) = wallets.counts(2.0);
        idle(
            day,
            if waiting {
                "fading 30 s after a losing taker's buy".into()
            } else {
                format!(
                    "no longshot buy (YES 0.02–0.15) by one of the {losing} losing takers in the last {} min",
                    window.num_minutes()
                )
            },
            out,
        );
        return;
    }
    for t in latest.values() {
        let note = format!(
            "losing taker {} bought YES {} at {:.3}, {} UTC",
            t.taker.unwrap_or("?"),
            t.outcome.label,
            t.yes_price,
            t.at.format("%H:%M:%S")
        );
        if taker {
            day.taker(
                Buy {
                    outcome: t.outcome,
                    side: OutcomeSide::No,
                    band: (0.80, 0.98),
                    p_win: None,
                    rationale: vec![note],
                },
                Vec::new(),
                out,
            );
        } else {
            let tick = day
                .ctx
                .books
                .get(&t.outcome.no_token)
                .map_or(0.01, |b| tick_of(b).as_f64());
            let offer = (t.yes_price - tick).max(0.01);
            let price = Price::from_f64(1.0 - offer)
                .ok()
                .map(|p| p.floor_to_tick(Price::from_f64(tick).unwrap_or(p)));
            day.maker_no(
                Quote {
                    outcome: t.outcome,
                    yes_band: (0.0, 0.15),
                    price,
                    expires_at: Some(t.at + Duration::minutes(30)),
                    rationale: vec![note],
                },
                Vec::new(),
                out,
            );
        }
    }
}

/// L21 (`next_degree`: the next degree, bought at ≥ 0.15, NO at
/// 0.50–0.85). A new high is a surprise for the buckets above it, and
/// in-play markets overreact to big surprises for minutes: 2–15 minutes
/// after a METAR raised the high, the bucket two degrees above bought at
/// ≥ 0.08 since the report and KNMI ≤ the new edge + 0.3 °C → its NO at
/// 0.60–0.92.
pub(crate) fn jump_fade(day: &Day<'_, '_>, next_degree: bool, out: &mut StrategyOutput) {
    let new = day.high;
    let target = if next_degree {
        day.next_outcome()
    } else {
        day.outcome_of(new + 2)
            .filter(|o| !o.bucket.contains(new + 1) && !o.bucket.contains(new))
    };
    let Some(target) = target else {
        return;
    };
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    let reports = day.reports();
    match reports.last() {
        None => blockers.push("no METAR today".into()),
        Some(now_r) => {
            let before = reports[..reports.len() - 1]
                .iter()
                .filter_map(|r| r.temp_tenths)
                .map(|t| super::day::round_half_up(f64::from(t) / 10.0))
                .max();
            let raised = now_r.observed_at == day.f.high_at && before.is_none_or(|b| b < new);
            if !raised {
                blockers.push("the latest METAR did not raise the high".into());
            }
            let start = now_r.known_at + Duration::minutes(2);
            let end = now_r.known_at + Duration::minutes(15);
            if day.now() < start || day.now() > end {
                blockers.push(format!(
                    "outside 2–15 min after the report was known ({}–{} UTC)",
                    start.format("%H:%M"),
                    end.format("%H:%M")
                ));
            }
            let min_jump = if next_degree { 0.15 } else { 0.08 };
            let jumped = day.flow().into_iter().find(|t| {
                t.outcome.condition_id == target.condition_id
                    && t.taker_buys_yes
                    && t.at >= now_r.observed_at
                    && t.at <= start
                    && t.yes_price >= min_jump
            });
            match jumped {
                Some(t) => rationale.push(format!(
                    "new high {new} °C: {} bought at {:.3} ({} UTC)",
                    target.label,
                    t.yes_price,
                    t.at.format("%H:%M:%S")
                )),
                None if day.lab.takers.is_empty() => {
                    blockers.push("no taker trades received (the Data API poll)".into());
                }
                None => blockers.push(format!(
                    "{} not bought at ≥ {min_jump:.2} since the report",
                    target.label
                )),
            }
        }
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m <= new * 10 + 5 + 3 => {
                rationale.push(format!("{}: no further rise in sight", Day::describe(r)))
            }
            Some(m) => blockers.push(format!(
                "KNMI mean {} > {} (the new edge + 0.3): the rise goes on",
                c(m),
                c(new * 10 + 8)
            )),
            None => blockers.push("KNMI mean missing".into()),
        },
    }
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::No,
            band: if next_degree {
                (0.50, 0.85)
            } else {
                (0.60, 0.92)
            },
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L22 (`variant`: ≥ 4 places away). Resting orders were paid overnight
/// and at 0–2¢; quote only the far tails of the market's own favourite:
/// 00:00–09:00, NO bids one tick inside the spread (YES offered at
/// 0.01–0.05) on buckets ≥ 3 places from the favourite (≥ 0.20) that can
/// still win, withdrawn 10 minutes before each report.
pub(crate) fn overnight_tails(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_away = if variant { 4 } else { 3 };
    let ordered = day.ordered();
    let Some((fav, p)) = day.favourite() else {
        if let Some(h) = day.outcome_of(day.high) {
            day.note(
                h,
                OutcomeSide::No,
                vec!["no fresh book to find the market's favourite".into()],
                out,
            );
        }
        return;
    };
    let Some(pf) = ordered
        .iter()
        .position(|o| o.condition_id == fav.condition_id)
    else {
        return;
    };
    let mut common: Vec<String> = Vec::new();
    if day.minute() >= 9 * 60 {
        common.push(format!("after 09:00 (now {})", hm(day.minute())));
    }
    if p < 0.20 {
        common.push(format!("favourite {} at {p:.2} < 0.20", fav.label));
    }
    let expires = quote_expiry(
        day.now(),
        day.ctx.routine_minutes,
        Duration::minutes(day.cfg.cancel_before_report_minutes),
        Duration::minutes(day.cfg.min_rest_minutes),
    );
    if expires.is_none() {
        common.push(format!(
            "within {} min of the next report (quotes are withdrawn {} min before it)",
            day.cfg.cancel_before_report_minutes + day.cfg.min_rest_minutes,
            day.cfg.cancel_before_report_minutes
        ));
    }
    for (pos, o) in ordered.iter().enumerate() {
        if pos.abs_diff(pf) < min_away || o.bucket.upper.is_some_and(|u| u < day.high) {
            continue;
        }
        day.maker_no(
            Quote {
                outcome: o,
                yes_band: (0.01, 0.05),
                price: None,
                expires_at: expires,
                rationale: vec![format!(
                    "{} is {} places from the favourite {} ({p:.2})",
                    o.label,
                    pos.abs_diff(pf),
                    fav.label
                )],
            },
            common.clone(),
            out,
        );
    }
}
