//! The strategy lab: 25 new strategies (L1–L25) replayed at traded prices
//! (`research market`). Each combines inputs no public weather bot or paper
//! we found combines; `docs/research/strategy-lab.md` gives the reasoning
//! and sources of each.
//!
//! The machinery is G–K's ([`crate::market_gk`]): decisions at every report
//! from local midnight at the knowledge time; **takers** pay the latest
//! trade of the kind they need when it is at most `fresh_quote_minutes`
//! old, else the first acceptable one while the signal stands, plus the
//! slippage allowance and the taker fee; **makers** rest one tick better
//! than the latest trade on their side and fill only when a later trade
//! goes through that price, pay no fee and earn the rebate share. Every
//! rule takes at most one trade per day and bucket (and side), at a stake
//! of `stake_usd` (L3 at F's shares).
//!
//! The inputs beyond G–K's: the METAR's weather groups (wind, weather,
//! clouds, QNH, TREND — [`wm_core::metar_wx`]), the day-1 hourly forecast
//! even when the model does not use it, KNMI's global radiation and the
//! ten-minute temperatures of neighbouring stations, every taker's track
//! record on the days before (wallets scored prequentially), and F's
//! replayed trades. A rule whose input is missing is reported as not
//! replayed, never as "no trade".
//!
//! Each family has a variant or a control (the same rule without the new
//! input), and the out-of-sample line chooses on the first half of the
//! market days and judges on the second. Twenty-five families and fifty-one
//! rules are many tries: on the first half one or two will look good by
//! chance, so only a later-days result whose interval excludes zero counts.
//! The lab trades nothing live; a strategy that holds up goes live under its
//! own letter, with its own tests, as G–K did.

use crate::forecast_eval::ForecastHistory;
use crate::market_eval::MarketTrade;
use crate::market_gk::{fresh_quote, taker_price, tick};
use crate::market_makers::{Resting, cancel_time, next_routine, through_fill};
use crate::market_peak::local_to_utc;
use crate::market_sim::{Decision, MarketSimConfig, SimTrade, StrategyRow, local_hm, row};
use chrono::{DateTime, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use wm_core::market::TemperatureBucket;
use wm_core::metar_wx::{MetarWx, parse_wx};
use wm_core::time::{local_day_bounds, local_minute_of_day};
use wm_core::weather::{Observation, TenMinuteObservation};
pub(crate) use wm_strategy::lab::WalletBook;
use wm_strategy::lab::solar::{angle_between, bearing, clear_sky_ghi};
use wm_strategy::lab::wallets::ScoredTrade;
use wm_strategy::{ForecastDay, PeakTimes};
use wm_weather::knmi::SeriesPoint;

/// The lab's trades carry this structure (no model structure).
pub(crate) const STRUCTURE: &str = "lab";

/// Fewest replayed market days for the out-of-sample split.
const MIN_SPLIT_DAYS: usize = 20;

/// A family holds up only on at least this many later-days trades, on at
/// least [`MIN_HELD_DAYS`] days: a handful of wins has an interval of no
/// width, not one above zero.
const MIN_HELD_TRADES: u64 = 5;
const MIN_HELD_DAYS: u64 = 3;

/// Families L1–L25.
pub const FAMILIES: u8 = 25;

/// A KNMI station near the market's (L24's upwind lead).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Neighbour {
    /// WMO number (`06215`); its EDR location is the WIGOS id.
    pub wmo: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
}

/// The lab's settings (fixed before looking at results).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LabSim {
    /// USD at the price paid per trade (L3 trades F's shares).
    pub stake_usd: f64,
    /// The market station's position: clear-sky radiation (L25) and the
    /// bearings of the neighbours (L24).
    pub latitude: f64,
    pub longitude: f64,
    /// KNMI's global-radiation parameter (W/m²).
    pub radiation_parameter: String,
    /// KNMI's temperature parameter of the neighbours (°C).
    pub neighbour_parameter: String,
    pub neighbours: Vec<Neighbour>,
}

impl Default for LabSim {
    fn default() -> Self {
        // Schiphol (06240) and three KNMI stations around it: Voorschoten
        // near the coast to the south-west (Valkenburg's successor, which
        // closed in 2016), De Bilt inland to the south-east, Berkhout to
        // the north-north-east.
        Self {
            stake_usd: 20.0,
            latitude: 52.318,
            longitude: 4.790,
            radiation_parameter: "qg".into(),
            neighbour_parameter: "ta".into(),
            neighbours: vec![
                Neighbour {
                    wmo: "06215".into(),
                    name: "Voorschoten".into(),
                    latitude: 52.141,
                    longitude: 4.437,
                },
                Neighbour {
                    wmo: "06260".into(),
                    name: "De Bilt".into(),
                    latitude: 52.100,
                    longitude: 5.180,
                },
                Neighbour {
                    wmo: "06249".into(),
                    name: "Berkhout".into(),
                    latitude: 52.644,
                    longitude: 4.979,
                },
            ],
        }
    }
}

impl LabSim {
    /// Initial bearing (degrees true, 0–360) from the station to `n`.
    pub fn bearing_to(&self, n: &Neighbour) -> f64 {
        bearing(self.latitude, self.longitude, n.latitude, n.longitude)
    }
}

/// Readings of KNMI parameters by local date (oldest first within a day).
pub type SeriesHistory = BTreeMap<NaiveDate, Vec<SeriesPoint>>;

/// What only the lab reads. Each part may be missing: a rule without its
/// input is reported as not replayed.
#[derive(Debug, Clone, Default)]
pub struct LabInputs {
    /// The day-1 forecast, also when the model does not use it (L14–L17).
    pub forecasts: Option<ForecastHistory>,
    /// KNMI global radiation at the station (L25).
    pub radiation: Option<SeriesHistory>,
    /// KNMI temperatures of the neighbours by WMO number (L24).
    pub neighbours: BTreeMap<String, SeriesHistory>,
}

/// How much of each input the replayed market days had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LabCoverage {
    pub days: u64,
    pub knmi_days: u64,
    pub forecast_days: u64,
    /// Days with a forecast for the day before too (L17).
    pub forecast_pairs: u64,
    pub radiation_days: u64,
    pub neighbour_days: u64,
    /// Reports whose METAR weather groups were read.
    pub weather_reports: u64,
    /// F's trades on days with KNMI readings (L3).
    pub f_trades: u64,
    /// Takers scored by the end, and how many counted as skilled (t ≥ 2) or
    /// as losing.
    pub wallets: u64,
    pub skilled: u64,
    pub unskilled: u64,
}

/// One replayed rule's mechanism and parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    /// L1: a NO bid on the next degree before reports, only while KNMI's
    /// readings shield it.
    ShieldedMaker { shield: bool },
    /// L2: a NO bid on the high's bucket once KNMI says the next METAR
    /// raises the high.
    InformedMaker { margin: i32 },
    /// L3: F's trades, sold before the METAR when KNMI sees a new high
    /// (`None`: held, F as is).
    FExit { margin: Option<i32> },
    /// L4: YES of the high's bucket when KNMI shows the afternoon cooling.
    CoolingLock { lo: f64, hi: f64 },
    /// L5: NO of the next degree late in the day under KNMI cooling.
    LateNextNo { start: u16 },
    /// L6: YES of the next degree when K fires late, the rise flattening.
    LateNewYes { filtered: bool },
    /// L7: K one reading early, by the slope of KNMI's means.
    SlopeK { target: i32 },
    /// L8: the sea breeze turned the wind onshore and moistened the air.
    SeaBreeze { yes_high: bool },
    /// L9: rain or thunder cooled the air well below the high.
    RainCap { thunder_only: bool },
    /// L10: the METAR's TREND forecasts showers, an onshore wind or low
    /// cloud (the variant: a NOSIG clear afternoon locks the high).
    TrendCap { nosig_lock: bool },
    /// L11: fog or low stratus in the morning, the favourite far above.
    FogFade { gap_tenths: i32 },
    /// L12: a clear, dry, continental morning: the bucket above the
    /// favourite.
    ClearSky { any_wind: bool },
    /// L13: an early high and a cold front through (veer, pressure rise).
    FrontLock { no_above: bool },
    /// L14: the morning's departure from the hourly forecast carried to
    /// the day's maximum.
    Anomaly { lambda: f64 },
    /// L15: well past the forecast's own peak hour, cooling.
    OwnPeak { after_min: i64 },
    /// L16: the forecast peaks in the evening; YES of the next degree.
    EveningHigh { min_rise: i32 },
    /// L17: yesterday's forecast error carried to today's ladder.
    YesterdayError { rho: f64 },
    /// L18: a burst of rise-side flow before a report, confirmed by KNMI.
    BurstFollow { knmi: bool },
    /// L19: follow takers with a winning record on the days before.
    SkillFollow { min_t: f64 },
    /// L20: sell the longshots that losing takers buy.
    LongshotFade { maker: bool },
    /// L21: fade the jump of a bucket two (one) above a new high.
    Overreaction { two_above: bool },
    /// L22: NO bids on the tails of the market's own favourite overnight.
    OvernightTails { min_away: usize },
    /// L23: a shower passed, the sky cleared, the temperature recovers.
    ShowerRecovery { knmi_slope: bool },
    /// L24: the upwind KNMI station is warmer (cooler) than Schiphol.
    Upwind { warm: bool },
    /// L25: global radiation collapsed (surged) against the clear sky.
    Radiation { collapse: bool },
}

/// Inputs a rule needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Needs {
    knmi: bool,
    forecast: bool,
    forecast_pair: bool,
    radiation: bool,
    neighbours: bool,
}

impl Kind {
    fn needs(self) -> Needs {
        use Kind::*;
        let knmi = Needs {
            knmi: true,
            ..Needs::default()
        };
        match self {
            ShieldedMaker { .. }
            | InformedMaker { .. }
            | FExit { .. }
            | CoolingLock { .. }
            | LateNextNo { .. }
            | LateNewYes { .. }
            | SlopeK { .. }
            | BurstFollow { .. }
            | Overreaction { .. } => knmi,
            ShowerRecovery { knmi_slope } => Needs {
                knmi: knmi_slope,
                ..Needs::default()
            },
            Anomaly { .. } | OwnPeak { .. } | EveningHigh { .. } => Needs {
                forecast: true,
                ..Needs::default()
            },
            YesterdayError { .. } => Needs {
                forecast: true,
                forecast_pair: true,
                ..Needs::default()
            },
            Upwind { .. } => Needs {
                knmi: true,
                neighbours: true,
                ..Needs::default()
            },
            Radiation { .. } => Needs {
                radiation: true,
                ..Needs::default()
            },
            SeaBreeze { .. }
            | RainCap { .. }
            | TrendCap { .. }
            | FogFade { .. }
            | ClearSky { .. }
            | FrontLock { .. }
            | SkillFollow { .. }
            | LongshotFade { .. }
            | OvernightTails { .. } => Needs::default(),
        }
    }
}

/// One replayed rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabRule {
    pub(crate) family: u8,
    pub(crate) label: String,
    pub(crate) kind: Kind,
}

/// The families' names, L1 first (the paper strategies' names).
pub const NAMES: [&str; FAMILIES as usize] = wm_strategy::lab::NAMES;

impl LabRule {
    pub(crate) fn name(&self) -> &'static str {
        NAMES[usize::from(self.family.saturating_sub(1)).min(NAMES.len() - 1)]
    }

    fn needs(&self) -> Needs {
        self.kind.needs()
    }

    /// The prices the rule trades, as the table shows them.
    pub(crate) fn range(&self) -> String {
        use Kind::*;
        match self.kind {
            ShieldedMaker { .. } => "maker NO, YES 0.03–0.40".into(),
            InformedMaker { .. } => "maker NO, YES 0.25–0.97".into(),
            FExit { margin: None } => "F as is".into(),
            FExit { margin: Some(_) } => "F, sold at the bid".into(),
            CoolingLock { lo, hi } => format!("YES {lo:.2}–{hi:.2}"),
            LateNextNo { .. } => "NO 0.75–0.97".into(),
            LateNewYes { .. } => "YES 0.20–0.70".into(),
            SlopeK { .. } => "NO 0.05–0.75".into(),
            SeaBreeze { yes_high: false } => "NO 0.60–0.96".into(),
            SeaBreeze { yes_high: true } => "YES 0.50–0.92".into(),
            RainCap { .. } => "NO 0.60–0.96".into(),
            TrendCap { nosig_lock: false } => "NO 0.55–0.94".into(),
            TrendCap { nosig_lock: true } => "YES 0.55–0.92".into(),
            FogFade { .. } => "NO 0.30–0.70".into(),
            ClearSky { .. } => "YES 0.06–0.30".into(),
            FrontLock { no_above: false } => "YES 0.40–0.88".into(),
            FrontLock { no_above: true } => "NO 0.60–0.95".into(),
            Anomaly { .. } => "YES 0.05–0.35".into(),
            OwnPeak { .. } => "YES 0.70–0.95".into(),
            EveningHigh { .. } => "YES 0.05–0.30".into(),
            YesterdayError { .. } => "YES 0.05–0.30".into(),
            BurstFollow { .. } => "NO 0.05–0.85".into(),
            SkillFollow { .. } => "their price + 0.03".into(),
            LongshotFade { maker: true } => "maker NO, their YES 0.02–0.15".into(),
            LongshotFade { maker: false } => "NO 0.80–0.98".into(),
            Overreaction { two_above: true } => "NO 0.60–0.92".into(),
            Overreaction { two_above: false } => "NO 0.50–0.85".into(),
            OvernightTails { .. } => "maker NO, YES 0.01–0.05".into(),
            ShowerRecovery { .. } => "YES 0.05–0.35".into(),
            Upwind { warm: true } => "NO 0.05–0.75".into(),
            Upwind { warm: false } => "NO 0.65–0.96".into(),
            Radiation { collapse: true } => "NO 0.60–0.95".into(),
            Radiation { collapse: false } => "YES 0.05–0.35".into(),
        }
    }

    pub(crate) fn key(&self) -> (&'static str, &str, u32, String) {
        (STRUCTURE, &self.label, 0, self.range())
    }

    /// The rule in one phrase, for the report.
    pub(crate) fn describe(&self) -> String {
        use Kind::*;
        match self.kind {
            ShieldedMaker { shield: true } => "10:00–20:00 local, a NO bid one tick under the next degree's YES ask from each KNMI reading to the next or the report after it, only while KNMI's means of the last 30′ are ≥ 0.5 °C under the high's rounding edge and not rising".into(),
            ShieldedMaker { shield: false } => "the same NO bids without KNMI's shield (the control)".into(),
            InformedMaker { margin } => format!("a NO bid one tick under the high bucket's YES ask once KNMI's mean is ≥ the rounding edge + {:.1} °C, until the anticipated report is public", f64::from(margin) / 10.0),
            FExit { margin: Some(m) } => format!("F's trades, sold at the bid before the METAR once KNMI's mean is ≥ the bucket's rounding edge + {:.1} °C", f64::from(m) / 10.0),
            FExit { margin: None } => "F's trades on the same days, held (the control)".into(),
            CoolingLock { .. } => "13:00–20:00 local, YES of the high's bucket when KNMI's maxima of the last 90′ stay ≥ 0.3 °C under its rounding edge, the mean fell ≥ 0.6 °C in an hour and the METAR is ≥ 1 °C under the high".into(),
            LateNextNo { start } => format!("{}–21:00 local, NO of the next degree when KNMI's means of the last hour stayed ≥ 1 °C and its maxima ≥ 0.5 °C under the rounding edge, falling", hm(start)),
            LateNewYes { filtered: true } => "YES of the next degree when K fires (KNMI ≥ edge + 0.8 °C) after the season's median peak time with a rise of ≤ 0.4 °C in 30′".into(),
            LateNewYes { filtered: false } => "YES of the next degree whenever K fires (the control: K · YES above)".into(),
            SlopeK { target } => format!("NO of the high's bucket when KNMI's mean is within 0.5 °C under to 0.8 °C over the edge and its 20′ slope reaches edge + {:.1} °C by the report", f64::from(target) / 10.0),
            SeaBreeze { yes_high } => format!("11:00–17:30 local on a day ≥ 20 °C: the wind turned onshore (250–020°, ≥ 6 kt) after an offshore morning, the dew point ≥ 1 °C above the high's report, the temperature ≥ 1 °C under the high → {}", if yes_high { "YES of the high's bucket" } else { "NO of the next degree" }),
            RainCap { thunder_only } => format!("12:00–19:00 local, {} and the temperature ≥ 2 °C under the high → NO of the next degree", if thunder_only { "thunder or a cumulonimbus" } else { "rain, showers or thunder now or since the last report" }),
            TrendCap { nosig_lock: false } => "11:00–17:00 local, a BECMG/TEMPO forecasting showers or thunder, an onshore wind (240–020°) or a ceiling under 3000 ft → NO of the next degree".into(),
            TrendCap { nosig_lock: true } => "11:00–19:00 local after the median peak time: NOSIG, no ceiling under 5000 ft, ≥ 1 °C under the high → YES of the high's bucket".into(),
            FogFade { gap_tenths } => format!("08:30–11:30 local, fog or mist under 5 km or a ceiling ≤ 800 ft, the favourite (≥ 0.30) ≥ {:.0} °C above the temperature → NO of the favourite", f64::from(gap_tenths) / 10.0),
            ClearSky { any_wind } => format!("10:00–13:00 local, clear (no ceiling under 5000 ft), the dew point ≥ 8 °C under the temperature{} → YES of the bucket above the favourite", if any_wind { "" } else { ", the wind continental (045–225°) or calm" }),
            FrontLock { no_above } => format!("09:00–15:00 local, the high first reached before 11:00, since then the wind veered from 120–229° to 230–340° (≥ 8 kt), QNH ≥ 1 hPa up and ≥ 1.5 °C under the high → {}", if no_above { "NO of the next degree" } else { "YES of the high's bucket" }),
            Anomaly { lambda } => format!("10:00–12:30 local, the forecast's remaining maximum + {lambda:.1} × (observed − forecast now) → YES of that bucket when the market favours another"),
            OwnPeak { after_min } => format!("≥ {after_min}′ after the hourly forecast's peak (and from 12:00), the forecast rise ≤ −1 °C, ≥ 1 °C under the high → YES of the high's bucket"),
            EveningHigh { min_rise } => format!("on days the forecast's 17–24 h maximum beats its 10–17 h one by ≥ 0.5 °C: 14:00–18:00 local, forecast rise ≥ {:.1} °C, within 1 °C of the high → YES of the next degree", f64::from(min_rise) / 10.0),
            YesterdayError { rho } => format!("07:00–10:00 local, today's forecast maximum + {rho:.1} × yesterday's error (observed − forecast) → YES of that bucket when the raw forecast's bucket differs"),
            BurstFollow { knmi: true } => "6′ before a routine report until it is known: ≥ 40 shares from ≥ 2 takers within 3′ selling the high's bucket or buying the next, and KNMI ≥ edge + 0.3 °C → NO of the high's bucket 10″ later".into(),
            BurstFollow { knmi: false } => "the same bursts without KNMI's confirmation (the control)".into(),
            SkillFollow { min_t } => format!("takers whose ≥ 30 earlier trades made ≥ 0.02 a share (t ≥ {min_t:.0}): their side 30″ later within 10′ at ≤ their price + 0.03"),
            LongshotFade { maker: true } => "takers whose ≥ 30 earlier trades lost ≥ 0.05 a share buying YES at 0.02–0.15: a NO bid one tick under their price for 30′".into(),
            LongshotFade { maker: false } => "the same buys, faded by taking NO at 0.80–0.98 within 10′".into(),
            Overreaction { two_above } => format!("2–15′ after a new high, {} bought at ≥ {:.2} since the report and KNMI ≤ the new edge + 0.3 °C → its NO", if two_above { "the bucket two degrees above" } else { "the next degree" }, if two_above { 0.08 } else { 0.15 }),
            OvernightTails { min_away } => format!("00:00–09:00 local, NO bids one tick under the YES ask (0.01–0.05) on buckets ≥ {min_away} places from the market's favourite (≥ 0.20) that can still win"),
            ShowerRecovery { knmi_slope } => format!("12:00–16:30 local, rain in the last 3 h, now dry with no ceiling under 4000 ft, within 1.5 °C of the high and {} → YES of the next degree", if knmi_slope { "KNMI's mean up ≥ 0.5 °C in 30′" } else { "the METAR slope ≥ 1 °C/h" }),
            Upwind { warm: true } => "10:00–18:00 local, wind ≥ 6 kt from within 40° of a neighbour ≥ 0.8 °C warmer than Schiphol, KNMI within 0.6 °C of the edge → NO of the high's bucket".into(),
            Upwind { warm: false } => "12:00–18:00 local, wind from a neighbour ≥ 1.0 °C cooler, KNMI ≥ 0.5 °C under the edge → NO of the next degree".into(),
            Radiation { collapse: true } => "10:30–15:30 local, KNMI's global radiation under 35% of the clear sky for 30′ after ≥ 70% for the hour before, KNMI ≥ 0.3 °C under the edge → NO of the next degree".into(),
            Radiation { collapse: false } => "the clearing: ≥ 80% of the clear sky after ≤ 40%, KNMI within 0.8 °C of the edge → YES of the next degree".into(),
        }
    }
}

