# Where the edge is — and where it is not

*Question (27 September 2026).* The market had "Highest temperature in
Amsterdam on September 27?" at 21 °C = 99.5–99.9 % while Weather Machine
was still waiting for its 60-minute confirmation (the high was retested every
half hour, so the clock never started). The book had decided the day. How do
we take the book into account, and where can the bot actually earn?

**Short answer.** Agreeing with a book that has already decided earns
nothing. The money is made only where the price is *wrong*:

1. for a short time after new information, before quotes are updated;
2. where the bot knows something the market does not;
3. by providing liquidity instead of taking it (not built yet).

This change therefore:

* uses the book as information (it can veto a model trade);
* adds a strategy for the one case where the answer is certain;
* fixes two defects that would have blocked afternoon trading;
* adds a measurement (`research market`) that shows, on settled markets and
  with the bot's own model, which of these edges exist for EHAM.

## 1. What a decided book leaves on the table

Buying YES at 0.998 pays 0.002 per share, minus the taker fee of
0.05 × 0.998 × 0.002 ≈ 0.0001. On a $10 position that is about **$0.02**. A
single later report of 22 °C loses the whole **$10**. The trade is worth it
only if P(21 °C is final) > 99.81 %.

Prices of near-certain contracts also stay below 1.0 for a reason unrelated
to the weather: the collateral is locked until the oracle settles. Gebele &
Matthes measure this settlement discount across prediction markets.

So 0.995–0.998 on a "decided" 21 °C is mostly a correct price plus a
settlement discount, not a mispricing.

## 2. What others found

