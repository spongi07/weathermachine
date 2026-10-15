//! L27, L29, L32, L37, L42 and L43: the air at Schiphol beyond its
//! temperature. KNMI's ten-minute dew point, humidity, wind, gusts,
//! pressure and sunshine say how the boundary layer behaves before the
//! thermometer and the METAR do: drying and gusts as thermals mix warm air
//! down, a dropping wind under the sun, falling pressure ahead of weather,
//! saturated air that cannot warm, a sea wind all day.

use super::day::{Buy, Day, RISE_BAND, c};
use crate::strategy::StrategyOutput;
use chrono::Duration;
use wm_core::market::{MarketOutcome, OutcomeSide};
use wm_core::metar_wx::in_arc;

/// KNMI's parameter names the rules read (the collection's).
pub(crate) const DEW_POINT: &str = "td";
pub(crate) const HUMIDITY: &str = "rh";
pub(crate) const WIND: &str = "ff";
pub(crate) const GUST: &str = "fx";
pub(crate) const DIRECTION: &str = "dd";
pub(crate) const PRESSURE: &str = "pp";
pub(crate) const SUNSHINE: &str = "ss";

/// KNMI's latest mean temperature (tenths) and its change over `minutes`,
/// or why not.
fn knmi_trend(day: &Day<'_, '_>, minutes: i64) -> Result<(i32, i32, String), String> {
    let r = day.latest_reading()?;
    let now = r.mean.map(|m| m.tenths()).ok_or("KNMI mean missing")?;
    let then = day
        .mean_at(r.interval_end - Duration::minutes(minutes))
        .ok_or_else(|| format!("no KNMI reading {minutes} min before the latest"))?;
    Ok((now, now - then, Day::describe(r)))
}

/// The bucket above the market's favourite when it can still win.
fn above_favourite<'c>(day: &Day<'c, '_>) -> Result<&'c MarketOutcome, String> {
    let (fav, p) = day.favourite().ok_or("no fresh book to name a favourite")?;
    let up = day
        .neighbour(fav, 1)
        .filter(|o| day.alive(o))
        .ok_or_else(|| format!("no bucket above the favourite {} ({p:.2})", fav.label))?;
    Ok(up)
}

