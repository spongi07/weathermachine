//! The strategy lab's 65 strategies (L1–L65) as paper strategies.
//!
//! `research market` replays L1–L25 at traded prices
//! (`wm_backtest::market_lab`, reasoning and sources in
//! `docs/research/strategy-lab.md`). Here each family's rule trades live in
//! paper, on the order book as it is, so that every family collects fresh
//! evidence on days no rule was chosen on. The rules and their thresholds
//! are the replay's main rules (or, listed in [`LabConfig::variants`], the
//! variants), fixed before any result.
//!
//! **L26–L65** come from how this market works rather than from public
//! forecasts (which lose to the market): air on its way from upstream
//! airports, KNMI's dew point, wind, gusts, pressure and sunshine, the shape
//! and history of the price ladder, who trades and when, and the market's
//! own consistency. Each reads its own signal, none shared with another
//! family, and carries a risk profile ([`Profile`]) that sets its stake.
//! None depends on speed: a signal must hold for
//! [`LabConfig::confirm_seconds`] before it trades (a race lasting less is
//! not theirs to win), and a taker buy pays up to
//! [`LabConfig::taker_tolerance`] above the ask it saw, its expected profit
//! counted at that worse price.
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
//! held to settlement, except L3, which sells F's YES before a new high,
//! and L59, which makes one round trip a day.

mod air;
mod day;
mod flow;
mod forecast;
mod knmi;
mod ladder;
mod memory;
mod metar;
mod reaction;
pub mod sky;
pub mod solar;
mod summaries;
mod tape;
mod upstream;
pub mod wallets;

use crate::peak_slot::{PeakSlotConfig, PeakSlotHigh};
use crate::strategy::{Strategy, StrategyContext, StrategyOutput};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wm_core::ids::{StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, TakerTrade};
use wm_core::metar_wx::{MetarWx, parse_wx};
use wm_core::trading::IntentKind;
use wm_core::units::{Price, Usd, decimal_serde};
use wm_core::weather::{Observation, TenMinuteExtras, TenMinuteObservation, UpstreamReport};

pub use crate::forecast::ForecastDay;
pub use day::taker_size;
pub use wallets::{WalletBook, WalletScores, WalletStats};

/// Number of families.
pub const FAMILIES: usize = 65;

/// L26–L65: how long a held signal may be missing from evaluations
/// without its clock starting again.
pub const CONFIRM_GAP: Duration = Duration::seconds(15);

/// The families `research market` replays (L1–L25).
pub const REPLAYED: usize = 25;

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
    "L26_upstream_warmth",
    "L27_mixing_surge",
    "L28_ladder_dip",
    "L29_wind_slack",
    "L30_yesterday_anchor",
    "L31_upwind_clearing",
    "L32_gust_onset",
    "L33_whale_bid",
    "L34_stale_ask",
    "L35_post_high_drift",
    "L36_front_eta",
    "L37_pressure_fall",
    "L38_calm_tape_tails",
    "L39_pair_lock",
    "L40_tail_order",
    "L41_wet_ground",
    "L42_saturated_air",
    "L43_onshore_day",
    "L44_fresh_wallet",
    "L45_complement_gap",
    "L46_evening_discount",
    "L47_lottery_hour",
    "L48_self_contradiction",
    "L49_boundary_straddle",
    "L50_newsless_move",
    "L51_momentum_divergence",
    "L52_runup_fade",
    "L53_curve_projection",
    "L54_overround",
    "L55_edge_hover",
    "L56_station_spread",
    "L57_laggard_bucket",
    "L58_round_number",
    "L59_imbalance_scalp",
    "L60_gap_filler",
    "L61_speci_pulse",
    "L62_haze_lid",
    "L63_iceberg_seller",
    "L64_ask_vacuum",
    "L65_midnight_high",
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
    "Warm air upstream on its way",
    "Mixing surge: the dew point drops as the air warms",
    "A dip in the price ladder",
    "Wind slack under the sun",
    "Market anchored on yesterday's high",
    "Clearing sky upwind",
    "Gusts begin: thermals mixing",
    "A whale's bid wall",
    "A stale ask while KNMI warms",
    "Under-reaction after a new high",
    "A front upstream, due before the peak",
    "Falling pressure",
    "Calm-tape tail maker",
    "Pair lock: the high and the next degree together",
    "The tail out of order after the peak",
    "Wet ground from yesterday's rain",
    "Saturated air",
    "An onshore day",
    "Fade the unknown longshot buyer",
    "NO cheaper than the other buckets' YES",
    "Evening capital-recycling discount",
    "Evening lottery tickets",
    "The market contradicts itself",
    "Forecast on a rounding edge",
    "A move without news",
    "Prices against the thermometer",
    "Pre-report run-up KNMI does not back",
    "The day's projected curve",
    "Overround squeeze",
    "Hovering on the rounding edge",
    "Stations disagree",
    "The high's bucket lags its neighbour",
    "Round-number premium",
    "Order-book imbalance round trip",
    "Fill the gap in a thin ask ladder",
    "A special report's improvement",
    "Haze lid",
    "A hidden seller",
    "Sellers withdraw",
    "Midnight high",
];

