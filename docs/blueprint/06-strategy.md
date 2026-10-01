# 22–28 · Peak detection, probability, strategies A/B/C/D/E/F, market pooling, unwind (+ EV)

> **The core trading belief is a HYPOTHESIS TO BACKTEST, not a fact:** once
> the day's high has been reached and not retested for N minutes, it becomes
> increasingly likely to be final. The code never assumes it. The
> probability of "final" comes only from a model trained on history
> (`EmpiricalPeakModel`). Without such a model the `NoEdgeModel` returns
> nothing, and strategies A, B, E and F cannot propose any trade.

## 22. PeakDetectionEngine

**PURPOSE.** Turn a view's `DayState` into features and confirmation evidence.
**INPUTS.** `DayState` (§17), time zone, station longitude (for solar noon).
**OUTPUTS.**
`PeakAssessment { features: PeakFeatures, windows_met: Vec<u32>, peak_watch: bool }`.
**RUST INTERFACE.**
`PeakDetectionEngine::new(PeakConfig)`, `assess(&DayState, Tz, now) -> Option<PeakAssessment>`.

* Confirmation windows **30, 45, 60, 75, 90, 105, 120, 150, 180 min**
  (`CONFIRMATION_WINDOWS`). A window is met by *observed* coverage after the
  last touch of the high (`last_observation_at − high.last_at`). Missing data
  never counts as confirmation.
* `peak_watch` (collector hint) is set inside a solar-noon-relative window
  (−180…+300 min) when the current value is within 1 °C of the high. It
  only switches the polling mode, inside arrival windows and never below the
  NOAA floors.

**FAILURE MODES.** No observations → no assessment → no trade. A new high
resets coverage.
**TESTING.** Unit tests for coverage vs wall-clock time, retests and resets;
`hints_request_attention_only_near_the_peak`.

## 23. Trajectory features

`PeakFeatures` (per view, every evaluation): `high_tenths`, `high_whole`
(ICAO rounding), `high_at`, `current_tenths`, `drop_tenths`,
`minutes_since_high` (observed), `minutes_since_first_high`,
`lower_obs_since_high`, `retests`, `slope_c_per_hour` (least squares over
90 min), `accel_c_per_hour2` (slope of the last 60 min minus the previous
60 min), `trajectory`, `local_minute_now`, `high_local_minute`,
`minutes_after_solar_noon`, `month`, `season`, `observation_count`,
`data_age_minutes`.

`TrajectoryClass`: **AtHigh** (latest point is the high), **SteadyDecline**
(non-increasing with a decrease, e.g. 18.0 → 17.8 → 17.5 → 17.2),
**Oscillating** (down and up below the high, e.g. 18.0 → 17.9 → 18.0 → 17.9),
**Insufficient**.

**HYPOTHESIS TO BACKTEST.** SteadyDecline carries more "final" information
than Oscillating at the same minutes-since-high. The survival study (§36)
stratifies by `trajectory=` precisely to test this. **TESTING.** Unit tests
classify the brief's two example sequences.

## 24. Probability model

**PURPOSE.** P(final high = observed high + k) for k = 0, 1, 2, ≥ 3.

```rust
pub trait ProbabilityModel { fn id(&self) -> &str;
    fn distribution(&self, f: &PeakFeatures) -> Option<IncrementDistribution>; }
pub struct IncrementDistribution { probs: Vec<f64>, support: u32, source: String }
```

* **`EmpiricalPeakModel`** counts outcomes per feature cell with
  *hierarchical Dirichlet smoothing*: global → minutes-since-high →
  +drop → +season → +local hour. Each cell is shrunk toward its parent with
  prior strength α = 20, so sparse cells borrow strength instead of
  overfitting. `support` reports the most specific cell's sample count, and
  strategies require ≥ `min_model_support` (50).
* **Conservative bucket probabilities.** The ≥ 3 tail is counted towards a
  YES bucket only if the bucket contains *every* such value (lower bound),
  and towards a NO bucket's loss if it contains *any* (upper bound).
  Property test: for any distribution and bucket partition, each lower ≤
  upper and Σ lower ≤ 1 ≤ Σ upper.
* Training: automatic on the host. When the model file is missing, `run`
  downloads the station's METAR history from IEM (rate-limited, finished
  years cached), runs the peak-survival study, refuses fewer than 730
  usable days, writes the model and report atomically and restarts once to
  load it. Manual: `weather-machine model train`, or
  `research peak-survival --csv <IEM export> --model-out model.json`. The
  model is JSON, versioned by id (with training date), station and view.

**FAILURE MODES.** No model or no distribution → no signal. Training
failure (e.g. IEM unreachable) → still no model, retried after 6 h.
Unusable model file or station/model mismatch → the service runs without a
model (no weather trades) and raises a critical alert.
**TESTING.** Smoothing shrinks sparse cells, tail handling, serialisation,
and the bucket-bounds property test.

