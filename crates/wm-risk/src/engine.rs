//! The risk engine: every trade intent passes here. Fail closed.

use crate::exposure::{Leg, event_exposure};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use wm_core::health::ProviderHealthState;
use wm_core::ids::{
    ClientOrderId, DecisionId, EventSlug, LocationId, StationId, StrategyId, TokenId,
};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side, TemperatureBucket};
use wm_core::portfolio::PositionBook;
use wm_core::trading::{IntentKind, RunMode, TimeInForce, TradeIntent};
use wm_core::units::{Price, Rounding, Shares, Usd, decimal_serde, notional};

/// Risk limits. Money/prices are exact decimal strings in configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    #[serde(with = "decimal_serde::usd")]
    pub position_size_usd: Usd,
    #[serde(with = "decimal_serde::usd")]
    pub global_max_exposure_usd: Usd,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_market_exposure_usd: Option<Usd>,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_location_exposure_usd: Option<Usd>,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_strategy_exposure_usd: Option<Usd>,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_daily_new_exposure_usd: Option<Usd>,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_daily_loss_usd: Option<Usd>,
    #[serde(with = "decimal_serde::price")]
    pub max_spread: Price,
    #[serde(with = "decimal_serde::price")]
    pub max_price: Price,
    #[serde(with = "decimal_serde::price")]
    pub min_price: Price,
    pub max_weather_age_minutes: i64,
    /// Largest tolerated hole in the local day's observation series (from local
    /// midnight to the latest report). A day with a hole may have missed the
    /// true high, so it is never traded (fail closed).
    #[serde(default = "default_max_observation_gap_minutes")]
    pub max_observation_gap_minutes: i64,
    pub max_book_age_ms: i64,
    /// After a correction to the current day's data, block new weather positions this long.
    pub correction_cooldown_minutes: i64,
    pub max_orders_per_minute: u32,
    /// Live requires a human-approved resolution spec; paper may use machine-tradable specs.
    pub require_approved_resolution_spec: bool,
    /// Caps of single strategies (by strategy id) that replace the default
    /// per-position, per-market and per-strategy caps — for a strategy that
    /// buys a fixed number of shares. The portfolio-wide caps (global,
    /// location, daily) still count every strategy together.
    #[serde(default)]
    pub strategy_caps: BTreeMap<String, StrategyCaps>,
}

/// One strategy's own caps (see [`RiskConfig::strategy_caps`]); an unset
/// cap falls back to the default one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyCaps {
    #[serde(with = "decimal_serde::usd")]
    pub position_size_usd: Usd,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_market_exposure_usd: Option<Usd>,
    #[serde(with = "decimal_serde::opt_usd", default)]
    pub max_strategy_exposure_usd: Option<Usd>,
    /// Widest book spread at which the strategy may open a position (the
    /// default `max_spread` otherwise): a strategy that trades just before
    /// a report, when makers widen their quotes, may need more room.
    #[serde(with = "decimal_serde::opt_price", default)]
    pub max_spread: Option<Price>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            position_size_usd: Usd::from_whole(10),
            global_max_exposure_usd: Usd::from_whole(100),
            max_market_exposure_usd: Some(Usd::from_whole(30)),
            max_location_exposure_usd: Some(Usd::from_whole(50)),
            max_strategy_exposure_usd: Some(Usd::from_whole(60)),
            max_daily_new_exposure_usd: Some(Usd::from_whole(60)),
            max_daily_loss_usd: Some(Usd::from_whole(30)),
            max_spread: Price::saturating_from_micros(50_000),
            max_price: Price::saturating_from_micros(990_000),
            min_price: Price::saturating_from_micros(10_000),
            max_weather_age_minutes: 40,
            max_observation_gap_minutes: default_max_observation_gap_minutes(),
            max_book_age_ms: 15_000,
            correction_cooldown_minutes: 10,
            max_orders_per_minute: 6,
            require_approved_resolution_spec: false,
            strategy_caps: BTreeMap::new(),
        }
    }
}

