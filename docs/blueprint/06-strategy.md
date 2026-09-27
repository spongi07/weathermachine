# 22–28 · Peak detection, probability, strategies A/B/C/D, market pooling, unwind (+ EV)

> **The core trading belief is a HYPOTHESIS TO BACKTEST, not a fact:** once
> the day's high has been reached and not retested for N minutes, it becomes
> increasingly likely to be final. The code never assumes it. The
> probability of "final" comes only from a model trained on history
> (`EmpiricalPeakModel`). Without such a model the `NoEdgeModel` returns
> nothing, and strategies A and B cannot propose any trade.

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
`strategy_a_lets_the_market_veto_but_not_create_a_trade`,
`strategy_b_pools_each_bucket_with_its_own_book`, plus configuration defaults
and validation tests.

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
* Unwinds are *reduce* intents. They are allowed even when weather data is
  unhealthy, because reducing risk is never blocked by the weather gates.
* Never assumes a mid-price fill: fills come from the book (§35).

**HYPOTHESIS TO BACKTEST.** Which style (marketable, best-bid, passive or
progressive) minimises the total cost of exits, including spread, partial
fills, slippage, holding risk and gas.
**TESTING.** `unwind_exits_yes_when_high_breaks_and_holds_certain_no` and `progressive_unwind_steps_toward_bid` (`wm-strategy/tests/strategies.rs`).
