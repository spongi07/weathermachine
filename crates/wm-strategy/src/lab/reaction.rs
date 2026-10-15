//! L35, L46, L50, L51, L52, L53, L55, L56, L57, L61 and L62: how the
//! market reacts to the weather — too little, too much, too late, or not
//! at all. Each rule waits until the market has had minutes to react
//! before it reads what the market did, so it never races the traders who
//! see a report 2 minutes before the bot does.

use super::day::{Buy, Day, c, hm, implied, round_half_up};
use crate::strategy::StrategyOutput;
use chrono::{Duration, Timelike};
use wm_core::market::OutcomeSide;

/// The report that first reached today's high (`None` before one did).
fn high_report<'a>(day: &Day<'_, 'a>) -> Option<&'a super::WxReport> {
    day.reports()
        .iter()
        .find(|r| r.temp_tenths.is_some_and(|t| t >= day.high * 10))
}

/// L35 (`variant`: the next degree up ≤ 0.02). Under-reaction after a new
/// high: 10:00–15:00 and before the season's median peak + 60 minutes,
/// 3–15 minutes after a report raised the high, the next degree's price up
/// ≤ 0.05 since while KNMI is still rising (≥ 0.3 °C in 30 minutes) → YES
/// of the next degree at 0.05–0.35.
pub(crate) fn post_high_drift(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let max_move = if variant { 0.02 } else { 0.05 };
    let end = (day.season_median() + 60).min(15 * 60);
    let mut blockers: Vec<String> = day.outside(10 * 60, end).into_iter().collect();
    let mut rationale = Vec::new();
    let now = day.now();
    match high_report(day) {
        None => blockers.push("no report reached the high today".into()),
        Some(r) => {
            let since = now - r.known_at;
            if since < Duration::minutes(3) || since > Duration::minutes(15) {
                blockers.push(format!(
                    "the high {} came with the {} UTC report, known {} min ago (3–15 wanted)",
                    c(day.high * 10),
                    r.observed_at.format("%H:%M"),
                    since.num_minutes()
                ));
            } else {
                match (day.yes_price(j), day.mid_at(j, r.known_at)) {
                    (Some(now_p), Some(then)) => {
                        let moved = now_p - then;
                        if moved > max_move + 1e-9 {
                            blockers.push(format!(
                                "{} up {moved:+.3} since the report > {max_move:.2}: priced in",
                                j.label
                            ));
                        } else {
                            rationale.push(format!(
                                "new high {} at {} UTC; {} moved only {moved:+.3} since",
                                c(day.high * 10),
                                r.observed_at.format("%H:%M"),
                                j.label
                            ));
                        }
                    }
                    _ => {
                        blockers.push("no ladder sample at the report and no fresh book now".into())
                    }
                }
            }
        }
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            let m = r.mean.map(|m| m.tenths());
            let before = day.mean_at(r.interval_end - Duration::minutes(30));
            match (m, before) {
                (Some(m), Some(b)) if m - b >= 3 => {
                    rationale.push(format!(
                        "{}: still up {} in 30 min",
                        Day::describe(r),
                        c(m - b)
                    ));
                }
                (Some(m), Some(b)) => {
                    blockers.push(format!("KNMI up {} in 30 min < 0.3 °C", c(m - b)))
                }
                _ => blockers.push("no KNMI mean now and 30 min before".into()),
            }
        }
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::Yes,
            band: (0.05, 0.35),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L46 (`variant`: from 22:00, ask ≤ 0.98). The evening's capital-recycling
/// discount: 21:00–23:50, KNMI ≥ 3 °C under the edge and not rising, the
/// high's bucket offered at 0.95–0.985 — holders sell a near-certain
/// winner cheap to free their money for tomorrow → its YES (p = 0.997).
pub(crate) fn evening_discount(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let (from, hi) = if variant {
        (22 * 60, 0.98)
    } else {
        (21 * 60, 0.985)
    };
    let mut blockers: Vec<String> = day.outside(from, 23 * 60 + 50).into_iter().collect();
    let mut rationale = Vec::new();
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            let m = r.mean.map(|m| m.tenths());
            let before = day.mean_at(r.interval_end - Duration::minutes(10));
            match (m, before) {
                (Some(m), _) if m > day.edge() - 30 => blockers.push(format!(
                    "KNMI mean {} not ≥ 3 °C under the edge {}",
                    c(m),
                    c(day.edge())
                )),
                (Some(m), Some(b)) if m > b => {
                    blockers.push(format!("KNMI rising ({} after {})", c(m), c(b)));
                }
                (Some(_), Some(_)) => rationale.push(Day::describe(r)),
                _ => blockers.push("no KNMI mean now and 10 min before".into()),
            }
        }
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::Yes,
            band: (0.95, hi),
            p_win: Some((0.997, 0.003)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L50 (`variant`: ≥ 0.18). A move without news: 07:00–21:00, a bucket
/// that can still win moved ≥ 0.12 against 8–15 minutes before with no
/// report known, no forecast and KNMI flat (≤ 0.3 °C) over those 15
/// minutes — nothing about the weather changed → against the move: YES
/// after a fall, NO after a rise (0.15–0.80).
pub(crate) fn newsless_move(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_move = if variant { 0.18 } else { 0.12 };
    if let Some(why) = day.outside(7 * 60, 21 * 60) {
        day.idle(why, out);
        return;
    }
    let now = day.now();
    let Some(then) = day
        .sample_at(now - Duration::minutes(8))
        .filter(|s| s.at >= now - Duration::minutes(15))
    else {
        day.idle("no ladder sample 8–15 min ago".into(), out);
        return;
    };
    let best = day
        .ordered()
        .into_iter()
        .filter(|o| day.alive(o))
        .filter_map(|o| {
            let p = day.yes_price(o)?;
            let b = then.quote(&o.yes_token)?.mid()?;
            Some((o, p, b, p - b))
        })
        .max_by(|a, b| a.3.abs().total_cmp(&b.3.abs()));
    let Some((o, p, b, moved)) = best else {
        day.idle("no bucket with a price now and 8–15 min ago".into(), out);
        return;
    };
    let mut blockers = Vec::new();
    let mut rationale = vec![format!(
        "{} {b:.3} → {p:.3} ({moved:+.3}) since {} UTC",
        o.label,
        then.at.format("%H:%M")
    )];
    if moved.abs() + 1e-9 < min_move {
        blockers.push(format!(
            "largest move {moved:+.3} ({}) < {min_move:.2}",
            o.label
        ));
    }
    if let Some(r) = day.latest_report()
        && r.known_at > now - Duration::minutes(15)
    {
        blockers.push(format!(
            "the {} UTC report is news (known {} min ago)",
            r.observed_at.format("%H:%M"),
            (now - r.known_at).num_minutes()
        ));
    }
    if let Some(f) = day.lab.forecast
        && f.known_at > now - Duration::minutes(15)
        && f.known_at <= now
    {
        blockers.push("a new forecast in the last 15 min is news".into());
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            let m = r.mean.map(|m| m.tenths());
            let before = day.mean_at(r.interval_end - Duration::minutes(20));
            match (m, before) {
                (Some(m), Some(b)) if (m - b).abs() <= 3 => {
                    rationale.push(format!("KNMI flat: {} after {} 20 min before", c(m), c(b)));
                }
                (Some(m), Some(b)) => {
                    blockers.push(format!("KNMI moved {} in 20 min: that is news", c(m - b)))
                }
                _ => blockers.push("no KNMI mean now and 20 min before".into()),
            }
        }
    }
    let side = if moved < 0.0 {
        OutcomeSide::Yes
    } else {
        OutcomeSide::No
    };
    day.taker(
        Buy {
            outcome: o,
            side,
            band: (0.15, 0.80),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L51 (`variant`: 0.9 °C against 0.8 °C). Prices against the thermometer:
/// 10:00–15:00, the ladder's mean down ≥ 0.6 °C in 30 minutes while KNMI
/// rose ≥ 0.5 °C → YES of the bucket a degree above the ladder's mean
/// (0.08–0.40); the mean up ≥ 0.6 °C while KNMI fell ≥ 0.5 °C → NO of the
/// mean's bucket (0.55–0.92).
pub(crate) fn momentum_divergence(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (min_mu, min_knmi) = if variant { (0.9, 8) } else { (0.6, 5) };
    if let Some(why) = day.outside(10 * 60, 15 * 60) {
        day.idle(why, out);
        return;
    }
    let now = day.now();
    let (Some((mu, _)), Some((mu0, _))) = (
        implied(&day.alive_prices()),
        implied(&day.alive_prices_at(now - Duration::minutes(30))),
    ) else {
        day.idle("no ladder now and 30 min ago".into(), out);
        return;
    };
    let shift = mu - mu0;
    let trend = day.latest_reading().ok().and_then(|r| {
        let m = r.mean?.tenths();
        let b = day.mean_at(r.interval_end - Duration::minutes(30))?;
        Some((m - b, Day::describe(r)))
    });
    let Some((dk, d)) = trend else {
        day.idle("no fresh KNMI mean now and 30 min before".into(), out);
        return;
    };
    let rising = shift < 0.0;
    let v = round_half_up(mu) + i32::from(rising);
    let Some(target) = day.outcome_of(v).filter(|o| day.alive(o)) else {
        day.idle(
            format!("the ladder's mean {mu:.1} °C: no live bucket for {v} °C"),
            out,
        );
        return;
    };
    let mut blockers = Vec::new();
    let rationale = vec![format!(
        "the ladder's mean {mu0:.1} → {mu:.1} °C in 30 min; {d}, {} in 30 min",
        c(dk)
    )];
    if rising {
        if -shift + 1e-9 < min_mu || dk < min_knmi {
            blockers.push(format!(
                "the mean fell {:.1} °C (needs ≥ {min_mu:.1}) while KNMI moved {} (needs ≥ +{})",
                -shift,
                c(dk),
                c(min_knmi)
            ));
        }
    } else if shift + 1e-9 < min_mu || -dk < min_knmi {
        blockers.push(format!(
            "the mean rose {shift:.1} °C (needs ≥ {min_mu:.1}) while KNMI moved {} (needs ≤ −{})",
            c(dk),
            c(min_knmi)
        ));
    }
    let (side, band) = if rising {
        (OutcomeSide::Yes, (0.08, 0.40))
    } else {
        (OutcomeSide::No, (0.55, 0.92))
    };
    day.taker(
        Buy {
            outcome: target,
            side,
            band,
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L52 (`variant`: +0.08). A pre-report run-up KNMI does not back: in the
/// 12 minutes before a routine report, the next degree up ≥ 0.05 in 10
/// minutes while KNMI's reading ahead of it is ≥ 0.4 °C under the edge →
/// its NO at 0.40–0.92.
pub(crate) fn runup_fade(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_rise = if variant { 0.08 } else { 0.05 };
    let mut blockers = Vec::new();
    let mut rationale = Vec::new();
    match day.minutes_to_report() {
        None => blockers.push("report schedule unknown".into()),
        Some(m) if m > 12 => blockers.push(format!("{m} min before the next report > 12")),
        Some(_) => {}
    }
    let now = day.now();
    match (day.yes_price(j), day.mid_at(j, now - Duration::minutes(10))) {
        (Some(p), Some(b)) => {
            if p - b + 1e-9 < min_rise {
                blockers.push(format!(
                    "{} {b:.3} → {p:.3} in 10 min: up < {min_rise:.2}",
                    j.label
                ));
            } else {
                rationale.push(format!("{} run up {b:.3} → {p:.3} in 10 min", j.label));
            }
        }
        _ => blockers.push("no price now and 10 min ago".into()),
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            day.ahead_of_metar(r, &mut blockers);
            match r.mean.map(|m| m.tenths()) {
                Some(m) if m <= day.edge() - 4 => {
                    rationale.push(format!("{}: ≥ 0.4 °C under the edge", Day::describe(r)));
                }
                Some(m) => blockers.push(format!(
                    "KNMI mean {} within 0.4 °C of the edge {}: the run-up may be right",
                    c(m),
                    c(day.edge())
                )),
                None => blockers.push("KNMI mean missing".into()),
            }
        }
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::No,
            band: (0.40, 0.92),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L53 (`variant`: ≥ 1.2 °C off). The day's projected curve: 10:00–13:00, a
/// sine from the night's lowest METAR (before 09:00) through KNMI's
/// reading now to the season's median peak time projects the day's
/// maximum; when it lies ≥ 0.8 °C off the ladder's mean → YES of its bucket
/// at 0.10–0.45.
pub(crate) fn curve_projection(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_off = if variant { 1.2 } else { 0.8 };
    if let Some(why) = day.outside(10 * 60, 13 * 60) {
        day.idle(why, out);
        return;
    }
    let low = day
        .reports()
        .iter()
        .filter(|r| day.minute_of(r.observed_at) < 9 * 60)
        .filter_map(|r| r.temp_tenths.map(|t| (r.observed_at, t)))
        .min_by_key(|(_, t)| *t);
    let (Some((t_min, low)), Ok(r), Some(peak)) =
        (low, day.latest_reading(), day.local(day.season_median()))
    else {
        day.idle(
            "needs the night's METARs, a fresh KNMI reading and the season's peak time".into(),
            out,
        );
        return;
    };
    let Some(now_t) = r.mean.map(|m| m.tenths()) else {
        day.idle("KNMI mean missing".into(), out);
        return;
    };
    let span = (peak - t_min).num_minutes();
    #[allow(clippy::cast_precision_loss)]
    let x = (r.interval_end - t_min).num_minutes() as f64 / span.max(1) as f64;
    let mut blockers = Vec::new();
    if !(0.3..=0.95).contains(&x) {
        day.idle(
            format!(
                "{:.0} % of the way from the night's low ({} UTC) to the peak time {} (needs 30–95 %)",
                100.0 * x,
                t_min.format("%H:%M"),
                hm(day.season_median())
            ),
            out,
        );
        return;
    }
    let rise = f64::from(now_t - low) / 10.0;
    let amp = rise / (std::f64::consts::FRAC_PI_2 * x).sin();
    let projected = f64::from(low) / 10.0 + amp;
    let v = round_half_up(projected);
    let Some((mu, _)) = implied(&day.alive_prices()) else {
        day.idle("no fresh books for the ladder".into(), out);
        return;
    };
    let Some(target) = day.outcome_of(v).filter(|o| day.alive(o)) else {
        day.idle(
            format!("the curve projects {projected:.1} °C: no live bucket for {v} °C"),
            out,
        );
        return;
    };
    if (projected - mu).abs() + 1e-9 < min_off {
        blockers.push(format!(
            "projected {projected:.1} °C vs the ladder's {mu:.1} °C: off by < {min_off:.1}"
        ));
    }
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.10, 0.45),
            p_win: None,
            rationale: vec![format!(
                "low {} at {} UTC, KNMI {} now ({:.0} % of the way to {}): the curve peaks at {projected:.1} °C, the ladder at {mu:.1} °C",
                c(low),
                t_min.format("%H:%M"),
                c(now_t),
                100.0 * x,
                hm(day.season_median())
            )],
        },
        blockers,
        out,
    );
}

/// L55 (`variant`: ± 0.1 °C). Hovering on the rounding edge: 12:00–17:00
/// within an hour of the season's median peak, KNMI's last three means
/// within 0.2 °C of the edge — a coin flip between the high's bucket and
/// the next — while the market prices one side ≤ 0.35 and the other ≥ 0.55
/// → YES of the cheap side at 0.10–0.35.
pub(crate) fn edge_hover(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (Some(h), Some(j)) = (day.high_outcome(), day.next_outcome()) else {
        return;
    };
    let near = if variant { 1 } else { 2 };
    let median = day.season_median();
    let mut blockers: Vec<String> = day
        .outside(
            (12 * 60).max(median.saturating_sub(60)),
            (17 * 60).min(median + 60),
        )
        .into_iter()
        .collect();
    let mut rationale = Vec::new();
    let rs = day.readings();
    let last3: Vec<i32> = rs[rs.len().saturating_sub(3)..]
        .iter()
        .filter_map(|r| r.mean.map(|m| m.tenths()))
        .collect();
    if let Err(why) = day.latest_reading() {
        blockers.push(why);
    } else if last3.len() < 3 || last3.iter().any(|m| (m - day.edge()).abs() > near) {
        blockers.push(format!(
            "KNMI's last means {} not all within {} of the edge {}",
            last3.iter().map(|m| c(*m)).collect::<Vec<_>>().join(", "),
            c(near),
            c(day.edge())
        ));
    } else {
        rationale.push(format!(
            "KNMI on the edge {}: {}",
            c(day.edge()),
            last3.iter().map(|m| c(*m)).collect::<Vec<_>>().join(", ")
        ));
    }
    let (ph, pj) = (day.yes_price(h), day.yes_price(j));
    let target = match (ph, pj) {
        (Some(a), Some(b)) if a <= 0.35 && b >= 0.55 => Some(h),
        (Some(a), Some(b)) if b <= 0.35 && a >= 0.55 => Some(j),
        _ => None,
    };
    let Some(target) = target else {
        blockers.push(format!(
            "{} {} and {} {}: not one ≤ 0.35 against the other ≥ 0.55",
            h.label,
            ph.map_or_else(|| "—".to_owned(), |p| format!("{p:.2}")),
            j.label,
            pj.map_or_else(|| "—".to_owned(), |p| format!("{p:.2}"))
        ));
        day.note(j, OutcomeSide::Yes, blockers, out);
        return;
    };
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.10, 0.35),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L56 (`variant`: ≥ 4 °C). Stations disagree: 10:00–16:00, Schiphol and
/// the KNMI neighbours ≥ 3 °C apart in the same ten minutes (≥ 3 stations)
/// — a front or a sea breeze is crossing the area — while the market's
/// favourite is priced ≥ 0.55 → the favourite's NO at 0.25–0.45.
pub(crate) fn station_spread(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_spread = if variant { 40 } else { 30 };
    let Some((fav, p)) = day.favourite() else {
        day.idle("no fresh book to name a favourite".into(), out);
        return;
    };
    let mut blockers: Vec<String> = day.outside(10 * 60, 16 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            let at = r.interval_end;
            let mut temps: Vec<(String, i32)> = r
                .mean
                .map(|m| vec![("Schiphol".to_owned(), m.tenths())])
                .unwrap_or_default();
            for n in day.lab.neighbours {
                if let Some(m) = n
                    .readings
                    .iter()
                    .find(|x| x.interval_end == at)
                    .and_then(|x| x.mean)
                {
                    temps.push((n.name.to_owned(), m.tenths()));
                }
            }
            let hi = temps.iter().map(|t| t.1).max().unwrap_or(0);
            let lo = temps.iter().map(|t| t.1).min().unwrap_or(0);
            let list = temps
                .iter()
                .map(|(n, t)| format!("{n} {}", c(*t)))
                .collect::<Vec<_>>()
                .join(", ");
            if temps.len() < 3 {
                blockers.push(format!(
                    "{} stations read at {} UTC < 3 ({list})",
                    temps.len(),
                    at.format("%H:%M")
                ));
            } else if hi - lo < min_spread {
                blockers.push(format!(
                    "stations {} apart < {} ({list})",
                    c(hi - lo),
                    c(min_spread)
                ));
            } else {
                rationale.push(format!(
                    "stations {} apart at {} UTC: {list}",
                    c(hi - lo),
                    at.format("%H:%M")
                ));
            }
        }
    }
    if p < 0.55 {
        blockers.push(format!("favourite {} at {p:.2} < 0.55", fav.label));
    }
    day.taker(
        Buy {
            outcome: fav,
            side: OutcomeSide::No,
            band: (0.25, 0.45),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L57 (`variant`: +0.10). The high's bucket lags its neighbour:
/// 10:00–18:00, the next degree up ≥ 0.06 in 4–7 minutes while the high's
/// bucket has not fallen (≥ −0.01), KNMI within 0.2 °C under the edge or
/// above — the two cannot both be right, and the thermometer sides with
/// the rise → the high's bucket's NO at 0.10–0.75.
pub(crate) fn laggard(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (Some(h), Some(j)) = (day.high_outcome(), day.next_outcome()) else {
        return;
    };
    let min_rise = if variant { 0.10 } else { 0.06 };
    let mut blockers: Vec<String> = day.outside(10 * 60, 18 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let now = day.now();
    let then = day
        .sample_at(now - Duration::minutes(4))
        .filter(|s| s.at >= now - Duration::minutes(7));
    let moved = |o: &wm_core::market::MarketOutcome| {
        Some(day.yes_price(o)? - then?.quote(&o.yes_token)?.mid()?)
    };
    match (moved(j), moved(h)) {
        (Some(dj), Some(dh)) => {
            if dj + 1e-9 < min_rise {
                blockers.push(format!(
                    "{} up {dj:+.3} in 4–7 min < {min_rise:.2}",
                    j.label
                ));
            } else if dh < -0.01 - 1e-9 {
                blockers.push(format!("{} already fell {dh:+.3}: no lag", h.label));
            } else {
                rationale.push(format!(
                    "{} {dj:+.3} in 4–7 min, {} {dh:+.3}: the high's bucket lags",
                    j.label, h.label
                ));
            }
        }
        _ => blockers.push("no prices now and 4–7 min ago".into()),
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m >= day.edge() - 2 => rationale.push(Day::describe(r)),
            Some(m) => blockers.push(format!(
                "KNMI mean {} more than 0.2 °C under the edge {}",
                c(m),
                c(day.edge())
            )),
            None => blockers.push("KNMI mean missing".into()),
        },
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::No,
            band: (0.10, 0.75),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// Whether a report was issued off the routine schedule (a SPECI).
fn special(day: &Day<'_, '_>, r: &super::WxReport) -> bool {
    let m = u8::try_from(r.observed_at.minute()).unwrap_or(0);
    !day.ctx.routine_minutes.is_empty() && !day.ctx.routine_minutes.contains(&m)
}

/// L61 (`variant`: a worsening → NO of the next degree, p = 0.90). A
/// special report's improvement: 09:00–16:00, the latest report a SPECI
/// (off the routine minutes) known 3–20 minutes ago, the sky opening (a
/// ceiling ≤ 1500 ft gone or ≥ 3000 ft) or the rain stopped against the
/// report before, ≤ 2 °C under the high, the next degree up ≤ 0.05 since
/// → its YES at 0.05–0.35. The variant: rain, thunder or a ceiling under
/// 1000 ft arriving → its NO at 0.55–0.92.
pub(crate) fn speci_pulse(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(9 * 60, 16 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let now = day.now();
    let reports = day.reports();
    match reports {
        [.., prev, last] if special(day, last) => {
            let since = now - last.known_at;
            if since < Duration::minutes(3) || since > Duration::minutes(20) {
                blockers.push(format!(
                    "the {} UTC SPECI known {} min ago (3–20 wanted)",
                    last.observed_at.format("%H:%M"),
                    since.num_minutes()
                ));
            }
            let (c0, c1) = (prev.wx.body.ceiling_ft(), last.wx.body.ceiling_ft());
            let (rain0, rain1) = (
                prev.wx.body.precipitation() || prev.wx.body.thunder(),
                last.wx.body.precipitation() || last.wx.body.thunder(),
            );
            let opened = c0.is_some_and(|f| f <= 1500) && c1.is_none_or(|f| f >= 3000);
            let dried = rain0 && !rain1;
            let closed = c1.is_some_and(|f| f < 1000) && c0.is_none_or(|f| f >= 1500);
            let wetted = rain1 && !rain0;
            let what = if variant {
                (closed || wetted).then_some(if wetted {
                    "rain or thunder arrived"
                } else {
                    "the ceiling fell under 1000 ft"
                })
            } else {
                (opened || dried).then_some(if dried {
                    "the rain stopped"
                } else {
                    "the sky opened"
                })
            };
            match what {
                None => blockers.push(format!(
                    "the {} UTC SPECI shows no {} ({} → {})",
                    last.observed_at.format("%H:%M"),
                    if variant { "worsening" } else { "improvement" },
                    prev.wx.summary(),
                    last.wx.summary()
                )),
                Some(w) => rationale.push(format!(
                    "SPECI {} UTC: {w} ({} → {})",
                    last.observed_at.format("%H:%M"),
                    prev.wx.summary(),
                    last.wx.summary()
                )),
            }
            if !variant {
                match last.temp_tenths {
                    Some(t) if t >= (day.high - 2) * 10 => {}
                    Some(t) => {
                        blockers.push(format!("SPECI {} more than 2 °C under the high", c(t)))
                    }
                    None => blockers.push("no temperature in the SPECI".into()),
                }
                match (day.yes_price(j), day.mid_at(j, last.known_at)) {
                    (Some(p), Some(b)) if p - b > 0.05 + 1e-9 => blockers.push(format!(
                        "{} up {:+.3} since the SPECI: priced in",
                        j.label,
                        p - b
                    )),
                    (Some(_), Some(_)) => {}
                    _ => blockers.push("no price at the SPECI and now".into()),
                }
            }
        }
        _ => blockers.push("the latest report is no SPECI".into()),
    }
    let (side, band, p) = if variant {
        (OutcomeSide::No, (0.55, 0.92), Some((0.90, 0.005)))
    } else {
        (OutcomeSide::Yes, (0.05, 0.35), None)
    };
    day.taker(
        Buy {
            outcome: j,
            side,
            band,
            p_win: p,
            rationale,
        },
        blockers,
        out,
    );
}

/// L62 (`variant`: ≤ 5 km). A haze lid: 11:00–15:00, the last two reports
/// with visibility ≤ 7 km in dry air (no rain, no mist or fog, the dew
/// point ≥ 5 °C under the temperature) or haze or smoke reported —
/// particles that dim the sun — and ≥ 1 °C under the high → NO of the next
/// degree at 0.60–0.95 (p = 0.93).
pub(crate) fn haze_lid(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let max_vis = if variant { 5000 } else { 7000 };
    let mut blockers: Vec<String> = day.outside(11 * 60, 15 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let hazy = |r: &super::WxReport| {
        let b = &r.wx.body;
        let named = b
            .weather
            .iter()
            .any(|w| w.contains("HZ") || w.contains("FU"));
        let dry = r
            .temp_tenths
            .zip(r.dew_tenths)
            .is_some_and(|(t, d)| t - d >= 50);
        let dim = b.visibility_m.is_some_and(|v| v <= max_vis)
            && !b.precipitation()
            && !b.fog_or_mist()
            && dry;
        named || dim
    };
    match day.reports() {
        [.., prev, last] => {
            if hazy(prev) && hazy(last) {
                rationale.push(format!(
                    "haze in the last two reports ({}; {})",
                    prev.wx.summary(),
                    last.wx.summary()
                ));
            } else {
                blockers.push(format!(
                    "no dry haze ≤ {} km in the last two reports ({}; {})",
                    max_vis / 1000,
                    prev.wx.summary(),
                    last.wx.summary()
                ));
            }
            match last.temp_tenths {
                Some(t) if t <= (day.high - 1) * 10 => {}
                Some(t) => blockers.push(format!("METAR {} within 1 °C of the high", c(t))),
                None => blockers.push("no temperature in the latest METAR".into()),
            }
        }
        _ => blockers.push("fewer than two reports today".into()),
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::No,
            band: (0.60, 0.95),
            p_win: Some((0.93, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}
