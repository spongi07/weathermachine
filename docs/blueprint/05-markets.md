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

## 19. ForecastProvider and the day-1 forecast feature

**PURPOSE.** A predictive input that may sharpen P(high is final) — never a
substitute for the observation or resolution source, and used for trading
only after it has been *measured* to help.

```rust
pub trait ForecastProvider: Send + Sync {
    fn provider(&self) -> &ProviderId;
    fn gate(&self) -> &Arc<ProviderGate>;   // own rate-limit policy per provider
    fn model(&self) -> &str;
    fn fetch<'a>(&'a self, q: &'a ForecastQuery, max_gate_wait: Duration)
        -> BoxFuture<'a, Result<ForecastEvent, ForecastError>>;
}
// ForecastEvent { location, provider, model, issued_at, predicted_max, hourly, lead_days }
// ForecastProduct { provider, model, lead_days, ready_local_minute }   (wm-core::forecast)
```

**Product.** `OpenMeteoPreviousRuns` (`wm-weather::open_meteo`) requests
`temperature_2m_previous_day1` from the Open-Meteo Previous Runs API: for
every valid hour, the value of the model run initialised 24 hours earlier.
The *same product* serves years of training history and today's live
series. The "Historical Forecast API", by contrast, stitches together the
first hours of consecutive runs — a same-day analysis that leaks the
outcome into the past (that leak is what made the reviewed
"weatherforecaster" bot look profitable; see
[its review](../research/weatherforecaster-review.md)). Default model
`gfs_global` (2 m temperature archived since March 2021; most other models
since January 2024). One request per location per refresh (60 min) and one
per history year; `open_meteo` gate: 10 s spacing, concurrency 1, 500 a
day, backoff and circuit breaker. `WM_OPEN_METEO_API_KEY` switches to the
customer host; the key is never logged or stored (audit records carry a
key-free endpoint label and the HTTP layer strips URLs from errors).

**Knowledge rule.** A local day's series may be used from local midnight +
`ready_local_minute` (08:00) — in training for every sample, live only if the
series was *retrieved* at or after that instant (`ForecastProduct::usable_from`).
For lead 1, every value of day D comes from runs initialised before about
D 00:00 local, which are published hours before 08:00.

**Feature.** `forecast rise` = maximum of the day's hourly forecast over the
rest of the local day minus its maximum over the part already elapsed
(`ForecastDay::rise_tenths`). Level errors of the forecast (grid cell vs.
runway sensor, seasonal bias) cancel; what remains is whether the forecast
expects the day to get warmer later — the situation in which an observed
high is least likely to be final. A day not covered hourly and completely is
no forecast at all.

**Model.** The empirical model gains one refinement level,
`[MinutesSinceHigh, Drop, Season, LocalHour, ForecastRise]` (buckets
≤ −2.5 °C, cooling, ±0.4 °C, ≥ +0.5 °C), shrunk toward its parent like every
level. Without a forecast the level is skipped, so the model then *is* the
model without forecasts. `support` stays the count of the most specific
level without the forecast, so the strategies' support gate means the same
thing with and without it.

**Candidate structure (§36b).** A second, pre-registered structure reads the
forecast as **headroom**: the rest-of-day forecast maximum minus the
*observed* high (`forecast_headroom_tenths`; buckets ≤ −1.0 °C, −0.9…+0.4,
+0.5…+1.4, more). Unlike the rise it keeps the forecast's level, which is
what the market used on 28 September 2026: the observed 21 °C had reached the
forecast's 21.4 °C day maximum at 11:55
([replay](../research/replay-2026-09-28.md)). Its levels are
`[MinutesSinceFirstHigh] → [+Drop] → [+Season] → [+LocalHourFine]`, refined by
`ForecastHeadroom`. Each structure's forecast input is evaluated with its own
placebo; which structure the service uses is decided by the walk-forward
comparison of §36b.

**Evaluation and adoption (`wm-backtest::forecast_eval`).** Training joins
the forecast history with the METAR history and runs a **prequential**
(walk-forward, day by day) test: every decision point of day D — the first
report with the daytime high confirmed for 60 minutes — is scored with the
model as trained on the days *before* D, then D is learned. Both predictions
come from the same model (forecast level skipped / used), and a **placebo**
model is trained and scored identically with the forecast of 14 days earlier
shifted onto the day (realistic shape and season, no information about the
day). Rule fixed in advance: at least 365 scored days, and 95 % day-block
bootstrap intervals of the change in multi-class log loss per decision that
lie entirely below zero *both* against no forecast *and* against the
placebo. Otherwise the installed model has no forecast level and forecasts
cannot influence trading. Brier score, calibration, P(final) by rise bucket
and a constant-price trading proxy are reported, not optimized. The verdict
is stored in the model (`ForecastModelInfo`) and shown on the dashboard.