## Expected value and break-even (brief §21)

For a contract bought at price *P* with win probability *p*, per share:
`EV = p − P − fee(P) − slippage`, where `fee(P) = feeRate × P × (1 − P)`
(rounded up). The break-even probability is `p* = P + fee(P) + slippage`.
**KNOWN FACT (arithmetic):** near 0.99 the required accuracy is extreme.

| Price | Fee/share (5 %) | Break-even p | Wins to recover one loss |
|---|---|---|---|
| 0.90 | 0.00450 | 90.45 % | 9.5 |
| 0.95 | 0.00238 | 95.24 % | 20.0 |
| 0.97 | 0.00146 | 97.15 % | 34.0 |
| 0.99 | 0.00050 | 99.05 % | 104.2 |

(`weather-machine ev-table` prints the full 0.90…0.99 grid; the dashboard
shows it permanently.) Performance is judged on EV, drawdown and confidence
intervals, never on win rate.

## Which strategies run (1 October 2026)

| | strategy | live | why |
|---|---|---|---|
| A | BUY YES on the final high (§25) | off | no trade on 30 Sep–1 Oct; the live A/B rule made 6 trades, −$5.95, in the 122-day replay |
| B | BUY NO above the high (§26) | off | as A; buying NO above the high lost at every replayed setting |
| C | split + unwind (§27) | off | research only |
| D | decided outcomes (§27a) | off | dead buckets reprice a median 39 s after the observation, before the bot knows the report |
| E | book-confirmed high (§27c) | off | −$7.18 over 80 replayed trades |
| F | peak slot (§27d) | **on** | traded; slot 75 % → 95 % since 1 Oct |
| G | tail seller (§27e) | **on** | new |
| H | next degree (§27f) | **on** | new |
| I | middle fade (§27g) | **on** | new |
| J | morning maker (§27h) | **on** | new |
| K | KNMI nowcast (§27i) | **on**, idle without `WM_KNMI_API_KEY` | new |
| U | unwind (§28) | **on** | the exit engine; leaves G–K alone |

