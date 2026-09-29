# 32–37 · Historical data, replay, backtest, simulation, research, overfitting

## 32. Historical-data audit

Status legend: **AVAILABLE**, **PARTIAL**, **UNAVAILABLE**. Everything here is
**ASSUMPTION** until the listed verification has run, because the build
environment had no egress to these hosts. Never fabricate what is missing:
backtests label their data fidelity (§34), and synthetic data is never
evidence.

| Dataset | Status | Source | Retention / granularity | Timestamp semantics | Limits / limitations | Verification |
|---|---|---|---|---|---|---|
| EHAM METAR history | AVAILABLE | IEM ASOS/METAR archive, network `NL__ASOS` (CSV `station,valid,metar`) | Multi-year; every METAR/SPECI | `valid` = observation time (UTC). **Publication time absent** → knowledge time modelled as observation + delay (`--publication-delay-min`, default 5) | Mirror, not the resolution source; gaps possible; corrections usually only final versions | Download once, store locally; `import_iem_csv` reports every skipped row |
| EHAM recent METARs | PARTIAL | AWC Data API (`hours` parameter) | Recent window only | `obsTime` (observation), `receiptTime` (receipt at AWC) | Documented 100 req/min; we use ≪ 1/min | `collect --once` |
| NCEI ISD Global Hourly (06240099999) | PARTIAL | NCEI | Long archive; hourly-ish synoptic + METAR | Observation time | Processed dataset with days of latency; cross-check only | Manual download |
| KNMI station observations | PARTIAL | KNMI Data Platform (API key) | 10-minute in-situ data | Observation time | Different measurement than METAR (not the settlement values of NOAA markets) | Phase 11 |
| WRH / Synoptic time series | PARTIAL | Synoptic API (own token) | Depends on plan | Observation time as displayed by WRH | Token required; the WRH page's token is not ours | Only if AWC ≠ WRH (§8) |
| Polymarket historical markets + rules | AVAILABLE | Gamma (closed events) | All listed events | Creation / end dates | Rules text may differ between days (KNOWN FACT for Amsterdam) → store per market | `markets discover --date` |
| Historical prices | PARTIAL | CLOB `/prices-history` | Price points at a chosen fidelity (minutes) | Point time | No depth, no spread → **approximate price backtest only** | Research phase 5 |
| Historical trades | PARTIAL | Polymarket data API / on-chain fills | Trade prints | Trade time | Trades imply prices, not resting liquidity | Phase 5 |
| Historical order books | **UNAVAILABLE** (venue) | Reported: the order-book-history endpoint stopped producing snapshots around Feb 2026 | — | — | True order-book backtests need **our own recorder** (enabled: `record_orderbooks`) or a commercial archive | From deployment onwards |
| Final resolutions | AVAILABLE | Gamma closed markets (resolved outcome prices) | Per market | Resolution time | Occasional disputes (UMA) must be respected | Phase 5 |
| Historical forecasts | YES (fixed lead) | Open-Meteo Previous Runs API: `temperature_2m_previous_day1` = value of the run initialised 24 h before valid time; GFS 2 m temperature since 2021-03, most models since 2024-01 | Hourly | Same product live and in history ⇒ no look-ahead. **Not** the "Historical Forecast API": stitched from the first hours of each run, i.e. a same-day analysis that leaks the outcome (0.80 °C MAE at EHAM vs ~2 °C persistence in the reviewed bot's data) | Free tier non-commercial; subscription for commercial use | Phase 11 ✅ (evaluated automatically, §19) |

## 33. ReplayEngine

**PURPOSE.** Feed the *same* event types to the *same* kernel in knowledge-time order.

```rust
pub struct SimulationSession { .. }   // Engine + SimulatedExchange + knowledge-time queue
impl SimulationSession {
    pub fn new(cfg: EngineConfig, sim: SimConfig, settle_grace: Duration, model: Arc<dyn ProbabilityModel>) -> Self;
    pub fn push(&mut self, env: EventEnvelope);                 // also schedules market settlement checks
    pub fn run_until(&mut self, t: DateTime<Utc>) -> SessionOutput;
    pub fn run_all(&mut self) -> SessionOutput;
    pub fn next_due(&self) -> Option<DateTime<Utc>>;
}
```

The session is used by backtests, demo mode **and** the live paper runtime:
one loop, three drivers. Inputs come from synthetic days, IEM imports, or a
recorded run's journal (`weather-machine backtest --journal <run-id>`, with
execution re-simulated).

