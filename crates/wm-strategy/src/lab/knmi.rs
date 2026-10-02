//! L1–L7: KNMI's ten-minute readings as shield, stop and early warning.

use super::day::{Buy, Day, Quote, c};
use crate::peak_slot::PeakSlotHigh;
use crate::quoting::next_routine_report;
use crate::strategy::{Strategy, StrategyOutput};
use chrono::Duration;
use wm_core::market::OutcomeSide;

/// L1 (`control`: without the shield). Makers on the next degree lose just
/// before reports to takers who know the METAR first; KNMI says when no
/// rise is coming: rest a NO bid on the next degree only while the three
/// means of the last 30 minutes are ≥ 0.5 °C under the high's rounding edge
/// and not climbing, until the next reading or the report after it.
pub(crate) fn shielded_maker(day: &Day<'_, '_>, control: bool, out: &mut StrategyOutput) {
    let Some(next) = day.next_outcome() else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(10 * 60, 20 * 60).into_iter().collect();
    let edge = day.edge();
    let mut rationale = Vec::new();
    let mut expires = None;
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            rationale.push(super::day::Day::describe(r));
            if !control {
                let w = day.window(r.interval_end, 30);
                if w.len() < 3 {
                    blockers.push(format!(
                        "{} KNMI reading(s) in the last 30 min < 3",
                        w.len()
                    ));
                } else if let Some(x) = w
                    .iter()
                    .find(|x| x.mean.is_none_or(|m| m.tenths() > edge - 5))
                {
                    blockers.push(format!(
                        "KNMI mean {} at {} UTC not ≥ 0.5 °C under the edge {}",
                        x.mean.map_or_else(|| "—".into(), |m| c(m.tenths())),
                        x.interval_end.format("%H:%M"),
                        c(edge)
                    ));
                }
                match (
                    r.mean.map(|m| m.tenths()),
                    day.mean_at(r.interval_end - Duration::minutes(20)),
                ) {
                    (Some(now), Some(before)) if now > before => blockers.push(format!(
                        "KNMI mean rising ({} → {} in 20 min)",
                        c(before),
                        c(now)
                    )),
                    (Some(_), Some(_)) => {
                        rationale.push(format!(
                            "shield: KNMI ≥ 0.5 °C under the edge {} for 30 min, not rising",
                            c(edge)
                        ));
                    }
                    _ => blockers.push("no KNMI mean from 20 min before".into()),
                }
            }
            let report = next_routine_report(day.now(), day.ctx.routine_minutes);
            expires = report.map(|n| {
                (n + Duration::minutes(day.cfg.report_known_minutes))
                    .min(r.received_at.max(r.interval_end) + Duration::minutes(10))
            });
            if report.is_none() {
                blockers.push("report schedule unknown".into());
            }
        }
    }
    day.maker_no(
        Quote {
            outcome: next,
            yes_band: (0.03, 0.40),
            price: None,
            expires_at: expires,
            rationale,
        },
        blockers,
        out,
    );
}

/// L2 (`variant`: margin 0.6 °C). K's signal as a maker: once KNMI's mean
/// before the METAR is ≥ the edge + 0.3 °C, rest a NO bid on the high's
/// bucket until the anticipated report is known.
pub(crate) fn informed_maker(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let margin = if variant { 6 } else { 3 };
    let edge = day.edge();
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    let mut expires = None;
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            rationale.push(Day::describe(r));
            if let Some(n) = day.ahead_of_metar(r, &mut blockers) {
                expires = Some(n + Duration::minutes(day.cfg.report_known_minutes));
            }
            match r.mean.map(|m| m.tenths()) {
                None => blockers.push("KNMI mean missing".into()),
                Some(m) if m < edge + margin => blockers.push(format!(
                    "KNMI mean {} < {} (the edge + {})",
                    c(m),
                    c(edge + margin),
                    c(margin)
                )),
                Some(_) => rationale.push(format!(
                    "the next METAR should report {}: {} is dead then",
                    day.high + 1,
                    h.label
                )),
            }
        }
    }
    day.maker_no(
        Quote {
            outcome: h,
            yes_band: (0.25, 0.97),
            price: None,
            expires_at: expires,
            rationale,
        },
        blockers,
        out,
    );
}

