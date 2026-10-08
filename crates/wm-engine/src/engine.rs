//! The kernel implementation.

use crate::restore::{RestoreState, RestoreSummary};
use crate::snapshot::{
    EngineSnapshot, ForecastSnapshot, LabBookSnapshot, LabInputsSnapshot, LocationSnapshot,
    NeighbourSnapshot, ViewSnapshot,
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use wm_core::event::{
    EventEnvelope, ForecastEvent, OperatorCommand, OrderUpdateEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::forecast::ForecastProduct;
use wm_core::health::{ProviderHealthSnapshot, station_state};
use wm_core::ids::{
    DecisionId, EventSlug, LocationId, ProviderId, RunId, StationId, StrategyId, TokenId,
};
use wm_core::market::{DailyTemperatureMarket, OrderBook, TakerTrade, TradePrint};
use wm_core::portfolio::{Position, PositionBook};
use wm_core::resolution::ObservationFilter;
use wm_core::time::{local_date, local_day_bounds};
use wm_core::trading::{DecisionRecord, IntentKind, OrderStatus, RunMode, TradeIntent};
use wm_core::units::{Probability, Rounding, Usd, notional};
use wm_core::weather::{Observation, TenMinuteObservation};
use wm_execution::{Applied, OrderManager};
use wm_risk::{
    ApprovedIntent, OpenOrderView, PortfolioView, RiskConfig, RiskDecision, RiskEngine, RiskInputs,
    WeatherStatus,
};
use wm_strategy::lab::{NeighbourReadings, WalletScores, WxReport};
use wm_strategy::{
    BookConfirmedConfig, BookConfirmedHigh, BucketEvaluation, BuyNoAboveHigh, BuyNoConfig,
    BuyYesConfig, BuyYesFinalHigh, CertainConfig, CertainOutcomes, ForecastDay, KnmiNowcast,
    KnmiNowcastConfig, LabConfig, LabInputs, LabRiskConfig, MiddleFade, MiddleFadeConfig,
    MorningMaker, MorningMakerConfig, NextDegree, NextDegreeConfig, PeakConfig,
    PeakDetectionEngine, PeakSlotConfig, PeakSlotHigh, ProbabilityModel, Proposal, SplitUnwind,
    SplitUnwindConfig, Strategy, StrategyContext, TailSeller, TailSellerConfig,
    TemperatureStateEngine, UnwindConfig, UnwindEngine, ViewEvaluation, ViewKind, lab_strategies,
};

/// How long the lab's KNMI readings and METAR weather groups are kept.
const LAB_HISTORY: Duration = Duration::hours(36);

/// A settled market, its books and its finished orders are forgotten this
/// long after its day: the dashboard still shows yesterday's, the database
/// keeps everything, and the kernel's memory stays flat over months.
const SETTLED_HISTORY: Duration = Duration::days(2);

/// A neighbouring KNMI station of a location (the strategy lab's L24).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighbourStation {
    /// The station id its readings carry (its WMO number, e.g. `06215`).
    pub station: StationId,
    pub name: String,
    /// Bearing from the location's station, degrees true.
    pub bearing_deg: f64,
}

/// A location the engine trades.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineLocation {
    pub location: LocationId,
    pub station: StationId,
    pub timezone: Tz,
    pub peak: PeakConfig,
    /// Verified observation filter (after Phase 0). `None` ⇒ evaluate every
    /// candidate view from the market rules and act only if all agree.
    pub confirmed_filter: Option<ObservationFilter>,
    /// Minutes past each UTC hour of the station's routine reports (resting
    /// orders expire before them). Empty: unknown.
    #[serde(default)]
    pub routine_minutes: Vec<u8>,
    /// The station's latitude and longitude (the lab's clear sky).
    #[serde(default)]
    pub position: Option<(f64, f64)>,
    /// KNMI stations around it (the lab's upwind rule).
    #[serde(default)]
    pub neighbours: Vec<NeighbourStation>,
}

/// Engine configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub mode: RunMode,
    pub run_id: RunId,
    pub locations: Vec<EngineLocation>,
    pub risk: RiskConfig,
    pub buy_yes: BuyYesConfig,
    pub buy_no: BuyNoConfig,
    pub split_unwind: SplitUnwindConfig,
    /// Strategy D: outcomes the observations have decided.
    #[serde(default)]
    pub certain: CertainConfig,
    /// Strategy E: the high's bucket once the clock, the temperature and a
    /// shrinking book agree.
    #[serde(default)]
    pub book_confirmed: BookConfirmedConfig,
    /// Strategy F: the high's bucket inside the season's peak slot.
    #[serde(default)]
    pub peak_slot: PeakSlotConfig,
    /// Strategy G: resting NO bids on cheap buckets well above the high.
    #[serde(default = "TailSellerConfig::absent")]
    pub tail_seller: TailSellerConfig,
    /// Strategy H: YES on the bucket one degree above the high.
    #[serde(default = "NextDegreeConfig::absent")]
    pub next_degree: NextDegreeConfig,
    /// Strategy I: NO on mid-priced buckets the model rates lower.
    #[serde(default = "MiddleFadeConfig::absent")]
    pub middle_fade: MiddleFadeConfig,
    /// Strategy J: two-sided resting quotes in the morning.
    #[serde(default = "MorningMakerConfig::absent")]
    pub morning_maker: MorningMakerConfig,
    /// Strategy K: the NO of the high's bucket when KNMI's ten-minute
    /// reading says the next METAR will beat it.
    #[serde(default = "KnmiNowcastConfig::absent")]
    pub knmi_nowcast: KnmiNowcastConfig,
    /// The strategy lab's paper strategies L1–L25, each on its own paper
    /// book with its own limits ([`LabConfig::risk`]).
    #[serde(default = "LabConfig::absent")]
    pub lab: LabConfig,
    /// The day-1 forecast product the lab reads (the configured one; the
    /// model may use none). `None`: the model's product, if any.
    #[serde(default)]
    pub lab_forecast: Option<ForecastProduct>,
    pub unwind: UnwindConfig,
    pub evaluate_on_book_updates: bool,
    pub decision_log_capacity: usize,
    /// Identical rejected proposals (same strategy, token, side, price and
    /// reasons) within this window are counted but not re-recorded, so fast
    /// market data cannot flood the decision log. 0 disables suppression.
    #[serde(default = "default_rejection_dedup_secs")]
    pub rejection_dedup_secs: i64,
}

pub const fn default_rejection_dedup_secs() -> i64 {
    60
}

/// Engine hint for a station's collector (never a request by itself).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StationHint {
    pub peak_watch: bool,
    pub has_exposure: bool,
}

/// Counters (latency figures are informational and never affect decisions).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineStats {
    pub events_total: u64,
    pub events_by_kind: BTreeMap<String, u64>,
    pub evaluations_total: u64,
    pub proposals_total: u64,
    pub approvals_total: u64,
    pub rejections_total: u64,
    /// Rejections not re-recorded (identical to one inside the dedup window).
    #[serde(default)]
    pub rejections_suppressed: u64,
    pub fills_total: u64,
    pub last_handle_micros: u64,
    pub max_handle_micros: u64,
    pub last_seq: u64,
    pub out_of_order_seq: u64,
}

/// Outputs of handling one event.
#[derive(Debug, Default)]
pub struct EngineOutput {
    pub approved: Vec<ApprovedIntent>,
    pub decisions: Vec<DecisionRecord>,
    pub hints: Vec<(StationId, StationHint)>,
    pub alerts: Vec<String>,
}

/// A forecast series as received, keyed by location and product.
#[derive(Debug, Clone)]
struct StoredForecast {
    event: ForecastEvent,
    /// Knowledge time (envelope `available_at`).
    received_at: DateTime<Utc>,
}

/// `(location, provider, model, lead_days)`.
type ForecastKey = (LocationId, ProviderId, String, Option<u8>);

fn forecast_key(e: &ForecastEvent) -> ForecastKey {
    (
        e.location.clone(),
        e.provider.clone(),
        e.model.clone(),
        e.lead_days,
    )
}

fn product_label(e: &ForecastEvent) -> String {
    match e.lead_days {
        Some(d) => format!("{}/{}/d{d}", e.provider, e.model),
        None => format!("{}/{}", e.provider, e.model),
    }
}