/// Routine METARs are half-hourly: one missed report plus a margin.
pub const fn default_max_observation_gap_minutes() -> i64 {
    75
}

/// Configuration errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid risk config: {0}")]
pub struct RiskConfigError(pub String);

impl RiskConfig {
    pub fn validate(&self) -> Result<(), RiskConfigError> {
        let pos = self.position_size_usd;
        if pos <= Usd::ZERO || self.global_max_exposure_usd <= Usd::ZERO {
            return Err(RiskConfigError("sizes must be positive".into()));
        }
        if pos > self.global_max_exposure_usd {
            return Err(RiskConfigError(
                "position_size_usd exceeds global_max_exposure_usd".into(),
            ));
        }
        if self.min_price >= self.max_price {
            return Err(RiskConfigError("min_price must be < max_price".into()));
        }
        if self.max_orders_per_minute == 0 {
            return Err(RiskConfigError("max_orders_per_minute must be ≥ 1".into()));
        }
        if self.max_observation_gap_minutes < 30 {
            return Err(RiskConfigError(
                "max_observation_gap_minutes must be ≥ 30 (half-hourly reports)".into(),
            ));
        }
        for (strategy, caps) in &self.strategy_caps {
            if caps.position_size_usd <= Usd::ZERO
                || caps.position_size_usd > self.global_max_exposure_usd
            {
                return Err(RiskConfigError(format!(
                    "strategy_caps.{strategy}.position_size_usd must be positive and ≤ global_max_exposure_usd"
                )));
            }
        }
        Ok(())
    }

    /// The per-position cap of `strategy`.
    pub fn position_size_for(&self, strategy: &StrategyId) -> Usd {
        self.strategy_caps
            .get(strategy.as_str())
            .map_or(self.position_size_usd, |c| c.position_size_usd)
    }

    /// The per-event worst-case cap checked on `strategy`'s orders (the
    /// event's legs of every strategy count).
    pub fn market_cap_for(&self, strategy: &StrategyId) -> Option<Usd> {
        self.strategy_caps
            .get(strategy.as_str())
            .and_then(|c| c.max_market_exposure_usd)
            .or(self.max_market_exposure_usd)
    }

    /// The cap on `strategy`'s own capital.
    pub fn strategy_cap_for(&self, strategy: &StrategyId) -> Option<Usd> {
        self.strategy_caps
            .get(strategy.as_str())
            .and_then(|c| c.max_strategy_exposure_usd)
            .or(self.max_strategy_exposure_usd)
    }

    /// The widest spread at which `strategy` may open a position.
    pub fn max_spread_for(&self, strategy: &StrategyId) -> Price {
        self.strategy_caps
            .get(strategy.as_str())
            .and_then(|c| c.max_spread)
            .unwrap_or(self.max_spread)
    }
}

/// Identifies each pre-trade check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckId {
    KillSwitch,
    Mode,
    ResearchOnly,
    Compliance,
    Storage,
    Execution,
    WeatherHealth,
    WeatherFreshness,
    WeatherCoverage,
    CorrectionCooldown,
    MarketData,
    MarketStatus,
    PriceBounds,
    Tick,
    Spread,
    Liquidity,
    ResolutionSpec,
    Partition,
    Duplicate,
    PositionSize,
    MinSize,
    GlobalExposure,
    MarketExposure,
    LocationExposure,
    StrategyExposure,
    DailyNewExposure,
    DailyLoss,
    OrderRate,
    Oversell,
}

/// One failed check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskRejection {
    pub check: CheckId,
    pub detail: String,
}

/// An intent that passed every check. Only this crate can construct it, so
/// the execution layer cannot be handed an unchecked order (type-level gate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedIntent {
    intent: TradeIntent,
    client_order_id: ClientOrderId,
    approved_at: DateTime<Utc>,
    post_trade_global_exposure: Usd,
}

impl ApprovedIntent {
    pub fn intent(&self) -> &TradeIntent {
        &self.intent
    }