/// L3 (`variant`: exit at +0.0 °C). Enters as F does, on its own book;
/// once holding, sells the YES at the bid before the METAR when KNMI's
/// reading is ≥ the bucket's rounding edge + 0.3 °C (F's losses were late
/// new highs, which KNMI sees first). One entry a day.
pub(crate) fn f_escape(
    day: &Day<'_, '_>,
    variant: bool,
    f: &mut PeakSlotHigh,
    out: &mut StrategyOutput,
) {
    let ctx = day.ctx;
    let slug = &ctx.market.event_slug;
    let mine: Vec<_> = ctx
        .positions
        .iter()
        .filter(|p| &p.instrument.event_slug == slug)
        .collect();
    let held = mine
        .iter()
        .find(|p| p.shares.micros() > 0 && p.instrument.outcome_side == OutcomeSide::Yes);
    if let Some(p) = held {
        let Some(outcome) = ctx
            .market
            .outcomes
            .iter()
            .find(|o| o.yes_token == p.instrument.token)
        else {
            return;
        };
        let margin = if variant { 0 } else { 3 };
        let mut blockers = Vec::new();
        let mut rationale = vec![format!("holding {} YES {}", p.shares, outcome.label)];
        match outcome.bucket.upper {
            None => blockers.push("open-ended bucket: no new high can kill it".into()),
            Some(u) if day.high > u => {
                blockers.push(format!(
                    "a report already raised the high past {}",
                    outcome.label
                ));
            }
            Some(u) => {
                let edge = u * 10 + 5;
                match day.latest_reading() {
                    Err(why) => blockers.push(why),
                    Ok(r) => {
                        rationale.push(Day::describe(r));
                        let _ = day.ahead_of_metar(r, &mut blockers);
                        match r.mean.map(|m| m.tenths()) {
                            None => blockers.push("KNMI mean missing".into()),
                            Some(m) if m < edge + margin => blockers.push(format!(
                                "KNMI mean {} < {} (the bucket's edge + {}): hold",
                                c(m),
                                c(edge + margin),
                                c(margin)
                            )),
                            Some(_) => rationale.push(format!(
                                "the next METAR should report {}: {} would die",
                                u + 1,
                                outcome.label
                            )),
                        }
                    }
                }
            }
        }
        day.sell_yes(outcome, p.shares, rationale, blockers, out);
        return;
    }
    // F's entry, on this strategy's book.
    let mut entry = f.evaluate(ctx);
    let exited = !mine.is_empty();
    for e in &mut entry.evaluations {
        e.strategy = day.id.clone();
        if exited {
            e.signal = false;
            e.blockers
                .push("exited today: F's rule does not buy again".into());
        }
    }
    if !exited {
        for p in &mut entry.proposals {
            p.strategy = day.id.clone();
            p.rationale
                .push("L3: F's entry; sold at the bid if KNMI sees a new high first".into());
        }
        out.proposals.extend(entry.proposals);
    }
    out.evaluations.extend(entry.evaluations);
}

