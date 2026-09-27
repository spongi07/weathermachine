//! The kernel implementation.

use crate::snapshot::{EngineSnapshot, ForecastSnapshot, LocationSnapshot, ViewSnapshot};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use wm_core::event::{
    EventEnvelope, ForecastEvent, OperatorCommand, OrderUpdateEvent, TimerKind, WeatherMachineEvent,
};
use wm_core::health::{ProviderHealthSnapshot, station_state};
use wm_core::ids::{
    DecisionId, EventSlug, LocationId, ProviderId, RunId, StationId, StrategyId, TokenId,
};
use wm_core::market::{DailyTemperatureMarket, OrderBook, TradePrint};
use wm_core::portfolio::PositionBook;
use wm_core::resolution::ObservationFilter;
use wm_core::time::local_date;
use wm_core::trading::{DecisionRecord, RunMode, TradeIntent};
use wm_core::units::{Probability, Rounding, Usd, notional};
use wm_execution::{Applied, OrderManager};
use wm_risk::{
    ApprovedIntent, PortfolioView, RiskConfig, RiskDecision, RiskEngine, RiskInputs, WeatherStatus,
};
use wm_strategy::{
    BucketEvaluation, BuyNoAboveHigh, BuyNoConfig, BuyYesConfig, BuyYesFinalHigh, CertainConfig,
    CertainOutcomes, ForecastDay, PeakConfig, PeakDetectionEngine, ProbabilityModel, Proposal,
    SplitUnwind, SplitUnwindConfig, Strategy, StrategyContext, TemperatureStateEngine,
    UnwindConfig, UnwindEngine, ViewEvaluation, ViewKind,
};

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
    risk: RiskEngine,
    orders: OrderManager,
    positions: PositionBook,
    position_strategy: HashMap<TokenId, StrategyId>,
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
    realized_pnl_total: Usd,
    recent_rejections: HashMap<String, DateTime<Utc>>,
    forecasts: HashMap<ForecastKey, StoredForecast>,
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
        let strategies: Vec<Box<dyn Strategy>> = vec![
            Box::new(CertainOutcomes::new(cfg.certain.clone())),
            Box::new(BuyYesFinalHigh::new(cfg.buy_yes.clone())),
            Box::new(BuyNoAboveHigh::new(cfg.buy_no.clone())),
            Box::new(SplitUnwind::new(cfg.split_unwind.clone())),
        ];
        let risk = RiskEngine::new(cfg.risk.clone(), &cfg.run_id);
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
            risk,
            orders: OrderManager::new(),
            positions: PositionBook::new(),
            position_strategy: HashMap::new(),
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
            realized_pnl_total: Usd::ZERO,
            recent_rejections: HashMap::new(),
            forecasts: HashMap::new(),
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

    /// Replace the default strategy set (e.g. research configurations).
    pub fn with_strategies(mut self, strategies: Vec<Box<dyn Strategy>>) -> Self {
        self.strategies = strategies;
        self
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn positions(&self) -> &PositionBook {
        &self.positions
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

    pub fn realized_pnl_total(&self) -> Usd {
        self.realized_pnl_total
    }

    pub fn set_storage_ok(&mut self, ok: bool) {
        self.storage_ok = ok;
    }

    pub fn set_execution_ok(&mut self, ok: bool) {
        self.execution_ok = ok;
    }

    pub fn set_compliance_ok(&mut self, ok: bool) {
        self.compliance_ok = ok;
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
                if let Some(i) = self.location_index_for_station(o.observation.station()) {
                    self.evaluate_location(i, true, &mut out);
                }
            }
            WeatherMachineEvent::WeatherCorrection(c) => {
                self.temps.apply_correction(c);
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
            Ok(Applied::Changed { newly_filled, .. }) => {
                if newly_filled.micros() > 0
                    && let Some(fill) = &ev.fill
                    && let Some(rec) = self.orders.get(&ev.update.client_order_id).cloned()
                {
                    let before = self.positions.total_realized_pnl();
                    match self.positions.apply_fill(fill, &rec.instrument()) {
                        Ok(()) => {
                            self.stats.fills_total += 1;
                            if fill.side == wm_core::market::Side::Buy {
                                self.position_strategy
                                    .entry(fill.token.clone())
                                    .or_insert(rec.strategy.clone());
                                self.unwind.note_entry(&fill.token, fill.ts);
                            }
                            let delta = self.positions.total_realized_pnl() - before;
                            if !delta.is_zero() {
                                self.risk.record_realized_pnl(delta, self.now);
                                self.realized_pnl_total += delta;
                            }
                            if self
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
            self.positions.for_event(&m.event_slug).next().is_some()
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

        let pending = self.orders.pending_tokens();
        let mut proposals: Vec<Proposal> = Vec::new();
        let mut evaluations: Vec<BucketEvaluation> = Vec::new();
        if complete && !views.is_empty() {
            let ctx = StrategyContext {
                now: self.now,
                mode: self.cfg.mode,
                location: &loc.location,
                market: &market,
                books: &self.books,
                views: &views,
                positions: &self.positions,
                pending_tokens: &pending,
            };
            for s in self.strategies.iter_mut() {
                if !s.enabled() {
                    continue;
                }
                let o = s.evaluate(&ctx);
                proposals.extend(o.proposals);
                evaluations.extend(o.evaluations);
            }
        }
        // Unwind runs even when views are incomplete (exits are risk-reducing).
        let unwind_views = if views.is_empty() {
            Vec::new()
        } else {
            views.clone()
        };
        proposals.extend(self.unwind.evaluate(
            &market,
            &self.positions,
            &self.books,
            &unwind_views,
            &pending,
            self.now,
        ));
        self.evaluations
            .insert(market.event_slug.clone(), evaluations.clone());

        if from_weather {
            let id = self.alloc_decision();
            let blockers: Vec<String> = evaluations
                .iter()
                .map(|e| {
                    format!(
                        "{} {} {}: {}",
                        e.strategy,
                        e.bucket_label,
                        e.outcome_side.as_str(),
                        if e.signal {
                            "SIGNAL".to_owned()
                        } else {
                            e.blockers.join("; ")
                        }
                    )
                })
                .collect();
            let rec = DecisionRecord {
                decision_id: id,
                strategy: StrategyId::from_static("evaluation"),
                at: self.now,
                location: loc.location.clone(),
                event_slug: Some(market.event_slug.clone()),
                summary: if complete {
                    format!("evaluated {} bucket(s)", evaluations.len())
                } else {
                    "resolution views incomplete — no trading".to_owned()
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
        let open = self.orders.open_views();
        let book = self.books.get(&intent.token);
        let top = book.map(|b| serde_json::json!({ "bid": b.best_bid().map(|l| l.price.to_string()), "ask": b.best_ask().map(|l| l.price.to_string()), "age_ms": b.age_ms(self.now) }));
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
                positions: &self.positions,
                open_orders: &open,
                markets: &self.markets,
                position_strategy: &self.position_strategy,
            },
        };
        let decision = self.risk.evaluate(intent.clone(), &inputs);
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
    pub fn settle(&mut self, slug: &EventSlug, final_value: i32) -> Usd {
        let pnl = self.positions.settle_event(slug, final_value);
        if !pnl.is_zero() {
            self.risk.record_realized_pnl(pnl, self.now);
            self.realized_pnl_total += pnl;
        }
        if let Some(m) = self.markets.get_mut(slug) {
            m.closed = true;
        }
        pnl
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
            });
        }
        let open = self.orders.open_views();
        let exposure = self.risk.exposure(&PortfolioView {
            positions: &self.positions,
            open_orders: &open,
            markets: &self.markets,
            position_strategy: &self.position_strategy,
        });
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
            positions: self.positions.iter().cloned().collect(),
            orders: self.orders.recent(50).into_iter().cloned().collect(),
            exposure,
            risk: self.risk.config().clone(),
            daily_new_exposure: self.risk.daily_new_exposure(),
            daily_realized_pnl: self.risk.daily_realized_pnl(),
            realized_pnl_total: self.realized_pnl_total,
            decisions: self.decisions.iter().rev().take(100).cloned().collect(),
        }
    }
}
