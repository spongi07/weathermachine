# Strategy lab: 25 new strategies (October 2026)

*Question (2 October 2026).* Invent 25 good strategies that cannot be found
on the internet: new combinations, with the internet as a source of
inspiration.

**Short answer.** Twenty-five strategies, L1–L25, each a combination of
inputs that no public weather bot or paper we found combines: KNMI's
ten-minute readings as a *shield* or a *stop* rather than only as a signal,
the METAR's own weather groups (sea breeze, showers, fog, fronts, and the
two-hour TREND that Schiphol's reports carry), the hourly path of the
forecast instead of its maximum, KNMI's global radiation against the clear
sky, the temperatures of upwind stations, and every taker's track record on
the days before. They are built as a **lab**: `weather-machine research
market` replays all of them at traded prices on the settled Amsterdam
markets, each with a variant or a control, and judges each family out of
sample. None trades live. A family that holds up on days it was not chosen
on, and again on the next run's new days, goes live under its own letter
with its own tests, as G–K did.

## 1. What others do — and what is left

| Source | What they do / found | What the lab takes from it |
|---|---|---|
| [jattree/weather-edge](https://github.com/jattree/weather-edge) (post-mortem) | A multi-model forecast consensus against Polymarket daily highs: $210 → $51.61 (−75 %); over 697 events the market's log loss 1.31 against the model's 1.59; 1,905 simulated trades −13.9 % a trade. | Do not trade forecast against price; trade what the forecast cannot know yet (L14 uses the forecast's *path*, L15–L17 its timing and its error). |
| [abelianbee/isobar](https://github.com/abelianbee/isobar) | Walk-forward: a 13-model blend beat Kalshi by +4.8¢ a contract (6,977 trades, event-clustered t = 9.17), mostly in winter (+7.6¢ against +2.2¢ in summer); Kalshi's prices predict Polymarket's settlement (information share 40 %). | Forecast errors are structured (L14, L17); the season matters. |
| [BallesJr/polymarket-weather-edge](https://github.com/BallesJr/polymarket-weather-edge) | Same-day NO at 0.15–0.40 from METAR plus a Gaussian: 527 trades at 32.5 % won, then 847 at 28.9 %, −$592. | A plain observation-vs-price rule decays; the lab adds a physical trigger to every rule. |
| [Celmaro/polymeteo](https://github.com/Celmaro/polymeteo), [CEPR DP21615](https://cepr.org/publications/dp21615) ([summary](https://www.theblock.co/post/398902/skilled-polymarket-traders-are-a-3-minority-and-everyone-else-funds-their-gains-study)) | Copying profitable weather wallets: the edge decays within about a second. Skill on Polymarket is concentrated (~3 % of traders move prices right) and persistent. | L19 follows skill scored only on earlier days; L18 copies a burst only when KNMI confirms it; L20 fades the losers. |
| Kalshi "reactive lock-in" ([write-up](https://edge-assist.duckdns.org/data-status/docs/weather/ruling/Weather%20Addendum%20%E2%80%94%20Extended%20Climate%20Markets)) | After the peak, 4 °F below the running high: sell the strikes above it. | L4 and L5 replace the 4 °F and the clock with KNMI's ten-minute maxima and trend. |
| [suislanchez](https://github.com/suislanchez/polymarket-kalshi-weather-bot), [tobiasbischoff](https://github.com/tobiasbischoff/polymarket-weather-bot) and the other GitHub bots | Ensemble counts against the price, edge > 8 %, taker orders. | The crowded trade: none of the 25 does it. |
| [EUMeTrain: sea breeze](https://resources.eumetrain.org/satmanu/CMs/SB/navmenu.php?page=5.0.0) | Schiphol lies ~15 km inland; the sea-breeze front passed it around 13–14 UTC with a small drop in temperature and a rise in humidity. | L8 |
| [Frontiers in Earth Science 2023](https://www.frontiersin.org/journals/earth-science/articles/10.3389/feart.2023.1099344/pdf); [fog burn-off busts](https://theweatherprediction.com/habyhints2/371/) | The error of maximum-temperature forecasts grows with cloud cover; a stratus deck that does not mix out busts the maximum. | L11, L12 |
| [NWS Goodland: wet microburst](https://www.weather.gov/gld/MingoWetMicroburst) and gust-front studies | Rain-cooled outflow drops the surface temperature by 3–8 °C. | L9, L23 |
| [KNMI AUTOTREND](https://www.knmi.nl/research/publications/autotrend-automated-guidance-for-short-term-aviation-weather-forecasts); [LVNL eAIP GEN 3.5](https://eaip.lvnl.nl/web/2025-01-09-AIRAC/html/eAIP/EH-GEN-3.5-en-GB.html) | Schiphol's METARs carry a two-hour TREND (BECMG/TEMPO/NOSIG) on cloud, visibility, wind and weather, from KNMI's guidance and forecaster. | L10 |
| [KNMI ten-minute dataset](https://dataplatform.knmi.nl/dataset/10-minute-in-situ-meteorological-observations-1-0); [station data](https://www.knmi.nl/nederland-nu/klimatologie/uurgegevens) | `qg` global radiation (W/m²), `ta` temperature, wind, humidity … every ten minutes for every automatic station (Valkenburg, De Bilt, Berkhout among them). | L24, L25 |
| [Haurwitz clear-sky model (pvlib)](https://pvlib-python.readthedocs.io/en/stable/reference/generated/pvlib.clearsky.haurwitz.html) | GHI = 1098 · cos z · exp(−0.057 / cos z). | L25's clear-sky index |
| [Choi & Hui 2014](https://researchportal.hkust.edu.hk/en/publications/the-role-of-surprise-understanding-overreaction-and-underreaction/) | In-play betting overreacts to big surprises and underreacts to small ones; the overreaction fades within minutes. | L21 |
| [Late informed betting (arXiv:2509.14645)](https://arxiv.org/html/2509.14645v1) | Returns fall with last-minute odds moves: informed money comes late. | L18 |
| [Easley, López de Prado & O'Hara, flow toxicity](https://papers.ssrn.com/abstract=1695596) | Makers lose to informed flow; toxicity can be measured. | L1, L2 (a physical sensor as the toxicity filter) |
| [Polymarket liquidity rewards](https://docs.polymarket.com/market-makers/liquidity-rewards) | Resting orders near the midpoint earn rewards, quadratic in the distance. | L1, L2, L20, L22 (rewards not counted) |
| [Upstream conditions](https://theweatherprediction.com/habyhints2/422/) | The air upwind is the air of the next hours. | L24 |

## 2. How the lab replays

`research market` replays every settled market day report by report, as for
G–K ([§9 of the edge research](edge-research.md#9-october-2026-five-new-strategies)):

* **Decisions** at every METAR from local midnight, known `--delay-secs`
  after the observation; KNMI readings known `knmi_delay_minutes` (5) after
  their interval ends — the live dashboard measures the real delay.
* **Takers** pay the latest trade of the kind they need when it is at most
  ten minutes old (one minute for the rules that race the METAR), else the
  first acceptable one while the signal stands, plus 0.005 slippage and the
  taker fee 0.05 · p · (1 − p). **Makers** rest one tick better than the
  latest trade on their side and fill only when a later trade goes through
  their price; no fee, 25 % of it as rebate.
* **One trade a day per rule and bucket** (and side). $20 a trade; L3 at F's
  100 shares so that it compares with F itself.
* **No look-ahead.** Takers are scored only on the market days before the
  one replayed; yesterday's forecast error only once yesterday is over; the
  forecast only from its knowledge time; F's exit only after its fill.
* **Inputs.** The METAR's weather groups are parsed from the archived raw
  reports ([`metar_wx`](../../crates/wm-weather/src/metar_wx.rs)); the day-1
  forecast is read even when the installed model does not use it; with
  `WM_KNMI_API_KEY` set, KNMI's global radiation at Schiphol and the
  temperatures of Valkenburg (06210), De Bilt (06260) and Berkhout (06249)
  are downloaded a week at a time (about 72 requests for four months, well
  inside the 1,000 an hour) and cached under `research/knmi-series/`. A
  station or parameter the API refuses twice in a row is given up; its
  rules are reported as not replayed.
* **Judged out of sample.** Per family, the rule with the best P&L on the
  first half of the market days is scored on the second half, and the main
  rule's own second half is shown when another was chosen. The report's
  last lab line names the families whose chosen rule held up: the 95 %
  interval of the P&L a trade above zero on the later days, over at least
  five trades on three days (a few wins alone give an interval of no
  width).

**Many tries.** Twenty-five families and fifty-one rules: on the first half
one or two will look good by chance. A family counts only when its later
days hold up, and then again on the next run's new days. Every variant is
fixed here, before any result.

## 3. The 25 strategies

| | Name | Inputs | Trade | Variant or control |
|---|---|---|---|---|
| L1 | KNMI-shielded maker on the next degree | KNMI, tape | maker NO, next degree | without the shield |
| L2 | KNMI-informed maker on the doomed bucket | KNMI, tape | maker NO, high's bucket | margin 0.6 °C |
| L3 | F's escape hatch | KNMI, F's trades | sell F's YES before the METAR | exit at +0.0; hold (F) |
| L4 | KNMI cooling lock | KNMI | YES, high's bucket 0.70–0.95 | 0.90–0.98 |
| L5 | Late next-degree NO under KNMI cooling | KNMI | NO, next degree | from 17:00 |
| L6 | K late: YES of the new degree | KNMI | YES, next degree | unfiltered (K · YES above) |
| L7 | KNMI slope: K one reading early | KNMI | NO, high's bucket | to +1.0 °C |
| L8 | Sea-breeze lock | METAR wind, dew point | NO, next degree | YES of the high's bucket |
| L9 | Rain-cooled cap | METAR weather | NO, next degree | thunder only |
| L10 | TREND cap | METAR TREND | NO, next degree | NOSIG lock (YES of the high) |
| L11 | Fog and stratus fade | METAR visibility, cloud | NO, the favourite | 4 °C gap |
| L12 | Clear dry morning: the bucket above | METAR cloud, dew point, wind | YES above the favourite | any wind |
| L13 | Cold-front early-high lock | METAR wind, QNH | YES, high's bucket | NO of the next degree |
| L14 | Morning departure from the hourly forecast | forecast path | YES, predicted bucket | λ = 1.0 |
| L15 | Past the forecast's own peak hour | forecast timing | YES, high's bucket | +60′ |
| L16 | Evening-high days | forecast timing | YES, next degree | rise ≥ 0.5 °C |
| L17 | Yesterday's forecast error | forecast, yesterday's high | YES, adjusted bucket | the full error |
| L18 | Pre-report burst, confirmed by KNMI | tape, KNMI | NO, high's bucket | without KNMI |
| L19 | Follow skilled takers | wallets (prequential) | their side | t ≥ 3 |
| L20 | Fade losing longshot buyers | wallets (prequential) | maker NO at their price | as a taker |
| L21 | Fade the jump after a new high | tape, KNMI | NO, two above | the next degree |
| L22 | Overnight tail maker around the favourite | tape | maker NO, far tails | ≥ 4 places |
| L23 | After the shower: the recovery | METAR weather, KNMI | YES, next degree | METAR slope |
| L24 | Upwind KNMI station | KNMI neighbours, METAR wind | NO, high's bucket | cool side |
| L25 | Radiation collapse | KNMI radiation | NO, next degree | the clearing (YES) |

"Edge" below is the high's rounding edge: the high + 0.5 °C, above which
the next METAR reports the next degree. All times are local.

### KNMI's ten-minute readings as shield, stop and early warning

**L1 — KNMI-shielded maker on the next degree.** Makers on the next degree
lose just before reports (−0.66¢ a share; on the high's bucket −1.18¢ in
the last five minutes): the takers who hit them know the METAR first. KNMI
says when no rise is coming — no report of 3,448 rose with the mean ≥ 1 °C
under the edge, 0.5 % with it 0.5–0.9 °C under. *Rule:* 10:00–20:00, at
each reading, when the three means of the last 30′ are ≥ 0.5 °C under the
edge and not climbing, rest a NO bid on the next degree one tick under its
YES ask (0.03–0.40) until the next reading or the report after it is known.
The quote stays up exactly when other makers pull theirs. *Control:* the
same quotes without the shield. *Risk:* the shield covers the next report
only.

**L2 — KNMI-informed maker on the doomed bucket.** K's signal as a maker:
when KNMI's mean is ≥ edge + 0.3 °C before the METAR (the next report rose
194 times of 199), rest a NO bid on the high's bucket one tick under its
YES ask (YES 0.25–0.97) until the anticipated report is public. No fee, a
rebate, and an earlier threshold than K's 0.8 °C. *Variant:* margin
0.6 °C (137 of 137 rose). *Risk:* few fills — only someone still buying the
doomed YES fills it.

**L3 — F's escape hatch.** F is about break-even and its losses were late
new highs, which KNMI sees minutes before the METAR. *Rule:* after F's
fill, at the first reading newer than the last METAR, ≤ 16′ before the next
report, with the mean ≥ the bucket's rounding edge + 0.3 °C, sell the YES
into the bid. *Variants:* exit at +0.0 °C; hold (F itself, the control on
the same trades). *Risk:* the bid collapses as fast as others learn; false
exits cost the spread.

**L4 — KNMI cooling lock.** Favourites are underpriced (0.70–0.90 won
83.3 % at 79.9 %; 0.90–0.98 96.9 % at 94.9 %). The Kalshi lock-in waits for
a 4 °F drop after the clock's peak; L4 asks KNMI. *Rule:* 13:00–20:00, all
maxima of the last 90′ ≥ 0.3 °C under the bucket's upper edge, the mean
down ≥ 0.6 °C in an hour, the METAR ≥ 1 °C under the high → YES of the
high's bucket at 0.70–0.95 with an expected profit at p = 0.97. *Variant:*
F's band 0.90–0.98. *Risk:* a second, evening peak.

**L5 — Late next-degree NO under KNMI cooling.** G's tails, one degree
closer and late. *Rule:* 15:00–21:00, every mean of the last hour ≥ 1 °C
and every maximum ≥ 0.5 °C under the edge, the mean lower than an hour ago
→ NO of the next degree at 0.75–0.97 (p = 0.99). *Variant:* from 17:00.
*Risk:* small wins, one loss costs many.

**L6 — K late: YES of the new degree.** K · YES above lost $583 because
highs kept rising after the new degree. Late in the day and flattening, the
new degree should be the last. *Rule:* K's trigger (mean ≥ edge + 0.8 °C)
after the season's median peak time with a rise of ≤ 0.4 °C in 30′ → YES of
the next degree at 0.20–0.70. *Control:* unfiltered (K · YES above).

**L7 — KNMI slope: K one reading early.** The 20′ slope of the mean,
projected to the report, says ten minutes before K's threshold where the
METAR will land (K with the reading known after 2′ instead of 5′ made 12
trades, all won). *Rule:* mean within 0.5 °C under to 0.8 °C over the
edge, rising, projected ≥ edge + 0.6 °C → NO of the high's bucket at
≤ 0.75 (p = 0.85). *Variant:* projected ≥ edge + 1.0 °C. *Risk:* warming
that stalls.

### The METAR's weather groups

**L8 — Sea-breeze lock.** At Schiphol the sea breeze arrives in the early
afternoon with a small drop and a humidity rise; the day's high is then
usually in. No bot reads the wind. *Rule:* 11:00–17:30 on a day ≥ 20 °C,
the wind now 250–020° at ≥ 6 kt after an offshore report (060–220°) since
07:00, the dew point ≥ 1 °C above the high's report, ≥ 1 °C under the high
→ NO of the next degree at 0.60–0.96 (p = 0.95). *Variant:* YES of the
high's bucket at 0.50–0.92. *Risk:* the breeze retreats.

**L9 — Rain-cooled cap.** Outflow cools the station 3–8 °C. *Rule:*
12:00–19:00, rain, showers or thunder now or since the last report, ≥ 2 °C
under the high → NO of the next degree at 0.60–0.96 (p = 0.95). *Variant:*
thunder or a cumulonimbus only. *Risk:* the rebound (L23 trades it).

**L10 — TREND cap.** Schiphol's METARs end with KNMI's two-hour TREND; the
settlement source itself says when showers, an onshore wind or low cloud
are coming. *Rule:* 11:00–17:00, a BECMG/TEMPO with showers or thunder, a
wind from 240–020° or a ceiling under 3000 ft → NO of the next degree at
0.55–0.94 (p = 0.92). *Variant:* the NOSIG lock — after the median peak
time, NOSIG, no ceiling under 5000 ft, ≥ 1 °C under the high → YES of the
high's bucket at 0.55–0.92 (p = 0.93). *Risk:* TREND serves aviation
thresholds, not temperature.

**L11 — Fog and stratus fade.** A deck that does not mix out busts the
maximum forecast, and the market is anchored on it. *Rule:* 08:30–11:30,
fog or mist under 5 km or a ceiling ≤ 800 ft, the favourite (≥ 0.30) ≥ 6 °C
above the temperature → NO of the favourite at 0.30–0.70. *Variant:* 4 °C.
*Risk:* summer fog burns off by ten.

**L12 — Clear dry morning: the bucket above.** Dry continental air under a
clear sky heats past the forecast; the 0.10–0.30 band already won 21.3 % at
18.8 %. *Rule:* 10:00–13:00, no ceiling under 5000 ft and no fog or
precipitation, the dew point ≥ 8 °C under the temperature, the wind
045–225° or calm → YES of the bucket above the favourite (≥ 0.25) at
0.06–0.30. *Variant:* any wind.

**L13 — Cold-front early-high lock.** A front through in the morning sets
the high early (25 % of winter and 11 % of autumn highs before 09:00); the
market and F wait for the afternoon. *Rule:* 09:00–15:00, the high first
reached before 11:00, the wind veered from 120–229° at the high's report to
230–340° (≥ 8 kt), QNH up ≥ 1 hPa, ≥ 1.5 °C under the high → YES of the
high's bucket at 0.40–0.88 (p = 0.90). *Variant:* NO of the next degree.
*Risk:* rare in summer, the replay's season.

### The forecast's path, not its maximum

**L14 — Morning departure from the hourly forecast.** Bots compare the
forecast maximum with the price; the forecast's own hour-by-hour path says
how far the morning is off. *Rule:* 10:00–12:30, predicted maximum = the
forecast's remaining maximum + 0.7 × (observed − forecast now); if its
bucket is not the market's favourite, YES of it at 0.05–0.35. *Variant:*
λ = 1.0. *Risk:* the market beats forecast models (§1).

**L15 — Past the forecast's own peak hour.** F's slot is the season's; the
forecast knows today's peak hour. *Rule:* ≥ 120′ after the forecast's peak
(from 12:00), the forecast falling ≥ 1 °C, ≥ 1 °C under the high → YES of
the high's bucket at 0.70–0.95 (p = 0.96). *Variant:* +60′.

**L16 — Evening-high days.** On warm-advection days the forecast peaks in
the evening; F's late losses were such days. *Rule:* when the forecast's
17–24 h maximum beats its 10–17 h one by ≥ 0.5 °C: 14:00–18:00, the
forecast still rising ≥ 1 °C, within 1 °C of the high → YES of the next
degree at 0.05–0.30. *Variant:* rise ≥ 0.5 °C.

**L17 — Yesterday's forecast error.** The same air mass and the same grid
cell repeat yesterday's miss. *Rule:* 07:00–10:00, today's forecast maximum
+ 0.5 × (yesterday's observed high − yesterday's forecast maximum); when its
bucket differs from the raw forecast's, YES of it at 0.05–0.30. *Variant:*
the full error. *Risk:* traders see yesterday's result too.

### Who trades: flow and track records

**L18 — Pre-report burst, confirmed by KNMI.** $3,372 was taken before
reports by traders who knew first; copying them works only within about a
second. Here a burst is followed only when KNMI says it is right. *Rule:*
from 6′ before a routine report until it is known, ≥ 40 shares from ≥ 2
takers within 3′ selling the high's bucket or buying the next degree, and
KNMI ≥ edge + 0.3 °C → NO of the high's bucket 10″ later (0.05–0.85).
*Control:* the bursts without KNMI.

**L19 — Follow skilled takers.** Skill on Polymarket is a persistent
minority. Each taker is scored on the market days before (P&L a share after
the fee). *Rule:* a taker with ≥ 30 earlier trades, ≥ +0.02 a share and
t ≥ 2: their side 30″ later, within 10′, at ≤ their price + 0.03 (their
price 0.05–0.90). *Variant:* t ≥ 3. *Risk:* skilled accounts are mostly
makers, whose orders the tape does not name.

**L20 — Fade losing longshot buyers.** Longshots are overpriced (0.00–0.02
won 0.1 % at 0.4 %), and some takers keep buying them. *Rule:* a taker with
≥ 30 earlier trades losing ≥ 0.05 a share (t ≤ −2) buys YES at 0.02–0.15 →
rest a NO bid one tick under their price for 30′. *Variant:* take NO at
0.80–0.98 within 10′.

**L21 — Fade the jump after a new high.** A new high is a surprise for the
buckets above it; in-play markets overreact to big surprises for minutes.
*Rule:* 2–15′ after a METAR raised the high, the bucket two degrees above
bought at ≥ 0.08 since the report and KNMI ≤ the new edge + 0.3 °C → its NO
at 0.60–0.92. *Variant:* the next degree (bought at ≥ 0.15; NO 0.50–0.85).

**L22 — Overnight tail maker around the favourite.** Resting orders were
paid overnight (+0.74¢ a share 00–09) and at 0–2¢ (+0.34¢); J lost by
quoting the middle. *Rule:* 00:00–09:00, NO bids one tick under the YES ask
(0.01–0.05) on buckets ≥ 3 places from the market's favourite (≥ 0.20) that
can still win, withdrawn 10′ before each report. *Variant:* ≥ 4 places.

### Weather that changes during the day

**L23 — After the shower: the recovery.** The opposite of L9: once the
shower has passed and the sky clears, the sun heats again while the market
still prices the cap. *Rule:* 12:00–16:30, rain in the last 3 h, now dry
with no ceiling under 4000 ft, within 1.5 °C of the high, KNMI's mean up
≥ 0.5 °C in 30′ → YES of the next degree at 0.05–0.35. *Variant:* the
METAR's slope ≥ 1 °C/h instead of KNMI.

**L24 — Upwind KNMI station.** The air 30–40 km upwind is Schiphol's next
hour. *Rule:* 10:00–18:00, the METAR wind ≥ 6 kt from within 40° of a
neighbour's bearing (Valkenburg ~236°, De Bilt ~132°, Berkhout ~20°): the
neighbour ≥ 0.8 °C warmer than Schiphol in the same ten minutes and
Schiphol within 0.6 °C under the edge → NO of the high's bucket at ≤ 0.75
(p = 0.80). *Variant:* the cool side — from 12:00 the neighbour ≥ 1.0 °C
cooler and Schiphol ≥ 0.5 °C under the edge → NO of the next degree at
0.65–0.96 (p = 0.93). *Risk:* a coast–inland difference that is local, not
advected.

**L25 — Radiation collapse.** Solar nowcasting watches the clear-sky index;
the temperature follows the sun with a lag. *Rule:* 10:30–15:30, KNMI's
global radiation over the last 30′ ≤ 35 % of the clear sky (Haurwitz) after
≥ 70 % over the hour before, KNMI ≥ 0.3 °C under the edge → NO of the next
degree at 0.60–0.95 (p = 0.93). *Variant:* the clearing — ≥ 80 % after
≤ 40 %, KNMI within 0.8 °C under the edge → YES of the next degree at
0.05–0.35.

## 4. Running it and reading it

Run `research market` as before ([deployment guide](../deployment/portainer.md));
with the KNMI key in the shell it also downloads the lab's KNMI series. The
report gains a section **Strategy lab: L1–L25 at traded prices** with the
inputs each day had, a line per family (or why it was not replayed), the
out-of-sample lines and a table of all rules; the summary repeats which
families held up. To take one live:

1. its chosen rule held up on the later days (interval above zero, at
   least five trades on three days) — and the main rule was not far
   behind;
2. it holds up again on the next run's new days;
3. it is built as a live strategy with its own tests and runs in paper
   (`weather-machine report paper`) before any money.

## 5. Caveats

* One city, one summer of markets, correlated within each day; several
  rules will trade only a handful of times.
* Fills come from the public tape: depth and queue position are not
  archived, so every fill assumes the whole stake was there.
* KNMI's delay is assumed (5′); the dashboard measures it. L1, L2, L3, L7,
  L18, L21 and L24 race the METAR and depend on it most.
* The METAR rules read the archived raw reports; a station whose reports
  carry no TREND (outside the Netherlands) gives L10 nothing to read.
* The neighbours and their bearings are Schiphol's; for another station
  L24 is not replayed.
