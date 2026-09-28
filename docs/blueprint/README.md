# Weather Machine — Technical Blueprint

This blueprint describes the system *as built* in this repository and the
research programme that must run before any capital is at risk. Every
component lists **PURPOSE · INPUTS · OUTPUTS · RUST INTERFACE · FAILURE
MODES · TESTING**, and every architectural or research statement carries one
of three labels:

| Label | Meaning |
|---|---|
| **KNOWN FACT** | Verified from primary documentation, source code, or this repository's tests. Sources are linked. |
| **ASSUMPTION** | Engineering choice or secondary-source report that has not been verified against the live system; each one names how it will be verified. |
| **HYPOTHESIS TO BACKTEST** | A trading belief. It is never encoded as fact: the code measures it, and the strategies are blocked unless a trained model supplies a probability. |

## The 43 components

| # | Component | Document |
|---|---|---|
| 1 | System architecture | [01-architecture.md](01-architecture.md#1-system-architecture) |
| 2 | Rust workspace | [01-architecture.md](01-architecture.md#2-rust-workspace) |
| 3 | Event architecture | [01-architecture.md](01-architecture.md#3-event-architecture) |
| 4 | Domain types | [01-architecture.md](01-architecture.md#4-domain-types) |
| 5 | Rust traits | [01-architecture.md](01-architecture.md#5-rust-traits) |
| 6 | Amsterdam configuration | [03-phase0-eham.md](03-phase0-eham.md#6-amsterdam-configuration) |
| 7 | NWS WRH technical investigation | [03-phase0-eham.md](03-phase0-eham.md#7-nws-wrh-technical-investigation) |
| 8 | Underlying EHAM data-source discovery | [03-phase0-eham.md](03-phase0-eham.md#8-underlying-eham-data-source-discovery) |
| 9 | NWS rate-limit-safe architecture | [04-ingestion.md](04-ingestion.md#9-nws-rate-limit-safe-architecture) |
| 10 | Adaptive PollingPolicy | [04-ingestion.md](04-ingestion.md#10-adaptive-pollingpolicy) |
| 11 | RateLimiter | [04-ingestion.md](04-ingestion.md#11-ratelimiter) |
| 12 | Caching | [04-ingestion.md](04-ingestion.md#12-caching) |
| 13 | Observation deduplication | [04-ingestion.md](04-ingestion.md#13-observation-deduplication) |
| 14 | Observation correction handling | [04-ingestion.md](04-ingestion.md#14-observation-correction-handling) |
| 15 | Raw-data persistence | [04-ingestion.md](04-ingestion.md#15-raw-data-persistence) |
| 16 | ProviderHealth model | [04-ingestion.md](04-ingestion.md#16-providerhealth-model) |
| 17 | TemperatureStateEngine | [04-ingestion.md](04-ingestion.md#17-temperaturestateengine) |
| 18 | ResolutionSource architecture | [05-markets.md](05-markets.md#18-resolutionsource-architecture) |
| 19 | ForecastProvider and the day-1 forecast feature | [05-markets.md](05-markets.md#19-forecastprovider-and-the-day-1-forecast-feature) |
| 20 | Polymarket integration | [05-markets.md](05-markets.md#20-polymarket-integration) |
| 21 | TemperatureOutcomeMapper | [05-markets.md](05-markets.md#21-temperatureoutcomemapper) |
| 22 | PeakDetectionEngine | [06-strategy.md](06-strategy.md#22-peakdetectionengine) |
| 23 | Trajectory features | [06-strategy.md](06-strategy.md#23-trajectory-features) |
| 24 | Probability model | [06-strategy.md](06-strategy.md#24-probability-model) |
| 25 | BUY YES (strategy A) | [06-strategy.md](06-strategy.md#25-buy-yes--strategy-a) |
| 26 | BUY NO (strategy B) | [06-strategy.md](06-strategy.md#26-buy-no--strategy-b) |
| 27 | SPLIT + UNWIND (strategy C) | [06-strategy.md](06-strategy.md#27-split--unwind--strategy-c) |
| 27a | DECIDED OUTCOMES (strategy D) | [06-strategy.md](06-strategy.md#27a-decided-outcomes--strategy-d) |
| 27b | Market pooling (A and B) | [06-strategy.md](06-strategy.md#27b-the-book-as-information--market-pooling-a-and-b) |
| 28 | UnwindEngine | [06-strategy.md](06-strategy.md#28-unwindengine) |
| 29 | RiskEngine | [07-risk-storage.md](07-risk-storage.md#29-riskengine) |
| 30 | $10 / $100 exposure model | [07-risk-storage.md](07-risk-storage.md#30-10--100-exposure-model) |
| 31 | PostgreSQL schema | [07-risk-storage.md](07-risk-storage.md#31-postgresql-schema) |
| 32 | Historical-data audit | [08-research.md](08-research.md#32-historical-data-audit) |
| 33 | ReplayEngine | [08-research.md](08-research.md#33-replayengine) |
| 34 | BacktestEngine | [08-research.md](08-research.md#34-backtestengine) |
| 35 | Execution simulator | [08-research.md](08-research.md#35-execution-simulator) |
| 36 | Parameter research | [08-research.md](08-research.md#36-parameter-research) |
| 36a | Model versus market (`research market`) | [08-research.md](08-research.md#36a-model-versus-market-research-market) |
| 36b | Model structure selection | [08-research.md](08-research.md#36b-model-structure-selection) |
| 37 | Overfitting safeguards | [08-research.md](08-research.md#37-overfitting-safeguards) |
| 38 | Testing | [09-operations.md](09-operations.md#38-testing) |
| 39 | Observability | [09-operations.md](09-operations.md#39-observability) |
| 40 | Recovery architecture | [09-operations.md](09-operations.md#40-recovery-architecture) |
| 41 | Paper trading | [09-operations.md](09-operations.md#41-paper-trading) |
| 42 | Live trading | [09-operations.md](09-operations.md#42-live-trading) |
| 43 | Multi-location expansion | [09-operations.md](09-operations.md#43-multi-location-expansion) |

Supporting documents:

* [02-dependencies.md](02-dependencies.md) — every dependency: why, where, alternative.
* [10-roadmap.md](10-roadmap.md) — Phases 0–14 with status and exit criteria,
  the first data experiment and the first strategy experiment.
* [../deployment/portainer.md](../deployment/portainer.md) — deployment and operations.

## The rule above all others

> Weather Machine never increases polling frequency to approach an
> undocumented provider limit. Reliability and respectful provider usage take
> priority over collecting redundant observations.

This is enforced in code, not by convention. Every NOAA/NWS request passes a
per-provider gate with a hard floor of 30 s between requests, one request at
a time, a daily budget, and mandatory `Retry-After` handling. The polling
policy can only lengthen intervals when data is late. `wm-net`'s
`RateLimitPolicy::validate` rejects any configuration below those floors, and
the tests `nws_floor_is_enforced_by_config_validation` and
`run_loop_request_budget_over_six_virtual_hours` guard it.
