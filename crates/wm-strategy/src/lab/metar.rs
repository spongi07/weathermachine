//! L8–L13 and L23: the METAR's own weather groups (wind, weather, cloud,
//! QNH, the TREND).

use super::day::{Buy, Day, c, hm};
use crate::strategy::StrategyOutput;
use chrono::Duration;
use wm_core::market::OutcomeSide;
use wm_core::metar_wx::Wind;

fn wind_text(w: &Wind) -> String {
    match w.direction {
        Some(d) => format!("{d:03}°/{} kt", w.speed_kt),
        None => format!("variable/{} kt", w.speed_kt),
    }
}

/// The rules that take one trade a day.
fn once_a_day(day: &Day<'_, '_>, blockers: &mut Vec<String>) {
    if day.traded_today() {
        blockers.push("already traded today (one trade a day)".into());
    }
}

/// L8 (`yes_high`: YES of the high's bucket). Schiphol lies ~15 km inland:
/// the sea breeze arrives in the early afternoon with a small drop and a
/// humidity rise, and the day's high is then usually in. 11:00–17:30 on a
/// day ≥ 20 °C: the wind now 250–020° at ≥ 6 kt after an offshore report
/// (060–220°) since 07:00, the dew point ≥ 1 °C above the high's report,
/// ≥ 1 °C under the high → NO of the next degree (p = 0.95).
pub(crate) fn sea_breeze(day: &Day<'_, '_>, yes_high: bool, out: &mut StrategyOutput) {
    let target = if yes_high {
        day.outcome_of(day.high)
    } else {
        day.next_outcome()
    };
    let Some(target) = target else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(11 * 60, 17 * 60 + 30).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if day.high < 20 {
        blockers.push(format!("high {} °C < 20 °C", day.high));
    }
    if day.f.drop_tenths < 10 {
        blockers.push(format!(
            "METAR {} under the high < 1.0 °C",
            c(day.f.drop_tenths)
        ));
    }
    match day.latest_report() {
        None => blockers.push("no METAR weather groups today".into()),
        Some(now) => match now.wx.wind() {
            None => blockers.push("no wind in the latest METAR".into()),
            Some(w) if !(w.from_arc(250, 20) && w.speed_kt >= 6) => blockers.push(format!(
                "wind {} not onshore (250–020°, ≥ 6 kt)",
                wind_text(&w)
            )),
            Some(w) => {
                let earlier = &day.reports()[..day.reports().len() - 1];
                let offshore = earlier.iter().any(|r| {
                    day.minute_of(r.observed_at) >= 7 * 60
                        && r.wx
                            .wind()
                            .is_some_and(|x| x.from_arc(60, 220) && x.speed_kt >= 3)
                });
                if !offshore {
                    blockers.push("no offshore wind (060–220°) since 07:00".into());
                }
                match (
                    now.dew_tenths,
                    day.report_observed(day.f.high_at)
                        .and_then(|r| r.dew_tenths),
                ) {
                    (Some(d_now), Some(d_high)) if d_now < d_high + 10 => blockers.push(format!(
                        "dew point {} not ≥ 1 °C above the high's report ({})",
                        c(d_now),
                        c(d_high)
                    )),
                    (Some(d_now), Some(d_high)) => rationale.push(format!(
                        "sea breeze: wind {} after an offshore morning, dew point {} → {}",
                        wind_text(&w),
                        c(d_high),
                        c(d_now)
                    )),
                    _ => blockers.push("no dew point now or at the high's report".into()),
                }
            }
        },
    }
    let (side, band, p) = if yes_high {
        (OutcomeSide::Yes, (0.50, 0.92), (0.94, 0.01))
    } else {
        (OutcomeSide::No, (0.60, 0.96), (0.95, 0.01))
    };
    day.taker(
        Buy {
            outcome: target,
            side,
            band,
            p_win: Some(p),
            rationale,
        },
        blockers,
        out,
    );
}

