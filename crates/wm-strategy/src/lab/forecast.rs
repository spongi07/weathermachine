//! L14–L17: the forecast's hour-by-hour path, not its maximum.

use super::day::{Buy, Day, c, hm, round_half_up};
use crate::forecast::ForecastDay;
use crate::strategy::StrategyOutput;
use chrono::{DateTime, Duration, Utc};
use wm_core::market::OutcomeSide;

/// The forecast at `t` (tenths), linear between the hourly values.
pub(crate) fn forecast_at(fc: &ForecastDay, t: DateTime<Utc>) -> Option<f64> {
    let i = fc.hourly.partition_point(|(x, _)| *x <= t);
    let (t0, v0) = *fc.hourly.get(i.checked_sub(1)?)?;
    let Some(&(t1, v1)) = fc.hourly.get(i) else {
        return Some(f64::from(v0));
    };
    let span = (t1 - t0).num_seconds() as f64;
    let w = if span > 0.0 {
        (t - t0).num_seconds() as f64 / span
    } else {
        0.0
    };
    Some(f64::from(v0) + w * f64::from(v1 - v0))
}

/// When the forecast first reaches its maximum.
pub(crate) fn forecast_peak(fc: &ForecastDay) -> Option<DateTime<Utc>> {
    fc.hourly
        .iter()
        .fold(
            None,
            |best: Option<(DateTime<Utc>, i32)>, &(t, v)| match best {
                Some((_, b)) if b >= v => best,
                _ => Some((t, v)),
            },
        )
        .map(|(t, _)| t)
}

/// The forecast (usable now) or why not, shown on the high's bucket.
fn usable<'a>(day: &Day<'_, 'a>, out: &mut StrategyOutput) -> Option<&'a ForecastDay> {
    let why = match day.lab.forecast {
        None => "no day-1 forecast for today (the [forecast] loop, Open-Meteo)".to_owned(),
        Some(fc) if day.now() < fc.known_at => format!(
            "the forecast is usable from {} UTC",
            fc.known_at.format("%H:%M")
        ),
        Some(fc) => return Some(fc),
    };
    if let Some(h) = day.outcome_of(day.high) {
        day.note(h, OutcomeSide::Yes, vec![why], out);
    }
    None
}

/// The latest report's time (the forecast is read there, as the replay
/// does at each report).
fn report_time(day: &Day<'_, '_>) -> DateTime<Utc> {
    day.latest_report()
        .map_or_else(|| day.last_metar_at(), |r| r.observed_at)
}

