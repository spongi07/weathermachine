# Live review: the paper week of 4–8 October 2026

The paper report of 4–8 October 2026 (`report paper`, five settled days: 244
routine evaluations, 381 proposals of which 102 approved, 102 orders, 75
fills) read strategy by strategy against the replays
([strategy review](strategy-review.md), [lab's first run](strategy-lab.md#6-the-first-run-2-october-2026)),
with the operator's question: which strategies go, which stay, and what to
build next. Nothing here trades money.

Until 8 October 11:20 (local) I was on, F's books were limited to a 0.05
spread and all 25 lab families ran; the changes of
[8 October](strategy-review.md#5-applied-on-8-october-2026) took effect
from then.

## 1. The week in numbers

| Day | High | Main book | Lab books |
|---|---|---:|---:|
| 4 Oct | 20 °C (15:25) | +$4.99 (G +1.55, I +3.44) | −$51.98 |
| 5 Oct | 20 °C (12:55) | −$0.03 (F +7.38, G +0.60, I −8.01) | +$39.13 |
| 6 Oct | 21 °C (14:25) | −$1.66 (F +5.72, G +2.56, I −9.94) | −$1.37 |
| 7 Oct | 22 °C (13:55) | +$2.20 (G +2.56, I −0.36) | −$5.52 |
| 8 Oct | 15 °C (12:25) | +$3.70 (F +6.38, G +2.56, I −5.24) | −$0.70 |
| **total** | | **+$9.19**; without I **+$29.30** | **−$20.46** |

## 2. Verdicts

| | live, 4–8 October | replay | verdict |
|---|---|---|---|
| **F** peak slot | 3 of 3 won, +$19.47. Missed 4 Oct: the high's bucket was offered above 0.95 from the report that made it the high. Missed 7 Oct: books wider than 0.05. L3 — F's rule on a book allowed 0.10 — won 4 of 4, +$27.04. | +$11.13 over 80 trades | **keep**; the 0.10 limit (since 8 Oct) is what L3 shows |
| **G** tail seller | 5 of 5 won, +$9.83, all as the maker (no fee). The sold buckets lay 1–2 °C above the final high. | 125 of 127 | **keep** as it is (§3.2) |
| **K** KNMI nowcast | no trade; the log could not show its decisive moment | 4 of 4 | **keep**; now recorded at that moment and polled faster (§4) |
| **I** middle fade | 4 of 9 won, −$20.11: each of its five losses was the NO of the bucket that won | thin edge | **off** since 8 Oct — confirmed |
| **U** unwind | no exit | — | stays on; with F–K exempt it manages nothing now |
| L3 F's escape | 4 of 4, +$27.04 (the escape never fired) | +$10.91 vs F's +$11.13 | keep: the only live test of the escape |
| L4 cooling lock | 1 of 1, +$3.22 | later days 8 of 8 | keep: candidate |
| L7 KNMI slope | 1 won, 1 lost, +$4.32 | later days 7 trades, −$3.52 | keep collecting |
| L14 morning departure | 2 of 5, −$19.44; its largest stake (57 shares) lost, its wins were 16 and 5 shares | main rule positive in both halves | keep: five trades cannot refute it; the next replay's new days decide |
| L20 longshot fade | 1 of 1, +$2.20 | too rare | keep collecting |
| **L1** shielded maker | 2 won, 1 lost, −$7.00: its NO bid on 20 °C at 0.60 (placed 14:44 on 4 Oct) filled, and 20 °C became the high at 15:25 | later days 41 trades, −$25.01; the shield brings the next-degree maker to about zero | **off** since 9 Oct |
| L10 TREND cap, L11 fog fade | 0 of 1 each (−$9.47, −$2.06) | too rare | keep collecting: one trade each |
| L17, L19, L21 | −$3.88; −$17.77; +$2.40 over 3 trades | refuted | stay off (since 8 Oct) |
| L2, L5, L6, L8, L13, L15, L16, L18, L24, L25 | no trade | too rare | keep collecting |

**Eliminated:** I (8 October, confirmed by this week), L1 (9 October), and
the seven lab families switched off on 8 October. **Kept:** F, G, K, U and
seventeen lab families, paper only.

## 3. What the week shows

### 3.1 The market prices the day better than the model

Log loss on the winning bucket over 135 routine evaluations: model 0.584,
market 0.281. On 8 October the model's was 1.097 against 0.383: from
midnight to 09:00 it gave the winning 15 °C 8–13 %, the market 47–64 %. The
day-1 forecast (15.5 °C) lay 1.5 °C above the high so far (14 °C), which
puts the day in the model's open-ended forecast-headroom cell `above2`
(≥ 1.5 °C, `headroom_bucket`), whose history is dominated by mornings with
far more rise to come; the model expected a large rise that the forecast
did not. No strategy that trades now uses the model's probability (G's
veto is off, F and K trade on prices and KNMI), so this costs nothing
today; splitting the cell (§5.4) is for the next training. An independent
study finds the same order on Kalshi: an hour into trading, the
market-implied forecast of the next day's high beats the best public
forecast, the National Blend of Models, by about 10 % in root-mean-square
error (in six of seven US cities), and the forecast moves toward the market
far more than the market toward it
([Crosier 2026, arXiv:2609.23969](https://arxiv.org/abs/2609.23969)).

### 3.2 G sells close to the forecast — and that is its edge

G's five buckets started 0.3–1.4 °C above the day-1 forecast's maximum
(21 °C against 20.2, 22 against 20.1, 22 against 20.6, 23 against 21.1,
17 against 15.5), and all five won. The forecast guard of the review
(§3.2 there: the bucket at least 2 °C above the forecast) would have
blocked every one of them. G sells what the market itself prices at 5–8¢;
its edge is the takers who overpay for longshots, not distance from the
forecast. The guard stays a replay variant, not a live rule.

### 3.3 I was not only losing

On each of the five days I already held the NO of the high's bucket — the
token K buys when KNMI announces a new high — so K could not have bought
it even with its trigger met (one position per token, the Duplicate
check). Switching I off cleared K's way as well.

### 3.4 Fixes of the week, seen in the record

* Spread: F was refused thousands of times on 7 October between 15:01 and
  15:45 (165 records after the once-a-minute deduplication) and L7 74 times
  in one minute on 4 October; on 8 October, after the engine's spread
  pre-check, not once.
* Tick: G's 0.975 and 0.982 bids were refused on 5 October ("not on tick
  0.01"); from 6 October its sub-cent bids (0.937, 0.932, 0.989) went
  through.
* Restarts (4, 5 and 8 October) restored every position; no warning.

### 3.5 Speed decides K

The METAR reaches the bot a median 150 s after its observation time (90th
percentile 210 s; TGFTP first for two reports in three). The first stale
trade after a new high comes a median 39 s after the observation
([edge research §9.2](edge-research.md#92-what-our-own-data-say)): the
market knows the report well before the bot. K's window is the time
between KNMI's last reading before a report (the interval ending :20,
known about :24) and that repricing (about :25¾).

## 4. Applied on 9 October 2026

1. **L1 off** (`disabled` in `[strategies.lab]`).
2. **K's decisive moment is recorded.** When KNMI's last reading before a
   routine report arrives (the intervals ending :20 and :50 at Schiphol),
   the engine records an evaluation of the strategies that read KNMI (K and
   the lab's KNMI rules), marked `knmi_checkpoint` with the reading and the
   report it precedes. `report paper` shows these per strategy apart from
   the routine evaluations ("At KNMI's last reading before each report"),
   the strategy logs and the strategies' dashboard pages mark them, and the
   model is still scored on the routine evaluations only. Until now every K
   line read "KNMI reading not newer than the last METAR"; in the demo the
   checkpoint line shows K's own condition instead ("KNMI mean 11.7 °C <
   11.8 °C"). Since 10 October a reading counts only if it arrives at most
   ten minutes after the report it precedes: each restart on the evening of
   9 October had read the last three hours at once and recorded each
   reading before a long-known report as a checkpoint (five at 21:55 UTC),
   which `report paper` now leaves out.
3. **KNMI is polled faster when a reading is due.** Every reading of
   8 October was found by the poll 4 minutes after its interval, none by
   the one at 3½ minutes; the loop now asks from 3 minutes after each
   interval's end every 10 s for two and a half minutes — to 5½ minutes,
   about when the market prices the report the reading precedes — and every
   minute after that, so a reading reaches K about 10 s sooner on average, for
   50–60 requests an hour instead of 48; readings that come late or not at
   all stay inside the daily budget.

## 5. What to build next

Ranked by expected value; each starts as a measurement, none trades money.

1. **A faster METAR: KNMI's own.** KNMI produces Schiphol's METAR and
   publishes it as open data ([dataset `metar` 1.0](https://dataplatform.knmi.nl/dataset/metar-1-0),
   CC BY 4.0, updated continually); KNMI's
   [Notification Service](https://developer.dataplatform.knmi.nl/notification-service)
   announces each new file over MQTT, which its
   [FAQ](https://developer.dataplatform.knmi.nl/faq) names the fastest way
   to get new files (excessive polling for new files counts as abuse). If those files arrive well before AWC and TGFTP (150 s), every
   strategy gains, and D (decided outcomes, off because the bot learns the
   report after the market has repriced) may come back. Next: measure the
   delay with an Open Data API key, then add it as a third METAR source —
   the collector already takes the first of several sources and the report
   shows which delivered first.
2. **The ten-minute readings by notification.** KNMI's recommended way to
   follow its datasets: its
   [fair-use policy](https://developer.dataplatform.knmi.nl/fair-use) calls
   a notification faster and cheaper than polling. The loop's polling stays
   far inside the EDR API's quota of 1,000 requests an hour a key
   ([EDR API](https://developer.dataplatform.knmi.nl/edr-api)) — 50–60, and
   at 10 s only while a reading is due — but a notification would save the
   last ≤ 10 s and most of the requests. The dataset's files are NetCDF,
   one per ten minutes for all stations, which the bot does not read yet.
3. **F and G in more cities.** Their edges — the favourite–longshot bias
   and the season's peak slot — are not Amsterdam's own. Polymarket lists
   daily-high markets for other cities too (New York, Chicago, Mexico City,
   Shanghai, Guangzhou, Hong Kong among them). Next: replay F and G at
   traded prices per city before any paper trading; K needs a fast local
   source and stays Schiphol's.
4. **Split the model's forecast-headroom cell** (§3.1): `above2` from 1.5 °C
   upward mixes a 1.5 °C day with an 8 °C morning. At the next training,
   offered as a candidate structure and adopted only if the walk-forward
   test prefers it.
5. **Schiphol every 12 seconds — for research, not for K.** KNMI also
   publishes
   [actual synoptic observations of Schiphol Airport per 12 seconds](https://data.overheid.nl/en/dataset/56855-meteo-data---actual-synoptic-observations-schiphol-airport-per-12-seconds)
   (CC BY 4.0), but not live: the files are zipped once a day, kept on S3
   for 100 days and not served by the Data Platform (access through
   opendata@knmi.nl), so they cannot speed K up. Afterwards they show, at
   12 s, how the METAR's temperature relates to the ten-minute mean and
   maximum K and the lab's KNMI rules trigger on — a check on their margins
   (0.3–0.8 °C over the bucket's edge). Next: ask KNMI for access.
6. **Count maker rebates.** Weather takers pay 0.05 × p × (1 − p) a share
   and makers receive 25 % of those fees as rebates
   ([Polymarket](https://docs.polymarket.com/market-makers/maker-rebates);
   [River Markets](https://www.rivermarkets.com/insights/polymarket-fees.html)).
   G's fills earn a small rebate the paper book does not show — a few
   cents a fill at 0.92, so it changes no decision.
7. **Not worth a rule: ladder arbitrage.** Buying every bucket when the
   asks sum to under $1 after fees is riskless in principle, but such gaps
   are rare, brief and thin: in Polymarket's NBA markets 75 million book
   snapshots over 173 games held 7 executable single-market gaps, open a
   median 3.6 s, and three in four of the combinatorial ones only about
   15 shares deep
   ([Cheng et al. 2026, arXiv:2605.00864](https://arxiv.org/abs/2605.00864));
   with 11 buckets and fees of about 3¢ for the whole ladder, a gap would
   have to be wide.

## 6. Next review

The next replay (`research market --from 2026-10-02`, after about 20 new
settled days) judges L4 and L14 on days no rule was chosen on; the paper
record checks F's 0.10 limit (from 8 October), K's checkpoints and the
faster KNMI cadence (from 9 October) against it.
