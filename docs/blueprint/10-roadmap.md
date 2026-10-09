# Roadmap — Phases 0–14

Legend: ✅ built and tested · 🟡 tooling ready, needs real-world data or a
live run · ⏳ not started · ⛔ deliberately blocked.

| Phase | Scope | Status | Exit criterion |
|---|---|---|---|
| 0 | NWS WRH investigation, rate-limit-safe retrieval | ✅ Desk research (§7–8); live since late September 2026: AWC and TGFTP both deliver EHAM's reports (TGFTP first on several), no throttling, knowledge delay 150 s median; 🟡 `confirmed_filter` still unset (the WRH page unverified) | WRH rows compared with AWC values; `confirmed_filter` decided |
| 1 | Rust workspace, domain model | ✅ | — |
| 2 | EHAM collector | ✅ mock providers (13 scenarios, budget test) and live: collecting since late September 2026 without throttling, failover AWC → TGFTP seen in production; the knowledge-delay distribution is in `report paper` | — |
| 3 | Historical EHAM storage | ✅ schema, raw payloads, versions; IEM importer | Multi-year EHAM METAR import with an import-statistics report |
| 4 | Polymarket market-data ingestion | ✅ Gamma, rules capture, CLOB, WS, book recorder | Books recorded continuously for today's/tomorrow's markets |
| 5 | Historical market database | ✅ recorder running since deployment; `research market` imports the settled days' trades from the Data API (123 days by 2 October 2026, cached, fidelity labelled) | — |
| 6 | ReplayEngine | ✅ knowledge-time session, prefix stability, journal replay | — |
| 7 | Peak-detection research | 🟡 automatic: the service trains from IEM history on first start and writes the survival report; review pending | Survival table with Wilson CIs on EHAM history, per view (all and hourly), reviewed |
| 8 | YES backtest | 🟡 engine ready; `research market` replays A at traded prices on settled days (both structures, 0′/30′/60′, two ask ranges) | Walk-forward EV per threshold 0.90…0.99 with CIs, stable neighbourhood |
| 9 | NO backtest | 🟡 engine ready; replayed at traded prices like A | Same, per distance +1/+2/+3 |
| 10 | Split/unwind backtest | 🟡 strategy C (research-only) and unwind styles ready | C vs wait-and-confirm on identical data, net of all costs |
| 11 | Forecast integration | ✅ Open-Meteo day-1 series live + history, forecast rise and headroom, prequential evaluation with placebo control and automatic adoption, walk-forward model-structure selection (§36b); 🟡 verdicts on real EHAM data come from the trainings on the host | A forecast feature improves walk-forward calibration (decided by the evaluation, §19) |
| 12 | Portfolio research | 🟡 scenario exposure and limits built | Correlation across days/cities measured; limits reviewed |
| 13 | Paper trading | ✅ running in Portainer since late September 2026 with the model trained on the host; open paper positions, realized P&L and the daily limits carried across restarts; F, G and K live (I until 8 October), the strategy lab (L1–L25) on books of their own since 2 October (17 families since 9 October) | 🟡 ≥ 4 weeks of paper results consistent with the replays (`report paper` against `research market`) |
| 14 | Live trading | ⛔ | See [09-operations.md §42](09-operations.md#42-live-trading) |

## First data experiment (brief §37) — how to run it

```sh
# On the deployment host (outbound HTTPS to aviationweather.gov / tgftp.nws.noaa.gov):
docker run --rm -e WM_CONTACT=you@example.org ghcr.io/spongi07/weathermachine:latest collect --once --no-db
```
This makes one request per source and makes zero trades. It shows the
underlying data (raw METARs), identifiers, timestamps, report types,
duplicates or corrections, provider health and the request count. The
proof-of-concept requirements are all implemented and tested: retrieve,
parse, latest observation, daily high (engine), deduplicate, provider
health, conservative polling, back off, zero trades.

## First strategy experiment (brief §38) — how to run it

1. Automatic: on first start the service downloads EHAM METARs from the IEM
   archive (2005 → today, one station-year per request, 15 s apart), trains
   the model and writes `/data/research/eham-survival.md`. Manual:
   `weather-machine model train`, or `research peak-survival --csv …`
   (repeat with `--filter hourly-nws-faa` for the hourly view).
2. Review P(final | N minutes) per season, hour, drop and trajectory, with
   CIs, the forecast evaluation (Phase 11: verdict, log loss with and
   without the forecast and against a placebo, P(final) by forecast rise or
   headroom) and the structure comparison (§36b) in the same report. Then
   prices (Phases 8–10): `research market`, with `--day` for single days.

## Immediate next steps

1. Let the paper stack run ([deployment guide](../deployment/portainer.md);
   CI publishes `ghcr.io/spongi07/weathermachine` on every push): it
   collects, records books, journals everything and paper-trades F, G and
   K, with the strategy lab on its own books.
2. Every few weeks, rerun `research market --from <date>` on the new
   settled days and compare with `report paper`: a strategy whose paper
   record and replay disagree is switched off, a lab family that holds up
   on new days and in paper is a candidate for its own letter.
3. Decide `confirmed_filter` once the WRH page has been compared with the
   AWC values on a few settled days (Phase 0's last item).
4. Review the survival report (`/data/research/eham-survival.md`) after
   each retraining (every 30 days). If it stops supporting the strategy,
   set `WM_MODEL_AUTO_TRAIN=false` and delete `/data/models/eham.json`: the
   engine returns to trading nothing.
