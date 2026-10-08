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
The running build names its commit in the dashboard's status bar
(`v0.1.0+1a2b3c4`) and in the startup log line ("starting paper runtime …
build=0.1.0+1a2b3c4"): it matches the tag `sha-1a2b3c4`.

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
| `WM_KNMI_API_KEY` | for strategy K and the lab's KNMI rules | Free key from the [KNMI Developer Portal](https://developer.dataplatform.knmi.nl/) (*EDR API*). The airport's ten-minute readings (with global radiation and three neighbouring stations for the strategy lab), the input of K and of most lab strategies; without it they do nothing and the dashboard says so. Sent only in the `Authorization` header, never logged. |

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
  `/api/v1/stream`. Prometheus metrics: `/metrics`. The paper run day by
  day: `/api/v1/report/paper` ([below](#the-paper-run-report)).
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

Training also learns, per season, **when the day's high is first
reported**: the mean, the median and the quantiles of that local time.
These are strategy F's slots, stored in the model and shown in the section
*When the day's high is first reported (strategy F)* of the same report.
After updating to the version with strategy F, the model retrains once in
the background. Until the new model is loaded, F uses its fallback slots
(`[strategies.peak_slot.fallback_slots]`), and every F evaluation names the
slot and where it came from.

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
docker run --rm -e WM_CONTACT=you@example.org -e WM_KNMI_API_KEY \
  -v weather-machine_wmdata:/data \
  ghcr.io/spongi07/weathermachine:latest \
  research market --from 2026-06-01 --to 2026-10-01 --print
```

Mount the stack's own data volume. Portainer names it after the stack
(`<stack name>_wmdata`; `docker volume ls | grep wmdata` lists them), and
`docker run` silently creates an empty volume for a name that does not
exist. On the wrong volume the study finds no installed model and the
dashboard's *Reports to paste* panel does not see the report. The first
line of the run says which model it found: *installed model … evaluated
with the day-1 forecast* is what the service uses; *no installed model at
…* means another volume or a stack that sets `WM_MODEL_PATH` (pass the same
`-e WM_MODEL_PATH=…` to the study). `-e WM_KNMI_API_KEY`
without a value passes the key from your shell (`read -rs WM_KNMI_API_KEY &&
export WM_KNMI_API_KEY`), so it never appears on the command line.

Besides model versus market, the report replays strategies A and B at the
prices the market actually traded. It also shows what resting (maker)
orders earned on the other side of every trade, and replays A, B and E as
limit orders; that is the part to read first
([why](../research/edge-research.md#7-what-we-had-not-tried-providing-liquidity)). The replay runs with both model
structures, confirmation 0′/30′/live and the live and a wider ask range. It
shows trades, wins and P&L per variant. Strategy E runs three ways: as
configured, without its book condition, and with the model's agreement. The
book is not archived, so the replay stands in for it with the trades. Each
`--day` (repeatable) adds that day report by report: both models' cells and
probabilities, the market, the high bucket's taker flow and every simulated
trade.

Strategy F (the high's bucket inside the season's peak slot, 100 shares)
runs six ways at 100 shares a trade:

* as configured (the 75 % → 95 % slot);
* up to 0.99;
* an earlier slot (25 % → 75 %) and the median slot (50 % → 90 %, F's
  rule until 1 October 2026); the later-slot variant is the configured
  rule now, so it is not repeated;
* only once the report is 1 °C below the high;
* as a resting bid.

Each market day uses the slots that the METAR days before it learned. Read
the line **Strategy F out of sample** first. It takes the rule that did best
on the first half of the days and scores it on the second half, which needs
at least 20 market days. The F table's best row overstates what to expect.

Run it after 00:00 UTC (02:00 in Amsterdam in summer) to include the day
that just ended. Before then the METAR archive holds only its first hours,
and the report lists that day as skipped.

It reads each day's event from Gamma and its trades from the Polymarket Data
API. That is one or two requests per day, two per second at most, and
settled days are cached in `/data/research/polymarket/EHAM/`. The METAR
history comes from the training cache. The log shows the verdict and the
full report, which is also saved as `/data/research/eham-market.md` and
`.json`. The dashboard's *Reports to paste* panel then copies it in one
click.

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
* If *Strategy F out of sample* is negative, set `enabled = false` in
  `[strategies.peak_slot]`. If another F rule did better out of sample, you
  can switch to it in the same section: for example `min_drop_tenths = 10`
  for "1 °C below", `max_price = "0.99"`, or other slot quantiles. With an
  interval that includes zero, keep F in paper and collect more days.

**Strategies G–K** (since 1 October 2026) have their own section, *Strategies
G–K at traded prices*: each as configured plus variants (G only up to 3¢,
three or more above the high, without the model, from 14:00; H without the
model, until the 90 % peak time, as a resting bid; I without the model or
the measured bias, as a resting bid; J YES or NO bids only, until 09:00;
K with a 0.5 °C or 0.1 °C margin, buying the YES above instead, the reading
known after 2 or 8 minutes). Makers fill only when a later trade goes
through their price, takers pay the fee. Read the lines **Strategy G out
of sample** … **Strategy K out of sample** in the verdict first: the best
variant of each strategy on the first half of the days, scored on the
second half (with its 95 % interval) — and, when that variant is not the
configured one, the configured rule's own result on the second half. That
last part decides whether a strategy stays on. A negative one means
`enabled = false` in that strategy's section; a better variant can be
switched to in the same section.

Strategy K needs KNMI's readings: run the study with the key
(`-e WM_KNMI_API_KEY=…`). It then downloads the airport's ten-minute
readings for the period (a week per request; a week that ended at least
eight days ago is cached in `/data/research/knmi/EHAM/`, since KNMI fills
gaps for up to seven days) and adds the table
*KNMI's ten-minute mean before the METAR*: how often the next METAR raised
the high, by how far the last mean before it stood above the high's rounding
edge. Set K's `p_new_high` from the row of its margin (shipped: +0.3 °C,
assumed 0.80, measured 0.975 on 2 October 2026; the shipped value is its
lower bound, 0.94). A week KNMI refuses is left out and the log names it;
only when no week downloads is K listed as not replayed — as it is without
the key.

**The strategy lab (L1–L25)** (since 2 October 2026) is the report's
section *Strategy lab: L1–L25 at traded prices*: 25 new strategies, each
with a variant or a control, which the service also runs in paper on books
of their own (see *The strategy lab in paper* below;
[what each does and why](../research/strategy-lab.md)). It reads the
day-1 forecast even when the installed model does not use it, the METAR's
weather groups from the archived reports, and — with the KNMI key — KNMI's
global radiation at Schiphol and the ten-minute temperatures of Voorschoten,
De Bilt and Berkhout: about 72 extra requests for four months, a few
minutes at KNMI's request spacing, cached in `/data/research/knmi-series/`
once final. A station or parameter KNMI refuses twice in a row is given up
(the log says which); its strategies are listed as not replayed. Read the
lab's **out of sample** lines and its last line, *Strategy lab: held up on
the later days …*, which the verdict repeats. A family named there is a
candidate, not a switch: it needs the same result on the next run's new
days, and a paper record that agrees. Paste the lab's lines when you want
one judged.

### Strategy pages and logs to paste

The dashboard's **Strategies** panel has a card per strategy that is
switched on (F, G, K and the unwind exits since 8 October 2026; a
strategy with `enabled = false` is not shown). Each card shows what the strategy is doing now, for example
"autumn slot 13:25–15:26: before the slot: it starts in 2 h 13 min", or the
blocker on the high's bucket. It also shows this run's proposals and orders,
with two buttons:

* **Open** shows the strategy's page:
  * its settings;
  * strategy F's time slot today and per season;
  * its evaluation of every bucket right now, with everything that blocks
    it;
  * this run's proposals and risk verdicts, orders and evaluation trail;
  * a preview of the log.
* **Copy log** copies that log plus the strategy's last 7 days from the
  database, including its settled P&L. Paste it into the chat. *Download*
  saves it as a `.md` file, and *Open as text* shows it in a tab.

The **Reports to paste** panel at the bottom copies, downloads or opens:

* the replay at traded prices (`research market`), once it has run;
* the training report, including when each season's high is first reported;
* the paper run day by day.

On plain `http://` in the LAN the browser has no clipboard API. The button
then copies another way, or opens the text selected: press Ctrl+C (⌘C).
Without JavaScript, `/lite` lists every strategy with links to its log.

### The strategy lab in paper (L1–L25)

Since 2 October 2026 the 25 lab strategies run in the service as paper
strategies, switched on by `[strategies.lab]` in the shipped configuration.
Each trades a paper book of its own — its own positions, limits ($25 an
order, $100 for L3, which buys F's 100 shares; $120 open; $100 of loss a
day) and P&L — so none of them ever blocks F–K, the unwind exits or another
lab strategy, and F–K's caps and P&L do not change. Nothing to set up
beyond the KNMI key:

* **KNMI** (`WM_KNMI_API_KEY`): Schiphol's ten-minute readings with global
  radiation, and after each new reading one request each for Voorschoten,
  De Bilt and Berkhout — about 140 requests an hour in all, well inside a
  registered key's 1,000. Without the key the KNMI rules stay idle (one
  alert says so).
* **Polymarket Data API** (public, no key): today's taker trades every
  20 s for L18–L21, and once a day the settled markets on which every taker
  is scored for L19 and L20. The first scoring after a start downloads up
  to 60 settled days into `/data/research/polymarket/EHAM/` — the cache
  `research market` uses, so a host that ran the study already has most of
  it — and takes a few minutes; until then L19 and L20 show "no taker
  records yet". Every morning at 07:00 local adds the day before.

The dashboard's **Strategy lab · paper · L1–L25** panel, below the
strategy cards, lists every running lab strategy with its status now, this
run's counts, and its own book's open positions and realized P&L; a line
above the table shows what the lab reads. The letters link to each
strategy's page, whose **Copy log** works as for F–K. The KPI strip and the
positions and orders panels show the main book only (each position with the
letter of the strategy that opened it, which its strategy's log also
lists); `/lite` lists the
lab's books, and `report paper` gives the lab's settled P&L apart from the
main book's.

To switch families off or run a family's variant, set `disabled` or
`variants` in `[strategies.lab]` (codes `L1` … `L25`, e.g.
`disabled = ["L19", "L20"]`) and redeploy; `enabled = false` switches the
whole lab off, and with it the extra KNMI and Data API requests.

### The paper-run report

What did the bot do while nobody was watching? The dashboard keeps only the
last 100 decisions and forgets them on a restart; the database keeps
everything. The report reads it back, day by day:

* the METAR high and when it was first reached; how long each report took
  to reach the bot, and which source (AWC or TGFTP) delivered it first;
* the day-1 forecast's maximum and its error against the METAR high;
* per strategy: how often each blocker stopped it, any signals, and the
  three closest calls with how they would have ended if bought at the ask
  (a maker, G or J, at the bid it would rest). They are ranked by model EV
  for A, B, D and I; for E and F, which trade on a price, by fewest
  blockers and then the highest ask; for the other rules (G–K, the lab) by
  fewest blockers and then the EV, since a rule's probability only holds
  once its trigger fires;
* the model against the market on the bucket that won: average
  probability, log loss, and who was sure (≥ 0.90) first. Evaluations
  where that bucket still lay in the model's open last cell ("≥ high + 3")
  are left out and counted: the model only bounds such a bucket;
* proposals with the risk verdicts, paper orders, fills and settled P&L;
* requests, failures and latency per provider, health changes and logged
  events.

Outcomes are judged by the METAR high, as paper settlement does; today is
provisional. It only reads.

**In the browser (easiest).** Open `http://<host>:<port>/api/v1/report/paper`
(the dashboard's address; Basic auth applies if you set it). It covers the
days since the service first ran, at most the last 31. Choose days with
`?from=2026-09-26&to=2026-09-29`; add `&format=json` for JSON. Select all,
copy, paste.

**As a one-off container** (any range; the report is also saved as
`/data/reports/amsterdam-paper.md` and `.json` when the data volume is
mounted). In Portainer: *Containers → Add container*, image
`ghcr.io/spongi07/weathermachine:<tag>`, *Command* override
`report paper --print`, *Network* the stack's network
(`weather-machine_default`; *Networks* lists the actual name), env
`WM_DB_PASSWORD` with the stack's value, restart policy *Never*; then read
its *Logs*. Or with the Docker CLI:

```sh
docker run --rm --network weather-machine_default -e WM_DB_PASSWORD=… \
  ghcr.io/spongi07/weathermachine:latest report paper --print
```

The first start after updating to the version with this report builds a
small index on the event journal. It reads through the week of order books
the journal keeps, which can delay that start by a minute.

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
| Logs | JSON lines (Portainer → container → Logs): besides the providers and the restore, every paper fill, settlement and engine alert. Change verbosity with `RUST_LOG`. |
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
| Alert "strategy K needs KNMI's ten-minute readings: set WM_KNMI_API_KEY …" (or "strategy K and the lab's KNMI rules …", "the lab's KNMI rules …") | K or the lab is switched on without a key. Request a free key at the [KNMI Developer Portal](https://developer.dataplatform.knmi.nl/) (*EDR API*), set `WM_KNMI_API_KEY` and redeploy — or set `enabled = false` in `[strategies.knmi_nowcast]` and `[strategies.lab]`. |
| Alert "KNMI ten-minute readings unavailable (…); strategy K and the lab's KNMI rules wait" | The KNMI API refused or did not answer (HTTP 401/403: wrong or expired key; otherwise an outage). K and the lab's KNMI rules do nothing meanwhile; "KNMI ten-minute readings available again" follows on recovery. |
| Alert "KNMI readings of neighbour 06215 unavailable (…); the lab's L24 reads the others" | KNMI did not serve one neighbouring station; only L24 is affected. It is retried after every new Schiphol reading. |
| Alert "taker trades (Data API) unavailable (…); the lab's flow rules L18–L21 wait" | The Data API refused or did not answer; the service backs off and says "available again" on recovery. Nothing else waits for it. |
| Alert "the strategy lab's flow rules (L18–L21) need the Data API …" | `[providers.polymarket_data]` is switched off: L18–L21 stay idle. Switch it on, or disable those families. |
| Alert "takers' records incomplete: settled days would not download …" | Three settled days in a row would not download for the takers' scores; L19 and L20 use the days that loaded, and the next morning tries again. |
| Report says "not evaluated: …" | The forecast history could not be downloaded (e.g. `previous-runs-api.open-meteo.com` blocked); training is retried after 6 h. A "rejected by Open-Meteo" reason naming the model means `WM_FORECAST_MODEL` is not a valid model id. |
| Alert "persistence backlog — new positions blocked" | The database was slow for a moment (e.g. a backup or vacuum). Records are held and retried, nothing is lost; trading resumes with "persistence caught up". If it persists, check disk space and the `postgres` logs. |
| Dashboard says *storage DOWN*, no trades | Audit storage failing ⇒ fail closed by design; check database health/disk. |
| Providers *throttled* / *unavailable* | The rate limiter backs off (Retry-After honoured, circuit breaker); trading of that station stays blocked until data is healthy and fresh. Do not lower the polling floors. |
| `collector lease held by another instance` | Another Weather Machine is running against the same database; stop it. |
| Blank page, `/lite` works | Browser without WebAssembly, or a proxy stripping `application/wasm`; use `/lite`. |