/// The families whose main rule reads KNMI's ten-minute readings (L3 only
/// for its exit): without `WM_KNMI_API_KEY` they stay idle, and L3 enters
/// as F does without its exit.
pub const KNMI_FAMILIES: [u8; 30] = [
    1, 2, 3, 4, 5, 6, 7, 18, 21, 23, 24, 25, 27, 29, 32, 34, 35, 37, 42, 43, 46, 47, 50, 51, 52,
    53, 55, 56, 57, 65,
];

/// How hard a family of L26–L65 bets, which sets its stake
/// ([`LabConfig::aggressive_notional`] …): cheap YES on a rise
/// (aggressive), NO of what the weather or the market's own structure rules
/// out (defensive), and both sides of the market's mistakes (balanced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Aggressive,
    Balanced,
    Defensive,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Aggressive => "aggressive",
            Profile::Balanced => "balanced",
            Profile::Defensive => "defensive",
        }
    }
}

/// The risk profile of a family of L26–L65 (`None` for L1–L25, which
/// trade [`LabConfig::notional`]).
pub fn profile(family: u8) -> Option<Profile> {
    match family {
        26..=35 | 61 | 64 => Some(Profile::Aggressive),
        36..=48 | 62 => Some(Profile::Defensive),
        49..=60 | 63 | 65 => Some(Profile::Balanced),
        _ => None,
    }
}

/// A family's code in the configuration and on the dashboard: `L1` … `L65`.
pub fn code(family: u8) -> String {
    format!("L{family}")
}

/// The family (1–65) of a lab strategy id or code (`L4_cooling_lock`, `L4`).
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

/// Whether family `family` reads KNMI's ten-minute readings
/// ([`KNMI_FAMILIES`]).
pub fn reads_knmi(family: u8) -> bool {
    KNMI_FAMILIES.contains(&family)
}