fn hm(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Every rule: per family the main rule first, then its variant or control.
pub(crate) fn rules() -> Vec<LabRule> {
    use Kind::*;
    let list: Vec<(u8, &str, Kind)> = vec![
        (1, "L1", ShieldedMaker { shield: true }),
        (1, "L1 · no shield", ShieldedMaker { shield: false }),
        (2, "L2", InformedMaker { margin: 3 }),
        (2, "L2 · margin 0.6", InformedMaker { margin: 6 }),
        (3, "L3", FExit { margin: Some(3) }),
        (3, "L3 · exit at +0.0", FExit { margin: Some(0) }),
        (3, "L3 · hold (F)", FExit { margin: None }),
        (4, "L4", CoolingLock { lo: 0.70, hi: 0.95 }),
        (4, "L4 · 0.90–0.98", CoolingLock { lo: 0.90, hi: 0.98 }),
        (5, "L5", LateNextNo { start: 15 * 60 }),
        (5, "L5 · from 17:00", LateNextNo { start: 17 * 60 }),
        (6, "L6", LateNewYes { filtered: true }),
        (6, "L6 · unfiltered", LateNewYes { filtered: false }),
        (7, "L7", SlopeK { target: 6 }),
        (7, "L7 · to +1.0", SlopeK { target: 10 }),
        (8, "L8", SeaBreeze { yes_high: false }),
        (8, "L8 · YES high", SeaBreeze { yes_high: true }),
        (
            9,
            "L9",
            RainCap {
                thunder_only: false,
            },
        ),
        (9, "L9 · thunder", RainCap { thunder_only: true }),
        (10, "L10", TrendCap { nosig_lock: false }),
        (10, "L10 · NOSIG lock", TrendCap { nosig_lock: true }),
        (11, "L11", FogFade { gap_tenths: 60 }),
        (11, "L11 · gap 4 °C", FogFade { gap_tenths: 40 }),
        (12, "L12", ClearSky { any_wind: false }),
        (12, "L12 · any wind", ClearSky { any_wind: true }),
        (13, "L13", FrontLock { no_above: false }),
        (13, "L13 · NO above", FrontLock { no_above: true }),
        (14, "L14", Anomaly { lambda: 0.7 }),
        (14, "L14 · λ 1.0", Anomaly { lambda: 1.0 }),
        (15, "L15", OwnPeak { after_min: 120 }),
        (15, "L15 · +60′", OwnPeak { after_min: 60 }),
        (16, "L16", EveningHigh { min_rise: 10 }),
        (16, "L16 · rise 0.5", EveningHigh { min_rise: 5 }),
        (17, "L17", YesterdayError { rho: 0.5 }),
        (17, "L17 · full error", YesterdayError { rho: 1.0 }),
        (18, "L18", BurstFollow { knmi: true }),
        (18, "L18 · no KNMI", BurstFollow { knmi: false }),
        (19, "L19", SkillFollow { min_t: 2.0 }),
        (19, "L19 · t ≥ 3", SkillFollow { min_t: 3.0 }),
        (20, "L20", LongshotFade { maker: true }),
        (20, "L20 · taker", LongshotFade { maker: false }),
        (21, "L21", Overreaction { two_above: true }),
        (21, "L21 · next degree", Overreaction { two_above: false }),
        (22, "L22", OvernightTails { min_away: 3 }),
        (22, "L22 · ≥ 4 away", OvernightTails { min_away: 4 }),
        (23, "L23", ShowerRecovery { knmi_slope: true }),
        (
            23,
            "L23 · METAR slope",
            ShowerRecovery { knmi_slope: false },
        ),
        (24, "L24", Upwind { warm: true }),
        (24, "L24 · cool side", Upwind { warm: false }),
        (25, "L25", Radiation { collapse: true }),
        (25, "L25 · clearing", Radiation { collapse: false }),
    ];
    list.into_iter()
        .map(|(family, label, kind)| LabRule {
            family,
            label: label.to_owned(),
            kind,
        })
        .collect()
}

/// The METAR weather groups of one report, with its dew point.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReportWx {
    pub(crate) wx: MetarWx,
    pub(crate) dew_tenths: Option<i32>,
}

/// The weather of each decision's report (the last observation at its
/// time), aligned with `decisions`.
pub(crate) fn weather_of(decisions: &[Decision], obs: &[&Observation]) -> Vec<Option<ReportWx>> {
    decisions
        .iter()
        .map(|d| {
            obs.iter()
                .rev()
                .find(|o| o.key.observed_at == d.at)
                .map(|o| ReportWx {
                    wx: parse_wx(&o.raw_text),
                    dew_tenths: o.dewpoint.map(|t| t.tenths()),
                })
        })
        .collect()
}

/// Learn a settled day's taker trades (after replaying it): each trade's
/// P&L a share after the taker fee, held to settlement.
pub(crate) fn learn_day(
    book: &mut WalletBook,
    per_bucket: &[Vec<&MarketTrade>],
    winner: usize,
    fee_rate: f64,
) {
    book.add_trades(
        per_bucket.iter().enumerate().flat_map(|(i, trades)| {
            trades.iter().filter_map(move |t| {
                Some(ScoredTrade {
                    taker: t.taker.as_deref()?,
                    yes_price: t.yes_price,
                    taker_buys_yes: t.taker_buys_yes,
                    bucket_won: i == winner,
                })
            })
        }),
        fee_rate,
    );
}

/// One market day's inputs.
pub(crate) struct LabDay<'a> {
    pub(crate) date: NaiveDate,
    pub(crate) buckets: &'a [TemperatureBucket],
    pub(crate) labels: &'a [String],
    pub(crate) winner: usize,
    /// Every report of the day, from local midnight.
    pub(crate) decisions: &'a [Decision],
    /// Each decision's METAR weather (aligned with `decisions`).
    pub(crate) wx: &'a [Option<ReportWx>],
    pub(crate) per_bucket: &'a [Vec<&'a MarketTrade>],
    pub(crate) peak: &'a PeakTimes,
    pub(crate) knmi: Option<&'a [TenMinuteObservation]>,
    pub(crate) radiation: Option<&'a [SeriesPoint]>,
    /// Neighbours with readings this day: (station, bearing from the
    /// market's station, readings).
    pub(crate) neighbours: &'a [(&'a Neighbour, f64, &'a [SeriesPoint])],
    pub(crate) forecast: Option<&'a ForecastDay>,
    /// Yesterday's observed high minus its forecast maximum (tenths).
    pub(crate) yesterday_error_tenths: Option<i32>,
    pub(crate) wallets: &'a WalletBook,
    /// The day's replayed F trades.
    pub(crate) f_trades: &'a [SimTrade],
    pub(crate) tz: Tz,
}

impl LabDay<'_> {
    fn trades(&self, i: usize) -> &[&MarketTrade] {
        self.per_bucket.get(i).map_or(&[][..], Vec::as_slice)
    }

    fn bucket_of(&self, v: i32) -> Option<usize> {
        self.buckets.iter().position(|b| b.contains(v))
    }

    /// The bucket of the high when the next degree is not in it (it dies
    /// with a one-degree rise).
    fn high_bucket(&self, high: i32) -> Option<usize> {
        self.bucket_of(high)
            .filter(|i| !self.buckets[*i].contains(high + 1))
    }

    /// The bucket of the next degree when it is not the high's.
    fn next_bucket(&self, high: i32) -> Option<usize> {
        self.bucket_of(high + 1)
            .filter(|i| !self.buckets[*i].contains(high))
    }

    fn day_end(&self) -> DateTime<Utc> {
        local_day_bounds(self.date, self.tz).1
    }

    /// The latest decision known at `t`.
    fn decision_at(&self, t: DateTime<Utc>) -> Option<usize> {
        self.decisions
            .partition_point(|d| d.knowledge <= t)
            .checked_sub(1)
    }

    /// The decision whose report was observed at `at`.
    fn decision_observed(&self, at: DateTime<Utc>) -> Option<usize> {
        self.decisions.iter().position(|d| d.at == at)
    }

    /// While decision `k` stands: until the next one is known or the day
    /// ends.
    fn until(&self, k: usize) -> DateTime<Utc> {
        self.decisions
            .get(k + 1)
            .map_or(self.day_end(), |n| n.knowledge)
            .min(self.day_end())
    }

    fn minute(&self, t: DateTime<Utc>) -> u16 {
        local_minute_of_day(t, self.tz)
    }

    fn wx(&self, k: usize) -> Option<&ReportWx> {
        self.wx.get(k).and_then(Option::as_ref)
    }

    /// The season's median time of first reaching the high (14:00 without
    /// history).
    fn season_median(&self, d: &Decision) -> u16 {
        self.peak
            .season(d.f.season)
            .and_then(|s| s.quantile(0.5))
            .unwrap_or(14 * 60)
    }

    /// The bucket the market favours at a decision: the highest price
    /// (midpoint, else ask, else bid).
    fn favourite(&self, d: &Decision) -> Option<(usize, f64)> {
        d.quotes
            .iter()
            .enumerate()
            .filter_map(|(i, q)| q.mid.or(q.yes_ask).or(q.yes_bid).map(|p| (i, p)))
            .fold(None, |best: Option<(usize, f64)>, (i, p)| match best {
                Some((_, b)) if b >= p => best,
                _ => Some((i, p)),
            })
    }
}

/// What every rule shares.
pub(crate) struct Ctx<'a> {
    pub(crate) sim: &'a MarketSimConfig,
    pub(crate) fee_rate: f64,
    pub(crate) routine: &'a [u8],
    /// A METAR is known this long after its observation.
    pub(crate) metar_delay: Duration,
}

impl Ctx<'_> {
    fn knmi_delay(&self) -> Duration {
        Duration::minutes(self.sim.gk.knmi_delay_minutes)
    }

    fn fresh(&self) -> Duration {
        Duration::minutes(self.sim.gk.fresh_quote_minutes)
    }

    fn fee(&self, p: f64) -> f64 {
        self.fee_rate * p * (1.0 - p)
    }

    fn taker(&self, p: f64, won: bool) -> f64 {
        f64::from(u8::from(won)) - p - self.fee(p) - self.sim.slippage
    }

    fn maker(&self, p: f64, won: bool) -> f64 {
        f64::from(u8::from(won)) - p + self.sim.maker.rebate_share * self.fee(p)
    }

    /// Expected profit a share of a taker buy at `p` winning with `p_win`.
    fn ev(&self, p_win: f64, p: f64) -> f64 {
        p_win - p - self.fee(p) - self.sim.slippage
    }

    /// When a resting order from `at` is withdrawn before the next routine
    /// report (`None`: less than `min_rest` to go).
    fn expiry(&self, at: DateTime<Utc>, min_rest: i64) -> Option<DateTime<Utc>> {
        let c = cancel_time(at, self.routine, self.sim.maker.cancel_before_report_min)?;
        (c - at >= Duration::minutes(min_rest)).then_some(c)
    }
}

/// The KNMI reading whose interval ends at `t`.
fn reading_at(rs: &[TenMinuteObservation], t: DateTime<Utc>) -> Option<&TenMinuteObservation> {
    rs.binary_search_by_key(&t, |r| r.interval_end)
        .ok()
        .map(|i| &rs[i])
}

fn mean_at(rs: &[TenMinuteObservation], t: DateTime<Utc>) -> Option<i32> {
    reading_at(rs, t)?.mean.map(|m| m.tenths())
}

/// Readings whose intervals end in `(end − minutes, end]`.
fn window(
    rs: &[TenMinuteObservation],
    end: DateTime<Utc>,
    minutes: i64,
) -> &[TenMinuteObservation] {
    let lo = rs.partition_point(|r| r.interval_end <= end - Duration::minutes(minutes));
    let hi = rs.partition_point(|r| r.interval_end <= end);
    &rs[lo..hi.max(lo)]
}

/// The latest reading known at `t`.
fn latest_known(
    rs: &[TenMinuteObservation],
    t: DateTime<Utc>,
    delay: Duration,
) -> Option<&TenMinuteObservation> {
    let i = rs.partition_point(|r| r.interval_end + delay <= t);
    i.checked_sub(1).map(|k| &rs[k])
}

fn in_window(minute: u16, start: u16, end: u16) -> bool {
    (start..end).contains(&minute)
}

