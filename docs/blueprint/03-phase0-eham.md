# 6–8 · Amsterdam configuration, NWS WRH investigation, EHAM data source

## 6. Amsterdam configuration

**PURPOSE.** Everything location-specific lives in data (`configs/locations/*.toml`),
never in strategy code. A new location is a new file (see §43).

```toml
[location]  id = "amsterdam", timezone = "Europe/Amsterdam"
[station]   id = "EHAM", wmo_id = "06240", latitude = 52.3156, longitude = 4.7903
            routine_minutes = [25, 55], first_poll_delay_secs = 90, arrival_window_secs = 720
[market]    event_slug_template = "highest-temperature-in-amsterdam-on-{month}-{day}-{year}"
            unit = "celsius", taker_fee_rate = "0.05"
            # confirmed_filter = "hourly_nws_faa"   ← only after Phase 0
[observation_sources] primary = "awc", secondary = ["tgftp"]
[peak]      watch_start_after_noon = -180, watch_end_after_noon = 300, watch_margin_tenths = 10
```

| Setting | Label | Notes |
|---|---|---|
| Time zone `Europe/Amsterdam` | KNOWN FACT | Defines the market's "specified day" (verbatim rules: "on the specified day"); DST handled by `chrono-tz`. |
| Routine METARs at HH:25/HH:55 UTC | ASSUMPTION (reported by the Dutch AIP GEN 3.5 and KNMI aviation pages) | Verified by the first data experiment: timestamps of `collect --once` output. Drives the arrival-window scheduler only; a wrong cadence costs latency, never correctness. |
| `taker_fee_rate = 0.05` | ASSUMPTION (secondary reports of Polymarket's 2026 fee schedule: weather category 0.05, fee = C × feeRate × p × (1 − p)) | Verify per market (the order's `feeRateBps`) before Phase 13. The fee model is exact once the rate is known. |
| `confirmed_filter` unset | Deliberate | Until verified, every candidate resolution view is evaluated and they must agree (§18). |

**FAILURE MODES.** Invalid time zones, station ids, minute values or rate
policies fail validation at startup (`AppConfig::validate`, fail closed).
Unknown keys are rejected (`deny_unknown_fields`).
**TESTING.** `shipped_configuration_is_valid`, `nws_floor_is_enforced_by_config_validation`.

## 7. NWS WRH technical investigation

**PURPOSE.** Know exactly what the resolution page shows and how it gets its
data, before building on it.

### What the market contract says (KNOWN FACT — verbatim rules of the Amsterdam NOAA markets, captured in `wm-polymarket/src/rules.rs` tests)

> "…the highest temperature recorded by NOAA at the Amsterdam Airport
> Schiphol Station in degrees Celsius… the highest reading under the "Temp"
> column for all times on the specified day, available here:
> https://www.weather.gov/wrh/timeseries?site=eham. This market resolves off
> of the Hourly Data provided using the "Show Hourly Data" button. … whole
> degrees Celsius (eg, 9°C)… Revisions to temperatures recorded within the
> market's timeframe will be considered until the first datapoint for the
> following date has been published… If NOAA data for the observation date is
> unavailable by 11:59 PM ET on the day following the observation date, the
> Weather Underground Daily Observations table will be used…"

Consequences, all implemented:
1. **Resolution sources change between markets (KNOWN FACT).** Amsterdam
   markets in May 2026 resolved on KNMI (13 May) and Weather Underground
   (12 May); September 2026 markets use NOAA WRH. So the rules of *every*
   market are captured verbatim, hashed (SHA-256) and parsed. Unknown
   clauses make the market non-tradable until a human approves that exact
   hash (`weather-machine rules approve`).
2. **"Hourly Data" is a filtered view.** The WRH "Show Hourly Data" toggle
   keeps one row per hour. It is reported, but not yet verified, that it keeps
   minutes :51–:59 for NWS/FAA platforms and :56–:04 for others
   (ASSUMPTION). If EHAM's hourly rows are the :55 METARs, **the :25
   reports do not count toward settlement.** A :25 high that falls back by
   :55 would never settle. The engine therefore evaluates an *all rows* view
   and each candidate filtered view, and trades only when they agree
   (`ObservationFilter::{AllRows, MinuteWindow}`, `FilterCertainty::Unconfirmed`).
3. **Whole degrees.** METAR temperatures are whole °C. The engine rounds
   half-up (ICAO) only where tenths exist (T-groups); EHAM METARs carry whole
   degrees (KNOWN FACT, METAR format).
4. **Revisions until the next day's first datapoint (KNOWN FACT, rules).**
   Corrections are first-class events (§14). A recent correction blocks new
   positions for a cooldown (`correction_cooldown_minutes`).

### How the page gets its data

