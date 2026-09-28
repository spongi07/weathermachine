# Deploying Weather Machine with Portainer

Weather Machine ships as one container image (`ghcr.io/spongi07/weathermachine`)
containing the Rust service, the Rust/WebAssembly dashboard and the default
configuration. The Portainer stack (`docker-compose.yml`) adds PostgreSQL 18
(audit storage) and a nightly database backup.

> **Scope of this build: paper trading only.** Live order placement does not
> exist in the code. Polymarket is not available to users in some
> jurisdictions (the Netherlands is close-only); check yours before you
> consider enabling anything beyond paper trading.

## 1. Prerequisites

| Item | Notes |
|---|---|
| Portainer CE/BE ≥ 2.19 on a Docker standalone environment | Swarm also works; resource limits then come from `deploy.resources`. |
| x86-64 host (amd64) | arm64 images: run the CI workflow manually with `platforms = linux/amd64,linux/arm64`. |
| Outbound HTTPS | `aviationweather.gov` (NOAA AWC), `tgftp.nws.noaa.gov`, `gamma-api.polymarket.com`, `clob.polymarket.com`, `ws-subscriptions-clob.polymarket.com`, `mesonet.agron.iastate.edu` (IEM METAR history, to train the model) and `previous-runs-api.open-meteo.com` (day-1 forecast; `customer-previous-runs-api.open-meteo.com` with an API key). |
| Image access | The package `ghcr.io/spongi07/weathermachine` is public: anonymous pulls work, so no registry credentials are needed. If the package is ever made private, add a registry in Portainer: *Registries → Add registry → Custom*, URL `ghcr.io`, username = GitHub user, password = a token with `read:packages`. |

The image is built and pushed by `.github/workflows/ci.yml` on every push after
formatting, lint and tests pass. Tags: `latest` (default branch), the branch
name, `sha-<short>` for every commit, and semantic versions for `v*` tags.

## 2. Try it first: the demo stack (2 minutes, no secrets)

*Stacks → Add stack → name `weather-machine-demo` → Repository*