    pub fn client_order_id(&self) -> &ClientOrderId {
        &self.client_order_id
    }

    pub fn approved_at(&self) -> DateTime<Utc> {
        self.approved_at
    }

    pub fn post_trade_global_exposure(&self) -> Usd {
        self.post_trade_global_exposure
    }
}

/// Outcome of a risk evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RiskDecision {
    Approved(ApprovedIntent),
    Rejected {
        intent: TradeIntent,
        reasons: Vec<RiskRejection>,
    },
}

impl RiskDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, RiskDecision::Approved(_))
    }
}

/// Freshness/health of the observation source for a station.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherStatus {
    pub station: StationId,
    pub last_observation_at: Option<DateTime<Utc>>,
    /// Best health among the station's observation sources.
    pub health: ProviderHealthState,
    /// Most recent correction/revision affecting the station.
    pub last_correction_at: Option<DateTime<Utc>>,
    /// Largest gap (minutes) in the current local day's series, counted from
    /// local midnight to the latest observation. `None` = no observation today.
    pub max_gap_minutes: Option<i64>,
}

/// A live (unfilled) order, counted as exposure if it is a buy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrderView {
    pub client_order_id: ClientOrderId,
    pub token: TokenId,
    pub event_slug: EventSlug,
    pub location: LocationId,
    pub strategy: StrategyId,
    pub side: Side,
    pub outcome_side: OutcomeSide,
    pub bucket: TemperatureBucket,
    pub remaining: Shares,
    pub limit_price: Price,
}

/// Portfolio and market context.
pub struct PortfolioView<'a> {
    pub positions: &'a PositionBook,
    pub open_orders: &'a [OpenOrderView],
    pub markets: &'a HashMap<EventSlug, DailyTemperatureMarket>,
    /// Strategy that opened each held token (for per-strategy limits).
    pub position_strategy: &'a HashMap<TokenId, StrategyId>,
}

/// Everything the risk engine needs for one decision.
pub struct RiskInputs<'a> {
    pub now: DateTime<Utc>,
    pub mode: RunMode,
    pub kill_switch: Option<&'a str>,
    pub compliance_ok: bool,
    pub storage_ok: bool,
    pub execution_ok: bool,
    pub weather: Option<&'a WeatherStatus>,
    pub market: &'a DailyTemperatureMarket,
    pub book: Option<&'a OrderBook>,
    pub portfolio: PortfolioView<'a>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DailyCounters {
    date: Option<NaiveDate>,
    new_exposure: Usd,
    realized_pnl: Usd,
}

/// The risk engine.
#[derive(Debug, Clone)]
pub struct RiskEngine {
    config: RiskConfig,
    daily: DailyCounters,
    recent_orders: VecDeque<DateTime<Utc>>,
    seen_decisions: HashSet<(DecisionId, TokenId)>,
    run_short: String,
}

/// Aggregated exposure for dashboards.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExposureSummary {
    pub global_worst_case: Usd,
    pub capital_deployed: Usd,
    pub per_event: Vec<(EventSlug, Usd)>,
    pub per_location: Vec<(LocationId, Usd)>,
}

