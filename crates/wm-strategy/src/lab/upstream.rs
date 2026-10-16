//! L26, L31 and L36: the air on its way. METARs of airports 60–300 km
//! upstream show the air that reaches Schiphol hours later — warmer,
//! clearer, or behind a front. The surface wind carries the air at about
//! one and a half times its own speed (the wind above the ground is
//! faster); a front moves with the air behind it, at the pace its passages
//! upstream show. Each rule knows when it arrives and trades only what
//! arrives in time.

use super::UpstreamReadings;
use super::WxReport;
use super::day::{Buy, Day, RISE_BAND, c};
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
/// least the high's rounding edge), its air due before 16:00 → NO of the
/// high's bucket (a new high, however far) at 0.10–0.70.
pub(crate) fn warmth(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
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

/// L31 (`variant`: due within 2 hours). Clearing sky upwind: 09:00–14:00,
/// Schiphol under a ceiling ≤ 2500 ft within 2 °C of the high, an airport
/// within 40° of the wind (≥ 6 kt) and 60–200 km away clear (no ceiling
/// under 5000 ft) in its last two reports, its air due within 3 hours and
/// before 16:00 → NO of the high's bucket (a new high, however far) at
/// 0.10–0.70.
pub(crate) fn clearing(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
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

/// How far back L36 looks for a front's passage at an airport.
const FRONT_LOOKBACK: Duration = Duration::hours(6);
/// Two passages at least this far apart along a front's path time it: a
/// passage is known only to the half hour between two reports.
const FRONT_MIN_SPREAD_KM: f64 = 80.0;
/// The speeds a front plausibly moves at, km/h.
const FRONT_KMH: (f64, f64) = (10.0, 80.0);
/// Three passages or more further than this from a steady front (RMS,
/// minutes) are not one front.
const FRONT_MAX_MISS_MIN: f64 = 45.0;

/// A front's passage at an airport.
struct Passage {
    /// Halfway between the reports before and after it.
    at: DateTime<Utc>,
    /// `18.0 °C down 3.0 °C by 08:20 UTC, wind veered 90°`.
    detail: String,
}

/// The latest front's passage in an airport's reports of the last
/// [`FRONT_LOOKBACK`]: a report `drop` tenths or more under one at most 70
/// minutes before it (the largest such drop), the wind veered 40–180° or
/// the pressure up ≥ 1 hPa between the two.
fn passage(reports: &[&WxReport], now: DateTime<Utc>, drop: i32) -> Option<Passage> {
    let since = now - FRONT_LOOKBACK;
    reports
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, b)| b.observed_at >= since)
        .find_map(|(j, b)| {
            let after = b.temp_tenths?;
            reports[..j]
                .iter()
                .rev()
                .take_while(|a| b.observed_at - a.observed_at <= Duration::minutes(70))
                .filter_map(|a| {
                    let fell = a.temp_tenths? - after;
                    if fell < drop {
                        return None;
                    }
                    let veer = match (
                        a.wx.wind().and_then(|x| x.direction),
                        b.wx.wind().and_then(|x| x.direction),
                    ) {
                        (Some(d1), Some(d2)) => {
                            Some((i32::from(d2) - i32::from(d1)).rem_euclid(360))
                        }
                        _ => None,
                    };
                    let rise = match (a.wx.qnh_hpa, b.wx.qnh_hpa) {
                        (Some(p1), Some(p2)) => Some(i32::from(p2) - i32::from(p1)),
                        _ => None,
                    };
                    let veered = veer.filter(|v| (40..=180).contains(v));
                    let pressure = rise.filter(|r| *r >= 1);
                    (veered.is_some() || pressure.is_some()).then_some((*a, fell, veered, pressure))
                })
                .max_by_key(|(_, fell, _, _)| *fell)
                .map(|(a, fell, veered, pressure)| Passage {
                    at: a.observed_at + (b.observed_at - a.observed_at) / 2,
                    detail: format!(
                        "{} down {} by {} UTC{}{}",
                        c(a.temp_tenths.unwrap_or(0)),
                        c(fell),
                        b.observed_at.format("%H:%M"),
                        veered
                            .map(|v| format!(", wind veered {v}°"))
                            .unwrap_or_default(),
                        pressure
                            .map(|r| format!(", QNH +{r} hPa"))
                            .unwrap_or_default()
                    ),
                })
        })
}

/// A front on its way to Schiphol.
struct Front {
    /// When it reaches Schiphol.
    eta: DateTime<Utc>,
    /// Where it passed, its path, its speed and when it is due.
    line: String,
}