**KNOWN FACT (tests).** Events are released by `(available_at, priority,
insertion)`; the clock never runs backwards (debug assertion). The
**prefix-stability test** runs a backtest on days 1…N and on 1…N+k, then
asserts the decisions of days 1…N are identical, so no future event
influenced a past decision.

**FAILURE MODES.** Knowledge time must be modelled for imported history (IEM
has no publication time). An unrealistically short delay would leak future
information, so the default is conservative and the delay is a reported
parameter.

## 34. BacktestEngine

**PURPOSE.** Measure strategies with honest execution and honest statistics.
`run_backtest(inputs, &BacktestConfig, model) -> (BacktestReport, Engine)`.

`BacktestReport`: fidelity, events, decisions, approvals, fills, trades (with
fees), settled markets, realized PnL, daily PnL, winning/losing days, max
drawdown, **95 % bootstrap CI of mean daily PnL** (seeded), PnL by strategy,
alerts, and the full decision log (approved and rejected, with reasons).

**Fidelity labels (KNOWN FACT, enforced in the report).**
`TrueOrderBook` (recorded books), `TradesOnly`, `PriceOnly` (approximate price
backtest) and `Synthetic` (plumbing only, **not evidence**). The CLI prints the
label, and journal backtests choose `TrueOrderBook` only when recorded books
exist.

Settlement uses the day's final value under the market's primary view,
after local midnight plus a grace period.
**TESTING.** `synthetic_backtest_runs_end_to_end_and_is_deterministic`
(20 days, all settle, determinism, consistent accounting) and
`no_look_ahead_decisions_are_prefix_stable`.

## 35. Execution simulator

**PURPOSE.** Fills that could have happened, never better.
`SimulatedExchange { submit, cancel, process_due, on_book, on_trade, expire }`
with `SimConfig { latency_ms: 250, adverse_ticks: 0 }`.
* **Latency.** An order becomes executable `latency_ms` after submission,
  against the book *at that time*.
* **Taker orders (FAK/FOK).** They walk the book level by level at prices at
  or better than the limit. Partial fills are allowed; FOK is all-or-none.
  **Never a mid-price fill.**
* **Maker orders (GTC/GTD).** They rest *behind* the displayed size at their
  price (queue position) and fill only from subsequent trade prints at or
  through the price, after the queue ahead is consumed. They expire at their
  GTD time.
* **Fees.** Exact per fill, rounded up; the average price and fees are tracked
  per order.
* **Pessimism knob.** `adverse_ticks` shifts the opposite side against us for
  robustness runs.
* Network costs (gas for redeem/split/merge) are modelled in the CTF
  economics; paper fills on the CLOB carry no gas.

**TESTING.** `wm-execution/tests/simulated_exchange.rs`:
`fak_fills_after_latency_against_current_book`,
`liquidity_can_vanish_during_latency`,
`fok_is_all_or_nothing_and_missing_book_rejects`,
`gtc_rests_then_fills_from_trades_after_queue`,
`gtc_crossed_by_book_fills_as_maker_and_gtd_expires` and
`invalid_transitions_and_fills_are_rejected`, plus fill-model property tests.

## 36. Parameter research

**Dimensions.** Season, month, local time, confirmation window, slope, drop
from high, retest count, trajectory class, forecast rise (day-1 forecast:
rest of day vs. so far, §19), forecast headroom (rest-of-day forecast maximum
minus the observed high), clock from the last or the first report at the
high (§36b), YES price threshold (0.90…0.99), NO price,
outcome distance (+1/+2/+3), split timing, unwind style/timing, exit price.

**First strategy experiment (brief §38).** Runs automatically when the
service first starts without a model (report:
`/data/research/eham-survival.md`, with the SHA-256 of every downloaded
year). Manual equivalents:
```text
weather-machine model train                      # download (cached) + train
weather-machine research peak-survival --csv eham_iem.csv --station EHAM \
    [--filter all|hourly-nws-faa|hourly-other] --model-out eham.json --report-out survival.md
```
The command produces **P(observed high is final | no higher observation for
N minutes)** for N ∈ {30, 45, 60, 75, 90, 105, 120, 150, 180}. It stratifies by
`season=`, `hour=`, `drop=` and `trajectory=`, each with sample counts and
95 % Wilson intervals, and trains the empirical model in the same pass. Order
of work, per the brief:
1. survival table on history;
2. does a forecast improve it (Phase 11) — answered automatically at every
   training by the prequential evaluation with a placebo control (§19); the
   report gains P(final) by forecast rise, calibration and a constant-price
   proxy, and the forecast is used only if adopted;
3. only then join with historical prices (Phases 8–10).