fn round_half_up(x: f64) -> i32 {
    (x + 0.5).floor() as i32
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// How a trade is sized.
#[derive(Debug, Clone, Copy)]
enum Size {
    /// USD at the price paid.
    Stake(f64),
    Shares(f64),
}

/// A trade to record.
struct Fill {
    shown: DateTime<Utc>,
    bucket: usize,
    yes: bool,
    price: f64,
    p_model: f64,
    per_share: f64,
    won: bool,
    filled: Option<DateTime<Utc>>,
    size: Size,
}

fn record(day: &LabDay<'_>, rule: &LabRule, f: Fill) -> SimTrade {
    SimTrade {
        date: day.date,
        report: local_hm(f.shown, day.tz),
        structure: STRUCTURE.to_owned(),
        strategy: rule.label.clone(),
        window: 0,
        range: rule.range(),
        bucket: day.labels[f.bucket].clone(),
        side: if f.yes { "YES" } else { "NO" }.to_owned(),
        price: f.price,
        p_model: f.p_model,
        p_used: f.p_model,
        won: f.won,
        pnl_usd: match f.size {
            Size::Stake(usd) => usd / f.price.max(1e-6) * f.per_share,
            Size::Shares(n) => n * f.per_share,
        },
        resolved: day.labels[day.winner].clone(),
        filled: f.filled.map(|t| local_hm(t, day.tz)),
    }
}

/// A taker buy of `bucket` (`yes` or its NO) from `at` while the signal
/// stands, at a price inside `band` and, with `p_win = Some((p, min))`, an
/// expected profit of at least `min` a share. Recorded when it fills.
struct Take {
    shown: DateTime<Utc>,
    at: DateTime<Utc>,
    until: DateTime<Utc>,
    bucket: usize,
    yes: bool,
    band: (f64, f64),
    p_win: Option<(f64, f64)>,
    fresh: Duration,
}

fn take(day: &LabDay<'_>, rule: &LabRule, ctx: &Ctx<'_>, t: Take, out: &mut Vec<SimTrade>) -> bool {
    if t.until <= t.at {
        return false;
    }
    let ok = |p: f64| {
        p >= t.band.0 - 1e-9
            && p <= t.band.1 + 1e-9
            && t.p_win.is_none_or(|(pw, min)| ctx.ev(pw, p) >= min)
    };
    let Some((price, filled)) =
        taker_price(day.trades(t.bucket), t.at, t.until, t.fresh, t.yes, ok)
    else {
        return false;
    };
    let won = (t.bucket == day.winner) == t.yes;
    out.push(record(
        day,
        rule,
        Fill {
            shown: t.shown,
            bucket: t.bucket,
            yes: t.yes,
            price,
            p_model: t.p_win.map_or(price, |(p, _)| p),
            per_share: ctx.taker(price, won),
            won,
            filled: filled.or(Some(t.at)),
            size: Size::Stake(ctx.sim.lab.stake_usd),
        },
    ));
    true
}

/// A resting NO bid on `bucket` (the YES offered at `offer`) from `from`
/// to `until`. Recorded when a later taker buys YES through it.
#[allow(clippy::too_many_arguments)]
fn rest_no(
    day: &LabDay<'_>,
    rule: &LabRule,
    ctx: &Ctx<'_>,
    shown: DateTime<Utc>,
    bucket: usize,
    offer: f64,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    out: &mut Vec<SimTrade>,
) -> bool {
    if until <= from {
        return false;
    }
    let Some(at) = through_fill(day.trades(bucket), from, until, Resting::YesAsk(offer)) else {
        return false;
    };
    let price = 1.0 - offer;
    let won = bucket != day.winner;
    out.push(record(
        day,
        rule,
        Fill {
            shown,
            bucket,
            yes: false,
            price,
            p_model: price,
            per_share: ctx.maker(price, won),
            won,
            filled: Some(at),
            size: Size::Stake(ctx.sim.lab.stake_usd),
        },
    ));
    true
}

/// The YES offer one tick under the latest taker buy, when inside `band`.
fn offer_under_ask(
    day: &LabDay<'_>,
    ctx: &Ctx<'_>,
    bucket: usize,
    at: DateTime<Utc>,
    band: (f64, f64),
) -> Option<f64> {
    let (ask, _) = fresh_quote(day.trades(bucket), at, ctx.fresh());
    let ask = ask?;
    let offer = ask - tick(ask);
    (offer >= band.0 - 1e-9 && offer <= band.1 + 1e-9).then_some(offer)
}

/// Every lab rule's trades on one market day.
pub(crate) fn simulate_lab(day: &LabDay<'_>, ctx: &Ctx<'_>) -> Vec<SimTrade> {
    let mut out = Vec::new();
    for rule in rules() {
        let n = rule.needs();
        if (n.knmi && day.knmi.is_none())
            || (n.forecast && day.forecast.is_none())
            || (n.forecast_pair && day.yesterday_error_tenths.is_none())
            || (n.radiation && day.radiation.is_none())
            || (n.neighbours && day.neighbours.is_empty())
        {
            continue;
        }
        use Kind::*;
        match rule.kind {
            ShieldedMaker { shield } => shielded_maker(day, &rule, shield, ctx, &mut out),
            InformedMaker { margin } => informed_maker(day, &rule, margin, ctx, &mut out),
            FExit { margin } => f_exit(day, &rule, margin, ctx, &mut out),
            CoolingLock { lo, hi } => cooling_lock(day, &rule, (lo, hi), ctx, &mut out),
            LateNextNo { start } => late_next_no(day, &rule, start, ctx, &mut out),
            LateNewYes { filtered } => late_new_yes(day, &rule, filtered, ctx, &mut out),
            SlopeK { target } => slope_k(day, &rule, target, ctx, &mut out),
            SeaBreeze { yes_high } => sea_breeze(day, &rule, yes_high, ctx, &mut out),
            RainCap { thunder_only } => rain_cap(day, &rule, thunder_only, ctx, &mut out),
            TrendCap { nosig_lock } => trend_cap(day, &rule, nosig_lock, ctx, &mut out),
            FogFade { gap_tenths } => fog_fade(day, &rule, gap_tenths, ctx, &mut out),
            ClearSky { any_wind } => clear_sky(day, &rule, any_wind, ctx, &mut out),
            FrontLock { no_above } => front_lock(day, &rule, no_above, ctx, &mut out),
            Anomaly { lambda } => anomaly(day, &rule, lambda, ctx, &mut out),
            OwnPeak { after_min } => own_peak(day, &rule, after_min, ctx, &mut out),
            EveningHigh { min_rise } => evening_high(day, &rule, min_rise, ctx, &mut out),
            YesterdayError { rho } => yesterday_error(day, &rule, rho, ctx, &mut out),
            BurstFollow { knmi } => burst_follow(day, &rule, knmi, ctx, &mut out),
            SkillFollow { min_t } => skill_follow(day, &rule, min_t, ctx, &mut out),
            LongshotFade { maker } => longshot_fade(day, &rule, maker, ctx, &mut out),
            Overreaction { two_above } => overreaction(day, &rule, two_above, ctx, &mut out),
            OvernightTails { min_away } => overnight_tails(day, &rule, min_away, ctx, &mut out),
            ShowerRecovery { knmi_slope } => shower_recovery(day, &rule, knmi_slope, ctx, &mut out),
            Upwind { warm } => upwind(day, &rule, warm, ctx, &mut out),
            Radiation { collapse } => radiation(day, &rule, collapse, ctx, &mut out),
        }
    }
    out
}

// ---------------------------------------------------------------- KNMI ---

/// L1. Makers lose on the next degree just before reports (informed takers
/// know the METAR first); KNMI's readings say when no rise is coming (no
/// report of 3,448 rose with the mean ≥ 1 °C under the edge, 0.5% with it
/// 0.5–0.9 °C under), the more so when the mean is not climbing.
fn shielded_maker(
    day: &LabDay<'_>,
    rule: &LabRule,
    shield: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        if !in_window(day.minute(known), 10 * 60, 20 * 60) {
            continue;
        }
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let high = day.decisions[k].f.high_whole;
        let Some(i) = day.next_bucket(high) else {
            continue;
        };
        if done.contains(&i) {
            continue;
        }
        let edge = high * 10 + 5;
        if shield {
            let w = window(rs, r.interval_end, 30);
            if w.len() < 3
                || w.iter()
                    .any(|x| x.mean.is_none_or(|m| m.tenths() > edge - 5))
            {
                continue;
            }
            let (Some(now), Some(before)) = (
                r.mean.map(|m| m.tenths()),
                mean_at(rs, r.interval_end - Duration::minutes(20)),
            ) else {
                continue;
            };
            if now > before {
                continue;
            }
        }
        let Some(offer) = offer_under_ask(day, ctx, i, known, (0.03, 0.40)) else {
            continue;
        };
        let Some(report) = next_routine(known, ctx.routine) else {
            continue;
        };
        let until = (report + ctx.metar_delay)
            .min(known + Duration::minutes(10))
            .min(day.day_end());
        if rest_no(day, rule, ctx, r.interval_end, i, offer, known, until, out) {
            done.insert(i);
        }
    }
}

/// L2. K as a maker: the same signal, a resting NO bid instead of a taker
/// buy, filled by whoever still buys the doomed bucket's YES.
fn informed_maker(
    day: &LabDay<'_>,
    rule: &LabRule,
    margin: i32,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let d = &day.decisions[k];
        let Some(n) = next_routine(r.interval_end, ctx.routine) else {
            continue;
        };
        if r.interval_end <= d.at || n - r.interval_end > Duration::minutes(16) {
            continue;
        }
        let high = d.f.high_whole;
        if r.mean.is_none_or(|m| m.tenths() < high * 10 + 5 + margin) {
            continue;
        }
        let Some(h) = day.high_bucket(high) else {
            continue;
        };
        if done.contains(&h) {
            continue;
        }
        let Some(offer) = offer_under_ask(day, ctx, h, known, (0.25, 0.97)) else {
            continue;
        };
        let until = (n + ctx.metar_delay).min(day.day_end());
        if rest_no(day, rule, ctx, r.interval_end, h, offer, known, until, out) {
            done.insert(h);
        }
    }
}

/// When F's trade was filled (its fill time, else its report's knowledge
/// time). A fill time is kept to the minute: its end is taken, so no exit
/// signal can precede the fill.
fn entry_time(day: &LabDay<'_>, t: &SimTrade) -> Option<DateTime<Utc>> {
    match &t.filled {
        Some(hm) => {
            let (h, m) = hm.split_once(':')?;
            let minute = h.parse::<u16>().ok()? * 60 + m.parse::<u16>().ok()?;
            local_to_utc(day.date, minute, day.tz).map(|t| t + Duration::seconds(59))
        }
        None => day
            .decisions
            .iter()
            .find(|d| local_hm(d.at, day.tz) == t.report)
            .map(|d| d.knowledge),
    }
}

/// The first KNMI-signalled exit of a YES on bucket `i` (upper bound
/// `upper`) after `entry`: the bid then, and when.
fn exit_at_bid(
    day: &LabDay<'_>,
    rs: &[TenMinuteObservation],
    i: usize,
    upper: i32,
    margin: i32,
    entry: DateTime<Utc>,
    ctx: &Ctx<'_>,
) -> Option<(f64, DateTime<Utc>)> {
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        if known <= entry {
            continue;
        }
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let d = &day.decisions[k];
        if d.f.high_whole > upper {
            // A report already raised the high past the bucket.
            return None;
        }
        let Some(n) = next_routine(r.interval_end, ctx.routine) else {
            continue;
        };
        if r.interval_end <= d.at || n - r.interval_end > Duration::minutes(16) {
            continue;
        }
        if r.mean.is_none_or(|m| m.tenths() < upper * 10 + 5 + margin) {
            continue;
        }
        // Selling YES into the bid is buying NO at one minus it.
        if let Some((no, filled)) = taker_price(
            day.trades(i),
            known,
            day.until(k),
            Duration::minutes(1),
            false,
            |p| p <= 0.999,
        ) {
            return Some((1.0 - no, filled.unwrap_or(known)));
        }
    }
    None
}

/// L3. F's five losses were late new highs: KNMI sees them before the
/// METAR, so the YES is sold at the bid instead of expiring worthless.
fn f_exit(
    day: &LabDay<'_>,
    rule: &LabRule,
    margin: Option<i32>,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    for t in day.f_trades.iter().filter(|t| t.strategy == "F") {
        let Some(i) = day.labels.iter().position(|l| *l == t.bucket) else {
            continue;
        };
        let Some(entry) = entry_time(day, t) else {
            continue;
        };
        let cost = t.price + ctx.fee(t.price) + ctx.sim.slippage;
        let exit = match (margin, day.buckets[i].upper) {
            (Some(m), Some(u)) => exit_at_bid(day, rs, i, u, m, entry, ctx),
            _ => None,
        };
        let (per_share, won, filled) = match exit {
            Some((bid, at)) => {
                let p = bid - ctx.fee(bid) - ctx.sim.slippage - cost;
                (p, p > 0.0, Some(at))
            }
            None => (f64::from(u8::from(t.won)) - cost, t.won, None),
        };
        let mut trade = record(
            day,
            rule,
            Fill {
                shown: entry,
                bucket: i,
                yes: true,
                price: t.price,
                p_model: t.p_model,
                per_share,
                won,
                filled,
                size: Size::Shares(ctx.sim.f.shares),
            },
        );
        trade.report = t.report.clone();
        if exit.is_some() {
            trade.side = "YES, sold".into();
        }
        out.push(trade);
    }
}

/// L4. The 0.70–0.90 band won 83% at 80¢ and 0.90–0.98 97% at 95¢: the
/// high's bucket is underpriced, the more so when KNMI shows the cooling.
fn cooling_lock(
    day: &LabDay<'_>,
    rule: &LabRule,
    band: (f64, f64),
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        if !in_window(day.minute(known), 13 * 60, 20 * 60) {
            continue;
        }
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let d = &day.decisions[k];
        let Some(h) = day.bucket_of(d.f.high_whole) else {
            continue;
        };
        let Some(upper) = day.buckets[h].upper else {
            continue;
        };
        if done.contains(&h) || d.f.drop_tenths < 10 {
            continue;
        }
        let edge = upper * 10 + 5;
        let w = window(rs, r.interval_end, 90);
        if w.len() < 8
            || w.iter()
                .any(|x| x.max.or(x.mean).is_none_or(|m| m.tenths() > edge - 3))
        {
            continue;
        }
        let (Some(now), Some(hour_ago)) = (
            r.mean.map(|m| m.tenths()),
            mean_at(rs, r.interval_end - Duration::minutes(60)),
        ) else {
            continue;
        };
        if now > hour_ago - 6 {
            continue;
        }
        let t = Take {
            shown: r.interval_end,
            at: known,
            until: day.until(k).min(known + Duration::minutes(10)),
            bucket: h,
            yes: true,
            band,
            p_win: Some((0.97, 0.005)),
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(h);
        }
    }
}

/// L5. G's tails, one degree closer, late, when KNMI rules out the rise.
fn late_next_no(
    day: &LabDay<'_>,
    rule: &LabRule,
    start: u16,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        if !in_window(day.minute(known), start, 21 * 60) {
            continue;
        }
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let high = day.decisions[k].f.high_whole;
        let Some(i) = day.next_bucket(high) else {
            continue;
        };
        if done.contains(&i) {
            continue;
        }
        let edge = high * 10 + 5;
        let w = window(rs, r.interval_end, 60);
        if w.len() < 5
            || w.iter().any(|x| {
                x.mean.is_none_or(|m| m.tenths() > edge - 10)
                    || x.max.is_some_and(|m| m.tenths() > edge - 5)
            })
        {
            continue;
        }
        let (Some(now), Some(hour_ago)) = (
            r.mean.map(|m| m.tenths()),
            mean_at(rs, r.interval_end - Duration::minutes(60)),
        ) else {
            continue;
        };
        if now >= hour_ago {
            continue;
        }
        let t = Take {
            shown: r.interval_end,
            at: known,
            until: day.until(k).min(known + Duration::minutes(10)),
            bucket: i,
            yes: false,
            band: (0.75, 0.97),
            p_win: Some((0.99, 0.005)),
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(i);
        }
    }
}

/// L6. K · YES above lost because highs kept rising; late in the day, the
/// rise flattening, the new degree is likely the last.
fn late_new_yes(
    day: &LabDay<'_>,
    rule: &LabRule,
    filtered: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let d = &day.decisions[k];
        let Some(n) = next_routine(r.interval_end, ctx.routine) else {
            continue;
        };
        if r.interval_end <= d.at || n - r.interval_end > Duration::minutes(16) {
            continue;
        }
        let high = d.f.high_whole;
        let Some(now) = r.mean.map(|m| m.tenths()) else {
            continue;
        };
        if now < high * 10 + 5 + 8 {
            continue;
        }
        if filtered {
            if day.minute(known) < day.season_median(d) {
                continue;
            }
            let Some(before) = mean_at(rs, r.interval_end - Duration::minutes(30)) else {
                continue;
            };
            if now - before > 4 {
                continue;
            }
        }
        let Some(j) = day.next_bucket(high) else {
            continue;
        };
        if done.contains(&j) {
            continue;
        }
        let t = Take {
            shown: r.interval_end,
            at: known,
            until: day.until(k),
            bucket: j,
            yes: true,
            band: (0.20, 0.70),
            p_win: None,
            fresh: Duration::minutes(1),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(j);
        }
    }
}

/// L7. The mean's slope over 20′ carried to the report: K's signal one
/// reading earlier, at a lower price.
fn slope_k(day: &LabDay<'_>, rule: &LabRule, target: i32, ctx: &Ctx<'_>, out: &mut Vec<SimTrade>) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for r in rs {
        let known = r.interval_end + ctx.knmi_delay();
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let d = &day.decisions[k];
        let Some(n) = next_routine(r.interval_end, ctx.routine) else {
            continue;
        };
        let lead = n - r.interval_end;
        if r.interval_end <= d.at || lead > Duration::minutes(16) {
            continue;
        }
        let (Some(m0), Some(m20)) = (
            r.mean.map(|m| m.tenths()),
            mean_at(rs, r.interval_end - Duration::minutes(20)),
        ) else {
            continue;
        };
        let slope = f64::from(m0 - m20) / 20.0;
        let high = d.f.high_whole;
        let edge = high * 10 + 5;
        if slope <= 0.0 || m0 >= edge + 8 || m0 < edge - 5 {
            continue;
        }
        let projected = f64::from(m0) + slope * lead.num_minutes() as f64;
        if projected < f64::from(edge + target) {
            continue;
        }
        let Some(h) = day.high_bucket(high) else {
            continue;
        };
        if done.contains(&h) {
            continue;
        }
        let t = Take {
            shown: r.interval_end,
            at: known,
            until: day.until(k),
            bucket: h,
            yes: false,
            band: (0.05, 0.75),
            p_win: Some((0.85, 0.02)),
            fresh: Duration::minutes(1),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(h);
        }
    }
}

// --------------------------------------------------------------- METAR ---