/// The engine id of family `family` (1–65).
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
    /// Cost of one trade of L1–L25 at its price (L3 buys F's shares).
    #[serde(with = "decimal_serde::usd")]
    pub notional: Usd,
    /// Cost of one trade of an aggressive family of L26–L65 ([`Profile`]).
    #[serde(with = "decimal_serde::usd")]
    pub aggressive_notional: Usd,
    /// … of a balanced one …
    #[serde(with = "decimal_serde::usd")]
    pub balanced_notional: Usd,
    /// … and of a defensive one.
    #[serde(with = "decimal_serde::usd")]
    pub defensive_notional: Usd,
    /// L26–L65: a signal trades only once it has held this long (on every
    /// evaluation in between), so no family depends on reacting faster than
    /// the bot can: 250 ms to Polymarket and 100 ms from decision to order.
    pub confirm_seconds: i64,
    /// L26–L65: a taker buy's limit lies this far above the ask it saw, so
    /// that a book that moved during the order's way still fills; its
    /// expected profit is counted at the limit.
    #[serde(with = "decimal_serde::price")]
    pub taker_tolerance: Price,
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
            aggressive_notional: Usd::from_whole(15),
            balanced_notional: Usd::from_whole(20),
            defensive_notional: Usd::from_whole(25),
            confirm_seconds: 60,
            taker_tolerance: Price::saturating_from_micros(10_000),
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

    /// The cost of one trade of family `family` at its price.
    pub fn stake(&self, family: u8) -> Usd {
        match profile(family) {
            None => self.notional,
            Some(Profile::Aggressive) => self.aggressive_notional,
            Some(Profile::Balanced) => self.balanced_notional,
            Some(Profile::Defensive) => self.defensive_notional,
        }
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

impl WxReport {
    /// An upstream airport's report (its weather groups, temperature and
    /// dew point).
    pub fn from_upstream(r: &UpstreamReport) -> Self {
        Self {
            observed_at: r.observed_at,
            known_at: r.received_at,
            temp_tenths: r.temperature.map(|t| t.tenths()),
            dew_tenths: r.dewpoint.map(|t| t.tenths()),
            wx: parse_wx(&r.raw_text),
        }
    }
}

/// An upstream airport's reports (L26, L31, L36).
#[derive(Debug, Clone, Copy)]
pub struct UpstreamReadings<'a> {
    /// ICAO code.
    pub icao: &'a str,
    /// Bearing from the market's station, degrees true.
    pub bearing_deg: f64,
    pub distance_km: f64,
    /// Its METARs of the last hours, oldest first.
    pub reports: &'a [WxReport],
}

/// One bucket's YES book at a sample of the price ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct LadderQuote {
    pub token: TokenId,
    /// Best bid `(price, shares)`.
    pub bid: Option<(f64, f64)>,
    /// Best ask `(price, shares)`.
    pub ask: Option<(f64, f64)>,
    /// Shares bid and offered over the levels the engine keeps.
    pub bid_depth: f64,
    pub ask_depth: f64,
}

impl LadderQuote {
    /// The YES book of `book`.
    pub fn of(book: &OrderBook) -> Self {
        let lvl =
            |l: Option<wm_core::market::BookLevel>| l.map(|l| (l.price.as_f64(), l.size.as_f64()));
        Self {
            token: book.token.clone(),
            bid: lvl(book.best_bid()),
            ask: lvl(book.best_ask()),
            bid_depth: book.bids.iter().map(|l| l.size.as_f64()).sum(),
            ask_depth: book.asks.iter().map(|l| l.size.as_f64()).sum(),
        }
    }

    /// The midpoint, else the ask, else the bid (as [`day`]'s YES price).
    pub fn mid(&self) -> Option<f64> {
        match (self.bid, self.ask) {
            (Some((b, _)), Some((a, _))) if a >= b => Some(f64::midpoint(a, b)),
            (_, Some((a, _))) => Some(a),
            (Some((b, _)), None) => Some(b),
            (None, None) => None,
        }
    }
}

/// The YES books of a market's buckets at one moment: the engine keeps one
/// a minute of today's market for the lab's ladder rules.
#[derive(Debug, Clone, PartialEq)]
pub struct LadderSample {
    pub at: DateTime<Utc>,
    /// One per bucket whose YES book was fresh.
    pub quotes: Vec<LadderQuote>,
}

impl LadderSample {
    /// The fresh YES books (≤ `max_age_ms` old) of `market` at `now`.
    pub fn of(
        market: &DailyTemperatureMarket,
        books: &HashMap<TokenId, OrderBook>,
        now: DateTime<Utc>,
        max_age_ms: i64,
    ) -> Self {
        Self {
            at: now,
            quotes: market
                .outcomes
                .iter()
                .filter_map(|o| books.get(&o.yes_token))
                .filter(|b| b.age_ms(now) <= max_age_ms)
                .map(LadderQuote::of)
                .collect(),
        }
    }