* Repository URL: `https://github.com/spongi07/weathermachine`
* Repository reference: `refs/heads/claude/great-wright-xvo6io` (the repository's default branch)
* Compose path: `deploy/portainer/demo-stack.yml`

Deploy, then open `http://<host>:8081/`. The demo drives the real engine,
risk engine and simulated venue with synthetic data in accelerated time,
including a data correction, a simulated HTTP 429 throttling episode (watch
the gates fail closed) and daily settlement. A yellow **DEMO** banner is always
shown. Optional variables: `WM_DEMO_SPEED` (default 60 = one day in 24 min),
`WM_DEMO_PORT`, `WM_ADMIN_TOKEN` (enables the kill-switch button).

## 3. Production (paper) stack

*Stacks → Add stack → name `weather-machine` → Repository*

* Repository URL: `https://github.com/spongi07/weathermachine`
* Repository reference: `refs/heads/claude/great-wright-xvo6io` (the default branch), or a release tag.
  If the code later moves to another branch such as `main`, change this reference.
* Compose path: `docker-compose.yml`
* **Environment variables** (see `stack.env.example` for all):

| Variable | Required | Purpose |
|---|---|---|
| `WM_DB_PASSWORD` | yes | PostgreSQL password; any characters (encoded by the app). `openssl rand -hex 24` |
| `WM_CONTACT` | yes | Contact (e-mail/URL) in the NOAA User-Agent — NOAA requires identification. Never compiled into the image. |
| `WM_ADMIN_TOKEN` | recommended | Operator token for the kill switch. Empty ⇒ operator endpoints disabled. `openssl rand -hex 32` |
| `WM_DASHBOARD_USER` / `WM_DASHBOARD_PASSWORD` | recommended if exposed | HTTP Basic auth for dashboard, API and metrics. |
| `WM_IMAGE_TAG` | no | Pin a tag (e.g. `sha-1a2b3c4`) for reproducible deploys and rollbacks. |
| `WM_HTTP_PORT` | no | Host port (default 8080). |
| `WM_MODEL_PATH` | no | Model file. Default `/data/models/eham.json`, trained automatically on first start (see §4). Without a model the engine never trades weather (fail closed). |
| `WM_MODEL_AUTO_TRAIN` | no | `true` (default): train the model from IEM history when the file is missing, and retrain it in the background every 30 days. `false`: never download history. |
| `WM_FORECAST` | no | `true` (default): fetch the day-1 forecast and evaluate it at every training (§4). `false`: never fetched or evaluated. |
| `WM_FORECAST_MODEL` | no | Open-Meteo model id (default `gfs_global`, the longest archive). Changing it triggers a retraining. |
| `WM_OPEN_METEO_API_KEY` | no | Open-Meteo subscription key. The free tier is for **non-commercial** use; for real-money trading use a subscription. |

Optionally enable **GitOps updates** (polling, or the webhook Portainer shows
after creation; store it as the repository secret `PORTAINER_WEBHOOK_URL` and
CI will call it after each successful image push on the default branch).

What the stack contains:

| Service | Image | Notes |
|---|---|---|
| `postgres` | `postgres:18-alpine` | Volume `pgdata` at `/var/lib/postgresql` (PG 18 layout). Not published on the host. |
| `weather-machine` | GHCR image | `run` = paper-trading service; waits for a healthy database, applies migrations automatically, read-only root filesystem, non-root (uid 65532), all capabilities dropped, `no-new-privileges`, CPU/memory limits, log rotation. |
| `db-backup` | `postgres:18-alpine` | `pg_dump --format=custom` every 24 h into volume `backups`, keeps `WM_BACKUP_KEEP_DAYS` (14). The replay journal is excluded unless `WM_BACKUP_JOURNAL=true`. |

Health: the image has a built-in probe (`weather-machine healthcheck`) — no
shell or curl needed. Portainer shows *healthy* once the engine loop publishes
snapshots. `GET /readyz` additionally reports startup, storage and kill-switch
state as JSON.

## 4. After deployment

* Dashboard: `http://<host>:8080/` (WebAssembly). Zero-JavaScript fallback:
  `/lite`. JSON snapshot: `/api/v1/snapshot`. Live stream (SSE):
  `/api/v1/stream`. Prometheus metrics: `/metrics`.
* The kill switch (top-right button or
  `curl -X POST -H "X-WM-Admin-Token: $TOKEN" -H 'content-type: application/json' -d '{"engaged":true,"reason":"manual"}' http://<host>:8080/api/v1/kill-switch`)
  blocks every new order immediately; it is journaled like any other event.
* Expect **no trades** until (a) the model is trained (automatic, below),
  (b) the day's market is discovered and machine-tradable, (c) the day's
  observation series is complete, and (d) every pre-trade gate on the
  dashboard passes.

### The probability model (automatic)

On the first start there is no model, so the dashboard shows **MODEL
TRAINING** and a banner, and the engine trades nothing. In the background
the service:

1. downloads EHAM METARs from the IEM archive, 2005 to today, one year per
   request, 15 s apart (IEM allows one request per second per IP), and
   caches finished years in `/data/research/iem/EHAM/`;
2. downloads the day-1 forecast history from Open-Meteo (newest year first,
   one request per year, cached in `/data/research/open-meteo/EHAM/`);
3. parses the METARs with its own parser, runs the peak-survival study and
   the forecast evaluation (below);
4. refuses to install a model built from fewer than 730 usable days;
5. writes `/data/models/eham.json` and the report
   `/data/research/eham-survival.md` (with the SHA-256 of every year it used);
6. **loads the model into the running service** — no restart: the badge
   turns green.

This takes a few minutes. If IEM is unreachable the badge shows **MODEL
ERROR** with the reason, and training is retried every 6 hours. Later
starts load the saved model directly. The model is retrained **in the
background** every 30 days (and when the forecast has not been evaluated
yet): the current model keeps trading, the badge shows **MODEL ↻**, and the
new model is swapped in when it is ready.

### The day-1 forecast (measured, not assumed)

The service fetches a day-1 forecast for Schiphol every hour: every hourly
value was forecast 24 hours before its time, exactly the product used in the
training history — so the evaluation cannot be flattered by look-ahead.
Whether the model may *use* it is decided by the training, never assumed:
it replays years of days one at a time, scoring every trading moment with a
model that has only seen earlier days, with and without the forecast and
against a placebo (the forecast of two weeks earlier). Only if the forecast
clearly beats both is it adopted. Read the verdict:

* the **FORECAST** pill (status bar) — green: in use; amber **FORECAST
  WAITING**: adopted, but today's series is not usable yet (before 08:00
  local, or the source is down); grey **forecast not used**: not adopted
  (hover for the numbers);
