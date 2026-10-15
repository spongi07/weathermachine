//! L26, L31 and L36: the air on its way. METARs of airports 60–300 km
//! upstream show the air that reaches Schiphol hours later — warmer,
//! clearer, or behind a front. The surface wind carries it at about one
//! and a half times its own speed (the wind above the ground is faster),
//! so each rule knows when it arrives and trades only what arrives in time.

use super::UpstreamReadings;
use super::WxReport;
use super::day::{Buy, Day, c};
use super::solar::angle_between;
use crate::strategy::StrategyOutput;
use chrono::{DateTime, Duration, Utc};
use wm_core::market::OutcomeSide;
use wm_core::metar_wx::Wind;

/// The air moves at this multiple of the surface wind.
const STEERING: f64 = 1.5;
/// An upstream report older than this is no picture of the air now.
const MAX_REPORT_AGE_MINUTES: i64 = 75;

/// The latest METAR's wind at Schiphol with a direction and at least
/// `min_kt`.
fn wind(day: &Day<'_, '_>, min_kt: u16) -> Result<(u16, u16), String> {
    match day.latest_report().and_then(|r| r.wx.wind()) {
        None => Err("no wind in the latest METAR".into()),
        Some(Wind {
            direction: None, ..
        }) => Err("variable or calm wind: no upstream".into()),
        Some(w) if w.speed_kt < min_kt => Err(format!("wind {} kt < {min_kt}", w.speed_kt)),
        Some(Wind {
            direction: Some(d),
            speed_kt,
            ..
        }) => Ok((d, speed_kt)),
    }
}

/// When air `distance_km` upstream at `from` reaches the station with a
/// surface wind of `speed_kt`.
fn arrival(from: DateTime<Utc>, distance_km: f64, speed_kt: u16) -> DateTime<Utc> {
    let kmh = f64::from(speed_kt.max(1)) * 1.852 * STEERING;
    #[allow(clippy::cast_possible_truncation)]
    let minutes = (distance_km / kmh * 60.0).round() as i64;
    from + Duration::minutes(minutes)
}

/// The airports within `arc`° of the wind's direction and `km` away, or
/// why there are none.
fn upwind<'a>(
    day: &Day<'_, 'a>,
    dir: u16,
    arc: f64,
    km: (f64, f64),
) -> Result<Vec<&'a UpstreamReadings<'a>>, String> {
    let all = day.upstream();
    if all.is_empty() {
        return Err("no upstream airport configured or read ([[lab.upstream]])".into());
    }
    let found: Vec<&UpstreamReadings<'_>> = all
        .iter()
        .filter(|u| angle_between(f64::from(dir), u.bearing_deg) <= arc)
        .filter(|u| (km.0..=km.1).contains(&u.distance_km))
        .collect();
    if found.is_empty() {
        Err(format!(
            "wind from {dir:03}°: no airport within {arc:.0}° at {:.0}–{:.0} km ({})",
            km.0,
            km.1,
            all.iter()
                .map(|u| format!("{} {:.0}° {:.0} km", u.icao, u.bearing_deg, u.distance_km))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else {
        Ok(found)
    }
}

/// An airport's reports known by now, oldest first.
fn known<'a>(day: &Day<'_, '_>, u: &UpstreamReadings<'a>) -> Vec<&'a WxReport> {
    u.reports
        .iter()
        .filter(|r| r.known_at <= day.now())
        .collect()
}

/// Its latest report, when fresh.
fn latest<'a>(day: &Day<'_, '_>, u: &UpstreamReadings<'a>) -> Result<&'a WxReport, String> {
    let r = known(day, u)
        .last()
        .copied()
        .ok_or_else(|| format!("{}: no report", u.icao))?;
    let age = day.now() - r.observed_at;
    if age > Duration::minutes(MAX_REPORT_AGE_MINUTES) {
        return Err(format!(
            "{}: latest report {} min old > {MAX_REPORT_AGE_MINUTES}",
            u.icao,
            age.num_minutes()
        ));
    }
    Ok(r)
}

