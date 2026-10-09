# Weather Machine

Automated research and **paper** trading of Polymarket *daily highest
temperature* markets, in Rust end to end: collectors, rate limiting,
deterministic trading kernel, risk engine, execution simulator, backtester,
storage, API and a WebAssembly dashboard. Location #1 is Amsterdam /
Schiphol (EHAM); nothing in the strategy code is Amsterdam-specific.

![Weather Machine dashboard (demo mode, synthetic data)](docs/dashboard-demo.png)

> **Status.** Research platform and paper trading. Live order placement
> does not exist in this build (Phase 14 gate). Nothing here is financial
> advice, and synthetic/demo results are not evidence of edge.

## What it does

* **Collects EHAM METARs respectfully.** One collector per station (with a
  PostgreSQL lease across processes), official NOAA sources (AWC Data API,
  NWS TGFTP), and one rate-limit gate per provider: ≥ 30 s spacing,
  `Retry-After`, backoff with jitter, circuit breaker, daily budget, and
  politeness after throttling. Polling is scheduled around the expected
  HH:25/HH:55 reports: quick polls first, slower ones until the arrival
  window closes, and the standby source (TGFTP) is asked too while a report
  is missing. It never speeds up to chase limits. With a free KNMI key it
  also reads the airport's ten-minute readings (KNMI EDR API), with global
  radiation and three neighbouring stations for the strategy lab —
  predictive inputs, never the observed high or settlement.
* **Keeps everything.** Raw payloads, every request, observation versions
  and corrections, verbatim market rules (SHA-256), every decision with its
  inputs, orders and fills, and a replayable journal of every engine input.
* **Decides deterministically.** The same kernel runs in backtest, demo and
  paper. The chain is: temperature state → peak detection per resolution view
  → probability model trained on real EHAM history (downloaded from IEM and
  trained automatically on first start, retrained every 30 days in the
  background and swapped in without a restart) → strategies → risk engine
  → simulated venue. Switched on in the shipped configuration (2 October
  2026, after the first replay of G–K at traded prices, revised on 8 October
  after the first live week; each one a hypothesis that `research market`
  judges out of sample):
  * **F** peak slot: 100 shares of YES on the high's bucket inside the
    season's peak slot (the 75th to 95th percentile of the local time at
    which the station's history first reported its day's high), once that
    bucket is offered above 0.90, at most 0.95, on books up to 0.10 wide,
    and held to settlement as in its replay;
  * **G** tail seller: resting NO bids (the YES offered at 1–8¢) on buckets
    three or more degrees above the high, withdrawn before each report (in
    the replay 125 of 127 won; 53 of 53 out of sample);
  * **K** KNMI nowcast: the NO of the high's bucket when KNMI's ten-minute
    mean is already above the next degree, before the METAR is published
    (in the replay every K trade won, also out of sample).

  Switched off, code kept as research baselines (`enabled = true` brings one
  back): A (YES of the final-high bucket), B (NO above it), C (split and
  unwind), D (outcomes the observations have already decided), E (YES of
  the high's bucket once the book confirms it), H (the cheap YES of high + 1),
  I (the NO of buckets priced 0.30–0.70 that the model rates lower) and J
  (morning quotes on both sides) — H and J lost in the replay, also out of
  sample; I lost $20.49 live over nine trades and was switched off on
  8 October 2026. A and B pool the model with the market's price: the book
  can veto a trade, never create one.
* **Tries more than it trades.** `research market` also replays a
  [strategy lab](docs/research/strategy-lab.md) of 25 new strategies
  (L1–L25) at traded prices: KNMI's ten-minute readings as a maker's shield
  or a stop, the METAR's own weather groups and TREND (sea breeze, showers,
  fog, fronts), the forecast's hourly path, KNMI's global radiation and the
  temperatures of upwind stations, and every taker's record on the days
  before. Each has a variant or a control and is judged out of sample.
  Since 2 October 2026 they also run live as **paper strategies**, each
  on a paper book of its own — its own positions, limits and P&L — so none
  of them ever blocks F–K or another lab strategy (`[strategies.lab]`:
  `disabled`, `variants`); 17 since 9 October: the seven families the
  replay refuted or the books never let trade were switched off on 8
  October, L1 on 9 October. The dashboard's lab panel, each family's page
  and `report paper` show their record apart from the main book's.