**Live.** `forecast_loop` fetches each location's series hourly and two
minutes after each day's ready time and sends a `ForecastUpdate`. The engine
keeps the latest series per (location, product) and derives the feature only
for the model's own product, under the knowledge rule; the dashboard shows
the series (dashed line), the rise and why it is or is not in use. The model
maintenance task retrains when the forecast was never evaluated, the product
changed, history was unavailable (after 6 h), the model was trained without
the structure comparison (§36b), or it is 30 days old — in the background,
swapping the new model into the running engine. The dashboard's forecast box
also shows the headroom.

* **KNOWN FACT (tests).** A forecast never changes the observed high, the
  views or settlement (`a_used_forecast_changes_probabilities_only_under_the_knowledge_rule`,
  `forecasts_never_change_the_observed_high_or_create_trades`); a series
  retrieved before the ready time and another model's series are ignored;
  on synthetic weather with forecastable evening surges the evaluation adopts
  an informative forecast and rejects one unrelated to the day, or known only
  after the decisions (`crates/wm-backtest/tests/forecast_eval.rs`); no day
  informs its own prediction, and without the refinement the model equals
  the one trained without forecasts.
* **ASSUMPTION.** Open-Meteo's `previous_day1` values come from runs
  initialised ≥ 24 h before valid time (Open-Meteo documentation), and those
  runs are published before 08:00 local.
* **HYPOTHESIS TO BACKTEST — decided automatically on the host.** The
  forecast rise improves P(high is final) at EHAM beyond the observed
  trajectory. The training report (`/data/research/eham-survival.md`)
  contains the evidence either way.

**FAILURE MODES.** Open-Meteo unreachable, throttled or rejecting (unknown
model, date outside the archive): live — the model runs without the forecast
(fail-safe, it then is the pre-forecast model) and one alert is raised until
recovery; training — the history walk stops at the archive start (remembered
in `archive-start.txt`), and a failed download leaves the model without the
forecast and schedules a retry. Partial or implausible series are no
forecast.

**TESTING.** Parser and client (`open_meteo` unit + wiremock tests: UTC,
units, nulls, rejections with the archive's first date, API key never in
records), `ForecastDay` coverage/DST tests, model refinement tests, kernel
knowledge-rule and model-swap tests, evaluation tests on synthetic truth,
training with a mock archive (newest year first, retry from the archive
start, cache, marker, unreachable source), runtime end-to-end (forecast shown
but unused by a model trained without it; model trained, evaluated and
swapped in without a restart).

## 20. Polymarket integration

**PURPOSE.** Typed, read-only market access with its own rate limits;
strategies never touch it.

| Capability | Implementation | Status |
|---|---|---|
| Market discovery, metadata, rules, outcomes, token ids | `GammaClient::events_by_slug`, `build_market` (`GET gamma-api.polymarket.com/events?slug=`) | ✅ |
| Order books | `ClobClient::book` (`GET clob.polymarket.com/book?token_id=`), REST fallback while the stream is down | ✅ |
| Prices / history | `ClobClient::prices_history` (`/prices-history`, research only) | ✅ |
| Trade history | `DataApiClient::trades` (`GET data-api.polymarket.com/trades`, taker side, all buckets of an event in one query; `limit`/`offset` ≤ 10,000, so a window that reaches the cap is halved and read again). Research only (`research market`) | ✅ |
| Trades, live books | `MarketStream` (`wss://ws-subscriptions-clob.polymarket.com/ws/market`: `book`, `price_change`, `tick_size_change`, `last_trade_price`), local book with invalidation on disconnect | ✅ |
| Split / merge / redeem / neg-risk convert | `wm_polymarket::ctf` economics (pure) | ✅ model; on-chain calls Phase 14 |
| Orders, cancellations, fills, positions, balances | `ExecutionVenue` port; `SimulatedExchange` (paper); `DisabledLiveVenue` (live) | paper ✅, live ⛔ Phase 14 |

* **Own limits (KNOWN FACT).** Gamma (1 s spacing), CLOB REST (250 ms,
  concurrency 2), the Data API (500 ms, one at a time; documented limit 200
  requests per 10 s) and WebSocket reconnects (5 s) each have their own
  gate. None of them inherits NOAA's rules, and vice versa.
* **Market end time (KNOWN FACT, [clob-client#331](https://github.com/Polymarket/clob-client/issues/331)).**
  Gamma's `endDate` of a daily weather market is a nominal 12:00 UTC on the
  target day, although the market trades until the day's data are final.
  `build_market` therefore sets the end to the later of `endDate` and the
  end of the local resolution day. Used as-is, it made the risk engine
  reject every intent after 14:00 Amsterdam summer time.
* **Quiet books.** The market channel sends only changes. Each PONG to the
  client's heartbeat emits a `MarketStreamHeartbeat` (with the connection's
  start), and the engine marks every book received on that connection as
  confirmed (`OrderBook::confirmed_at`). A book nobody touched stays current
  while its connection is alive; after a reconnect it is current again only
  once the new connection has sent it. Heartbeats are pruned with the book
  journal.
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