/// L14 (`variant`: λ = 1.0). The morning's departure from the hourly
/// forecast persists: 10:00–12:30, predicted maximum = the forecast's
/// remaining maximum + 0.7 × (observed − forecast now); when its bucket is
/// not the market's favourite, YES of it at 0.05–0.35.
pub(crate) fn departure(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let lambda = if variant { 1.0 } else { 0.7 };
    let at = report_time(day);
    let (Some(now), Some(rest)) = (forecast_at(fc, at), fc.remaining_max_tenths(at)) else {
        return;
    };
    let departure = (f64::from(day.f.current_tenths) - now) / 10.0;
    let predicted = f64::from(rest) / 10.0 + lambda * departure;
    let v = round_half_up(predicted).max(day.high);
    let Some(target) = day.outcome_of(v) else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(10 * 60, 12 * 60 + 30).into_iter().collect();
    match day.favourite() {
        Some((fav, p)) if fav.condition_id == target.condition_id => blockers.push(format!(
            "{} is already the market's favourite ({p:.2})",
            target.label
        )),
        _ => {}
    }
    let rationale = vec![format!(
        "observed {} vs forecast {} at {} UTC: {departure:+.1} °C; the forecast's remaining maximum {} + {lambda:.1} × {departure:+.1} = {predicted:.1} °C → {}",
        c(day.f.current_tenths),
        c(round_half_up(now)),
        at.format("%H:%M"),
        c(rest),
        target.label
    )];
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.05, 0.35),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L15 (`variant`: +60′). The forecast knows the day's own peak hour: ≥
/// 120 minutes after it (and from 12:00), the forecast falling ≥ 1 °C,
/// ≥ 1 °C under the high → YES of the high's bucket at 0.70–0.95
/// (p = 0.96). One trade a day.
pub(crate) fn own_peak(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let Some(h) = day.outcome_of(day.high) else {
        return;
    };
    let after = Duration::minutes(if variant { 60 } else { 120 });
    let mut blockers = Vec::new();
    if day.traded_today() {
        blockers.push("already traded today (one trade a day)".into());
    }
    let mut rationale = Vec::new();
    match forecast_peak(fc) {
        None => blockers.push("the forecast has no peak".into()),
        Some(peak) if peak + after >= day.day_end() => blockers.push(format!(
            "the forecast peaks at {} UTC: + {} min is past the day",
            peak.format("%H:%M"),
            after.num_minutes()
        )),
        Some(peak) => {
            let from = day.minute_of(peak + after).max(12 * 60);
            if day.minute() < from {
                blockers.push(format!(
                    "before {} (the forecast's peak {} + {} min, from 12:00)",
                    hm(from),
                    hm(day.minute_of(peak)),
                    after.num_minutes()
                ));
            } else {
                rationale.push(format!(
                    "past the forecast's own peak ({}) by ≥ {} min",
                    hm(day.minute_of(peak)),
                    after.num_minutes()
                ));
            }
        }
    }
    if day.f.drop_tenths < 10 {
        blockers.push(format!(
            "METAR {} under the high < 1.0 °C",
            c(day.f.drop_tenths)
        ));
    }
    match fc.rise_tenths(report_time(day)) {
        Some(r) if r <= -10 => rationale.push(format!("the forecast falls {} from here", c(-r))),
        Some(r) => blockers.push(format!("forecast rise {} > −1.0 °C", c(r))),
        None => blockers.push("no forecast rise".into()),
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::Yes,
            band: (0.70, 0.95),
            p_win: Some((0.96, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L16 (`variant`: rise ≥ 0.5 °C). On warm-advection days the forecast
/// peaks in the evening: when its 17–24 h maximum beats its 10–17 h one by
/// ≥ 0.5 °C, 14:00–18:00, the forecast still rising ≥ 1 °C, within 1 °C of
/// the high → YES of the next degree at 0.05–0.30. One trade a day.
pub(crate) fn evening_high(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_rise = if variant { 5 } else { 10 };
    let mut blockers: Vec<String> = day.outside(14 * 60, 18 * 60).into_iter().collect();
    if day.traded_today() {
        blockers.push("already traded today (one trade a day)".into());
    }
    let mut rationale = Vec::new();
    let max_in = |a: DateTime<Utc>, b: DateTime<Utc>| {
        fc.hourly
            .iter()
            .filter(|(t, _)| *t >= a && *t < b)
            .map(|(_, v)| *v)
            .max()
    };
    match (day.local(10 * 60), day.local(17 * 60)) {
        (Some(ten), Some(five)) => match (
            max_in(ten, five),
            max_in(five, day.day_end() + Duration::seconds(1)),
        ) {
            (Some(daytime), Some(evening)) if evening >= daytime + 5 => rationale.push(format!(
                "an evening high: the forecast's 17–24 h maximum {} beats its 10–17 h {}",
                c(evening),
                c(daytime)
            )),
            (Some(daytime), Some(evening)) => blockers.push(format!(
                "no evening high in the forecast (17–24 h {} vs 10–17 h {})",
                c(evening),
                c(daytime)
            )),
            _ => blockers.push("the forecast does not cover the day's hours".into()),
        },
        _ => blockers.push("local times of the day unknown".into()),
    }
    if day.f.current_tenths < day.high * 10 - 10 {
        blockers.push(format!(
            "{} more than 1 °C under the high",
            c(day.f.current_tenths)
        ));
    }
    match fc.rise_tenths(report_time(day)) {
        Some(r) if r >= min_rise => rationale.push(format!("the forecast still rises {}", c(r))),
        Some(r) => blockers.push(format!("forecast rise {} < {}", c(r), c(min_rise))),
        None => blockers.push("no forecast rise".into()),
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::Yes,
            band: (0.05, 0.30),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L17 (`variant`: the full error). Forecast errors persist from one day
/// to the next: 07:00–10:00, today's forecast maximum + 0.5 × (yesterday's
/// observed high − its forecast maximum); when that bucket differs from the
/// raw forecast's, YES of it at 0.05–0.30.
pub(crate) fn yesterday_error(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let Some(e) = day.lab.yesterday_error_tenths else {
        if let Some(h) = day.outcome_of(day.high) {
            day.note(
                h,
                OutcomeSide::Yes,
                vec![
                    "yesterday's forecast error unknown (no forecast or high for yesterday)".into(),
                ],
                out,
            );
        }
        return;
    };
    let Some(fmax) = fc.day_max_tenths() else {
        return;
    };
    let rho = if variant { 1.0 } else { 0.5 };
    let raw = day.outcome_of(round_half_up(f64::from(fmax) / 10.0));
    let adjusted = (f64::from(fmax) + rho * f64::from(e)) / 10.0;
    let Some(target) = day.outcome_of(round_half_up(adjusted).max(day.high)) else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(7 * 60, 10 * 60).into_iter().collect();
    if raw.is_some_and(|r| r.condition_id == target.condition_id) {
        blockers.push(format!("{} is the raw forecast's bucket too", target.label));
    }
    let rationale = vec![format!(
        "forecast maximum {} + {rho:.1} × yesterday's error {} = {adjusted:.1} °C → {}",
        c(fmax),
        c(e),
        target.label
    )];
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.05, 0.30),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}
