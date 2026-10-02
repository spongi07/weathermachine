//! The strategy lab's 25 strategies (L1–L25) as paper strategies.
//!
//! `research market` replays the lab at traded prices
//! (`wm_backtest::market_lab`, reasoning and sources in
//! `docs/research/strategy-lab.md`). Here each family's rule trades live in
//! paper, on the order book as it is, so that every family collects fresh
//! evidence on days no rule was chosen on. The rules and their thresholds
//! are the replay's main rules (or, listed in [`LabConfig::variants`], the
//! variants), fixed before any result.
//!
//! **Isolated.** Each lab strategy trades its own paper book (the engine
//! keeps positions, risk counters and order limits per lab strategy), so no
//! lab strategy blocks another one or strategies A–K, and each one's P&L is
//! its own — as each rule's was in the replay.
//!
//! **Inputs** beyond A–K's ([`LabInputs`]): the station's KNMI ten-minute
//! readings of the last hours (with KNMI's global radiation), the readings
//! of neighbouring KNMI stations, the weather groups of today's METARs, the
//! day-1 hourly forecast, yesterday's forecast error, the market's taker
//! trades with their (hashed) wallets, and the takers' records on settled
//! days. A missing input blocks only the families that read it, and says so.
//!
//! Takers buy at the ask with fill-and-kill orders, sized to the stake and
//! the depth at the ask; makers rest good-till-date NO bids; everything is
//! held to settlement, except L3, which sells F's YES before a new high.

mod day;
mod flow;
mod forecast;
mod knmi;
mod metar;
pub mod sky;
pub mod solar;
pub mod wallets;

use crate::peak_slot::{PeakSlotConfig, PeakSlotHigh};
use crate::strategy::{Strategy, StrategyContext, StrategyOutput};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use wm_core::ids::StrategyId;
use wm_core::market::TakerTrade;
use wm_core::metar_wx::{MetarWx, parse_wx};
use wm_core::units::{Price, Usd, decimal_serde};
use wm_core::weather::{Observation, TenMinuteObservation};

pub use crate::forecast::ForecastDay;
pub use day::taker_size;
pub use wallets::{WalletBook, WalletScores, WalletStats};

/// Number of families.
pub const FAMILIES: usize = 25;

/// Engine ids, L1 first. The part before the first `_` tags each
/// strategy's evaluation lines (`L4 21°C YES · …`).
pub const IDS: [&str; FAMILIES] = [
    "L1_shielded_maker",
    "L2_informed_maker",
    "L3_f_escape",
    "L4_cooling_lock",
    "L5_late_next_no",
    "L6_late_new_yes",
    "L7_knmi_slope",
    "L8_sea_breeze",
    "L9_rain_cap",
    "L10_trend_cap",
    "L11_fog_fade",
    "L12_clear_sky",
    "L13_front_lock",
    "L14_forecast_departure",
    "L15_own_peak",
    "L16_evening_high",
    "L17_yesterday_error",
    "L18_burst_follow",
    "L19_skill_follow",
    "L20_longshot_fade",
    "L21_jump_fade",
    "L22_overnight_tails",
    "L23_shower_recovery",
    "L24_upwind",
    "L25_radiation",
];

/// The families' names, L1 first.
pub const NAMES: [&str; FAMILIES] = [
    "KNMI-shielded maker on the next degree",
    "KNMI-informed maker on the doomed bucket",
    "F's escape hatch",
    "KNMI cooling lock",
    "Late next-degree NO under KNMI cooling",
    "K late: YES of the new degree",
    "KNMI slope: K one reading early",
    "Sea-breeze lock",
    "Rain-cooled cap",
    "TREND cap",
    "Fog and stratus fade",
    "Clear dry morning: the bucket above",
    "Cold-front early-high lock",
    "Morning departure from the hourly forecast",
    "Past the forecast's own peak hour",
    "Evening-high days",
    "Yesterday's forecast error",
    "Pre-report burst, confirmed by KNMI",
    "Follow skilled takers",
    "Fade losing longshot buyers",
    "Fade the jump after a new high",
    "Overnight tail maker around the favourite",
    "After the shower: the recovery",
    "Upwind KNMI station",
    "Radiation collapse",
];

/// The families whose main rule reads KNMI's ten-minute readings (L3 only
/// for its exit): without `WM_KNMI_API_KEY` they stay idle, and L3 enters
/// as F does without its exit.
pub const KNMI_FAMILIES: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 18, 21, 23, 24, 25];

