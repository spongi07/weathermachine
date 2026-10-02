//! L24 and L25: weather that changes during the day — the air upwind and
//! the sun.

use super::day::{Buy, Day, c, mean};
use super::solar::{angle_between, clear_sky_ghi};
use crate::strategy::StrategyOutput;
use chrono::{DateTime, Duration, Utc};
use wm_core::market::OutcomeSide;

/// L24 (`cool`: the cool side). The air 30–40 km upwind is Schiphol's next
/// hour: 10:00–18:00, the METAR wind ≥ 6 kt from within 40° of a KNMI
/// neighbour's bearing, the neighbour ≥ 0.8 °C warmer than Schiphol in the
/// same ten minutes and Schiphol within 0.6 °C under the edge → NO of the
/// high's bucket at ≤ 0.75 (p = 0.80). The cool side: from 12:00 the
/// neighbour ≥ 1.0 °C cooler and Schiphol ≥ 0.5 °C under the edge → NO of
/// the next degree at 0.65–0.96 (p = 0.93).
pub(crate) fn upwind(day: &Day<'_, '_>, cool: bool, out: &mut StrategyOutput) {
    let target = if cool {
        day.next_outcome()
    } else {
        day.high_outcome()
    };
    let Some(target) = target else {
        return;
    };
    let edge = day.edge();
    let mut blockers: Vec<String> = day
        .outside(if cool { 12 * 60 } else { 10 * 60 }, 18 * 60)
        .into_iter()
        .collect();
    let mut rationale = Vec::new();
    let wind = day.latest_report().and_then(|r| r.wx.wind());
    match wind {
        None => blockers.push("no wind in the latest METAR".into()),
        Some(w) => match w.direction {
            None => blockers.push("variable or calm wind: no upwind station".into()),
            Some(_) if w.speed_kt < 6 => blockers.push(format!("wind {} kt < 6", w.speed_kt)),
            Some(dir) => {
                let upwind: Vec<_> = day
                    .lab
                    .neighbours
                    .iter()
                    .filter(|n| angle_between(f64::from(dir), n.bearing_deg) <= 40.0)
                    .collect();
                if day.lab.neighbours.is_empty() {
                    blockers.push("no neighbouring KNMI station configured or read".into());
                } else if upwind.is_empty() {
                    blockers.push(format!(
                        "wind from {dir:03}°: no neighbour within 40° ({})",
                        day.lab
                            .neighbours
                            .iter()
                            .map(|n| format!("{} {:.0}°", n.name, n.bearing_deg))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                } else {
                    let mut found = false;
                    let mut why = Vec::new();
                    for n in upwind {
                        let Some(r) = n
                            .readings
                            .iter()
                            .rev()
                            .find(|r| r.interval_end <= day.now())
                        else {
                            why.push(format!("{}: no reading", n.name));
                            continue;
                        };
                        if day.now() - r.interval_end
                            > Duration::minutes(day.cfg.max_reading_age_minutes)
                        {
                            why.push(format!("{}: reading too old", n.name));
                            continue;
                        }
                        let (Some(there), Some(here)) =
                            (r.mean.map(|m| m.tenths()), day.mean_at(r.interval_end))
                        else {
                            why.push(format!(
                                "{}: no Schiphol reading for {} UTC",
                                n.name,
                                r.interval_end.format("%H:%M")
                            ));
                            continue;
                        };
                        let delta = there - here;
                        let ok = if cool {
                            delta <= -10 && here <= edge - 5
                        } else {
                            delta >= 8 && here >= edge - 6 && r.interval_end > day.last_metar_at()
                        };
                        if ok {
                            found = true;
                            rationale.push(format!(
                                "wind {dir:03}° from {} ({:.0}°): {} there vs {} at Schiphol at {} UTC",
                                n.name,
                                n.bearing_deg,
                                c(there),
                                c(here),
                                r.interval_end.format("%H:%M")
                            ));
                        } else {
                            why.push(format!(
                                "{} {} vs Schiphol {} (needs {}, Schiphol {} the edge {})",
                                n.name,
                                c(there),
                                c(here),
                                if cool {
                                    "≥ 1.0 °C cooler"
                                } else {
                                    "≥ 0.8 °C warmer"
                                },
                                if cool {
                                    "≥ 0.5 °C under"
                                } else {
                                    "within 0.6 °C under"
                                },
                                c(edge)
                            ));
                        }
                    }
                    if !found {
                        blockers.push(why.join("; "));
                    }
                }
            }
        },
    }
    let (band, p) = if cool {
        ((0.65, 0.96), (0.93, 0.005))
    } else {
        ((0.05, 0.75), (0.80, 0.02))
    };
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::No,
            band,
            p_win: Some(p),
            rationale,
        },
        blockers,
        out,
    );
}