A strategy switched off is not shown on the dashboard; its code stays,
because `research market` replays A, B, E and F as baselines, and
`enabled = true` brings it back. G–K were chosen on 1 October 2026 from the
122-day replay of the settled Amsterdam markets at traded prices
(June–September 2026; the tables are in `research market`'s report) and the
prediction-market literature ([edge research
§9](../research/edge-research.md#9-october-2026-five-new-strategies)). Every
one is a **HYPOTHESIS TO BACKTEST**: `research market` replays each with
variants and judges the best variant of each on the second half of the
days, out of sample.

## 25. BUY YES — strategy A

**PURPOSE.** Buy YES of the bucket containing the observed high when the
model says it is very likely final and the price leaves positive EV.
**INPUTS.** `StrategyContext { now, mode, market, books, views (all
resolution views), positions, pending_tokens }`.
**OUTPUTS.** `Proposal`s plus a `BucketEvaluation` for every bucket (signal
or blockers, shown on the dashboard).

Conditions, all required:
* every view evaluable and agreeing on the high;
* confirmation ≥ `min_confirmation_minutes` (60);
* model support ≥ 50;
* data age ≤ 40 min;
* book fresh (≤ 15 s) with an ask in [`min_price`, `max_price`] = [0.90, 0.99];
* p_win = **minimum across views** of the lower-bound bucket probability,
  pooled with the market (§27b);
* EV − `min_edge` (0.01) > 0 after fee and slippage (0.005);
* not already positioned or pending;
* size ≥ the market minimum.

Orders are FAK at the ask, sized `$10 / price` rounded down to the lot.

**HYPOTHESIS TO BACKTEST.** Each threshold 0.90…0.99 separately, and each
confirmation window. The code makes all of them configuration, not constants.
**FAILURE MODES.** Any missing input becomes a named blocker. Risk (§29)
re-checks everything independently.
**TESTING.** `wm-strategy/tests/strategies.rs`, plus kernel lifecycle and
approval tests.

## 26. BUY NO — strategy B

**PURPOSE.** Buy NO of buckets *above* the observed high (high + 1, + 2,
+ 3 …) when the heating cycle appears finished.

The conditions mirror A. The loss probability is the **maximum across views**
of the upper-bound bucket probability (its complement is pooled with the
market, §27b), and distances are configurable (`distances = [1, 2, 3]`), so
each is backtested separately.
**HYPOTHESIS TO BACKTEST.** NO(+1) versus NO(+2/+3) EV after fees. Deep NO
contracts are priced near 0.99, where one loss erases about 100 wins.
**TESTING.** As for A. `paper_lifecycle_signal_risk_fill_settle` approves
YES 18 plus NO 19 and NO 20.

## 27. SPLIT + UNWIND — strategy C

**PURPOSE.** Research whether two-sided exposure *before* confirmation beats
waiting (brief §17).

* `SplitUnwind` (`research_only = true`, **disabled** by default) acts
  *before* confirmation (< 30 min since the high). It buys YES of the bucket
  containing the current high and YES of the next bucket up (the two
  outcomes still plausible), when their combined probability is ≥ 0.90 and
  their combined ask is ≤ 0.95, with $5 per leg. The `UnwindEngine` later
  sells the leg the evidence turns against.
* **Mechanics (§18 of 05-markets.md).** With a consistent book, split-and-sell
  equals a direct buy plus the sale fee, so any advantage must come from
  timing or liquidity. That is why C is compared *directly* against
  "wait → confirm → directional entry" in the backtest.
* The risk engine rejects research-only strategies outside backtest mode
  (`CheckId::ResearchOnly`).

**HYPOTHESIS TO BACKTEST.** C has higher EV than A+B net of both legs' costs.
Do not assume it.

## 27a. DECIDED OUTCOMES — strategy D

**PURPOSE.** Trade the only case in which the answer is certain. The daily
high can only rise. Once *every* resolution view has seen a high of `H`
whole degrees:

* a bucket entirely below `H` cannot win, so its **NO** wins;
* an open-ended top bucket "≥ L" with `L ≤ H` has won, so its **YES** wins.

No model is involved (`p_win = 1`). The edge is speed. Right after a report
raises the high, quotes on the bucket that just died can still be stale.
D (`CertainOutcomes`, `wm-strategy::certain`) evaluates on the observation
event itself; A and B need a confirmation window.

Conditions, all required (`[strategies.certain]`):

* the lowest high across views decides, so every view must have seen it;
* a high more than `max_jump_tenths` (3 °C) above the report before it is
  trusted only once a later report repeats it (`high_jump_tenths`,
  `retests`);
* data age ≤ 40 min; book fresh (≤ 15 s); ask ≤ `max_price` (0.99);
* profit per share ≥ `min_edge` (0.02) after the taker fee and a 0.002
  slippage allowance. The default skips long-dead buckets quoted at
  0.98–0.99: a settlement discount of well under a cent per share is not
  worth the resolution risk, nor the daily exposure budget ($60) it would
  use up;
* not already positioned or pending; market accepting orders; size ≥ the
  market minimum.

The risk engine applies all its gates, D included: correction cooldown,
spread ≤ 0.05, no one-sided book, depth, exposure. The market's midpoint
is recorded next to each evaluation but not used: a stale quote on a
decided outcome is the opportunity, not a warning.

**RESIDUAL RISK.** An erroneous report that is corrected later (the jump
guard and the correction cooldown cover most of it), and a resolution
source that differs from the METAR high. `research market` counts how often
the METAR high matched the resolved bucket.
**TESTING.** Five `strategy_d_*` tests in `wm-strategy/tests/strategies.rs`
(NO below the high; top bucket YES; jump guard; views must agree; books,
positions, data age, edge). Kernel tests `a_new_high_is_traded_on_the_observation_event_itself`
(including an unconfirmed quiet book) and
`a_corrected_high_blocks_certain_outcome_trades`.

## 27b. The book as information — market pooling (A and B)

**PURPOSE.** On these contracts the market predicts better than public
forecasts ([research](../research/edge-research.md)). When it disagrees with
the model, the model is more likely wrong.

`Pooling { weight, max_spread, max_book_age_ms }`:

* **Market probability.** The midpoint of the traded token's own book if it
  is fresh, two-sided, not crossed and its spread is ≤ `max_market_spread`
  (0.10); otherwise one minus the complementary token's midpoint; otherwise
  none.
* **Pool.** `logit p = w·logit(market) + (1 − w)·logit(model)` with
  `w = market_weight` (default 0.5; 0 = model only), **capped at the
  model's probability**. The market can veto a trade but never create one.
  With consistent books the cap is a no-op, because the midpoint is at or
  below the ask.
* **No check, no trade.** With `market_weight` > 0, a book that gives no
  market probability (spread above `max_market_spread`, one-sided or
  crossed, on the token and its complement) blocks the trade: "spread 0.20 >
  0.10: too wide to check the model", "no bid: …", "crossed book: …". The
  model alone used to decide then, but it is the weaker opinion: live on
  1 Oct it priced 21 °C at about 6 % while the book bid NO only 0.74–0.81,
  and the temperature came back to within a degree of it by midday. The
  risk engine already rejects spreads above its 0.05 limit and one-sided
  books, so in practice only crossed books trade differently; the rule
  keeps a wider risk limit from letting the model trade alone and ends the
  proposal bursts on wide books (563 rejections in half a second on
  1 Oct). Weight 0 stays model only. A missing, stale or ask-less book is
  already blocked and gets no second blocker. The replay applies the same
  rule (taker and maker).
* The pooled probability drives EV, break-even and the edge blocker. The
  blocker names the market when it pulled the probability down. The
  rationale records model, market and the result. The dashboard ladder
  shows the probability used ("used") and computes EVs from it.

**HYPOTHESIS TO BACKTEST.** Which weight predicts best at EHAM. `weather-machine research market`
scores w ∈ {0, 0.25, 0.5, 0.75, 1} on settled markets (§36a). If the market
alone is best, A and B have no information edge. With w = 1 they
effectively stop trading, because the midpoint never clears its own ask.
**TESTING.** `log_pool_averages_log_odds`,
`pooled_win_probability_never_exceeds_the_model`,
`market_probability_needs_a_fresh_tight_two_sided_book`,
`strategy_a_lets_the_market_veto_but_not_create_a_trade` (also: no trade on a
wide, one-sided or crossed book unless the complement is tight or the weight
is 0), `strategy_b_pools_each_bucket_with_its_own_book`,
`a_and_b_replays_need_the_market_to_check_the_model`,
`the_a_maker_replay_needs_the_market_to_check_the_model`, plus configuration
defaults and validation tests.

## 27c. BOOK-CONFIRMED HIGH — strategy E

**PURPOSE.** The operator's rule: between two local times, once the
temperature has peaked and the order book of the high's bucket is
shrinking, buy YES on that bucket at 0.90–0.99. `BookConfirmedHigh`
(`wm-strategy::book_confirmed`), `[strategies.book_confirmed]`.

**STATUS.** Off in the shipped configuration since 1 Oct 2026. In the
120-day replay it won 76 of 79 trades and still lost $7.72; live (29 Sep –
1 Oct) it never traded, because its conditions never held while the ask was
still in range; and its losses would count toward the daily loss limit
that strategy F's 100-share trades need. A disabled strategy is neither
evaluated nor fed books, so its page shows only its history; `research
market` still replays all three variants (none marked live). The code
default stays on, so tests and older files keep the rule as designed; set
`enabled = true` to run it again.

Conditions, all required:

* the local time is inside `[start_local_minute, end_local_minute)`
  (12:00–18:00);
* the high was first reported ≥ `min_minutes_at_high` (60 min) before the
  latest report, and that report is ≥ `min_drop_tenths` (1.0 °C) below it.
  The clock runs from the first report at the high, so a flat top or a
  retest does not restart it; the lowest value across views counts;
* **the book is shrinking**: the YES shares offered at or below `max_price`
  fell by ≥ `min_depth_shrink` (30 %) over `lookback_minutes` (30), from at
  least `min_depth_shares` (50), while the best ask did not fall. Buyers
  taking the offers or sellers withdrawing them count; sellers undercutting
  each other do not. The engine keeps the best five levels per side
  (`ENGINE_BOOK_DEPTH`), so a level entering or leaving those five is not a
  change of the book: two states are compared only up to the highest price
  both show (and at most `max_price`);
* the ask is within `[min_price, max_price]` (0.90–0.99); book fresh
  (≤ 15 s); weather data ≤ 40 min old;
* a probability model is loaded (fail closed, as for A and B). With
  `min_model_p` > 0 its probability for the bucket is also a veto (default
  0: no veto);
* not already positioned or pending; market accepting orders; size ≥ the
  market minimum.

**BOOK HISTORY.** Every book update the engine receives reaches the
strategy (`Strategy::observe_book`), also while the views are incomplete
and nothing is evaluated. States are coalesced to one per 10 s (the latest
book of a slot wins) and kept for the lookback; tokens without an update for
a day are forgotten. After a restart the history is empty, and E waits one
lookback ("book history shorter than 30m").

E claims no model edge. The evaluation shows the model pooled with the
market (as for A) and the EV at that probability, but neither is a gate.
The risk engine applies all its gates: weather health, correction cooldown,
spread ≤ 0.05, no one-sided book, depth, exposure and the per-strategy
capital cap. E is evaluated last, so when A and E signal on the same token
A's order goes first and E's is rejected as a duplicate.

**HYPOTHESIS TO BACKTEST.** The premise is the favourite–longshot bias:
late favourites win slightly more often than their price says. The margin
is about the size of fee plus slippage (break-even 0.957 at an ask of 0.95,
0.9955 at 0.99), and some studies find the opposite for weather at short
horizons ([evidence](../research/edge-research.md#6-strategy-e--a-late-favourite-confirmed-by-the-book)).
`research market` replays E at traded prices (§36a). The book is not
archived, so there a trade-flow stand-in replaces the book condition. Three
variants run: as configured, without the book condition, and with the
model ≥ 0.90 as well.

**RESIDUAL RISK.** A second peak after the drop (warm advection, clearing
cloud): the window and the drop reduce this risk but do not remove it. An
offer pulled and re-posted can mimic buying. The resolution source may
differ from the METAR high. One loss at 0.95 costs as much as about 22 wins.
**TESTING.** Unit tests in `book_confirmed.rs` cover:

* depth under the cap;
* coalescing and pruning;
* the visible-range comparison ignoring a truncation artifact;
* the held ask.

Four `strategy_e_*` tests in `wm-strategy/tests/strategies.rs` cover:

* the signal;
* every blocker;
* the model requirement and veto;
* the history fed between evaluations.

The kernel test `strategy_e_buys_the_high_when_its_book_shrinks_after_the_peak`
fails without `observe_book` and checks that the no-edge model does not
trade. Replay tests `strategy_e_is_replayed_with_its_book_stand_in` and
`taker_flow_counts_the_lookback_and_the_ask_it_began_with` cover the
research side, and further tests cover configuration mapping and
validation.

## 27d. PEAK SLOT — strategy F

**PURPOSE.** The operator's rule: learn when the day's highest temperature
is measured, which differs per season; inside that time slot, watch the
order book of the bucket holding the day's high, and once it is offered
above 0.90 buy 100 shares of its YES. `PeakSlotHigh`
(`wm-strategy::peak_slot`), `[strategies.peak_slot]`; the peak times are
`PeakTimes` (`wm-strategy::peak_times`).

**PEAK TIMES.** Training (`model train`, and the automatic training of
`run`) walks the METAR history and records, per meteorological season
(DJF / MAM / JJA / SON, hemisphere from `[peak]`), the local minute of the
**first report at each day's whole-degree high**. From that report on, the
bucket holding the high wins unless a later report beats it. Only days whose
reports cover them count: the first within 90 min of midnight, the last
within 90 min of the day's end, no gap over 120 min. Others are counted as
incomplete. The histogram is stored in the model file (`peak_times`) and
printed in the training report and in `research market`. The printout
shows:

* the mean and the median (the "average time");
* the 10 / 25 / 75 / 90 % quantiles;
* the slot;
* the share of days whose high came after the slot, after 17:00, or before
  09:00.

Night maxima (winter warm fronts) pull the mean earlier, so the slot is
built from quantiles. A model file without peak times is retrained once,
automatically. Operator-supplied files (`retrain_existing = false`) are
never retrained, and F then uses its fallback slots.

**KNOWN FACT (KMI/IRM).** In Belgium maxima are typically reached around
14:30 UTC all year, 2–3 hours after solar noon: ≈ 16:30 local in summer,
≈ 15:30 in winter. In winter the day's highest temperature can come at
night, after a warm front
([KMI, maximumtemperatuur](https://www.meteo.be/nl/info/weerwoorden/maximumtemperatuur)).
The lag after noon varies between about 1.9 and 2.9 h with latitude and
season
([diurnal temperature variation](https://en.wikipedia.org/wiki/Diurnal_temperature_variation)).
The first report at the *whole-degree* high usually comes somewhat earlier
than the true maximum, which is why the station's own history sets the
slot.

**SLOT.** From quantile `slot_from_quantile` to `slot_to_quantile` of the
season's peak times, end exclusive. The code's defaults are the median and
the 90th percentile, `[q50, q90 + 1 min)`. The shipped configuration uses
**0.75 → 0.95** since 1 October 2026: in the 122-day replay at traded
prices the median → 90 % slot lost $122.00 over 84 trades and the
75 % → 95 % slot made $11.13 over 80 (+$0.14 per trade, 95 % CI
−$5.93 … +$5.14). That difference is within noise and was chosen after
seeing the data, so it is a hypothesis for the coming days, not an edge;
`research market` keeps replaying the median slot beside it. Until an
installed model carries peak times, `fallback_slots` apply:

| season | fallback slot |
|---|---|
| winter | 13:00–16:00 |
| spring | 14:30–17:30 |
| summer | 15:00–18:00 |
| autumn | 14:00–17:00 |

Every evaluation and proposal names the slot and its source, e.g.
`16:20 local inside the summer slot 15:55–17:26 (75% → 95% of 412 days' peak times)`.

**CONDITIONS**, all required. F is evaluated on every weather update and on
every order-book update, so the book is watched continuously.

* The local time is inside the current season's slot.
* The bucket holding **the day's high so far** is priced right. This is how
  "the current measured temperature" is read: a bucket below the high has
  already lost.
  * Its best ask is **above** `min_price` (0.90, exclusive).
  * All `shares` (100) are offered at or below `max_price` (0.95). The
    order's limit is the price at which 100 shares fill walking the asks
    (`sweep_price`), fill-and-kill.
* Optionally, the latest report is ≥ `min_drop_tenths` below the high
  (default 0 = off).
* The weather is ≤ 40 min old and the book ≤ 15 s old.
* A probability model is loaded (fail closed). F claims no model edge: the
  evaluation shows the pooled probability and the EV, but neither is a gate.
* F holds no position and no order on the token.
* The market accepts orders, and 100 shares is at least the market minimum.

**RISK.** 100 shares at up to 0.95 is $95, above the $10 position size.
`[risk.strategy_caps.F_peak_slot]` gives F its own caps: $100 per position,
$110 per market and $110 per strategy. The portfolio caps were raised by
one F position (§30). The per-market cap counts every strategy's legs, so
after F buys, A, B and E add nothing more to that event. One F loss trips
the $30 daily-loss stop for the rest of that UTC day. In practice F holds
one position a day.

**THE PREMISE IS NOT A FACT.** "After the slot the temperature cannot go
higher" is false by construction: one day in ten (the slot's upper
quantile) first reports its high after the slot. Inside the slot, a later
report can also still beat the high. What F needs is that the market
underprices the high's bucket inside the slot by more than fee and
slippage.

* At 0.95 the taker fee is 0.05 × p × (1 − p) = 0.24 ¢, so with 0.005
  slippage the bucket must win **95.7 %** of the time.
* One loss (−$94 to −$96) costs as much as about 15 wins at 0.93 (+$6.17
  each) or 22 wins at 0.95 (+$4.26 each).

**Evidence** ([edge research §6a](../research/edge-research.md#6a-strategy-f--the-highs-bucket-inside-the-peak-slot)):

* EHAM buckets priced 0.90–0.98 won 96.8 % of the time at a mean price of
  0.949.
* E, which bought 0.90–0.99 later in the day, won 96.2 % at a mean of
  0.964 and lost 1 % per trade.
* F's cap at 0.95 keeps it where the favourite discount was measured.

HYPOTHESIS TO BACKTEST: `research market` replays F, five variants and a
maker version at traded prices, and picks among them out of sample
(§36a).

**RESIDUAL RISK.**

* A second peak (warm advection, clearing cloud), or a night maximum in
  winter.
* A METAR high that is not the resolution value.
* 100 shares may not be offered at ≤ 0.95. F then waits, and the evaluation
  says how many were.

**TESTING.**

* `peak_times.rs`: first reach, the coverage rules, quantiles, mean, slot,
  the report table, serialization.
* `peak_slot.rs`: the sweep price, and the slot source (history or
  fallback).
* `strategy_f_*` in `wm-strategy/tests/strategies.rs`: the signal, the
  learned slot, every blocker, no second position.
* The kernel scenario
  `strategy_f_buys_100_shares_of_the_high_inside_the_learned_slot`:
  approved with F's caps, rejected by the $10 cap without them, and blocked
  outside a later slot.
* The risk tests for `strategy_caps`, and configuration validation
  (quantiles, prices, shares, slot windows, and every cap one F position
  must fit).
* The training test asserting the peak times in the model and the report.
* The retrain-once test.
* The replay tests (§36a).

## 27e. TAIL SELLER — strategy G

**PURPOSE.** Sell the overpriced far tails to the takers who buy them:
rest a NO bid (an offer of the YES) on buckets two or more degrees above the
day's high.

**EVIDENCE.**
* **LITERATURE.** Prediction markets show a favourite–longshot bias: on
  Kalshi contracts under 10¢ lose over 60 % of the stake, and makers earn
  more than takers at every price (Bürgi, Deng & Whelan, *Makers and
  Takers*, 2025/26); on Polymarket buys under 10¢ lose about 19¢ per
  dollar.
* **REPLAY (122 days).** YES priced 0.00–0.02 won 0.1 % of the time at a
  mean price of 0.4 %. The resting orders filled by takers who paid
  0.00–0.02 earned +0.34¢ a share net (95 % CI +0.26 … +0.41); on buckets
  two above the high +1.04¢ (−0.06 … +2.27), three or more above +0.41¢.
  YES at 0.02–0.10 was about fair (5.6 % won at 5.0 %), hence the 8¢ cap.

**RULE** (`[strategies.tail_seller]`). For every bucket whose lowest value is
≥ high + `min_distance` (2), between 10:00 and 21:00 local:
* the order is a NO bid one tick above the best NO bid (at it when the
  spread is one tick): the YES offered is one minus that bid and must lie in
  [`min_yes_price`, `max_yes_price`] = [0.01, 0.08];
* the model's upper-bound probability for the bucket is at most
  `max_model_ratio` (1.0) × that YES price; no model, no order;
* $30 an order (`notional`), good till 10 minutes before the next routine
  report (`cancel_before_report_minutes`), not posted with less than 3
  minutes left; posted again after the report;
* one position per bucket and day, held to settlement.

**RISK.** A bucket that wins costs the whole NO price (12–100 × the
premium). The edge is a fraction of a cent per dollar a day: a small, steady
earner whose result rests on the rare loss. Caps: $30 an order, $120 in
all.

## 27f. NEXT DEGREE — strategy H

**PURPOSE.** Buy YES on the bucket one degree above the high while the day
can still warm.

**EVIDENCE (replay).** On the bucket one above the high, takers gained
+0.78¢ a share before fees and the resting orders against them lost 0.68¢
(95 % CI −1.27 … −0.10). Strategy B, which bought the NO of the buckets above
the high, lost at every replayed setting (−$92.74 over 129 trades at NO
asks 0.70–0.99). YES at 0.10–0.30 won 21.0 % at a mean price of 18.7 %; when
the model was ≥ 5 points above the market, 13.8 % at 11.9 % (95 % CI
12.3 … 15.5 %).

**RULE** (`[strategies.next_degree]`). From 10:00 local until the season's
75 % peak time (`until_quantile`; 15:30 until the model carries peak
times), while the latest report is within 1.0 °C of the high: the YES of the
bucket holding high + 1 (not the high's own bucket), ask in [0.05, 0.35],
the model's probability ≥ `min_model_ratio` (1.0) × the ask, $10 at the
price that fills the whole size (never more than $10), fill-and-kill, held
to settlement.

**RISK.** It loses its stake whenever the day does not warm one more degree
— most of the time. A few 3–20× winners must pay for many small losses.

## 27g. MIDDLE FADE — strategy I

**PURPOSE.** Buy NO on the overpriced middle of the ladder.

**EVIDENCE (replay).** Buckets whose YES traded at 0.30–0.70 won 45.4 % at a
mean price of 48.5 % (95 % CI 42.8 … 48.0 %, 1,392 decision points). With
the model ≥ 5 points below the market: 51.1 % at 53.6 % (49.0 … 53.3 %). The
prices are trade midpoints (latest taker buy and sell within 60 min), which
can be stale on a falling bucket; the replay of I therefore uses quotes at
most 10 minutes old.

**RULE** (`[strategies.middle_fade]`). From 09:00 to 18:00 local, for every
live bucket whose fresh YES book (spread ≤ 0.04) has its midpoint in [0.30,
0.70] and whose upper-bound model probability is ≥ 0.05 below that
midpoint: buy the NO when its EV is ≥ `min_edge` (0), with p(NO) = 1 −
(midpoint − `calibration_bias` 0.03), never above the model's own NO
probability; $10, fill-and-kill, held to settlement.

**RISK.** Three points is about what the fee (≈ 1.25¢ at 0.50), half the
spread and slippage cost: I trades only on tight books, and each trade is a
coin flip with a slight tilt.

## 27h. MORNING MAKER — strategy J

**PURPOSE.** Provide liquidity in the low-information morning hours and
earn the spread.

**EVIDENCE (replay).** Resting orders earned +0.71¢ a share net in the
evening before the day, +0.61¢ from 00:00 to 09:00 and +0.47¢ from 09:00 to
12:00 (each interval spans zero; together about $14,300 for the market's
makers over 122 days). They lost 0.59¢ from 12:00 to 15:00, 0.49¢ in the
last five minutes before a report and 1.34¢ on the high's bucket then
(−2.05 … −0.66).

**RULE** (`[strategies.morning_maker]`). From 00:00 to 11:00 local, for every
bucket whose fresh YES book has its midpoint in [0.10, 0.90] and a spread of
0.02–0.05: a YES bid and a NO bid, each one tick above the best bid on its
own book (at it when there is no room), $10 each, good till 10 minutes before
the next routine report and posted again after it. A filled side is held to
settlement; both sides filled pay $1 a pair whatever the weather, so J keeps
the spread. No model needed.

**RISK.** Inventory: a side that fills alone is a directional position at
the market's own price a moment ago. Caps: $10 a quote, $80 in all.

## 27i. KNMI NOWCAST — strategy K

**PURPOSE.** Be early to a new high with faster data than the METAR.

The market resolves on the METAR: a whole-degree reading every half hour
(EHAM: HH:25 and HH:55 UTC) that reaches the public feeds two to five
minutes later. KNMI publishes the same airport's automatic weather station
every ten minutes, to a tenth of a degree, a few minutes after each interval
— the reading of HH:10–HH:20 is out before the HH:25 METAR is observed. The
EDR API's `10-minute-in-situ-meteorological-observations` collection serves
it as CoverageJSON (`ta`: 10-minute mean, `tx`: maximum; Schiphol's WIGOS id
`0-20000-0-06240`) with a free API key in the `Authorization` header.

**EVIDENCE (replay).** 58 reports raised the high while the dead buckets
were still priced. In the ten minutes *before* those reports' observation
times takers sold the soon-dead buckets' YES for $3,330 of profit; on the
high's bucket in the last five minutes before any report takers gained
+1.39¢ a share before fees while the resting orders against them lost 1.34¢
(95 % CI −2.05 … −0.66). Someone trades on the weather before the METAR
shows it.

**RULE** (`[strategies.knmi_nowcast]`). When the latest ten-minute reading is
newer than the last METAR, at most 12 minutes old and ended at most 16
minutes before the report it anticipates — the first routine report after
its interval, so the 11:10–11:20 reading, arriving about when the 11:25
METAR is taken, still counts until that METAR is published — its mean is
≥ high + 0.5 + 0.3 °C (the
rounding edge plus `mean_margin_tenths`) and its maximum ≥ high + 0.5 °C:
buy the NO of the bucket holding the high (it dies the moment a report beats
it) when the NO ask is in [0.02, 0.75] and the EV at `p_new_high` (0.80) is
≥ 0.05 after fee and slippage; $25, fill-and-kill, held to settlement. A
bucket that also holds high + 1 is skipped. Its books may be up to 0.10 wide
(`[risk.strategy_caps.K_knmi_nowcast].max_spread`): makers widen their quotes
before a report.

**ASSUMPTION.** `p_new_high` = 0.80 until `research market` (run with the
key) measures how often the next METAR raised the high, by the ten-minute
mean before it — the table *KNMI's ten-minute mean before the METAR*. The
replay assumes a reading is known 5 minutes after its interval (variants: 2
and 8); the live logs (`KNMI ten-minute reading … delay_minutes`) and the
dashboard's *KNMI 10-minute* box measure the real delay.

**FAIL CLOSED.** No key, an API error or a stale reading: K does nothing,
and the dashboard says why. The readings never change the observed high,
the views or settlement.

**TESTING (G–K).**
* `wm-strategy/tests/new_strategies.rs`: each strategy's signal, the order
  it proposes (price, size, time in force) and its blockers; H never pays
  more than its notional; the unwind engine exits F's position but not
  G–K's.
* `wm-strategy/src/quoting.rs`: the report schedule, the quote expiry, the
  passive bid.
* `wm-backtest/tests/maker_session.rs`: J in the shared session loop —
  quotes, expiry, the daily new exposure given back, quotes again after the
  report, a maker fill from a trade print, no second bid on a held side.
* `wm-backtest/tests/nowcast_session.rs`: K in the shared session loop — a
  reading older than the last METAR does nothing; one that arrives as the
  next METAR is taken buys the high's NO, which fills; a late, older
  reading neither replaces the newer one nor buys again.
* The demo (`weather-machine demo`) feeds synthetic ten-minute readings of
  its own temperature curve, labelled *synthetic*, so K and its dashboard
  box can be watched without a key.
* `wm-backtest/src/market_gk.rs`: the replay of each strategy at traded
  prices and K's accuracy table; the market report's G–K section and its
  JSON compatibility.
* `wm-weather` (`knmi.rs`, `tests/knmi.rs`): CoverageJSON parsing, the
  request (path, period, parameters, key in the header) and a refused key;
  `wm-net`: the key is sent as a header and never printed.
* `wm-app`: configuration validation of G–K (ranges, windows, caps each
  order must fit), the shipped values, `research market` with a KNMI mock
  (download, weekly cache, report section).

## 28. UnwindEngine

**PURPOSE.** Exit positions the evidence has turned against, with explicit
execution style.

```rust
pub enum UnwindStyle {
    Marketable { floor: Price },                                  // walk the bids (FAK)
    BestBid,                                                      // FAK at best bid (default)
    Passive { offset: Price },                                    // GTC maker above bid
    Progressive { start_offset: Price, step: Price, step_secs: i64 },
}
pub struct UnwindConfig { enabled, style, exit_below_probability /*0.50*/, max_hold_minutes }
```

* Signal-based: exit when the position's p_win drops below
  `exit_below_probability`. Time-based: `max_hold_minutes`.
  Probability-based: the same p_win as entry, across views.
* `exempt_strategies` (default: G–K) are left alone: they buy outcomes the
  model rates below the exit level by design — a cheap YES, the NO of a
  middle bucket, a quote's inventory — and hold them to settlement.
* Unwinds are *reduce* intents. They are allowed even when weather data is
  unhealthy, because reducing risk is never blocked by the weather gates.
* Never assumes a mid-price fill: fills come from the book (§35).

**HYPOTHESIS TO BACKTEST.** Which style (marketable, best-bid, passive or
progressive) minimises the total cost of exits, including spread, partial
fills, slippage, holding risk and gas.
**TESTING.** `unwind_exits_yes_when_high_breaks_and_holds_certain_no` and `progressive_unwind_steps_toward_bid` (`wm-strategy/tests/strategies.rs`).