**Protocol.** Walk-forward splits (`walk_forward_splits(dates, train,
embargo, test)`): train on past days, skip an embargo, test on the next
block, and roll forward. No test day ever precedes its training data. Report
train, validation and out-of-sample results separately, where there are
enough days.

## 36a. Model versus market (`research market`)

**PURPOSE.** Measure, on EHAM's own settled markets, what the market knows
that the model does not, and the reverse. This is the evidence for
`market_weight` (§27b) and for strategy D (§27a), and the test of
strategies E (§27c) and F (§27d).

**INPUTS.**

* Settled events from Gamma: every bucket closed and exactly one resolved
  YES.
* Their taker trades from the Data API: all buckets in one query, from six
  hours before the local day to its end.
* The METAR history, and the forecast history when the installed model
  uses the forecast, from the training caches.

Settled days are cached under `research/polymarket/<STATION>/` and never
downloaded again. Wallets are stored as short hashes, used only to count
distinct traders.

A throttled or transiently failing request (429, 5xx, timeout) is tried up
to five times. The provider gate spaces the attempts by the server's
Retry-After and its own backoff, and slows down afterwards. A day that still
fails is listed as skipped and nothing of it is cached, so the next run asks
again. After three failed days in a row no further days are requested, and
the days downloaded so far are studied.

**METHOD (`wm-backtest::market_eval`).**

* **Prequential replay.** The history is replayed with the live state, peak
  and model code, and each day is scored with the model trained on the days
  before it.
* **Complete days only.** A market day whose METAR history ends more than
  90 min before local midnight is listed as skipped, not scored and not
  resolution-checked. The archive is read up to 00:00 UTC of the current
  day, so a run before then holds only the first hours of the day that
  just ended.
* **Decision points.** Every report from 09:00 local. The state is taken as
  of the report; the market as of the report plus the decision delay
  (`--delay-secs`, default 180).
* **Market probability.** The midpoint of the latest taker buy and taker
  sell of YES (NO trades converted), each at most 60 min old; dust trades
  (< 1 share) are ignored.
* **What is scored.** Every bucket that can still win, where the model's
  tail probability is unambiguous and its cell has enough support.
  Probabilities are clamped to [0.001, 0.999].
* **Both structures.** The candidate structure (§36b) is trained alongside,
  prequentially, and scored on the same decisions.
* **Strategies at traded prices (`wm-backtest::market_sim`).** A and B are
  replayed at every decision with each structure, for every confirmation
  window (0′, 30′ and the live one, counted from the last report at the
  high) and ask range (the live one and 0.70 to its maximum). YES ask = the
  latest taker buy of YES, NO ask = one minus the latest taker sell of YES;
  the order fills at that price plus the slippage allowance, the taker fee
  is paid, and the model is pooled with the traded midpoint and capped at
  the model, as live. At most one trade per day, bucket and variant.
* **Strategy E at traded prices.** E's clock, temperature and ask-range
  conditions are replayed at every decision with the live settings. The
  order book is not archived, so a stand-in built from the trades replaces
  the book condition. In the lookback before the decision, takers must
  have bought ≥ `min_depth_shares × min_depth_shrink` YES shares at or
  below the price cap (15 by default, as many as the live rule needs gone
  from the book), more than they sold, and the ask proxy must not have
  fallen. Three variants are replayed: *E* (as configured), *E w/o book*
  and *E + model* (model ≥ 0.90 as well). As live, E needs a model but no
  model edge.
* **Makers and takers (`wm-backtest::market_makers`).** Every trade of the
  studied days has a taker, who paid the fee, and a maker, who paid none
  and gets 25 % of it back as a rebate. The taker's P&L at settlement is
  the maker's before fees with the sign flipped. The study splits it by:
  * the price the taker paid;
  * the side;
  * the bucket against the METAR high published by then;
  * minutes to the next routine report (`routine_minutes`);
  * the high's bucket before a report;
  * local time.

  Each segment has a 95 % day-block interval. This is what the actual
  makers earned; nothing is simulated.
* **Maker versions of the live rules.** *A maker*, *B maker* and *E maker*
  post the live rules' orders as limit orders at the same gates:
  * a YES bid at the latest taker sell (A, E), or a NO bid at one minus the
    latest taker buy (B), never crossing the other side;
  * A's and B's edge is measured at that price, without fee or slippage;
  * an order counts as filled only when a later trade goes through its
    price (a trade at a worse price would have met it first);
  * orders are cancelled `cancel_before_report_min` (10) before the next
    routine report;
  * fills earn the rebate.