/// The clear-sky index of each reading with radiation, while the sun is
/// up enough (clear sky ≥ 150 W/m²): `(interval end, index)`.
pub fn clear_sky_index(
    readings: &[wm_core::weather::TenMinuteObservation],
    latitude: f64,
    longitude: f64,
) -> Vec<(DateTime<Utc>, f64)> {
    readings
        .iter()
        .filter_map(|r| {
            let v = f64::from(r.radiation?);
            let cs = clear_sky_ghi(r.interval_end - Duration::minutes(5), latitude, longitude);
            (cs >= 150.0).then_some((r.interval_end, v / cs))
        })
        .collect()
}

/// L25 (`clearing`: ≥ 80 % after ≤ 40 % → YES of the next degree).
/// Radiation leads the temperature: 10:30–15:30, KNMI's global radiation
/// over the last 30 minutes ≤ 35 % of the clear sky (Haurwitz) after ≥ 70 %
/// over the hour before, KNMI ≥ 0.3 °C under the edge → NO of the next
/// degree at 0.60–0.95 (p = 0.93).
pub(crate) fn radiation(day: &Day<'_, '_>, clearing: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let edge = day.edge();
    let mut blockers: Vec<String> = day
        .outside(10 * 60 + 30, 15 * 60 + 30)
        .into_iter()
        .collect();
    let mut rationale = Vec::new();
    match (day.lab.position, day.latest_reading()) {
        (None, _) => blockers.push("the station's position is unknown (clear sky)".into()),
        (_, Err(why)) => blockers.push(why),
        (Some((lat, lon)), Ok(r)) => {
            let end = r.interval_end;
            let index = clear_sky_index(day.readings(), lat, lon);
            let span = |a: i64, b: i64| -> Vec<f64> {
                index
                    .iter()
                    .filter(|(t, _)| {
                        *t > end - Duration::minutes(a) && *t <= end - Duration::minutes(b)
                    })
                    .map(|(_, k)| *k)
                    .collect()
            };
            let (last, before) = (span(30, 0), span(90, 30));
            if index.is_empty() {
                blockers.push("no KNMI radiation readings (qg)".into());
            } else if last.len() < 3 || before.len() < 5 {
                blockers.push(format!(
                    "{} + {} radiation readings in the last 30 + 60 min < 3 + 5",
                    last.len(),
                    before.len()
                ));
            } else {
                let (now, then) = (mean(&last), mean(&before));
                let ta = r.mean.map(|m| m.tenths());
                if clearing {
                    if now < 0.80 || then > 0.40 {
                        blockers.push(format!(
                            "clear-sky index {:.0} % after {:.0} % (needs ≥ 80 % after ≤ 40 %)",
                            100.0 * now,
                            100.0 * then
                        ));
                    }
                    match ta {
                        None => blockers.push("KNMI mean missing".into()),
                        Some(t) if t < edge - 8 => blockers.push(format!(
                            "KNMI mean {} more than 0.8 °C under the edge {}",
                            c(t),
                            c(edge)
                        )),
                        Some(_) => {}
                    }
                } else {
                    if now > 0.35 || then < 0.70 {
                        blockers.push(format!(
                            "clear-sky index {:.0} % after {:.0} % (needs ≤ 35 % after ≥ 70 %)",
                            100.0 * now,
                            100.0 * then
                        ));
                    }
                    if let Some(t) = ta.filter(|t| *t > edge - 3) {
                        blockers.push(format!(
                            "KNMI mean {} not ≥ 0.3 °C under the edge {}",
                            c(t),
                            c(edge)
                        ));
                    }
                }
                rationale.push(format!(
                    "radiation {:.0} % of the clear sky over 30 min after {:.0} % the hour before",
                    100.0 * now,
                    100.0 * then
                ));
            }
        }
    }
    let (side, band, p) = if clearing {
        (OutcomeSide::Yes, (0.05, 0.35), None)
    } else {
        (OutcomeSide::No, (0.60, 0.95), Some((0.93, 0.005)))
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