    pub fn quote(&self, yes_token: &TokenId) -> Option<&LadderQuote> {
        self.quotes.iter().find(|q| &q.token == yes_token)
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
    /// When today's forecast may be used at the earliest (the knowledge
    /// rule's ready time), also while `forecast` is still absent.
    pub forecast_ready: Option<DateTime<Utc>>,
    /// Yesterday's observed high minus its forecast maximum (tenths °C).
    pub yesterday_error_tenths: Option<i32>,
    /// Today's market's taker trades, oldest first.
    pub takers: &'a [TakerTrade],
    /// Takers' records on settled days; `None` until they are loaded.
    pub wallets: Option<&'a WalletScores>,
    /// The station's latitude and longitude (clear-sky radiation).
    pub position: Option<(f64, f64)>,
    /// The station's other KNMI ten-minute parameters, oldest first.
    pub knmi_extra: &'a [TenMinuteExtras],
    /// Airports upstream with their recent METARs.
    pub upstream: &'a [UpstreamReadings<'a>],
    /// Today's market's price ladder, a sample a minute, oldest first.
    pub ladder: &'a [LadderSample],
    /// Yesterday's METARs (local day), oldest first.
    pub yesterday_reports: &'a [WxReport],
}

impl LabInputs<'static> {
    /// No lab input at all.
    pub const EMPTY: Self = Self {
        knmi: &[],
        neighbours: &[],
        reports: &[],
        forecast: None,
        forecast_ready: None,
        yesterday_error_tenths: None,
        takers: &[],
        wallets: None,
        position: None,
        knmi_extra: &[],
        upstream: &[],
        ladder: &[],
        yesterday_reports: &[],
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
    /// L26–L65: since when each token's signal has held, and when it was
    /// last seen ([`LabConfig::confirm_seconds`]).
    armed: HashMap<TokenId, (DateTime<Utc>, DateTime<Utc>)>,
    /// L59: when its position was first seen (its round trip's clock).
    entered: Option<(TokenId, DateTime<Utc>)>,
}

impl LabStrategy {
    /// Family `family` (1–65) with `cfg`; `f` is strategy F's configuration
    /// (L3 enters as F does).
    ///
    /// # Panics
    ///
    /// When `family` is not 1–65.
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
            armed: HashMap::new(),
            entered: None,
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

