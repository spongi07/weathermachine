# 22–28 · Peak detection, probability, strategies A/B/C, unwind (+ EV)

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
* Training: `weather-machine research peak-survival --csv <IEM export>
  --model-out model.json`. The model is JSON, versioned by id, station and
  view, with training dates.

**FAILURE MODES.** No model or no distribution → no signal.
Station/model mismatch → startup error.
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
* p_win = **minimum across views** of the lower-bound bucket probability;
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
of the upper-bound bucket probability, and distances are configurable
(`distances = [1, 2, 3]`), so each is backtested separately.
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