/// A family's code in the configuration and on the dashboard: `L1` … `L25`.
pub fn code(family: u8) -> String {
    format!("L{family}")
}

/// The family (1–25) of a lab strategy id or code (`L4_cooling_lock`, `L4`).
pub fn family_of(id: &str) -> Option<u8> {
    let rest = id.strip_prefix('L')?;
    let digits = rest.split('_').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u8 = digits.parse().ok()?;
    (1..=FAMILIES as u8).contains(&n).then_some(n)
}

/// Whether `id` is a lab strategy's engine id.
pub fn is_lab_id(id: &str) -> bool {
    family_of(id).is_some_and(|f| IDS[usize::from(f - 1)] == id)
}

/// The engine id of family `family` (1–25).
pub fn id_of(family: u8) -> Option<&'static str> {
    IDS.get(usize::from(family.checked_sub(1)?)).copied()
}

/// The lab's paper-trading settings. Every rule threshold is the replay's
/// (see `docs/research/strategy-lab.md` §3); only the stake, the timing of
/// quotes and the paper books' limits are set here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LabConfig {
    pub enabled: bool,
    /// Families not run, by code (`["L19"]`).
    pub disabled: Vec<String>,
    /// Families that run their variant (the second rule `research market`
    /// lists for them) instead of their main rule, by code.
    pub variants: Vec<String>,
    /// Cost of one trade at its price (L3 buys F's shares).
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    /// Slippage allowed for in the expected profit of a taker buy.
    #[serde(with = "decimal_serde::price")]
    pub slippage_allowance: Price,
    pub max_book_age_ms: i64,
    /// METAR data age limit.
    pub max_data_age_minutes: i64,
    /// A KNMI reading older than this (from the end of its interval) is no
    /// signal.
    pub max_reading_age_minutes: i64,
    /// A routine report counts as known this long after its observation
    /// (quotes that wait for one stay until then).
    pub report_known_minutes: i64,
    /// L22's quotes expire this long before the next routine report …
    pub cancel_before_report_minutes: i64,
    /// … and are not posted with less than this to go.
    pub min_rest_minutes: i64,
    /// Each lab strategy's own paper book.
    pub risk: LabRiskConfig,
}

impl Default for LabConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled: Vec::new(),
            variants: Vec::new(),
            notional: Usd::from_whole(20),
            slippage_allowance: Price::saturating_from_micros(5_000),
            max_book_age_ms: 15_000,
            max_data_age_minutes: 40,
            max_reading_age_minutes: 15,
            report_known_minutes: 3,
            cancel_before_report_minutes: 10,
            min_rest_minutes: 5,
            risk: LabRiskConfig::default(),
        }
    }
}

impl LabConfig {
    /// Off (a configuration file without the section).
    pub fn absent() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Whether family `family` runs.
    pub fn runs(&self, family: u8) -> bool {
        self.enabled && !self.disabled.iter().any(|c| family_of(c) == Some(family))
    }

    /// Whether family `family` runs its variant.
    pub fn variant(&self, family: u8) -> bool {
        self.variants.iter().any(|c| family_of(c) == Some(family))
    }

    /// Codes in `disabled` and `variants` that name no family.
    pub fn unknown_codes(&self) -> Vec<String> {
        self.disabled
            .iter()
            .chain(&self.variants)
            .filter(|c| family_of(c).is_none_or(|f| code(f) != c.as_str()))
            .cloned()
            .collect()
    }
}

/// The limits of each lab strategy's own paper book. The engine combines
/// them with the main risk settings (data freshness, prices, books), so a
/// lab strategy passes the same pre-trade gates as A–K, against its own
/// money only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LabRiskConfig {
    /// Largest cost of one order.
    #[serde(with = "decimal_serde::usd")]
    pub position_size_usd: Usd,
    /// L3 buys F's shares: its largest order.
    #[serde(with = "decimal_serde::usd")]
    pub f_escape_position_usd: Usd,
    /// Capital one lab strategy may hold in open positions and live orders
    /// (also its worst-case loss cap).
    #[serde(with = "decimal_serde::usd")]
    pub max_exposure_usd: Usd,
    #[serde(with = "decimal_serde::usd")]
    pub max_daily_new_exposure_usd: Usd,
    #[serde(with = "decimal_serde::usd")]
    pub max_daily_loss_usd: Usd,
    /// Widest book on which a lab strategy opens a position: the replay
    /// traded at the tape's prices, and several rules trade minutes before
    /// a report, when makers widen their quotes.
    #[serde(with = "decimal_serde::price")]
    pub max_spread: Price,
    pub max_orders_per_minute: u32,
}