    fn reads_nowcast(&self) -> bool {
        reads_knmi(self.family)
    }

    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput {
        let mut out = StrategyOutput::default();
        let Some(day) = day::Day::new(ctx, &self.cfg, &self.id, self.cfg.stake(self.family)) else {
            self.armed.clear();
            return out;
        };
        let v = self.variant;
        if self.family > REPLAYED as u8 {
            let day = day.with_tolerance(self.cfg.taker_tolerance);
            let once = !MANY_A_DAY.contains(&self.family) && day.traded_today();
            evaluate_new(self.family, &day, v, &mut self.entered, &mut out);
            if once {
                one_a_day(&mut out);
            }
            self.confirm(ctx.now, &mut out);
            return out;
        }
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

/// The families of L26–L65 that may open more than once a day: the makers,
/// whose quotes rest and expire before each report (L38, L44, L47, L60),
/// and L39's two legs. The others open one position a day, as the
/// replayed rules do; L59 manages its own round trip.
const MANY_A_DAY: [u8; 5] = [38, 39, 44, 47, 60];

/// A family that already traded today's market opens nothing more today.
fn one_a_day(out: &mut StrategyOutput) {
    let opening: Vec<TokenId> = out
        .proposals
        .iter()
        .filter(|p| p.kind == IntentKind::Open)
        .map(|p| p.token.clone())
        .collect();
    out.proposals.retain(|p| p.kind != IntentKind::Open);
    for e in out
        .evaluations
        .iter_mut()
        .filter(|e| e.signal && opening.contains(&e.token))
    {
        e.signal = false;
        e.blockers
            .push("one trade a day: this book already traded today's market".into());
    }
}

/// L26–L65 (`entered`: L59's position clock).
fn evaluate_new(
    family: u8,
    day: &day::Day<'_, '_>,
    v: bool,
    entered: &mut Option<(TokenId, DateTime<Utc>)>,
    out: &mut StrategyOutput,
) {
    match family {
        26 => upstream::warmth(day, v, out),
        27 => air::mixing_surge(day, v, out),
        28 => ladder::dip(day, v, out),
        29 => air::wind_slack(day, v, out),
        30 => memory::yesterday_anchor(day, v, out),
        31 => upstream::clearing(day, v, out),
        32 => air::gust_onset(day, v, out),
        33 => ladder::whale_bid(day, v, out),
        34 => ladder::stale_ask(day, v, out),
        35 => reaction::post_high_drift(day, v, out),
        36 => upstream::front_eta(day, v, out),
        37 => air::pressure_fall(day, v, out),
        38 => tape::calm_tails(day, v, out),
        39 => ladder::pair_lock(day, v, out),
        40 => ladder::tail_order(day, v, out),
        41 => memory::wet_ground(day, v, out),
        42 => air::saturated(day, v, out),
        43 => air::onshore_day(day, v, out),
        44 => tape::fresh_wallet(day, v, out),
        45 => ladder::complement_gap(day, v, out),
        46 => reaction::evening_discount(day, v, out),
        47 => tape::lottery_hour(day, v, out),
        48 => ladder::self_contradiction(day, v, out),
        49 => memory::boundary_straddle(day, v, out),
        50 => reaction::newsless_move(day, v, out),
        51 => reaction::momentum_divergence(day, v, out),
        52 => reaction::runup_fade(day, v, out),
        53 => reaction::curve_projection(day, v, out),
        54 => ladder::overround(day, v, out),
        55 => reaction::edge_hover(day, v, out),
        56 => reaction::station_spread(day, v, out),
        57 => reaction::laggard(day, v, out),
        58 => ladder::round_number(day, v, out),
        59 => *entered = ladder::imbalance_scalp(day, v, entered.take(), out),
        60 => ladder::gap_filler(day, v, out),
        61 => reaction::speci_pulse(day, v, out),
        62 => reaction::haze_lid(day, v, out),
        63 => tape::iceberg(day, v, out),
        64 => ladder::ask_vacuum(day, v, out),
        65 => memory::midnight_high(day, v, out),
        _ => {}
    }
}

impl LabStrategy {
    /// Hold back an opening proposal of L26–L65 until its signal has held
    /// for [`LabConfig::confirm_seconds`]: the bot learns of a report about
    /// two minutes after the market and needs 350 ms to place an order, so
    /// a family that has to be first is not one to run. A signal missing
    /// from evaluations for up to [`CONFIRM_GAP`] (a book refreshing, one
    /// level at a time) keeps its clock; longer, and it starts again. Exits
    /// are not held.
    fn confirm(&mut self, now: DateTime<Utc>, out: &mut StrategyOutput) {
        let hold = Duration::seconds(self.cfg.confirm_seconds.max(0));
        let live: Vec<TokenId> = out
            .proposals
            .iter()
            .filter(|p| p.kind == IntentKind::Open)
            .map(|p| p.token.clone())
            .collect();
        self.armed
            .retain(|t, (_, seen)| live.contains(t) || now - *seen <= CONFIRM_GAP);
        for t in &live {
            self.armed
                .entry(t.clone())
                .and_modify(|(_, seen)| *seen = now)
                .or_insert((now, now));
        }
        let mut kept = Vec::with_capacity(out.proposals.len());
        for p in std::mem::take(&mut out.proposals) {
            let since = self.armed.get(&p.token).map_or(now, |(since, _)| *since);
            if p.kind != IntentKind::Open || now - since >= hold {
                kept.push(p);
                continue;
            }
            if let Some(e) = out
                .evaluations
                .iter_mut()
                .find(|e| e.token == p.token && e.signal)
            {
                e.signal = false;
                e.blockers.push(format!(
                    "signal since {} UTC: trades once it has held {} s (no race on speed)",
                    since.format("%H:%M:%S"),
                    hold.num_seconds()
                ));
            }
        }
        out.proposals = kept;
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
        _ => summaries::summary(family, variant),
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
        assert_eq!(family_of("L26"), Some(26));
        assert_eq!(family_of("L65"), Some(65));
        assert_eq!(family_of("L66"), None);
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
        assert_eq!(all.len(), FAMILIES - 2);
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
