# Strategy lab: 65 strategies in paper (October 2026)

L1–L25 (2 October 2026, §1–§7) are replayed at traded prices and run in
paper; L26–L65 (14 October 2026, §8) are forty more from how this market
works, run in paper only.

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
sample. Since 2 October 2026 they also run in the live service as
**paper strategies**, each on a paper book of its own, so they never block
A–K or each other; 17 of them since 9 October (§7). None trades money: a family that holds up on days
it was not chosen on, again on the next run's new days and in its paper
record is a candidate, nothing more.

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
| [KNMI ten-minute dataset](https://dataplatform.knmi.nl/dataset/10-minute-in-situ-meteorological-observations-1-0); [station data](https://www.knmi.nl/nederland-nu/klimatologie/uurgegevens) | `qg` global radiation (W/m²), `ta` temperature, wind, humidity … every ten minutes for every automatic station (Voorschoten, De Bilt, Berkhout among them; [Valkenburg closed in 2016, Voorschoten replaced it](https://www.knmi.nl/research/publications/analysis-of-parallel-measurements-of-the-automatic-weather-stations-valkenburg-and-voorschoten-in-the-netherlands)). | L24, L25 |
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
  reports ([`metar_wx`](../../crates/wm-core/src/metar_wx.rs)); the day-1
  forecast is read even when the installed model does not use it; with
  `WM_KNMI_API_KEY` set, KNMI's global radiation at Schiphol and the
  temperatures of Voorschoten (06215), De Bilt (06260) and Berkhout (06249)
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
neighbour's bearing (Voorschoten ~231°, De Bilt ~132°, Berkhout ~19°): the
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
3. its paper record (§7, `weather-machine report paper`) agrees — before
   any money, which would need its own decision and its own letter.

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

## 6. The first run (2 October 2026)

`research market` over 123 settled days (1 June – 1 October 2026) with
every input: KNMI's readings, the day-1 forecast, KNMI's radiation and the
neighbours on all days, 5,885 reports' weather groups, 80 F trades, 8,291
takers scored (71 skilled, 58 losing by the end). Valkenburg (06210)
returned no data: the station closed in 2016, and Voorschoten (06215), its
successor 3 km inland, replaces it from the next run. *Later days*: the 62
days from 1 August, on which no rule was chosen.

| | all days | later days | reading |
|---|---|---|---|
| L1 shield | 98 trades, 81 won, +$94.18; without the shield 212 trades, −$254.89 | L1: 41 trades, −$25.01 | KNMI's shield turns the makers' loss before reports around (−$1.20 → +$0.96 a trade); what is left is about zero |
| L4 cooling lock | 20 trades, 18 won, +$19.28 | 8 of 8 won, +$29.05 (95 % CI +1.68 … +5.68 a trade) | the only family named as held up — but it lost $9.77 in June–July |
| L7 KNMI slope | 20 trades, 13 won, +$402.72 | 7 trades, 3 won, −$3.52 | the profit came from a few cheap NOs in June–July |
| L14 morning departure | 104 trades, 31 won, +$663.27 (YES at 0.21 won 30 %) | chosen λ 1.0: 57 trades, −$186.78; the main rule: 51 trades, 15 won, +$199.41 (CI −6.44 … +16.70) | the main rule earned in both halves (+$463.86 and +$199.41): the most promising new idea, not proven |
| L18 burst + KNMI | 4 of 4 won, +$30.58; without KNMI 122 trades, −$580.12 (CI −9.50 … −0.02) | 3 of 3, +$22.84 | following flow blindly loses; KNMI decides which bursts to follow |
| L24 upwind, cool side | 10 trades, 9 won, +$40.71 | 3 of 3, +$19.86 | without the coastal station; rerun with Voorschoten |
| L3 F's escape hatch | +$10.91 against F's own +$11.13 | — | selling at the bid does not save F's losses: the bid is gone by then |
| L9, L12, L17, L19, L21, L23 | −$79.31, −$504.78, −$397.84, +$53.31, −$85.53, −$130.74 | all negative | refuted: the market prices showers, sunny mornings, yesterday's error, the skilled wallets (L19 lost $613.09 on the later days) and the jump after a new high |

L2, L5, L6, L8, L11, L13, L15, L16, L22 and L25 traded too rarely to say
anything (L11 and L13 wait for fog and fronts: autumn and winter).

**Decisions.** Nothing from the lab trades money. L4 and L14's main rule get
their confirmation on days no rule was chosen on: after about 20 new
settled days, `research market --from 2026-10-02` replays only those days,
and a rule that does not earn there is dropped. What works, here and in
G–K, is one thing: KNMI's ten-minute readings ahead of the METAR (K, L7,
L18, L1's shield). The market prices public weather information well; the
edge is being earlier.

On the operator's request the same day, all 25 families were built as
paper strategies (§7): their live record now grows beside the replay's, at
no cost and without touching A–K.

## 7. Paper trading (live, from 2 October 2026)

The families run in the live service — **paper only**, like everything
the service trades. They see the same markets, books, reports and KNMI
readings as A–K, at the same moments, and their record is kept apart. All
25 ran from 2 October 2026; since 8 October the shipped configuration
switches off the six families the replay refuted (L9, L12, L17, L19, L21,
L23) and L22, whose 1–5¢ band the far tails never offer
([strategy review §3.5](strategy-review.md#35-lab-notes)); since 9 October
L1 too: its shield brought the next-degree maker to about zero, the
replay's later days lost $25.01 over 41 trades and its first live week
$7.00 over three ([live review](live-review-2026-10-09.md)); since 12
October ten more: L10, L11 and L14, which lost live (−$8.88, −$22.13,
−$23.88), and L2, L5, L6, L8, L13, L15 and L16, which made no trade in nine
days and too few in the replay to show an edge
([live review of 12 October](live-review-2026-10-12.md)); since 14 October
L3, which enters as F does: F was switched off that day, and L3's escape
did not fire on F's loss of 14 October
([live review of 14 October](live-review-2026-10-14.md)). Six run: L4, L7,
L18, L20, L24 and L25. Their replay code stays: `research market` replays
all 25.

**Own books.** Each lab strategy trades a paper book of its own: its
positions (and the order that opened each), its live orders, its risk
limits and counters and its realized P&L. A lab trade never counts toward
the main book's caps (A–K and the unwind engine) or another lab strategy's,
so none of them can block another: K and L7 may hold the same NO at once,
each in its own book
([`lab_session.rs`](../../crates/wm-backtest/tests/lab_session.rs)). The
unwind engine exits the main book's positions only. A restart gives each
fill back to the book of the strategy whose order it filled, with each lab
strategy's new exposure of the day; a family switched off in the meantime
keeps what it holds on its own book until it settles.

**Rules.** Every threshold is the replay's main rule (§3): the same time
windows, price bands and win probabilities
([`wm-strategy/src/lab`](../../crates/wm-strategy/src/lab/mod.rs)).
`variants = ["L2"]` runs a family's second rule instead, `disabled =
["L19"]` switches a family off (`[strategies.lab]`). How they trade:

* takers buy at the ask, fill-and-kill: shares for the notional ($20) at
  the ask, no more than the book offers at that price, at least the
  market's minimum order;
* makers (L1, L2, L20, L22) rest a NO bid good-till-date — one tick inside
  the NO book's spread (L20: offering the YES one tick under the price the
  losing taker paid) — that expires as replayed and is held to settlement
  once filled; paper fills follow the simulator's maker rule (a later trade
  print at or through the price, after the queue ahead);
* L3 enters as F does (F's slot, filters and 100 shares at up to 0.95) and
  sells the YES at the bid, fill-and-kill, when KNMI's reading says the
  next METAR will kill its bucket;
* the rules that took one trade a day in the replay take one a day here,
  and no rule holds a token twice.

**Risk** (`[strategies.lab.risk]`, per book). $25 an order ($100 for L3),
$120 in open positions and live orders, $120 of new exposure and $100 of
loss a day, books up to 0.10 wide, 10 orders a minute. A rule names a book
its risk check would refuse, wider than that or one-sided, as a blocker of
its own and waits, instead of proposing into a rejection on every update. Everything else is
the main `[risk]`: data freshness, prices 0.01–0.99, the kill switch, a
stale or throttled weather source.

**Inputs** (`[lab]`).

| input | source | read by |
|---|---|---|
| Schiphol's ten-minute readings (180 minutes back at start) | KNMI EDR, `WM_KNMI_API_KEY` | L1, L2, L3's exit, L4–L7, L18, L21, L23, L24 |
| global radiation `qg` against the clear sky | the same request | L25 |
| Voorschoten, De Bilt, Berkhout | one KNMI request each after every new reading | L24 |
| the METAR's wind, weather, cloud, pressure and TREND | the reports' raw text | L8–L13, L23, L24 |
| the hourly day-1 forecast and yesterday's error | Open-Meteo, read even when the model does not use it; today's series from the knowledge rule's ready time (08:00 local), which the rules name until then | L14–L17 |
| today's taker trades, wallets hashed as in `research market`'s cache | Data API, every 20 s | L18–L21 |
| every taker's record on the last 60 settled days | Gamma and the Data API, at start and at 07:00 local | L19, L20 |

Without the KNMI key the KNMI rules stay idle and L3 enters as F does
without its exit; the dashboard says so. With the Data API off, L18–L21
stay idle. L19 and L20 wait for the takers' records: the first scoring
downloads up to 60 settled days into
`<data_dir>/research/polymarket/<STATION>/`, the cache `research market`
uses, so it takes a while once; every later morning adds a day.

**Reading the results.**

* The dashboard's panel *Strategy lab · paper · L1–L65*: each running
  family's status now (its signal or its first blocker), its approvals,
  rejections, orders and fills this run, and its own book's open positions
  and realized P&L; above it, per location, what the lab reads (KNMI's
  readings, the radiation and its share of the clear sky, the neighbours,
  the latest METAR's weather, the forecast maximum, yesterday's error,
  today's taker trades and the takers' records). The KPI strip, positions
  and orders panels show the main book only.
* Each family's page and copyable log (`/api/v1/strategies/L7/log`), with
  its own book's positions.
* The lite page (`/lite`) lists the lab's books; its positions table names
  each position's book.
* `weather-machine report paper`: the main book's settled P&L, and the
  lab's apart, both split per strategy.

**Caveats.** Paper fills are simulated: a taker fills against the live
book after the simulator's latency, a maker only from later trade prints
after the queue ahead — and no order reaches the market, so nobody reacts
to it. One city, correlated days, and 25 rules on the same days are 25
chances to look good by luck: a family's paper record counts only on days
after its rule was fixed, and only together with the replay (§4).


## 8. L26–L65: forty families from how the market works (14 October 2026)

*Question (14 October 2026).* The lab as it ran was not working and lost:
think of strategies that do not exist yet — combinations nobody tried,
aggressive, defensive and balanced — with no signal reused, at least 25,
then ten more "ways we have not considered", then five more; paper trade
them all in the lab. Midway: rely less on the internet and reason about how
the market works; take the bot's latency into account (250 ms to Polymarket
and 100 ms from decision to order on the server) so that no strategy
depends on being fast; and count slippage.

**Why not more forecasts.** The public evidence is one-sided: bots that
trade a public forecast against these markets lose (the post-mortems we
read before: about −14 % a trade over 1,905 trades for one, adverse
selection for another), because the market already prices every public
model within minutes. The eight forecast-model ideas first drafted for this
batch (more models, ensembles, 850 hPa, rain nowcasts, cloud busts, sea
temperature, day-ahead) were dropped for that reason. What is left is what
the market cannot price from a website: air that is still on its way, the
boundary layer at Schiphol before the thermometer moves, the market's own
inconsistencies, who trades and when, and how traders react — or fail to.

### 8.1 How they trade: no race, no free fills

* **The bot is slow by design.** It learns a METAR about 150 s after its
  observation, while the market reprices about 39 s after it, and an order
  takes 350 ms from decision to matching. So every family of L26–L65 waits
  until its signal has held for a minute (`confirm_seconds = 60`; a gap of
  up to 15 s, a book refreshing level by level, keeps the clock). A signal
  that lives less than a minute was never ours to win. The families that
  read a report wait minutes after it (L35: 3–15, L61: 3–20) and check that
  the market had time to react and did not.
* **Slippage.** A taker's limit lies a cent above the ask it saw
  (`taker_tolerance = 0.01`), and its expected profit and break-even are
  counted at that limit, with the taker fee and the slippage allowance. In
  paper (`[paper]`, every book) an order meets the book as it is 350 ms
  later and a taker pays a tick above each level it takes, never above its
  limit (`slippage_ticks = 1`); a resting order fills only from trades
  through its price after the queue ahead.
* **One position a day.** A taker family opens one position a day in a
  market, as the replayed rules do; the makers (L38, L44, L47, L60), whose
  quotes expire before each report, and L39's pair may open again; L59
  makes its one round trip.
* **Stakes by profile.** Aggressive families (cheap YES on a rise) stake
  $15, balanced ones $20, defensive ones (NO of what the weather or the
  market's own structure rules out) $25 — each on a paper book of its own
  with the lab's limits ($25 an order, $120 open, $100 of loss a day).

### 8.2 New inputs

| input | source | read by |
|---|---|---|
| METARs of twelve airports 60–300 km around Schiphol (`[[lab.upstream]]`), three hours back | one AWC request at :10 and :40, away from Schiphol's report windows | L26, L31, L36 |
| KNMI's dew point, humidity, wind, gust, direction, pressure and sunshine at Schiphol (`[lab].knmi_extra_parameters`) | one KNMI request after each new reading (four hours at start); a name KNMI refuses is dropped with one alert | L27, L29, L32, L37, L42, L43 |
| the price ladder: today's YES books, a sample a minute, three hours | the engine, from the live books | L33, L34, L35, L50–L52, L57, L63, L64 |
| yesterday's METARs (the engine keeps 48 hours) | the reports | L30, L41 |
| the book itself (shape, depth, gaps, sums) | the live books | L28, L39, L40, L45, L48, L54, L58–L60 |

### 8.3 The families

Each reads a signal no other family reads. Every rule waits a minute on
its signal and is paper only; the variant is the second rule a family can
run (`variants = ["L26"]`).

**Aggressive ($15): cheap YES on what the market has not seen coming.**

| | family | signal | trade | variant |
|---|---|---|---|---|
| L26 | warm air upstream | 08:00–14:00, METAR wind ≥ 8 kt; an airport within 35° of it, 60–300 km away, ≥ 2.5 °C warmer than Schiphol and at least the rounding edge, its air (1.5 × the surface wind) due before 16:00 | YES next degree 0.05–0.35 | ≥ 3.5 °C |
| L27 | mixing surge | 09:00–13:00, KNMI dew point down ≥ 1.5 °C in an hour while the temperature rose ≥ 1 °C: thermals mix dry warm air down | YES above the favourite 0.04–0.30 | ≥ 2.5 °C |
| L28 | ladder dip | 00:00–16:00, a live bucket ≥ 0.05 under both neighbours (each ≥ 0.12): a day's maximum has one peak | YES of the dip 0.03–0.30 | dip 0.08, sides 0.20 |
| L29 | wind slack | 11:00–15:00, KNMI wind ≤ 3.5 m/s and ≤ 0.6 × an hour before, sunny in two of three readings | YES next degree 0.05–0.35 | ≤ 2.5 m/s, ≤ half |
| L30 | yesterday's anchor | 08:00–11:00, the favourite holds yesterday's high while the forecast lies ≥ 1.5 °C away | YES forecast's bucket 0.05–0.35 | ≥ 2.5 °C |
| L31 | upwind clearing | 09:00–14:00, Schiphol under ≤ 2500 ft; an airport 60–200 km upwind clear twice, its air due within 3 h and by 16:00 | YES next degree 0.05–0.30 | within 2 h |
| L32 | gust onset | 10:00–13:30, gusts ≥ 4 m/s over the mean wind in three readings after ≤ 2 an hour before | YES next degree 0.04–0.30 | ≥ 5 m/s |
| L33 | whale bid | 08:00–18:00, a best YES bid ≥ $250 and ≥ 4 × the largest of the 2–10 minutes before | YES ≤ the wall + 0.03 (0.05–0.45) | $500, 6 × |
| L34 | stale ask | 10:00–17:00, the next degree's ask unchanged for 55 minutes while KNMI rose ≥ 1 °C | YES at ≤ 0.40 | ≥ 1.5 °C |
| L35 | post-high drift | 3–15 minutes after a new high, the next degree up ≤ 0.05 since while KNMI still rises | YES next degree 0.05–0.35 | ≤ 0.02 |
| L61 | SPECI pulse | 09:00–16:00, a special report (off the routine minutes) 3–20 minutes old opening the sky or ending the rain, the market not moved | YES next degree 0.05–0.35 | a worsening → NO next 0.55–0.92 |
| L64 | sellers withdraw | 09:00–18:00 away from reports, the next degree's offered shares down ≥ 60 % in 4–7 minutes, the price not moved | YES next degree 0.05–0.40 | ≥ 75 % |

**Defensive ($25): NO of what the weather or the market's own structure
rules out.**

| | family | signal | trade | variant |
|---|---|---|---|---|
| L36 | front ETA | 08:00–14:00, an airport upwind saw a front pass in three hours (≥ 2 °C down in ≤ 70 min with a veer or rising pressure), due before the median peak | NO next degree 0.60–0.95 (p 0.92) | ≥ 3 °C |
| L37 | pressure fall | 10:00–16:00, KNMI pressure down ≥ 2 hPa in three hours, ≥ 0.8 °C under the edge | NO next degree 0.55–0.92 (p 0.90) | ≥ 3 hPa |
| L38 | calm-tape tails | 09:00–21:00, ≤ 1500 shares in 30 min, two-sided, no trade ≥ 200 in 15 | NO bids two and three above the favourite (YES 0.02–0.12) | three and four |
| L39 | pair lock | from the 75 % peak time to 22:00, everything above the next degree ≤ 0.015 | YES of the high's bucket and the next degree, same shares, ≤ 0.99 a pair at the limits — both or neither | ≤ 0.98 |
| L40 | tail order | after the median peak, the bucket two above priced ≥ 0.02 over the next degree | its NO 0.85–0.99 (p 0.985) | ≥ 0.01 |
| L41 | wet ground | 08:00–14:00, ≥ 4 of yesterday's METARs with rain, the forecast ≥ 1 °C above yesterday's high | NO a degree above the forecast's 0.70–0.97 (p 0.94) | ≥ 8 |
| L42 | saturated air | 10:00–16:00, humidity ≥ 93 % all hour, up ≤ 0.3 °C, ≥ 0.6 °C under the edge | NO next degree 0.60–0.95 (p 0.93) | ≥ 97 % |
| L43 | onshore day | 12:00–17:00, ≥ 80 % of the wind since 09:00 from the sea, ≥ 8 kt, ≥ 0.6 °C under the edge | NO next degree 0.65–0.96 (p 0.94) | ≥ 95 % |
| L44 | unknown longshot buyer | a wallet with no record buys ≥ 20 YES at 0.02–0.10 of a bucket ≥ 2 °C above the high | NO bid a tick under their price, 30 min | ≥ 50 shares |
| L45 | complement gap | 07:00–22:00, a bucket's NO at its limit, fee and slippage ≥ 0.02 under the YES bids of the other buckets that can still win, added up | that NO 0.50–0.99 (p = the sum) | ≥ 0.04 |
| L46 | evening discount | 21:00–23:50, KNMI ≥ 3 °C under the edge and not rising, the high's bucket offered 0.95–0.985 | its YES (p 0.997) | from 22:00, ≤ 0.98 |
| L47 | evening lottery | 17:00–23:00, ≥ 5 small YES buys (≤ 20 shares at 0.02–0.15) in 30 min on the next two degrees, KNMI ≥ 1 °C under the edge | NO bid on that bucket | ≥ 8 buys |
| L48 | self-contradiction | 07:00–11:30, the ladder's own mean ≥ 2 °C above the high while the high's bucket is ≥ 0.05 | its NO 0.80–0.95 (p 0.95) | ≥ 3 °C |
| L62 | haze lid | 11:00–15:00, visibility ≤ 7 km in dry air (or HZ/FU) twice, ≥ 1 °C under the high | NO next degree 0.60–0.95 (p 0.93) | ≤ 5 km |

**Balanced ($20): both sides of the market's mistakes.**

| | family | signal | trade | variant |
|---|---|---|---|---|
| L49 | boundary straddle | 08:00–12:00, the forecast within 0.3 °C of an x.5 edge while one side is ≤ 0.55 × the other | YES of the cheap side 0.10–0.45 | 0.2 °C, 0.45 × |
| L50 | move without news | 07:00–21:00, a bucket moved ≥ 0.12 in 8–15 min with no report, no forecast and KNMI flat | against the move 0.15–0.80 | ≥ 0.18 |
| L51 | momentum divergence | 10:00–15:00, the ladder's mean down ≥ 0.6 °C in 30 min while KNMI rose ≥ 0.5 °C (or the reverse) | YES a degree above the mean (or NO of the mean's bucket) | 0.9 / 0.8 |
| L52 | run-up fade | ≤ 12 min before a report, the next degree up ≥ 0.05 in 10 min while KNMI ahead of the METAR is ≥ 0.4 °C under the edge | its NO 0.40–0.92 | ≥ 0.08 |
| L53 | projected curve | 10:00–13:00, a sine from the night's lowest METAR through KNMI to the median peak time ≥ 0.8 °C off the ladder's mean | YES of the projection's bucket 0.10–0.45 | ≥ 1.2 °C |
| L54 | overround squeeze | 06:00–22:00, the buckets that can still win add up to ≥ 1.08 | NO of the bucket carrying most of the excess 0.30–0.80 | ≥ 1.12 |
| L55 | edge hover | near the median peak, KNMI's last three means within 0.2 °C of the edge while one side is ≤ 0.35 and the other ≥ 0.55 | YES of the cheap side 0.10–0.35 | 0.1 °C |
| L56 | stations disagree | 10:00–16:00, Schiphol and the KNMI neighbours ≥ 3 °C apart, the favourite ≥ 0.55 | its NO 0.25–0.45 | ≥ 4 °C |
| L57 | laggard bucket | 10:00–18:00, the next degree up ≥ 0.06 in 4–7 min while the high's bucket has not fallen, KNMI near the edge | NO of the high's bucket 0.10–0.75 | ≥ 0.10 |
| L58 | round-number premium | 07:00–20:00, a multiple-of-5 bucket ≥ 0.08 above what a normal fitted to the other buckets gives it | its NO 0.40–0.85 | ≥ 0.12 |
| L59 | imbalance round trip | 09:00–18:00 away from reports, the favourite's spread ≤ 0.03 with ≥ 3 × the shares bid as offered | YES, sold at +0.03, −0.05, after 45 min or before a report; once a day | ≥ 5 × |
| L60 | gap filler | 08:00–20:00, a thin best ask (≤ 50 shares) with the next ≥ 0.10 above, on the favourite or a neighbour | offer YES halfway into the gap (a NO bid) until 10 min before the report | ≥ 0.15 |
| L63 | hidden seller | 08:00–20:00, ≥ 3 buys at one price in 30 min taking ≥ 3 × the size the ask showed, the ask still there | its NO 0.20–0.80 | ≥ 5 × |
| L65 | midnight high | 08:00–11:00, the high first reached before 02:00, KNMI ≥ 1 °C under it, the forecast's rest of the day ≤ the high + 0.4 °C | YES of the high's bucket 0.10–0.60 | forecast ≥ 0.5 °C under |

### 8.4 The ten "new ways" and the five extras

Asked for ten ways not yet considered, from the market's mechanics rather
than the weather: **L46** (holders sell a near-certain winner cheap in the
evening to free their money), **L34** (orders nobody updates while the
weather moves on), **L47** (evening lottery buyers), **L58** (round-number
bias), **L59** (book imbalance, the one round trip), **L35** (under-reaction
after a new high), **L48** (the market's own distribution against its own
price), **L60** (providing liquidity where a thin ladder has a hole),
**L45** (the negRisk identity NO = the others' YES) and **L36** (fronts
seen upstream before they arrive). The five extras: **L61** (special
reports), **L62** (haze), **L63** (iceberg sellers), **L64** (withdrawn
asks) and **L65** (the midnight high of a cold-air day).

### 8.5 What would refute them

Forty rules on the same days are forty chances to look good by luck, and
none is replayed yet (`research market` replays L1–L25 only; the journal
keeps every input these read, so a replay can follow). A family counts
only on days after its rule was fixed, and only when its paper record over
several weeks shows a positive mean P&L a trade with its spread, not its
best day. Specific risks: the KNMI parameter names `td`, `rh`, `ff`, `fx`,
`dd`, `pp` and `ss` are the collection's as we know them — one KNMI does
not know is alerted and its families stay idle; upstream air does not
always travel at 1.5 × the surface wind; a bid wall can be withdrawn
(spoofing), and an imbalance can flip. The pair lock (L39) can be left with
one leg when the other's book empties in the 350 ms; that loss is counted.