impl Default for LabRiskConfig {
    fn default() -> Self {
        Self {
            position_size_usd: Usd::from_whole(25),
            f_escape_position_usd: Usd::from_whole(100),
            max_exposure_usd: Usd::from_whole(120),
            max_daily_new_exposure_usd: Usd::from_whole(120),
            max_daily_loss_usd: Usd::from_whole(100),
            max_spread: Price::saturating_from_micros(100_000),
            max_orders_per_minute: 10,
        }
    }
}

/// One of today's METARs with its weather groups.
#[derive(Debug, Clone, PartialEq)]
pub struct WxReport {
    pub observed_at: DateTime<Utc>,
    /// When it reached the bot.
    pub known_at: DateTime<Utc>,
    pub temp_tenths: Option<i32>,
    pub dew_tenths: Option<i32>,
    pub wx: MetarWx,
}

impl WxReport {
    pub fn from_observation(o: &Observation) -> Self {
        Self {
            observed_at: o.key.observed_at,
            known_at: o.fetched_at,
            temp_tenths: o.temperature.map(|t| t.tenths()),
            dew_tenths: o.dewpoint.map(|t| t.tenths()),
            wx: parse_wx(&o.raw_text),
        }
    }
}

/// A neighbouring KNMI station's readings (L24).
#[derive(Debug, Clone, Copy)]
pub struct NeighbourReadings<'a> {
    pub name: &'a str,
    /// Bearing from the market's station, degrees true.
    pub bearing_deg: f64,
    /// Oldest first.
    pub readings: &'a [TenMinuteObservation],
}

/// What only the lab strategies read. The engine fills it for each
/// evaluation; anything it does not have is empty.
#[derive(Debug, Clone, Copy)]
pub struct LabInputs<'a> {
    /// The station's KNMI readings of the last hours, oldest first.
    pub knmi: &'a [TenMinuteObservation],
    pub neighbours: &'a [NeighbourReadings<'a>],
    /// Today's METARs (local day), oldest first.
    pub reports: &'a [WxReport],
    /// Today's day-1 hourly forecast, usable from its knowledge time.
    pub forecast: Option<&'a ForecastDay>,
    /// Yesterday's observed high minus its forecast maximum (tenths °C).
    pub yesterday_error_tenths: Option<i32>,
    /// Today's market's taker trades, oldest first.
    pub takers: &'a [TakerTrade],
    /// Takers' records on settled days; `None` until they are loaded.
    pub wallets: Option<&'a WalletScores>,
    /// The station's latitude and longitude (clear-sky radiation).
    pub position: Option<(f64, f64)>,
}

impl LabInputs<'static> {
    /// No lab input at all.
    pub const EMPTY: Self = Self {
        knmi: &[],
        neighbours: &[],
        reports: &[],
        forecast: None,
        yesterday_error_tenths: None,
        takers: &[],
        wallets: None,
        position: None,
    };
}

impl Default for LabInputs<'_> {
    fn default() -> Self {
        LabInputs::EMPTY
    }
}

/// One lab family as a paper strategy.
pub struct LabStrategy {
    id: StrategyId,
    family: u8,
    variant: bool,
    cfg: LabConfig,
    /// L3: strategy F's entry rule, trading L3's own book.
    f: Option<PeakSlotHigh>,
}

impl LabStrategy {
    /// Family `family` (1–25) with `cfg`; `f` is strategy F's configuration
    /// (L3 enters as F does).
    ///
    /// # Panics
    ///
    /// When `family` is not 1–25.
    pub fn new(family: u8, cfg: &LabConfig, f: &PeakSlotConfig) -> Self {
        let id = id_of(family).unwrap_or_else(|| panic!("no lab family {family}"));
        Self {
            id: StrategyId::from_static(id),
            family,
            variant: cfg.variant(family),
            cfg: cfg.clone(),
            f: (family == 3).then(|| {
                PeakSlotHigh::new(PeakSlotConfig {
                    enabled: true,
                    ..f.clone()
                })
            }),
        }
    }

    pub fn family(&self) -> u8 {
        self.family
    }

    pub fn variant(&self) -> bool {
        self.variant
    }
}

