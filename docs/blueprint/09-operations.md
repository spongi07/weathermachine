# 38–43 · Testing, observability, recovery, paper, live, multi-location

## 38. Testing

`cargo test --workspace` (286 tests, plus the dashboard's) runs in CI against a PostgreSQL 18
service, alongside the dashboard's own tests and a container smoke test.

| Kind | Where |
|---|---|
| Unit | every crate (`#[cfg(test)]`) |
| Integration | `crates/*/tests/*.rs`, e.g. collector scenarios, HTTP fetcher, engine scenarios, backtests, storage, HTTP API, runtime |
| Property (`proptest`) | units, gate invariants, METAR parser, outcome mapping, fill model, risk outages, bucket probability bounds, **kernel safety invariant** |
| Adapter | AWC / TGFTP / api.weather.gov fixtures, Gamma mapping, CLOB books, WS messages |
| Replay | determinism, prefix stability (no look-ahead), journal round-trip |
| Failure | every provider behaviour below; storage down; stale data; kill switch; live mode |
| End to end | paper runtime against mock NOAA/Polymarket with and without PostgreSQL; container smoke test in CI |

Provider behaviours required by the brief:

| Behaviour | Test |
|---|---|
| HTTP 200 | `http_200_is_audited_and_cached`, `http_200_new_observations_are_emitted_and_persisted_with_raw_payload` |
| HTTP 429 + Retry-After | `http_429_with_retry_after_closes_the_gate`, `retry_after_http_date_is_parsed`, `http_429_retry_after_throttles_without_retry_storm_or_observation_events` |
| HTTP 500 / backoff | `http_500_backs_off`, `repeated_http_500_makes_provider_unavailable_and_opens_circuit` |
| Timeout | `timeout_is_classified`, `timeout_and_connection_failures_are_failures_not_data` |
| Connection reset / refused | `connection_reset_mid_response_is_a_failure_not_data`, `connection_refused_is_classified` |
| Malformed response | `malformed_response_is_preserved_and_degrades_health` |
| Duplicate response | `duplicate_response_produces_no_new_observation_events` |
| Corrected observation | `corrected_observation_emits_correction_and_preserves_versions` |
| Out-of-order observation | `out_of_order_observation_is_flagged` |
| Long outage | `long_outage_goes_stale_then_recovers` |
| Circuit breaker / recovery | `circuit_opens_after_repeated_failures_and_recovers`, `circuit_recovers_after_open_period` |
| Failover | `fails_over_to_secondary_when_primary_is_throttled` |
| One collector per station | `second_collector_for_same_station_is_refused`; PostgreSQL lease test |
| Request budget | `run_loop_request_budget_over_six_virtual_hours`, `gate_invariants` |
| **Outage can never trade** | `provider_outage_never_produces_a_trade`, `outages_never_approve_weather_positions`, `no_approval_without_healthy_fresh_complete_data` (plus its non-vacuity check), `untried_fallback_never_masks_a_throttled_primary`, `paper_runtime_end_to_end_without_storage_fails_closed` |
| Model training | `one_station_year_per_request_with_routine_and_specials`, `throttling_closes_the_gate_instead_of_retrying`, `an_error_page_is_not_mistaken_for_an_empty_year`, `trains_from_history_and_downloads_each_finished_year_once`, `a_failed_download_installs_nothing_but_keeps_finished_years`, `too_little_history_is_refused`, `missing_model_is_trained_from_history_and_loaded_after_restart` |

## 39. Observability

* **Metrics** (`/metrics`, Prometheus):
  * NOAA traffic: `nws_requests_total`, `nws_requests_per_hour` (gauge per
    provider), `nws_429_total`, `nws_failures_total`, `nws_cache_hits`,
    `nws_new_observations_total`.
  * Polymarket: `polymarket_requests_total`.
  * Engine and trading: `wm_engine_events_total`, `wm_engine_handle_seconds`,
    `wm_decisions_total`, `wm_orders_approved_total`,
    `wm_global_exposure_usd`, `wm_kill_switch`, `wm_storage_ok`,
    `wm_persist_failures_total`, `wm_persist_backlog_episodes_total`,
    `wm_journal_shed_total` (should stay 0), `wm_duplicate_observations_total`.
* **Request audit:** one `provider_requests` row per request (§15). The
  actual provider load is a SQL query away, e.g. requests per hour per
  provider.
* **Logs:** `tracing` with JSON output in containers; every request, health
  transition, decision and failure is structured.
* **Dashboard:** live SSE snapshot (500 ms) and the zero-JS `/lite` page.
  They show provider health and budgets, collector schedule, the knowledge
  delay of each report, gates with reasons, the decision log, positions and
  orders, and kernel latency (µs).
* **Strategy pages:** one per strategy (`/#/strategy/<id>`), fed by the
  snapshot's strategy catalog (settings straight from the configuration)
  and every strategy's latest evaluation of each bucket. Each shows
  strategy F's slot where it applies, the live evaluation with all
  blockers, this run's proposals, orders and evaluation trail.
  `GET /api/v1/strategies/{id}/log` (id or letter; `?days=`, default 7;
  `?download`) renders all of it as Markdown, plus the strategy's last days
  from `report paper`, with its settled P&L split by strategy. Its times are
  on the station's clock, named in each section's heading (UTC, labelled,
  when stations differ), and the high says when it was first and last
  reported (1 Oct: first 02:25, back at it from 11:25). The page's
  *Copy log* button copies it. It uses the clipboard API on https and
  localhost, `execCommand` on plain http, and otherwise opens a dialog with
  the text selected.
* **Reports to paste:** `GET /api/v1/research` lists and
  `GET /api/v1/research/{market|training}` serves the replay at traded
  prices and the training report from the data volume. Only these names are
  served, never a path, and files over 16 MB are refused. The dashboard
  copies them with the same button.
* **Probes:** `/healthz` (engine loop publishing) and `/readyz` (startup,
  storage, kill switch).
* **The paper run, day by day:** `weather-machine report paper` (or
  `/api/v1/report/paper`, at most 31 days, one report at a time, reused for
  a minute) reads the database back: METAR high, report delays and the
  source that delivered each report first, the day-1 forecast's error, the
  blockers of every evaluation per strategy, the closest calls and how they
  would have ended (by model EV for A, B and D; for the price rules E and F
  fewest blockers, then the highest ask), the model against the recorded
  book on the winning bucket, proposals, orders, fills, P&L settled at the
  METAR high, provider requests, health changes and logged events. The
  dashboard only holds the last 100 decisions of the current run.

## 40. Recovery architecture

**Crash-only design.** `panic = "abort"` in release: a crash restarts the
container (restart policy) instead of limping on.
* **Weather state:** on start, the last 60 h of observations are loaded from
  PostgreSQL. They warm the dedup ledger *and* rebuild the engine's day
  state, also for yesterday's market, which a restored position may still
  have to settle. The first AWC poll backfills 26 h, and the ledger turns
  repeats into duplicates (tested: 0 new, no duplicate rows).
* **Journal:** every engine input of a run is stored (`event_journal`), so
  the run can be replayed exactly or backtested with new parameters
  (`backtest --journal <run-id>`). Order-book updates in it are kept for 7
  days (`WM_JOURNAL_RETENTION_DAYS`), which bounds disk use; exact replay
  covers that window. The audit trail itself (observations, decisions,
  orders, fills, recorded books) is kept.
* **Persistence backlog:** engine records are written in coalesced
  transactions and retried on failure; a full queue holds them instead of
  dropping them. New positions stay blocked until the backlog is written.
* **Single writer:** the station lease is released on shutdown and dropped
  automatically when the connection dies, so a standby instance can take
  over.
* **Graceful shutdown:** SIGTERM/SIGINT stops producers, drains
  persistence, releases leases and records a `system_events` row
  (`stop_grace_period: 30s` in the stack).
* **Fail closed during recovery:** until the day series is complete and
  fresh again, the coverage and freshness gates block new positions.
* **Open paper positions are restored.** A restart begins a new run, but
  before its first live event the runtime rebuilds the paper book of the
  runs before it from PostgreSQL (`crates/wm-app/src/restore.rs`):
  * the fills of every market that settles today (UTC) or later, replayed
    into the position book with the strategy that opened each position, so
    the per-strategy caps and "one position a day" rules (F) still hold;
  * those markets, rebuilt from their latest stored Gamma payload with the
    mapping discovery uses and their stored rules review;
  * the filled cost (at the limit) of today's opening orders, which counts
    toward the daily new-exposure limit, and the realized P&L of today's
    restored sales, which counts toward the daily loss limit. (An opening
    order that ends unfilled — a resting order that expires, a
    fill-and-kill that finds no liquidity — gives its unfilled cost back to
    the daily limit; one still resting when the old run stopped ended with
    it.)

  A restored market whose settlement time has passed (the previous run
  settled it, or the restart fell across midnight) settles again at the
  next step, so its P&L counts toward today's loss limit once. Orders that
  were still resting when the old run stopped are not restored: the
  simulated venue lived in that process. The restore is logged as an alert
  and a `system_events` row (`kind = 'restore'`); a fill the book refuses is
  named there. If the database cannot be read, the run starts flat and says
  so in a critical alert. Live position reconciliation against the venue
  remains a Phase 14 prerequisite (§42).

## 41. Paper trading

`weather-machine run` (Portainer stack). The service runs the real
collectors, market discovery, WebSocket books and the same kernel, with the
`SimulatedExchange` as venue. Fills are simulated against live books with
latency, depth, fees and no mid fills. Settlement is from observed data
(labelled as such). Everything is persisted and visible on the dashboard.
Paper trading is Phase 13. It needs a trained model, recorded books and
the Phase 0 filter confirmation to be meaningful. The model is trained on
the host automatically when none exists: IEM history (one station-year per
request, 15 s apart, finished years cached on the data volume) → day-1
forecast history (Open-Meteo, newest year first, cached) → the peak-survival
study with the forecast evaluation (§19) → at least 730 usable days → model
and report written atomically → swapped into the running engine (no
restart). Until then the MODEL badge shows the progress and the engine does
not trade weather. Afterwards the model is retrained in the background when
the forecast has not been evaluated yet, the forecast model changed, or the
model is 30 days old (`retrain_after_days`); the current model keeps trading
until the new one is swapped in (MODEL ↻ on the dashboard).

## 42. Live trading

**Not implemented, by design.** The risk engine rejects `mode = live`
(`CheckId::Mode`), the runtime refuses to start in live mode, and
`DisabledLiveVenue` is the only live venue.

Prerequisites for Phase 14 (all must hold):
1. Positive out-of-sample EV with stable parameters across walk-forward
   folds, from **TrueOrderBook** backtests on our own recorded books, and
   ≥ N weeks of paper trading consistent with them.
2. A jurisdiction and compliance review. Polymarket restricts or limits
   users in many countries; reported 2026 status for the Netherlands is
   **close-only** (ASSUMPTION; verify with Polymarket's terms).
   `compliance_ok` must be satisfied explicitly.
3. A venue adapter implementing `ExecutionVenue`: EIP-712 signing of CLOB v2
   orders, pUSD collateral, `feeRateBps`, idempotent client order ids
   (already deterministic), order/fill/position reconciliation on start, and
   keys in a secret store (never in config or logs).
4. Human-approved resolution specs (`require_approved_resolution_spec =
   true`), an armed kill switch, and alerting.
5. Small, staged limits: start below $10/$100 until live slippage matches the
   simulator.

## 43. Multi-location expansion

**Adding a city requires only configuration**, not strategy code:
1. `configs/locations/<city>.toml`: station (ICAO), time zone, coordinates,
   routine report minutes, slug template, unit (°C/°F; buckets such as
   `86-87°F` are supported), observation sources.
2. Re-run Phase 0 for that station (rules text, resolution page, data
   source agreement). **Never assume the same provider or resolution
   source** (KNOWN FACT: Amsterdam itself switched between KNMI, Weather
   Underground and NOAA).
3. Train a station-specific model. The loader refuses a model trained for
   another station.
4. Risk: per-location limits exist (`max_location_exposure_usd`). Global
   exposure sums worst cases across locations. Cross-city correlation (shared
   weather regimes) is a research item (ASSUMPTION: independent until
   measured).

Scaling notes: one collector per station shares the per-provider gates. AWC
requests stay ≪ its documented limit even for dozens of stations, and a
single multi-station AWC request is possible later. The kernel is
single-threaded and deterministic, and handles an event in ~0.1 ms
(dashboard: kernel latency).
