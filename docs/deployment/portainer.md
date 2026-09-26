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
| Outbound HTTPS | `aviationweather.gov` (NOAA AWC), `tgftp.nws.noaa.gov`, `gamma-api.polymarket.com`, `clob.polymarket.com`, `ws-subscriptions-clob.polymarket.com`. |
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
| `WM_MODEL_PATH` | no | Trained model on the data volume, e.g. `/data/models/eham.json`. Without a model the engine never trades weather (fail closed). |

Optionally enable **GitOps updates** (polling, or the webhook Portainer shows
after creation; store it as the repository secret `PORTAINER_WEBHOOK_URL` and
CI will call it after each successful image push on the default branch).

What the stack contains:

| Service | Image | Notes |
|---|---|---|
| `postgres` | `postgres:18-alpine` | Volume `pgdata` at `/var/lib/postgresql` (PG 18 layout). Not published on the host. |
| `weather-machine` | GHCR image | `run` = paper-trading service; waits for a healthy database, applies migrations automatically, read-only root filesystem, non-root (uid 65532), all capabilities dropped, `no-new-privileges`, CPU/memory limits, log rotation. |
| `db-backup` | `postgres:18-alpine` | `pg_dump --format=custom` every 24 h into volume `backups`, keeps `WM_BACKUP_KEEP_DAYS` (14). |

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
* Expect **no trades** until (a) a trained model is configured, (b) the
  day's market is discovered and machine-tradable, (c) the day's observation
  series is complete, and (d) every pre-trade gate on the dashboard passes.

### Training and installing a model

The image has no shell (distroless), so run one-off commands as their own
container with the stack's data volume (Portainer: *Containers → Add
container*, or the Docker CLI on the host):

```sh
# 1. Put an IEM ASOS CSV export (columns station,valid,metar) on the data volume.
docker run --rm -v weather-machine_wmdata:/data -v "$PWD":/in:ro alpine \
  sh -c 'cp /in/eham.csv /data/research/ && chown 65532:65532 /data/research/eham.csv'
# 2. Run the peak-survival study; it prints P(high is final | N minutes) and writes the model.
docker run --rm -v weather-machine_wmdata:/data ghcr.io/spongi07/weathermachine:latest \
  research peak-survival --csv /data/research/eham.csv --station EHAM \
  --model-out /data/models/eham.json --report-out /data/research/eham-report.md
# 3. Set WM_MODEL_PATH=/data/models/eham.json on the stack and redeploy.
```

Volume names are prefixed with the stack name (`weather-machine_…`).

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
| Metrics | Scrape `/metrics` (Basic auth applies if configured). Key series: `nws_requests_total`, `nws_429_total`, `nws_failures_total`, `nws_cache_hits`, `nws_new_observations_total`, `wm_engine_events_total`, `wm_global_exposure_usd`, `wm_kill_switch`, `wm_storage_ok`. |

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
| Image pull `denied` | Make the GHCR package public or add GHCR credentials under *Registries*. |
| Container unhealthy, logs show `database not reachable yet` | PostgreSQL still initialising (first start) — it retries for 90 s; check the `postgres` service logs. |
| Dashboard says *storage DOWN*, no trades | Audit storage failing ⇒ fail closed by design; check database health/disk. |
| Providers *throttled* / *unavailable* | The rate limiter backs off (Retry-After honoured, circuit breaker); trading of that station stays blocked until data is healthy and fresh. Do not lower the polling floors. |
| `collector lease held by another instance` | Another Weather Machine is running against the same database; stop it. |
| Blank page, `/lite` works | Browser without WebAssembly, or a proxy stripping `application/wasm`; use `/lite`. |