/// The families `cfg` runs, L1 first (none when the lab is off).
pub fn lab_strategies(cfg: &LabConfig, f: &PeakSlotConfig) -> Vec<LabStrategy> {
    (1..=FAMILIES as u8)
        .filter(|&n| cfg.runs(n))
        .map(|n| LabStrategy::new(n, cfg, f))
        .collect()
}

impl Strategy for LabStrategy {
    fn id(&self) -> &StrategyId {
        &self.id
    }

    fn enabled(&self) -> bool {
        self.cfg.runs(self.family)
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        let Some(day) = day::Day::new(ctx, &self.cfg, &self.id) else {
            return out;
        };
        let v = self.variant;
        match self.family {
            1 => knmi::shielded_maker(&day, v, &mut out),
            2 => knmi::informed_maker(&day, v, &mut out),
            3 => {
                if let Some(f) = self.f.as_mut() {
                    knmi::f_escape(&day, v, f, &mut out);
                }
            }
            4 => knmi::cooling_lock(&day, v, &mut out),
            5 => knmi::late_next_no(&day, v, &mut out),
            6 => knmi::late_new_yes(&day, v, &mut out),
            7 => knmi::slope_k(&day, v, &mut out),
            8 => metar::sea_breeze(&day, v, &mut out),
            9 => metar::rain_cap(&day, v, &mut out),
            10 => metar::trend_cap(&day, v, &mut out),
            11 => metar::fog_fade(&day, v, &mut out),
            12 => metar::clear_sky(&day, v, &mut out),
            13 => metar::front_lock(&day, v, &mut out),
            14 => forecast::departure(&day, v, &mut out),
            15 => forecast::own_peak(&day, v, &mut out),
            16 => forecast::evening_high(&day, v, &mut out),
            17 => forecast::yesterday_error(&day, v, &mut out),
            18 => flow::burst_follow(&day, v, &mut out),
            19 => flow::skill_follow(&day, v, &mut out),
            20 => flow::longshot_fade(&day, v, &mut out),
            21 => flow::jump_fade(&day, v, &mut out),
            22 => flow::overnight_tails(&day, v, &mut out),
            23 => metar::shower_recovery(&day, v, &mut out),
            24 => sky::upwind(&day, v, &mut out),
            25 => sky::radiation(&day, v, &mut out),
            _ => {}
        }
        out
    }
}