* **Uses forecasts only when they are proven.** A day-1 forecast (Open-Meteo
  Previous Runs: every hourly value forecast 24 h ahead, the same product in
  training and live, so no look-ahead) can refine the model with one feature:
  does the forecast expect the rest of the day to get warmer? Training runs a
  walk-forward test with a placebo control, and the forecast changes trading
  probabilities only if it clearly improves them. The verdict is in the
  training report and on the dashboard.
* **Tests its own structure.** Training also compares the model with a
  pre-registered candidate. The candidate's clock runs from the first report
  at the high, its morning is split per hour, and it reads the forecast as
  headroom above the observed high. The candidate is used only if it clearly
  predicts better in a walk-forward test. See the
  [replay of 28 September 2026](docs/research/replay-2026-09-28.md) for why.
* **Fails closed.** A throttled or unhealthy provider, stale data, gaps in the
  day's series, recent corrections, stale books, unverified resolution rules,
  no model, storage down, or the kill switch each block new weather-dependent
  positions.
* **Shows it all.** A dashboard with live SSE updates shows the intraday
  chart, peak state and model distribution, market ladder (implied vs model,
  edge, EV, break-even), gates, provider rate limits, blotter, decision audit
  log and kernel latency. Every strategy has its own page (`#/strategy/F`):
  its settings, strategy F's time slot, its live evaluation of every bucket,
  this run's proposals, orders and evaluation trail, and a **Copy log**
  button that copies all of it plus its last days from the database, ready
  to paste into a chat. A *Reports to paste* panel copies the replay at
  traded prices, the training report and the paper-run report the same way.
  A zero-JS `/lite` view, JSON API and Prometheus metrics are also served.

## Quick start

```sh
# Demo: real engine on synthetic data in accelerated time — no network, no database.
cargo run -p wm-app -- demo            # open http://localhost:8080/lite
./ui/build.sh                          # optional: build the WASM dashboard (needs wasm-bindgen-cli 0.2.129)
WM_UI_DIR=ui/dist cargo run -p wm-app -- demo   # full dashboard at http://localhost:8080/
```

Docker / Portainer:

```sh
docker run --rm -p 8080:8080 ghcr.io/spongi07/weathermachine:latest demo
cp stack.env.example .env   # set WM_DB_PASSWORD, WM_CONTACT, WM_ADMIN_TOKEN
docker compose up -d        # PostgreSQL 18 + paper-trading service + nightly backups
```

For Portainer (Git stack, demo stack, variables, backups, upgrades), see
**[docs/deployment/portainer.md](docs/deployment/portainer.md)**.

## CLI

| Command | Purpose |
|---|---|
| `weather-machine run` | Paper-trading service (default) |
| `weather-machine demo [--speed 60]` | Synthetic accelerated demo |
| `weather-machine collect [--once] [--no-db]` | Phase-0 data experiment: collectors only, zero trades |
| `weather-machine model train` | Download METAR history from IEM and day-1 forecast history from Open-Meteo (rate-limited, cached), train both model structures, evaluate the forecast and pick the structure, and learn per season when the day's high is first reported (strategy F's slots); `run` does this automatically |
| `weather-machine research peak-survival --csv … --model-out …` | P(high is final \| N min) with Wilson CIs from a CSV; trains the model |
| `weather-machine research market [--from --to --delay-secs --day … --print]` | Model versus market on settled markets (Polymarket Data API trades, cached): who predicts better (both structures), the best `market_weight`, who is sure first, how fast dead buckets reprice, strategies A/B/E at traded prices and as limit orders, when each season's high is first reported and strategy F with its variants at traded prices (one chosen out of sample), what makers earned on the other side of every trade, strategies G–K with their variants at traded prices (the best of each judged out of sample), with a KNMI key how often the next METAR followed KNMI's ten-minute mean, the strategy lab's 25 new strategies (L1–L25) with their variants and controls (each family judged out of sample), a report-by-report replay of each `--day`, resolution check |
| `weather-machine report paper [--from --to --print]` | The paper run day by day, from the database: METAR high, report delays and which source delivered first, the day-1 forecast's error, every evaluation's blockers per strategy, the closest calls and how they ended, model against market on the winning bucket, proposals, orders, fills, P&L, provider health. The running service also serves it at `/api/v1/report/paper` |
| `weather-machine backtest --synthetic-days N` / `--journal <run-id>` | Backtests with fidelity labels |
| `weather-machine markets discover [--date]` | Fetch and parse today's markets and rules, and any liquidity-reward pools (read-only) |
| `weather-machine rules approve --sha256 … --reviewer …` | Human approval of a rules text |
| `weather-machine ev-table` | Break-even probabilities (fees included) |
| `weather-machine migrate` · `check-config` · `healthcheck` | Operations |

