# Live review: the paper record to 14 October 2026, and the cut

The strategy pages of 14 October 2026 (the dashboard's logs, history
8–14 October) read with the reviews of
[9 October](live-review-2026-10-09.md) and
[12 October](live-review-2026-10-12.md) and the replay
([edge research §9.5](edge-research.md#95-the-replay-of-2-october-2026),
[lab's first run](strategy-lab.md#6-the-first-run-2-october-2026)), on the
operator's two questions: which strategies stay, and which could go live
beside G. Only G's live twin trades money.

## 1. The days since the last review

| Day | High | Main book | Lab books | G's live twin |
|---|---|---:|---:|---:|
| 12 Oct | 16 °C | +$7.01 (F +6.41, G +0.60) | +$6.41 (L3) | — |
| 13 Oct | 20 °C | +$4.80 (G: NO 18 °C and NO 21 °C) | +$1.89 (L20) | +$1.50 (2 fills) |
| 14 Oct | 18 °C | −$76.98 (F −78.50, G +1.52) | −$78.50 (L3) | +$0.87 (2 fills) |

14 October is decided (the high stayed 18 °C into the evening) and settles
at midnight UTC.

## 2. Per strategy, since each started

| | paper | replay | verdict |
|---|---|---|---|
| **G** tail seller | 15 of 15, +$24.09 | 125 of 127, +$76.64; later days 53 of 53, +$55.09 | **on** |
| **G_live** (real money) | 4 of 4, +$2.37 | as G | **on**, limits unchanged |
| **F** peak slot | 8 of 9, −$31.10 | 75 of 80, +$11.13 | **off** |
| **K** KNMI nowcast | no trade | 4 of 4, +$81.10 | **on**; buys what is offered up to its cap |
| **U** unwind | no exit | — | **off** |
| L3 F's escape | 7 of 8, −$31.82 | as F (+$10.91) | **off** |
| L4 cooling lock | 2 of 2, +$5.74 | later days 8 of 8, +$29.05 | on |
| L7 KNMI slope | 1 of 2, +$4.32 | later days 3 of 7, −$3.52 | on, on probation |
| L18 burst + KNMI | no trade | later days 3 of 3, +$22.84 | on |
| L20 longshot fade | 3 of 3, +$5.33 | too rare | on |
| L24 upwind | 1 of 1, +$8.80 | later days 3 of 3, +$19.86 | on |
| L25 radiation collapse | 1 of 1, +$3.48 | too rare | on |

The rule of 12 October stands: a strategy stays on when it earned live, or
has not traded yet but earned on the replay's later days; it goes off when
it lost live. F and L3 lost; the others stay.

## 3. What the days show

* **F has no edge, and its loss is large.** On 14 October at 15:04 F
  bought 85 of 100 shares of 17 °C YES at 0.92 (only 85 were offered up to
  0.95); the 15:25 report raised the high to 18 °C. The model had given the
  bucket 0.72 at 14:58 and the day-1 forecast's maximum was 17.6 °C, but
  the model gave 0.72 on 12 October too, when F won, so neither is a filter
  one day can justify. F wins about $5.93 a trade (8 wins, $47.40) and lost
  $78.50 once: about 13 wins. At 0.93 it must win 93.8 % of its trades; the
  replay and the paper record together won 83 of 89 (93.3 %), for −$19.97.
  No variant held up out of sample (the maker variant chosen on June–July
  lost $18.76 in August–September).
* **L3's escape did not fire when it mattered.** It sells F's YES once
  KNMI's mean is 0.3 °C above the bucket's rounding edge; the reading of
  13:10–13:20 UTC before the 15:25 report was 17.6 °C, under 17.8 °C, and
  the METAR rounded up to 18 °C. In eight trades the escape never fired,
  and the replay found it no help (the bid is gone by the time KNMI knows).
* **K missed the same new high by 0.2 °C** (17.6 °C against its 17.8 °C).
  Its trigger held once in 14 days (10 October, when the NO was already at
  0.95): about the replay's rate of one trade a month. One condition was
  stricter live than replayed: K bought only when the whole $25 was offered
  at the best ask ("only 10 NO offered at 0.12 (need 208)" at 15:28), and
  right before a report the NO book of the high's bucket is thin; the
  replay, at traded prices, never saw depth. It had not yet cost a trade
  (the KNMI condition never held with a price in the band).
* **G sells close to the high.** Of its eight fills 8–14 October, three
  were the bucket one degree above the final high (19 °C on 9 and
  14 October, 21 °C on 13 October) and twice the high passed through the
  sold bucket on its way up (16 °C on 9 October, 18 °C on 13 October). All
  won, as in the replay (two losses in 127), but one loss costs about
  $29.50, some 18 of these wins.
* **Live G fills more readily than paper G.** On 14 October the live twin
  bought NO 19 °C at 0.924 and NO 20 °C at 0.989 within about a minute
  each, while paper G's eleven bids on NO 20 °C filled none: the simulator
  fills a resting bid only after a later trade through its price. Paper is
  the cautious side here, not the optimistic one.
* **U had nothing to manage.** Every strategy that runs holds to
  settlement (G and K are exempt), and the lab's and the live twin's books
  are their own: the engine passes the unwind engine the main book only.

## 4. Applied on 14 October 2026

| change | where | why |
|---|---|---|
| F off | `enabled = false` in `[strategies.peak_slot]` | §3 |
| L3 off | `"L3"` in `[strategies.lab].disabled` | it enters as F does; its escape did not fire |
| the unwind engine off | `enabled = false` in `[strategies.unwind]` | nothing to manage; it comes back with any of A–E |
| K walks the NO asks | `wm-strategy::knmi_nowcast` | while each price stays in [0.02, 0.75] with the 0.05 edge it buys what $25 buys, or everything offered up to where the walk stops (at least the market's minimum); limit × shares ≤ $25 |
| the dashboard lists only strategies switched on | `wm-app::strategies::catalog` | the unwind engine was listed even when off |
| G's summary | the strategy page | it named "two or more degrees" and the model's veto; the setting is three degrees and the veto is off |

The replay code of F and L3 stays: `research market` replays both on
every run, and `enabled = true` (or removing `"L3"` from `disabled`)
brings one back. F's open position of 14 October settles as usual.

## 5. Beside G, live?

The live venue places one kind of order: a post-only good-till-date BUY
([operations §42](../blueprint/09-operations.md#42-live-trading)). No
taker orders, no sells.

| | evidence | execution | verdict |
|---|---|---|---|
| K | the clearest edge in the replay (4 of 4; the KNMI table's 194 of 199), but no paper trade, and its one trigger in 14 days found the market already priced | a taker buy: the venue cannot place it | first candidate once it has paper fills at a price in its band, and the venue takes taker orders |
| L20 | 3 of 3 on paper, too rare in the replay | a resting NO bid, as G | not yet: it sells the same buckets as G (both sold 18 °C on 13 October), so it adds to the same outcome rather than a new edge |
| L4, L24 | later days 8 of 8 and 3 of 3; 2 and 1 paper trades | taker buys | after the next replay, if its new days agree |
| L7, L18, L25 | too few trades either way | taker buys | no |

So none beside G yet. The next step for money is G itself: §42 raises its
limits in stages only once its live fills keep matching paper G's, and
four fills are too few to say so.

## 6. Next review

About 22 October, with 20 settled days from 2 October,
`research market --from 2026-10-02` replays the new days for all 25
families: L4, L7, L18 and L24 are judged there (L7 goes off if neither
record earns), and K's paper record shows whether walking the asks lets it
buy when its trigger holds.
