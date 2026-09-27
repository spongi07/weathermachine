//! The deterministic event model.
//!
//! Every input to the engine — live or replayed — is an [`EventEnvelope`].
//! `available_at` is the *knowledge time*: the earliest instant Weather Machine
//! could have acted on the event. The replay engine releases events strictly in
//! `available_at` order, which rules out look-ahead by construction.

use crate::health::{ProviderHealthSnapshot, ProviderHealthState};
use crate::ids::{LocationId, ProviderId};
use crate::market::{DailyTemperatureMarket, OrderBook, TradePrint};
use crate::trading::{Fill, OrderUpdate};
use crate::units::TempC;
use crate::weather::{DedupClass, Observation};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Live,
    Replay,
    /// Deterministic synthetic data (demo mode / tests). Always labelled in the UI.
    Synthetic,
    Operator,
}

/// Envelope carrying ordering and knowledge-time metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonic sequence number assigned by the sequencer (0 = unsequenced).
    pub seq: u64,
    /// Knowledge time.
    pub available_at: DateTime<Utc>,
    /// Wall-clock time the envelope was recorded.
    pub recorded_at: DateTime<Utc>,
    pub source: EventSource,
    pub event: WeatherMachineEvent,
}

impl EventEnvelope {
    pub fn new(
        available_at: DateTime<Utc>,
        source: EventSource,
        event: WeatherMachineEvent,
    ) -> Self {
        Self {
            seq: 0,
            available_at,
            recorded_at: available_at,
            source,
            event,
        }
    }
}

/// All event types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WeatherMachineEvent {
    WeatherObservation(ObservationEvent),
    WeatherCorrection(CorrectionEvent),
    ForecastUpdate(ForecastEvent),
    MarketSnapshot(MarketSnapshotEvent),
    OrderBookUpdate(OrderBookEvent),
    MarketTrade(MarketTradeEvent),
    OrderUpdate(OrderUpdateEvent),
    Timer(TimerEvent),
    ProviderHealthChanged(ProviderHealthEvent),
    Operator(OperatorCommand),
}

impl WeatherMachineEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            WeatherMachineEvent::WeatherObservation(_) => "weather_observation",
            WeatherMachineEvent::WeatherCorrection(_) => "weather_correction",
            WeatherMachineEvent::ForecastUpdate(_) => "forecast_update",
            WeatherMachineEvent::MarketSnapshot(_) => "market_snapshot",
            WeatherMachineEvent::OrderBookUpdate(_) => "order_book_update",
            WeatherMachineEvent::MarketTrade(_) => "market_trade",
            WeatherMachineEvent::OrderUpdate(_) => "order_update",
            WeatherMachineEvent::Timer(_) => "timer",
            WeatherMachineEvent::ProviderHealthChanged(_) => "provider_health_changed",
            WeatherMachineEvent::Operator(_) => "operator",
        }
    }
}

/// A new (or late) observation. Duplicates never produce this event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub observation: Observation,
    pub class: DedupClass,
}

/// An observation changed after first publication. Both versions are preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrectionEvent {
    pub previous: Observation,
    pub current: Observation,
    /// `true` if the new report carried `COR`, `false` for an unlabelled revision.
    pub labeled: bool,
}

/// Forecast snapshot. Predictive input only: it never changes the observed
/// high, a resolution view or a settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastEvent {
    pub location: LocationId,
    pub provider: ProviderId,
    pub model: String,
    /// When the forecast was issued — for a fixed-lead series (`lead_days`)
    /// the retrieval time, since its values come from many runs.
    pub issued_at: DateTime<Utc>,
    pub predicted_max: Option<TempC>,
    /// Hourly values by valid time (UTC), ascending.
    pub hourly: Vec<(DateTime<Utc>, TempC)>,
    /// `Some(n)`: every value was forecast n × 24 h before its valid time (the
    /// same product in training and live). `None`: a single model run.
    #[serde(default)]
    pub lead_days: Option<u8>,
}

/// Market metadata/rules discovered or changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketSnapshotEvent {
    pub market: DailyTemperatureMarket,
}

/// Full order-book snapshot for one token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBookEvent {
    pub book: OrderBook,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketTradeEvent {
    pub trade: TradePrint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderUpdateEvent {
    pub update: OrderUpdate,
    pub fill: Option<Fill>,
}

/// Timer kinds requested by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimerKind {
    /// Re-evaluate a location (e.g. when a confirmation window elapses).
    Evaluate { location: LocationId },
    /// Periodic heartbeat (freshness checks, housekeeping).
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerEvent {
    pub due_at: DateTime<Utc>,
    pub kind: TimerKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealthEvent {
    pub previous_state: Option<ProviderHealthState>,
    pub snapshot: ProviderHealthSnapshot,
}

/// Operator commands (dashboard/CLI). Always journaled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum OperatorCommand {
    KillSwitch { engaged: bool, reason: String },
}