fn tenths_series(e: &ForecastEvent) -> Vec<(DateTime<Utc>, i32)> {
    e.hourly.iter().map(|(t, v)| (*t, v.tenths())).collect()
}

/// One paper book: its positions, the strategy that opened each, its risk
/// limits and counters, and its realized P&L. Strategies A–K and the unwind
/// engine share the main book; each lab strategy trades a book of its own,
/// so it neither blocks nor is blocked by any other strategy.
struct PaperBook {
    positions: PositionBook,
    position_strategy: HashMap<TokenId, StrategyId>,
    risk: RiskEngine,
    realized_pnl_total: Usd,
}

impl PaperBook {
    fn new(risk: RiskConfig, run: &RunId) -> Self {
        Self {
            positions: PositionBook::new(),
            position_strategy: HashMap::new(),
            risk: RiskEngine::new(risk, run),
            realized_pnl_total: Usd::ZERO,
        }
    }
}

/// The main book (strategies A–K and the unwind engine).
const MAIN: usize = 0;

/// A lab strategy's own limits: the main risk settings (data freshness,
/// prices, books, weather gates) with the lab's money limits.
pub fn lab_risk_config(main: &RiskConfig, lab: &LabRiskConfig, family: u8) -> RiskConfig {
    let position = if family == 3 {
        lab.f_escape_position_usd
    } else {
        lab.position_size_usd
    };
    let exposure = lab.max_exposure_usd.max(position);
    RiskConfig {
        position_size_usd: position,
        global_max_exposure_usd: exposure,
        max_market_exposure_usd: Some(exposure),
        max_location_exposure_usd: Some(exposure),
        max_strategy_exposure_usd: Some(exposure),
        max_daily_new_exposure_usd: Some(lab.max_daily_new_exposure_usd.max(position)),
        max_daily_loss_usd: Some(lab.max_daily_loss_usd),
        max_spread: lab.max_spread,
        max_orders_per_minute: lab.max_orders_per_minute.max(1),
        strategy_caps: BTreeMap::new(),
        ..main.clone()
    }
}

/// What the lab strategies read beyond A–K's inputs.
#[derive(Default)]
struct LabState {
    /// KNMI readings per station of the last [`LAB_HISTORY`], oldest first.
    knmi: HashMap<StationId, Vec<TenMinuteObservation>>,
    /// METARs with their weather groups per station, oldest first.
    reports: HashMap<StationId, Vec<WxReport>>,
    /// Taker trades per market, oldest first, and the ids seen.
    takers: HashMap<EventSlug, (Vec<TakerTrade>, HashSet<String>)>,
    wallets: Option<WalletScores>,
}

/// The kernel.
pub struct Engine {
    cfg: EngineConfig,
    temps: TemperatureStateEngine,
    peak: HashMap<LocationId, PeakDetectionEngine>,
    model: Arc<dyn ProbabilityModel>,
    markets: HashMap<EventSlug, DailyTemperatureMarket>,
    token_index: HashMap<TokenId, EventSlug>,
    books: HashMap<TokenId, OrderBook>,
    last_trades: HashMap<TokenId, TradePrint>,
    strategies: Vec<Box<dyn Strategy>>,
    unwind: UnwindEngine,
    orders: OrderManager,
    /// The main book first, then one per lab strategy.
    paper: Vec<PaperBook>,
    /// The book of each lab strategy (others trade the main book).
    book_of: HashMap<StrategyId, usize>,
    lab: LabState,
    health: BTreeMap<(ProviderId, Option<StationId>), ProviderHealthSnapshot>,
    corrections: HashMap<StationId, DateTime<Utc>>,
    kill_switch: Option<String>,
    storage_ok: bool,
    execution_ok: bool,
    compliance_ok: bool,
    next_decision: u64,
    now: DateTime<Utc>,
    decisions: VecDeque<DecisionRecord>,
    evaluations: HashMap<EventSlug, Vec<BucketEvaluation>>,
    views: HashMap<LocationId, Vec<ViewEvaluation>>,
    hints: HashMap<StationId, StationHint>,
    stats: EngineStats,
    recent_rejections: HashMap<String, DateTime<Utc>>,
    forecasts: HashMap<ForecastKey, StoredForecast>,
    /// The latest ten-minute reading per station (predictive input only).
    nowcasts: HashMap<StationId, TenMinuteObservation>,
}

/// Largest hole (minutes) in a day series, counting local midnight → first
/// report. `None` when the series is empty.
fn max_gap_minutes(day_start: DateTime<Utc>, points: &[wm_strategy::ObsPoint]) -> Option<i64> {
    let mut prev = day_start;
    let mut max = None;
    for p in points {
        // Rounded up: a 75 min 30 s hole exceeds a 75-minute limit.
        let gap = ((p.observed_at - prev).num_seconds().max(0) + 59) / 60;
        max = Some(max.map_or(gap, |m: i64| m.max(gap)));
        prev = prev.max(p.observed_at);
    }
    max
}

fn view_of(filter: ObservationFilter) -> ViewKind {
    match filter {
        ObservationFilter::AllRows => ViewKind::All,
        f => ViewKind::Filtered { filter: f },
    }
}

impl Engine {
    pub fn new(cfg: EngineConfig, model: Arc<dyn ProbabilityModel>) -> Self {
        let mut temps = TemperatureStateEngine::new(4);
        let mut peak = HashMap::new();
        for l in &cfg.locations {
            temps.register_station(l.station.clone(), l.timezone);
            peak.insert(l.location.clone(), PeakDetectionEngine::new(l.peak.clone()));
        }
        let mut strategies: Vec<Box<dyn Strategy>> = vec![
            Box::new(CertainOutcomes::new(cfg.certain.clone())),
            Box::new(BuyYesFinalHigh::new(cfg.buy_yes.clone())),
            Box::new(BuyNoAboveHigh::new(cfg.buy_no.clone())),
            Box::new(SplitUnwind::new(cfg.split_unwind.clone())),
            Box::new(BookConfirmedHigh::new(cfg.book_confirmed.clone())),
            Box::new(PeakSlotHigh::new(cfg.peak_slot.clone())),
            Box::new(TailSeller::new(cfg.tail_seller.clone())),
            Box::new(NextDegree::new(cfg.next_degree.clone())),
            Box::new(MiddleFade::new(cfg.middle_fade.clone())),
            Box::new(MorningMaker::new(cfg.morning_maker.clone())),
            Box::new(KnmiNowcast::new(cfg.knmi_nowcast.clone())),
        ];
        let mut paper = vec![PaperBook::new(cfg.risk.clone(), &cfg.run_id)];
        let mut book_of = HashMap::new();
        for s in lab_strategies(&cfg.lab, &cfg.peak_slot) {
            let risk = lab_risk_config(&cfg.risk, &cfg.lab.risk, s.family());
            book_of.insert(s.id().clone(), paper.len());
            paper.push(PaperBook::new(risk, &cfg.run_id));
            strategies.push(Box::new(s));
        }
        let unwind = UnwindEngine::new(cfg.unwind.clone());
        Self {
            temps,
            peak,
            model,
            markets: HashMap::new(),
            token_index: HashMap::new(),
            books: HashMap::new(),
            last_trades: HashMap::new(),
            strategies,
            unwind,
            orders: OrderManager::new(),
            paper,
            book_of,
            lab: LabState::default(),
            health: BTreeMap::new(),
            corrections: HashMap::new(),
            kill_switch: None,
            storage_ok: true,
            execution_ok: true,
            compliance_ok: false,
            next_decision: 1,
            now: DateTime::<Utc>::MIN_UTC,
            decisions: VecDeque::new(),
            evaluations: HashMap::new(),
            views: HashMap::new(),
            hints: HashMap::new(),
            stats: EngineStats::default(),
            recent_rejections: HashMap::new(),
            forecasts: HashMap::new(),
            nowcasts: HashMap::new(),
            cfg,
        }
    }

    /// Swap the probability model (e.g. after a retraining). Takes effect at
    /// the next evaluation; decisions record the model id in their provenance.
    pub fn set_model(&mut self, model: Arc<dyn ProbabilityModel>) {
        self.model = model;
    }

    pub fn model_id(&self) -> &str {
        self.model.id()
    }

    /// The book `strategy` trades.
    fn book_index(&self, strategy: &StrategyId) -> usize {
        self.book_of.get(strategy).copied().unwrap_or(MAIN)
    }

