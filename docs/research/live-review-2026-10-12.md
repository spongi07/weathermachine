# Live review: 9–12 October 2026 and the cut

The paper record of 9–12 October 2026 (`report paper` for 10–12 October
and the dashboard of 10 October) read strategy by strategy against the
[week of 4–8 October](live-review-2026-10-09.md) and the replay
([lab's first run](strategy-lab.md#6-the-first-run-2-october-2026),
[strategy review](strategy-review.md)), on the operator's request: switch
off what does not work. Nothing here trades money.

## 1. The days

| Day | High | Main book | Lab books |
|---|---|---:|---:|
| 9 Oct | 18 °C | +$9.90 (F +6.95, G +1.80 and +1.15) | −$6.06 (L24 +8.80, L3 +6.95, L10 +0.59, L14 −2.33, L11 −20.07) |
| 10 Oct | 14 °C | +$6.28 (F) | +$10.54 (L3 +6.28, L14 +4.26) |
| 11 Oct | 14 °C | +$0.63 (G) | +$0.88 (L25 +3.48, L4 +2.52, L20 +1.24, L14 −6.37) |
| 12 Oct | 16 °C (provisional) | F holds 100 × 16 °C YES at 0.93, G 30 × 18 °C NO at 0.98 | L3 as F |
| **9–11 Oct** | | **+$16.81** | **+$5.36** |

Since 4 October the main book made +$26.00 (+$46.11 without I, off since
8 October) and the lab books lost $15.10.

## 2. Per strategy

| | live, 4–11 October | replay, later days | verdict |
|---|---|---|---|
| **F** peak slot | 5 of 5, +$32.70 (12 Oct: the winner) | +$11.13 over 80 trades (all days) | on |
| **G** tail seller | 8 of 8, +$13.41 (12 Oct: the winning side) | 125 of 127 | on |
| **K** KNMI nowcast | no trade | 4 of 4, +$56.41 | on: about one trade a month (4 in 123 days) |
| **U** unwind | no exit | — | on: the exit engine, idle while F–K hold to settlement |
| L3 F's escape | 6 of 6, +$40.27 (12 Oct as F) | as F | on |
| L4 cooling lock | 2 of 2, +$5.74 | 8 of 8, +$29.05 | on |
| L7 KNMI slope | 1 of 2, +$4.32 | 3 of 7, −$3.52 | on, on probation |
| L18 burst + KNMI | no trade | 3 of 3, +$22.84 | on |
| L20 longshot fade | 2 of 2, +$3.44 | too rare | on |
| L24 upwind | 1 of 1, +$8.80 | 3 of 3, +$19.86 | on |
| L25 radiation collapse | 1 of 1, +$3.48 | too rare | on |
| L10 TREND cap | 1 of 2, −$8.88 | too rare | **off** |
| L11 fog fade | 0 of 2, −$22.13 | too rare | **off** |
| L14 morning departure | 3 of 10, −$23.88 | main rule 15 of 51, +$199.41 (95 % CI −6.44 … +16.70 a trade) | **off** |
| L2, L5, L6, L8, L13, L15, L16 | no trade in nine days | too rare | **off** |

**The rule.** A strategy stays on when it earned live, or has not traded
yet but earned on the replay's later days, on which no rule was chosen. It
goes off when it lost live, or made no trade in nine days while the replay
saw it too rarely to show an edge. Switching off stops only the live paper
book: `research market` replays all 25 families on every run, so the next
replay still judges the families switched off, and one that earns there
comes back with one line in `[strategies.lab].disabled`.

Off since 12 October: L2, L5, L6, L8, L10, L11, L13, L14, L15 and L16.
Seven lab families run: L3, L4, L7, L18, L20, L24 and L25. Off before:
L9, L12, L17, L19, L21, L22, L23 (8 October), L1 (9 October); in the main
book A–E and H–J, and I since 8 October.

## 3. What the days show

* **L14 loses on its large fills.** Its three wins were 16, 5 and 6
  shares, its largest loss 57. On 11 October it bought 15 °C at 0.24 and
  16 °C at 0.08 while the high stayed at 14 °C, as the forecast had said
  (13.9 °C). A cheap YES is on offer in size when it is wrong; the replay at
  traded prices cannot see how much was offered.
* **F missed 11 October** as it missed 4 October: the 14 °C YES crossed
  0.90 at 14:27, before the slot (14:55–17:26), and inside the slot was
  never offered at 0.90–0.95.
* **K's trigger held once** (10 October, 12:54): KNMI's mean 15.0 °C, 0.5
  °C over the rounding edge, and the 12:55 METAR reported 14 °C, with
  showers around.
  The market had priced the same (the NO at 0.95), so K could not buy. It
  is one of the misses the replay's table expects in that band (194 new
  highs in 199 reports).
* **The model against the market** on the winning bucket: on 10 October
  the model (log loss 0.973 against 0.979; at 12:57 it gave 14 °C 0.62, the
  market 0.05), on 11 and 12 October the market (0.412 against 1.061; 0.293
  against 0.482). No strategy that trades uses the model's probability.
* **Data.** The day-1 forecast's maximum was within 0.5 °C (errors +0.0,
  −0.1, +0.5 °C); 136 reports arrived on time (median 150 s); AWC was
  degraded twice for a few minutes (on 12 October from 16:02 to 16:07, in
  F's slot) while TGFTP delivered.

## 4. Next review

After about 20 settled days from 2 October, `research market --from
2026-10-02` replays the new days for all 25 families. The families on keep
their live record beside it; L7 goes off if neither record earns.
