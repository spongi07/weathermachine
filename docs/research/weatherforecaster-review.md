# Review: the "weatherforecaster" bot — what it shows, what was ported

*Scope.* A separate Python project (CatBoost temperature models, Open-Meteo
forecasts, weather.com observations, Polymarket paper trading for 49 cities)
was reviewed to decide what could make Weather Machine more profitable. Its
code, outputs and data (forecast archives, station daily highs, backtest
results and forward paper-trading ledgers for 3–16 July 2026) were analysed.
All numbers below are computed from those files.

**Conclusion.** Its backtested edge came from look-ahead in the forecast
archive; forward, every strategy lost money. Its strategies were therefore
**not** ported. The one sound idea — numerical weather forecasts as an
input — was rebuilt in Rust on a leak-free product and is used only if a
walk-forward test on real EHAM history proves it helps
([blueprint §19](../blueprint/05-markets.md#19-forecastprovider-and-the-day-1-forecast-feature)).

## 1. The backtest edge was look-ahead

The bot's "forecast" history (`fetch_forecast_archive.py`) comes from
Open-Meteo's *Historical Forecast API*. That archive is stitched together
from the first hours of consecutive model runs: the value for an afternoon
comes from a run started that same morning. It is close to an analysis of
the day, not a forecast available the evening before — yet the backtest
(`market_pnl_backtest.py`) pairs it with the *day-ahead* market price.

How good was that "forecast"? Mean absolute error of the archive's daily
maximum against the station's observed daily maximum, 2022-01 → 2026-06
(≈1,636 days per city):

| | MAE of archive "forecast" | MAE of persistence (yesterday's high) |
|---|---:|---:|
| London | 0.62 °C | 1.96 °C |
| **Amsterdam** | **0.80 °C** | 2.04 °C |
| Paris | 0.74 °C | 2.23 °C |
| median of 49 cities | 1.34 °C | 2.15 °C |

0.8 °C against whole-degree METAR highs (rounding alone costs ~0.25 °C) is
analysis-level accuracy. Live, the same bot's bucket-centre error on its own
settled bets was **1.42 °C** (46 bets) — the leak is the gap.

| | forecast-bucket hit rate | average price paid |
|---|---:|---:|
| Backtest (1,840 markets, 49 cities) | 32.6 % | 0.264 |
| Live paper ledger (46 settled bets) | 28.3 % | 0.220 |

At those prices the live hit rate is roughly break-even *before* fees; the
backtest's +11.7 % "realistic" ROI (`net_pnl.json`) does not survive.

## 2. Forward paper trading lost money everywhere

Four strategies × three forecast sources, target days 3–16 July 2026, 49 cities:

| strategy | source | settled | staked | P&L | ROI |
|---|---|---:|---:|---:|---:|
| spread | hist / blend / om | 673 / 662 / 617 | $2,834 / $2,346 / $1,607 | −$445 / −$404 / −$460 | −16 % / −17 % / −29 % |
| hedge (60/20/20) | hist / blend / om | 1,001 / 1,113 / 971 | $2,260 / $2,162 / $1,888 | −$432 / −$414 / −$439 | −19 % / −19 % / −23 % |
| value (edge ≥ 0.08) | hist / blend / om | 565 / 588 / 432 | $2,994 / $2,185 / $2,161 | −$451 / −$472 / −$485 | −15 % / −22 % / −22 % |
| quarter-Kelly | hist / blend / om | 458 / 519 / 563 | $2,910 / $2,176 / $2,390 | −$385 / −$461 / −$481 | −13 % / −21 % / −20 % |

All books together: 8,212 settled positions, $28,243 staked, **−$5,554
(−19.7 %)**. How the positions ended:

| exit | share | P&L per $ staked |
|---|---:|---:|
| stop-loss (price −25 %) | 78.5 % | −0.52 |
| take-profit | 14.7 % | +0.11 |
| held to settlement, won | 3.9 % | +4.33 |
| held to settlement, lost | 2.8 % | −1.04 |

A 25 % stop on cheap binary contracts (average entry ≈ 0.08–0.25) converts
ordinary price noise into realized losses — 78 % of positions were stopped
out. (The earlier ledger of 28 June – 4 July, with different exit rules,
shows +24.6 % on 54 bets — within noise at that sample size.)

## 3. Other findings

* **Wrong target day.** Station daily highs are grouped by *UTC* date
  (`wx_fetch.py`, `to_daily`), but Polymarket resolves on the *local* day.
  For Amsterdam the UTC day starts at 01:00/02:00 local; for US cities it
  includes the previous local evening. Training labels and paper settlement
  both inherit the error.
* **Inconsistent fees.** Three different fee models across scripts
  (`0.025·min(p,1−p)`, `0.05·stake·(1−p)`, `5 %·p(1−p)`); the compounding
  results in `strategy_backtest.json` are not realistic.
* **Credentials in source.** An api.weather.com key is hard-coded in
  `src/wx_fetch.py` and `src/wx_validate.py`. Treat it as exposed: remove it
  from the code and the repository history, and check the data provider's
  terms for automated use. (The key is deliberately not reproduced here.)
* **Request volume.** The server polls Gamma every 3 minutes and re-forecasts
  49 cities every 10 minutes without a rate-limit layer.

## 4. What was ported, and how

| Idea in the bot | Decision | In Weather Machine |
|---|---|---|
| NWP forecast as an input | **Ported, leak-free** | Open-Meteo *Previous Runs* `temperature_2m_previous_day1`: every hourly value forecast 24 h before its time — the same product for years of training and for today, so no look-ahead. One request per location per hour through a rate-limited gate; optional subscription key. |
| Bias correction (MOS, rolling bias) | Ported as a robust feature | The model uses the *forecast rise* — rest-of-day maximum minus maximum so far of the forecast — in which level biases cancel. |
| "The model is good because the backtest says so" | **Replaced by a test that can fail** | Prequential walk-forward evaluation on the host's real history, a placebo control (forecast of 14 days earlier), and a fixed adoption rule. The forecast changes probabilities only if adopted; the verdict is in the training report and on the dashboard. |
| Day-ahead "buy the forecast bucket" (+ straddle) | Not ported | No edge after removing the leak (§1–2). |
| Stop-loss / take-profit | Not ported | Destroyed value forward (§2). Weather Machine's exits stay model-driven (unwind when P(win) collapses). |
| Kelly sizing | Deferred | Needs a proven, calibrated edge first; revisit after the forecast evaluation and paper results. |
| weather.com observations | Not ported | Credential and terms issues; Weather Machine uses official NOAA sources. |
| 49-city universe | Deferred | The architecture is multi-location; each city needs its resolution source and station verified (Phase 0) before it is added. |

## 5. How to read the verdict on the server

After the first training with this version, open
`/data/research/eham-survival.md`, section **Forecast evaluation**:

* *Verdict* — adopted or not, with the numbers behind it.
* *log loss per decision* with vs. without the forecast, and vs. the
  placebo, each with a 95 % interval. Adoption needs both intervals below
  zero and at least 365 scored days.
* *P(high is final) by forecast rise* — whether "warming later" days really
  end with a new high more often.
* *Constant-price proxy* — at fixed YES prices, how many trades each
  prediction would take and what they would have earned. It compares trade
  selection only; real prices vary.

The dashboard's **FORECAST** pill shows the same verdict.