/// The front on its way: its passages in the last [`FRONT_LOOKBACK`] at
/// the airports south through west to north of Schiphol (where fronts
/// come from) 60–300 km away; its path from the wind behind it (a front
/// moves with the air behind it, not with the wind ahead of it at
/// Schiphol); its speed from the order of its passages along that path,
/// two at least [`FRONT_MIN_SPREAD_KM`] apart; and when it reaches
/// Schiphol on the same line. Or why there is none.
fn front(day: &Day<'_, '_>, drop: i32) -> Result<Front, String> {
    let all = day.upstream();
    if all.is_empty() {
        return Err("no upstream airport configured or read ([[lab.upstream]])".into());
    }
    let west: Vec<&UpstreamReadings<'_>> = all
        .iter()
        .filter(|u| angle_between(u.bearing_deg, 270.0) <= 90.0)
        .filter(|u| (60.0..=300.0).contains(&u.distance_km))
        .collect();
    if west.is_empty() {
        return Err("no airport south through west to north of Schiphol at 60–300 km".into());
    }
    let now = day.now();
    let seen: Vec<(&UpstreamReadings<'_>, Passage, Option<Wind>)> = west
        .iter()
        .filter_map(|u| {
            let reports = known(day, u);
            let p = passage(&reports, now, drop)?;
            Some((*u, p, reports.last().and_then(|r| r.wx.wind())))
        })
        .collect();
    if seen.is_empty() {
        return Err(format!(
            "no front in the last {} hours at {}",
            FRONT_LOOKBACK.num_hours(),
            west.iter().map(|u| u.icao).collect::<Vec<_>>().join(", ")
        ));
    }
    let passed = seen
        .iter()
        .map(|(u, p, _)| format!("{} ({} UTC: {})", u.icao, p.at.format("%H:%M"), p.detail))
        .collect::<Vec<_>>()
        .join(", ");
    // Its path: the winds behind it, turned around and averaged.
    let (mut x, mut y, mut n) = (0.0_f64, 0.0_f64, 0_u32);
    for (_, _, w) in &seen {
        if let Some(Wind {
            direction: Some(d),
            speed_kt,
            ..
        }) = w
            && *speed_kt >= 3
        {
            let to = (f64::from(*d) + 180.0).to_radians();
            x += to.sin();
            y += to.cos();
            n += 1;
        }
    }
    if n == 0 {
        return Err(format!(
            "front through {passed}: no wind behind it to tell its path"
        ));
    }
    let len = x.hypot(y);
    if len < 0.7 * f64::from(n) {
        return Err(format!(
            "front through {passed}: the winds behind it disagree on its path"
        ));
    }
    let (mx, my) = (x / len, y / len);
    let toward = mx.atan2(my).to_degrees().rem_euclid(360.0);
    let behind = (toward + 180.0).rem_euclid(360.0);
    let head = format!(
        "front through {passed}; the air behind it from {behind:03.0}°, so on its way to {toward:03.0}°"
    );
    // Each passage: the distance still to go along the path to Schiphol
    // (km) and when it was (minutes from now).
    let points: Vec<(f64, f64)> = seen
        .iter()
        .map(|(u, p, _)| {
            let b = u.bearing_deg.to_radians();
            let to_go = -(mx * b.sin() + my * b.cos()) * u.distance_km;
            #[allow(clippy::cast_precision_loss)]
            let at = (p.at - now).num_seconds() as f64 / 60.0;
            (to_go, at)
        })
        .collect();
    if points.len() < 2 {
        return Err(format!(
            "{head}: its speed needs a second airport along its path"
        ));
    }
    let nearest = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let farthest = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    if farthest - nearest < FRONT_MIN_SPREAD_KM {
        return Err(format!(
            "{head}: its passages {:.0} km apart along that path, < {FRONT_MIN_SPREAD_KM:.0} to time it",
            farthest - nearest
        ));
    }
    // at = due − pace × to_go, by least squares (pace: minutes a km).
    #[allow(clippy::cast_precision_loss)]
    let count = points.len() as f64;
    let mean_go = points.iter().map(|p| p.0).sum::<f64>() / count;
    let mean_at = points.iter().map(|p| p.1).sum::<f64>() / count;
    let spread: f64 = points.iter().map(|p| (p.0 - mean_go).powi(2)).sum();
    let together: f64 = points
        .iter()
        .map(|p| (p.0 - mean_go) * (p.1 - mean_at))
        .sum();
    let pace = -together / spread;
    if pace <= 0.0 {
        return Err(format!("{head}: its passages out of order along that path"));
    }
    let kmh = 60.0 / pace;
    if !(FRONT_KMH.0..=FRONT_KMH.1).contains(&kmh) {
        return Err(format!(
            "{head}: its passages give {kmh:.0} km/h, outside {:.0}–{:.0}",
            FRONT_KMH.0, FRONT_KMH.1
        ));
    }
    let due = mean_at + pace * mean_go;
    if points.len() >= 3 {
        let miss = (points
            .iter()
            .map(|p| (p.1 - (due - pace * p.0)).powi(2))
            .sum::<f64>()
            / count)
            .sqrt();
        if miss > FRONT_MAX_MISS_MIN {
            return Err(format!(
                "{head}: its passages miss a steady front by {miss:.0} min"
            ));
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    let eta = now + Duration::seconds((due * 60.0).round() as i64);
    Ok(Front {
        eta,
        line: format!(
            "{head} at {kmh:.0} km/h: due at Schiphol {} UTC",
            eta.format("%H:%M")
        ),
    })
}

/// L36 (`variant`: a drop of ≥ 3 °C). A front on its way, due before the
/// peak: 08:00–14:00, airports south through west to north of Schiphol
/// (60–300 km) saw it pass in the last 6 hours (≥ 2 °C down within 70
/// minutes, the wind veering 40–180° or QNH up ≥ 1 hPa); the wind behind
/// it gives its path, two passages ≥ 80 km apart along that path its
/// speed, and it is due at Schiphol from now until the season's median
/// peak time → NO of the next degree at 0.60–0.95 (p = 0.92).
pub(crate) fn front_eta(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let drop = if variant { 30 } else { 20 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 14 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    match front(day, drop) {
        Err(why) => blockers.push(why),
        Ok(f) => {
            let median = super::day::hm(day.season_median());
            let peak = day.local(day.season_median());
            if f.eta >= day.now() && peak.is_some_and(|p| f.eta < p) {
                rationale.push(format!("{}, before the median peak {median} local", f.line));
            } else {
                blockers.push(format!(
                    "{}: not between now and the median peak {median} local",
                    f.line
                ));
            }
        }
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
