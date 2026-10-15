//! L30, L41, L49 and L65: what the market remembers and what it forgets.
//! Traders anchor on yesterday's high and on the forecast's round number;
//! they forget that yesterday's rain is still in the ground, that the
//! forecast can sit right on a rounding edge, and that on a cold day the
//! high may already have come at midnight.

use super::day::{Buy, Day, c, hm, round_half_up};
use super::forecast::usable;
use crate::strategy::StrategyOutput;
use wm_core::market::OutcomeSide;

/// Yesterday's high (whole °C) from its METARs, when they cover its
/// afternoon (12:00–17:00 local).
fn yesterday_high(day: &Day<'_, '_>) -> Result<i32, String> {
    let rs = day.lab.yesterday_reports;
    let afternoon = rs
        .iter()
        .filter(|r| (12 * 60..17 * 60).contains(&day.minute_of(r.observed_at)))
        .count();
    if afternoon < 4 {
        return Err(format!(
            "{afternoon} of yesterday's METARs from its afternoon (needs ≥ 4; {} in all)",
            rs.len()
        ));
    }
    rs.iter()
        .filter_map(|r| r.temp_tenths)
        .max()
        .map(|t| round_half_up(f64::from(t) / 10.0))
        .ok_or_else(|| "no temperature in yesterday's METARs".to_owned())
}

