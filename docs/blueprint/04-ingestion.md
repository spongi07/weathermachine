# 9–17 · Rate-limit-safe ingestion

## 9. NWS rate-limit-safe architecture

**PURPOSE.** Collect every EHAM report as early as is *respectful*, and never
more often.

```text
 StationCollector (1 per station; in-process registry + PostgreSQL advisory lease)
   └─ PollingPolicy.next_poll()  → when (arrival windows after HH:25/HH:55, never faster when late)
       └─ ObservationSource.fetch()
            └─ HttpFetcher.get()  → ProviderGate.acquire()   (the ONLY path to the network)
                 order of checks: daily budget → circuit breaker → Retry-After → backoff (equal jitter)
                                  → minimum spacing (× politeness after throttling) → concurrency
```

**KNOWN FACT (code + tests).**
* One gate per *provider*, shared by all stations and consumers. Ten
  strategies watching EHAM cause zero requests: strategies have no network
  access at all.
* One collector per station, enforced twice. `CollectorRegistry` rejects a
  second in-process claim. The PostgreSQL advisory lock
  (`PgStore::try_station_lease`) makes a second *process* skip the station,
  which then stays untradable there (tested).
* NOAA-class hard floors cannot be configured away: ≥ 30 s spacing,
  concurrency 1, `Retry-After` always honoured (`ProviderClass::NoaaNws`,
  `RateLimitPolicy::validate`).
* Nothing ever probes limits. There is no adaptive speed-up and no
  "retry faster" path; a 429 multiplies spacing (politeness ×2, compounding
  to ×16) for 24 h.

**INPUTS.** Station config, cadence model, engine hints (peak watch,
exposure), provider health, gate state. **OUTPUTS.** At most one request per
decision; audit rows for every request.

**FAILURE MODES.** 429 → honour `Retry-After` (up to 6 h, then declare
unavailable), else a 5-minute throttle backoff. 5xx/timeouts/connection
errors → exponential backoff with equal jitter (60 s → 30 min), and the
circuit opens after 5 consecutive failures (10 min → 2 h, escalating). 401/403
→ circuit open for the maximum duration (the operator must look). Daily
budget exhausted → closed until UTC midnight. In every case the station's
health degrades and **new weather-dependent positions stop**.

**TESTING.** `wm-net/tests/http_fetcher.rs` has 11 cases: 200 with audit,
conditional GET with 304, 429 with delta and HTTP-date `Retry-After`, 500
backoff, timeout, connection refused, oversized body, circuit open/recovery,
minimum spacing, and concurrent callers sharing one gate. The `gate_invariants`
property test checks that, whatever the provider does, admitted requests
never start closer than the class floor, never start inside a `Retry-After`
window, and never exceed the daily budget. `run_loop_request_budget_over_six_virtual_hours`
checks the real collector loop makes 12–120 requests in six virtual hours.