/// What a family does, in a sentence or two (main rule or variant).
pub fn summary(family: u8, variant: bool) -> &'static str {
    match (family, variant) {
        (1, false) => {
            "10:00–20:00: while KNMI's means of the last 30 minutes stay ≥ 0.5 °C under the high's rounding edge and are not rising, rests a NO bid on the next degree one tick inside the spread (YES offered at 0.03–0.40) until the next reading or the report after it."
        }
        (1, true) => "The control: the same NO bids on the next degree without KNMI's shield.",
        (2, false) => {
            "When KNMI's reading before the next METAR is ≥ 0.3 °C above the high's rounding edge, rests a NO bid on the high's bucket (YES offered at 0.25–0.97) until that report is known."
        }
        (2, true) => "As L2 with a margin of 0.6 °C above the rounding edge.",
        (3, false) => {
            "Enters as strategy F does (its own 100 shares), then sells the YES at the bid before the METAR when KNMI's reading is ≥ 0.3 °C above the bucket's rounding edge."
        }
        (3, true) => "As L3, selling once KNMI's reading reaches the bucket's rounding edge.",
        (4, false) => {
            "13:00–20:00: when KNMI's maxima of the last 90 minutes stay ≥ 0.3 °C under the bucket's rounding edge, its mean fell ≥ 0.6 °C in an hour and the METAR is ≥ 1 °C under the high, buys YES of the high's bucket at 0.70–0.95 (expected profit at p = 0.97)."
        }
        (4, true) => "As L4 in F's band 0.90–0.98.",
        (5, false) => {
            "15:00–21:00: when KNMI's means of the last hour stayed ≥ 1 °C and its maxima ≥ 0.5 °C under the rounding edge, falling, buys NO of the next degree at 0.75–0.97 (p = 0.99)."
        }
        (5, true) => "As L5 from 17:00.",
        (6, false) => {
            "When K's trigger fires (KNMI ≥ edge + 0.8 °C before the METAR) after the season's median peak time with a rise of ≤ 0.4 °C in 30 minutes, buys YES of the next degree at 0.20–0.70."
        }
        (6, true) => "The control: YES of the next degree whenever K's trigger fires.",
        (7, false) => {
            "When KNMI's mean is within 0.5 °C under to 0.8 °C over the rounding edge and its 20-minute slope reaches edge + 0.6 °C by the next report, buys NO of the high's bucket at 0.05–0.75 (p = 0.85)."
        }
        (7, true) => "As L7 with the slope reaching edge + 1.0 °C.",
        (8, false) => {
            "11:00–17:30 on a day ≥ 20 °C: the wind turned onshore (250–020°, ≥ 6 kt) after an offshore morning, the dew point ≥ 1 °C above the high's report, ≥ 1 °C under the high → NO of the next degree at 0.60–0.96 (p = 0.95)."
        }
        (8, true) => "As L8, buying YES of the high's bucket at 0.50–0.92 (p = 0.94).",
        (9, false) => {
            "12:00–19:00: rain, showers or thunder now or since the last report and ≥ 2 °C under the high → NO of the next degree at 0.60–0.96 (p = 0.95)."
        }
        (9, true) => "As L9 on thunder or a cumulonimbus only.",
        (10, false) => {
            "11:00–17:00: the METAR's TREND announces showers or thunder, an onshore wind (240–020°) or a ceiling under 3000 ft → NO of the next degree at 0.55–0.94 (p = 0.92)."
        }
        (10, true) => {
            "The NOSIG lock: after the season's median peak time, NOSIG, no ceiling under 5000 ft and ≥ 1 °C under the high → YES of the high's bucket at 0.55–0.92 (p = 0.93)."
        }
        (11, false) => {
            "08:30–11:30: fog or mist under 5 km or a ceiling ≤ 800 ft and the market's favourite (≥ 0.30) ≥ 6 °C above the temperature → NO of the favourite at 0.30–0.70."
        }
        (11, true) => "As L11 with a gap of 4 °C.",
        (12, false) => {
            "10:00–13:00: no ceiling under 5000 ft, the dew point ≥ 8 °C under the temperature, the wind continental (045–225°) or calm → YES of the bucket above the favourite (≥ 0.25) at 0.06–0.30."
        }
        (12, true) => "As L12 with any wind.",
        (13, false) => {
            "09:00–15:00: the high first reached before 11:00, the wind veered from 120–229° to 230–340° (≥ 8 kt), QNH ≥ 1 hPa up, ≥ 1.5 °C under the high → YES of the high's bucket at 0.40–0.88 (p = 0.90)."
        }
        (13, true) => "As L13, buying NO of the next degree at 0.60–0.95 (p = 0.95).",
        (14, false) => {
            "10:00–12:30: the forecast's remaining maximum + 0.7 × (observed − forecast now); when that bucket is not the market's favourite, YES of it at 0.05–0.35."
        }
        (14, true) => "As L14 with the full departure (λ = 1.0).",
        (15, false) => {
            "≥ 120 minutes after the hourly forecast's own peak (and from 12:00), the forecast falling ≥ 1 °C, ≥ 1 °C under the high → YES of the high's bucket at 0.70–0.95 (p = 0.96)."
        }
        (15, true) => "As L15 from 60 minutes after the forecast's peak.",
        (16, false) => {
            "On days the forecast's 17–24 h maximum beats its 10–17 h one by ≥ 0.5 °C: 14:00–18:00, the forecast still rising ≥ 1 °C, within 1 °C of the high → YES of the next degree at 0.05–0.30."
        }
        (16, true) => "As L16 with the forecast rising ≥ 0.5 °C.",
        (17, false) => {
            "07:00–10:00: today's forecast maximum + 0.5 × yesterday's error (observed − forecast); when its bucket differs from the raw forecast's, YES of it at 0.05–0.30."
        }
        (17, true) => "As L17 with the full error.",
        (18, false) => {
            "From 6 minutes before a routine report until it is known: ≥ 40 shares from ≥ 2 takers within 3 minutes selling the high's bucket or buying the next, and KNMI ≥ edge + 0.3 °C → NO of the high's bucket at 0.05–0.85."
        }
        (18, true) => "The control: the same bursts without KNMI's confirmation.",
        (19, false) => {
            "A taker whose ≥ 30 trades on settled days made ≥ 0.02 a share (t ≥ 2) trades at 0.05–0.90: their side from 30 seconds later for 10 minutes, at ≤ their price + 0.03."
        }
        (19, true) => "As L19 with t ≥ 3.",
        (20, false) => {
            "A taker whose ≥ 30 trades on settled days lost ≥ 0.05 a share buys YES at 0.02–0.15: a NO bid one tick under their price for 30 minutes."
        }
        (20, true) => "As L20, taking NO at 0.80–0.98 within 10 minutes.",
        (21, false) => {
            "2–15 minutes after a METAR raised the high: the bucket two degrees above was bought at ≥ 0.08 since the report and KNMI is ≤ the new rounding edge + 0.3 °C → its NO at 0.60–0.92."
        }
        (21, true) => "As L21 on the next degree (bought at ≥ 0.15; NO at 0.50–0.85).",
        (22, false) => {
            "00:00–09:00: NO bids one tick inside the spread (YES offered at 0.01–0.05) on buckets ≥ 3 places from the market's favourite (≥ 0.20) that can still win, withdrawn 10 minutes before each report."
        }
        (22, true) => "As L22 on buckets ≥ 4 places away.",
        (23, false) => {
            "12:00–16:30: rain in the last 3 hours, now dry with no ceiling under 4000 ft, within 1.5 °C of the high and KNMI's mean up ≥ 0.5 °C in 30 minutes → YES of the next degree at 0.05–0.35."
        }
        (23, true) => "As L23 with the METAR's slope ≥ 1 °C an hour instead of KNMI.",
        (24, false) => {
            "10:00–18:00: the METAR wind (≥ 6 kt) blows from within 40° of a KNMI neighbour ≥ 0.8 °C warmer than Schiphol, Schiphol within 0.6 °C under the rounding edge → NO of the high's bucket at 0.05–0.75 (p = 0.80)."
        }
        (24, true) => {
            "The cool side: from 12:00 the upwind neighbour ≥ 1.0 °C cooler and Schiphol ≥ 0.5 °C under the edge → NO of the next degree at 0.65–0.96 (p = 0.93)."
        }
        (25, false) => {
            "10:30–15:30: KNMI's global radiation under 35 % of the clear sky for 30 minutes after ≥ 70 % for the hour before, KNMI ≥ 0.3 °C under the rounding edge → NO of the next degree at 0.60–0.95 (p = 0.93)."
        }
        (25, true) => {
            "The clearing: ≥ 80 % of the clear sky after ≤ 40 %, KNMI within 0.8 °C of the edge → YES of the next degree at 0.05–0.35."
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_codes_and_families_agree() {
        for (i, id) in IDS.iter().enumerate() {
            let family = u8::try_from(i + 1).unwrap();
            assert_eq!(family_of(id), Some(family), "{id}");
            assert!(is_lab_id(id));
            assert_eq!(id_of(family), Some(*id));
            assert_eq!(id.split('_').next(), Some(code(family).as_str()));
            assert!(!summary(family, false).is_empty() && !summary(family, true).is_empty());
        }
        assert_eq!(family_of("L4"), Some(4));
        assert_eq!(family_of("L26"), None);
        assert_eq!(family_of("L0_x"), None);
        assert_eq!(family_of("K_knmi_nowcast"), None);
        assert!(!is_lab_id("L4"), "a code is not an id");
        assert!(!is_lab_id("L4_other"));
        assert!(!is_lab_id("F_peak_slot"));
    }

    #[test]
    fn families_switch_off_and_to_their_variant_by_code() {
        let cfg = LabConfig {
            disabled: vec!["L19".into(), "L20".into()],
            variants: vec!["L24".into()],
            ..LabConfig::default()
        };
        assert!(cfg.runs(1) && !cfg.runs(19) && !cfg.runs(20));
        assert!(cfg.variant(24) && !cfg.variant(25));
        let f = PeakSlotConfig::default();
        let all = lab_strategies(&cfg, &f);
        assert_eq!(all.len(), 23);
        assert!(all.iter().all(|s| s.enabled()));
        assert!(all.iter().find(|s| s.family() == 24).unwrap().variant());
        assert!(lab_strategies(&LabConfig::absent(), &f).is_empty());
        let odd = LabConfig {
            disabled: vec!["L99".into(), "l4".into()],
            variants: vec!["L04".into(), "L5".into()],
            ..LabConfig::default()
        };
        assert_eq!(odd.unknown_codes(), vec!["L99", "l4", "L04"]);
    }
}
