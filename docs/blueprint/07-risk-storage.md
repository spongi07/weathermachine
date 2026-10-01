# 29–31 · Risk engine, exposure model, PostgreSQL schema

## 29. RiskEngine

**PURPOSE.** The last, independent gate before any order. Only it can
produce an `ApprovedIntent`, and only such an intent can reach a venue
(type-level guarantee: `VenueOrder: From<&ApprovedIntent>`).

**INPUTS.** `TradeIntent` plus `RiskInputs { now, mode, kill_switch,
compliance_ok, storage_ok, execution_ok, weather: WeatherStatus, market, book,
portfolio }`. **OUTPUTS.** `RiskDecision::Approved(ApprovedIntent)` or
`Rejected { reasons: Vec<RiskRejection> }`. *All* failing checks are
reported, never just the first. Client order ids are deterministic
(`wm-<run>-<decision>-<leg>`), which makes resubmission idempotent.

| Check (`CheckId`) | Rule (defaults) | Brief requirement |
|---|---|---|
| KillSwitch | engaged ⇒ reject | kill switch |
| Mode | live ⇒ reject (Phase 14 gate) | — |
| Compliance | live needs a jurisdiction gate | — |
| ResearchOnly | research strategies only in backtest | — |
| Storage | paper/live need working audit storage | FAIL CLOSED |
| Execution | venue unhealthy ⇒ reject | execution health |
| WeatherHealth | source must be **Healthy** (opening, weather-dependent) | provider throttled ⇒ no new position |
| WeatherFreshness | latest report ≤ 40 min (exact comparison) | weather stale ⇒ no new position |
| WeatherCoverage | no gap > 75 min in the local day series | (incomplete day may hide the high) |
| CorrectionCooldown | 10 min after a correction | — |
| MarketData | book present, ≤ 15 s old (received or confirmed by the stream's heartbeat), correct token | market data stale ⇒ no new position |
| Spread | ≤ 0.05 (a strategy may have its own: K ≤ 0.10) | maximum spread |
| Liquidity | FAK/FOK: ask depth ≥ size at limit | minimum liquidity |
| Tick / PriceBounds / MinSize | on tick; 0.01 ≤ price ≤ 0.99; ≥ market minimum | — |
| MarketStatus | accepting orders, not closed, before end time (a daily market ends no earlier than its local day) | — |
| ResolutionSpec | machine-tradable; human-approved hash if required (live) | resolution rules |
| Partition | bucket set valid | correlated risk |
| Duplicate | no live order on the token; decision id not reused | duplicate orders |
| PositionSize | cost ≤ $10, or the strategy's own cap (F $100, G $30, K $25) | $10 per position |
| GlobalExposure | post-trade worst case ≤ $400 | room for F–K together |
| MarketExposure / LocationExposure / StrategyExposure | ≤ $400 / $400 / $60 (optional; per strategy: F $110, G $120, H $30, I $30, J $80, K $50) | future limits |
| DailyNewExposure / DailyLoss | ≤ $600 new per day (an order that ends unfilled gives its unfilled cost back); stop after −$100 realised | daily loss |
| OrderRate | ≤ 20 orders/min (G and J quote several buckets after a report) | — |
| Oversell | never sell more than held | — |

*Forecast freshness* is enforced by construction (§19): a model that adopted
the day-1 forecast uses a series only for the local day it covers, only when
it was retrieved after that day's ready time and only if it covers the day
completely. Anything else means "no forecast", and the model then gives the
distributions of the model without forecasts — the forecast can never be
stale, only absent.

Weather gates apply to *opening, weather-dependent* intents. Reductions and
unwinds are never blocked by weather data, since reducing risk is always
allowed.

**FAILURE MODES.** Every missing input is a rejection, never a default.
Rejected proposals are audited, and identical repeats within 60 s are counted
but not re-recorded (the log stays readable under fast market data).
**TESTING.** `wm-risk/tests/risk_engine.rs` covers every gate
(`each_gate_rejects`), plus stale books, correction cooldown, duplicates,
reduce intents under stale weather, daily loss, order rate and TOML decimals.
The property test `outages_never_approve_weather_positions` checks that any
non-Healthy, stale or kill-switched state rejects. The kernel scenarios and the
`no_approval_without_healthy_fresh_complete_data` property test drive random
health/data interleavings through the full engine.

## 30. Exposure model: shared limits and per-strategy caps

```toml
[risk]
position_size_usd = "10.00"            # any strategy without its own caps
global_max_exposure_usd = "400.00"     # F ≤ $110 + G $120 + H $30 + I $30 + J $80 + K $50
max_market_exposure_usd = "400.00"     # one market a day: the same as the location
max_location_exposure_usd = "400.00"
max_strategy_exposure_usd = "60.00"
max_daily_new_exposure_usd = "600.00"  # unfilled cost is given back
max_daily_loss_usd = "100.00"
max_spread = "0.05"
max_orders_per_minute = 20

[risk.strategy_caps.F_peak_slot]       # a fixed 100 shares at ≤ 0.95
position_size_usd = "100.00"
max_strategy_exposure_usd = "110.00"
[risk.strategy_caps.G_tail_seller]     # $30 a resting NO bid, ≤ 4 buckets
position_size_usd = "30.00"
max_strategy_exposure_usd = "120.00"
[risk.strategy_caps.H_next_degree]     # $10 a trade, ≤ 3 positions
position_size_usd = "10.00"
max_strategy_exposure_usd = "30.00"
[risk.strategy_caps.I_middle_fade]     # $10 a trade, ≤ 3 positions
position_size_usd = "10.00"
max_strategy_exposure_usd = "30.00"
[risk.strategy_caps.J_morning_maker]   # $10 a quote, both sides of ≤ 4 buckets
position_size_usd = "10.00"
max_strategy_exposure_usd = "80.00"
[risk.strategy_caps.K_knmi_nowcast]    # $25 a trade, books up to 0.10 wide
position_size_usd = "25.00"
max_strategy_exposure_usd = "50.00"
max_spread = "0.10"
```

**Per-strategy caps.** `strategy_caps` gives one strategy (by id) its own
position, per-market and per-strategy caps and its own spread limit; an
unset one falls back to the default. The portfolio caps (global, market,
location, daily new exposure, daily loss) count every strategy together.
Configuration validation refuses an enabled strategy whose one order would
not fit every cap it meets (position, market, strategy, global, location,
daily new exposure); a strategy cap must be positive and ≤ the global cap.

**Unfilled orders give their cost back.** The daily new exposure counts an
opening buy when the risk engine approves it. When the order ends without
filling in full — a resting quote of G or J that expires before a report, a
fill-and-kill that finds less than it asked for — its unfilled cost is
taken off again (same UTC day, never below zero), so quotes posted and
withdrawn every half hour do not use up the day's limit. A restart restores
the counter from the filled cost of the day's opening orders
(`opening_orders_since`).

Consequences, by design:

* The per-market cap is the event's worst case over *every* strategy's
  legs; F–K share $400 in one event, so a strategy may find it full.
* One token holds one position: a strategy does not buy a token another
  strategy holds or has an order on (`holds_or_pending`, the Duplicate
  check). K, for instance, does not add to I's NO of the same bucket.
* One F loss (≈ $95) stays under the $100 daily-loss stop; a second loss
  on the same UTC day stops all new positions for the rest of that day.

**KNOWN FACT (design, tested).** Buckets of one daily event are mutually
exclusive and exhaustive, so exposure is computed **per scenario**. For each
possible final bucket, `wm_risk::event_exposure` sums every leg's PnL
(positions plus pending buys treated as filled at their limit), and the event's
exposure is the worst-case loss. This captures correlation exactly:
* NO 19 + NO 20 + NO 21: at most one can lose, so the risk is not the sum of
  costs.
* YES 18 + NO 18 is a hedge.
* Global exposure = Σ worst cases over events (different days and locations
  are treated as independent until research says otherwise; ASSUMPTION).

Amounts are exact micro-dollars; configuration uses decimal strings.
**TESTING.** `correlated_legs_in_one_event_use_worst_case_not_sum`, exposure
unit tests for hedges and pending buys, and the global cap in `each_gate_rejects`.
`a_strategy_with_its_own_caps_buys_100_shares_while_the_others_keep_theirs`,
`a_strategy_may_have_its_own_spread_limit` and
`strategy_caps_parse_and_are_validated` cover the per-strategy caps;
`unfilled_cost_returns_to_the_daily_new_exposure_of_its_day` and
`wm-backtest/tests/maker_session.rs` the cost given back.

## 31. PostgreSQL schema

`migrations/0001_initial.sql`, applied automatically (`sqlx::migrate!`, embedded in the binary).

| Area | Tables |
|---|---|
| Reference | `stations`, `locations` |
| Provider audit | `provider_requests` (every request: endpoint, times, status, latency, bytes, cache, retries, throttled, error class, gate wait, payload hash), `provider_health_events` |
| Weather | `raw_weather_payloads` (verbatim, unique by provider+hash, seen count), `weather_observations` (versioned, unique `(station, observed_at, report_type, version)`), view `weather_observations_current`, `weather_corrections`, `forecast_snapshots`, `temperature_states`, `peak_candidates` |
| Markets | `market_rules` (verbatim + SHA-256 + parsed spec + human review), `markets`, `market_outcomes`, `market_snapshots` (raw Gamma payloads), `orderbook_snapshots`, `market_trades` |
| Decisions and trading | `strategy_runs` (config + model per run), `decision_snapshots` (inputs/outputs JSON per decision), `signals`, `orders`, `fills` (FK → orders), `positions` |
| Research | `backtest_runs`, `backtest_trades` |
| Operations | `system_events`, `event_journal` (every engine input per run, `(run_id, seq)` primary key) |

**Principles.** Append-only where history matters (observations, decisions,
journal). Raw payloads are always kept. Every decision row links to its run
and config. Advisory locks provide per-station collector leases.

**Engine writes.** The engine loop never waits for the database. It hands
each batch (journal entries, decisions, orders, fills, book snapshots) to a
writer task, which coalesces whatever is queued (up to 20,000 events) into
**one transaction** with multi-row inserts. A failed write rolls back
completely and the same batch is retried with backoff (1 s → 30 s), so
nothing is lost or duplicated. If the queue is full, the engine holds the
batch and resends it; nothing is dropped. Only if storage stays stalled
beyond 200,000 held events are order-book entries shed (alert
"journal gap", metric `wm_journal_shed_total`); decisions, orders, fills
and all other events are never shed.

**Bounded growth.** Order-book updates are ~99 % of engine inputs (about
1.5 million a day for three days of Amsterdam markets). They are journaled
with 5 levels per side and deleted from the journal after
`journal_book_retention_days` (7; `WM_JOURNAL_RETENTION_DAYS`, 0 = keep).
Market history lives on in `orderbook_snapshots` (changes only, ≤ 1 per
10 s per token). Nightly backups exclude the replay journal by default
(`WM_BACKUP_JOURNAL=true` includes it).

**FAILURE MODES.** A write failure, or a batch the writer has not accepted
yet, turns `storage_ok` off, which blocks new positions until the backlog is
written (one alert per episode, one on recovery). Migrations run before any
collector starts.
**TESTING.** `wm-storage/tests/pg_store.rs` (each test in a fresh database):
ingest round-trip, versioning, journal round-trip, rules approval, lease
exclusivity, `engine_batch_is_atomic_and_old_book_updates_are_pruned`.
Runtime unit tests: `a_full_queue_holds_batches_blocks_trading_and_alerts_once`,
`shedding_drops_only_order_book_entries`,
`recorder_stores_changes_at_most_once_per_interval_and_keeps_the_last`. The
runtime PostgreSQL test covers persistence, warm restart and no duplicate
rows.