**KNMI ten-minute readings (strategy K's input, `[knmi]`).** With
`WM_KNMI_API_KEY` set (and `[knmi] enabled`), one loop per station asks the
KNMI EDR API (`10-minute-in-situ-meteorological-observations`, the station
by its WIGOS id, `0-20000-0-06240` for Schiphol) every `poll_seconds` (30)
for the last `lookback_minutes` (40) of `ta` (ten-minute mean) and `tx`
(maximum), through its own gate (`[providers.knmi]`: ≥ 5 s spacing, one
request at a time, `Retry-After` honoured, a daily budget). The key goes
only in the `Authorization` header — never in a URL, a log line or an audit
record. A reading newer than the last one becomes a `NowcastUpdate`; the
log records how many minutes after its interval it arrived. It is
predictive input only: it never changes the observed high, the views or
settlement. Failures raise one alert, recovery another; K then does
nothing (fail closed). K enabled without a key raises a warning and stays
idle. At 30 s the loop makes about 2,900 requests a day, under the budget
of 4,000.

## 10. Adaptive PollingPolicy

**PURPOSE.** Decide *when* to ask, from what is expected, not from what is possible.

**INPUTS.** `PollingInputs { now, tz, last_observation_time, last_poll_at,
polls_in_current_window, hints{peak_watch, has_exposure}, health,
gate_not_before, throttled_recently }` plus `CadenceModel { routine_minutes,
first_poll_delay_secs, arrival_window_secs }`.
**OUTPUTS.** `PollDecision { at, mode, reason, in_window, expected_report }`.

Rules (KNOWN FACT, `wm-weather::polling`):
1. The expected report is the next routine time (HH:25/HH:55 for EHAM) after
   the newest known observation. The first poll comes `first_poll_delay`
   (90 s) after it, then every `window_interval` until the report arrives,
   at most `max_polls_per_window` quick polls; after them every
   `late_interval` until the 12-minute arrival window closes.
2. Between windows: a slow background poll (catches SPECIs and corrections).
3. Mode: **Peak** when the engine hints peak watch or exposure; **Low** at
   night (22:00–05:00 local); otherwise **Normal**. Recent throttling steps
   the mode down one level.
4. A window that closes without the report falls back to the background
   cadence (**never faster when late**). With no data for 2 h, the policy
   slows to one poll per 20 min.
5. The gate always wins: if it says not before T, the decision moves to T.
6. While a window poll finds the expected report missing, the collector
   asks the standby source (TGFTP) right after it, if that source's own
   gate admits a request now (it never waits for it). These asks never move
   the primary's schedule and are not failovers; whichever source publishes
   first delivers the report, and the stored observation names it
   (`report paper` counts them). `poll_standby_in_window = false` turns
   this off.

| Mode | Quick polls | Then, to the window's end | Background | Quick polls/window | Brief's research range |
|---|---|---|---|---|---|
| Low | 120 s | 180 s | 15 min | 3 | 2–5 min |
| Normal | 60 s | 90 s | 10 min | 6 | 1–2 min |
| Peak | 30 s (NOAA floor) | 60 s | 5 min | 10 | 30–60 s |

**KNOWN FACT (29 Sep 2026, live).** The 12:25 report was more than six
minutes late at AWC. Peak mode had spent its ten quick polls by +360 s and
the next poll was the background one, so the report was seen only at
+660 s. With the late polls the same case is seen at +420 s (test
`late_report_is_seen_within_a_late_interval`). A day on which every report
is 11 minutes late costs under 800 AWC requests (budget 2,000) and sees
each report within 3 minutes of publication.

**ASSUMPTION.** These are engineering starting points, not optimised
parameters. **HYPOTHESIS TO BACKTEST.** Whether peak-mode latency changes
outcomes at all: the knowledge delay (`fetched_at − observed_at`) is
recorded for every report, so the value of faster polling can be measured
before anyone asks for it.

**TESTING.** Unit tests for cadence math, window/late/overdue/stale
transitions and night mode; collector scenarios with a manual clock,
including a standby that publishes first, one that is only asked while the
report is missing, and the switch turned off.

## 11. RateLimiter

**PURPOSE.** A reusable per-provider limiter that composes every protection.

```rust
pub struct RateLimitPolicy {            // wm-net::policy, per provider, from TOML
    class: ProviderClass,               // NoaaNws | PublicData | Exchange | Local (hard floors per class)
    min_interval, max_concurrency, timeout, connect_timeout,
    backoff_base, backoff_max, throttle_backoff_base,
    respect_retry_after, max_retry_after,
    circuit_failure_threshold, circuit_open_base, circuit_open_max,
    daily_budget, politeness_factor, politeness_decay, max_body_bytes,
}
pub struct GateCore { .. }              // pure state machine: admit(now) / complete(now, outcome)
pub struct ProviderGate { .. }          // Arc; async acquire(max_wait) -> GatePermit (RAII: Drop = failure)
```

Separate policies exist for NOAA AWC and TGFTP (30 s / 60 s), api.weather.gov,
Polymarket Gamma (1 s), CLOB REST (0.25 s) and WS reconnects (5 s). **KNOWN
FACT:** no provider's assumptions are applied globally.

**FAILURE MODES.** A dropped permit (panic or cancellation) counts as a
failure, so the gate never forgets a request. Clock skew cannot help because
monotonic time drives the gate.
**TESTING.** Property test: admissions are never closer than the effective
minimum interval. Unit tests for jitter bounds, circuit escalation, budget
roll-over and `Retry-After` parsing (delta seconds and HTTP dates).

## 12. Caching

**PURPOSE.** Never fetch what we already have.
* **HTTP layer:** conditional GET (`ETag` / `If-Modified-Since`). A 304
  serves the cached body and is counted as `nws_cache_hits`. Polymarket book
  requests are unconditional (fresh by definition).
* **Collector:** consumers never trigger requests. The latest status and
  recent observations are published through a `watch` channel
  (`CollectorStatus`) that the dashboard and engine read.
* **Storage:** market metadata and rules are upserted and deduplicated by
  hash. Raw payloads are unique by `(provider, sha256)` with a `seen_count`.
  Observations warm-start from PostgreSQL after a restart, so the backfill is
  deduplicated and not re-emitted.
* **Adaptive backfill:** AWC requests 26 h of history only on a cold start or
  after a gap, and 3 h otherwise.

**FAILURE MODES.** A stale cache cannot mask an outage: freshness gates use
*observation* time, not fetch time.
**TESTING.** 304 path in `http_fetcher.rs`; warm restart in `runtime_paper.rs` (0 new, ≥ 40 duplicates, no duplicate rows).

## 13. Observation deduplication

**PURPOSE.** One real-world report produces exactly one event.

**Identity (KNOWN FACT).** `ObservationKey = (station, observed_at,
report_type)`; content identity = SHA-256 of the canonicalised METAR text
(whitespace-normalised, parser-independent).

| Class | Condition | Event? |
|---|---|---|
| `New` | unseen key, newer than the newest | `WeatherObservation` |
| `OutOfOrder` | unseen key, older than the newest (late delivery) | `WeatherObservation` (knowledge time = arrival) |
| `Duplicate` | key and content seen (any provider, any earlier version) | **none** |
| `Correction` | key seen, new content with a `COR` marker | `WeatherCorrection` |
| `Revision` | key seen, new content without a marker | `WeatherCorrection` (`labeled = false`) |

**FAILURE MODES.** Provider flip-flops (A → B → A) are recognised as
duplicates of an earlier version, not new corrections. The ledger is bounded
(retention window) and seeded from storage.
**TESTING.** Ledger unit tests (new, duplicate, correction, revision,
flip-flop, out-of-order) and collector scenarios delivering the same report
through two providers.

## 14. Observation correction handling

**PURPOSE.** Preserve history; let the day's state reflect the latest truth.
* Versions are append-only: `weather_observations` is unique on
  `(station, observed_at, report_type, version)`, `weather_corrections` links
  old and new, and the view `weather_observations_current` exposes the latest.
* The temperature state engine replaces the point with the new version and
  recomputes the day. The dashboard shows `COR`/`vN` flags and an alert.
* **Risk:** after a correction to the station, new weather positions are
  blocked for `correction_cooldown_minutes` (10).
* **KNOWN FACT (rules):** revisions count until the next day's first
  datapoint, which is why corrections are never discarded.

**TESTING.** Collector correction scenario; engine alert; PostgreSQL
round-trip of versions.

## 15. Raw-data persistence

**PURPOSE.** Parser bugs must be fixable later without data loss.

Per poll, `IngestBatch` is written atomically:
* `provider_requests`: endpoint, times, status, latency, bytes, cache
  outcome, retries, throttled, error class, gate wait, payload hash.
* `raw_weather_payloads`: verbatim body, content type and SHA-256, stored
  once per hash with a seen counter.
* `weather_observations`: raw METAR, parsed and provider-decoded
  temperature, precision, report type, quality flags (AUTO, COR, NIL,
  missing temperature, decoded mismatch, failover), `parser_version`,
  `fetched_at` (knowledge time) and `provider_receipt_at`.
* Health transitions go to `provider_health_events`.

**FAILURE MODES.** A failed write marks the collector's storage as failing.
The engine loop's own persistence failures, or a backlog the writer has not
accepted yet, turn `storage_ok` off, which blocks new positions (audit is a
precondition to trade). Engine records are retried until written, never
dropped ([07-risk-storage.md](07-risk-storage.md)).
**TESTING.** `wm-storage/tests/pg_store.rs` (fresh database per test) and
the PostgreSQL runtime test.

## 16. ProviderHealth model

**PURPOSE.** A single, explainable answer to "may we trust this station's data right now?"

`ProviderHealthState` precedence: **Unavailable > Throttled > Stale >
Degraded > Standby > Healthy**. Only **Healthy** allows new weather-dependent
positions.

| State | Trigger (defaults) |
|---|---|
| Unavailable | circuit open or half-open, or ≥ 3 consecutive failures |
| Throttled | a 429 within the last 30 min |
| Stale | no observation yet, or newest observation older than 45 min |
| Degraded | any recent failure or malformed payload, or latency > 5 s both on average (EWMA) and on the latest request (one slow response clears on the next normal poll) |
| Standby | no successful request **of this source** within 45 min: never contacted, or an idle fallback. No evidence either way. |
| Healthy | otherwise |

Health is evidence from a source's own requests. The newest observation is
shared by the station, but it never makes an untested fallback Healthy: the
active source is polled at least every 20 minutes, so only an idle fallback
reaches Standby.

A spent daily budget closes the gate: no requests are made, and the station
turns Stale as its data ages.

Tracked per provider and station: last request, last success, last *new*
observation, newest observation time, last HTTP status, last error,
consecutive failures, current backoff, blocked-until, latency (last and
EWMA), throttle events, requests today and daily budget, circuit state.
Transitions become `ProviderHealthChanged` events; the engine takes the
best-evidenced source per station (`station_state`, failover-aware). Standby
ranks last, so a fallback that was never contacted cannot mask a throttled
or failing primary: trading stays blocked until the fallback has actually
delivered.

**TESTING.** Health tracker unit tests
(`a_source_is_healthy_only_on_its_own_recent_success`); outage, throttle and
recovery collector scenarios, including
`untried_fallback_never_masks_a_throttled_primary`; the kernel property test
(no approval unless Healthy, Standby included).

## 17. TemperatureStateEngine

**PURPOSE.** Maintain each station's local-day series, from which every view's state is derived.

**OUTPUTS** (`DayState`, per view): `current`, `high` (value, first/last
touch, retests), `observed_minutes_since_high` (observed coverage — a data
gap never counts as confirmation), `minutes_since_high`,
`drop_from_high_tenths`, `lower_since_high`, 90-minute slope, acceleration,
`last_observation_at`, `observation_count`, `points`.

**KNOWN FACT.** A higher observation resets the peak candidate (a new `high`
with zero coverage). The state is always derived from the observations known
*so far*, so replay and live use the same transitions. Local days are
DST-correct (23/25-hour days).

**FAILURE MODES.** Missing temperatures are counted, not zero-filled. An
incomplete day (a gap from local midnight to now > 75 min) blocks trading
through the risk engine's coverage gate.
**TESTING.** State unit tests (retests, resets, filtered views, DST days) and
kernel scenarios, including `missing_morning_data_blocks_trading`.
