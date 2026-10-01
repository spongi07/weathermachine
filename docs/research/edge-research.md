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
| **The book as information (A, B)** | When the market disagrees, the market is more likely right (§2). | The model's probability is pooled with the midpoint of a fresh book (spread ≤ 0.10): logit p = w·logit(market) + (1 − w)·logit(model), with w = `market_weight` (default 0.5). The result is capped at the model's probability, so the market can veto a trade but never create one. Without such a book (too wide, one-sided or crossed) A and B do not trade: the model alone is the weaker opinion. The ladder shows the probability used, and blockers name the market. |
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

**Result on EHAM, 1 June – 27 September 2026** (119 settled days, run of
28 September):

| variant | trades | won | total ($10 per trade) |
|---|---:|---:|---:|
| E | 79 | 76 | −$7.72 |
| E w/o book | 86 | 83 | −$4.15 |
| E + model, current structure | 70 | 68 | −$8.03 |
| E + model, candidate structure | 71 | 69 | −$5.45 |

* E lost $0.10 per trade (95 % CI −$0.55 … +$0.29).
* The book stand-in removed seven winners and none of the three losers.
* The mean ask was 0.964, where break-even is 97.1 %; E won 96.2 %.

On these days the rule has no edge. By the time the high is an hour old
and a degree lower, the market already prices the bucket so high that the
favourite–longshot margin no longer covers fee, slippage and the rare loss.

Live, every evaluation line names E's blocker, e.g.
`E 21°C YES · ask 0.95 … — book not shrinking: 300 → 290 shares offered ≤ 0.96 in 30m (−3%, need −30%)`.
Every paper trade records the time, the drop and the book numbers in its
rationale.

