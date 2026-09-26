# 1–5 · Architecture, workspace, events, domain types, traits

## 1. System architecture

**PURPOSE.** Trade daily highest-temperature markets only when observed
weather, a *measured* probability and every risk gate agree, and prove all of
it after the fact.

```text
 NOAA AWC Data API ─┐                                  Polymarket Gamma ─┐  CLOB REST ─┐  market WS ─┐
 NWS TGFTP ─────────┤  one ProviderGate per provider    (discovery+rules) │  (books)    │  (books,    │
 api.weather.gov* ──┤  (rate limit · Retry-After ·                        │             │   trades)   │
                    │   backoff · circuit · budget)                       ▼             ▼             ▼
          StationCollector (exactly one per station,        Market discovery + rules capture → MarketSnapshot
          PostgreSQL advisory lease across processes)                            │
                    │ parse → normalize → dedup ledger                            │
                    │ → persist (raw payload + request audit + versions)          │
                    ▼                                                            ▼
            WeatherObservation / WeatherCorrection / ProviderHealthChanged / OrderBookUpdate / Timer / Operator
                    └──────────────────────────────┬─────────────────────────────┘
                                                   ▼  (knowledge-time sequencer, event journal)
             ┌──────────────────────── deterministic kernel  (wm-engine::Engine) ─────────────────────────┐
             │ TemperatureStateEngine → PeakDetectionEngine (per resolution view) → ProbabilityModel     │
             │ → Strategy A / B / C + UnwindEngine → proposals → RiskEngine → ApprovedIntent (typed gate) │
             └──────────────────────────────────────────┬────────────────────────────────────────────────┘
                                                        ▼
                            SimulatedExchange (backtest · demo · paper)   |   DisabledLiveVenue (live, Phase 14)
                                                        ▼
                      decision audit log · orders · fills · positions → PostgreSQL · dashboard (SSE) · metrics
```
\* disabled until Phase 0 confirms it serves EHAM.

**INPUTS.** Provider HTTP/WS payloads, operator commands, configuration.
**OUTPUTS.** Decision records, approved intents (simulated fills), positions,
provider audit, metrics, dashboard snapshots.

**RUST INTERFACE.** `wm_engine::Engine::handle(&EventEnvelope) -> EngineOutput`
is the only entry point into trading logic. Everything else is an adapter.

**KNOWN FACT (this repository).** One kernel runs in backtest, demo and paper.
The test `replay_is_deterministic` and the prefix-stability test in
`wm-backtest/tests/backtest_synthetic.rs` enforce determinism and no
look-ahead.

**FAILURE MODES → behaviour.** Provider outage, throttling or stale data
blocks new weather-dependent positions (fail closed). The same happens for a
storage failure (audit impossible), a full persistence queue, an incomplete
day series, missing or stale books, an unverified resolution view, a missing
model, and the kill switch.

**TESTING.** Kernel scenarios (`wm-engine/tests`), end-to-end runtime with
mock providers and PostgreSQL (`wm-app/tests/runtime_paper.rs`), synthetic
multi-day backtests.

## 2. Rust workspace

**PURPOSE.** Crate boundaries follow dependency direction and failure
isolation, not layers for their own sake.