| Finding | Label |
|---|---|
| The WRH time-series viewer renders client-side. The table is not in the HTML response, so HTML scraping would require executing JavaScript. | ASSUMPTION (secondary sources; confirm by viewing the network requests in a browser, no automated polling) |
| The viewer queries the Synoptic Data API (`api.synopticdata.com/v2/stations/timeseries`), which requires a `token` parameter; the page uses a token issued to NWS. | KNOWN FACT for the Synoptic API contract ([Synoptic docs](https://docs.synopticdata.com/services/time-series)); ASSUMPTION that WRH embeds an NWS-issued token |
| **Decision:** Weather Machine does **not** reuse the page's embedded token. It is not ours, and its terms and limits are undocumented for third parties. | Policy |

## 8. Underlying EHAM data-source discovery

Source preference, per the brief (structured API → machine-readable feed → structured download → HTML):

| Tier | Source | Status in code | Labels |
|---|---|---|---|
| Official structured API | **NOAA Aviation Weather Center Data API** `GET /api/data/metar?ids=EHAM&format=json&hours=N` | `AwcMetarSource` (primary) | KNOWN FACT: official, documented, JSON; fields include `icaoId`, `receiptTime`, `obsTime`, `reportTime`, `temp`, `metarType`, `rawOb`; documented limit 100 requests/min ([AWC Data API](https://aviationweather.gov/data/api/)). We use ≤ 2/min at peak, typically ≈ 2/hour. |
| Official machine-readable feed | **NWS TGFTP** `https://tgftp.nws.noaa.gov/data/observations/metar/stations/EHAM.TXT` (latest METAR, conditional GET) | `TgftpMetarSource` (failover) | KNOWN FACT: official NWS product distribution ([TG data help](https://www.weather.gov/tg/datahelp)). It holds only the latest report, so it cannot backfill a day. |
| Official API (US-centric) | `api.weather.gov/stations/EHAM/observations` | `NwsApiSource`, **disabled** | ASSUMPTION: coverage of non-US ICAO stations is uncertain; enable only if Phase 0 shows EHAM data. |
| Same backend as WRH | Synoptic Data API with **our own** token (Open Access or paid tier) | not implemented | Only if Phase 0 shows AWC/TGFTP values diverge from WRH. It would be one more `ObservationSource` with no architectural change. |
| Structured download (history) | Iowa Environmental Mesonet ASOS/METAR archive (network `NL__ASOS`, CSV `station,valid,metar`) | `import_iem_csv` | KNOWN FACT: public archive of international METARs ([IEM](https://mesonet.agron.iastate.edu/request/download.phtml)). Used for research only, never live. |
| HTML | WRH page | **not used** | Client-rendered; last resort, not needed. |

**Key hypothesis for Phase 0 (ASSUMPTION until measured).** WRH's EHAM rows
are the same METARs AWC distributes. Verification: for one or more days,
compare WRH "Temp" values (read manually in a browser) with the stored AWC
reports; record agreement in `docs/blueprint/phase0-results.md`.

### Phase-0 protocol (first data experiment, §37 of the brief)

The environment that built this repository had **no egress** to NOAA or
Polymarket hosts (every request returned status 000). The live part of Phase 0
therefore runs on the deployment host:

1. `weather-machine collect --once --no-db` → **exactly one request per
   configured source.** It prints each parsed report (observed time, type,
   °C, knowledge delay, raw METAR) and the provider's health.
2. `weather-machine collect` (with PostgreSQL) for ≥ 24 h. The collector is
   bounded by the NOAA gate: ≥ 30 s between requests, one at a time,
   ≤ 2,000/day. In practice it makes ≈ 50–70 requests/day, concentrated in
   the minutes after HH:25/HH:55.
3. Measure from `provider_requests` and `weather_observations`: cadence, SPECI
   frequency, COR corrections, duplicates, knowledge delay
   (`fetched_at − observed_at`), response sizes, 304 rate, error rates.
4. Compare with WRH hourly rows (manual), then set `confirmed_filter` in
   `amsterdam.toml` only if the rows match one filter unambiguously.

| Question from the brief | Where the answer comes from |
|---|---|
| actual underlying data source | §7 table + manual browser inspection |
| machine-readable availability | `collect --once` success per source |
| observation frequency / identifiers | `weather_observations.observed_at`, `report_type` |
| temperature representation | raw METAR `TT/DD` group (whole °C) |
| timestamps | `observed_at` vs AWC `receiptTime` vs our `fetched_at` |
| METAR/SPECI behaviour, corrections, duplicates | `DedupClass` counters, `weather_corrections` |
| response size, provider stability | `provider_requests.bytes`, `status`, `latency_ms` |

**FAILURE MODES.** A source that disappears, throttles or changes format is
isolated by its own gate and health tracker. The collector fails over, or the
station goes *Unavailable* and trading stops (fail closed). Malformed
payloads are stored raw and marked, never guessed.

**TESTING.** Fixture parsers for AWC JSON, TGFTP text and api.weather.gov
GeoJSON; 13 collector scenarios with a scripted mock server (429 +
Retry-After, 5xx, timeouts, malformed, duplicates, corrections, out-of-order,
long outage, recovery, six virtual hours of request budget).
