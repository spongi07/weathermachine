# 18–21 · Resolution, forecasts, Polymarket, outcome mapping (+ CTF mechanics)

## 18. ResolutionSource architecture

**PURPOSE.** Keep three concepts strictly apart:

| Concept | Meaning | Type |
|---|---|---|
| **ResolutionSource** | What the *contract* settles on, per market | `ResolutionSourceKind` inside `ResolutionSpec` |
| **WeatherObservationSource** | What we poll to learn the weather early | `ObservationSource` (§8) |
| **ForecastSource** | Predictive inputs | `ForecastProvider` (§19) |

**INPUTS.** Verbatim rules text and resolution URL of each Gamma event.
**OUTPUTS.**

```rust
pub struct RulesText { text, resolution_source_url, sha256 }        // verbatim + SHA-256
pub struct ResolutionSpec {
    source: ResolutionSourceKind,        // NoaaWrhTimeseries{site,url} | WundergroundDaily | Knmi | Unrecognized
    fallback: Option<ResolutionSourceKind>,
    extreme: MarketExtreme, unit: TempUnit, whole_degrees: bool, timezone,
    filters: Vec<ObservationFilter>,     // AllRows | MinuteWindow{start,end}
    filter_certainty: FilterCertainty,   // Unconfirmed until Phase 0
    revision_policy: RevisionPolicy,     // e.g. until the next day's first datapoint
    unrecognized_clauses: Vec<String>,   // any ⇒ not machine-tradable
    review: SpecReviewStatus,            // AutoParsed | Approved | Rejected (human, by hash)
    parser_version, notes, ..
}
```

* **Conservative parsing (KNOWN FACT, `wm-polymarket::rules`).** Sentences
  that mention decision-relevant concepts the parser cannot map are recorded
  as unrecognised. Examples: rounding, decimals, UTC/GMT/time zones,
  exclusions, averages, METAR/SPECI, sensors, voids/cancellations, 50-50
  outcomes, station changes. Any unrecognised clause makes the market
  non-tradable until a human approves that exact rules hash.
* **Multiple views.** Every candidate filter becomes a *view*. Unless the
  filter is confirmed, the engine requires every view to be evaluable and to
  agree on the day's high. It uses the minimum win probability and the
  maximum loss probability across views.
* **Paper settlement** uses the engine's observed final value under the
  primary view, recorded as observed, never as the venue's resolution.
  Backtests settle the same way from historical data.

**FAILURE MODES.** A rules change produces a new hash, which invalidates any
prior approval. An unknown source kind means the market is never traded. A
parser upgrade bumps `RULES_PARSER_VERSION`.
**TESTING.** Parser tests on verbatim NOAA, Weather Underground and KNMI
Amsterdam rules, risky-word detection without false positives ("underground",
"specified"), and approval round-trip in PostgreSQL.

## 19. ForecastProvider

**PURPOSE.** Predictive features for Phase 11, never a substitute for the
observation or resolution source.

```rust
pub trait ForecastProvider: Send + Sync {
    fn provider(&self) -> &ProviderId;
    fn gate(&self) -> &Arc<ProviderGate>;   // own rate-limit policy per provider
    fn model(&self) -> &str;
    fn fetch<'a>(&'a self, q: &'a ForecastQuery, max_gate_wait: Duration)
        -> BoxFuture<'a, Result<ForecastEvent, ForecastError>>;
}
// ForecastEvent { location, provider, model, issued_at, predicted_max, hourly }
```

* **KNOWN FACT (test `forecasts_never_change_the_observed_high_or_create_trades`).**
  Injecting a forecast of 30 °C changes neither the observed high, the views,
  the settlement value nor any decision.
* Candidates (Phase 11): KNMI (open data platform), ECMWF, GFS, each behind
  its own gate. Archives must provide *issue time* so replays avoid
  forecast look-ahead (§33).
* **HYPOTHESIS TO BACKTEST.** Adding a forecast of the remaining maximum
  improves the calibration of P(high is final). It enters the model as a
  feature only if walk-forward evaluation shows a stable improvement.

**TESTING.** `StaticForecastProvider` (archives and tests); kernel isolation test.

## 20. Polymarket integration

**PURPOSE.** Typed, read-only market access with its own rate limits;
strategies never touch it.