* **Strategy F at traded prices (`wm-backtest::market_peak`).** The peak
  times (§27d) are learned prequentially too: a market day is replayed with
  the slots of the days before it, and a season without such a day uses the
  fallback slot, as live. Live F watches the book between reports, so the
  replay follows the tape: at a report's decision time the latest taker buy
  of YES on the high's bucket stands for the ask, after it every taker buy
  does, until the next report is known or the report is older than F's
  data-age limit (40 min). The first price inside the slot and above
  `min_price`, at most `max_price`, fills F's `shares` (100) plus the
  slippage allowance and the taker fee; one trade a day, as the risk caps
  allow live. Six rules are replayed: *F* (as configured), *F · to 0.99*,
  *F · earlier slot* (25 → 75 %), *F · later slot* (75 → 95 %),
  *F · 1 °C below* (the report ≥ 1.0 °C below the high) and *F maker* (a
  YES bid at the latest taker sell inside the slot, filled only through
  its price, cancelled at the slot's end or 10 min before the next routine
  report). **Out of sample:** the rule with the best P&L on the first half
  of the replayed market days is judged on the second half (at least 20
  days); that line, not the best row, is the result.

**OUTPUTS** (`/data/research/<station>-market.md` and `.json`):

* log loss and Brier score of the model, the market and the pools, each
  with a 95 % day-block bootstrap interval of the difference to the market,
  overall and where A/B trade (market 0.90–0.99 or 0.01–0.10), plus the
  best weight;
* calibration by market price, and outcomes when the model disagreed with
  the market by ≥ 5 points;
* when the winning bucket first reached 90/95/99 % in the market vs in the
  model;
* for each report that raised the high: stale-quote trades on the killed
  buckets (YES sold, or NO bought, at ≥ 0.02 in YES terms) by delay after
  the observation. Profit is split into before and after the bot's
  decision time, with the number of distinct takers;
* agreement of the METAR high with the resolved bucket;
* the candidate structure against the market and against the current one;
* per variant: trades, wins, P&L per trade with a 95 % day-block interval,
  and the total at the live stake, with the live rule marked. Thirty
  variants are replayed (24 for A and B, 6 for E), so the best one
  overstates what to expect: a variant only counts if it stays profitable
  on days after it was chosen. E gets its own verdict line comparing the
  three variants, and its losing trades are listed with the bucket that
  resolved and the variants that took them. The maker versions add six
  rows (36 in all) and a verdict line;
* makers and takers: per segment, trades, shares, the taker's P&L and fee,
  and the maker's net with its interval and total, plus the best and worst
  segment for resting orders (≥ 2 % of the shares, on ≥ 5 days);
* when each season's high is first reported over the whole METAR history
  (the table of the training report), then strategy F's section: its six
  rules at **100 shares a trade** (apart from the $10 rows), its verdict
  line, the out-of-sample line (also in the main verdict) and its losing
  trades, with the time of fills from the tape;
* with `--day YYYY-MM-DD` (repeatable): that day report by report. It shows
  both clocks, the forecast rise and headroom, both structures' cells and
  P(high stays), the market and ask of the high's bucket, its taker flow
  over E's lookback (bought / sold, E's stand-in for the book), and every
  simulated trade;
* a plain-language verdict.

**LIMITS.** Trades show only liquidity someone took, which is a lower
bound on what was offered. The midpoint of the last taker buy and sell
approximates the book's midpoint. Simulated fills ignore depth, so they are
optimistic in thin markets. E's stand-in sees only offers that were taken:
offers withdrawn or added without a trade are invisible to it, while the
live rule reads the book itself. The replay decides only at report times,
whereas live E is evaluated on every book update. Maker fills from the
public tape are estimates: it shows no queue positions, and in fast markets
maker replays have flipped sign with 150 ms of latency. F's replay fills
100 shares at a traded price where fewer may have been offered, and its
taker buys stand for an ask it cannot see. The study is read-only and
changes nothing.
**TESTING.** Unit tests on synthetic history:

* an oracle market beats the model;
* a uniform market loses to it;
* no day is scored with a model that has learned it;
* stale-quote timing and profit accounting;
* the price proxy;
* skipped days;
* strategies at traded prices: live-range gating, P&L arithmetic, one trade
  per day, bucket and variant, a mis-resolved day as a loss, and the day
  replay (`strategies_are_replayed_at_traded_prices`, plus `market_sim`
  unit tests);
* strategy E: the stand-in trades only where buyers lifted the offers, the
  variants' relations, the flow columns
  (`strategy_e_is_replayed_with_its_book_stand_in`), and the flow window
  (`taker_flow_counts_the_lookback_and_the_ask_it_began_with`);