* the *Day-1 forecast* box under the model distribution and the dashed line
  in the chart;
* the section *Forecast evaluation* in `/data/research/eham-survival.md`
  (log loss with and without the forecast and vs. the placebo, P(final) by
  forecast rise, calibration, a constant-price trading proxy).

Training also compares the model's structure with a candidate (clock from the
first report at the high, morning split per hour, forecast as headroom above
the observed high) in the same walk-forward way, and switches only if the
candidate clearly predicts better. The verdict is in the section *Model
structure — walk-forward comparison* of the same report and in the MODEL
pill's tooltip. After updating to a version with this comparison, the model
retrains once in the background; the running model keeps trading meanwhile.

If Open-Meteo is unreachable the model simply runs without the forecast.

Retrain by hand (for example once a year) with a one-off container on the
stack's data volume. The image has no shell (distroless), so run it as its
own container (Portainer: *Containers → Add container*, or the Docker CLI):

```sh
docker run --rm -e WM_CONTACT=you@example.org -v weather-machine_wmdata:/data \
  ghcr.io/spongi07/weathermachine:latest model train
# then restart the weather-machine container
```

Deleting `/data/models/eham.json` and restarting also retrains; only the
current year is downloaded again. To use a model you trained elsewhere, put
it on the volume and set `WM_MODEL_PATH`.

Volume and network names are prefixed with the Portainer stack name
(`weather-machine_…` above). If you named the stack differently, adjust the
commands; *Volumes* in Portainer lists the actual names.

### Model versus market (research)

Does the model know anything the market does not? Is there anything left
for strategy D (decided outcomes) after faster traders? Run the study on the
settled markets (read-only: it trades nothing and changes nothing):

```sh
docker run --rm -e WM_CONTACT=you@example.org -v weather-machine_wmdata:/data \
  ghcr.io/spongi07/weathermachine:latest \
  research market --from 2026-06-01 --to 2026-09-28 --day 2026-09-28 --print
```

Besides model versus market, the report replays strategies A and B at the
prices the market actually traded. The replay runs with both model
structures, confirmation 0′/30′/live and the live and a wider ask range. It
shows trades, wins and P&L per variant. Strategy E runs three ways: as
configured, without its book condition, and with the model's agreement. The
book is not archived, so the replay stands in for it with the trades. Each
`--day` (repeatable) adds that day report by report: both models' cells and
probabilities, the market, the high bucket's taker flow and every simulated
trade.

It reads each day's event from Gamma and its trades from the Polymarket Data
API. That is one or two requests per day, two per second at most, and
settled days are cached in `/data/research/polymarket/EHAM/`. The METAR
history comes from the training cache. The log shows the verdict and the
full report, which is also saved as `/data/research/eham-market.md` and
`.json`.

