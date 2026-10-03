# Strategy review (3 October 2026)

A review of every strategy against three kinds of evidence: the replays at
traded prices (`research market`, 123 settled days to 1 October 2026,
[edge research §9.5](edge-research.md#95-the-replay-of-2-october-2026) and
the [lab's first run](strategy-lab.md#6-the-first-run-2-october-2026)), the
live paper record (26 September – 3 October, `report paper`), and the code.
Nothing here trades money; every recommendation is the operator's call.

## 1. Verdicts

| | strategy | live | evidence | verdict |
|---|---|---|---|---|
| K | KNMI nowcast | on | replay 4 of 4 won (+$81.10), out of sample too; no live trade yet | **keep**: the one clear edge, being minutes ahead of the METAR. Its decisive moment is not in the log (§3.4) |
| G | tail seller | on | replay 125 of 127 won (+$76.64), 53 of 53 out of sample; live +$1.20 settled, +$2.56 pending | **keep**, but test a forecast guard (§3.2): one loss costs about $29, some 27 average wins |
| F | peak slot | on | replay +$11.13 over 80 trades (−$60, then +$71); live 2 of 2 won, +$8.28 | **keep on paper**; let it run as replayed (§3.1) |
| I | middle fade | on | replay +$58.79 over 90 trades, the configured rule +$10.03 on the later days (interval spans zero); live −$5.62 | **watch**: a thin edge from the market's mid-ladder overpricing (§3.3); first to go if it does not hold |
| U | unwind | on | exits on the model's probability; not part of any replay | **narrow** to what was tested (§3.1) |
| A, B | model edge | off | the model predicts worse than the market (log loss +0.06 to +0.10 a decision) | stay off |
| D | decided outcomes | off | dead buckets reprice a median 39 s after the observation, before the bot knows | stay off |
| E | book-confirmed high | off | −$7.72 over 79 replayed trades | stay off |
| H, J | next degree, morning maker | off | replay −$77.18 and −$197.52; resting into informed flow; J lost $9.88 live on 2 October before it was switched off | stay off |
| L4, L14 | cooling lock, morning departure | paper | L4 8 of 8 on the later days; L14's main rule earned in both halves | **the lab's candidates**: judge on new days |
| L1, L7, L18, L24 | KNMI shield, slope, burst, upwind | paper | positive overall, small or mixed on the later days | keep collecting |
| L9, L12, L17, L19, L21, L23 | showers, clear sky, yesterday's error, skilled takers, jump fade, recovery | paper | refuted by the replay (L19 −$613.09 on the later days) | **switch off** to cut noise (§3.5) |
| L22 | overnight tail maker | paper | 178 evaluations on 3 October, all "no NO bid to join" | **switch off**: the far tails sit at 0.001, so its 1–5¢ band never exists |
| L2, L3, L5, L6, L8, L10, L11, L13, L15, L16, L20, L25 | the rest | paper | traded too rarely to say anything | keep collecting |

## 2. What the evidence says

* **The market prices public weather well.** Over 120 days the market's
  prices predicted the winning bucket better than the model in every pool
  `research market` scores, and pooling the two did not beat the market
  alone. A strategy that needs the model to be right (A, B, H) has no
  edge; the model's conditions in G and I change little.
* **Two kinds of edge survive.** Speed: KNMI's ten-minute reading arrives
  minutes before the METAR (K; in the lab L1's shield, L7, L18). Calibration:
  far tails are overpriced (0.1 % won at 0.4 %), the middle of the ladder
  too (45.4 % at 48.5 %), favourites slightly underpriced (96.8 % at
  94.9 %). After the 5 % taker fee and the spread only a fraction of a cent
  a share is left, so these earn slowly and lose in lumps.
* **Late "lock" rules find no price.** When the evidence is in (F late in
  its slot, E, L4, L15), the high's bucket already trades at 0.98–0.999. On
  3 October 20 °C went from 0.89 at 14:27 to 0.989 at 14:57, as F's slot
  opened.
* **Eight paper days decide nothing.** Single trades swing a strategy by
  $5–20; the replay's halves are the evidence, the paper record is the
  check that live fills match them.

## 3. Findings

### 3.1 F runs with exits it was never tested with

F buys the high's bucket on price and claims no model edge; its replay
holds every trade to settlement. Live, the unwind engine U may sell an F
position at the best bid once the model's probability for it drops below
0.5. F is the only main-book strategy U still manages (A–E are off, G–K
are exempt), so U's exits apply to an untested rule, on the signal the
replay found weaker than the market's. L3, which exits F on KNMI's far
better signal, did not improve it either (+$10.91 against +$11.13).
Recommendation: `exempt_strategies` in `[strategies.unwind]` gains
`"F_peak_slot"`, so live F is the replayed F.

### 3.2 G measures "far" from the current high

G sells every bucket at least 3 °C above the day's high so far. In the
morning that is not a tail: on 3 October at 11:10 the high was 16 °C and G
sold YES on 19 °C at 0.08, the bucket of the day-1 forecast (19.1 °C). It
won only because the high climbed past 19 to 20 °C. The replay supports G
as configured, but a fill at the 8¢ cap needs fewer than one loss in
twelve, and in the morning, with most of the day's rise still to come, a
bucket three degrees above the high is the likeliest to win. Recommendation:
replay a variant that also requires the bucket to lie at least 2 °C above
the forecast's maximum, or starts G later in the day, before trusting the
morning fills with money.

### 3.3 I's edge is the market's, not the model's

I buys NO where the YES trades at 0.30–0.70 and the model is 5 points
lower. Its probability is the midpoint minus the measured 3-point
overpricing, so its expected profit is about that overpricing minus half
the spread, the fee (1.25 points at 0.50) and slippage: close to zero in
anything but a tight book. The replay without the model condition earned
more (+$62.35). Keep it on paper and drop it if the next replay's later
days are negative again.

### 3.4 K's decisive moment is not in the log

The routine evaluation is recorded when a METAR arrives. At that moment
K's KNMI reading is always older than the report, so every K line says
"KNMI reading not newer than the last METAR" and compares the old reading
with the new high. K decides on the reading that arrives a few minutes
before the report. On 3 October the reading of 12:10–12:20 UTC reached
K's mean threshold for a high of 19 °C (19.8 °C) just before the 14:25
local report raised the high to 20 °C. Half an hour earlier the NO of 19 °C already
cost 0.85, above K's 0.75 cap, and the log cannot show its price at that
moment. Recommendation: also record an evaluation of the KNMI-reading
strategies (K, L1, L2, L4–L7, L18, L23–L25) when the last reading before a
report arrives. That doubles the evaluation records and makes K and the
KNMI lab rules reviewable.

### 3.5 Lab notes

* **L19 holds both sides of 19 °C** (166 YES at 0.12 and 22 NO at 0.90):
  it follows each skilled taker per bucket and side, and two of them traded
  against each other. Its replay was already negative; on 3 October it lost
  about $15.6 while the other lab books together made $1.6.
* **L21 was rejected 5,265 times in five minutes** on 3 October (spread
  0.12–0.13 against the lab books' limit of 0.10): it proposed again on
  every book update. Identical rejections were already stored once a
  minute; since this review the lab's taker and maker paths name a book
  their risk check would refuse as a blocker ("spread 0.13 > 0.10 (the lab
  book's limit)", or a one-sided book), so the rule waits in its evaluation
  instead.
* **L22 cannot trade here**: overnight the far tails are offered at 0.001
  and their NO books are one-sided, so there is never a bid to join inside
  its 1–5¢ band.
* Switching off the refuted families and L22 (`disabled = ["L9", "L12",
  "L17", "L19", "L21", "L22", "L23"]` in `[strategies.lab]`) keeps their
  replay code and leaves the lab's other books untouched.

## 4. The live record so far

| strategy | trades | result | note |
|---|---:|---:|---|
| F | 2 | +$8.28 | 30 Sep 100 at 0.95; 2 Oct 59.84 of 100 at 0.94 |
| G | 3 | +$1.20 settled, +$2.56 pending | 2 Oct NO 22 °C and 19 °C; 3 Oct NO 19 °C at 0.92 |
| I | 2 | −$5.62 | 2 Oct: NO 21 °C won, NO 20 °C lost |
| J | 1 | −$9.88 | 2 Oct morning, before it was switched off |
| lab, 3 Oct | 6 | about −$14.0 pending | L1 +$2.64, L21 +$2.10, L14 −$3.10, L19 −$15.62 |

Next: after about 20 new settled days, `research market --from 2026-10-02`
replays only days no rule was chosen on; that, not this table, decides L4,
L14 and the verdicts above.