| Capability | Implementation | Status |
|---|---|---|
| Market discovery, metadata, rules, outcomes, token ids | `GammaClient::events_by_slug`, `build_market` (`GET gamma-api.polymarket.com/events?slug=`) | ✅ |
| Order books | `ClobClient::book` (`GET clob.polymarket.com/book?token_id=`), REST fallback while the stream is down | ✅ |
| Prices / history | `ClobClient::prices_history` (`/prices-history`, research only) | ✅ |
| Trades, live books | `MarketStream` (`wss://ws-subscriptions-clob.polymarket.com/ws/market`: `book`, `price_change`, `tick_size_change`, `last_trade_price`), local book with invalidation on disconnect | ✅ |
| Split / merge / redeem / neg-risk convert | `wm_polymarket::ctf` economics (pure) | ✅ model; on-chain calls Phase 14 |
| Orders, cancellations, fills, positions, balances | `ExecutionVenue` port; `SimulatedExchange` (paper); `DisabledLiveVenue` (live) | paper ✅, live ⛔ Phase 14 |

* **Own limits (KNOWN FACT).** Gamma (1 s spacing), CLOB REST (250 ms,
  concurrency 2) and WebSocket reconnects (5 s) each have their own gate.
  None of them inherits NOAA's rules, and vice versa.
* **KNOWN FACT (Polymarket code/docs).** Since the CTF-exchange v2 migration
  (2026-04-28) collateral is pUSD (6 decimals). Orders carry `feeRateBps`.
  Live order placement would need EIP-712 signing of the v2 order struct,
  which is not implemented by design.
* **ASSUMPTION (secondary reports).** The venue's historical order-book
  endpoint stopped returning new snapshots around 20 Feb 2026. Weather
  Machine therefore records its own books (`orderbook_snapshots`: a token's
  top 5 levels per side whenever the book changed, at most once per 10 s,
  the last change of a burst included) for true order-book backtests later.
  The engine itself keeps 5 levels per side (`ENGINE_BOOK_DEPTH`): positions
  are ≤ $10, so fills and depth checks never reach deeper.

**FAILURE MODES.** WS disconnect → local books invalidated → stale-book gate
blocks trading → REST fallback polls the books → reconnect with a gated
backoff. Malformed payloads are logged and ignored. Mapping failures reject the
event (never guessed).
**TESTING.** Gamma fixture mapping; WS message parsing for both price-change
formats; local book deltas; stream plus discovery against mock servers; REST
fallback in the end-to-end runtime test.

## 21. TemperatureOutcomeMapper

**PURPOSE.** Turn outcome labels into typed buckets that partition the temperature line.

* `parse_bucket_label` / `map_outcome` handle `18°C`, `13°C or below`,
  `24°C or higher`, `≤13°C`, `≥24°C`, `86-87°F` / `86–87°F`, negative
  values (`-2°C`, `−2°C`), and fall back to the market question.
* `DailyTemperatureMarket::validate_partition` checks buckets over
  −80…140: no overlaps, no gaps, exactly one bucket per value. An unmappable
  or inconsistent event is **rejected as a whole**.
* YES/NO token ids come from `outcomes` + `clobTokenIds` by name, never by
  position.

**TESTING.** Celsius and Fahrenheit label tests, ambiguity refusal, question
fallback, and property tests (`exact_roundtrip`, `never_panics` on arbitrary
input).

## Split / merge / redeem — actual mechanics (brief §18)

KNOWN FACT, from Polymarket's `ctf-exchange-v2` and `neg-risk-ctf-adapter` sources:

| Operation | Effect |
|---|---|
| `split(amount)` | `amount` collateral → `amount` YES + `amount` NO of one binary market |
| `merge(amount)` | `amount` YES + `amount` NO → `amount` collateral |
| `redeem` | after resolution, each winning token pays 1 collateral |
| neg-risk `convertPositions(marketId, indexSet, amount)` | `amount` NO of *m* of the *n* questions → `amount × (m − 1)` collateral + `amount` YES of each of the other *n − m* questions, minus a market fee parameter |
| CLOB matching | complementary orders match via MINT (a buy YES at *p* against a buy NO at 1 − *p* splits collateral) and via MERGE (two sells) |

**Consequence (derived, KNOWN FACT given the above).** In a consistent book,
`ask(YES) ≈ 1 − bid(NO)`. Then "split, then sell the unfavoured side" costs
the same as buying the favoured side directly, plus the sale's taker fee and
network gas (`wm_strategy::ev::split_then_sell_effective_price` vs
`direct_buy_effective_price`). Strategy C is therefore **research-only**
until a backtest shows a benefit the direct route cannot capture, such as
timing or liquidity (§27).

Settlement and costs: redemption requires an on-chain transaction (gas in
POL). The CLOB taker fee is `shares × feeRate × p × (1 − p)` (ASSUMPTION:
secondary reports give feeRate 0.05 for weather in 2026; confirm per market).
Weather Machine models fees exactly and rounds fees up.