/// L9 (`thunder_only`: thunder or a cumulonimbus). Rain-cooled outflow
/// drops the station 3–8 °C: 12:00–19:00, rain, showers or thunder now or
/// since the last report, ≥ 2 °C under the high → NO of the next degree
/// (p = 0.95).
pub(crate) fn rain_cap(day: &Day<'_, '_>, thunder_only: bool, out: &mut StrategyOutput) {
    let Some(i) = day.next_outcome() else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(12 * 60, 19 * 60).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if day.f.drop_tenths < 20 {
        blockers.push(format!(
            "METAR {} under the high < 2.0 °C",
            c(day.f.drop_tenths)
        ));
    }
    match day.latest_report() {
        None => blockers.push("no METAR weather groups today".into()),
        Some(now) => {
            let wet = if thunder_only {
                now.wx.thunder_now_or_recent()
            } else {
                now.wx.precipitation_now_or_recent() || now.wx.thunder_now_or_recent()
            };
            if wet {
                rationale.push(format!(
                    "{} in the {} UTC METAR, {} under the high",
                    if thunder_only {
                        "thunder"
                    } else {
                        "rain or thunder"
                    },
                    now.observed_at.format("%H:%M"),
                    c(day.f.drop_tenths)
                ));
            } else {
                blockers.push(if thunder_only {
                    "no thunder or cumulonimbus now or since the last report".into()
                } else {
                    "no rain, showers or thunder now or since the last report".into()
                });
            }
        }
    }
    day.taker(
        Buy {
            outcome: i,
            side: OutcomeSide::No,
            band: (0.60, 0.96),
            p_win: Some((0.95, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L10 (`nosig_lock`: the NOSIG lock). Schiphol's METARs carry KNMI's
/// two-hour TREND: 11:00–17:00, a BECMG/TEMPO with showers or thunder, a
/// wind from 240–020° or a ceiling under 3000 ft → NO of the next degree
/// (p = 0.92). The variant: after the median peak time, NOSIG, no ceiling
/// under 5000 ft, ≥ 1 °C under the high → YES of the high's bucket
/// (p = 0.93).
pub(crate) fn trend_cap(day: &Day<'_, '_>, nosig_lock: bool, out: &mut StrategyOutput) {
    let target = if nosig_lock {
        day.outcome_of(day.high)
    } else {
        day.next_outcome()
    };
    let Some(target) = target else {
        return;
    };
    let mut blockers = Vec::new();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    let latest = day.latest_report();
    if latest.is_none() {
        blockers.push("no METAR weather groups today".into());
    }
    if nosig_lock {
        blockers.extend(day.outside(11 * 60, 19 * 60));
        let median = day.season_median();
        if day.minute() < median {
            blockers.push(format!(
                "before the season's median peak time {}",
                hm(median)
            ));
        }
        if day.f.drop_tenths < 10 {
            blockers.push(format!(
                "METAR {} under the high < 1.0 °C",
                c(day.f.drop_tenths)
            ));
        }
        if let Some(now) = latest {
            if !now.wx.nosig() {
                blockers.push("the TREND is not NOSIG".into());
            }
            if !now.wx.clear(5000) {
                blockers.push("a ceiling under 5000 ft (or fog, or precipitation)".into());
            }
            if now.wx.nosig() && now.wx.clear(5000) {
                rationale
                    .push("NOSIG under a clear sky after the usual peak: the high is in".into());
            }
        }
    } else {
        blockers.extend(day.outside(11 * 60, 17 * 60));
        if let Some(now) = latest {
            let mut why = Vec::new();
            if now.wx.trend_precipitation() {
                why.push("showers or thunder");
            }
            if now.wx.trend_wind_from(240, 20) {
                why.push("an onshore wind");
            }
            if now.wx.trend_ceiling_below(3000) {
                why.push("a ceiling under 3000 ft");
            }
            if why.is_empty() {
                blockers.push("the TREND announces no showers, onshore wind or low cloud".into());
            } else {
                rationale.push(format!("the TREND announces {}", why.join(", ")));
            }
        }
    }
    let (side, band, p) = if nosig_lock {
        (OutcomeSide::Yes, (0.55, 0.92), (0.93, 0.005))
    } else {
        (OutcomeSide::No, (0.55, 0.94), (0.92, 0.005))
    };
    day.taker(
        Buy {
            outcome: target,
            side,
            band,
            p_win: Some(p),
            rationale,
        },
        blockers,
        out,
    );
}

/// L11 (`variant`: a gap of 4 °C). A stratus deck that does not mix out
/// busts the maximum forecast the market is anchored on: 08:30–11:30, fog
/// or mist under 5 km or a ceiling ≤ 800 ft, the favourite (≥ 0.30) ≥ 6 °C
/// above the temperature → NO of the favourite at 0.30–0.70.
pub(crate) fn fog_fade(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let gap = if variant { 40 } else { 60 };
    let Some((fav, p)) = day.favourite() else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(8 * 60 + 30, 11 * 60 + 30).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if p < 0.30 {
        blockers.push(format!("favourite {} at {p:.2} < 0.30", fav.label));
    }
    match fav.bucket.lower {
        None => blockers.push("the favourite has no lower bound".into()),
        Some(lower) if lower * 10 - day.f.current_tenths < gap => blockers.push(format!(
            "favourite {} only {} above the temperature {} (< {})",
            fav.label,
            c(lower * 10 - day.f.current_tenths),
            c(day.f.current_tenths),
            c(gap)
        )),
        Some(_) => {}
    }
    match day.latest_report() {
        None => blockers.push("no METAR weather groups today".into()),
        Some(now) => {
            let b = &now.wx.body;
            let grey = (b.fog_or_mist() && b.visibility_m.is_some_and(|v| v < 5000))
                || b.ceiling_ft().is_some_and(|c| c <= 800);
            if grey {
                rationale.push(format!(
                    "fog or low stratus (visibility {}, ceiling {}) under a favourite {} at {p:.2}",
                    b.visibility_m
                        .map_or_else(|| "—".into(), |v| format!("{v} m")),
                    b.ceiling_ft()
                        .map_or_else(|| "none".into(), |c| format!("{c} ft")),
                    fav.label
                ));
            } else {
                blockers.push("no fog or mist under 5 km and no ceiling ≤ 800 ft".into());
            }
        }
    }
    day.taker(
        Buy {
            outcome: fav,
            side: OutcomeSide::No,
            band: (0.30, 0.70),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L12 (`any_wind`). Dry continental air under a clear sky heats past the
/// forecast: 10:00–13:00, no ceiling under 5000 ft, the dew point ≥ 8 °C
/// under the temperature, the wind 045–225° or calm → YES of the bucket
/// above the favourite (≥ 0.25) at 0.06–0.30.
pub(crate) fn clear_sky(day: &Day<'_, '_>, any_wind: bool, out: &mut StrategyOutput) {
    let Some((fav, p)) = day.favourite() else {
        return;
    };
    let Some(target) = fav.bucket.upper.and_then(|u| day.outcome_of(u + 1)) else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(10 * 60, 13 * 60).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if p < 0.25 {
        blockers.push(format!("favourite {} at {p:.2} < 0.25", fav.label));
    }
    match day.latest_report() {
        None => blockers.push("no METAR weather groups today".into()),
        Some(now) => {
            if !now.wx.clear(5000) {
                blockers.push("a ceiling under 5000 ft (or fog, or precipitation)".into());
            }
            match now.dew_tenths {
                None => blockers.push("no dew point".into()),
                Some(d) if day.f.current_tenths - d < 80 => blockers.push(format!(
                    "dew point {} only {} under the temperature (< 8 °C)",
                    c(d),
                    c(day.f.current_tenths - d)
                )),
                Some(_) => {}
            }
            match now.wx.wind() {
                None => blockers.push("no wind in the latest METAR".into()),
                Some(w) if !(any_wind || w.direction.is_none() || w.from_arc(45, 225)) => {
                    blockers.push(format!("wind {} not continental (045–225°)", wind_text(&w)));
                }
                Some(w) => rationale.push(format!(
                    "clear and dry, wind {}: the bucket above the favourite {}",
                    wind_text(&w),
                    fav.label
                )),
            }
        }
    }
    day.taker(
        Buy {
            outcome: target,
            side: OutcomeSide::Yes,
            band: (0.06, 0.30),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L13 (`no_above`: NO of the next degree). A front through in the morning
/// sets the high early: 09:00–15:00, the high first reached before 11:00,
/// the wind veered from 120–229° at the high's report to 230–340° (≥ 8 kt),
/// QNH up, ≥ 1.5 °C under the high → YES of the high's bucket (p = 0.90).
pub(crate) fn front_lock(day: &Day<'_, '_>, no_above: bool, out: &mut StrategyOutput) {
    let target = if no_above {
        day.next_outcome()
    } else {
        day.outcome_of(day.high)
    };
    let Some(target) = target else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(9 * 60, 15 * 60).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if day.f.drop_tenths < 15 {
        blockers.push(format!(
            "METAR {} under the high < 1.5 °C",
            c(day.f.drop_tenths)
        ));
    }
    match (day.latest_report(), day.report_observed(day.f.high_at)) {
        (None, _) => blockers.push("no METAR weather groups today".into()),
        (_, None) => blockers.push("the high's report has no weather groups".into()),
        (Some(now), Some(then)) => {
            let first = now.observed_at - Duration::minutes(day.f.minutes_since_first_high);
            if day.minute_of(first) >= 11 * 60 {
                blockers.push(format!(
                    "the high was first reached at {}, not before 11:00",
                    hm(day.minute_of(first))
                ));
            }
            let veered_now = now
                .wx
                .wind()
                .is_some_and(|w| w.from_arc(230, 340) && w.speed_kt >= 8);
            let before = then.wx.wind().is_some_and(|w| w.from_arc(120, 229));
            if !veered_now {
                blockers.push(format!(
                    "wind {} not 230–340° at ≥ 8 kt",
                    now.wx.wind().map_or_else(|| "—".into(), |w| wind_text(&w))
                ));
            }
            if !before {
                blockers.push(format!(
                    "wind at the high's report {} not 120–229°",
                    then.wx.wind().map_or_else(|| "—".into(), |w| wind_text(&w))
                ));
            }
            match (now.wx.qnh_hpa, then.wx.qnh_hpa) {
                (Some(a), Some(b)) if a > b => rationale.push(format!(
                    "a front through: wind veered to {}, QNH {b} → {a} hPa",
                    now.wx.wind().map_or_else(|| "—".into(), |w| wind_text(&w))
                )),
                (Some(a), Some(b)) => blockers.push(format!("QNH not up ({b} → {a} hPa)")),
                _ => blockers.push("no QNH now or at the high's report".into()),
            }
        }
    }
    let (side, band, p) = if no_above {
        (OutcomeSide::No, (0.60, 0.95), (0.95, 0.005))
    } else {
        (OutcomeSide::Yes, (0.40, 0.88), (0.90, 0.01))
    };
    day.taker(
        Buy {
            outcome: target,
            side,
            band,
            p_win: Some(p),
            rationale,
        },
        blockers,
        out,
    );
}

/// L23 (`metar_slope`: the METAR's slope instead of KNMI's). After a shower
/// the sky clears and the sun heats again: 12:00–16:30, rain in the last
/// 3 hours, now dry with no ceiling under 4000 ft, within 1.5 °C of the
/// high, KNMI's mean up ≥ 0.5 °C in 30 minutes → YES of the next degree at
/// 0.05–0.35.
pub(crate) fn shower_recovery(day: &Day<'_, '_>, metar_slope: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let mut blockers: Vec<String> = day.outside(12 * 60, 16 * 60 + 30).into_iter().collect();
    once_a_day(day, &mut blockers);
    let mut rationale = Vec::new();
    if day.f.current_tenths < day.high * 10 - 15 {
        blockers.push(format!(
            "{} more than 1.5 °C under the high",
            c(day.f.current_tenths)
        ));
    }
    match day.latest_report() {
        None => blockers.push("no METAR weather groups today".into()),
        Some(now) => {
            if now.wx.body.precipitation() {
                blockers.push("still raining".into());
            }
            if !now.wx.clear(4000) {
                blockers.push("a ceiling under 4000 ft (or fog)".into());
            }
            let reports = day.reports();
            let rained = reports[..reports.len() - 1].iter().any(|r| {
                r.observed_at >= now.observed_at - Duration::hours(3) && r.wx.body.precipitation()
            });
            if !rained {
                blockers.push("no rain in the last 3 hours".into());
            }
        }
    }
    if metar_slope {
        match day.f.slope_c_per_hour {
            Some(s) if s >= 1.0 => rationale.push(format!("METAR warming {s:.1} °C/h")),
            Some(s) => blockers.push(format!("METAR slope {s:.1} °C/h < 1.0")),
            None => blockers.push("no METAR slope".into()),
        }
    } else {
        match day.latest_reading() {
            Err(why) => blockers.push(why),
            Ok(r) => match (
                r.mean.map(|m| m.tenths()),
                day.mean_at(r.interval_end - Duration::minutes(30)),
            ) {
                (Some(a), Some(b)) if a - b >= 5 => rationale.push(format!(
                    "the sun is back: KNMI {} → {} in 30 min",
                    c(b),
                    c(a)
                )),
                (Some(a), Some(b)) => {
                    blockers.push(format!("KNMI up only {} in 30 min (< 0.5 °C)", c(a - b)))
                }
                _ => blockers.push("no KNMI means 30 min apart".into()),
            },
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