Protocol sources: Polymarket's market channel sends a `book` snapshot on
subscribe and when a trade changes the book. `price_change` gives a level's
new size (0 removes it), and the tick size changes above 0.96 and below
0.04
([Polymarket websocket reference](https://github.com/Polymarket/agent-skills/blob/main/websocket.md)).

## 6a. Strategy F — the high's bucket inside the peak slot

The operator's rule: *learn at what time the day's highest temperature is
measured (it differs per season). Inside that time slot, watch the order
book; once the bucket of the current measured temperature trades above
90 ¢, buy 100 shares, because after the slot the temperature cannot go
higher.* Strategy F implements it
([§27d](../blueprint/06-strategy.md#27d-peak-slot--strategy-f)) in paper
mode.

**When is the high measured?**

* In Belgium maxima typically come around 14:30 UTC all year, 2–3 hours
  after solar noon: about 16:30 local in summer and 15:30 in winter. In
  winter the highest temperature can also come at night, after a warm
  front ([KMI](https://www.meteo.be/nl/info/weerwoorden/maximumtemperatuur)).
* The lag after noon varies between about 1.9 and 2.9 hours with latitude
  and season
  ([diurnal temperature variation](https://en.wikipedia.org/wiki/Diurnal_temperature_variation)).
  The weather of the day, the terrain and the surroundings shift it
  ([Hong Kong Observatory](https://www.hko.gov.hk/en/education/weather/sunshine-and-uv/00692-What-time-in-a-day-is-highest-lowest-air-temperature.html)).
* The market settles on something else: the day's highest *whole-degree
  METAR value*. The moment that matters is the first report at that value.
  It usually comes before the true maximum, because the rounded value is
  reached on the way up to the peak. So the bot measures that moment in
  EHAM's own METAR history, per season.
  * Training prints the mean, the median, the quantiles and the share of
    days later than the slot, later than 17:00 and before 09:00.
  * F's slot ran from the median to the 90th percentile of that moment
    until 1 October 2026. After the 122-day replay (that slot −$122.00
    over 84 trades, 75 % → 95 % +$11.13 over 80, within noise) the shipped
    configuration uses the 75th to the 95th percentile.

**"It cannot go higher after the slot" is a hypothesis.** The slot ends at
the 90th percentile, so by construction one day in ten first reports its
high later. In winter the high can come at night. The table's *later than
the slot* column shows how often it happened at EHAM.

**Is there an edge?** The arithmetic first. After the taker fee
(0.05 × p × (1 − p)) and 0.005 slippage, the bucket must win:

| price | must win |
|---:|---:|
| 0.91 | 91.9 % |
| 0.93 | 93.8 % |
| 0.95 | 95.7 % |

One loss of the 100 shares (about −$94) costs as much as 15 wins at 0.93;
at 0.95 it is 22 wins.

* EHAM's own report (1 June – 28 September 2026): buckets priced 0.90–0.98
  won 96.8 % of the time at a mean price of 0.949. That is about one point
  above break-even, so roughly +$1 per 100 shares, on correlated decision
  points.
* Strategy E bought later and dearer (mean 0.964). It won 96.2 % against a
  97.1 % break-even and lost 1 % per trade (§6).
* On Polymarket, purchases at ≥ 90 ¢ earned +0.83 ¢ per dollar
  ([Cardozo & Rivero-Wildemauwe 2026](https://arxiv.org/abs/2609.12878)).
* Temperature markets are well calibrated at the top
  ([anaborne](https://github.com/anaborne/kalshi-temperature-calibration)),
  and weather prices can be too extreme at short horizons
  ([Decomposing Crowd Wisdom](https://arxiv.org/abs/2602.19520)). The small
  discount may therefore be gone.

On this evidence the plain rule ("above 90 ¢, 100 shares, in the slot") is
break-even at best.

**What F does to make it work:**

1. **Price cap 0.95.** All 100 shares must be offered at or below 0.95,
   walking the offers, fill-and-kill. From 0.96 to 0.99 one loss costs 30
   to 220 wins, and that is where E lost. Above the cap F waits, and the
   evaluation shows how many shares were offered.
2. **The station's own slot per season,** learned in training. In research
   it is learned prequentially: a day is never traded with a slot that knew
   it.
3. **Continuous watching.** F is evaluated on every book update, so it buys
   the moment the ask first crosses 0.90 inside the slot. That is the lowest
   price the rule allows, not the price at the next report.
4. **One position a day, with its own risk caps.** A loss also trips the
   daily-loss stop.
5. **Out-of-sample choice between variants.** `research market` replays F
   at traded prices with five variants: to 0.99, an earlier slot (25 → 75 %),
   a later slot (75 → 95 %), 1 °C below the high, and a resting bid instead
   of paying the ask (which pays no fee and earns the rebate). The rule that
   did best on the first half of the market days is scored on the second
   half.

**How to decide.** Read the line *Strategy F out of sample* in the verdict.

* Positive, with its 95 % interval above zero: keep F, or switch to the
  chosen variant through `[strategies.peak_slot]`. Examples:
  `min_drop_tenths = 10`, `max_price`, `slot_from_quantile` /
  `slot_to_quantile`.
* An interval that includes zero: keep it in paper and collect more days.
* Negative: set `enabled = false`.

## 7. What we had not tried: providing liquidity

Every strategy so far (A, B, D, E, F) *takes* liquidity. It crosses the
spread with a fill-and-kill order and pays the taker fee. The research on
who wins on these exchanges points the other way.

* **Polymarket.** The study covers 2.4 million users and $67 billion in
  volume, November 2022 – March 2026
  ([Akey, Grégoire, Harvie & Martineau 2026](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6443103)):
  * The strongest predictor of a profitable account is how much of its
    volume it provides as a maker: +9.0 percentage points per standard
    deviation of maker share.
  * The top 0.1 % of winners earn the spread; losers pay it. Removing just
    the minimum-tick spread would lift 18.5 % of losers to non-negative P&L.
  * Weather prices stray from calibration even a day before resolution.
    Weather also has the largest share of profitable users (52.5 %).
* **Kalshi.** Over 72.1 million trades, takers averaged −1.12 % and makers
  +1.12 %. Takers overpay for YES at longshot prices
  ([Becker](https://www.jbecker.dev/research/prediction-market-microstructure)).
  Takers also lose far more than makers in
  [Bürgi, Deng & Whelan](https://www.karlwhelan.com/Papers/Kalshi.pdf).
* **Polymarket weather fees** (since 30 March 2026). Takers pay
  0.05 × p × (1 − p) per share. Makers pay nothing and receive 25 % of the
  taker fees as a rebate. A market can also carry a daily liquidity-reward
  pool for resting orders near the midpoint: a quadratic score within a
  maximum spread and above a minimum size, favouring two-sided quotes.
* **The catch is adverse selection.** In EHAM's own report, stale quotes
  worth $3,209 were sold into resting bids in the ten minutes before METAR
  reports, against $138 in the first minute after. Resting orders on a
  bucket about to die get picked off by takers who know first.
  * In Polymarket's 5-minute crypto markets the average maker netted
    +0.275 ¢ per share, the author's own maker lost 0.48 ¢, and a maker
    replay flipped sign when latency moved by 150 ms
    ([Yak0vkaSup/polymarket-microstructure](https://github.com/Yak0vkaSup/polymarket-microstructure)).
  * An open-source Polymarket market maker warns that it "can lose money"
    ([warproxxx/poly-maker](https://github.com/warproxxx/poly-maker)).

**What `research market` now measures.**

* **Makers and takers: the other side of every trade.** For every recorded
  trade on the settled days: the taker's P&L at settlement and the
  maker's (its negative, plus the rebate). It is split by:
  * the price the taker paid;
  * the side;
  * the bucket against the published high;
  * minutes to the next routine METAR;
  * the high's bucket before a report;
  * local time.

  This is not a simulation. It is what the actual makers earned, and it
  shows where resting orders are paid and where they are picked off.
* **A maker, B maker, E maker.** The live rules posted as limit orders:
  * a YES bid at the latest taker sell (A, E), or a NO bid at one minus the
    latest taker buy (B);
  * counted as filled only when a later trade goes through the price;
  * cancelled 10 minutes before the next routine report;
  * no fee, 25 % rebate.
* **`weather-machine markets discover`** now lists the liquidity-reward
  pool of today's markets, if Gamma lists one.

**How to read it.** Suppose the maker side of the high's bucket is positive
away from reports and negative in the minutes before them. Then the edge is
to quote passively between reports and step away before each METAR, which
is what the maker versions test. If even the maker side loses at EHAM,
being the maker is no edge here either. Fills from the public tape are
estimates either way. A positive result earns a paper test with live maker
orders, not money.

## 8. Next levers (not built)

* **Faster observations** (built in October 2026 as strategy K, §9). Most
  stale-quote profit is taken before the
  report: $3,209 in the ten minutes before, $138 in the minute after
  (1 June – 28 September 2026). Whoever takes it anticipates the METAR, so
  only a faster source can compete for it. KNMI publishes
  [10-minute station observations](https://english.knmidata.nl/open-data/10-minute-in-situ-meteorological-observations)
  a few minutes after each interval, through the
  [EDR API](https://developer.dataplatform.knmi.nl/edr-api) and the
  notification service (API key required).
* **Live maker orders** (built in October 2026 for strategies G and J,
  §9). If §7 finds a maker edge: resting GTD orders in
  paper mode, filled by the trade feed and cancelled before each report.
  This is order management that live execution (Phase 14) needs anyway.
* **A spread limit for D.** The risk engine's spread ≤ 0.05 and "no
  one-sided book" gates apply to D too. If §4 shows stale quotes mostly in
  wide books, a separate limit for decided outcomes can be justified with
  data.

## 9. October 2026: five new strategies

*Question (1 October 2026).* Build five new weather strategies that are
worth the time, with the best configuration the evidence supports, and
remove the strategies that did not trade in the last two days.

**Short answer.** Five strategies, G–K, each aimed at one mispricing that
the settled Amsterdam markets show at traded prices *and* the literature
explains. Three of them earn as makers or fade a bias (G, I, J), one buys the
cheap next degree (H), one is faster than the METAR (K). None is proven: each
is a hypothesis with replay evidence, judged out of sample by `research
market`, to run in paper first. A, B and D (no trade on 30 September – 1
October) are switched off, C and E were already off.

### 9.1 What others found

| Source | Finding | Used in |
|---|---|---|
| [Bürgi, Deng & Whelan, *Makers and Takers* (Kalshi)](https://www.karlwhelan.com/Papers/Kalshi.pdf) | 300,000+ contracts: a clear favourite–longshot bias; contracts under 10¢ lose over 60 % of the stake, those above 50¢ earn a small positive return. Makers earn more than takers at every price; the bias is much stronger for takers. | G, J |
| [Cardozo & Rivero-Wildemauwe, arXiv:2609.12878](https://arxiv.org/abs/2609.12878) (Polymarket) | Purchases under 10¢ lose 19.3¢ per dollar, purchases at 90¢ or more earn 0.83¢; calibration slopes 1.01–1.13 (favourites win more often than priced). Grouped by parent event, longshots *gain* 4.1¢ — the bias depends on how contracts are counted. | G (only far tails our own data confirm), F |
| [Becker, prediction-market microstructure](https://www.jbecker.dev/research/prediction-market-microstructure); [Akey et al. 2026](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=6443103) | Takers −1.12 %, makers +1.12 % over 72.1 M Kalshi trades; on Polymarket the maker share of volume is the strongest predictor of a profitable account (§7). | J, G |
| [Polymarket fees](https://docs.polymarket.com/trading/fees), [maker rebates](https://docs.polymarket.com/market-makers/maker-rebates), [liquidity rewards](https://docs.polymarket.com/market-makers/liquidity-rewards) | Weather takers pay 0.05 × p × (1 − p) a share; makers pay nothing and share 25 % of the fees as daily rebates; resting orders within a market's maximum spread of the midpoint can also earn liquidity rewards. | G, J (rebates not counted in the replay) |
| [Saguillo, Ghafouri, Kiffer & Suarez-Tangil, arXiv:2508.03474](https://arxiv.org/abs/2508.03474) | About $40 M taken on Polymarket by arbitrage: within one event (prices of exclusive outcomes not summing to 1) and across related markets. | not built (below) |
| [KNMI: new EDR collection](https://english.knmidata.nl/latest/news/2025/07/03/edr-api-new-10-minute-in-situ-meteorological-observations-collection), [dataset](https://dataplatform.knmi.nl/dataset/10-minute-in-situ-meteorological-observations-1-0), [EDR API](https://developer.dataplatform.knmi.nl/edr-api) | Ten-minute readings of every Dutch automatic station, "available a few minutes" after each interval, free API key; replaces `Actuele10mindataKNMIstations` (deprecated 1 July 2025, no updates after 29 September 2025). | K |
| [Weather Lock-In Desk spec](https://edge-assist.duckdns.org/data-status/docs/weather/strategy) (public strategy write-up) | After a city's typical peak time, a drop of 4 °F or more below the running high leaves it final in about 92–99 % of cases: sell the brackets above it. A design, not a measured result. | F, G (as a maker) |
| GitHub bots: [watkast/kalshi-weather-bot](https://github.com/watkast/kalshi-weather-bot), [suislanchez/polymarket-kalshi-weather-bot](https://github.com/suislanchez/polymarket-kalshi-weather-bot), [tobiasbischoff/polymarket-weather-bot](https://github.com/tobiasbischoff/polymarket-weather-bot), [myfirstcodeo/kalshi-weather-fair-value](https://github.com/myfirstcodeo/kalshi-weather-fair-value) | Almost all compare a forecast with the price and take liquidity. The published results are short (one: 8 days) or come with the author's warning: "Do not run this as a mechanical strategy; it loses." | why G–K do not trade forecast vs price |
| Trading guides ([tradetheoutcome](https://www.tradetheoutcome.com/polymarket-weather-strategy/)) | "Cluster betting": buy several adjacent buckets for well under $1 in total. | not built: it buys the middle that our data find overpriced (I fades it) |

### 9.2 What our own data say

122 settled EHAM markets (June–September 2026), 191,660 traded prints,
every decision point at the price actually traded after the bot could know
it (`research market`):

| Finding | Number | Strategy |
|---|---|---|
| YES at 0.00–0.02 | won 0.1 % at a mean price of 0.4 %; the makers who sold it earned +0.34¢ a share net (95 % CI +0.26 … +0.41) | G |
| Makers by bucket against the high | +2 above +1.04¢ (−0.06 … +2.27); +3 or more +0.41¢; +1 above **−0.68¢** (−1.27 … −0.10) | G sells from +2 only |
| Strategy B (taker NO above the high) | −$92.74 over 129 trades | B off |
| YES at 0.02–0.10 | 5.6 % won at 5.0 %: about fair | G's 8¢ cap |
| YES at 0.10–0.30 | 21.0 % won at 18.7 % (18.6 … 23.6); the takers on the +1 bucket gained +0.78¢ a share before fees | H |
| YES at 0.30–0.70 | 45.4 % won at 48.5 % (42.8 … 48.0); with the model ≥ 5 points below the price 51.1 % at 53.6 % | I |
| Makers by local time | evening before +0.71¢, 00–09 +0.61¢, 09–12 +0.47¢, 12–15 **−0.59¢** | J quotes until 11:00 |
| Makers before a report | the high's bucket 0–5 min before: **−1.34¢** (−2.05 … −0.66) | G, J cancel 10 min before |
| New highs | 58 reports raised the high; takers took $3,330 in the ten minutes before the observation; the first stale trade came a median 39 s after it | K is earlier; D off (too late) |

### 9.3 The five strategies

Full rules, risks and tests: [strategy blueprint §27e–§27i](../blueprint/06-strategy.md#27e-tail-seller--strategy-g).

| | Rule (shipped values) | Size, caps | Edge it takes |
|---|---|---|---|
| **G** tail seller | NO bid one tick inside the book on buckets ≥ high + 2, YES offered 1–8¢, model veto; good till 10 min before the next report; 10:00–21:00 | $30 an order, $120 | longshot bias, as the maker |
| **H** next degree | YES of high + 1 at 0.05–0.35 when the model rates it at least the ask, until the season's 75 % peak time; FAK | $10, $30 | cheap next degree while the day can warm |
| **I** middle fade | NO of buckets with a YES midpoint of 0.30–0.70, spread ≤ 4¢, the model ≥ 5 points below; p = 1 − (mid − 0.03); FAK | $10, $30 | overpriced middle of the ladder |
| **J** morning maker | YES and NO bids inside the spread, 00:00–11:00, midpoint 0.10–0.90, spread 2–5¢; good till 10 min before each report; no model | $10 a side, $80 | spread capture when makers are paid |
| **K** KNMI nowcast | NO of the high's bucket when KNMI's ten-minute mean is ≥ high + 0.8 °C (maximum ≥ high + 0.5 °C) before the METAR is published; NO ask 0.02–0.75, EV at p = 0.80 ≥ 0.05; FAK | $25, $50; spread ≤ 0.10 | minutes ahead of the report |

**How the configuration was chosen.** Each threshold sits where the replay
tables change sign (G's distance 2 and its 8¢ cap, J's 11:00 end, the
cancel 10 minutes before a report); sizes keep every strategy's worst case
inside the shared limits ($400 global, market and location; $600 new a day;
$100 daily loss). `research market` replays each strategy with variants (G
≤ 3¢, ≥ 3 above, no model, from 14:00; H no model, until 90 %, maker; I no
model, no bias, maker; J YES or NO bids only, until 09:00; K margins 0.1
and 0.5 °C, the YES above, the reading known after 2 or 8 minutes), picks
the best variant of each family on the first half of the days and judges it
on the second half. Change the shipped values only when that out-of-sample
line agrees.

**Not built.** Arbitrage across the ladder (the outcome prices of one event
not summing to 1) needs every leg filled at once: up to twelve legs, each
paying the taker fee, from book snapshots that do not arrive together.
Forecast-versus-price taking is the crowded trade the bots above make and
the earlier attempts lost on (§2).

### 9.4 Caveats

* All evidence is in sample: one city, one summer, 122 days, correlated
  within each day.
* G earns fractions of a cent per dollar; one winning tail costs 12 to 100
  times the premium. H and I take edges about the size of the taker fee plus
  half the spread.
* J's and G's fills come from the public tape; queue position is not
  known, so real fills will be fewer.
* K's `p_new_high` = 0.80 is an assumption and the KNMI delay (5 minutes)
  is assumed: run `research market` with `WM_KNMI_API_KEY` and read *KNMI's
  ten-minute mean before the METAR* before relying on it. Without the key K
  does nothing.
* Paper first: `weather-machine report paper` per strategy before any money.