/// L8. Schiphol lies ~15 km inland: the sea-breeze front passes in the
/// early afternoon with a small drop and a humidity rise (EUMeTrain), and
/// the high of that day is usually in.
fn sea_breeze(
    day: &LabDay<'_>,
    rule: &LabRule,
    yes_high: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 11 * 60, 17 * 60 + 30)
            || d.f.high_whole < 20
            || d.f.drop_tenths < 10
        {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        let Some(w) = now.wx.wind() else { continue };
        if !(w.from_arc(250, 20) && w.speed_kt >= 6) {
            continue;
        }
        let offshore_before = (0..k).any(|j| {
            day.decisions[j].f.local_minute_now >= 7 * 60
                && day
                    .wx(j)
                    .and_then(|x| x.wx.wind())
                    .is_some_and(|w| w.from_arc(60, 220) && w.speed_kt >= 3)
        });
        if !offshore_before {
            continue;
        }
        let Some(hk) = day.decision_observed(d.f.high_at) else {
            continue;
        };
        let (Some(dew_now), Some(dew_high)) =
            (now.dew_tenths, day.wx(hk).and_then(|x| x.dew_tenths))
        else {
            continue;
        };
        if dew_now < dew_high + 10 {
            continue;
        }
        let high = d.f.high_whole;
        let t = if yes_high {
            let Some(h) = day.bucket_of(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: h,
                yes: true,
                band: (0.50, 0.92),
                p_win: Some((0.94, 0.01)),
                fresh: ctx.fresh(),
            }
        } else {
            let Some(i) = day.next_bucket(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: i,
                yes: false,
                band: (0.60, 0.96),
                p_win: Some((0.95, 0.01)),
                fresh: ctx.fresh(),
            }
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L9. Rain-cooled air (gust fronts drop 3–8 °C): the day's high is
/// usually in once a shower has cooled the station well below it.
fn rain_cap(
    day: &LabDay<'_>,
    rule: &LabRule,
    thunder_only: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 12 * 60, 19 * 60) || d.f.drop_tenths < 20 {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        let wet = if thunder_only {
            now.wx.thunder_now_or_recent()
        } else {
            now.wx.precipitation_now_or_recent() || now.wx.thunder_now_or_recent()
        };
        if !wet {
            continue;
        }
        let Some(i) = day.next_bucket(d.f.high_whole) else {
            continue;
        };
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: i,
            yes: false,
            band: (0.60, 0.96),
            p_win: Some((0.95, 0.005)),
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L10. Schiphol's METARs carry KNMI's two-hour TREND (AUTOTREND guidance
/// plus the forecaster): showers, an onshore shift or low cloud announced
/// there cap the afternoon before the temperature shows it.
fn trend_cap(
    day: &LabDay<'_>,
    rule: &LabRule,
    nosig_lock: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        let Some(now) = day.wx(k) else { continue };
        let high = d.f.high_whole;
        let t = if nosig_lock {
            if !in_window(d.f.local_minute_now, 11 * 60, 19 * 60)
                || d.f.local_minute_now < day.season_median(d)
                || d.f.drop_tenths < 10
                || !(now.wx.nosig() && now.wx.clear(5000))
            {
                continue;
            }
            let Some(h) = day.bucket_of(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: h,
                yes: true,
                band: (0.55, 0.92),
                p_win: Some((0.93, 0.005)),
                fresh: ctx.fresh(),
            }
        } else {
            if !in_window(d.f.local_minute_now, 11 * 60, 17 * 60) {
                continue;
            }
            let capped = now.wx.trend_precipitation()
                || now.wx.trend_wind_from(240, 20)
                || now.wx.trend_ceiling_below(3000);
            if !capped {
                continue;
            }
            let Some(i) = day.next_bucket(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: i,
                yes: false,
                band: (0.55, 0.94),
                p_win: Some((0.92, 0.005)),
                fresh: ctx.fresh(),
            }
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L11. A stratus deck that does not mix out busts the maximum forecast;
/// the market, anchored on it, keeps the favourite too high.
fn fog_fade(
    day: &LabDay<'_>,
    rule: &LabRule,
    gap_tenths: i32,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 8 * 60 + 30, 11 * 60 + 30) {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        let grey = (now.wx.body.fog_or_mist()
            && now.wx.body.visibility_m.is_some_and(|v| v < 5000))
            || now.wx.body.ceiling_ft().is_some_and(|c| c <= 800);
        if !grey {
            continue;
        }
        let Some((fav, p)) = day.favourite(d) else {
            continue;
        };
        let Some(lower) = day.buckets[fav].lower else {
            continue;
        };
        if p < 0.30 || lower * 10 - d.f.current_tenths < gap_tenths {
            continue;
        }
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: fav,
            yes: false,
            band: (0.30, 0.70),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L12. Dry continental air under a clear sky heats past the forecast;
/// the 0.10–0.30 band already won 21% at 19¢ on all days.
fn clear_sky(
    day: &LabDay<'_>,
    rule: &LabRule,
    any_wind: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 10 * 60, 13 * 60) {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        if !now.wx.clear(5000) {
            continue;
        }
        let Some(dew) = now.dew_tenths else { continue };
        if d.f.current_tenths - dew < 80 {
            continue;
        }
        let Some(w) = now.wx.wind() else { continue };
        if !(any_wind || w.direction.is_none() || w.from_arc(45, 225)) {
            continue;
        }
        let Some((fav, p)) = day.favourite(d) else {
            continue;
        };
        let Some(upper) = day.buckets[fav].upper else {
            continue;
        };
        if p < 0.25 {
            continue;
        }
        let Some(target) = day.bucket_of(upper + 1) else {
            continue;
        };
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: target,
            yes: true,
            band: (0.06, 0.30),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L13. A cold front through in the morning sets the day's high early
/// (25% of winter highs, 11% of autumn ones before 09:00); the market and
/// F wait for the afternoon.
fn front_lock(
    day: &LabDay<'_>,
    rule: &LabRule,
    no_above: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 9 * 60, 15 * 60) || d.f.drop_tenths < 15 {
            continue;
        }
        let first_reach = d.at - Duration::minutes(d.f.minutes_since_first_high);
        if day.minute(first_reach) >= 11 * 60 {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        let Some(hk) = day.decision_observed(d.f.high_at) else {
            continue;
        };
        let Some(then) = day.wx(hk) else { continue };
        let veered = now
            .wx
            .wind()
            .is_some_and(|w| w.from_arc(230, 340) && w.speed_kt >= 8)
            && then.wx.wind().is_some_and(|w| w.from_arc(120, 229));
        let rising = matches!((now.wx.qnh_hpa, then.wx.qnh_hpa), (Some(a), Some(b)) if a > b);
        if !(veered && rising) {
            continue;
        }
        let high = d.f.high_whole;
        let t = if no_above {
            let Some(i) = day.next_bucket(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: i,
                yes: false,
                band: (0.60, 0.95),
                p_win: Some((0.95, 0.005)),
                fresh: ctx.fresh(),
            }
        } else {
            let Some(h) = day.bucket_of(high) else {
                continue;
            };
            Take {
                shown: d.at,
                at: d.knowledge,
                until: day.until(k),
                bucket: h,
                yes: true,
                band: (0.40, 0.88),
                p_win: Some((0.90, 0.01)),
                fresh: ctx.fresh(),
            }
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

// ------------------------------------------------------------ forecast ---

/// The forecast at `t` (tenths), linear between the hourly values.
fn forecast_at(fc: &ForecastDay, t: DateTime<Utc>) -> Option<f64> {
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

/// The time of the forecast's first maximum.
fn forecast_peak(fc: &ForecastDay) -> Option<DateTime<Utc>> {
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

/// L14. The morning's departure from the hourly forecast persists into the
/// afternoon; bots compare the market with the forecast maximum, not with
/// the forecast's own hour-by-hour path.
fn anomaly(day: &LabDay<'_>, rule: &LabRule, lambda: f64, ctx: &Ctx<'_>, out: &mut Vec<SimTrade>) {
    let Some(fc) = day.forecast else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 10 * 60, 12 * 60 + 30) || d.knowledge < fc.known_at {
            continue;
        }
        let (Some(now), Some(rest)) = (forecast_at(fc, d.at), fc.remaining_max_tenths(d.at)) else {
            continue;
        };
        let departure = (f64::from(d.f.current_tenths) - now) / 10.0;
        let predicted = f64::from(rest) / 10.0 + lambda * departure;
        let v = round_half_up(predicted).max(d.f.high_whole);
        let Some(target) = day.bucket_of(v) else {
            continue;
        };
        if done.contains(&target) || day.favourite(d).is_some_and(|(f, _)| f == target) {
            continue;
        }
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: target,
            yes: true,
            band: (0.05, 0.35),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(target);
        }
    }
}

/// L15. F's slot is the season's; the forecast knows the day's own peak
/// hour (early on frontal days, late on warm-advection days).
fn own_peak(
    day: &LabDay<'_>,
    rule: &LabRule,
    after_min: i64,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(fc) = day.forecast else { return };
    let Some(peak) = forecast_peak(fc) else {
        return;
    };
    let from = day.minute(peak + Duration::minutes(after_min));
    if peak + Duration::minutes(after_min) >= day.day_end() {
        return;
    }
    for (k, d) in day.decisions.iter().enumerate() {
        if d.f.local_minute_now < from.max(12 * 60) || d.f.drop_tenths < 10 {
            continue;
        }
        if fc.rise_tenths(d.at).is_none_or(|r| r > -10) {
            continue;
        }
        let Some(h) = day.bucket_of(d.f.high_whole) else {
            continue;
        };
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: h,
            yes: true,
            band: (0.70, 0.95),
            p_win: Some((0.96, 0.005)),
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L16. On warm-advection days the forecast peaks in the evening; models
/// trained on the usual afternoon peak, and F, call the high too early.
fn evening_high(
    day: &LabDay<'_>,
    rule: &LabRule,
    min_rise: i32,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(fc) = day.forecast else { return };
    let (Some(ten), Some(five)) = (
        local_to_utc(day.date, 10 * 60, day.tz),
        local_to_utc(day.date, 17 * 60, day.tz),
    ) else {
        return;
    };
    let max_in = |a: DateTime<Utc>, b: DateTime<Utc>| {
        fc.hourly
            .iter()
            .filter(|(t, _)| *t >= a && *t < b)
            .map(|(_, v)| *v)
            .max()
    };
    let (Some(day_max), Some(evening_max)) = (
        max_in(ten, five),
        max_in(five, day.day_end() + Duration::seconds(1)),
    ) else {
        return;
    };
    if evening_max < day_max + 5 {
        return;
    }
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 14 * 60, 18 * 60)
            || d.f.current_tenths < d.f.high_whole * 10 - 10
            || fc.rise_tenths(d.at).is_none_or(|r| r < min_rise)
        {
            continue;
        }
        let Some(j) = day.next_bucket(d.f.high_whole) else {
            continue;
        };
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: j,
            yes: true,
            band: (0.05, 0.30),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L17. Forecast errors persist from one day to the next (the same air
/// mass, the same grid-cell bias); the ladder opens on the raw forecast.
fn yesterday_error(
    day: &LabDay<'_>,
    rule: &LabRule,
    rho: f64,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let (Some(fc), Some(e)) = (day.forecast, day.yesterday_error_tenths) else {
        return;
    };
    let Some(fmax) = fc.day_max_tenths() else {
        return;
    };
    let raw = day.bucket_of(round_half_up(f64::from(fmax) / 10.0));
    let mut done: HashSet<usize> = HashSet::new();
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 7 * 60, 10 * 60) || d.knowledge < fc.known_at {
            continue;
        }
        let adjusted = (f64::from(fmax) + rho * f64::from(e)) / 10.0;
        let Some(target) = day.bucket_of(round_half_up(adjusted).max(d.f.high_whole)) else {
            continue;
        };
        if Some(target) == raw || done.contains(&target) {
            continue;
        }
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: target,
            yes: true,
            band: (0.05, 0.30),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(target);
        }
    }
}

// ----------------------------------------------------------------- tape ---

/// L18. $3,372 was taken before reports by traders who knew first; a burst
/// of their flow is a signal, KNMI says whether it is the right one.
fn burst_follow(
    day: &LabDay<'_>,
    rule: &LabRule,
    use_knmi: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    // Both rules on KNMI days only, so the control compares like with like.
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for k in 1..day.decisions.len() {
        let (prev, next) = (&day.decisions[k - 1], &day.decisions[k]);
        if !ctx
            .routine
            .contains(&u8::try_from(next.at.minute()).unwrap_or(u8::MAX))
        {
            continue;
        }
        let high = prev.f.high_whole;
        let Some(h) = day.high_bucket(high) else {
            continue;
        };
        if done.contains(&h) {
            continue;
        }
        let (from, to) = (next.at - Duration::minutes(6), next.knowledge);
        let inside = |t: &&&MarketTrade| t.at >= from && t.at < to;
        let mut events: Vec<&MarketTrade> = day
            .trades(h)
            .iter()
            .filter(inside)
            .filter(|t| !t.taker_buys_yes)
            .copied()
            .collect();
        if let Some(j) = day.next_bucket(high) {
            events.extend(
                day.trades(j)
                    .iter()
                    .filter(inside)
                    .filter(|t| t.taker_buys_yes)
                    .copied(),
            );
        }
        events.sort_by_key(|t| t.at);
        for e in &events {
            let span: Vec<&&MarketTrade> = events
                .iter()
                .filter(|t| t.at >= e.at - Duration::minutes(3) && t.at <= e.at)
                .collect();
            let shares: f64 = span.iter().map(|t| t.shares).sum();
            let takers: HashSet<&str> = span.iter().filter_map(|t| t.taker.as_deref()).collect();
            if shares < 40.0 || takers.len() < 2 {
                continue;
            }
            if use_knmi {
                let Some(r) = latest_known(rs, e.at, ctx.knmi_delay()) else {
                    continue;
                };
                if r.interval_end <= prev.at
                    || r.mean.is_none_or(|m| m.tenths() < high * 10 + 5 + 3)
                {
                    continue;
                }
            }
            let at = e.at + Duration::seconds(10);
            let t = Take {
                shown: e.at,
                at,
                until: to,
                bucket: h,
                yes: false,
                band: (0.05, 0.85),
                p_win: None,
                fresh: Duration::zero(),
            };
            if take(day, rule, ctx, t, out) {
                done.insert(h);
                break;
            }
        }
    }
}

/// All of a day's trades (dust excluded), oldest first.
fn day_tape<'a>(day: &LabDay<'a>) -> Vec<&'a MarketTrade> {
    let mut all: Vec<&MarketTrade> = day.per_bucket.iter().flatten().copied().collect();
    all.sort_by_key(|t| t.at);
    all
}

/// L19. Skill on Polymarket is concentrated and persistent (a ~3% minority
/// moves prices right): follow the takers whose record on the days before
/// shows it, at our latency, not theirs.
fn skill_follow(
    day: &LabDay<'_>,
    rule: &LabRule,
    min_t: f64,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let mut done: HashSet<(usize, bool)> = HashSet::new();
    for t in day_tape(day) {
        let Some(w) = t.taker.as_deref() else {
            continue;
        };
        if !day.wallets.skilled(w, min_t) {
            continue;
        }
        let yes = t.taker_buys_yes;
        let theirs = if yes { t.yes_price } else { 1.0 - t.yes_price };
        if !(0.05..=0.90).contains(&theirs) || done.contains(&(t.bucket, yes)) {
            continue;
        }
        let at = t.at + Duration::seconds(30);
        let follow = Take {
            shown: t.at,
            at,
            until: (t.at + Duration::minutes(10)).min(day.day_end()),
            bucket: t.bucket,
            yes,
            band: (0.01, theirs + 0.03),
            p_win: None,
            fresh: Duration::zero(),
        };
        if take(day, rule, ctx, follow, out) {
            done.insert((t.bucket, yes));
        }
    }
}

/// L20. Longshots are overpriced (0.00–0.02 won 0.1% at 0.4¢) and the
/// takers who keep buying them lose: their price is a good place to sell.
fn longshot_fade(
    day: &LabDay<'_>,
    rule: &LabRule,
    maker: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let mut done: HashSet<usize> = HashSet::new();
    for t in day_tape(day) {
        let Some(w) = t.taker.as_deref() else {
            continue;
        };
        if !t.taker_buys_yes
            || !(0.02..=0.15).contains(&t.yes_price)
            || done.contains(&t.bucket)
            || !day.wallets.losing(w)
        {
            continue;
        }
        let at = t.at + Duration::seconds(30);
        let filled = if maker {
            let offer = (t.yes_price - tick(t.yes_price)).max(0.01);
            let until = (t.at + Duration::minutes(30)).min(day.day_end());
            rest_no(day, rule, ctx, t.at, t.bucket, offer, at, until, out)
        } else {
            let fade = Take {
                shown: t.at,
                at,
                until: (t.at + Duration::minutes(10)).min(day.day_end()),
                bucket: t.bucket,
                yes: false,
                band: (0.80, 0.98),
                p_win: None,
                fresh: Duration::zero(),
            };
            take(day, rule, ctx, fade, out)
        };
        if filled {
            done.insert(t.bucket);
        }
    }
}

/// L21. In-play markets overreact to big surprises and the overreaction
/// fades within minutes (Choi & Hui 2014); a new high is such a surprise
/// for the buckets above it.
fn overreaction(
    day: &LabDay<'_>,
    rule: &LabRule,
    two_above: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(rs) = day.knmi else { return };
    let mut done: HashSet<usize> = HashSet::new();
    for k in 1..day.decisions.len() {
        let (prev, d) = (&day.decisions[k - 1], &day.decisions[k]);
        let new = d.f.high_whole;
        if new <= prev.f.high_whole {
            continue;
        }
        let target = if two_above {
            day.bucket_of(new + 2)
                .filter(|j| !day.buckets[*j].contains(new + 1) && !day.buckets[*j].contains(new))
        } else {
            day.next_bucket(new)
        };
        let Some(j) = target else { continue };
        if done.contains(&j) {
            continue;
        }
        let start = d.knowledge + Duration::minutes(2);
        let end = (d.knowledge + Duration::minutes(15)).min(day.until(k));
        let min_jump = if two_above { 0.08 } else { 0.15 };
        let jumped = day
            .trades(j)
            .iter()
            .any(|t| t.taker_buys_yes && t.at >= d.at && t.at <= start && t.yes_price >= min_jump);
        if !jumped {
            continue;
        }
        let Some(r) = latest_known(rs, start, ctx.knmi_delay()) else {
            continue;
        };
        if r.mean.is_none_or(|m| m.tenths() > new * 10 + 5 + 3) {
            continue;
        }
        let t = Take {
            shown: d.at,
            at: start,
            until: end,
            bucket: j,
            yes: false,
            band: if two_above {
                (0.60, 0.92)
            } else {
                (0.50, 0.85)
            },
            p_win: None,
            fresh: Duration::minutes(2),
        };
        if take(day, rule, ctx, t, out) {
            done.insert(j);
        }
    }
}

/// L22. Resting orders were paid overnight and at 0–2¢ (+0.74¢ and +0.34¢
/// a share); J quoted the middle and lost. Quote only the far tails of the
/// market's own favourite.
fn overnight_tails(
    day: &LabDay<'_>,
    rule: &LabRule,
    min_away: usize,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let mut order: Vec<usize> = (0..day.buckets.len()).collect();
    order.sort_by_key(|i| day.buckets[*i].sort_key());
    let mut done: HashSet<usize> = HashSet::new();
    for d in day.decisions {
        if d.f.local_minute_now >= 9 * 60 {
            break;
        }
        let Some((fav, p)) = day.favourite(d) else {
            continue;
        };
        let Some(pf) = order.iter().position(|i| *i == fav) else {
            continue;
        };
        if p < 0.20 {
            continue;
        }
        let Some(until) = ctx.expiry(d.knowledge, 5) else {
            continue;
        };
        for (pos, &j) in order.iter().enumerate() {
            if pos.abs_diff(pf) < min_away
                || done.contains(&j)
                || day.buckets[j].upper.is_some_and(|u| u < d.f.high_whole)
            {
                continue;
            }
            let Some(offer) = offer_under_ask(day, ctx, j, d.knowledge, (0.01, 0.05)) else {
                continue;
            };
            if rest_no(day, rule, ctx, d.at, j, offer, d.knowledge, until, out) {
                done.insert(j);
            }
        }
    }
}

/// L23. After a shower the sky clears and the sun heats the wet ground
/// again: the market, having priced the cap, misses the rebound.
fn shower_recovery(
    day: &LabDay<'_>,
    rule: &LabRule,
    knmi_slope: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    for (k, d) in day.decisions.iter().enumerate() {
        if !in_window(d.f.local_minute_now, 12 * 60, 16 * 60 + 30)
            || d.f.current_tenths < d.f.high_whole * 10 - 15
        {
            continue;
        }
        let Some(now) = day.wx(k) else { continue };
        if now.wx.body.precipitation() || !now.wx.clear(4000) {
            continue;
        }
        let rained = (0..k).any(|j| {
            day.decisions[j].at >= d.at - Duration::hours(3)
                && day.wx(j).is_some_and(|x| x.wx.body.precipitation())
        });
        if !rained {
            continue;
        }
        let warming = if knmi_slope {
            let Some(rs) = day.knmi else { return };
            let Some(r) = latest_known(rs, d.knowledge, ctx.knmi_delay()) else {
                continue;
            };
            match (
                r.mean.map(|m| m.tenths()),
                mean_at(rs, r.interval_end - Duration::minutes(30)),
            ) {
                (Some(a), Some(b)) => a - b >= 5,
                _ => false,
            }
        } else {
            d.f.slope_c_per_hour.is_some_and(|s| s >= 1.0)
        };
        if !warming {
            continue;
        }
        let Some(j) = day.next_bucket(d.f.high_whole) else {
            continue;
        };
        let t = Take {
            shown: d.at,
            at: d.knowledge,
            until: day.until(k),
            bucket: j,
            yes: true,
            band: (0.05, 0.35),
            p_win: None,
            fresh: ctx.fresh(),
        };
        if take(day, rule, ctx, t, out) {
            return;
        }
    }
}

/// L24. Air arrives from upwind: a station 30–40 km upwind shows Schiphol's
/// next hour (forecasters' "upstream conditions"), ten minutes at a time.
fn upwind(day: &LabDay<'_>, rule: &LabRule, warm: bool, ctx: &Ctx<'_>, out: &mut Vec<SimTrade>) {
    let Some(rs) = day.knmi else { return };
    let param = ctx.sim.lab.neighbour_parameter.as_str();
    let mut done: HashSet<usize> = HashSet::new();
    for (_, bearing, series) in day.neighbours {
        for p in *series {
            let Some(ta) = p.get(param) else { continue };
            let known = p.interval_end + ctx.knmi_delay();
            let minute = day.minute(known);
            if !in_window(minute, if warm { 10 * 60 } else { 12 * 60 }, 18 * 60) {
                continue;
            }
            let Some(k) = day.decision_at(known) else {
                continue;
            };
            let d = &day.decisions[k];
            let Some(w) = day.wx(k).and_then(|x| x.wx.wind()) else {
                continue;
            };
            let Some(dir) = w.direction else { continue };
            if w.speed_kt < 6 || angle_between(f64::from(dir), *bearing) > 40.0 {
                continue;
            }
            let Some(here) = mean_at(rs, p.interval_end) else {
                continue;
            };
            let delta = (ta * 10.0).round() as i32 - here;
            let high = d.f.high_whole;
            let edge = high * 10 + 5;
            let t = if warm {
                if delta < 8 || here < edge - 6 || p.interval_end <= d.at {
                    continue;
                }
                let Some(h) = day.high_bucket(high) else {
                    continue;
                };
                Take {
                    shown: p.interval_end,
                    at: known,
                    until: day.until(k).min(known + Duration::minutes(10)),
                    bucket: h,
                    yes: false,
                    band: (0.05, 0.75),
                    p_win: Some((0.80, 0.02)),
                    fresh: Duration::minutes(1),
                }
            } else {
                if delta > -10 || here > edge - 5 {
                    continue;
                }
                let Some(j) = day.next_bucket(high) else {
                    continue;
                };
                Take {
                    shown: p.interval_end,
                    at: known,
                    until: day.until(k).min(known + Duration::minutes(10)),
                    bucket: j,
                    yes: false,
                    band: (0.65, 0.96),
                    p_win: Some((0.93, 0.005)),
                    fresh: ctx.fresh(),
                }
            };
            if done.contains(&t.bucket) {
                continue;
            }
            let b = t.bucket;
            if take(day, rule, ctx, t, out) {
                done.insert(b);
            }
        }
    }
}

/// L25. Solar nowcasting watches the clear-sky index; the temperature
/// follows the sun with a lag, so a collapse of radiation caps the
/// afternoon before the thermometer shows it.
fn radiation(
    day: &LabDay<'_>,
    rule: &LabRule,
    collapse: bool,
    ctx: &Ctx<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(points) = day.radiation else { return };
    let lab = &ctx.sim.lab;
    let index: Vec<(DateTime<Utc>, f64)> = points
        .iter()
        .filter_map(|p| {
            let v = p.get(&lab.radiation_parameter)?;
            let cs = clear_sky_ghi(
                p.interval_end - Duration::minutes(5),
                lab.latitude,
                lab.longitude,
            );
            (cs >= 150.0).then_some((p.interval_end, v / cs))
        })
        .collect();
    let mut done: HashSet<usize> = HashSet::new();
    for (end, _) in &index {
        let known = *end + ctx.knmi_delay();
        if !in_window(day.minute(known), 10 * 60 + 30, 15 * 60 + 30) {
            continue;
        }
        let span = |a: i64, b: i64| -> Vec<f64> {
            index
                .iter()
                .filter(|(t, _)| {
                    *t > *end - Duration::minutes(a) && *t <= *end - Duration::minutes(b)
                })
                .map(|(_, k)| *k)
                .collect()
        };
        let (last, before) = (span(30, 0), span(90, 30));
        if last.len() < 3 || before.len() < 5 {
            continue;
        }
        let (now, then) = (mean(&last), mean(&before));
        let Some(k) = day.decision_at(known) else {
            continue;
        };
        let high = day.decisions[k].f.high_whole;
        let edge = high * 10 + 5;
        let ta = day.knmi.and_then(|rs| mean_at(rs, *end));
        let Some(j) = day.next_bucket(high) else {
            continue;
        };
        if done.contains(&j) {
            continue;
        }
        let t = if collapse {
            if now > 0.35 || then < 0.70 || ta.is_some_and(|t| t > edge - 3) {
                continue;
            }
            Take {
                shown: *end,
                at: known,
                until: day.until(k).min(known + Duration::minutes(10)),
                bucket: j,
                yes: false,
                band: (0.60, 0.95),
                p_win: Some((0.93, 0.005)),
                fresh: ctx.fresh(),
            }
        } else {
            if now < 0.80 || then > 0.40 || ta.is_none_or(|t| t < edge - 8) {
                continue;
            }
            Take {
                shown: *end,
                at: known,
                until: day.until(k).min(known + Duration::minutes(10)),
                bucket: j,
                yes: true,
                band: (0.05, 0.35),
                p_win: None,
                fresh: ctx.fresh(),
            }
        };
        if take(day, rule, ctx, t, out) {
            done.insert(j);
        }
    }
}

// ------------------------------------------------------------- report ---

/// Why a rule could not be replayed (`None`: its inputs were there).
fn missing(n: Needs, cov: &LabCoverage) -> Option<&'static str> {
    if n.knmi && cov.knmi_days == 0 {
        return Some("no KNMI ten-minute readings (set WM_KNMI_API_KEY for `research market`)");
    }
    if n.forecast && cov.forecast_days == 0 {
        return Some("no day-1 forecast history (`[forecast]` on; `research market` reads it)");
    }
    if n.forecast_pair && cov.forecast_pairs == 0 {
        return Some("no day with a forecast for the day before");
    }
    if n.radiation && cov.radiation_days == 0 {
        return Some("no KNMI radiation readings (downloaded with the KNMI key)");
    }
    if n.neighbours && cov.neighbour_days == 0 {
        return Some(
            "no readings of the neighbouring KNMI stations (downloaded with the KNMI key)",
        );
    }
    None
}

/// Rows of every rule, in order.
pub(crate) fn lab_rows(trades: &[SimTrade], iterations: usize, seed: u64) -> Vec<StrategyRow> {
    rules()
        .iter()
        .map(|r| row(trades, r.key(), false, iterations, seed))
        .collect()
}

fn find<'a>(rows: &'a [StrategyRow], r: &LabRule) -> Option<&'a StrategyRow> {
    rows.iter()
        .find(|x| x.structure == STRUCTURE && x.strategy == r.label && x.range == r.range())
}

/// One line per family: its main rule, then the variant or control.
pub(crate) fn verdict(
    rows: &[StrategyRow],
    cov: &LabCoverage,
    sim: &MarketSimConfig,
) -> Vec<String> {
    let rules = rules();
    let mut v = Vec::new();
    for family in 1..=FAMILIES {
        let fam: Vec<&LabRule> = rules.iter().filter(|r| r.family == family).collect();
        let Some(first) = fam.first() else { continue };
        let result = |r: &LabRule| -> String {
            if let Some(why) = missing(r.needs(), cov) {
                return format!("*{}* not replayed: {why}", r.label);
            }
            match find(rows, r) {
                Some(x) if x.trades > 0 => format!(
                    "*{}* {} trades, {} won, ${:+.2} (${:+.2} a trade, 95% CI {:+.2} … {:+.2})",
                    r.label, x.trades, x.wins, x.total_usd, x.pnl_per_trade, x.ci_low, x.ci_high
                ),
                _ => format!("*{}* no trade", r.label),
            }
        };
        let stake = if matches!(first.kind, Kind::FExit { .. }) {
            format!("F's {:.0} shares a trade", sim.f.shares)
        } else {
            format!("${:.0} a trade", sim.lab.stake_usd)
        };
        v.push(format!(
            "L{family} {} ({}; {stake}): {}.",
            first.name(),
            first.describe(),
            fam.iter().map(|r| result(r)).collect::<Vec<_>>().join("; ")
        ));
    }
    v
}

/// Per family: the rule with the best P&L on the first half of the
/// replayed market days (the main rule on a tie), judged on the second;
/// the main rule's second half too when another was chosen. The last line
/// names the families whose chosen rule held up (later-days interval above
/// zero).
pub(crate) fn out_of_sample(
    trades: &[SimTrade],
    replayed: &[NaiveDate],
    cov: &LabCoverage,
    iterations: usize,
    seed: u64,
) -> Vec<String> {
    let mut dates = replayed.to_vec();
    dates.sort_unstable();
    dates.dedup();
    if dates.is_empty() {
        return Vec::new();
    }
    if dates.len() < MIN_SPLIT_DAYS {
        return vec![format!(
            "Strategy lab out of sample: {} replayed market days are too few to choose a rule on one half and test it on the other (at least {MIN_SPLIT_DAYS} needed).",
            dates.len()
        )];
    }
    let before = dates.len() / 2;
    let split = dates[before];
    let rules = rules();
    let half = |label: &str, later: bool| -> Vec<SimTrade> {
        trades
            .iter()
            .filter(|t| {
                t.structure == STRUCTURE && t.strategy == label && (t.date >= split) == later
            })
            .cloned()
            .collect()
    };
    let total = |ts: &[SimTrade]| ts.iter().map(|t| t.pnl_usd).sum::<f64>();
    let mut v = Vec::new();
    let mut held: Vec<String> = Vec::new();
    for family in 1..=FAMILIES {
        let fam: Vec<&LabRule> = rules
            .iter()
            .filter(|r| r.family == family && missing(r.needs(), cov).is_none())
            .collect();
        if fam.is_empty() {
            continue;
        }
        let mut best: Option<(&LabRule, Vec<SimTrade>)> = None;
        for r in &fam {
            let first = half(&r.label, false);
            if first.is_empty() {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, b)| total(&first) > total(b) + 1e-9)
            {
                best = Some((r, first));
            }
        }
        let Some((best, first)) = best else {
            v.push(format!(
                "L{family} out of sample: no L{family} rule traded on the first {before} market days."
            ));
            continue;
        };
        let r = row(
            &half(&best.label, true),
            best.key(),
            false,
            iterations,
            seed,
        );
        let main = rules.iter().find(|r| r.family == family);
        let configured = match main {
            Some(c) if c.label != best.label => {
                let later = half(&c.label, true);
                if later.is_empty() {
                    format!(
                        " The main rule *{}* made no trade on those later days.",
                        c.label
                    )
                } else {
                    let c = row(&later, c.key(), false, iterations, seed);
                    format!(
                        " The main rule *{}* made {} trades there, {} won, ${:+.2} (${:+.2} a trade, 95% CI {:+.2} … {:+.2}).",
                        c.strategy,
                        c.trades,
                        c.wins,
                        c.total_usd,
                        c.pnl_per_trade,
                        c.ci_low,
                        c.ci_high
                    )
                }
            }
            _ => String::new(),
        };
        if r.trades >= MIN_HELD_TRADES && r.days >= MIN_HELD_DAYS && r.ci_low > 0.0 {
            held.push(format!("{} (${:+.2})", best.label, r.total_usd));
        }
        v.push(format!(
            "L{family} out of sample: on the first {before} market days ({} → {}) the best rule was *{}* ({} trades, {} won, ${:+.2}); on the {} later days ({} → {}) it made {} trades, {} won, ${:+.2} (${:+.2} a trade, 95% CI {:+.2} … {:+.2}).{configured}",
            dates[0],
            dates[before - 1],
            best.label,
            first.len(),
            first.iter().filter(|t| t.won).count(),
            total(&first),
            dates.len() - before,
            split,
            dates[dates.len() - 1],
            r.trades,
            r.wins,
            r.total_usd,
            r.pnl_per_trade,
            r.ci_low,
            r.ci_high
        ));
    }
    v.push(if held.is_empty() {
        format!("Strategy lab: no family's chosen rule held up on the later days (95% CI of the P&L a trade above zero, at least {MIN_HELD_TRADES} trades on {MIN_HELD_DAYS} days); with 25 families tried, treat any first-half winner as chance until one does.")
    } else {
        format!(
            "Strategy lab: held up on the later days (95% CI of the P&L a trade above zero, at least {MIN_HELD_TRADES} trades on {MIN_HELD_DAYS} days): {}. With 25 families tried, confirm on the next run's new days before taking any live.",
            held.join(", ")
        )
    });
    v
}

/// The lab's section of the market report.
pub(crate) fn markdown(
    rows: &[StrategyRow],
    sim: &MarketSimConfig,
    cov: &LabCoverage,
    verdict: &[String],
    out_of_sample: &[String],
) -> String {
    let mut s = format!(
        "\n## Strategy lab: L1–L25 at traded prices\n\nTwenty-five new strategies, each with a variant or a control, replayed like G–K (decisions from local midnight at the knowledge time; takers at the latest trade of their kind ≤ {}′ old or the next acceptable one, plus {:.3} slippage and the taker fee; makers filled only through their price, no fee, {:.0}% of it as rebate; KNMI readings known {}′ after their interval). ${:.0} a trade (L3: F's {:.0} shares). None trades live. The reasoning and sources of each: `docs/research/strategy-lab.md`. Fifty-one rules are many tries: judge a family by its later-days line, not by the best row.\n\nInputs: {} market days; KNMI readings on {}, the day-1 forecast on {} ({} with the day before's), KNMI radiation on {}, neighbouring stations on {}; {} reports' weather groups read; {} F trades on KNMI days; {} takers scored ({} skilled, {} losing at the end).\n\n",
        sim.gk.fresh_quote_minutes,
        sim.slippage,
        100.0 * sim.maker.rebate_share,
        sim.gk.knmi_delay_minutes,
        sim.lab.stake_usd,
        sim.f.shares,
        cov.days,
        cov.knmi_days,
        cov.forecast_days,
        cov.forecast_pairs,
        cov.radiation_days,
        cov.neighbour_days,
        cov.weather_reports,
        cov.f_trades,
        cov.wallets,
        cov.skilled,
        cov.unskilled,
    );
    for line in verdict.iter().chain(out_of_sample) {
        let _ = writeln!(s, "* {line}");
    }
    s.push_str("\n| rule | prices | trades | won | days | mean price | P&L per trade | 95% CI | total |\n|---|---|---:|---:|---:|---:|---:|---|---:|\n");
    for rule in rules() {
        if missing(rule.needs(), cov).is_some() {
            continue;
        }
        let Some(r) = find(rows, &rule) else { continue };
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {:.3} | {:+.2} | [{:+.2}, {:+.2}] | {:+.2} |",
            rule.label,
            r.range,
            r.trades,
            r.wins,
            r.days,
            r.mean_price,
            r.pnl_per_trade,
            r.ci_low,
            r.ci_high,
            r.total_usd
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_sim::{Flow, Quote};
    use wm_core::ids::{ProviderId, StationId};
    use wm_core::market::TempUnit;
    use wm_core::time::Season;
    use wm_core::units::TempC;
    use wm_strategy::{PeakFeatures, PeakTimesBuilder};

    const TZ: Tz = chrono_tz::Europe::Amsterdam;
    const FEE: f64 = 0.05;
    const SLIP: f64 = 0.005;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// `HH:MM` of 15 July 2026, UTC.
    fn z(hm: &str) -> DateTime<Utc> {
        utc(&format!("2026-07-15T{hm}:00Z"))
    }

    fn buckets() -> Vec<TemperatureBucket> {
        (17..=24)
            .map(|v| TemperatureBucket::exact(v, TempUnit::Celsius))
            .collect()
    }

    fn labels() -> Vec<String> {
        (17..=24).map(|v| format!("{v}°C")).collect()
    }

    /// Bucket index of a degree (17 °C is 0).
    fn idx(v: i32) -> usize {
        usize::try_from(v - 17).unwrap()
    }

    fn fee(p: f64) -> f64 {
        FEE * p * (1.0 - p)
    }

    fn features(at: DateTime<Utc>, high: i32, drop: i32) -> PeakFeatures {
        let local = local_minute_of_day(at, TZ);
        PeakFeatures {
            station: StationId::new("EHAM").unwrap(),
            date: date(),
            view: wm_strategy::ViewKind::All,
            high_tenths: high * 10,
            high_whole: high,
            high_at: at,
            current_tenths: high * 10 - drop,
            drop_tenths: drop,
            minutes_since_high: 0,
            minutes_since_first_high: 0,
            lower_obs_since_high: 0,
            retests: 0,
            slope_c_per_hour: None,
            accel_c_per_hour2: None,
            trajectory: wm_strategy::TrajectoryClass::AtHigh,
            local_minute_now: local,
            high_local_minute: local,
            minutes_after_solar_noon: 0,
            month: 7,
            season: Season::Summer,
            observation_count: 20,
            data_age_minutes: 3,
            forecast_rise_tenths: None,
            forecast_headroom_tenths: None,
            high_jump_tenths: None,
        }
    }

    /// A report at `hm` (UTC), known three minutes later.
    fn decision(hm: &str, high: i32, drop: i32) -> Decision {
        let at = z(hm);
        Decision {
            at,
            knowledge: at + Duration::minutes(3),
            f: features(at, high, drop),
            dists: [None, None],
            quotes: vec![Quote::default(); 8],
            flows: vec![Flow::default(); 8],
        }
    }

    fn trade(hm: &str, bucket: usize, price: f64, buy: bool) -> MarketTrade {
        MarketTrade {
            at: z(hm),
            bucket,
            yes_price: price,
            taker_buys_yes: buy,
            shares: 50.0,
            taker: None,
        }
    }

    fn by(
        hms: &str,
        bucket: usize,
        price: f64,
        buy: bool,
        shares: f64,
        taker: &str,
    ) -> MarketTrade {
        MarketTrade {
            at: utc(&format!("2026-07-15T{hms}Z")),
            bucket,
            yes_price: price,
            taker_buys_yes: buy,
            shares,
            taker: Some(taker.to_owned()),
        }
    }

    fn reading(hm: &str, mean: i32, max: i32) -> TenMinuteObservation {
        TenMinuteObservation {
            station: StationId::new("EHAM").unwrap(),
            provider: ProviderId::knmi(),
            interval_end: z(hm),
            mean: Some(TempC::from_tenths(mean)),
            max: Some(TempC::from_tenths(max)),
            radiation: None,
            received_at: z(hm) + Duration::minutes(5),
        }
    }

    fn wx(raw: &str, dew: i32) -> Option<ReportWx> {
        Some(ReportWx {
            wx: parse_wx(raw),
            dew_tenths: Some(dew),
        })
    }

    fn point(at: DateTime<Utc>, name: &str, v: f64) -> SeriesPoint {
        SeriesPoint {
            interval_end: at,
            values: [(name.to_owned(), v)].into_iter().collect(),
        }
    }

    /// The forecast `f(hours after 14 July 22:00 UTC)` for 15 July, hourly.
    fn forecast(f: impl Fn(i64) -> i32) -> ForecastDay {
        ForecastDay {
            date: date(),
            known_at: utc("2026-07-14T06:00:00Z"),
            hourly: (0..=24)
                .map(|h| (utc("2026-07-14T22:00:00Z") + Duration::hours(h), f(h)))
                .collect(),
        }
    }

    #[derive(Default)]
    struct Fx {
        decisions: Vec<Decision>,
        wx: Vec<Option<ReportWx>>,
        trades: Vec<MarketTrade>,
        winner: usize,
        knmi: Option<Vec<TenMinuteObservation>>,
        radiation: Option<Vec<SeriesPoint>>,
        neighbours: Vec<(Neighbour, Vec<SeriesPoint>)>,
        forecast: Option<ForecastDay>,
        yesterday: Option<i32>,
        wallets: WalletBook,
        f_trades: Vec<SimTrade>,
    }

    impl Fx {
        fn run(&self) -> Vec<SimTrade> {
            let (b, l) = (buckets(), labels());
            let mut per: Vec<Vec<&MarketTrade>> = vec![Vec::new(); b.len()];
            for t in &self.trades {
                per[t.bucket].push(t);
            }
            for v in &mut per {
                v.sort_by_key(|t| t.at);
            }
            let peak = PeakTimesBuilder::new().build();
            let sim = MarketSimConfig::default();
            let mut wx = self.wx.clone();
            wx.resize(self.decisions.len(), None);
            let neighbours: Vec<(&Neighbour, f64, &[SeriesPoint])> = self
                .neighbours
                .iter()
                .map(|(n, s)| (n, sim.lab.bearing_to(n), s.as_slice()))
                .collect();
            let day = LabDay {
                date: date(),
                buckets: &b,
                labels: &l,
                winner: self.winner,
                decisions: &self.decisions,
                wx: &wx,
                per_bucket: &per,
                peak: &peak,
                knmi: self.knmi.as_deref(),
                radiation: self.radiation.as_deref(),
                neighbours: &neighbours,
                forecast: self.forecast.as_ref(),
                yesterday_error_tenths: self.yesterday,
                wallets: &self.wallets,
                f_trades: &self.f_trades,
                tz: TZ,
            };
            let ctx = Ctx {
                sim: &sim,
                fee_rate: FEE,
                routine: &[25, 55],
                metar_delay: Duration::minutes(3),
            };
            simulate_lab(&day, &ctx)
        }
    }

    fn of<'a>(ts: &'a [SimTrade], label: &str) -> Vec<&'a SimTrade> {
        ts.iter().filter(|t| t.strategy == label).collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// $20 of a taker buy at `p`.
    fn taker_pnl(p: f64, won: bool) -> f64 {
        20.0 / p * (f64::from(u8::from(won)) - p - fee(p) - SLIP)
    }

    /// $20 of a resting buy at `p`.
    fn maker_pnl(p: f64, won: bool) -> f64 {
        20.0 / p * (f64::from(u8::from(won)) - p + 0.25 * fee(p))
    }

    #[test]
    fn the_rules_are_25_families_each_with_a_variant_or_control() {
        let rules = rules();
        assert_eq!(rules.len(), 51);
        for family in 1..=FAMILIES {
            let fam: Vec<&LabRule> = rules.iter().filter(|r| r.family == family).collect();
            assert!(fam.len() >= 2, "L{family}");
            assert_eq!(fam[0].label, format!("L{family}"));
            assert!(
                fam[1..]
                    .iter()
                    .all(|r| r.label.starts_with(&format!("L{family} · ")))
            );
        }
        let labels: HashSet<&str> = rules.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels.len(), rules.len(), "labels are unique");
        assert!(
            rules
                .iter()
                .all(|r| !r.describe().is_empty() && !r.range().is_empty())
        );
        assert_eq!(NAMES.len(), usize::from(FAMILIES));
    }

    #[test]
    fn l1_quotes_the_next_degree_only_under_knmi_s_shield() {
        // 11:55Z report: high 19 (edge 19.5). KNMI 12:00–12:20 at 18.2,
        // 18.0, 17.8: ≥ 0.5 °C under the edge and falling. The 20 °C YES
        // was bought at 0.06 at 12:20; a buyer at 0.07 at 12:30 fills the
        // 0.05 offer (NO at 0.95).
        let mut fx = Fx {
            decisions: vec![decision("11:55", 19, 0)],
            trades: vec![
                trade("12:20", idx(20), 0.06, true),
                trade("12:30", idx(20), 0.07, true),
            ],
            winner: idx(19),
            knmi: Some(vec![
                reading("12:00", 182, 184),
                reading("12:10", 180, 182),
                reading("12:20", 178, 180),
            ]),
            ..Fx::default()
        };
        let out = fx.run();
        let l1 = of(&out, "L1");
        assert_eq!(l1.len(), 1, "{out:?}");
        assert_eq!((l1[0].side.as_str(), l1[0].bucket.as_str()), ("NO", "20°C"));
        assert!(close(l1[0].price, 0.95) && l1[0].won);
        assert_eq!(l1[0].filled.as_deref(), Some("14:30"));
        assert!(close(l1[0].pnl_usd, maker_pnl(0.95, true)));
        assert_eq!(of(&out, "L1 · no shield").len(), 1);
        // KNMI rising towards the edge: L1 stays out; the control buys the
        // NO that the new high kills.
        fx.knmi = Some(vec![
            reading("12:00", 185, 187),
            reading("12:10", 190, 192),
            reading("12:20", 194, 196),
        ]);
        fx.winner = idx(20);
        let out = fx.run();
        assert!(of(&out, "L1").is_empty(), "{out:?}");
        let control = of(&out, "L1 · no shield");
        assert_eq!(control.len(), 1);
        assert!(!control[0].won && close(control[0].pnl_usd, maker_pnl(0.95, false)));
    }

    #[test]
    fn l2_rests_a_no_bid_on_the_doomed_bucket() {
        // 11:25Z: high 19. KNMI 11:40 at 19.8 (edge + 0.3), known 11:45,
        // 15′ before the 11:55 METAR. The 19 °C YES last bought at 0.60;
        // the 0.59 offer fills at 11:50 (NO at 0.41).
        let fx = Fx {
            decisions: vec![decision("11:25", 19, 0)],
            trades: vec![
                trade("11:44", idx(19), 0.60, true),
                trade("11:50", idx(19), 0.62, true),
            ],
            winner: idx(20),
            knmi: Some(vec![reading("11:40", 198, 200)]),
            ..Fx::default()
        };
        let out = fx.run();
        let l2 = of(&out, "L2");
        assert_eq!(l2.len(), 1, "{out:?}");
        assert_eq!((l2[0].side.as_str(), l2[0].bucket.as_str()), ("NO", "19°C"));
        assert!(close(l2[0].price, 0.41) && l2[0].won);
        assert!(close(l2[0].pnl_usd, maker_pnl(0.41, true)));
        // Edge + 0.6 needed by the variant: not reached.
        assert!(of(&out, "L2 · margin 0.6").is_empty());
    }

    fn f_trade() -> SimTrade {
        SimTrade {
            date: date(),
            report: "15:25".into(),
            structure: "–".into(),
            strategy: "F".into(),
            window: 0,
            range: "0.90–0.97".into(),
            bucket: "19°C".into(),
            side: "YES".into(),
            price: 0.93,
            p_model: 0.95,
            p_used: 0.95,
            won: false,
            pnl_usd: 100.0 * (0.0 - 0.93 - fee(0.93) - SLIP),
            resolved: "20°C".into(),
            filled: None,
        }
    }

    #[test]
    fn l3_sells_f_s_yes_before_the_metar_that_kills_it() {
        // F bought 19 °C YES at 0.93 on the 13:25Z (15:25 local) report.
        // KNMI 13:40 at 19.9 (edge + 0.4), known 13:45: sold to a taker at
        // 0.80 at 13:46. The 13:55 METAR reports 20.
        let fx = Fx {
            decisions: vec![decision("13:25", 19, 10), decision("13:55", 20, 0)],
            trades: vec![trade("13:46", idx(19), 0.80, false)],
            winner: idx(20),
            knmi: Some(vec![reading("13:30", 193, 194), reading("13:40", 199, 200)]),
            f_trades: vec![f_trade()],
            ..Fx::default()
        };
        let out = fx.run();
        let cost = 0.93 + fee(0.93) + SLIP;
        let sold = of(&out, "L3");
        assert_eq!(sold.len(), 1, "{out:?}");
        assert_eq!(sold[0].side, "YES, sold");
        assert_eq!(sold[0].report, "15:25");
        assert_eq!(sold[0].filled.as_deref(), Some("15:46"));
        assert!(close(
            sold[0].pnl_usd,
            100.0 * (0.80 - fee(0.80) - SLIP - cost)
        ));
        let held = of(&out, "L3 · hold (F)");
        assert_eq!(held.len(), 1);
        assert!(close(held[0].pnl_usd, 100.0 * (0.0 - cost)));
        assert!(
            close(held[0].pnl_usd, f_trade().pnl_usd),
            "the control is F itself"
        );
        assert!(sold[0].pnl_usd > held[0].pnl_usd);
        assert_eq!(of(&out, "L3 · exit at +0.0").len(), 1);
    }

    #[test]
    fn l4_buys_the_high_s_bucket_when_knmi_shows_the_cooling() {
        // High 21 (edge 21.5); 13:25Z (15:25 local) report 1.2 °C under it.
        // KNMI's maxima 12:10–13:30 ≤ 21.2 and the mean 20.5 → 19.8 in an
        // hour; known 13:35. YES bought at 0.85 at 13:34.
        let knmi: Vec<TenMinuteObservation> = [
            ("12:10", 207),
            ("12:20", 206),
            ("12:30", 205),
            ("12:40", 204),
            ("12:50", 203),
            ("13:00", 201),
            ("13:10", 200),
            ("13:20", 198),
            ("13:30", 197),
        ]
        .iter()
        .map(|(hm, mean)| reading(hm, *mean, 212))
        .collect();
        let mut fx = Fx {
            decisions: vec![decision("13:25", 21, 12)],
            trades: vec![trade("13:34", idx(21), 0.85, true)],
            winner: idx(21),
            knmi: Some(knmi),
            ..Fx::default()
        };
        // 12:30 mean 205, 13:30 mean 197: fell 0.8 °C.
        let rs = fx.knmi.as_ref().unwrap();
        assert_eq!(mean_at(rs, z("12:30")), Some(205));
        assert_eq!(mean_at(rs, z("13:30")), Some(197));
        let out = fx.run();
        let l4 = of(&out, "L4");
        assert_eq!(l4.len(), 1, "{out:?}");
        assert!(close(l4[0].price, 0.85) && l4[0].won);
        assert!(close(l4[0].pnl_usd, taker_pnl(0.85, true)));
        assert!(
            of(&out, "L4 · 0.90–0.98").is_empty(),
            "0.85 is under its band"
        );
        // A tenth over the rounding edge − 0.3 in the last 90′: no lock.
        fx.knmi.as_mut().unwrap()[4].max = Some(TempC::from_tenths(213));
        assert!(of(&fx.run(), "L4").is_empty());
    }

    #[test]
    fn l5_sells_the_next_degree_late_under_knmi_cooling() {
        // 14:55Z (16:55 local): high 20 (edge 20.5), 2.5 °C under it. KNMI
        // 14:00–15:00 means ≤ 19.5 and falling, maxima ≤ 20.0. A seller at
        // 0.08 (NO 0.92) at 15:03.
        let knmi: Vec<TenMinuteObservation> = [
            "14:00", "14:10", "14:20", "14:30", "14:40", "14:50", "15:00",
        ]
        .iter()
        .enumerate()
        .map(|(k, hm)| reading(hm, 192 - i32::try_from(k).unwrap(), 198))
        .collect();
        let fx = Fx {
            decisions: vec![decision("14:55", 20, 25)],
            trades: vec![trade("15:03", idx(21), 0.08, false)],
            winner: idx(20),
            knmi: Some(knmi),
            ..Fx::default()
        };
        let out = fx.run();
        let l5 = of(&out, "L5");
        assert_eq!(l5.len(), 1, "{out:?}");
        assert_eq!((l5[0].side.as_str(), l5[0].bucket.as_str()), ("NO", "21°C"));
        assert!(close(l5[0].price, 0.92) && l5[0].won);
        assert!(close(l5[0].pnl_usd, taker_pnl(0.92, true)));
    }

    #[test]
    fn l6_buys_the_new_degree_only_late_and_flattening() {
        // 14:25Z (16:25 local): high 21. KNMI 14:40 at 22.3 (edge + 0.8),
        // 0.3 °C over 14:10: K fires late and flat. YES of 22 at 0.55.
        let mut fx = Fx {
            decisions: vec![decision("14:25", 21, 0)],
            trades: vec![trade("14:45", idx(22), 0.55, true)],
            winner: idx(22),
            knmi: Some(vec![reading("14:10", 220, 221), reading("14:40", 223, 225)]),
            ..Fx::default()
        };
        let out = fx.run();
        assert_eq!(of(&out, "L6").len(), 1, "{out:?}");
        assert!(close(of(&out, "L6")[0].pnl_usd, taker_pnl(0.55, true)));
        assert_eq!(of(&out, "L6 · unfiltered").len(), 1);
        // Still climbing fast (1.3 °C in 30′): only the control buys.
        fx.knmi = Some(vec![reading("14:10", 210, 211), reading("14:40", 223, 225)]);
        let out = fx.run();
        assert!(of(&out, "L6").is_empty());
        assert_eq!(of(&out, "L6 · unfiltered").len(), 1);
    }

    #[test]
    fn l7_projects_the_slope_to_the_report() {
        // 11:25Z: high 19 (edge 19.5). KNMI 11:20 at 18.0, 11:40 at 19.2:
        // 0.06 °C a minute, 15′ to the 11:55 report → 20.1 ≥ edge + 0.6,
        // while K (≥ edge + 0.8) does not fire yet.
        let fx = Fx {
            decisions: vec![decision("11:25", 19, 0)],
            trades: vec![trade("11:45", idx(19), 0.40, false)],
            winner: idx(20),
            knmi: Some(vec![reading("11:20", 180, 182), reading("11:40", 192, 194)]),
            ..Fx::default()
        };
        let out = fx.run();
        let l7 = of(&out, "L7");
        assert_eq!(l7.len(), 1, "{out:?}");
        assert_eq!((l7[0].side.as_str(), l7[0].bucket.as_str()), ("NO", "19°C"));
        assert!(close(l7[0].price, 0.60) && l7[0].won);
        assert!(of(&out, "L7 · to +1.0").is_empty(), "20.1 < 20.5");
    }

    fn sea_breeze_day() -> Fx {
        let mut high_report = decision("10:55", 23, 0);
        high_report.f.high_at = z("10:55");
        let mut after = decision("12:25", 23, 12);
        after.f.high_at = z("10:55");
        Fx {
            decisions: vec![decision("06:55", 18, 0), high_report, after],
            wx: vec![
                wx("EHAM 150655Z 12008KT CAVOK 18/11 Q1018 NOSIG", 110),
                wx("EHAM 151055Z 14010KT CAVOK 23/12 Q1017 NOSIG", 120),
                wx("EHAM 151225Z 27012KT 9999 FEW025 22/14 Q1017 NOSIG", 140),
            ],
            trades: vec![
                trade("12:26", idx(23), 0.80, true),
                trade("12:27", idx(24), 0.10, false),
            ],
            winner: idx(23),
            ..Fx::default()
        }
    }

    #[test]
    fn l8_locks_the_high_when_the_sea_breeze_arrives() {
        let out = sea_breeze_day().run();
        let l8 = of(&out, "L8");
        assert_eq!(l8.len(), 1, "{out:?}");
        assert_eq!((l8[0].side.as_str(), l8[0].bucket.as_str()), ("NO", "24°C"));
        assert!(close(l8[0].price, 0.90) && l8[0].won);
        let yes = of(&out, "L8 · YES high");
        assert_eq!(yes.len(), 1);
        assert!(close(yes[0].price, 0.80) && yes[0].won);
        // No offshore morning: no breeze to lock on.
        let mut fx = sea_breeze_day();
        fx.wx[0] = wx("EHAM 150655Z 30008KT CAVOK 18/11 Q1018 NOSIG", 110);
        fx.wx[1] = wx("EHAM 151055Z 31010KT CAVOK 23/12 Q1017 NOSIG", 120);
        assert!(of(&fx.run(), "L8").is_empty());
        // The air did not moisten: no breeze either.
        let mut fx = sea_breeze_day();
        fx.wx[2] = wx("EHAM 151225Z 27012KT 9999 FEW025 22/12 Q1017 NOSIG", 125);
        assert!(of(&fx.run(), "L8").is_empty());
    }

    #[test]
    fn l9_caps_the_day_after_a_thunderstorm_cooled_it() {
        let mut fx = Fx {
            decisions: vec![decision("13:25", 22, 30)],
            wx: vec![wx(
                "EHAM 151325Z 25015G30KT 4000 TSRA BKN015CB 19/17 Q1009 RETSRA NOSIG",
                170,
            )],
            trades: vec![trade("13:26", idx(23), 0.12, false)],
            winner: idx(22),
            ..Fx::default()
        };
        let out = fx.run();
        let l9 = of(&out, "L9");
        assert_eq!(l9.len(), 1, "{out:?}");
        assert!(close(l9[0].price, 0.88) && l9[0].won);
        assert_eq!(of(&out, "L9 · thunder").len(), 1);
        fx.wx = vec![wx(
            "EHAM 151325Z 25015KT 9999 SCT030 19/12 Q1009 NOSIG",
            120,
        )];
        assert!(of(&fx.run(), "L9").is_empty(), "dry: no cap");
    }

    #[test]
    fn l10_reads_the_trend_both_ways() {
        let fx = Fx {
            decisions: vec![decision("11:55", 21, 0), decision("13:25", 21, 12)],
            wx: vec![
                wx(
                    "EHAM 151155Z 18008KT 9999 SCT030 21/14 Q1012 BECMG 30012KT",
                    140,
                ),
                wx("EHAM 151325Z 23005KT CAVOK 20/12 Q1015 NOSIG", 120),
            ],
            trades: vec![
                trade("11:57", idx(22), 0.30, false),
                trade("13:27", idx(21), 0.75, true),
            ],
            winner: idx(21),
            ..Fx::default()
        };
        let out = fx.run();
        let cap = of(&out, "L10");
        assert_eq!(cap.len(), 1, "{out:?}");
        assert_eq!(
            (cap[0].side.as_str(), cap[0].bucket.as_str()),
            ("NO", "22°C")
        );
        assert!(close(cap[0].price, 0.70) && cap[0].won);
        let lock = of(&out, "L10 · NOSIG lock");
        assert_eq!(lock.len(), 1);
        assert_eq!(
            (lock[0].side.as_str(), lock[0].bucket.as_str()),
            ("YES", "21°C")
        );
        assert!(close(lock[0].price, 0.75) && lock[0].won);
    }

    #[test]
    fn l11_fades_the_favourite_on_a_fog_morning() {
        let mut d = decision("07:25", 14, 0);
        d.quotes[idx(22)] = Quote {
            mid: Some(0.45),
            yes_ask: Some(0.47),
            yes_bid: Some(0.43),
        };
        let mut fx = Fx {
            decisions: vec![d],
            wx: vec![wx(
                "EHAM 150725Z 00000KT 0400 FG VV002 14/14 Q1020 BECMG 3000 BR",
                140,
            )],
            trades: vec![trade("07:27", idx(22), 0.45, false)],
            winner: idx(20),
            ..Fx::default()
        };
        let out = fx.run();
        let l11 = of(&out, "L11");
        assert_eq!(l11.len(), 1, "{out:?}");
        assert_eq!(
            (l11[0].side.as_str(), l11[0].bucket.as_str()),
            ("NO", "22°C")
        );
        assert!(close(l11[0].price, 0.55) && l11[0].won);
        assert!(close(l11[0].pnl_usd, taker_pnl(0.55, true)));
        fx.wx = vec![wx("EHAM 150725Z 09005KT CAVOK 14/09 Q1020 NOSIG", 90)];
        assert!(of(&fx.run(), "L11").is_empty(), "sunny: no fade");
    }

    #[test]
    fn l12_buys_the_bucket_above_the_favourite_on_a_clear_dry_morning() {
        let mut d = decision("08:25", 18, 0);
        d.quotes[idx(21)] = Quote {
            mid: Some(0.40),
            yes_ask: Some(0.42),
            yes_bid: Some(0.38),
        };
        let mut fx = Fx {
            decisions: vec![d],
            wx: vec![wx("EHAM 150825Z 10008KT CAVOK 18/06 Q1025 NOSIG", 60)],
            trades: vec![trade("08:27", idx(22), 0.20, true)],
            winner: idx(22),
            ..Fx::default()
        };
        let out = fx.run();
        let l12 = of(&out, "L12");
        assert_eq!(l12.len(), 1, "{out:?}");
        assert_eq!(
            (l12[0].side.as_str(), l12[0].bucket.as_str()),
            ("YES", "22°C")
        );
        assert!(close(l12[0].pnl_usd, taker_pnl(0.20, true)));
        // A sea wind: only the any-wind variant buys.
        fx.wx = vec![wx("EHAM 150825Z 30008KT CAVOK 18/06 Q1025 NOSIG", 60)];
        let out = fx.run();
        assert!(of(&out, "L12").is_empty());
        assert_eq!(of(&out, "L12 · any wind").len(), 1);
    }

    #[test]
    fn l13_locks_an_early_high_after_a_cold_front() {
        let first = decision("06:25", 19, 0);
        let mut later = decision("09:25", 19, 20);
        later.f.high_at = z("06:25");
        later.f.minutes_since_first_high = 180;
        let mut fx = Fx {
            decisions: vec![first, later],
            wx: vec![
                wx("EHAM 150625Z 20012KT 9999 BKN012 19/16 Q1008 NOSIG", 160),
                wx("EHAM 150925Z 29015KT 9999 SCT020 17/10 Q1012 NOSIG", 100),
            ],
            trades: vec![
                trade("09:27", idx(19), 0.70, true),
                trade("09:27", idx(20), 0.20, false),
            ],
            winner: idx(19),
            ..Fx::default()
        };
        let out = fx.run();
        let l13 = of(&out, "L13");
        assert_eq!(l13.len(), 1, "{out:?}");
        assert!(close(l13[0].price, 0.70) && l13[0].won);
        let no = of(&out, "L13 · NO above");
        assert_eq!(no.len(), 1);
        assert!(close(no[0].price, 0.80) && no[0].won);
        // The pressure did not rise: no front.
        fx.wx[1] = wx("EHAM 150925Z 29015KT 9999 SCT020 17/10 Q1008 NOSIG", 100);
        assert!(of(&fx.run(), "L13").is_empty());
    }

    /// 15.0 °C at night, a peak of 20.0 °C at 13:00 UTC.
    fn afternoon_peak(h: i64) -> i32 {
        // h = 0 is 22:00 UTC the day before: 13:00 UTC is h = 15.
        150 + i32::try_from((50 - 5 * (h - 15).abs()).max(0)).unwrap()
    }

    #[test]
    fn l14_carries_the_morning_departure_from_the_hourly_forecast() {
        // 08:25Z (10:25 local): the forecast says 17.7, the station 19.7.
        // 20.0 + 0.7 × 2.0 ≈ 21.4 → the 21 °C bucket, the market favours 20.
        let fc = forecast(afternoon_peak);
        assert!(close(
            forecast_at(&fc, z("08:25")).unwrap(),
            175.0 + 5.0 * 25.0 / 60.0
        ));
        let mut d = decision("08:25", 20, 3);
        d.quotes[idx(20)] = Quote {
            mid: Some(0.45),
            yes_ask: Some(0.47),
            yes_bid: Some(0.43),
        };
        let fx = Fx {
            decisions: vec![d],
            trades: vec![trade("08:27", idx(21), 0.25, true)],
            winner: idx(21),
            forecast: Some(fc),
            ..Fx::default()
        };
        let out = fx.run();
        let l14 = of(&out, "L14");
        assert_eq!(l14.len(), 1, "{out:?}");
        assert_eq!(l14[0].bucket, "21°C");
        assert!(close(l14[0].pnl_usd, taker_pnl(0.25, true)));
        // λ 1: 22.0 → the 22 °C bucket, which did not trade.
        assert!(of(&out, "L14 · λ 1.0").is_empty());
    }

    #[test]
    fn l15_locks_after_the_forecast_s_own_peak() {
        // Peak 13:00Z (15:00 local); 15:25Z is 145′ later, the forecast
        // falling 1.5 °C, the station 1.5 °C under its high.
        let fx = Fx {
            decisions: vec![decision("15:25", 21, 15)],
            trades: vec![trade("15:27", idx(21), 0.88, true)],
            winner: idx(21),
            forecast: Some(forecast(afternoon_peak)),
            ..Fx::default()
        };
        let out = fx.run();
        let l15 = of(&out, "L15");
        assert_eq!(l15.len(), 1, "{out:?}");
        assert!(close(l15[0].price, 0.88) && l15[0].won);
        assert_eq!(of(&out, "L15 · +60′").len(), 1);
    }

    #[test]
    fn l16_buys_the_next_degree_on_an_evening_high_day() {
        // A forecast rising all day to 26.0 at 20:00Z (22:00 local).
        let fc =
            forecast(|h| 150 + 5 * i32::try_from(h.min(22)).unwrap() - if h > 22 { 5 } else { 0 });
        let fx = Fx {
            decisions: vec![decision("13:25", 22, 5)],
            trades: vec![trade("13:27", idx(23), 0.22, true)],
            winner: idx(23),
            forecast: Some(fc),
            ..Fx::default()
        };
        let out = fx.run();
        let l16 = of(&out, "L16");
        assert_eq!(l16.len(), 1, "{out:?}");
        assert_eq!(l16[0].bucket, "23°C");
        assert!(l16[0].won);
        // An afternoon-peak day is no evening-high day.
        let fx = Fx {
            forecast: Some(forecast(afternoon_peak)),
            ..fx
        };
        assert!(of(&fx.run(), "L16").is_empty());
    }

    #[test]
    fn l17_moves_the_ladder_by_yesterday_s_error() {
        // Forecast maximum 20.0; yesterday the station beat its forecast by
        // 2.0 °C: half of it → 21 °C.
        let fx = Fx {
            decisions: vec![decision("06:25", 15, 0)],
            trades: vec![trade("06:27", idx(21), 0.18, true)],
            winner: idx(21),
            forecast: Some(forecast(afternoon_peak)),
            yesterday: Some(20),
            ..Fx::default()
        };
        let out = fx.run();
        let l17 = of(&out, "L17");
        assert_eq!(l17.len(), 1, "{out:?}");
        assert_eq!(l17[0].bucket, "21°C");
        assert!(
            of(&out, "L17 · full error").is_empty(),
            "22 °C did not trade"
        );
        // Without yesterday's error the rule is not replayed at all.
        let fx = Fx {
            yesterday: None,
            ..fx
        };
        assert!(of(&fx.run(), "L17").is_empty());
    }

    fn burst_day(knmi_mean: i32) -> Fx {
        Fx {
            decisions: vec![decision("11:25", 19, 0), decision("11:55", 20, 0)],
            trades: vec![
                by("11:50:00", idx(19), 0.55, false, 25.0, "a"),
                by("11:51:00", idx(20), 0.50, true, 25.0, "b"),
                by("11:52:00", idx(19), 0.40, false, 10.0, "c"),
            ],
            winner: idx(20),
            knmi: Some(vec![reading("11:40", knmi_mean, knmi_mean + 2)]),
            ..Fx::default()
        }
    }

    #[test]
    fn l18_follows_a_pre_report_burst_only_when_knmi_agrees() {
        let out = burst_day(199).run();
        let l18 = of(&out, "L18");
        assert_eq!(l18.len(), 1, "{out:?}");
        assert_eq!(
            (l18[0].side.as_str(), l18[0].bucket.as_str()),
            ("NO", "19°C")
        );
        assert!(close(l18[0].price, 0.60) && l18[0].won);
        assert_eq!(l18[0].filled.as_deref(), Some("13:52"));
        assert_eq!(of(&out, "L18 · no KNMI").len(), 1);
        let out = burst_day(190).run();
        assert!(of(&out, "L18").is_empty(), "KNMI under the edge");
        assert_eq!(of(&out, "L18 · no KNMI").len(), 1);
    }

    fn book(wallet: &str, n: u64, mean: f64, sd: f64) -> WalletBook {
        let mut b = WalletBook::default();
        b.insert(
            wallet,
            wm_strategy::lab::WalletStats::from_moments(n, mean, sd),
        );
        b
    }

    #[test]
    fn wallets_are_scored_on_their_settled_trades() {
        let trades = [
            by("10:00:00", 0, 0.30, true, 10.0, "w"),
            by("10:01:00", 1, 0.30, false, 10.0, "w"),
            by("10:02:00", 1, 0.30, true, 10.0, "v"),
        ];
        let per: Vec<Vec<&MarketTrade>> = vec![vec![&trades[0]], vec![&trades[1], &trades[2]]];
        let mut b = WalletBook::default();
        learn_day(&mut b, &per, 0, FEE);
        let w = b.get("w").unwrap();
        // YES of the winner at 0.30: 0.70 − fee; YES of a loser sold at
        // 0.30 (NO at 0.70): 0.30 − fee.
        assert_eq!(w.n, 2);
        assert!(close(
            w.mean(),
            ((0.70 - fee(0.30)) + (0.30 - fee(0.30))) / 2.0
        ));
        assert!(close(b.get("v").unwrap().mean(), -0.30 - fee(0.30)));
        assert!(!b.skilled("w", 2.0), "two trades are too few");
        let s = book("s", 40, 0.10, 0.05);
        assert!(s.get("s").unwrap().t() > 10.0 && s.skilled("s", 3.0));
        let l = book("l", 40, -0.10, 0.05);
        assert!(l.losing("l") && !l.skilled("l", 2.0));
        assert_eq!(l.counts(), (1, 0, 1));
    }

    #[test]
    fn l19_follows_a_skilled_taker_at_our_latency() {
        let fx = Fx {
            decisions: vec![decision("09:55", 18, 0)],
            trades: vec![
                by("10:00:00", idx(20), 0.30, true, 20.0, "s"),
                by("10:02:00", idx(20), 0.32, true, 20.0, "x"),
            ],
            winner: idx(20),
            wallets: book("s", 40, 0.10, 0.05),
            ..Fx::default()
        };
        let out = fx.run();
        let l19 = of(&out, "L19");
        assert_eq!(l19.len(), 1, "{out:?}");
        assert!(close(l19[0].price, 0.32) && l19[0].won);
        assert_eq!(l19[0].filled.as_deref(), Some("12:02"));
        assert_eq!(of(&out, "L19 · t ≥ 3").len(), 1);
        // Nobody to follow without a record.
        let fx = Fx {
            wallets: WalletBook::default(),
            ..fx
        };
        assert!(of(&fx.run(), "L19").is_empty());
    }

    #[test]
    fn l20_sells_the_longshot_a_losing_taker_buys() {
        let fx = Fx {
            decisions: vec![decision("09:55", 18, 0)],
            trades: vec![
                by("10:00:00", idx(23), 0.08, true, 20.0, "l"),
                by("10:05:00", idx(23), 0.06, false, 20.0, "x"),
                by("10:10:00", idx(23), 0.08, true, 20.0, "y"),
            ],
            winner: idx(20),
            wallets: book("l", 40, -0.10, 0.05),
            ..Fx::default()
        };
        let out = fx.run();
        let maker = of(&out, "L20");
        assert_eq!(maker.len(), 1, "{out:?}");
        assert!(close(maker[0].price, 0.93) && maker[0].won);
        assert!(close(maker[0].pnl_usd, maker_pnl(0.93, true)));
        let taker = of(&out, "L20 · taker");
        assert_eq!(taker.len(), 1);
        assert!(close(taker[0].price, 0.94) && taker[0].won);
    }

    #[test]
    fn l21_fades_the_jump_two_degrees_above_a_new_high() {
        let fx = Fx {
            decisions: vec![decision("11:25", 19, 0), decision("11:55", 20, 0)],
            trades: vec![
                trade("11:59", idx(22), 0.10, true),
                trade("12:03", idx(22), 0.09, false),
            ],
            winner: idx(20),
            knmi: Some(vec![reading("11:50", 203, 205)]),
            ..Fx::default()
        };
        let out = fx.run();
        let l21 = of(&out, "L21");
        assert_eq!(l21.len(), 1, "{out:?}");
        assert_eq!(
            (l21[0].side.as_str(), l21[0].bucket.as_str()),
            ("NO", "22°C")
        );
        assert!(close(l21[0].price, 0.91) && l21[0].won);
        assert!(
            of(&out, "L21 · next degree").is_empty(),
            "21 °C did not jump"
        );
        // KNMI still climbing: the jump may be right.
        let fx = Fx {
            knmi: Some(vec![reading("11:50", 212, 214)]),
            ..fx
        };
        assert!(of(&fx.run(), "L21").is_empty());
    }

    #[test]
    fn l22_quotes_the_far_tails_of_the_favourite_overnight() {
        let mut d = decision("00:25", 15, 0);
        d.quotes[idx(20)] = Quote {
            mid: Some(0.40),
            yes_ask: Some(0.42),
            yes_bid: Some(0.38),
        };
        let fx = Fx {
            decisions: vec![d],
            trades: vec![
                trade("00:20", idx(24), 0.04, true),
                trade("00:40", idx(24), 0.04, true),
                // Two places away: not a tail.
                trade("00:20", idx(22), 0.04, true),
                trade("00:40", idx(22), 0.04, true),
            ],
            winner: idx(20),
            ..Fx::default()
        };
        let out = fx.run();
        let l22 = of(&out, "L22");
        assert_eq!(l22.len(), 1, "{out:?}");
        assert_eq!(
            (l22[0].side.as_str(), l22[0].bucket.as_str()),
            ("NO", "24°C")
        );
        assert!(close(l22[0].price, 0.97) && l22[0].won);
        assert_eq!(of(&out, "L22 · ≥ 4 away").len(), 1);
    }

    #[test]
    fn l23_buys_the_rebound_after_a_shower() {
        let mut later = decision("11:55", 21, 10);
        later.f.slope_c_per_hour = Some(1.5);
        let fx = Fx {
            decisions: vec![decision("10:25", 21, 30), later],
            wx: vec![
                wx(
                    "EHAM 151025Z 24012KT 6000 -SHRA BKN020 18/16 Q1011 NOSIG",
                    160,
                ),
                wx(
                    "EHAM 151155Z 24008KT 9999 FEW030 SCT045 20/15 Q1011 NOSIG",
                    150,
                ),
            ],
            trades: vec![trade("11:57", idx(22), 0.15, true)],
            winner: idx(22),
            knmi: Some(vec![reading("11:20", 193, 195), reading("11:50", 201, 203)]),
            ..Fx::default()
        };
        let out = fx.run();
        assert_eq!(of(&out, "L23").len(), 1, "{out:?}");
        assert!(close(of(&out, "L23")[0].pnl_usd, taker_pnl(0.15, true)));
        assert_eq!(of(&out, "L23 · METAR slope").len(), 1);
        // Without KNMI the METAR-slope variant still replays.
        let fx = Fx { knmi: None, ..fx };
        let out = fx.run();
        assert!(of(&out, "L23").is_empty());
        assert_eq!(of(&out, "L23 · METAR slope").len(), 1);
    }

    #[test]
    fn l24_reads_the_upwind_station() {
        let lab = LabSim::default();
        let valkenburg = lab.neighbours[0].clone();
        let fx = Fx {
            decisions: vec![decision("11:25", 21, 0)],
            wx: vec![wx("EHAM 151125Z 24012KT CAVOK 21/12 Q1015 NOSIG", 120)],
            trades: vec![trade("11:45", idx(21), 0.50, false)],
            winner: idx(22),
            knmi: Some(vec![reading("11:40", 210, 212)]),
            neighbours: vec![(valkenburg, vec![point(z("11:40"), "ta", 22.4)])],
            ..Fx::default()
        };
        let out = fx.run();
        let l24 = of(&out, "L24");
        assert_eq!(l24.len(), 1, "{out:?}");
        assert_eq!(
            (l24[0].side.as_str(), l24[0].bucket.as_str()),
            ("NO", "21°C")
        );
        assert!(close(l24[0].price, 0.50) && l24[0].won);
        assert!(of(&out, "L24 · cool side").is_empty());
        // The wind from De Bilt's side: Voorschoten is not upwind.
        let mut fx = fx;
        fx.wx = vec![wx("EHAM 151125Z 12012KT CAVOK 21/12 Q1015 NOSIG", 120)];
        assert!(of(&fx.run(), "L24").is_empty());
    }

    #[test]
    fn l25_caps_the_afternoon_when_radiation_collapses() {
        let lab = LabSim::default();
        let points: Vec<SeriesPoint> = (0..13)
            .map(|k| {
                let end = z("09:00") + Duration::minutes(10 * k);
                let cs = clear_sky_ghi(end - Duration::minutes(5), lab.latitude, lab.longitude);
                let share = if end > z("10:30") { 0.2 } else { 0.8 };
                point(end, "qg", share * cs)
            })
            .collect();
        let fx = Fx {
            decisions: vec![decision("10:55", 21, 5)],
            trades: vec![trade("11:04", idx(22), 0.15, false)],
            winner: idx(21),
            radiation: Some(points),
            ..Fx::default()
        };
        let out = fx.run();
        let l25 = of(&out, "L25");
        assert_eq!(l25.len(), 1, "{out:?}");
        assert_eq!(
            (l25[0].side.as_str(), l25[0].bucket.as_str()),
            ("NO", "22°C")
        );
        assert!(close(l25[0].price, 0.85) && l25[0].won);
        assert!(of(&out, "L25 · clearing").is_empty());
    }

    #[test]
    fn clear_sky_radiation_and_bearings_are_right() {
        // Midsummer solar noon at Schiphol: the sun 61° high, ~900 W/m².
        let noon = utc("2026-06-21T11:42:00Z");
        let c = wm_strategy::lab::solar::cos_zenith(noon, 52.318, 4.790);
        assert!(
            (c - (90.0f64 - 61.1).to_radians().cos()).abs() < 0.01,
            "{c}"
        );
        let ghi = clear_sky_ghi(noon, 52.318, 4.790);
        assert!((850.0..950.0).contains(&ghi), "{ghi}");
        assert_eq!(
            clear_sky_ghi(utc("2026-06-21T23:00:00Z"), 52.318, 4.790),
            0.0
        );
        // Midwinter noon: the sun ~14° high.
        let w = wm_strategy::lab::solar::cos_zenith(utc("2026-12-21T11:51:00Z"), 52.318, 4.790);
        assert!(
            (w - (90.0f64 - 14.3).to_radians().cos()).abs() < 0.01,
            "{w}"
        );
        let lab = LabSim::default();
        let b: Vec<f64> = lab.neighbours.iter().map(|n| lab.bearing_to(n)).collect();
        assert!((b[0] - 231.0).abs() < 2.0, "Voorschoten {}", b[0]);
        assert!((b[1] - 132.0).abs() < 2.0, "De Bilt {}", b[1]);
        assert!((b[2] - 20.0).abs() < 2.0, "Berkhout {}", b[2]);
        assert!(close(angle_between(350.0, 10.0), 20.0));
        assert!(close(angle_between(10.0, 350.0), 20.0));
        assert!(close(angle_between(90.0, 270.0), 180.0));
    }

    #[test]
    fn the_weather_of_each_report_comes_from_its_own_observation() {
        use wm_core::weather::{ObservationKey, QualityFlags, ReportType, TempPrecision};
        let obs = |at: &str, raw: &str| Observation {
            key: ObservationKey {
                station: StationId::new("EHAM").unwrap(),
                observed_at: z(at),
                report_type: ReportType::Metar,
            },
            version: 1,
            temperature: Some(TempC::from_tenths(200)),
            dewpoint: Some(TempC::from_tenths(120)),
            precision: TempPrecision::WholeDegree,
            raw_text: raw.into(),
            content_hash: String::new(),
            provider: ProviderId::new("iem").unwrap(),
            provider_receipt_at: None,
            fetched_at: z(at),
            parser_version: 1,
            quality: QualityFlags::default(),
        };
        let a = obs("11:25", "EHAM 151125Z 24012KT CAVOK 20/12 Q1015 NOSIG");
        let b = obs(
            "11:55",
            "EHAM 151155Z 31015KT 9999 -SHRA BKN020 18/14 Q1016 NOSIG",
        );
        let decisions = [
            decision("11:25", 20, 0),
            decision("11:55", 20, 20),
            decision("12:25", 20, 20),
        ];
        let w = weather_of(&decisions, &[&a, &b]);
        assert_eq!(w.len(), 3);
        assert_eq!(
            w[0].as_ref()
                .and_then(|x| x.wx.wind())
                .and_then(|w| w.direction),
            Some(240)
        );
        assert!(w[1].as_ref().is_some_and(|x| x.wx.body.precipitation()));
        assert_eq!(w[1].as_ref().and_then(|x| x.dew_tenths), Some(120));
        assert!(w[2].is_none(), "no observation at 12:25");
    }

    #[test]
    fn a_rule_without_its_input_is_reported_as_not_replayed() {
        let cov = LabCoverage {
            days: 30,
            forecast_days: 30,
            ..LabCoverage::default()
        };
        let rows = lab_rows(&[], 50, 7);
        let v = verdict(&rows, &cov, &MarketSimConfig::default());
        assert_eq!(v.len(), usize::from(FAMILIES));
        assert!(v[0].starts_with("L1 KNMI-shielded maker"));
        assert!(
            v[0].contains("*L1* not replayed: no KNMI ten-minute readings"),
            "{}",
            v[0]
        );
        assert!(v[13].contains("*L14* no trade"), "{}", v[13]);
        assert!(v[2].contains("F's 100 shares a trade"), "{}", v[2]);
        assert!(
            v[22].contains("*L23* not replayed") && v[22].contains("*L23 · METAR slope* no trade")
        );
        let md = markdown(&rows, &MarketSimConfig::default(), &cov, &v, &[]);
        assert!(md.contains("## Strategy lab: L1–L25 at traded prices"));
        assert!(
            md.contains("| L14 |") && !md.contains("| L1 |"),
            "rows only for what replayed"
        );
    }

    fn lab_trade(day: u32, label: &str, pnl: f64) -> SimTrade {
        let rule = rules().into_iter().find(|r| r.label == label).unwrap();
        SimTrade {
            date: NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
            report: "12:00".into(),
            structure: STRUCTURE.into(),
            strategy: label.into(),
            window: 0,
            range: rule.range(),
            bucket: "20°C".into(),
            side: "NO".into(),
            price: 0.9,
            p_model: 0.9,
            p_used: 0.9,
            won: pnl > 0.0,
            pnl_usd: pnl,
            resolved: "21°C".into(),
            filled: None,
        }
    }

    #[test]
    fn out_of_sample_chooses_on_the_first_half_and_names_what_held_up() {
        let days: Vec<NaiveDate> = (1..=30)
            .map(|d| NaiveDate::from_ymd_opt(2026, 7, d).unwrap())
            .collect();
        let cov = LabCoverage {
            days: 30,
            knmi_days: 30,
            ..LabCoverage::default()
        };
        // L9's variant wins the first half, then keeps winning; L9 itself
        // loses later. L8 never trades on the first half.
        let mut trades = Vec::new();
        for d in 1..=30 {
            trades.push(lab_trade(d, "L9 · thunder", 2.0 + f64::from(d % 3)));
            trades.push(lab_trade(d, "L9", if d <= 15 { 1.0 } else { -1.0 }));
        }
        trades.push(lab_trade(20, "L8", 3.0));
        // L5 wins both its first-half trades and both of its two later
        // ones: an interval of no width, too few to hold up.
        for d in [2, 4, 18, 22] {
            trades.push(lab_trade(d, "L5", 1.5));
        }
        let v = out_of_sample(&trades, &days, &cov, 200, 3);
        let l5 = v
            .iter()
            .find(|l| l.starts_with("L5 out of sample"))
            .unwrap();
        assert!(l5.contains("it made 2 trades, 2 won"), "{l5}");
        let l9 = v
            .iter()
            .find(|l| l.starts_with("L9 out of sample"))
            .unwrap();
        assert!(l9.contains("the best rule was *L9 · thunder*"), "{l9}");
        assert!(
            l9.contains("The main rule *L9* made 15 trades there, 0 won"),
            "{l9}"
        );
        let l8 = v
            .iter()
            .find(|l| l.starts_with("L8 out of sample"))
            .unwrap();
        assert!(l8.contains("no L8 rule traded on the first 15"), "{l8}");
        let last = v.last().unwrap();
        assert!(
            last.contains("held up on the later days") && last.contains("L9 · thunder"),
            "{last}"
        );
        assert!(!last.contains("L9 ("), "{last}");
        assert!(!last.contains("L5"), "two trades do not hold up: {last}");
        // Too few days: one line.
        assert_eq!(out_of_sample(&trades, &days[..10], &cov, 200, 3).len(), 1);
    }

    #[test]
    fn helpers_read_knmi_windows_and_forecasts_exactly() {
        let rs = vec![
            reading("12:00", 180, 181),
            reading("12:10", 181, 182),
            reading("12:20", 182, 183),
        ];
        assert_eq!(window(&rs, z("12:20"), 20).len(), 2);
        assert_eq!(window(&rs, z("12:20"), 30).len(), 3);
        assert_eq!(window(&rs, z("12:05"), 30).len(), 1);
        assert!(window(&rs, z("11:00"), 30).is_empty());
        assert_eq!(mean_at(&rs, z("12:10")), Some(181));
        assert_eq!(mean_at(&rs, z("12:15")), None);
        let d = Duration::minutes(5);
        assert_eq!(
            latest_known(&rs, z("12:14"), d).map(|r| r.interval_end),
            Some(z("12:00"))
        );
        assert_eq!(
            latest_known(&rs, z("12:15"), d).map(|r| r.interval_end),
            Some(z("12:10"))
        );
        assert!(latest_known(&rs, z("12:04"), d).is_none());
        let fc = forecast(afternoon_peak);
        assert_eq!(forecast_peak(&fc), Some(z("13:00")));
        assert!(close(forecast_at(&fc, z("13:00")).unwrap(), 200.0));
        assert_eq!(round_half_up(20.5), 21);
        assert_eq!(round_half_up(20.49), 20);
    }

    #[test]
    fn l3_never_exits_on_a_signal_known_before_the_fill() {
        // F filled from the tape at 15:45 local (13:45Z); the 13:40 reading
        // was known at 13:45:00, inside that minute: no exit on it.
        let mut f = f_trade();
        f.filled = Some("15:45".into());
        let fx = Fx {
            decisions: vec![decision("13:25", 19, 10), decision("13:55", 20, 0)],
            trades: vec![trade("13:46", idx(19), 0.80, false)],
            winner: idx(20),
            knmi: Some(vec![reading("13:40", 199, 200)]),
            f_trades: vec![f],
            ..Fx::default()
        };
        let out = fx.run();
        let l3 = of(&out, "L3");
        assert_eq!(l3.len(), 1, "{out:?}");
        assert_eq!(l3[0].side, "YES", "held");
        assert!(close(l3[0].pnl_usd, of(&out, "L3 · hold (F)")[0].pnl_usd));
    }
}