    /// The book a restored fill of `strategy` belongs to. A lab strategy
    /// switched off since its order still gets a book of its own: its
    /// positions settle there and never land in the main book, where A–K's
    /// caps would count them and the unwind engine would sell them.
    fn restore_book(&mut self, strategy: &StrategyId) -> usize {
        if let Some(&b) = self.book_of.get(strategy) {
            return b;
        }
        let id = strategy.as_str();
        match wm_strategy::lab::family_of(id).filter(|_| wm_strategy::lab::is_lab_id(id)) {
            Some(family) => {
                let risk = lab_risk_config(&self.cfg.risk, &self.cfg.lab.risk, family);
                let b = self.paper.len();
                self.paper.push(PaperBook::new(risk, &self.cfg.run_id));
                self.book_of.insert(strategy.clone(), b);
                b
            }
            None => MAIN,
        }
    }

    /// Live orders of book `b`.
    fn book_orders(&self, b: usize) -> Vec<OpenOrderView> {
        self.orders
            .open_views()
            .into_iter()
            .filter(|o| self.book_index(&o.strategy) == b)
            .collect()
    }

    /// Tokens with live orders of book `b`.
    fn book_pending(&self, b: usize) -> HashSet<TokenId> {
        self.orders
            .open_orders()
            .filter(|o| self.book_index(&o.strategy) == b)
            .map(|o| o.token.clone())
            .collect()
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// The main book's positions (strategies A–K).
    pub fn positions(&self) -> &PositionBook {
        &self.paper[MAIN].positions
    }

    /// A lab strategy's own positions.
    pub fn lab_positions(&self, strategy: &StrategyId) -> Option<&PositionBook> {
        self.book_of
            .get(strategy)
            .map(|&b| &self.paper[b].positions)
    }

    pub fn orders(&self) -> &OrderManager {
        &self.orders
    }

    pub fn markets(&self) -> &HashMap<EventSlug, DailyTemperatureMarket> {
        &self.markets
    }

    pub fn stats(&self) -> &EngineStats {
        &self.stats
    }

    /// Realized P&L of the main book.
    pub fn realized_pnl_total(&self) -> Usd {
        self.paper[MAIN].realized_pnl_total
    }

    /// Realized P&L of every lab book together.
    pub fn lab_realized_pnl_total(&self) -> Usd {
        self.paper[1..].iter().map(|b| b.realized_pnl_total).sum()
    }

    pub fn set_storage_ok(&mut self, ok: bool) {
        self.storage_ok = ok;
    }

    pub fn kill_switch(&self) -> Option<&str> {
        self.kill_switch.as_deref()
    }

    fn location_index_for_station(&self, station: &StationId) -> Option<usize> {
        self.cfg
            .locations
            .iter()
            .position(|l| &l.station == station)
    }

    fn location_index(&self, location: &LocationId) -> Option<usize> {
        self.cfg
            .locations
            .iter()
            .position(|l| &l.location == location)
    }

    /// Keep a report's weather groups for the lab (the latest version of
    /// each report; [`LAB_HISTORY`] of them).
    fn note_report(&mut self, o: &Observation) {
        if !self.cfg.lab.enabled {
            return;
        }
        let list = self.lab.reports.entry(o.key.station.clone()).or_default();
        let r = WxReport::from_observation(o);
        match list.binary_search_by_key(&r.observed_at, |x| x.observed_at) {
            Ok(i) => list[i] = r,
            Err(i) => list.insert(i, r),
        }
        let cutoff = self.now - LAB_HISTORY;
        list.retain(|x| x.observed_at >= cutoff);
    }

    /// Keep a ten-minute reading for the lab (a later copy of the same
    /// interval replaces it; [`LAB_HISTORY`] of them).
    fn note_reading(&mut self, o: &TenMinuteObservation) {
        let list = self.lab.knmi.entry(o.station.clone()).or_default();
        match list.binary_search_by_key(&o.interval_end, |x| x.interval_end) {
            Ok(i) => list[i] = o.clone(),
            Err(i) => list.insert(i, o.clone()),
        }
        let cutoff = self.now - LAB_HISTORY;
        list.retain(|x| x.interval_end >= cutoff);
    }

    /// Handle one event.
    pub fn handle(&mut self, env: &EventEnvelope) -> EngineOutput {
        let started = std::time::Instant::now();
        let mut out = EngineOutput::default();
        if env.available_at > self.now {
            self.now = env.available_at;
        }
        if env.seq > 0 {
            if env.seq <= self.stats.last_seq {
                self.stats.out_of_order_seq += 1;
            }
            self.stats.last_seq = self.stats.last_seq.max(env.seq);
        }
        self.stats.events_total += 1;
        *self
            .stats
            .events_by_kind
            .entry(env.event.kind().to_owned())
            .or_insert(0) += 1;

        match &env.event {
            WeatherMachineEvent::WeatherObservation(o) => {
                self.temps.apply_observation(&o.observation);
                self.note_report(&o.observation);
                if let Some(i) = self.location_index_for_station(o.observation.station()) {
                    self.evaluate_location(i, true, &mut out);
                }
            }
            WeatherMachineEvent::WeatherCorrection(c) => {
                self.temps.apply_correction(c);
                self.note_report(&c.current);
                self.corrections
                    .insert(c.current.key.station.clone(), self.now);
                out.alerts.push(format!(
                    "correction {} v{} → v{}",
                    c.current.key.fingerprint(),
                    c.previous.version,
                    c.current.version
                ));
                if let Some(i) = self.location_index_for_station(&c.current.key.station) {
                    self.evaluate_location(i, true, &mut out);
                }
            }
            WeatherMachineEvent::ForecastUpdate(f) => {
                // Predictive input only: it never touches the observed high,
                // the views' observations or settlement — at most the model's
                // forecast feature, and only for the model's own product.
                if let Some(i) = self.location_index(&f.location) {
                    let key = forecast_key(f);
                    let newer = self
                        .forecasts
                        .get(&key)
                        .is_none_or(|s| s.received_at <= env.available_at);
                    if newer {
                        self.forecasts.insert(
                            key,
                            StoredForecast {
                                event: f.clone(),
                                received_at: env.available_at,
                            },
                        );
                        self.evaluate_location(i, false, &mut out);
                    }
                }
            }
            WeatherMachineEvent::NowcastUpdate(n) => {
                // Predictive input only: the latest reading per station (and
                // the lab's history of them). It never touches the observed
                // high, the views or settlement.
                let o = &n.observation;
                self.note_reading(o);
                let newer = self
                    .nowcasts
                    .get(&o.station)
                    .is_none_or(|x| x.interval_end < o.interval_end);
                if newer {
                    self.nowcasts.insert(o.station.clone(), o.clone());
                    if let Some(i) = self.location_index_for_station(&o.station) {
                        self.evaluate_location(i, false, &mut out);
                    }
                }
            }
            WeatherMachineEvent::MarketSnapshot(m) => {
                let m = m.market.clone();
                for o in &m.outcomes {
                    self.token_index
                        .insert(o.yes_token.clone(), m.event_slug.clone());
                    self.token_index
                        .insert(o.no_token.clone(), m.event_slug.clone());
                }
                let loc = m.location.clone();
                self.markets.insert(m.event_slug.clone(), m);
                if let Some(i) = self.location_index(&loc) {
                    self.evaluate_location(i, false, &mut out);
                }
            }
            WeatherMachineEvent::OrderBookUpdate(b) => {
                let token = b.book.token.clone();
                self.books.insert(token.clone(), b.book.clone());
                for s in self.strategies.iter_mut().filter(|s| s.enabled()) {
                    s.observe_book(&b.book);
                }
                if self.cfg.evaluate_on_book_updates
                    && let Some(loc) = self
                        .token_index
                        .get(&token)
                        .and_then(|s| self.markets.get(s))
                        .map(|m| m.location.clone())
                    && let Some(i) = self.location_index(&loc)
                {
                    self.evaluate_location(i, false, &mut out);
                }
            }
            WeatherMachineEvent::MarketStreamHeartbeat(h) => {
                // Quiet books delivered on the live connection stay current.
                for b in self.books.values_mut() {
                    if b.received_at >= h.connected_since {
                        b.confirmed_at = Some(env.available_at);
                    }
                }
            }
            WeatherMachineEvent::MarketTrade(t) => {
                self.last_trades
                    .insert(t.trade.token.clone(), t.trade.clone());
            }
            WeatherMachineEvent::TakerTrades(t) => {
                // Who traded (the lab's flow rules): new trades of known
                // markets, each once; their market is evaluated again.
                let mut touched: Vec<LocationId> = Vec::new();
                for trade in &t.trades {
                    let Some(slug) = self.token_index.get(&trade.token) else {
                        continue;
                    };
                    let (list, ids) = self.lab.takers.entry(slug.clone()).or_default();
                    if !ids.insert(trade.id.clone()) {
                        continue;
                    }
                    let at = list.partition_point(|x| x.at <= trade.at);
                    list.insert(at, trade.clone());
                    if let Some(m) = self.markets.get(slug)
                        && !touched.contains(&m.location)
                    {
                        touched.push(m.location.clone());
                    }
                }
                if self.cfg.lab.enabled {
                    for loc in touched {
                        if let Some(i) = self.location_index(&loc) {
                            self.evaluate_location(i, false, &mut out);
                        }
                    }
                }
            }
            WeatherMachineEvent::WalletScores(w) => {
                self.lab.wallets = Some(WalletScores::from_event(w));
            }
            WeatherMachineEvent::OrderUpdate(u) => self.apply_order_update(u, &mut out),
            WeatherMachineEvent::Timer(t) => match &t.kind {
                TimerKind::Evaluate { location } => {
                    if let Some(i) = self.location_index(location) {
                        self.evaluate_location(i, false, &mut out);
                    }
                }
                TimerKind::Heartbeat => {
                    for i in 0..self.cfg.locations.len() {
                        self.evaluate_location(i, false, &mut out);
                    }
                    self.forget_history();
                }
            },
            WeatherMachineEvent::ProviderHealthChanged(h) => {
                let s = h.snapshot.clone();
                self.health.insert((s.provider.clone(), s.scope.clone()), s);
            }
            WeatherMachineEvent::Operator(OperatorCommand::KillSwitch { engaged, reason }) => {
                self.kill_switch = engaged.then(|| reason.clone());
                out.alerts.push(if *engaged {
                    format!("KILL SWITCH ENGAGED: {reason}")
                } else {
                    "kill switch released".into()
                });
            }
        }

        let micros = started.elapsed().as_micros() as u64;
        self.stats.last_handle_micros = micros;
        self.stats.max_handle_micros = self.stats.max_handle_micros.max(micros);
        out
    }

    fn apply_order_update(&mut self, ev: &OrderUpdateEvent, out: &mut EngineOutput) {
        match self.orders.apply(&ev.update) {
            Ok(Applied::Changed {
                previous,
                newly_filled,
            }) => {
                self.release_unfilled(&ev.update.client_order_id, previous);
                if newly_filled.micros() > 0
                    && let Some(fill) = &ev.fill
                    && let Some(rec) = self.orders.get(&ev.update.client_order_id).cloned()
                {
                    let b = self.book_index(&rec.strategy);
                    let now = self.now;
                    let book = &mut self.paper[b];
                    let before = book.positions.total_realized_pnl();
                    match book.positions.apply_fill(fill, &rec.instrument()) {
                        Ok(()) => {
                            self.stats.fills_total += 1;
                            if fill.side == wm_core::market::Side::Buy {
                                book.position_strategy
                                    .entry(fill.token.clone())
                                    .or_insert(rec.strategy.clone());
                                if b == MAIN {
                                    self.unwind.note_entry(&fill.token, fill.ts);
                                }
                            }
                            let delta = book.positions.total_realized_pnl() - before;
                            if !delta.is_zero() {
                                book.risk.record_realized_pnl(delta, now);
                                book.realized_pnl_total += delta;
                            }
                            if b == MAIN
                                && book
                                    .positions
                                    .get(&fill.token)
                                    .is_some_and(|p| p.shares.is_zero())
                            {
                                self.unwind.forget(&fill.token);
                            }
                        }
                        Err(e) => out.alerts.push(format!("fill rejected by portfolio: {e}")),
                    }
                }
            }
            Ok(Applied::Unchanged) => {}
            Err(e) => out.alerts.push(format!("order update rejected: {e}")),
        }
    }

    /// An opening buy that just ended (expired, cancelled, rejected) with
    /// shares unfilled: their cost at the limit leaves today's new-exposure
    /// counter, which counted the whole order at its approval. Resting
    /// orders that expire unfilled would otherwise use up the daily limit.
    fn release_unfilled(&mut self, id: &wm_core::ids::ClientOrderId, previous: OrderStatus) {
        let Some(rec) = self.orders.get(id) else {
            return;
        };
        let ended_unfilled = !previous.is_terminal()
            && rec.status.is_terminal()
            && rec.status != OrderStatus::Filled;
        if ended_unfilled
            && rec.kind == IntentKind::Open
            && rec.side == wm_core::market::Side::Buy
            && rec.remaining().micros() > 0
        {
            let cost = notional(rec.limit_price, rec.remaining(), Rounding::Up);
            let (created, b) = (rec.created_at, self.book_index(&rec.strategy));
            self.paper[b]
                .risk
                .release_daily_new_exposure(cost, created, self.now);
        }
    }

    /// Candidate views for a location (and whether they are all evaluable).
    fn build_views(
        &self,
        loc: &crate::engine::EngineLocation,
        market: Option<&DailyTemperatureMarket>,
    ) -> (Vec<ViewEvaluation>, Vec<ViewSnapshot>, bool) {
        let today = local_date(self.now, loc.timezone);
        let filters: Vec<ObservationFilter> = match (loc.confirmed_filter, market) {
            (Some(f), _) => vec![f],
            (None, Some(m)) if !m.resolution.filters.is_empty() => m.resolution.filters.clone(),
            _ => vec![ObservationFilter::AllRows],
        };
        let peak = self.peak.get(&loc.location).cloned().unwrap_or_default();
        let forecast = self.forecast_day(loc, today).ok();
        let mut evals = Vec::new();
        let mut snaps = Vec::new();
        let mut complete = true;
        for f in filters {
            let view = view_of(f);
            let state = self.temps.day_state(&loc.station, today, view, self.now);
            let assessment = state
                .as_ref()
                .and_then(|s| peak.assess_with(s, loc.timezone, self.now, forecast.as_ref()));
            let distribution = assessment
                .as_ref()
                .and_then(|a| self.model.distribution(&a.features));
            snaps.push(ViewSnapshot {
                label: view.label(),
                state: state.clone(),
                features: assessment.as_ref().map(|a| a.features.clone()),
                windows_met: assessment
                    .as_ref()
                    .map(|a| a.windows_met.clone())
                    .unwrap_or_default(),
                distribution: distribution.clone(),
            });
            match assessment {
                Some(a) => evals.push(ViewEvaluation {
                    view,
                    assessment: a,
                    distribution,
                }),
                None => complete = false,
            }
        }
        (evals, snaps, complete)
    }

    /// Today's forecast as the model may use it: the model's own product,
    /// retrieved no earlier than the product's ready time for `date`, and
    /// covering the whole local day. `Err` says why not.
    fn forecast_day(
        &self,
        loc: &EngineLocation,
        date: chrono::NaiveDate,
    ) -> Result<ForecastDay, String> {
        let Some(product) = self.model.forecast_product() else {
            return Err("model does not use forecasts".into());
        };
        let key = (
            loc.location.clone(),
            product.provider.clone(),
            product.model.clone(),
            Some(product.lead_days),
        );
        let Some(stored) = self.forecasts.get(&key) else {
            return Err(format!("no {} forecast received", product.label()));
        };
        let ready = product.usable_from(date, loc.timezone);
        if stored.received_at < ready {
            return Err(format!(
                "latest series retrieved before today's ready time ({} UTC)",
                ready.format("%H:%M")
            ));
        }
        ForecastDay::from_series(
            date,
            loc.timezone,
            &tenths_series(&stored.event),
            stored.received_at,
        )
        .ok_or_else(|| "series does not cover today completely".to_owned())
    }

    /// Dashboard view of a location's forecast: the model's product if it
    /// has one, else the most recently received series.
    fn forecast_snapshot(
        &self,
        loc: &EngineLocation,
        date: chrono::NaiveDate,
    ) -> Option<ForecastSnapshot> {
        let wanted = self.model.forecast_product();
        let stored = self
            .forecasts
            .values()
            .filter(|s| s.event.location == loc.location)
            .filter(|s| wanted.is_none_or(|p| p.matches(&s.event)))
            .max_by_key(|s| s.received_at)?;
        let (in_use, status, day) = match self.forecast_day(loc, date) {
            Ok(day) => (true, "in use by the model".to_owned(), Some(day)),
            Err(why) => (
                false,
                why,
                ForecastDay::from_series(
                    date,
                    loc.timezone,
                    &tenths_series(&stored.event),
                    stored.received_at,
                ),
            ),
        };
        Some(ForecastSnapshot {
            product: product_label(&stored.event),
            received_at: stored.received_at,
            in_use,
            status,
            day_max_tenths: day.as_ref().and_then(ForecastDay::day_max_tenths),
            remaining_max_tenths: day.as_ref().and_then(|d| d.remaining_max_tenths(self.now)),
            rise_tenths: day.as_ref().and_then(|d| d.rise_tenths(self.now)),
            hourly: day.map(|d| d.hourly).unwrap_or_default(),
        })
    }

    /// The lab's day-1 forecast for `date` (the configured product, else
    /// the model's) and yesterday's error: its observed high (whole °C, the
    /// location's view) minus its forecast maximum, tenths.
    fn lab_forecast(
        &self,
        loc: &EngineLocation,
        date: NaiveDate,
    ) -> (Option<ForecastDay>, Option<i32>) {
        let Some(product) = self
            .cfg
            .lab_forecast
            .as_ref()
            .or_else(|| self.model.forecast_product())
        else {
            return (None, None);
        };
        let key = (
            loc.location.clone(),
            product.provider.clone(),
            product.model.clone(),
            Some(product.lead_days),
        );
        let Some(stored) = self.forecasts.get(&key) else {
            return (None, None);
        };
        let series = tenths_series(&stored.event);
        let today = (stored.received_at >= product.usable_from(date, loc.timezone))
            .then(|| ForecastDay::from_series(date, loc.timezone, &series, stored.received_at))
            .flatten();
        let error = date.pred_opt().and_then(|y| {
            let fmax = ForecastDay::from_series(y, loc.timezone, &series, stored.received_at)?
                .day_max_tenths()?;
            let view = loc.confirmed_filter.map_or(ViewKind::All, view_of);
            let (_, end) = local_day_bounds(y, loc.timezone);
            let high = self.temps.day_state(&loc.station, y, view, end)?.high?;
            Some(high.value.round_half_up_whole() * 10 - fmax)
        });
        (today, error)
    }

    fn weather_status(&self, station: &StationId) -> WeatherStatus {
        let health = station_state(
            self.health
                .values()
                .filter(|h| h.scope.as_ref() == Some(station))
                .map(|h| h.state),
        );
        let tz = self.temps.timezone(station).unwrap_or(chrono_tz::UTC);
        let today = local_date(self.now, tz);
        let today_state = self
            .temps
            .day_state(station, today, ViewKind::All, self.now);
        let max_gap_minutes = today_state
            .as_ref()
            .and_then(|s| max_gap_minutes(wm_core::time::local_day_start(today, tz), &s.points));
        let last = today_state.and_then(|s| s.last_observation_at).or_else(|| {
            self.temps
                .day_state(
                    station,
                    today.pred_opt().unwrap_or(today),
                    ViewKind::All,
                    self.now,
                )
                .and_then(|s| s.last_observation_at)
        });
        WeatherStatus {
            station: station.clone(),
            last_observation_at: last,
            health,
            last_correction_at: self.corrections.get(station).copied(),
            max_gap_minutes,
        }
    }

    fn today_market(&self, loc: &EngineLocation) -> Option<DailyTemperatureMarket> {
        let today = local_date(self.now, loc.timezone);
        self.markets
            .values()
            .find(|m| m.location == loc.location && m.local_date == today && !m.closed)
            .cloned()
    }

    fn evaluate_location(&mut self, idx: usize, from_weather: bool, out: &mut EngineOutput) {
        let Some(loc) = self.cfg.locations.get(idx).cloned() else {
            return;
        };
        self.stats.evaluations_total += 1;
        let market = self.today_market(&loc);
        let (views, _snaps, complete) = self.build_views(&loc, market.as_ref());

        let has_exposure = market.as_ref().is_some_and(|m| {
            self.paper
                .iter()
                .any(|b| b.positions.for_event(&m.event_slug).next().is_some())
                || self
                    .orders
                    .open_orders()
                    .any(|o| o.event_slug == m.event_slug)
        });
        let hint = StationHint {
            peak_watch: views.iter().any(|v| v.assessment.peak_watch),
            has_exposure,
        };
        if self.hints.get(&loc.station) != Some(&hint) {
            self.hints.insert(loc.station.clone(), hint);
            out.hints.push((loc.station.clone(), hint));
        }
        self.views.insert(loc.location.clone(), views.clone());
        let Some(market) = market else { return };

        // Each book's live tokens: a strategy sees only its own book.
        let pending: Vec<HashSet<TokenId>> = (0..self.paper.len())
            .map(|b| self.book_pending(b))
            .collect();
        let mut proposals: Vec<Proposal> = Vec::new();
        let mut evaluations: Vec<BucketEvaluation> = Vec::new();
        if complete && !views.is_empty() {
            let model = Arc::clone(&self.model);
            let today = local_date(self.now, loc.timezone);
            let (forecast, yesterday_error) = if self.cfg.lab.enabled {
                self.lab_forecast(&loc, today)
            } else {
                (None, None)
            };
            let day_start = local_day_bounds(today, loc.timezone).0;
            let reports: &[WxReport] = self.lab.reports.get(&loc.station).map_or(&[], |r| {
                &r[r.partition_point(|x| x.observed_at < day_start)..]
            });
            let neighbours: Vec<NeighbourReadings<'_>> = loc
                .neighbours
                .iter()
                .map(|n| NeighbourReadings {
                    name: &n.name,
                    bearing_deg: n.bearing_deg,
                    readings: self.lab.knmi.get(&n.station).map_or(&[], Vec::as_slice),
                })
                .collect();
            let lab = LabInputs {
                knmi: self.lab.knmi.get(&loc.station).map_or(&[], Vec::as_slice),
                neighbours: &neighbours,
                reports,
                forecast: forecast.as_ref(),
                yesterday_error_tenths: yesterday_error,
                takers: self
                    .lab
                    .takers
                    .get(&market.event_slug)
                    .map_or(&[], |(v, _)| v.as_slice()),
                wallets: self.lab.wallets.as_ref(),
                position: loc.position,
            };
            for s in self.strategies.iter_mut() {
                if !s.enabled() {
                    continue;
                }
                let b = self.book_of.get(s.id()).copied().unwrap_or(MAIN);
                let ctx = StrategyContext {
                    now: self.now,
                    mode: self.cfg.mode,
                    location: &loc.location,
                    market: &market,
                    books: &self.books,
                    views: &views,
                    positions: &self.paper[b].positions,
                    pending_tokens: &pending[b],
                    peak_times: model.peak_times(),
                    routine_minutes: &loc.routine_minutes,
                    nowcast: self.nowcasts.get(&loc.station),
                    lab: &lab,
                };
                let o = s.evaluate(&ctx);
                proposals.extend(o.proposals);
                evaluations.extend(o.evaluations);
            }
        }
        // Unwind runs even when views are incomplete (exits are risk-reducing).
        // It looks after the main book only: lab strategies hold to
        // settlement (L3 makes its own exit).
        let unwind_views = if views.is_empty() {
            Vec::new()
        } else {
            views.clone()
        };
        proposals.extend(self.unwind.evaluate(
            &market,
            &self.paper[MAIN].positions,
            &self.books,
            &unwind_views,
            &pending[MAIN],
            &self.paper[MAIN].position_strategy,
            self.now,
        ));
        // A proposal the risk check would refuse for its book's spread waits
        // in its strategy's evaluation instead of being refused on every book
        // update (F on 7 October 2026: thousands of refusals in 45 minutes).
        let mut kept = Vec::with_capacity(proposals.len());
        for p in proposals {
            match self.spread_refusal(&p) {
                None => kept.push(p),
                Some(why) => {
                    if let Some(e) = evaluations
                        .iter_mut()
                        .find(|e| e.strategy == p.strategy && e.token == p.token)
                    {
                        e.signal = false;
                        e.blockers.push(why);
                    }
                }
            }
        }
        let proposals = kept;
        self.evaluations
            .insert(market.event_slug.clone(), evaluations.clone());

        if from_weather {
            let id = self.alloc_decision();
            let blockers: Vec<String> = evaluations.iter().map(evaluation_line).collect();
            // The bucket that came closest to a trade: the fewest blockers,
            // then the highest EV. A rule's EV assumes its trigger holds, so
            // EV alone would crown the line furthest from trading (K's fixed
            // 0.94 against a NO offered at 0.001).
            let blocked = |e: &BucketEvaluation| if e.signal { 0 } else { e.blockers.len() };
            let closest = evaluations
                .iter()
                .filter_map(|e| e.ev_per_share.map(|ev| (ev, e)))
                .min_by(|a, b| {
                    blocked(a.1)
                        .cmp(&blocked(b.1))
                        .then_with(|| b.0.total_cmp(&a.0))
                })
                .map(|(_, e)| e);
            let rec = DecisionRecord {
                decision_id: id,
                strategy: StrategyId::from_static("evaluation"),
                at: self.now,
                location: loc.location.clone(),
                event_slug: Some(market.event_slug.clone()),
                summary: match (complete, closest) {
                    (false, _) => "resolution views incomplete — no trading".to_owned(),
                    (true, Some(e)) => format!(
                        "evaluated {} bucket(s); closest: {}",
                        evaluations.len(),
                        evaluation_line(e)
                    ),
                    (true, None) => format!(
                        "evaluated {} bucket(s); none had both a price and a probability",
                        evaluations.len()
                    ),
                },
                inputs: serde_json::json!({ "views": views.iter().map(|v| serde_json::json!({
                    "view": v.view.label(),
                    "high_whole": v.assessment.features.high_whole,
                    "minutes_since_high": v.assessment.features.minutes_since_high,
                    "drop_tenths": v.assessment.features.drop_tenths,
                    "trajectory": v.assessment.features.trajectory.as_str(),
                    "p": v.distribution.as_ref().map(|d| d.probs.clone()),
                })).collect::<Vec<_>>() }),
                outputs: serde_json::json!({ "evaluations": blockers }),
                approved: false,
                reasons: Vec::new(),
            };
            self.push_decision(rec.clone());
            out.decisions.push(rec);
        }

        for p in proposals {
            self.process_proposal(p, &market, &loc, out);
        }
    }

    /// Why the risk check would refuse this opening order for its book's
    /// spread, as it does: a one-sided book, or a spread over the limit of
    /// the strategy's own book. `None` for exits and acceptable books (and
    /// without a book, which the risk check names itself).
    fn spread_refusal(&self, p: &Proposal) -> Option<String> {
        if p.kind != IntentKind::Open {
            return None;
        }
        let b = self.book_index(&p.strategy);
        let limit = self.paper[b].risk.config().max_spread_for(&p.strategy);
        match self.books.get(&p.token)?.spread() {
            None => Some("book one-sided".to_owned()),
            Some(sp) if sp > limit => Some(format!(
                "spread {sp} > {limit} ({})",
                if b == MAIN {
                    "the risk limit"
                } else {
                    "the lab book's limit"
                }
            )),
            Some(_) => None,
        }
    }

    fn alloc_decision(&mut self) -> DecisionId {
        let id = DecisionId(self.next_decision);
        self.next_decision += 1;
        id
    }

    fn push_decision(&mut self, rec: DecisionRecord) {
        self.decisions.push_back(rec);
        while self.decisions.len() > self.cfg.decision_log_capacity.max(10) {
            self.decisions.pop_front();
        }
    }

    fn process_proposal(
        &mut self,
        p: Proposal,
        market: &DailyTemperatureMarket,
        loc: &EngineLocation,
        out: &mut EngineOutput,
    ) {
        self.stats.proposals_total += 1;
        let decision_id = self.alloc_decision();
        let intent = TradeIntent {
            decision_id,
            strategy: p.strategy.clone(),
            created_at: self.now,
            location: loc.location.clone(),
            event_slug: market.event_slug.clone(),
            condition_id: p.condition_id.clone(),
            token: p.token.clone(),
            outcome_side: p.outcome_side,
            bucket_label: p.bucket_label.clone(),
            side: p.side,
            kind: p.kind,
            weather_dependent: p.weather_dependent,
            limit_price: p.limit_price,
            shares: p.shares,
            notional: notional(p.limit_price, p.shares, Rounding::Up),
            tif: p.tif,
            model_probability: Probability::new(p.p_win),
            expected_value_per_share: p.ev_per_share,
            break_even_probability: p.break_even,
            research_only: p.research_only,
            rationale: p.rationale.clone(),
        };
        let weather = self.weather_status(&loc.station);
        // The proposal's own book: its positions, live orders and limits.
        let b = self.book_index(&p.strategy);
        let open = self.book_orders(b);
        let book = self.books.get(&intent.token);
        let top = book.map(|b| serde_json::json!({ "bid": b.best_bid().map(|l| l.price.to_string()), "ask": b.best_ask().map(|l| l.price.to_string()), "age_ms": b.age_ms(self.now) }));
        let PaperBook {
            positions,
            position_strategy,
            risk,
            ..
        } = &mut self.paper[b];
        let inputs = RiskInputs {
            now: self.now,
            mode: self.cfg.mode,
            kill_switch: self.kill_switch.as_deref(),
            compliance_ok: self.compliance_ok,
            storage_ok: self.storage_ok,
            execution_ok: self.execution_ok,
            weather: Some(&weather),
            market,
            book,
            portfolio: PortfolioView {
                positions,
                open_orders: &open,
                markets: &self.markets,
                position_strategy,
            },
        };
        let decision = risk.evaluate(intent.clone(), &inputs);
        let (approved, reasons) = match &decision {
            RiskDecision::Approved(_) => (true, Vec::new()),
            RiskDecision::Rejected { reasons, .. } => (
                false,
                reasons
                    .iter()
                    .map(|r| format!("{:?}: {}", r.check, r.detail))
                    .collect::<Vec<String>>(),
            ),
        };
        if !approved && self.cfg.rejection_dedup_secs > 0 {
            let window = Duration::seconds(self.cfg.rejection_dedup_secs);
            let key = format!(
                "{}|{}|{:?}|{}|{}",
                p.strategy,
                p.token,
                p.side,
                p.limit_price,
                reasons.join(";")
            );
            if self
                .recent_rejections
                .get(&key)
                .is_some_and(|t| self.now - *t < window)
            {
                self.stats.rejections_total += 1;
                self.stats.rejections_suppressed += 1;
                return;
            }
            let now = self.now;
            self.recent_rejections.retain(|_, t| now - *t < window);
            self.recent_rejections.insert(key, now);
        }
        let rec = DecisionRecord {
            decision_id,
            strategy: p.strategy.clone(),
            at: self.now,
            location: loc.location.clone(),
            event_slug: Some(market.event_slug.clone()),
            summary: format!(
                "{} {} {} {} @ {} ×{} — {}",
                p.strategy,
                match p.side {
                    wm_core::market::Side::Buy => "BUY",
                    wm_core::market::Side::Sell => "SELL",
                },
                p.outcome_side.as_str(),
                p.bucket_label,
                p.limit_price,
                p.shares,
                if approved { "APPROVED" } else { "REJECTED" }
            ),
            inputs: serde_json::json!({
                "p_win": p.p_win,
                "ev_per_share": p.ev_per_share,
                "break_even": p.break_even,
                "rationale": p.rationale,
                "book": top,
                "weather": { "health": weather.health.as_str(), "last_observation_at": weather.last_observation_at },
            }),
            outputs: serde_json::json!({ "approved": approved, "reasons": reasons }),
            approved,
            reasons: reasons.clone(),
        };
        self.push_decision(rec.clone());
        out.decisions.push(rec);
        match decision {
            RiskDecision::Approved(a) => {
                self.stats.approvals_total += 1;
                if let Err(e) = self.orders.register(&a, p.bucket, self.now) {
                    out.alerts.push(format!("order registration failed: {e}"));
                    return;
                }
                out.approved.push(a);
            }
            RiskDecision::Rejected { .. } => self.stats.rejections_total += 1,
        }
    }

    /// Settle an event at its final whole-degree value (backtests use the
    /// observed resolution value; paper/live use the venue's resolution).
    /// Every book settles; the main book's P&L is returned (each lab
    /// book's counts toward its own limits and totals).
    pub fn settle(&mut self, slug: &EventSlug, final_value: i32) -> Usd {
        let now = self.now;
        let mut main = Usd::ZERO;
        for (i, book) in self.paper.iter_mut().enumerate() {
            let pnl = book.positions.settle_event(slug, final_value);
            if !pnl.is_zero() {
                book.risk.record_realized_pnl(pnl, now);
                book.realized_pnl_total += pnl;
            }
            if i == MAIN {
                main = pnl;
            }
        }
        if let Some(m) = self.markets.get_mut(slug) {
            m.closed = true;
        }
        self.lab.takers.remove(slug);
        main
    }

    /// Drop what the kernel no longer needs: markets settled more than
    /// [`SETTLED_HISTORY`] ago (with their tokens' books, last trades and
    /// evaluations) and terminal orders older than that. Positions keep
    /// their realized P&L; the database keeps every market, book and order.
    fn forget_history(&mut self) {
        let cutoff = self.now - SETTLED_HISTORY;
        let gone: Vec<EventSlug> = self
            .markets
            .values()
            .filter(|m| m.closed && local_day_bounds(m.local_date, m.timezone).1 < cutoff)
            .map(|m| m.event_slug.clone())
            .collect();
        for slug in &gone {
            if let Some(m) = self.markets.remove(slug) {
                for o in &m.outcomes {
                    for token in [&o.yes_token, &o.no_token] {
                        self.token_index.remove(token);
                        self.books.remove(token);
                        self.last_trades.remove(token);
                    }
                }
            }
            self.evaluations.remove(slug);
            self.lab.takers.remove(slug);
        }
        self.orders.prune_terminal_before(cutoff);
    }

    /// Rebuild earlier runs' paper book after a restart (see [`RestoreState`]):
    /// the markets, the positions with the strategy that opened them, the
    /// unwind clocks, the realized P&L of today's (UTC) restored sells and
    /// today's new exposure. Nothing is evaluated or ordered. A finished
    /// market settles at the next step, as it would have live, so its P&L
    /// counts toward today's loss limit again.
    pub fn restore(&mut self, state: &RestoreState, now: DateTime<Utc>) -> RestoreSummary {
        // The restore happens at `now`: the clock moves there (never back),
        // so the snapshot reads today's counters of the restore's day.
        if now > self.now {
            self.now = now;
        }
        let mut summary = RestoreSummary {
            markets: state.markets.len(),
            fills: state.fills.len(),
            new_exposure_today: state.new_exposure_today,
            ..RestoreSummary::default()
        };
        for m in &state.markets {
            for o in &m.outcomes {
                self.token_index
                    .insert(o.yes_token.clone(), m.event_slug.clone());
                self.token_index
                    .insert(o.no_token.clone(), m.event_slug.clone());
            }
            self.markets
                .entry(m.event_slug.clone())
                .or_insert_with(|| m.clone());
        }
        let today = now.date_naive();
        for r in &state.fills {
            let b = self.restore_book(&r.strategy);
            let book = &mut self.paper[b];
            let before = book.positions.total_realized_pnl();
            match book.positions.apply_fill(&r.fill, &r.instrument) {
                Ok(()) => {
                    if r.fill.side == wm_core::market::Side::Buy {
                        book.position_strategy
                            .entry(r.fill.token.clone())
                            .or_insert(r.strategy.clone());
                        if b == MAIN {
                            self.unwind.note_entry(&r.fill.token, r.fill.ts);
                        }
                    }
                    let delta = book.positions.total_realized_pnl() - before;
                    if !delta.is_zero() {
                        book.risk.record_realized_pnl(delta, r.fill.ts);
                        if r.fill.ts.date_naive() == today {
                            book.realized_pnl_total += delta;
                            summary.realized_today += delta;
                        }
                    }
                    if b == MAIN
                        && book
                            .positions
                            .get(&r.fill.token)
                            .is_some_and(|p| p.shares.is_zero())
                    {
                        self.unwind.forget(&r.fill.token);
                    }
                }
                Err(e) => summary.rejected.push(format!(
                    "{} on {}: {e}",
                    r.fill.client_order_id, r.instrument.event_slug
                )),
            }
        }
        self.paper[MAIN]
            .risk
            .restore_daily_new_exposure(state.new_exposure_today, now);
        for (strategy, cost) in &state.lab_new_exposure_today {
            if let Some(&b) = self.book_of.get(strategy) {
                self.paper[b].risk.restore_daily_new_exposure(*cost, now);
                summary.lab_new_exposure_today += *cost;
            }
        }
        for p in self.paper.iter().flat_map(|b| b.positions.open_positions()) {
            summary.open_positions += 1;
            summary.open_shares += p.shares;
            summary.open_cost += p.cost_basis;
        }
        summary
    }

    /// Markets whose local day ended at least `grace` ago and are not settled.
    pub fn settleable_markets(&self, grace: Duration) -> Vec<EventSlug> {
        self.markets
            .values()
            .filter(|m| !m.closed)
            .filter(|m| {
                let (_, end) = wm_core::time::local_day_bounds(m.local_date, m.timezone);
                self.now >= end + grace
            })
            .map(|m| m.event_slug.clone())
            .collect()
    }

    /// Final resolution value of a finished day under the market's primary
    /// (first) view — used by backtests to settle.
    pub fn final_value(&self, slug: &EventSlug) -> Option<i32> {
        let m = self.markets.get(slug)?;
        let loc = self
            .cfg
            .locations
            .iter()
            .find(|l| l.location == m.location)?;
        let filter = loc
            .confirmed_filter
            .or_else(|| m.resolution.filters.first().copied())
            .unwrap_or(ObservationFilter::AllRows);
        let (_, end) = wm_core::time::local_day_bounds(m.local_date, m.timezone);
        let s = self
            .temps
            .day_state(&m.station, m.local_date, view_of(filter), end)?;
        s.high.map(|h| h.value.round_half_up_whole())
    }

    /// The latest ten-minute reading of `station`, if one arrived.
    pub fn nowcast(&self, station: &StationId) -> Option<&TenMinuteObservation> {
        self.nowcasts.get(station)
    }

    pub fn hints(&self) -> &HashMap<StationId, StationHint> {
        &self.hints
    }

    /// Dashboard/API snapshot.
    pub fn snapshot(&self) -> EngineSnapshot {
        let mut locations = Vec::new();
        for loc in &self.cfg.locations {
            let market = self.today_market(loc);
            let (_, snaps, _) = self.build_views(loc, market.as_ref());
            let today = local_date(self.now, loc.timezone);
            let series = self
                .temps
                .day_state(&loc.station, today, ViewKind::All, self.now)
                .map(|s| s.points)
                .unwrap_or_default();
            let books = market
                .as_ref()
                .map(|m| {
                    m.outcomes
                        .iter()
                        .flat_map(|o| [o.yes_token.clone(), o.no_token.clone()])
                        .filter_map(|t| self.books.get(&t).cloned())
                        .collect()
                })
                .unwrap_or_default();
            let evaluations = market
                .as_ref()
                .and_then(|m| self.evaluations.get(&m.event_slug).cloned())
                .unwrap_or_default();
            locations.push(LocationSnapshot {
                location: loc.location.clone(),
                station: loc.station.clone(),
                timezone: loc.timezone.name().to_owned(),
                local_date: today,
                series,
                views: snaps,
                market,
                books,
                evaluations,
                hint: self.hints.get(&loc.station).copied().unwrap_or_default(),
                forecast: self.forecast_snapshot(loc, today),
                nowcast: self.nowcasts.get(&loc.station).cloned(),
                lab: self.cfg.lab.enabled.then(|| self.lab_snapshot(loc, today)),
            });
        }
        // Closed positions of markets the kernel has forgotten stay out:
        // their realized P&L lives on in the books' totals and the database,
        // and the snapshot would otherwise grow with every settled day.
        let shown = |p: &&Position| {
            !p.shares.is_zero() || self.markets.contains_key(&p.instrument.event_slug)
        };
        let main = &self.paper[MAIN];
        let open = self.book_orders(MAIN);
        let exposure = main.risk.exposure(&PortfolioView {
            positions: &main.positions,
            open_orders: &open,
            markets: &self.markets,
            position_strategy: &main.position_strategy,
        });
        let mut lab_books: Vec<LabBookSnapshot> = self
            .book_of
            .iter()
            .map(|(strategy, &b)| {
                let book = &self.paper[b];
                let open = self.book_orders(b);
                LabBookSnapshot {
                    strategy: strategy.clone(),
                    positions: book.positions.iter().filter(shown).cloned().collect(),
                    exposure: book.risk.exposure(&PortfolioView {
                        positions: &book.positions,
                        open_orders: &open,
                        markets: &self.markets,
                        position_strategy: &book.position_strategy,
                    }),
                    daily_new_exposure: book.risk.daily_new_exposure(self.now),
                    daily_realized_pnl: book.risk.daily_realized_pnl(self.now),
                    realized_pnl_total: book.realized_pnl_total,
                }
            })
            .collect();
        lab_books.sort_by_key(|b| wm_strategy::lab::family_of(b.strategy.as_str()));
        EngineSnapshot {
            now: self.now,
            mode: self.cfg.mode,
            run_id: self.cfg.run_id,
            model_id: self.model.id().to_owned(),
            kill_switch: self.kill_switch.clone(),
            storage_ok: self.storage_ok,
            execution_ok: self.execution_ok,
            stats: self.stats.clone(),
            locations,
            health: self.health.values().cloned().collect(),
            positions: main.positions.iter().filter(shown).cloned().collect(),
            position_strategy: main
                .position_strategy
                .iter()
                .map(|(t, s)| (t.clone(), s.clone()))
                .collect(),
            // The main book's latest orders and the lab's apart, so the
            // lab's quotes never push A–K's out of the blotter.
            orders: {
                let (main, lab): (Vec<_>, Vec<_>) = self
                    .orders
                    .recent(usize::MAX)
                    .into_iter()
                    .partition(|o| self.book_index(&o.strategy) == MAIN);
                main.into_iter()
                    .take(100)
                    .chain(lab.into_iter().take(100))
                    .cloned()
                    .collect()
            },
            exposure,
            risk: main.risk.config().clone(),
            daily_new_exposure: main.risk.daily_new_exposure(self.now),
            daily_realized_pnl: main.risk.daily_realized_pnl(self.now),
            realized_pnl_total: main.realized_pnl_total,
            // As the orders: the main book's (and the evaluations) and the
            // lab's latest 100 apart.
            decisions: {
                let (main, lab): (Vec<_>, Vec<_>) = self
                    .decisions
                    .iter()
                    .rev()
                    .partition(|d| self.book_index(&d.strategy) == MAIN);
                main.into_iter()
                    .take(100)
                    .chain(lab.into_iter().take(100))
                    .cloned()
                    .collect()
            },
            lab_books,
        }
    }

    /// The lab's inputs at a location, for the dashboard.
    fn lab_snapshot(&self, loc: &EngineLocation, today: NaiveDate) -> LabInputsSnapshot {
        let knmi = self
            .lab
            .knmi
            .get(&loc.station)
            .map_or(&[][..], Vec::as_slice);
        let latest = knmi.last();
        let radiation = latest.and_then(|r| r.radiation);
        let clear_sky_index = match (latest, radiation, loc.position) {
            (Some(r), Some(v), Some((lat, lon))) => {
                let cs = wm_strategy::lab::solar::clear_sky_ghi(
                    r.interval_end - Duration::minutes(5),
                    lat,
                    lon,
                );
                (cs >= 150.0).then(|| f64::from(v) / cs)
            }
            _ => None,
        };
        let day_start = local_day_bounds(today, loc.timezone).0;
        let reports = self.lab.reports.get(&loc.station).map_or(&[][..], |r| {
            &r[r.partition_point(|x| x.observed_at < day_start)..]
        });
        let (forecast, yesterday_error) = self.lab_forecast(loc, today);
        let taker_trades = self
            .today_market(loc)
            .and_then(|m| self.lab.takers.get(&m.event_slug).map(|(v, _)| v.len()))
            .unwrap_or(0);
        let (skilled, losing) = self.lab.wallets.as_ref().map_or((0, 0), |w| w.counts(2.0));
        LabInputsSnapshot {
            knmi_readings: knmi.len(),
            radiation_wm2: radiation,
            clear_sky_index,
            neighbours: loc
                .neighbours
                .iter()
                .map(|n| {
                    let r = self.lab.knmi.get(&n.station).and_then(|v| v.last());
                    NeighbourSnapshot {
                        name: n.name.clone(),
                        bearing_deg: n.bearing_deg,
                        interval_end: r.map(|r| r.interval_end),
                        mean_tenths: r.and_then(|r| r.mean).map(|m| m.tenths()),
                    }
                })
                .collect(),
            reports: reports.len(),
            latest_weather: reports
                .last()
                .map(|r| format!("{} UTC: {}", r.observed_at.format("%H:%M"), r.wx.summary())),
            forecast_day_max_tenths: forecast.as_ref().and_then(ForecastDay::day_max_tenths),
            yesterday_error_tenths: yesterday_error,
            taker_trades,
            wallet_days: self.lab.wallets.as_ref().map(|w| w.days),
            wallets_skilled: skilled,
            wallets_losing: losing,
        }
    }
}

/// Strategy tag for audit lines: the id up to its first `_` ("A", "B", "D").
fn strategy_tag(s: &StrategyId) -> &str {
    s.as_str().split('_').next().unwrap_or(s.as_str())
}

/// One audit line per bucket evaluation, with the numbers behind the verdict:
/// `A 21°C YES · ask 0.97 · p 0.955 (model 0.970, market 0.940) · EV -0.0215 — edge …`.
/// A maker (G, J) also names the bid it would rest, where its EV is taken:
/// `G 20°C NO · ask 0.26 · bid 0.24 (maker) · p 0.248 · EV +0.0078 — …`.
fn evaluation_line(e: &BucketEvaluation) -> String {
    let mut price = e
        .ask
        .map_or_else(|| "no ask".to_owned(), |a| format!("ask {a}"));
    if let Some(b) = e.maker_bid {
        price.push_str(&format!(" · bid {b} (maker)"));
    }
    let p = match (e.p_win, e.model_p, e.market_p) {
        (Some(pw), Some(m), Some(k)) if (pw - m).abs() > 1e-9 => {
            format!(" · p {pw:.3} (model {m:.3}, market {k:.3})")
        }
        (Some(pw), _, Some(k)) => format!(" · p {pw:.3} (market {k:.3})"),
        (Some(pw), _, None) => format!(" · p {pw:.3}"),
        (None, ..) => String::new(),
    };
    let ev = e
        .ev_per_share
        .map_or_else(String::new, |v| format!(" · EV {v:+.4}"));
    let verdict = if e.signal {
        "SIGNAL".to_owned()
    } else {
        e.blockers.join("; ")
    };
    format!(
        "{} {} {} · {price}{p}{ev} — {verdict}",
        strategy_tag(&e.strategy),
        e.bucket_label,
        e.outcome_side.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::market::OutcomeSide;
    use wm_core::units::Price;

    fn eval(maker_bid: Option<Price>) -> BucketEvaluation {
        BucketEvaluation {
            strategy: StrategyId::from_static("G_tail_seller"),
            bucket_label: "20°C".into(),
            outcome_side: OutcomeSide::No,
            token: TokenId::new("n20").unwrap(),
            ask: Some(Price::from_f64(0.26).unwrap()),
            bid: Some(Price::from_f64(0.23).unwrap()),
            p_win: Some(0.2478),
            ev_per_share: Some(0.0078),
            break_even: Some(0.24),
            signal: false,
            blockers: vec!["YES offered at 0.76 outside [0.01, 0.08]".into()],
            model_p: Some(0.2478),
            market_p: None,
            maker_bid,
        }
    }

    /// A maker's EV is taken at the bid it would rest, so its line names
    /// that bid; a taker's line stays as it was.
    #[test]
    fn a_makers_line_names_the_bid_its_ev_is_taken_at() {
        assert_eq!(
            evaluation_line(&eval(Some(Price::from_f64(0.24).unwrap()))),
            "G 20°C NO · ask 0.26 · bid 0.24 (maker) · p 0.248 · EV +0.0078 — YES offered at 0.76 outside [0.01, 0.08]"
        );
        assert_eq!(
            evaluation_line(&eval(None)),
            "G 20°C NO · ask 0.26 · p 0.248 · EV +0.0078 — YES offered at 0.76 outside [0.01, 0.08]"
        );
    }
}