## Configuration

`configs/weather-machine.toml` (engine, providers and their rate-limit
policies, polling, risk, strategies) and `configs/locations/*.toml` (one
file per city). Deployment-specific values come only from the environment:

| Variable | Meaning |
|---|---|
| `WM_DATABASE_URL` or `WM_DB_PASSWORD` (+ `WM_DB_HOST/PORT/USER/NAME`) | PostgreSQL (audit storage; required to trade) |
| `WM_CONTACT` / `WM_USER_AGENT` | Contact for the NOAA User-Agent (required) |
| `WM_ADMIN_TOKEN` | Operator token (kill switch) |
| `WM_DASHBOARD_USER` / `WM_DASHBOARD_PASSWORD` | Optional Basic auth |
| `WM_MODEL_PATH` | Model file (default `/data/models/<station>.json`); without a model, no weather trades |
| `WM_MODEL_AUTO_TRAIN` | `false` stops automatic training from IEM history (default `true`) |
| `WM_FORECAST` | `false` never fetches or evaluates the day-1 forecast (default `true`) |
| `WM_FORECAST_MODEL` | Open-Meteo model id for the day-1 forecast (default `gfs_global`) |
| `WM_OPEN_METEO_API_KEY` | Open-Meteo subscription key; the free tier is for non-commercial use |
| `WM_KNMI_API_KEY` | Free KNMI Data Platform key ([developer portal](https://developer.dataplatform.knmi.nl/)) for the ten-minute readings; sent only in the `Authorization` header. Without it strategy K and the lab's KNMI rules do nothing |
| `WM_DATA_DIR` | Writable data directory for the model and history cache (default `/data`) |
| `WM_JOURNAL_RETENTION_DAYS` | Days of order-book updates kept in the replay journal (default 7; 0 = keep) |
| `WM_BACKUP_JOURNAL` | Stack only: include the replay journal in nightly backups (default `false`) |
| `WM_MODE`, `WM_HTTP_BIND`, `WM_UI_DIR`, `WM_LOG_FORMAT`, `WM_CONFIG` | Overrides |

## Development

```sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                   # PostgreSQL tests need:
WM_TEST_DATABASE_URL=postgres://wm:wm@localhost:5432/wm_test cargo test --workspace
(cd ui && cargo clippy --target wasm32-unknown-unknown -- -D warnings && ./build.sh)
```

Toolchain: Rust 1.94.1 (`rust-toolchain.toml`, includes the wasm32 target).
CI (`.github/workflows/ci.yml`) runs fmt, clippy, all tests against
PostgreSQL 18, the WASM build and a dependency audit. It then builds the
container, smoke-tests it (hardened runtime, health, API, assets) and pushes
to GHCR.

## Documentation

* **[Technical blueprint](docs/blueprint/README.md)**: all 43 components
  (purpose, inputs, outputs, Rust interface, failure modes, testing) with
  KNOWN FACT / ASSUMPTION / HYPOTHESIS TO BACKTEST labels, the dependency
  rationale, the historical-data audit and the Phase 0–14 roadmap.
* [Deployment with Portainer](docs/deployment/portainer.md).
* [Forecast layer and its evaluation](docs/blueprint/05-markets.md#19-forecastprovider-and-the-day-1-forecast-feature).
* [Review of the "weatherforecaster" bot](docs/research/weatherforecaster-review.md):
  why its backtest edge came from look-ahead, and what was (not) ported.
* [Where the edge is — and where it is not](docs/research/edge-research.md):
  the market versus models, decided outcomes (strategy D), pooling with the
  book, the evidence behind strategies E and F, providing liquidity instead
  of taking it, how to measure it on EHAM's settled markets, and the
  research and data behind strategies G–K (§9).