/// L4 (`variant`: F's band 0.90–0.98). The high's bucket is underpriced,
/// the more so when KNMI shows the afternoon cooling: 13:00–20:00, KNMI's
/// maxima of the last 90 minutes ≥ 0.3 °C under the bucket's rounding
/// edge, the mean down ≥ 0.6 °C in an hour, the METAR ≥ 1 °C under the
/// high → YES of the high's bucket (expected profit at p = 0.97).
pub(crate) fn cooling_lock(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.outcome_of(day.high) else {
        return;
    };
    let band = if variant { (0.90, 0.98) } else { (0.70, 0.95) };
    let mut blockers: Vec<String> = day.outside(13 * 60, 20 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    if day.f.drop_tenths < 10 {
        blockers.push(format!(
            "METAR {} under the high < 1.0 °C",
            c(day.f.drop_tenths)
        ));
    }
    match h.bucket.upper {
        None => blockers.push("open-ended bucket".into()),
        Some(upper) => {
            let edge = upper * 10 + 5;
            match day.latest_reading() {
                Err(why) => blockers.push(why),
                Ok(r) => {
                    rationale.push(Day::describe(r));
                    let w = day.window(r.interval_end, 90);
                    if w.len() < 8 {
                        blockers.push(format!("{} KNMI reading(s) in 90 min < 8", w.len()));
                    } else if let Some(x) = w
                        .iter()
                        .find(|x| x.max.or(x.mean).is_none_or(|m| m.tenths() > edge - 3))
                    {
                        blockers.push(format!(
                            "KNMI maximum {} at {} UTC above {} (the bucket's edge − 0.3)",
                            x.max
                                .or(x.mean)
                                .map_or_else(|| "—".into(), |m| c(m.tenths())),
                            x.interval_end.format("%H:%M"),
                            c(edge - 3)
                        ));
                    }
                    match (
                        r.mean.map(|m| m.tenths()),
                        day.mean_at(r.interval_end - Duration::minutes(60)),
                    ) {
                        (Some(now), Some(ago)) if now > ago - 6 => blockers.push(format!(
                            "KNMI mean fell {} in an hour < 0.6 °C",
                            c(ago - now)
                        )),
                        (Some(now), Some(ago)) => rationale.push(format!(
                            "cooling: KNMI {} → {} in an hour, maxima ≥ 0.3 °C under {} for 90 min",
                            c(ago),
                            c(now),
                            c(edge)
                        )),
                        _ => blockers.push("no KNMI mean from an hour before".into()),
                    }
                }
            }
        }
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::Yes,
            band,
            p_win: Some((0.97, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L5 (`variant`: from 17:00). G's tails one degree closer, late, when
/// KNMI rules the rise out: 15:00–21:00, every mean of the last hour ≥ 1 °C
/// and every maximum ≥ 0.5 °C under the edge, the mean lower than an hour
/// ago → NO of the next degree (p = 0.99).
pub(crate) fn late_next_no(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(i) = day.next_outcome() else {
        return;
    };
    let start = if variant { 17 * 60 } else { 15 * 60 };
    let edge = day.edge();
    let mut blockers: Vec<String> = day.outside(start, 21 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            rationale.push(Day::describe(r));
            let w = day.window(r.interval_end, 60);
            if w.len() < 5 {
                blockers.push(format!("{} KNMI reading(s) in the last hour < 5", w.len()));
            } else if let Some(x) = w.iter().find(|x| {
                x.mean.is_none_or(|m| m.tenths() > edge - 10)
                    || x.max.is_some_and(|m| m.tenths() > edge - 5)
            }) {
                blockers.push(format!(
                    "KNMI at {} UTC (mean {}, max {}) not ≥ 1 °C / 0.5 °C under the edge {}",
                    x.interval_end.format("%H:%M"),
                    x.mean.map_or_else(|| "—".into(), |m| c(m.tenths())),
                    x.max.map_or_else(|| "—".into(), |m| c(m.tenths())),
                    c(edge)
                ));
            }
            match (
                r.mean.map(|m| m.tenths()),
                day.mean_at(r.interval_end - Duration::minutes(60)),
            ) {
                (Some(now), Some(ago)) if now >= ago => blockers.push(format!(
                    "KNMI mean not lower than an hour ago ({} → {})",
                    c(ago),
                    c(now)
                )),
                (Some(_), Some(_)) => rationale.push(format!(
                    "KNMI ≥ 1 °C under the edge {} for an hour and falling",
                    c(edge)
                )),
                _ => blockers.push("no KNMI mean from an hour before".into()),
            }
        }
    }
    day.taker(
        Buy {
            outcome: i,
            side: OutcomeSide::No,
            band: (0.75, 0.97),
            p_win: Some((0.99, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L6 (`control`: unfiltered, K · YES above). K's trigger (KNMI 0.8 °C or
/// more above the edge before the METAR) after the season's median peak
/// time with a rise of ≤ 0.4 °C in 30 minutes: the new degree is likely the
/// last → YES of the next degree at 0.20–0.70.
pub(crate) fn late_new_yes(day: &Day<'_, '_>, control: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let edge = day.edge();
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            rationale.push(Day::describe(r));
            let _ = day.ahead_of_metar(r, &mut blockers);
            match r.mean.map(|m| m.tenths()) {
                None => blockers.push("KNMI mean missing".into()),
                Some(now) => {
                    if now < edge + 8 {
                        blockers.push(format!(
                            "KNMI mean {} < {} (K's trigger: the edge + 0.8)",
                            c(now),
                            c(edge + 8)
                        ));
                    }
                    if !control {
                        let median = day.season_median();
                        if day.minute() < median {
                            blockers.push(format!(
                                "before the season's median peak time {}",
                                super::day::hm(median)
                            ));
                        }
                        match day.mean_at(r.interval_end - Duration::minutes(30)) {
                            None => blockers.push("no KNMI mean from 30 min before".into()),
                            Some(before) if now - before > 4 => blockers.push(format!(
                                "KNMI still rising {} in 30 min > 0.4 °C",
                                c(now - before)
                            )),
                            Some(_) => rationale.push(
                                "late and flattening: the new degree should be the last".into(),
                            ),
                        }
                    }
                }
            }
        }
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::Yes,
            band: (0.20, 0.70),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L7 (`variant`: to the edge + 1.0 °C). K one reading early: the mean
/// within 0.5 °C under to 0.8 °C over the edge, rising, its 20-minute
/// slope carried to the next report reaching the edge + 0.6 °C → NO of the
/// high's bucket at ≤ 0.75 (p = 0.85).
pub(crate) fn slope_k(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let target = if variant { 10 } else { 6 };
    let edge = day.edge();
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            rationale.push(Day::describe(r));
            let next = day.ahead_of_metar(r, &mut blockers);
            match (
                r.mean.map(|m| m.tenths()),
                day.mean_at(r.interval_end - Duration::minutes(20)),
                next,
            ) {
                (Some(m0), Some(m20), Some(n)) => {
                    let slope = f64::from(m0 - m20) / 20.0;
                    let lead = (n - r.interval_end).num_minutes() as f64;
                    let projected = f64::from(m0) + slope * lead;
                    if slope <= 0.0 {
                        blockers.push(format!(
                            "KNMI not rising ({} → {} in 20 min)",
                            c(m20),
                            c(m0)
                        ));
                    }
                    if m0 >= edge + 8 {
                        blockers.push(format!(
                            "KNMI mean {} already ≥ the edge + 0.8 (K's)",
                            c(m0)
                        ));
                    }
                    if m0 < edge - 5 {
                        blockers.push(format!(
                            "KNMI mean {} more than 0.5 °C under the edge",
                            c(m0)
                        ));
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let proj = projected.round() as i32;
                    if projected < f64::from(edge + target) {
                        blockers.push(format!(
                            "projected {} at the report < {} (the edge + {})",
                            c(proj),
                            c(edge + target),
                            c(target)
                        ));
                    } else {
                        rationale.push(format!(
                            "rising {:.2} °C/min: {} by {} UTC",
                            slope / 10.0,
                            c(proj),
                            n.format("%H:%M")
                        ));
                    }
                }
                (None, _, _) => blockers.push("KNMI mean missing".into()),
                (_, None, _) => blockers.push("no KNMI mean from 20 min before".into()),
                _ => {}
            }
        }
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::No,
            band: (0.05, 0.75),
            p_win: Some((0.85, 0.02)),
            rationale,
        },
        blockers,
        out,
    );
}