/// L26 (`variant`: ≥ 3.5 °C warmer). Warm air upstream on its way:
/// 08:00–14:00, the METAR wind ≥ 8 kt, an airport within 35° of it and
/// 60–300 km away ≥ 2.5 °C warmer than Schiphol's latest METAR (and at
/// least the high's rounding edge), its air due before 16:00 → YES of the
/// next degree at 0.05–0.35.
pub(crate) fn warmth(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_delta = if variant { 35 } else { 25 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 14 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let here = day.latest_report().and_then(|r| r.temp_tenths);
    match (wind(day, 8), here) {
        (Err(why), _) => blockers.push(why),
        (_, None) => blockers.push("no temperature in the latest METAR".into()),
        (Ok((dir, kt)), Some(here)) => match upwind(day, dir, 35.0, (60.0, 300.0)) {
            Err(why) => blockers.push(why),
            Ok(found) => {
                let due_by = day.local(16 * 60);
                let mut why = Vec::new();
                let mut hit = false;
                for u in found {
                    let r = match latest(day, u) {
                        Ok(r) => r,
                        Err(e) => {
                            why.push(e);
                            continue;
                        }
                    };
                    let Some(there) = r.temp_tenths else {
                        why.push(format!("{}: no temperature", u.icao));
                        continue;
                    };
                    let eta = arrival(r.observed_at, u.distance_km, kt);
                    let delta = there - here;
                    if delta >= min_delta && there >= day.edge() && due_by.is_some_and(|d| eta <= d)
                    {
                        hit = true;
                        rationale.push(format!(
                            "wind {dir:03}° {kt} kt: {} ({:.0}°, {:.0} km) {} vs Schiphol {}, its air due {} UTC",
                            u.icao,
                            u.bearing_deg,
                            u.distance_km,
                            c(there),
                            c(here),
                            eta.format("%H:%M")
                        ));
                    } else {
                        why.push(format!(
                            "{} {} vs Schiphol {} (needs ≥ {} warmer and ≥ the edge {}), due {} UTC (by 16:00 local)",
                            u.icao,
                            c(there),
                            c(here),
                            c(min_delta),
                            c(day.edge()),
                            eta.format("%H:%M")
                        ));
                    }
                }
                if !hit {
                    blockers.push(why.join("; "));
                }
            }
        },
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

/// L31 (`variant`: due within 2 hours). Clearing sky upwind: 09:00–14:00,
/// Schiphol under a ceiling ≤ 2500 ft within 2 °C of the high, an airport
/// within 40° of the wind (≥ 6 kt) and 60–200 km away clear (no ceiling
/// under 5000 ft) in its last two reports, its air due within 3 hours and
/// before 16:00 → YES of the next degree at 0.05–0.30.
pub(crate) fn clearing(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let within = Duration::hours(if variant { 2 } else { 3 });
    let mut blockers: Vec<String> = day.outside(9 * 60, 14 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match day.latest_report() {
        None => blockers.push("no METAR today".into()),
        Some(r) => {
            match r.wx.body.ceiling_ft() {
                Some(ft) if ft <= 2500 => {
                    rationale.push(format!("Schiphol under a ceiling of {ft} ft"));
                }
                Some(ft) => blockers.push(format!("Schiphol's ceiling {ft} ft > 2500")),
                None => blockers.push("no ceiling at Schiphol: nothing to clear".into()),
            }
            match r.temp_tenths {
                Some(t) if t >= (day.high - 2) * 10 => {}
                Some(t) => blockers.push(format!(
                    "METAR {} more than 2 °C under the high {}",
                    c(t),
                    c(day.high * 10)
                )),
                None => blockers.push("no temperature in the latest METAR".into()),
            }
        }
    }
    match wind(day, 6) {
        Err(why) => blockers.push(why),
        Ok((dir, kt)) => match upwind(day, dir, 40.0, (60.0, 200.0)) {
            Err(why) => blockers.push(why),
            Ok(found) => {
                let due_by = day.local(16 * 60);
                let mut why = Vec::new();
                let mut hit = false;
                for u in found {
                    let reports = known(day, u);
                    let r = match latest(day, u) {
                        Ok(r) => r,
                        Err(e) => {
                            why.push(e);
                            continue;
                        }
                    };
                    let both_clear = reports.len() >= 2
                        && reports[reports.len() - 2..]
                            .iter()
                            .all(|x| x.wx.clear(5000));
                    let eta = arrival(r.observed_at, u.distance_km, kt);
                    let in_time = eta <= day.now() + within && due_by.is_some_and(|d| eta <= d);
                    if both_clear && in_time {
                        hit = true;
                        rationale.push(format!(
                            "wind {dir:03}° {kt} kt: {} ({:.0} km) clear in its last two reports, its air due {} UTC",
                            u.icao,
                            u.distance_km,
                            eta.format("%H:%M")
                        ));
                    } else {
                        why.push(format!(
                            "{}: {} its air due {} UTC (within {} h and by 16:00 local)",
                            u.icao,
                            if both_clear {
                                "clear,"
                            } else {
                                "not clear in its last two reports,"
                            },
                            eta.format("%H:%M"),
                            within.num_hours()
                        ));
                    }
                }
                if !hit {
                    blockers.push(why.join("; "));
                }
            }
        },
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

/// A front's passage in an airport's reports of the last 3 hours: two
/// reports ≤ 70 minutes apart, the temperature down ≥ `drop` tenths, the
/// wind veered 40–180° or the pressure up ≥ 1 hPa. `(passage, detail)`.
fn passage(
    reports: &[&WxReport],
    now: DateTime<Utc>,
    drop: i32,
) -> Option<(DateTime<Utc>, String)> {
    reports
        .windows(2)
        .rev()
        .filter(|w| w[1].observed_at >= now - Duration::hours(3))
        .find_map(|w| {
            let (a, b) = (w[0], w[1]);
            if b.observed_at - a.observed_at > Duration::minutes(70) {
                return None;
            }
            let fell = a.temp_tenths? - b.temp_tenths?;
            if fell < drop {
                return None;
            }
            let veer = match (
                a.wx.wind().and_then(|x| x.direction),
                b.wx.wind().and_then(|x| x.direction),
            ) {
                (Some(d1), Some(d2)) => Some((i32::from(d2) - i32::from(d1)).rem_euclid(360)),
                _ => None,
            };
            let rise = match (a.wx.qnh_hpa, b.wx.qnh_hpa) {
                (Some(p1), Some(p2)) => Some(i32::from(p2) - i32::from(p1)),
                _ => None,
            };
            let veered = veer.is_some_and(|v| (40..=180).contains(&v));
            let pressure = rise.is_some_and(|r| r >= 1);
            (veered || pressure).then(|| {
                (
                    b.observed_at,
                    format!(
                        "{} down {} by {} UTC{}{}",
                        c(a.temp_tenths.unwrap_or(0)),
                        c(fell),
                        b.observed_at.format("%H:%M"),
                        veer.filter(|_| veered)
                            .map(|v| format!(", wind veered {v}°"))
                            .unwrap_or_default(),
                        rise.filter(|_| pressure)
                            .map(|r| format!(", QNH +{r} hPa"))
                            .unwrap_or_default()
                    ),
                )
            })
        })
}

/// L36 (`variant`: a drop of ≥ 3 °C). A front upstream, due before the
/// peak: 08:00–14:00, an airport within 45° of the wind (≥ 6 kt) and
/// 60–300 km away saw a front pass in the last 3 hours (≥ 2 °C down within
/// 70 minutes, the wind veering 40–180° or QNH up ≥ 1 hPa), due at Schiphol
/// from now until the season's median peak time → NO of the next degree at
/// 0.60–0.95 (p = 0.92).
pub(crate) fn front_eta(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let drop = if variant { 30 } else { 20 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 14 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match wind(day, 6) {
        Err(why) => blockers.push(why),
        Ok((dir, kt)) => match upwind(day, dir, 45.0, (60.0, 300.0)) {
            Err(why) => blockers.push(why),
            Ok(found) => {
                let peak = day.local(day.season_median());
                let mut why = Vec::new();
                let mut hit = false;
                for u in found {
                    let reports = known(day, u);
                    match passage(&reports, day.now(), drop) {
                        None => why.push(format!(
                            "{}: no front in its reports of the last 3 hours",
                            u.icao
                        )),
                        Some((at, detail)) => {
                            let eta = arrival(at, u.distance_km, kt);
                            if eta >= day.now() && peak.is_some_and(|p| eta < p) {
                                hit = true;
                                rationale.push(format!(
                                    "front at {} ({:.0} km): {detail}; due at Schiphol {} UTC, before the median peak {}",
                                    u.icao,
                                    u.distance_km,
                                    eta.format("%H:%M"),
                                    super::day::hm(day.season_median())
                                ));
                            } else {
                                why.push(format!(
                                    "{}: front due {} UTC, not between now and the median peak {}",
                                    u.icao,
                                    eta.format("%H:%M"),
                                    super::day::hm(day.season_median())
                                ));
                            }
                        }
                    }
                }
                if !hit {
                    blockers.push(why.join("; "));
                }
            }
        },
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::No,
            band: (0.60, 0.95),
            p_win: Some((0.92, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}