* makers: fills only through the price, with exact P&L and rebate, and the
  study adding up to the tape family by family
  (`makers_fill_only_through_their_price_and_the_other_side_is_accounted`,
  which fails if at-price fills are allowed). The segmentation, the report
  schedule across the hour and midnight, cancel times and the fill window
  have their own unit tests in `market_makers`;
* strategy F: every trade inside the slot the days before it learned
  (checked against an independent prequential computation), at the high's
  bucket's ask, one a day, the mis-resolved day as a loss, the day replay,
  its own rows and section, the out-of-sample line, old reports still
  loading (`strategy_f_buys_the_high_inside_the_slot_the_days_before_learned`);
  unit tests in `market_peak` cover the ask at the report and from the tape,
  the price edges (above 0.90, at most the cap), the slot end, the data-age
  limit, the fallback slot, the drop variant, the maker's bid, cancel and
  rebate, the out-of-sample choice (ties keep the configured rule) and the
  variant list.

Data API paging, the offset cap, window splitting, dedup and the request
budget are tested against a mock server. An end-to-end run against mock
Gamma, Data API and IEM servers checks the cache and a rerun that makes no
new requests.

## 36b. Model structure selection

**PURPOSE.** The replay of 28 September 2026
([docs/research/replay-2026-09-28.md](../research/replay-2026-09-28.md))
showed structural faults of the model's cells:

* the clock restarts at every report that repeats the high;
* hours before noon share one cell;
* the rise cannot see the observed level.

A candidate structure fixes all three. Whether it predicts better is
measured, not assumed.

**THE TWO STRUCTURES (fixed before any result was seen).**

| | current | candidate |
|---|---|---|
| clock | minutes since the *last* report at the high | minutes since the *first* report at the high |
| hour | < 12, 12–13, 14–15, 16–17, ≥ 18 | < 09, 09, 10, 11, then as current |
| forecast refinement | rise | headroom |

Both use the same drop and season cells, the same Dirichlet smoothing and
K = 4. The strategies' confirmation gate is unchanged: it still counts from
the last report at the high.

**METHOD (`wm-backtest::selection`).**

* Training (`study_and_select`) trains both structures in one prequential
  pass.
* Every report between 10:00 and 18:00 local, after a 365-day burn-in, is
  predicted by each structure as trained on earlier days only. Each
  predicts with its forecast input only when its own placebo-controlled
  forecast evaluation adopted it, i.e. as the service would use it.
* **Rule:** the candidate replaces the current structure only with
  ≥ 365 scored days and a 95 % day-block bootstrap interval of the change
  in multi-class log loss per report (candidate − current) entirely below
  zero.
* The report adds log loss by hour and for repeated highs.
* The verdict is stored in the model (`selection`). It is shown in the
  training report, the log and the MODEL tooltip.
* A model trained without the comparison is retrained once in the
  background.

**TESTING.** `crates/wm-backtest/tests/structure_selection.rs`:

* a plateau world, where only the first-reached clock sees how long a high
  has held, adopts the candidate;
* a world without repeated highs gives identical predictions and changes
  nothing;
* a short history keeps the current structure;
* each structure evaluates its own forecast input against its own placebo.

Plus bucket, cell-key and backward-compatibility unit tests in
`wm-strategy`, training tests (verdict in the model and report) and the
runtime's retrain-once test.

## 37. Overfitting safeguards

| Safeguard | Mechanism |
|---|---|
| Minimum evidence | Strategies require model support ≥ 50 samples in the most specific cell; hierarchical smoothing shrinks sparse cells |
| Uncertainty always shown | Wilson intervals (survival); bootstrap CI of mean daily PnL (backtests) |
| No look-ahead | Knowledge-time replay, prefix-stability test, walk-forward with embargo, forecast issue times |
| Stable regions over peaks | Parameter grids report neighbourhoods; a threshold is acceptable only if adjacent values are also positive out of sample (ASSUMPTION: rule to be applied when sweeps run) |
| Few parameters | Strategies expose a handful of thresholds; the model has fixed hierarchy levels, not free features |
| Honest execution | Book-walk fills, latency, fees rounded up, `adverse_ticks` stress, fidelity labels |
| Objective | Robust EV subject to drawdown, sample size, stability and liquidity, never win rate, raw PnL or ROI alone |
| Audit | Every run stores config, model id and decision log (`strategy_runs`, `backtest_runs`, `decision_snapshots`) |
| Synthetic ≠ evidence | Synthetic data is labelled in reports, in the UI (DEMO banner) and in market titles |