/// L27 (`variant`: a drop of ≥ 2.5 °C). Mixing surge: 09:00–13:00, KNMI's
/// dew point down ≥ 1.5 °C in an hour while its temperature rose ≥ 1.0 °C —
/// thermals mix the dry air above down, and the afternoon runs warmer than
/// the morning promised → YES of the bucket above the favourite at
/// 0.04–0.30.
pub(crate) fn mixing_surge(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let target = match above_favourite(day) {
        Ok(o) => o,
        Err(why) => {
            if let Some(h) = day.outcome_of(day.high) {
                day.note(h, OutcomeSide::Yes, vec![why], out);
            }
            return;
        }
    };
    let min_drop = if variant { 2.5 } else { 1.5 };
    let mut blockers: Vec<String> = day.outside(9 * 60, 13 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.extra_latest(DEW_POINT) {
        Err(why) => blockers.push(why),
        Ok((t, td)) => match day.extra_at(DEW_POINT, t - Duration::minutes(60)) {
            None => blockers.push("no KNMI dew point an hour before the latest".into()),
            Some(before) => {
                let drop = before - td;
                if drop + 1e-9 < min_drop {
                    blockers.push(format!(
                        "dew point {td:.1} °C after {before:.1} °C an hour before: down {drop:.1} < {min_drop:.1}"
                    ));
                } else {
                    rationale.push(format!(
                        "KNMI dew point down {drop:.1} °C in an hour ({before:.1} → {td:.1} °C)"
                    ));
                }
            }
        },
    }
    match knmi_trend(day, 60) {
        Err(why) => blockers.push(why),
        Ok((_, rise, d)) => {
            if rise < 10 {
                blockers.push(format!("KNMI up {} in an hour < 1.0 °C", c(rise)));
            } else {
                rationale.push(format!("{d}: up {} in an hour", c(rise)));
            }
        }
    }
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.04, 0.30),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L29 (`variant`: ≤ 2.5 m/s at half the speed). Wind slack under the sun:
/// 11:00–15:00, KNMI's wind ≤ 3.5 m/s and ≤ 0.6 × its speed an hour before
/// (≥ 4.0 m/s then), sunshine ≥ 7 of 10 minutes in two of the last three
/// readings, KNMI ≥ 1.2 °C under the edge at most → NO of the high's
/// bucket (a new high, however far) at 0.10–0.70. Less wind mixes less
/// cool air into the sunlit surface layer.
pub(crate) fn wind_slack(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let (max_ff, ratio) = if variant { (2.5, 0.5) } else { (3.5, 0.6) };
    let mut blockers: Vec<String> = day.outside(11 * 60, 15 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.extra_latest(WIND) {
        Err(why) => blockers.push(why),
        Ok((t, ff)) => match day.extra_at(WIND, t - Duration::minutes(60)) {
            None => blockers.push("no KNMI wind an hour before the latest".into()),
            Some(before) => {
                if ff > max_ff + 1e-9 || before < 4.0 || ff > ratio * before + 1e-9 {
                    blockers.push(format!(
                        "wind {ff:.1} m/s after {before:.1} m/s an hour before (needs ≤ {max_ff:.1}, ≤ {ratio:.1} × an earlier ≥ 4.0)"
                    ));
                } else {
                    rationale.push(format!(
                        "KNMI wind down to {ff:.1} m/s from {before:.1} m/s an hour before"
                    ));
                }
            }
        },
    }
    let sun = day.extra_window(SUNSHINE, day.now(), 35);
    let last3 = &sun[sun.len().saturating_sub(3)..];
    let sunny = last3.iter().filter(|(_, s)| *s >= 7.0).count();
    if last3.len() < 3 || sunny < 2 {
        blockers.push(format!(
            "sunshine ≥ 7 min in {sunny} of the last {} readings < 2 of 3",
            last3.len()
        ));
    } else {
        rationale.push(format!("sunny in {sunny} of the last 3 readings"));
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m >= day.edge() - 12 => rationale.push(Day::describe(r)),
            Some(m) => blockers.push(format!(
                "KNMI mean {} more than 1.2 °C under the edge {}",
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
            band: RISE_BAND,
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L32 (`variant`: gusts ≥ 5 m/s over the mean). Gusts begin: 10:00–13:30,
/// KNMI's gusts ≥ 4 m/s above its mean wind (≥ 3 m/s) in each of the last
/// three readings, ≤ 2 m/s above it an hour before — thermals reach the
/// wind above and mix it down, and with it warmer air → NO of the high's
/// bucket (a new high, however far) at 0.10–0.70.
pub(crate) fn gust_onset(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let min_excess = if variant { 5.0 } else { 4.0 };
    let mut blockers: Vec<String> = day.outside(10 * 60, 13 * 60 + 30).into_iter().collect();
    let mut rationale = Vec::new();
    match day.extra_latest(GUST) {
        Err(why) => blockers.push(why),
        Ok((t, _)) => {
            let pairs: Vec<(f64, f64)> = day
                .extras()
                .iter()
                .filter(|r| r.interval_end <= t)
                .filter_map(|r| Some((r.get(WIND)?, r.get(GUST)?)))
                .collect();
            let last3 = &pairs[pairs.len().saturating_sub(3)..];
            let before = day
                .extra_at(WIND, t - Duration::minutes(60))
                .zip(day.extra_at(GUST, t - Duration::minutes(60)));
            let gusty = last3.len() == 3
                && last3
                    .iter()
                    .all(|(ff, fx)| *ff >= 3.0 && fx - ff + 1e-9 >= min_excess);
            match before {
                None => blockers.push("no KNMI wind and gust an hour before the latest".into()),
                Some((ff0, fx0)) if fx0 - ff0 > 2.0 + 1e-9 => blockers.push(format!(
                    "already gusty an hour before ({fx0:.1} over {ff0:.1} m/s)"
                )),
                Some(_) if !gusty => blockers.push(format!(
                    "gusts over the mean in the last readings: {} (needs ≥ {min_excess:.1} m/s on ≥ 3 m/s in all three)",
                    last3
                        .iter()
                        .map(|(ff, fx)| format!("{:.1}", fx - ff))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Some((ff0, fx0)) => rationale.push(format!(
                    "gusts {:.1} m/s over the mean now, {:.1} an hour before",
                    last3.last().map_or(0.0, |(ff, fx)| fx - ff),
                    fx0 - ff0
                )),
            }
        }
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::No,
            band: RISE_BAND,
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L37 (`variant`: −3 hPa). Falling pressure: 10:00–16:00, KNMI's pressure
/// down ≥ 2 hPa in three hours (weather on its way: cloud and rain ahead
/// of a low) and KNMI ≥ 0.8 °C under the edge → NO of the next degree at
/// 0.55–0.92 (p = 0.90).
pub(crate) fn pressure_fall(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_fall = if variant { 3.0 } else { 2.0 };
    let mut blockers: Vec<String> = day.outside(10 * 60, 16 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.extra_latest(PRESSURE) {
        Err(why) => blockers.push(why),
        Ok((t, pp)) => match day.extra_at(PRESSURE, t - Duration::minutes(180)) {
            None => blockers.push("no KNMI pressure three hours before the latest".into()),
            Some(before) => {
                let fall = before - pp;
                if fall + 1e-9 < min_fall {
                    blockers.push(format!(
                        "pressure {pp:.1} hPa after {before:.1} three hours before: down {fall:.1} < {min_fall:.1}"
                    ));
                } else {
                    rationale.push(format!(
                        "KNMI pressure down {fall:.1} hPa in three hours ({before:.1} → {pp:.1})"
                    ));
                }
            }
        },
    }
    under_edge(day, 8, &mut blockers, &mut rationale);
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::No,
            band: (0.55, 0.92),
            p_win: Some((0.90, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// KNMI's latest mean ≥ `margin` tenths under the edge.
fn under_edge(
    day: &Day<'_, '_>,
    margin: i32,
    blockers: &mut Vec<String>,
    rationale: &mut Vec<String>,
) {
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m <= day.edge() - margin => rationale.push(Day::describe(r)),
            Some(m) => blockers.push(format!(
                "KNMI mean {} not ≥ {} under the edge {}",
                c(m),
                c(margin),
                c(day.edge())
            )),
            None => blockers.push("KNMI mean missing".into()),
        },
    }
}

/// L42 (`variant`: ≥ 97 %). Saturated air: 10:00–16:00, KNMI's humidity
/// ≥ 93 % in every reading of the last hour (≥ 5), its temperature up ≤
/// 0.3 °C in that hour and ≥ 0.6 °C under the edge — sunlight goes into
/// evaporation, not warmth → NO of the next degree at 0.60–0.95 (p = 0.93).
pub(crate) fn saturated(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_rh = if variant { 97.0 } else { 93.0 };
    let mut blockers: Vec<String> = day.outside(10 * 60, 16 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.extra_latest(HUMIDITY) {
        Err(why) => blockers.push(why),
        Ok((t, _)) => {
            let hour = day.extra_window(HUMIDITY, t, 60);
            let low = hour.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
            if hour.len() < 5 {
                blockers.push(format!(
                    "{} humidity readings in the last hour < 5",
                    hour.len()
                ));
            } else if low + 1e-9 < min_rh {
                blockers.push(format!(
                    "humidity down to {low:.0} % in the last hour < {min_rh:.0} %"
                ));
            } else {
                rationale.push(format!("KNMI humidity ≥ {low:.0} % all hour"));
            }
        }
    }
    match knmi_trend(day, 60) {
        Err(why) => blockers.push(why),
        Ok((_, rise, _)) if rise > 3 => {
            blockers.push(format!("KNMI up {} in an hour > 0.3 °C", c(rise)));
        }
        Ok(_) => {}
    }
    under_edge(day, 6, &mut blockers, &mut rationale);
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

/// L43 (`variant`: ≥ 95 %). An onshore day: 12:00–17:00, ≥ 80 % of KNMI's
/// wind directions since 09:00 (≥ 12 readings) from the sea (240–020°), the
/// METAR wind ≥ 8 kt, KNMI ≥ 0.6 °C under the edge → NO of the next degree
/// at 0.65–0.96 (p = 0.94). The North Sea caps a day that blows from it.
pub(crate) fn onshore_day(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let share = if variant { 0.95 } else { 0.80 };
    let mut blockers: Vec<String> = day.outside(12 * 60, 17 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.local(9 * 60) {
        None => blockers.push("09:00 does not exist today".into()),
        Some(nine) => {
            let minutes = (day.now() - nine).num_minutes();
            let dirs = day.extra_window(DIRECTION, day.now(), minutes);
            let sea = dirs
                .iter()
                .filter(|(_, d)| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let d = d.round().rem_euclid(360.0) as u16;
                    in_arc(d, 240, 20)
                })
                .count();
            #[allow(clippy::cast_precision_loss)]
            let frac = if dirs.is_empty() {
                0.0
            } else {
                sea as f64 / dirs.len() as f64
            };
            if dirs.len() < 12 {
                blockers.push(format!(
                    "{} KNMI wind directions since 09:00 < 12",
                    dirs.len()
                ));
            } else if frac + 1e-9 < share {
                blockers.push(format!(
                    "{:.0} % of the wind since 09:00 from the sea (240–020°) < {:.0} %",
                    100.0 * frac,
                    100.0 * share
                ));
            } else {
                rationale.push(format!(
                    "{:.0} % of {} KNMI wind directions since 09:00 from the sea",
                    100.0 * frac,
                    dirs.len()
                ));
            }
        }
    }
    match day.latest_report().and_then(|r| r.wx.wind()) {
        Some(w) if w.speed_kt >= 8 => {}
        Some(w) => blockers.push(format!("METAR wind {} kt < 8", w.speed_kt)),
        None => blockers.push("no wind in the latest METAR".into()),
    }
    under_edge(day, 6, &mut blockers, &mut rationale);
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::No,
            band: (0.65, 0.96),
            p_win: Some((0.94, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}