impl RiskEngine {
    pub fn new(config: RiskConfig, run: &wm_core::ids::RunId) -> Self {
        Self {
            config,
            daily: DailyCounters::default(),
            recent_orders: VecDeque::new(),
            seen_decisions: HashSet::new(),
            run_short: run.short(),
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    /// Record realized PnL (fills/settlements) for the daily loss limit.
    pub fn record_realized_pnl(&mut self, pnl: Usd, now: DateTime<Utc>) {
        self.roll_day(now);
        self.daily.realized_pnl += pnl;
    }

    pub fn daily_realized_pnl(&self) -> Usd {
        self.daily.realized_pnl
    }

    pub fn daily_new_exposure(&self) -> Usd {
        self.daily.new_exposure
    }

    /// After a restart: count the cost of today's (UTC) opening orders of
    /// earlier runs toward the daily new-exposure limit, as their approval
    /// did. Rolls the day first, so restored realized P&L of an earlier day
    /// is dropped.
    pub fn restore_daily_new_exposure(&mut self, cost: Usd, now: DateTime<Utc>) {
        self.roll_day(now);
        self.daily.new_exposure += cost;
    }

    /// An opening buy ended with `cost` (at its limit) unfilled — expired,
    /// cancelled or rejected: that part never became exposure, so it no
    /// longer counts toward today's new-exposure limit. Only when the order
    /// was approved on the counter's (UTC) day; never below zero.
    pub fn release_daily_new_exposure(
        &mut self,
        cost: Usd,
        approved_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) {
        self.roll_day(now);
        if self.daily.date == Some(approved_at.date_naive()) {
            self.daily.new_exposure = (self.daily.new_exposure - cost).max(Usd::ZERO);
        }
    }

    fn roll_day(&mut self, now: DateTime<Utc>) {
        let d = now.date_naive();
        if self.daily.date != Some(d) {
            self.daily = DailyCounters {
                date: Some(d),
                ..DailyCounters::default()
            };
        }
    }

    /// Legs per event: positions plus pending buys (treated as filled at limit).
    fn legs_by_event(
        p: &PortfolioView<'_>,
    ) -> HashMap<EventSlug, Vec<(Leg, LocationId, Option<StrategyId>)>> {
        let mut map: HashMap<EventSlug, Vec<(Leg, LocationId, Option<StrategyId>)>> =
            HashMap::new();
        for pos in p.positions.open_positions() {
            let loc = p
                .markets
                .get(&pos.instrument.event_slug)
                .map(|m| m.location.clone());
            let Some(loc) = loc else { continue };
            map.entry(pos.instrument.event_slug.clone())
                .or_default()
                .push((
                    Leg {
                        bucket: pos.instrument.bucket,
                        side: pos.instrument.outcome_side,
                        shares: pos.shares,
                        cost: pos.cost_basis,
                    },
                    loc,
                    p.position_strategy.get(&pos.instrument.token).cloned(),
                ));
        }
        for o in p.open_orders.iter().filter(|o| o.side == Side::Buy) {
            map.entry(o.event_slug.clone()).or_default().push((
                Leg {
                    bucket: o.bucket,
                    side: o.outcome_side,
                    shares: o.remaining,
                    cost: notional(o.limit_price, o.remaining, Rounding::Up),
                },
                o.location.clone(),
                Some(o.strategy.clone()),
            ));
        }
        map
    }

    /// Current exposure summary.
    pub fn exposure(&self, p: &PortfolioView<'_>) -> ExposureSummary {
        let legs = Self::legs_by_event(p);
        let mut s = ExposureSummary::default();
        let mut per_loc: HashMap<LocationId, Usd> = HashMap::new();
        for (slug, ls) in &legs {
            let Some(m) = p.markets.get(slug) else {
                continue;
            };
            let only: Vec<Leg> = ls.iter().map(|(l, _, _)| *l).collect();
            let e = event_exposure(m, &only);
            s.global_worst_case += e.worst_case_loss;
            s.capital_deployed += e.capital;
            s.per_event.push((slug.clone(), e.worst_case_loss));
            *per_loc.entry(m.location.clone()).or_insert(Usd::ZERO) += e.worst_case_loss;
        }
        s.per_event.sort_by(|a, b| a.0.cmp(&b.0));
        let mut pl: Vec<_> = per_loc.into_iter().collect();
        pl.sort_by(|a, b| a.0.cmp(&b.0));
        s.per_location = pl;
        s
    }

    /// Evaluate one intent. All failing checks are reported (not just the first).
    pub fn evaluate(&mut self, intent: TradeIntent, inp: &RiskInputs<'_>) -> RiskDecision {
        self.roll_day(inp.now);
        let mut r: Vec<RiskRejection> = Vec::new();
        let mut fail = |check: CheckId, detail: String| r.push(RiskRejection { check, detail });
        let cfg = &self.config;
        let opening = intent.kind == IntentKind::Open;

        // -- System gates ------------------------------------------------------
        if let Some(reason) = inp.kill_switch {
            fail(
                CheckId::KillSwitch,
                format!("kill switch engaged: {reason}"),
            );
        }
        if inp.mode == RunMode::Live {
            fail(
                CheckId::Mode,
                "live trading is not enabled in this build (Phase 14 gate)".into(),
            );
        }
        if intent.research_only && inp.mode != RunMode::Backtest {
            fail(
                CheckId::ResearchOnly,
                "research-only strategy outside backtest".into(),
            );
        }
        if inp.mode == RunMode::Live && !inp.compliance_ok {
            fail(
                CheckId::Compliance,
                "jurisdiction/compliance gate not satisfied".into(),
            );
        }
        if inp.mode != RunMode::Backtest && !inp.storage_ok {
            fail(CheckId::Storage, "audit storage unavailable".into());
        }
        if !inp.execution_ok {
            fail(CheckId::Execution, "execution venue unhealthy".into());
        }

        // -- Weather gates (new weather-dependent positions only) ---------------
        if opening && intent.weather_dependent {
            match inp.weather {
                None => fail(
                    CheckId::WeatherHealth,
                    "no weather status for station".into(),
                ),
                Some(w) => {
                    if !w.health.allows_new_weather_positions() {
                        fail(
                            CheckId::WeatherHealth,
                            format!("observation source {}", w.health),
                        );
                    }
                    match w.last_observation_at {
                        None => fail(CheckId::WeatherFreshness, "no observation yet".into()),
                        Some(t) => {
                            // Exact comparison: 40 min 30 s is older than a 40-minute limit.
                            let age = inp.now - t;
                            if age > Duration::minutes(cfg.max_weather_age_minutes) {
                                let age = age.num_seconds().div_euclid(60);
                                fail(
                                    CheckId::WeatherFreshness,
                                    format!(
                                        "latest observation {age} min old > {}",
                                        cfg.max_weather_age_minutes
                                    ),
                                );
                            }
                            if t > inp.now + Duration::minutes(5) {
                                fail(
                                    CheckId::WeatherFreshness,
                                    "observation time in the future".into(),
                                );
                            }
                        }
                    }
                    match w.max_gap_minutes {
                        None => fail(
                            CheckId::WeatherCoverage,
                            "no observation for the local day yet".into(),
                        ),
                        Some(g) if g > cfg.max_observation_gap_minutes => {
                            fail(
                                CheckId::WeatherCoverage,
                                format!(
                                    "day series has a {g} min gap > {} (high may be missed)",
                                    cfg.max_observation_gap_minutes
                                ),
                            );
                        }
                        Some(_) => {}
                    }
                    if let Some(c) = w.last_correction_at
                        && inp.now - c < Duration::minutes(cfg.correction_cooldown_minutes)
                    {
                        fail(CheckId::CorrectionCooldown, "recent data correction".into());
                    }
                }
            }
        }

        // -- Market data and market status ---------------------------------------
        match inp.book {
            None => fail(CheckId::MarketData, "no order book".into()),
            Some(b) => {
                let age = b.age_ms(inp.now);
                if opening && age > cfg.max_book_age_ms {
                    fail(
                        CheckId::MarketData,
                        format!("order book {age} ms old > {}", cfg.max_book_age_ms),
                    );
                }
                if b.token != intent.token {
                    fail(CheckId::MarketData, "book/token mismatch".into());
                }
                if opening {
                    let max_spread = cfg.max_spread_for(&intent.strategy);
                    match b.spread() {
                        None => fail(CheckId::Spread, "one-sided book".into()),
                        Some(sp) if sp > max_spread => {
                            fail(CheckId::Spread, format!("spread {sp} > {max_spread}"))
                        }
                        _ => {}
                    }
                    // Marketable orders need displayed depth now; passive orders
                    // (GTC/GTD) rest below the ask by design.
                    let marketable = matches!(intent.tif, TimeInForce::Fak | TimeInForce::Fok);
                    if intent.side == Side::Buy && marketable {
                        let (depth, _) = b.ask_depth_up_to(intent.limit_price);
                        if depth < intent.shares {
                            fail(
                                CheckId::Liquidity,
                                format!(
                                    "ask depth {depth} < {} shares at ≤ {}",
                                    intent.shares, intent.limit_price
                                ),
                            );
                        }
                    }
                }
                if !intent.limit_price.is_on_tick(b.tick_size) {
                    fail(
                        CheckId::Tick,
                        format!("price {} not on tick {}", intent.limit_price, b.tick_size),
                    );
                }
            }
        }
        let outcome = inp
            .market
            .outcomes
            .iter()
            .find(|o| o.yes_token == intent.token || o.no_token == intent.token);
        match outcome {
            None => fail(CheckId::MarketStatus, "token not in market".into()),
            Some(o) => {
                if o.closed || !o.accepting_orders || inp.market.closed {
                    fail(
                        CheckId::MarketStatus,
                        "market closed or not accepting orders".into(),
                    );
                }
                if intent.shares < o.min_order_size {
                    fail(
                        CheckId::MinSize,
                        format!(
                            "{} shares < market minimum {}",
                            intent.shares, o.min_order_size
                        ),
                    );
                }
            }
        }
        if let Some(end) = inp.market.end_time
            && inp.now >= end
        {
            fail(CheckId::MarketStatus, "market end time passed".into());
        }
        if opening && (intent.limit_price > cfg.max_price || intent.limit_price < cfg.min_price) {
            fail(
                CheckId::PriceBounds,
                format!(
                    "price {} outside [{}, {}]",
                    intent.limit_price, cfg.min_price, cfg.max_price
                ),
            );
        }
        if opening {
            if !inp.market.resolution.is_machine_tradable() {
                fail(
                    CheckId::ResolutionSpec,
                    "resolution rules not machine-tradable (review required)".into(),
                );
            }
            if cfg.require_approved_resolution_spec
                && inp.market.resolution.review != wm_core::resolution::SpecReviewStatus::Approved
            {
                fail(
                    CheckId::ResolutionSpec,
                    "resolution spec not human-approved".into(),
                );
            }
            if let Err(e) = inp.market.validate_partition() {
                fail(CheckId::Partition, e.to_string());
            }
        }

        // -- Duplicates and sizing ----------------------------------------------------
        if inp
            .portfolio
            .open_orders
            .iter()
            .any(|o| o.token == intent.token)
        {
            fail(
                CheckId::Duplicate,
                "live order already exists on token".into(),
            );
        }
        if self
            .seen_decisions
            .contains(&(intent.decision_id, intent.token.clone()))
        {
            fail(CheckId::Duplicate, "decision already processed".into());
        }
        let cost = notional(intent.limit_price, intent.shares, Rounding::Up);
        let position_cap = cfg.position_size_for(&intent.strategy);
        if opening && cost > position_cap {
            fail(
                CheckId::PositionSize,
                format!("cost {cost} > position size {position_cap}"),
            );
        }
        if intent.side == Side::Sell {
            let held = inp
                .portfolio
                .positions
                .get(&intent.token)
                .map_or(Shares::ZERO, |p| p.shares);
            if intent.shares > held {
                fail(
                    CheckId::Oversell,
                    format!("sell {} > held {held}", intent.shares),
                );
            }
        }

        // -- Exposure (scenario-based, including pending buys) ------------------------
        let mut post_global = Usd::ZERO;
        if opening && intent.side == Side::Buy {
            let mut legs = Self::legs_by_event(&inp.portfolio);
            let bucket = outcome.map(|o| o.bucket);
            if let Some(bucket) = bucket {
                legs.entry(intent.event_slug.clone()).or_default().push((
                    Leg {
                        bucket,
                        side: intent.outcome_side,
                        shares: intent.shares,
                        cost,
                    },
                    intent.location.clone(),
                    Some(intent.strategy.clone()),
                ));
            }
            let mut per_loc: HashMap<LocationId, Usd> = HashMap::new();
            let mut per_strategy: HashMap<StrategyId, Usd> = HashMap::new();
            let mut this_event = Usd::ZERO;
            for (slug, ls) in &legs {
                let market = if slug == &inp.market.event_slug {
                    Some(inp.market)
                } else {
                    inp.portfolio.markets.get(slug)
                };
                let Some(m) = market else { continue };
                let only: Vec<Leg> = ls.iter().map(|(l, _, _)| *l).collect();
                let e = event_exposure(m, &only);
                post_global += e.worst_case_loss;
                *per_loc.entry(m.location.clone()).or_insert(Usd::ZERO) += e.worst_case_loss;
                if slug == &intent.event_slug {
                    this_event = e.worst_case_loss;
                }
                for (l, _, s) in ls {
                    if let Some(s) = s {
                        *per_strategy.entry(s.clone()).or_insert(Usd::ZERO) += l.cost;
                    }
                }
            }
            if post_global > cfg.global_max_exposure_usd {
                fail(
                    CheckId::GlobalExposure,
                    format!(
                        "post-trade worst-case {post_global} > {}",
                        cfg.global_max_exposure_usd
                    ),
                );
            }
            if let Some(max) = cfg.market_cap_for(&intent.strategy)
                && this_event > max
            {
                fail(
                    CheckId::MarketExposure,
                    format!("event worst-case {this_event} > {max}"),
                );
            }
            if let Some(max) = cfg.max_location_exposure_usd {
                let l = per_loc.get(&intent.location).copied().unwrap_or(Usd::ZERO);
                if l > max {
                    fail(
                        CheckId::LocationExposure,
                        format!("location worst-case {l} > {max}"),
                    );
                }
            }
            if let Some(max) = cfg.strategy_cap_for(&intent.strategy) {
                let sx = per_strategy
                    .get(&intent.strategy)
                    .copied()
                    .unwrap_or(Usd::ZERO);
                if sx > max {
                    fail(
                        CheckId::StrategyExposure,
                        format!("strategy capital {sx} > {max}"),
                    );
                }
            }
            if let Some(max) = cfg.max_daily_new_exposure_usd
                && self.daily.new_exposure + cost > max
            {
                fail(
                    CheckId::DailyNewExposure,
                    format!(
                        "daily new exposure {} + {cost} > {max}",
                        self.daily.new_exposure
                    ),
                );
            }
            if let Some(max) = cfg.max_daily_loss_usd
                && self.daily.realized_pnl <= -max
            {
                fail(
                    CheckId::DailyLoss,
                    format!("daily loss {} reached limit {max}", self.daily.realized_pnl),
                );
            }
        }

        // -- Order rate -------------------------------------------------------------
        while let Some(&t) = self.recent_orders.front() {
            if inp.now - t >= Duration::seconds(60) {
                self.recent_orders.pop_front();
            } else {
                break;
            }
        }
        if self.recent_orders.len() as u32 >= cfg.max_orders_per_minute {
            fail(CheckId::OrderRate, "order rate limit reached".into());
        }

        if !r.is_empty() {
            return RiskDecision::Rejected { intent, reasons: r };
        }
        self.recent_orders.push_back(inp.now);
        self.seen_decisions
            .insert((intent.decision_id, intent.token.clone()));
        if opening {
            self.daily.new_exposure += cost;
        }
        let client_order_id = ClientOrderId::from_static_string(format!(
            "wm-{}-{}-{}",
            self.run_short,
            intent.decision_id.0,
            &intent.token.as_str()[..intent.token.as_str().len().min(10)]
        ));
        RiskDecision::Approved(ApprovedIntent {
            intent,
            client_order_id,
            approved_at: inp.now,
            post_trade_global_exposure: post_global,
        })
    }
}