/// L30 (`variant`: ≥ 2.5 °C apart). Market anchored on yesterday's high:
/// 08:00–11:00, the favourite holds yesterday's high while the day's
/// forecast maximum lies ≥ 1.5 °C from it → YES of the forecast's bucket at
/// 0.05–0.35.
pub(crate) fn yesterday_anchor(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let min_gap = if variant { 25 } else { 15 };
    let (fav, p) = match day.favourite() {
        Some(x) => x,
        None => {
            day.idle("no fresh book to name a favourite".into(), out);
            return;
        }
    };
    let Some(fmax) = fc.day_max_tenths() else {
        day.idle("the forecast has no maximum".into(), out);
        return;
    };
    let fv = round_half_up(f64::from(fmax) / 10.0);
    let Some(target) = day.outcome_of(fv).filter(|o| day.alive(o)) else {
        day.idle(format!("no live bucket for the forecast's {fv} °C"), out);
        return;
    };
    let mut blockers: Vec<String> = day.outside(8 * 60, 11 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match yesterday_high(day) {
        Err(why) => blockers.push(why),
        Ok(y) => {
            if !fav.bucket.contains(y) {
                blockers.push(format!(
                    "the favourite {} ({p:.2}) does not hold yesterday's high {y} °C",
                    fav.label
                ));
            } else if (fmax - y * 10).abs() < min_gap {
                blockers.push(format!(
                    "the forecast {} within {} of yesterday's high {y} °C",
                    c(fmax),
                    c(min_gap)
                ));
            } else if target.condition_id == fav.condition_id {
                blockers.push("the forecast's bucket is the favourite".into());
            } else {
                rationale.push(format!(
                    "the favourite {} ({p:.2}) is yesterday's high {y} °C; the forecast says {}",
                    fav.label,
                    c(fmax)
                ));
            }
        }
    }
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

/// L41 (`variant`: ≥ 8 rainy reports). Wet ground from yesterday's rain:
/// 08:00–14:00, ≥ 4 of yesterday's METARs with rain, today's forecast
/// maximum ≥ 1 °C above yesterday's high — the sun dries the ground before
/// it warms the air → NO of the bucket a degree above the forecast's at
/// 0.70–0.97 (p = 0.94).
pub(crate) fn wet_ground(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let min_rainy = if variant { 8 } else { 4 };
    let Some(fmax) = fc.day_max_tenths() else {
        day.idle("the forecast has no maximum".into(), out);
        return;
    };
    let v = round_half_up(f64::from(fmax) / 10.0) + 1;
    let Some(target) = day.outcome_of(v).filter(|o| day.alive(o)) else {
        day.idle(
            format!("no live bucket for {v} °C (the forecast's + 1)"),
            out,
        );
        return;
    };
    let mut blockers: Vec<String> = day.outside(8 * 60, 14 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let rainy = day
        .lab
        .yesterday_reports
        .iter()
        .filter(|r| r.wx.precipitation_now_or_recent())
        .count();
    if rainy < min_rainy {
        blockers.push(format!(
            "{rainy} of yesterday's {} METARs with rain < {min_rainy}",
            day.lab.yesterday_reports.len()
        ));
    } else {
        rationale.push(format!("{rainy} of yesterday's METARs with rain"));
    }
    match yesterday_high(day) {
        Err(why) => blockers.push(why),
        Ok(y) if fmax < (y + 1) * 10 => blockers.push(format!(
            "the forecast {} not ≥ 1 °C above yesterday's high {y} °C",
            c(fmax)
        )),
        Ok(y) => rationale.push(format!(
            "the forecast {} after yesterday's {y} °C: the wet ground holds it back",
            c(fmax)
        )),
    }
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::No,
            band: (0.70, 0.97),
            p_win: Some((0.94, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L49 (`variant`: within 0.2 °C, ≤ 0.45 ×). Forecast on a rounding edge:
/// 08:00–12:00, the forecast maximum within 0.3 °C of an x.5 edge — a coin
/// flip between two whole degrees — while the market prices one of them ≤
/// 0.55 × the other → YES of the cheaper one at 0.10–0.45.
pub(crate) fn boundary_straddle(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(fc) = usable(day, out) else {
        return;
    };
    let (near, ratio) = if variant { (2, 0.45) } else { (3, 0.55) };
    let Some(fmax) = fc.day_max_tenths() else {
        day.idle("the forecast has no maximum".into(), out);
        return;
    };
    // The edge nearest the forecast: x.5 between `lo` and `lo + 1`.
    let lo = round_half_up(f64::from(fmax - 5) / 10.0);
    let edge = lo * 10 + 5;
    let (Some(a), Some(b)) = (day.outcome_of(lo), day.outcome_of(lo + 1)) else {
        day.idle(format!("no buckets for {lo} and {} °C", lo + 1), out);
        return;
    };
    let mut blockers: Vec<String> = day.outside(8 * 60, 12 * 60).into_iter().collect();
    if (fmax - edge).abs() > near {
        blockers.push(format!(
            "the forecast {} is {} from the edge {} (needs ≤ {})",
            c(fmax),
            c((fmax - edge).abs()),
            c(edge),
            c(near)
        ));
    }
    if a.condition_id == b.condition_id || !day.alive(a) || !day.alive(b) {
        day.idle(
            format!("{lo} and {} °C are not two live buckets", lo + 1),
            out,
        );
        return;
    }
    let (pa, pb) = (day.yes_price(a), day.yes_price(b));
    let (Some(pa), Some(pb)) = (pa, pb) else {
        day.idle("no fresh books on the two buckets".into(), out);
        return;
    };
    let (cheap, pc, pd) = if pa <= pb { (a, pa, pb) } else { (b, pb, pa) };
    if pc > ratio * pd + 1e-9 {
        blockers.push(format!(
            "{} at {pc:.2} vs {pd:.2}: more than {ratio:.2} × the other",
            cheap.label
        ));
    }
    day.taker(
        Buy {
            outcome: cheap,
            side: OutcomeSide::Yes,
            band: (0.10, 0.45),
            p_win: None,
            rationale: vec![format!(
                "the forecast {} sits on the edge {}: {} at {pc:.2} against {pd:.2}",
                c(fmax),
                c(edge),
                cheap.label
            )],
        },
        blockers,
        out,
    );
}

/// L65 (`variant`: the forecast ≥ 0.5 °C under the high). Midnight high:
/// 08:00–11:00, the high first reached before 02:00 (warm air leaving
/// overnight), KNMI ≥ 1 °C under it now and the forecast's rest of the day
/// ≤ the high + 0.4 °C → YES of the high's bucket at 0.10–0.60.
pub(crate) fn midnight_high(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let Some(fc) = usable(day, out) else {
        return;
    };
    let margin = if variant { -5 } else { 4 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 11 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day
        .reports()
        .iter()
        .find(|r| r.temp_tenths.is_some_and(|t| t >= day.high * 10))
    {
        None => blockers.push("no report reached the high".into()),
        Some(r) => {
            let m = day.minute_of(r.observed_at);
            if m >= 2 * 60 {
                blockers.push(format!(
                    "the high first came at {} (before 02:00 wanted)",
                    hm(m)
                ));
            } else {
                rationale.push(format!(
                    "the high {} came at {} local",
                    c(day.high * 10),
                    hm(m)
                ));
            }
        }
    }
    match fc.remaining_max_tenths_at(day.now()) {
        None => blockers.push("the forecast has no value for the rest of the day".into()),
        Some(f) if f > day.high * 10 + margin => blockers.push(format!(
            "the forecast still reaches {} (> the high {:+.1} °C)",
            c(f),
            f64::from(margin) / 10.0
        )),
        Some(f) => rationale.push(format!("the forecast's rest of the day peaks at {}", c(f))),
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m <= day.high * 10 - 10 => rationale.push(Day::describe(r)),
            Some(m) => blockers.push(format!("KNMI mean {} within 1 °C of the high", c(m))),
            None => blockers.push("KNMI mean missing".into()),
        },
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::Yes,
            band: (0.10, 0.60),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}