What each section means and how to act on it is in
[the edge research](../research/edge-research.md#4-the-measurement-weather-machine-research-market).
In short:

* If the market predicts best, set `market_weight = 1.0` in
  `[strategies.buy_yes]` and `[strategies.buy_no]`. A and B then stop
  trading.
* The latency section shows whether dead buckets are repriced before the
  bot could act.
* The resolution check shows whether the METAR high was the resolved
  bucket.

### Other one-off commands

```sh
docker run --rm ghcr.io/spongi07/weathermachine:latest ev-table
docker run --rm -e WM_CONTACT=you@example.org ghcr.io/spongi07/weathermachine:latest markets discover
docker run --rm -e WM_CONTACT=you@example.org ghcr.io/spongi07/weathermachine:latest collect --once --no-db
```

`collect --once --no-db` performs exactly one request per configured station
(Phase-0 data verification, zero trades).

## 5. Operations

| Task | How |
|---|---|
| Upgrade | Pull and redeploy the stack (or GitOps). Migrations run on start; the station lease and warm start make restarts safe. |
| Roll back | Set `WM_IMAGE_TAG` to the previous `sha-…` tag and redeploy. Migrations are additive. |
| Restart safety | Crash-only design: the process state is rebuilt from PostgreSQL (observations, journal) on start. A second instance cannot poll the same station (PostgreSQL advisory lease). |
| Backups | Volume `backups`. Restore (stop `weather-machine` first): `docker run --rm -it --network weather-machine_default -e PGPASSWORD=… -v weather-machine_backups:/b postgres:18-alpine pg_restore -h postgres -U wm -d weather_machine --clean --if-exists /b/<file>.dump` |
| Logs | JSON lines (Portainer → container → Logs). Change verbosity with `RUST_LOG`. |
| Metrics | Scrape `/metrics` (Basic auth applies if configured). Key series: `nws_requests_total`, `nws_429_total`, `nws_failures_total`, `nws_cache_hits`, `nws_new_observations_total`, `wm_engine_events_total`, `wm_global_exposure_usd`, `wm_kill_switch`, `wm_storage_ok`, `wm_persist_backlog_episodes_total`, `wm_journal_shed_total`. |
| Disk | The market stream delivers ~1.5 million order-book updates a day. They are journaled with 5 levels per side and deleted from the journal after 7 days (`WM_JOURNAL_RETENTION_DAYS`), so the database levels off at a few GB plus the recorded market history (books stored on change, ≤ 1 per 10 s per token), which grows by roughly 100–300 MB a day. Check with *Volumes* in Portainer or `docker system df -v`. |

## 6. Exposing the dashboard safely

Keep the stack on a private network or put it behind your reverse proxy
(Traefik, Nginx Proxy Manager, Caddy) with TLS. Set `WM_DASHBOARD_USER` /
`WM_DASHBOARD_PASSWORD`, and `WM_ADMIN_TOKEN` for operator actions. The
server already sends a strict Content-Security-Policy and
`X-Frame-Options: DENY`; for Server-Sent Events it sends
`X-Accel-Buffering: no`, but disable response buffering for `/api/v1/stream`
if your proxy ignores that header. The UI uses relative URLs and works under a
path prefix.

## 7. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Stack fails: `required variable WM_DB_PASSWORD is missing` | Set the required variables in the stack's environment section. |
| Stack fails to clone: reference not found | The repository reference must be an existing branch or tag: `refs/heads/claude/great-wright-xvo6io`. |
| `failed to bind host port 0.0.0.0:8080/tcp: address already in use` | Another service already uses host port 8080. Set `WM_HTTP_PORT` to a free port (e.g. `8090`) and redeploy; the dashboard is then at `http://<host>:8090/`. The demo stack uses `WM_DEMO_PORT` (default 8081) the same way. |
| Image pull `denied` | Make the GHCR package public or add GHCR credentials under *Registries*. |
| Container unhealthy, logs show `database not reachable yet` | PostgreSQL still initialising (first start) — it retries for 90 s; check the `postgres` service logs. |
| **MODEL TRAINING** / banner "No probability model yet" | Normal on the first start: the history download and training take a few minutes; the model is then loaded without a restart. |
| **MODEL ERROR**: `model training failed: … connection failed` | The host cannot reach `mesonet.agron.iastate.edu`. Allow outbound HTTPS to it; the service retries every 6 h (or restart it). |
| **MODEL ↻** | A background retraining is running (forecast evaluation, or the model is 30 days old); the current model keeps trading. |
| **forecast not used** | The evaluation did not show a clear improvement from the forecast (hover for the numbers; details in the report). Nothing to fix — the model trades without it. |
| **FORECAST WAITING** | The forecast is adopted, but today's series is not usable yet: before 08:00 local, or Open-Meteo unreachable (alert "day-1 forecast unavailable"). The model trades without it meanwhile. |
| Alert "day-1 forecast unavailable (HTTP status 429 …)" | Open-Meteo's free daily limit (shared by every client on your IP) was reached. The service backs off; consider `WM_OPEN_METEO_API_KEY`. |
| Report says "not evaluated: …" | The forecast history could not be downloaded (e.g. `previous-runs-api.open-meteo.com` blocked); training is retried after 6 h. A "rejected by Open-Meteo" reason naming the model means `WM_FORECAST_MODEL` is not a valid model id. |
| Alert "persistence backlog — new positions blocked" | The database was slow for a moment (e.g. a backup or vacuum). Records are held and retried, nothing is lost; trading resumes with "persistence caught up". If it persists, check disk space and the `postgres` logs. |
| Dashboard says *storage DOWN*, no trades | Audit storage failing ⇒ fail closed by design; check database health/disk. |
| Providers *throttled* / *unavailable* | The rate limiter backs off (Retry-After honoured, circuit breaker); trading of that station stays blocked until data is healthy and fresh. Do not lower the polling floors. |
| `collector lease held by another instance` | Another Weather Machine is running against the same database; stop it. |
| Blank page, `/lite` works | Browser without WebAssembly, or a proxy stripping `application/wasm`; use `/lite`. |