| Crate | Responsibility | Depends on |
|---|---|---|
| `wm-core` | Domain types, exact units, ids, time/clock, events, ingest port | serde, chrono(-tz), sha2, uuid |
| `wm-net` | HTTP client, per-provider `ProviderGate`, cache, User-Agent | wm-core, reqwest, tokio |
| `wm-weather` | METAR parser, AWC/TGFTP/api.weather.gov sources, dedup ledger, health, polling policy, `StationCollector`, forecast port | wm-core, wm-net |
| `wm-polymarket` | Gamma discovery, rules capture/parse, outcome mapping, CLOB books/history, market WebSocket, CTF economics (read-only) | wm-core, wm-net |
| `wm-strategy` | Temperature state, peak detection, probability models, EV, strategies A/B/C, unwind | wm-core |
| `wm-risk` | Risk engine, scenario exposure, `ApprovedIntent` type gate | wm-core |
| `wm-execution` | Order manager, fill model, simulated exchange, venue port | wm-core, wm-risk |
| `wm-engine` | Deterministic kernel wiring the above | strategy, risk, execution |
| `wm-storage` | PostgreSQL (SQLx), migrations, repositories, advisory leases | wm-core, sqlx |
| `wm-backtest` | Replay/simulation session, backtests, IEM import, research | engine, execution, weather |
| `wm-dashboard-api` | JSON contract for the UI (serde only, wasm-friendly) | serde |
| `wm-app` | Binary `weather-machine`: config, runtime, demo, HTTP/SSE, CLI | all |
| `ui/` (own workspace) | Leptos dashboard compiled to `wasm32` | wm-dashboard-api |

**Deviations from the candidate layout (ASSUMPTION: better boundaries).**
* `wm-config` is folded into `wm-app`, the only consumer of files and
  environment.
* `wm-forecast` became the `forecast` module of `wm-weather`: same
  rate-limit/HTTP plumbing, one trait, no implementation until Phase 11.
* `wm-monitoring` became the `metrics` facade plus `wm-app::telemetry`; a
  crate for two functions would be a micro-crate.
* `wm-net` and `wm-engine` are new. The first makes rate limiting impossible
  to bypass; the second makes the kernel reusable by backtest and live alike.

**FAILURE MODES.** Circular dependencies are impossible by construction:
strategies cannot reach the network (`wm-strategy` does not depend on
`wm-net` or `wm-polymarket`).

**TESTING.** `cargo test --workspace` (unit + integration + property tests);
CI also builds `ui/` for `wasm32`.

## 3. Event architecture

**PURPOSE.** A single, replayable input stream.

```rust
pub struct EventEnvelope {
    pub seq: u64,                    // assigned by the sequencer
    pub available_at: DateTime<Utc>, // knowledge time: earliest moment WM could act
    pub recorded_at: DateTime<Utc>,
    pub source: EventSource,         // Live | Replay | Synthetic | Operator
    pub event: WeatherMachineEvent,
}
pub enum WeatherMachineEvent {
    WeatherObservation(ObservationEvent), WeatherCorrection(CorrectionEvent),
    ForecastUpdate(ForecastEvent), MarketSnapshot(MarketSnapshotEvent),
    OrderBookUpdate(OrderBookEvent), MarketTrade(MarketTradeEvent),
    OrderUpdate(OrderUpdateEvent), Timer(TimerEvent),
    ProviderHealthChanged(ProviderHealthEvent), Operator(OperatorCommand),
}
```

* **Ordering (KNOWN FACT, `wm-backtest::replay`).** The replay queue releases
  events by `(available_at, priority, insertion)`. Priority puts health and
  operator events first, then markets, order updates, market data,
  observations, and timers last. Nothing with `available_at > now` is visible.
* **Live stamping.** The engine loop re-stamps live events with its receipt
  time (monotonic), so the journal replays in exactly the order it ran.
* **Polling ≠ observations (KNOWN FACT, collector tests).** An HTTP response
  that brings no new or changed report produces *no* weather event, so no
  evaluation is triggered by a mere request.

**FAILURE MODES.** Out-of-order sequence numbers are counted
(`stats.out_of_order_seq`). A full event channel applies backpressure to
producers. Journal write failures turn `storage_ok` off.

**TESTING.** `queue_orders_by_knowledge_time_then_priority`, backtest
prefix-stability test, and journal round-trip in `wm-storage/tests/pg_store.rs`.

## 4. Domain types

**PURPOSE.** Make invalid states unrepresentable and money exact.