| Source | Finding |
|---|---|
| [jattree/weather-edge](https://github.com/jattree/weather-edge) | 697 events scored 36 h before close: market log loss **1.31** vs model **1.59**. The market's implied temperature is off by ~0.2 °C, raw models by ~0.8 °C. Best blend: **100 % market**. Simulated trades: −13.9 % ± 5.3 % per trade (1,905 trades). |
| [BallesJr/polymarket-weather-edge](https://github.com/BallesJr/polymarket-weather-edge) | Daily highs computed from station METARs matched resolutions 93 % of the time, vs 66 % for a reanalysis grid. An early edge (+$157 over 527 trades) did not hold out of sample (−$592 over 847 trades). |
| [Celmaro/polymeteo](https://github.com/Celmaro/polymeteo) (copy-trading tool) | "Weather markets reprice quickly after informed flow"; ~1 s treated as typical copy latency, "edge decays fast". Its dashboard figures are curated demo numbers, not evidence. |
| [Our review of the weatherforecaster bot](weatherforecaster-review.md) | Forward paper trading lost 19.7 % across every strategy once look-ahead was removed. |
| [Polymarket fees](https://docs.polymarket.com/trading/fees), [fee guide](https://startpolymarket.com/learn/polymarket-fees/) | Taker fee = shares × 0.05 × p × (1 − p) on weather markets. Makers pay nothing and receive a share (up to 25 %) of taker fees as daily rebates. |
| [Gebele & Matthes, arXiv:2605.31431](https://arxiv.org/abs/2605.31431) | Near-certain contracts carry a maturity-dependent settlement discount (collateral lock-up until oracle settlement). |
| [Polymarket/clob-client#331](https://github.com/Polymarket/clob-client/issues/331) | Daily weather markets report `endDate` = 12:00 UTC on the target day, although they trade until the day's data are final. |

The independent attempts agree. On these contracts, public forecasts do
not beat the market's prices, and directional edges found in backtests did
not survive live trading. Tools that copy informed traders within about a
second are advertised for these markets.

## 3. What changed in Weather Machine

| Change | Why | Effect |
|---|---|---|
| **Market end time** | Gamma's nominal `endDate` (12:00 UTC) was used as the market's end, and the risk engine rejects intents after the end. | Every signal after 14:00 Amsterdam time (summer) would have been rejected. A daily-high market now ends no earlier than its local day. |
| **Quiet books** | The market channel sends only *changes*, so a book nobody touched looked stale. The dashboard showed book ages of 12–87 s, and every book older than 15 s blocked every strategy. | A heartbeat (PONG) on the live connection now confirms every book received on that connection. A book counts as current while its connection is alive. |
| **Strategy D — decided outcomes** | The one case where the answer is certain: the high can only rise. | When every resolution view has seen a high of H, buckets entirely below H are NO, and an open top bucket "≥ L" with L ≤ H is YES. D buys on the observation event itself (no model), at most 0.99, if ≥ 0.02 per share remains after fee and 0.002 slippage. That excludes the 0.98–0.99 quotes of long-dead buckets, which are settlement discount, not stale quotes. A jump of > 3 °C from the previous report waits for a second report; a recent correction blocks trading for 10 min; the risk gates are unchanged (spread ≤ 0.05, depth, exposure). |
| **The book as information (A, B)** | When the market disagrees, the market is more likely right (§2). | The model's probability is pooled with the midpoint of a fresh book (spread ≤ 0.10): logit p = w·logit(market) + (1 − w)·logit(model), with w = `market_weight` (default 0.5). The result is capped at the model's probability, so the market can veto a trade but never create one. The ladder shows the probability used, and blockers name the market. |
| **`research market`** | Replace opinions with measurements on EHAM's own settled markets. | See §4. |

## 4. The measurement: `weather-machine research market`

For every settled market in the date range, the command:

1. fetches the event from Gamma and its trades from the Polymarket Data API
   (settled days are cached under `/data/research/polymarket/EHAM/`);
2. replays the METAR history prequentially: each day is scored with the
   model trained only on the days before it, using live-trading code;
3. at every report (plus the bot's decision delay, default 180 s), compares
   the model's probability for each still-possible bucket with the market's
   (the midpoint of the latest taker buy and sell).

It answers these questions:

| Section | Question | How to act on it |
|---|---|---|
| Who predicts better | Log loss of model, market and pools (w = 0, 0.25, 0.5, 0.75, 1), with 95 % day-block intervals; overall and where A/B trade. | Set `market_weight` to the best weight. If the market alone is best, the model adds nothing: A and B have no information edge. With w = 1 they effectively never trade, because the market's own midpoint never clears its ask. |
| Where prices are wrong | Win rate by market price; outcomes when the model disagreed by ≥ 5 points. | A price bin whose win rate is clearly above its price is where buying pays. |
| Who is sure first | When the winning bucket reached 90 / 95 / 99 % in the market vs in the model. | Shows how much the confirmation rule costs (on 27 September the book was at 99.5 % while the bot still waited). |
| How fast dead buckets reprice | After each new high: stale quotes on the killed buckets that were taken, by delay after the observation, and before or after our decision time. | Profit taken before our decision time → D needs faster data. Profit still taken after it → D would have found fills. |
| Resolution check | Did the METAR high fall in the resolved bucket? | D assumes it does; any mismatch must be understood before trusting D. |
| Both structures | Does the candidate model structure ([replay of 28 September](replay-2026-09-28.md)) predict better than the current one, and than the market? | Training decides the structure; this shows it against the market. |
| Strategies at traded prices | What would A and B have earned at the prices the market actually traded, per structure, confirmation window (0′/30′/live) and ask range? | Change a live rule only if its variant stays profitable on days after it was chosen (24 variants are tried). |
| Day replay (`--day`) | Report by report: both models' cells and probabilities, the market, the ask and every simulated trade. | Explains a single day, such as 28 September. |

The image has no shell, so run it as a one-off container on the stack's
data volume. In Portainer: *Containers → Add container*, same image, the
volume `weather-machine_wmdata` at `/data`, `WM_CONTACT` set, and the command
below. The container's log shows the report.

```sh
docker run --rm -e WM_CONTACT=you@example.org -v weather-machine_wmdata:/data \
  ghcr.io/spongi07/weathermachine:latest \
  research market --from 2026-06-01 --to 2026-09-28 --day 2026-09-28 --print
# also written to /data/research/eham-market.md (+ eham-market.json)
```

It uses about 1–3 Data API requests and one Gamma request per day, two per
second at most. METAR history comes from the training cache.

## 5. Follow-up: the replay of 28 September

On 28 September nothing traded either. The step-by-step
[replay](replay-2026-09-28.md) shows why: the model, not a rule, was the
bottleneck. It lumped the whole morning into one cell, restarted its clock
when 21 °C was reported twice, and its forecast input could not see that the
observed 21 °C had already reached the forecast's 21.4 °C maximum. The only
money was in the first half hour after 21 °C was reached. A pre-registered
candidate structure, selected walk-forward at training, and the replay of
strategies at traded prices above were built from it.

## 6. Strategy E — a late favourite, confirmed by the book

The operator's rule: *between a set local time window, once the temperature
has reached its high and the book is shrinking, buy YES on the high's
bucket at 90–99 ¢.* Strategy E implements it
([§27c](../blueprint/06-strategy.md#27c-book-confirmed-high--strategy-e));
it runs in paper mode like A, B and D.

**What each condition means in the code** (defaults in
`[strategies.book_confirmed]`):

* *a specific time*: local time in 12:00–18:00;
* *the temperature reached its highest*: the high was first reported
  ≥ 60 min before the latest report, and that report is ≥ 1.0 °C lower. A
  flat top does not restart the clock (the lesson of 28 September, when
  21 °C was reported twice);
* *the book is shrinking*: the YES shares offered at or below 0.99 fell by
  ≥ 30 % within 30 min, from ≥ 50 shares, and the best ask did not fall.
  Offers taken or withdrawn count; sellers undercutting each other do not;
* *buy at 90–99 ¢*: a fill-and-kill order at the ask, $10, if the ask is in
  [0.90, 0.99]; a probability model must be loaded, but no model edge is
  needed.

**What the evidence says — read before trusting it.**

* Favourites tend to win a little more often than their price says. The
  margin is thin, and it varies by market and by how contracts are grouped:
  * On Polymarket, purchases at ≥ 90 ¢ earned +0.83 ¢ per dollar, while
    purchases under 10 ¢ lost 19.3 ¢. The pattern holds in crypto and
    politics but is absent in sports
    ([Cardozo & Rivero-Wildemauwe 2026](https://arxiv.org/abs/2609.12878)).
  * On Kalshi, high-priced contracts win more often than their price and
    earn a small positive return. Takers, which E is, do much worse than
    makers
    ([Bürgi, Deng & Whelan, "Makers and Takers"](https://www.karlwhelan.com/Papers/Kalshi.pdf)).
* Temperature markets are well calibrated at the top. Where Kalshi's
  temperature market said 0.99, the bucket settled YES (60,906 settled
  contracts). "Well calibrated" also means little is left over: that
  study's model-based strategy lost 8.75 ¢ per contract
  ([anaborne/kalshi-temperature-calibration](https://github.com/anaborne/kalshi-temperature-calibration)).
* Other work points the other way:
  * Weather prices are *too extreme* at short horizons
    ([Decomposing Crowd Wisdom](https://arxiv.org/abs/2602.19520)).
  * On Kalshi, backing sports favourites lost 0.42 ¢ per contract at the
    midpoint and 2.38 ¢ after spread and taker fees
    ([nalimmm/kalshi-calibration](https://github.com/nalimmm/kalshi-calibration)).
* EHAM's own report (1 June – 28 September 2026, market price bins, every
  decision and bucket) says:

  | market price | mean price | won |
  |---|---:|---:|
  | 0.90–0.98 | 0.949 | 96.8 % |
  | 0.70–0.90 | 0.799 | 83.9 % |
  | 0.98–1.00 | 0.997 | 100 % |

  These points are correlated within a day, so they are not independent
  trades.
* A shrinking book is order-flow information. Order-flow imbalance explains
  price changes over short intervals
  ([Cont, Kukanov & Stoikov](https://arxiv.org/abs/1011.6402)), not whether
  a bucket settles YES. Whether it also picks better days is exactly what
  the replay has to show.
* Arithmetic sets a high bar. After the taker fee (0.05 × p × (1 − p)) and
  0.005 slippage, the bucket must win 91.0 % at 0.90, 95.7 % at 0.95 and
  99.55 % at 0.99. One loss at 0.95 (−$10.08 at $10) wipes out about 22 wins
  (+$0.45 each).

**How to check it.** `research market` replays E at the prices the market
actually traded. The order book itself is not archived, so the replay
stands in for "the book is shrinking" with the trades. In the 30 min before
the decision, takers must have bought ≥ 15 YES shares at or below 0.99
(the live rule's 50 × 30 %), more than they sold, without the ask falling.
Read the E line of the verdict and the E rows:

* *E* against *E w/o book*: does the book condition pick better trades, or
  only fewer?
* *E* against *E + model*: does the model's agreement (≥ 0.90) help?
* The 95 % interval: with a few dozen trades one loss decides the total. A
  positive total whose interval includes zero is not evidence.

Live, every evaluation line names E's blocker, e.g.
`E 21°C YES · ask 0.95 … — book not shrinking: 300 → 290 shares offered ≤ 0.96 in 30m (−3%, need −30%)`.
Every paper trade records the time, the drop and the book numbers in its
rationale.

Protocol sources: Polymarket's market channel sends a `book` snapshot on
subscribe and when a trade changes the book. `price_change` gives a level's
new size (0 removes it), and the tick size changes above 0.96 and below
0.04
([Polymarket websocket reference](https://github.com/Polymarket/agent-skills/blob/main/websocket.md)).

## 7. Next levers (not built)

* **Faster observations.** KNMI publishes
  [10-minute station observations](https://english.knmidata.nl/open-data/10-minute-in-situ-meteorological-observations)
  through the [EDR API](https://developer.dataplatform.knmi.nl/edr-api)
  (API key required,
  [collection](https://english.knmidata.nl/latest/news/2025/07/03/edr-api-new-10-minute-in-situ-meteorological-observations-collection)).
  It could show a new high before the half-hourly METAR. This is only worth
  building if §4 shows that stale quotes are taken *after* the METAR, not
  before it.
* **Providing liquidity.** Late in the day, quoting NO on buckets far above
  the high earns the spread plus rebates instead of paying fees. This needs
  order management, and live execution is Phase 14.
* **A spread limit for D.** The risk engine's spread ≤ 0.05 and "no
  one-sided book" gates apply to D too. If §4 shows stale quotes mostly in
  wide books, a separate limit for decided outcomes can be justified with
  data.