| Type | Representation | Why |
|---|---|---|
| `TempC` | `i32` tenths of °C, ICAO half-up rounding to whole degrees | T-group precision; exact comparisons |
| `Price` | `u32` micro-units (6 dp), validated 0..=1 | CLOB ticks (0.01/0.001) are exact |
| `Usd`, `Shares` | `i64` micro-units | collateral has 6 decimals; no float drift |
| `Probability` | `f64` clamped, NaN→0 | model output only; never used for money |
| ids (`StationId`, `LocationId`, `TokenId`, `EventSlug`, `ClientOrderId`, `RunId`, `DecisionId`) | validated newtypes | no stringly-typed mix-ups |
| `Observation` / `ObservationKey` | station + observed_at + report type (+ version, content hash) | dedup identity (§13) |
| `DailyTemperatureMarket`, `TemperatureBucket`, `MarketOutcome` | partition-validated buckets | correlated exposure (§30) |
| `ResolutionSpec`, `RulesText` (SHA-256) | parsed + verbatim rules | §18 |
| `TradeIntent` → `ApprovedIntent` | only `wm-risk` can construct the latter | type-level risk gate |

**ASSUMPTION.** `rust_decimal` is not needed. Integer micro-units give exact
arithmetic with deterministic rounding direction (fees up, proceeds down),
which is simpler and faster (see [02-dependencies.md](02-dependencies.md)).

**FAILURE MODES.** Overflow saturates or errors explicitly. Non-finite floats
never reach money types. Bucket partitions that overlap or leave gaps reject
the market.

**TESTING.** Unit and property tests in `wm-core` (units round-trips,
partition validation), plus `proptest` in the risk engine.

## 5. Rust traits

**PURPOSE.** Ports at every external boundary; everything behind them is replaceable in tests.

```rust
// wm-weather — the only way to obtain observations
pub trait ObservationSource: Send + Sync {
    fn provider(&self) -> &ProviderId;
    fn gate(&self) -> &Arc<ProviderGate>;          // every request passes this gate
    fn fetch<'a>(&'a self, station: &'a StationId, max_gate_wait: Duration)
        -> BoxFuture<'a, Result<SourceFetch, SourceError>>;
}
// wm-weather — predictive inputs only (§19)
pub trait ForecastProvider: Send + Sync {
    fn provider(&self) -> &ProviderId;
    fn gate(&self) -> &Arc<ProviderGate>;
    fn model(&self) -> &str;
    fn fetch<'a>(&'a self, q: &'a ForecastQuery, max_gate_wait: Duration)
        -> BoxFuture<'a, Result<ForecastEvent, ForecastError>>;
}
// wm-core — persistence port used by collectors (PostgreSQL, memory, null)
pub trait IngestSink: Send + Sync {
    fn persist(&self, batch: IngestBatch) -> BoxFuture<'_, Result<(), SinkError>>;
}
// wm-core — time is injected (system, tokio, manual) so tests control it
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
    fn monotonic(&self) -> Duration;
}
// wm-strategy — pure decision logic, identical in every mode
pub trait Strategy: Send {
    fn id(&self) -> &StrategyId;
    fn research_only(&self) -> bool { false }
    fn enabled(&self) -> bool;
    fn evaluate(&mut self, ctx: &StrategyContext<'_>) -> StrategyOutput;
}
pub trait ProbabilityModel: Send + Sync {
    fn id(&self) -> &str;
    fn distribution(&self, features: &PeakFeatures) -> Option<IncrementDistribution>;
}
// wm-execution — accepts only risk-approved orders (VenueOrder: From<&ApprovedIntent>)
pub trait ExecutionVenue: Send + Sync {
    fn name(&self) -> &str;
    fn submit(&self, order: VenueOrder) -> BoxFuture<'_, Result<VenueAck, VenueError>>;
    fn cancel(&self, id: ClientOrderId) -> BoxFuture<'_, Result<(), VenueError>>;
}
```

**FAILURE MODES.** A trait implementation cannot skip rate limiting (the gate
is part of the port) or risk (the venue takes only `ApprovedIntent`-derived
orders).

**TESTING.** Mock-server adapters (wiremock), `ManualClock`-driven collector
scenarios, `MemoryIngestSink`, fixed probability models in kernel tests.
