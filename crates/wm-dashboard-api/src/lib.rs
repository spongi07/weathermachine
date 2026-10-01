//! # wm-dashboard-api
//!
//! The typed contract between the Weather Machine server and its Rust/WASM
//! dashboard. Depends only on `serde` so it compiles for `wasm32`.
//! Timestamps are Unix milliseconds; money and prices are display floats
//! (all *decisions* use exact fixed-point types server-side).

use serde::{Deserialize, Serialize};

/// API version (bump on breaking changes).
pub const API_VERSION: u32 = 1;

/// Peak-confirmation windows (minutes of observed data after the last touch
/// of the high) evaluated by the kernel; mirrored from `wm-strategy` and kept
/// in sync by a server-side test.
pub const CONFIRMATION_WINDOWS: [u32; 9] = [30, 45, 60, 75, 90, 105, 120, 150, 180];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub api_version: u32,
    pub generated_at_ms: i64,
    /// Engine (knowledge) time — differs from wall time in demo/replay.
    pub engine_time_ms: i64,
    pub mode: String,
    /// Synthetic data: the UI shows a prominent banner.
    pub demo: bool,
    pub version: String,
    pub instance: String,
    pub run_id: String,
    pub model_id: String,
    /// Probability model status (no model ⇒ no weather trades).
    #[serde(default)]
    pub model: ModelDto,
    pub kill_switch: Option<String>,
    pub storage_ok: bool,
    pub execution_ok: bool,
    pub live_trading_enabled: bool,
    pub engine: EngineDto,
    pub locations: Vec<LocationDto>,
    pub providers: Vec<ProviderDto>,
    pub market_stream: Option<StreamDto>,
    pub risk: RiskDto,
    pub positions: Vec<PositionDto>,
    pub orders: Vec<OrderDto>,
    pub decisions: Vec<DecisionDto>,
    pub alerts: Vec<AlertDto>,
    pub break_even: Vec<BreakEvenDto>,
    /// Every strategy of the engine, for its page.
    #[serde(default)]
    pub strategies: Vec<StrategyDto>,
}

impl DashboardSnapshot {
    /// A strategy's one-line status for its card: `(class, text)` — a
    /// signal, else strategy F's slot and the blocker on the high's bucket.
    pub fn strategy_status(&self, s: &StrategyDto) -> (&'static str, String) {
        if !s.enabled {
            return ("muted", "disabled".to_owned());
        }
        let evals: Vec<(&LocationDto, &EvaluationDto)> = self
            .locations
            .iter()
            .flat_map(|l| l.evaluations.iter().map(move |e| (l, e)))
            .filter(|(_, e)| e.strategy == s.id)
            .collect();
        if let Some((_, e)) = evals.iter().find(|(_, e)| e.signal) {
            return ("good", format!("SIGNAL {} {}", e.bucket, e.side));
        }
        let slot = s
            .peak_slot
            .as_ref()
            .and_then(|p| p.today.first())
            .map(|t| format!("{} slot {}–{}: {}", t.season, t.start, t.end, t.status));
        // The high's bucket first: that is where a trade would come from.
        let on_high = evals.iter().find(|(l, e)| {
            l.market.as_ref().is_some_and(|m| {
                m.rows
                    .iter()
                    .any(|r| r.contains_high && r.label == e.bucket)
            })
        });
        let blocker = on_high.or(evals.first()).and_then(|(_, e)| {
            e.blockers
                .first()
                .map(|b| format!("{} {}: {b}", e.bucket, e.side))
        });
        match (slot, blocker) {
            (Some(slot), Some(b)) => ("", format!("{slot} · {b}")),
            (Some(slot), None) => ("", slot),
            (None, Some(b)) => ("", b),
            (None, None) => (
                "muted",
                "no evaluation yet (no market, model or report)".to_owned(),
            ),
        }
    }

    /// A strategy's counts in this run: (proposals approved, rejected,
    /// orders, orders with a fill).
    pub fn strategy_counts(&self, s: &StrategyDto) -> (usize, usize, usize, usize) {
        let mine = self.decisions.iter().filter(|d| d.strategy == s.id);
        let approved = mine.clone().filter(|d| d.approved).count();
        let rejected = mine.filter(|d| !d.approved).count();
        let orders: Vec<&OrderDto> = self.orders.iter().filter(|o| o.strategy == s.id).collect();
        let filled = orders.iter().filter(|o| o.filled > 0.0).count();
        (approved, rejected, orders.len(), filled)
    }
}

/// One strategy: what it does, whether it runs and how it is configured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StrategyDto {
    /// Engine id, e.g. `F_peak_slot` (orders and decisions carry it).
    pub id: String,
    /// The letter evaluation lines start with, e.g. `F`.
    pub letter: String,
    pub name: String,
    pub enabled: bool,
    /// What it does, in a sentence or two.
    pub summary: String,
    /// The configured settings, keys as in the configuration file.
    pub settings: Vec<(String, String)>,
    /// Strategy F: its time slots.
    #[serde(default)]
    pub peak_slot: Option<PeakSlotDto>,
}

impl StrategyDto {
    /// Does an evaluation line (`"F 21°C YES · …"`) belong to this strategy?
    pub fn owns_line(&self, line: &str) -> bool {
        line.split_once(' ')
            .is_some_and(|(tag, _)| tag == self.letter)
    }
}

/// Strategy F's time slots per season.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeakSlotDto {
    /// Where the slots come from (the model's peak times or the fallback).
    pub source: String,
    /// Today, per location.
    pub today: Vec<SlotTodayDto>,
    pub seasons: Vec<SeasonSlotDto>,
}

/// Today's slot of one location and where its local time stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotTodayDto {
    pub location: String,
    pub season: String,
    pub local_time: String,
    /// `HH:MM`, end exclusive.
    pub start: String,
    pub end: String,
    pub inside: bool,
    /// e.g. "before the slot: it starts in 2 h 13 min".
    pub status: String,
}

/// One season's slot and the peak times behind it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeasonSlotDto {
    pub season: String,
    pub start: String,
    pub end: String,
    /// Days of history behind it (0: the fallback slot).
    pub days: u32,
    /// Local time the day's high was first reported: mean, median, 90 %.
    pub mean: Option<String>,
    pub median: Option<String>,
    pub q90: Option<String>,
    /// Share of days whose high was first reported after the slot.
    pub later_than_slot: Option<f64>,
}

/// One strategy's latest evaluation of one bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvaluationDto {
    /// Engine id of the strategy.
    pub strategy: String,
    pub bucket: String,
    /// `YES` or `NO`.
    pub side: String,
    pub ask: Option<f64>,
    pub bid: Option<f64>,
    /// The probability the strategy uses (model pooled with the market).
    pub p_win: Option<f64>,
    pub model_p: Option<f64>,
    pub market_p: Option<f64>,
    /// Per share after fee and slippage allowance.
    pub ev: Option<f64>,
    pub break_even: Option<f64>,
    pub signal: bool,
    /// Everything that blocks it (empty on a signal).
    pub blockers: Vec<String>,
}

/// A research report saved on the data volume (`/api/v1/research`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResearchReportDto {
    /// Path segment: `/api/v1/research/{name}`.
    pub name: String,
    pub title: String,
    pub available: bool,
    pub bytes: u64,
    pub modified_ms: Option<i64>,
    /// How to produce or refresh it.
    pub how: String,
}

/// Probability model status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelDto {
    /// `loaded` | `training` | `missing` | `failed` | `invalid` | `disabled`.
    pub state: String,
    pub detail: String,
    /// Training progress in station-years: (done, total).
    pub progress: Option<(u32, u32)>,
    /// The loaded model's forecast evaluation verdict (`None`: forecasts off).
    #[serde(default)]
    pub forecast: Option<String>,
    /// The loaded model's structure comparison verdict (`None`: no
    /// comparison ran for it).
    #[serde(default)]
    pub structure: Option<String>,
    /// Background retraining in progress; the loaded model keeps trading.
    #[serde(default)]
    pub retraining: Option<String>,
}

impl ModelDto {
    pub fn loaded(&self) -> bool {
        self.state == "loaded"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineDto {
    pub events_total: u64,
    pub evaluations_total: u64,
    pub proposals_total: u64,
    pub approvals_total: u64,
    pub rejections_total: u64,
    pub fills_total: u64,
    pub last_handle_micros: u64,
    pub max_handle_micros: u64,
    pub last_seq: u64,
    pub events_by_kind: Vec<(String, u64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PointDto {
    pub t_ms: i64,
    pub local: String,
    pub temp_c: f64,
    pub speci: bool,
    /// Counted by the primary resolution view.
    pub eligible: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewDto {
    pub label: String,
    pub observations: u32,
    pub high_c: Option<f64>,
    pub high_whole: Option<i32>,
    /// Local time of the latest report at the high (retests move it).
    pub high_local: Option<String>,
    /// Local time of the first report at the high.
    #[serde(default)]
    pub high_first_local: Option<String>,
    /// Later reports at the high.
    pub retests: u32,
    pub minutes_since_high: Option<i64>,
    pub drop_c: Option<f64>,
    pub lower_since_high: u32,
    pub slope_c_per_h: Option<f64>,
    pub accel: Option<f64>,
    pub trajectory: Option<String>,
    pub minutes_after_solar_noon: Option<i32>,
    pub season: Option<String>,
    pub windows_met: Vec<u32>,
    /// P(final = high + k), last entry = tail.
    pub distribution: Option<Vec<f64>>,
    pub model_support: Option<u32>,
    pub model_source: Option<String>,
}

impl ViewDto {
    /// When the high was reported: "first reported 02:25, last 13:25,
    /// 5 retests", or "first reported 13:25" without a retest. `None`
    /// without a high.
    pub fn high_times(&self) -> Option<String> {
        let last = self.high_local.as_deref()?;
        let first = self.high_first_local.as_deref();
        Some(match first {
            Some(f) if self.retests == 0 || f == last => format!("first reported {f}"),
            Some(f) => format!(
                "first reported {f}, last {last}, {} retest{}",
                self.retests,
                if self.retests == 1 { "" } else { "s" }
            ),
            // A snapshot from before the first time was sent.
            None => format!("last reported {last}"),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PollDto {
    pub at_ms: i64,
    pub mode: String,
    pub reason: String,
    pub expected_report_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectorDto {
    pub active_provider: Option<String>,
    pub polls_total: u64,
    pub gate_closed_total: u64,
    pub new_observations_total: u64,
    pub duplicates_total: u64,
    pub corrections_total: u64,
    pub out_of_order_total: u64,
    pub persist_failures_total: u64,
    pub storage_ok: bool,
    pub next_poll: Option<PollDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservationRowDto {
    pub t_ms: i64,
    pub local: String,
    pub temp_c: Option<f64>,
    pub report_type: String,
    pub version: u32,
    pub provider: String,
    pub raw: String,
    pub knowledge_delay_s: i64,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LadderRowDto {
    pub label: String,
    pub lower: Option<i32>,
    pub upper: Option<i32>,
    pub yes_bid: Option<f64>,
    pub yes_ask: Option<f64>,
    pub no_bid: Option<f64>,
    pub no_ask: Option<f64>,
    pub yes_spread: Option<f64>,
    pub yes_ask_depth_usd: Option<f64>,
    pub implied_p: Option<f64>,
    pub model_p: Option<f64>,
    /// P(YES) the strategies use: the model pooled with a reliable market
    /// midpoint, never above the model.
    #[serde(default)]
    pub used_p: Option<f64>,
    pub edge: Option<f64>,
    /// EVs after fee and slippage allowance, at the pooled probabilities.
    pub yes_ev: Option<f64>,
    pub no_ev: Option<f64>,
    pub yes_break_even: Option<f64>,
    pub signals: Vec<String>,
    pub blockers: Vec<String>,
    pub position_shares: f64,
    pub contains_high: bool,
    pub book_age_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MarketDto {
    pub event_slug: String,
    pub title: String,
    pub local_date: String,
    pub resolution_source: String,
    pub resolution_url: Option<String>,
    pub rules_sha256: String,
    pub rules_excerpt: String,
    pub filters: Vec<String>,
    pub filter_confirmed: bool,
    pub machine_tradable: bool,
    pub review_status: String,
    pub unrecognized_clauses: Vec<String>,
    pub taker_fee_rate: f64,
    pub neg_risk: bool,
    pub end_time_ms: Option<i64>,
    pub rows: Vec<LadderRowDto>,
}

/// The location's day-1 forecast (predictive input; never resolution data).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastDto {
    /// e.g. `open_meteo/gfs_global/d1`.
    pub product: String,
    pub received_ms: i64,
    /// The model conditions on it (evaluation adopted it, knowledge rule met).
    pub in_use: bool,
    /// Why it is (not) in use.
    pub status: String,
    pub day_max_c: Option<f64>,
    pub remaining_max_c: Option<f64>,
    /// Remaining-day maximum minus the maximum so far, per the forecast.
    pub rise_c: Option<f64>,
    /// Remaining-day forecast maximum minus the observed high.
    #[serde(default)]
    pub headroom_c: Option<f64>,
    /// Today's hourly values for the chart.
    pub hourly: Vec<ForecastPointDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastPointDto {
    pub t_ms: i64,
    /// Minutes since local midnight (0 ..= 1440; 1380/1500 on DST days).
    pub minute: i32,
    pub temp_c: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocationDto {
    pub location: String,
    pub station: String,
    pub timezone: String,
    pub local_date: String,
    pub local_time: String,
    pub current_temp_c: Option<f64>,
    pub last_observation_ms: Option<i64>,
    pub last_observation_age_s: Option<i64>,
    pub last_raw: Option<String>,
    pub peak_watch: bool,
    pub has_exposure: bool,
    pub views: Vec<ViewDto>,
    pub series: Vec<PointDto>,
    pub observations: Vec<ObservationRowDto>,
    pub collector: Option<CollectorDto>,
    pub market: Option<MarketDto>,
    #[serde(default)]
    pub forecast: Option<ForecastDto>,
    /// The station's latest KNMI ten-minute reading (strategy K's input).
    #[serde(default)]
    pub nowcast: Option<NowcastDto>,
    /// Every strategy's latest evaluation of today's buckets.
    #[serde(default)]
    pub evaluations: Vec<EvaluationDto>,
}

/// A ten-minute station reading from a faster source than the METAR
/// (KNMI): a predictive input only, never the observed high.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NowcastDto {
    pub provider: String,
    /// End of the ten-minute interval.
    pub interval_end_ms: i64,
    pub mean_c: Option<f64>,
    pub max_c: Option<f64>,
    pub received_ms: i64,
    /// Minutes from the interval's end to its arrival.
    pub delay_min: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderDto {
    pub provider: String,
    pub scope: Option<String>,
    pub state: String,
    pub reason: String,
    pub last_success_ms: Option<i64>,
    pub last_new_observation_ms: Option<i64>,
    pub last_observation_ms: Option<i64>,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    pub backoff_s: f64,
    pub blocked_until_ms: Option<i64>,
    pub latency_ms_ewma: Option<f64>,
    pub latency_ms_last: Option<u64>,
    pub throttle_events: u64,
    pub requests_total: u64,
    pub requests_today: u32,
    pub daily_budget: Option<u32>,
    pub circuit: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamDto {
    pub connected: bool,
    pub subscribed_assets: usize,
    pub messages_total: u64,
    pub reconnects_total: u64,
    pub last_message_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CheckDto {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskDto {
    pub position_size_usd: f64,
    pub global_worst_case_usd: f64,
    pub global_limit_usd: f64,
    pub capital_deployed_usd: f64,
    pub daily_new_exposure_usd: f64,
    pub daily_new_limit_usd: Option<f64>,
    pub daily_realized_pnl_usd: f64,
    pub daily_loss_limit_usd: Option<f64>,
    pub realized_pnl_total_usd: f64,
    pub max_price: f64,
    pub max_spread: f64,
    pub max_weather_age_min: i64,
    pub per_event: Vec<(String, f64)>,
    pub checks: Vec<CheckDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PositionDto {
    pub event_slug: String,
    pub bucket: String,
    pub side: String,
    pub shares: f64,
    pub cost_usd: f64,
    pub avg_cost: f64,
    pub mark: Option<f64>,
    pub unrealized_usd: Option<f64>,
    pub realized_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderDto {
    pub client_order_id: String,
    pub strategy: String,
    pub bucket: String,
    pub outcome: String,
    pub side: String,
    pub limit: f64,
    pub shares: f64,
    pub filled: f64,
    pub avg_price: Option<f64>,
    pub fees_usd: f64,
    pub status: String,
    pub reason: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionDto {
    pub id: u64,
    pub at_ms: i64,
    pub strategy: String,
    pub summary: String,
    pub approved: bool,
    pub reasons: Vec<String>,
    /// Routine evaluations: one line per strategy and bucket with the price,
    /// the probability used, the EV and what blocked it.
    #[serde(default)]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertDto {
    pub at_ms: i64,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BreakEvenDto {
    pub price: f64,
    pub fee_per_share: f64,
    pub break_even_probability: f64,
    pub wins_to_recover_one_loss: f64,
}

/// Operator command payloads (POST /api/v1/kill-switch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KillSwitchRequest {
    pub engaged: bool,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluation_lines_belong_to_the_strategy_of_their_letter() {
        let f = StrategyDto {
            id: "F_peak_slot".into(),
            letter: "F".into(),
            ..Default::default()
        };
        assert!(f.owns_line("F 21°C YES · ask 0.93 — SIGNAL"));
        assert!(!f.owns_line("E 21°C YES · ask 0.93 — SIGNAL"));
        assert!(!f.owns_line("FF 21°C YES"));
        assert!(!f.owns_line("F"));
    }

    #[test]
    fn a_snapshot_without_the_new_fields_still_decodes() {
        let old = r#"{"api_version":1,"generated_at_ms":0,"engine_time_ms":0,"mode":"paper","demo":false,"version":"0","instance":"i","run_id":"r","model_id":"m","kill_switch":null,"storage_ok":true,"execution_ok":true,"live_trading_enabled":false,"engine":{"events_total":0,"evaluations_total":0,"proposals_total":0,"approvals_total":0,"rejections_total":0,"fills_total":0,"last_handle_micros":0,"max_handle_micros":0,"last_seq":0,"events_by_kind":[]},"locations":[],"providers":[],"market_stream":null,"risk":{"position_size_usd":10.0,"global_worst_case_usd":0.0,"global_limit_usd":100.0,"capital_deployed_usd":0.0,"daily_new_exposure_usd":0.0,"daily_new_limit_usd":null,"daily_realized_pnl_usd":0.0,"daily_loss_limit_usd":null,"realized_pnl_total_usd":0.0,"max_price":0.99,"max_spread":0.05,"max_weather_age_min":40,"per_event":[],"checks":[]},"positions":[],"orders":[],"decisions":[],"alerts":[],"break_even":[]}"#;
        let s: DashboardSnapshot = serde_json::from_str(old).unwrap();
        assert!(s.strategies.is_empty());
    }

    fn f() -> StrategyDto {
        StrategyDto {
            id: "F_peak_slot".into(),
            letter: "F".into(),
            name: "Peak slot".into(),
            enabled: true,
            peak_slot: Some(PeakSlotDto {
                today: vec![SlotTodayDto {
                    location: "amsterdam".into(),
                    season: "autumn".into(),
                    start: "13:25".into(),
                    end: "15:26".into(),
                    status: "before the slot: it starts in 2 h".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn with_evals(evals: Vec<EvaluationDto>) -> DashboardSnapshot {
        DashboardSnapshot {
            locations: vec![LocationDto {
                market: Some(MarketDto {
                    rows: vec![
                        LadderRowDto {
                            label: "19°C".into(),
                            contains_high: true,
                            ..Default::default()
                        },
                        LadderRowDto {
                            label: "20°C".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }),
                evaluations: evals,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn eval(strategy: &str, bucket: &str, signal: bool, blocker: &str) -> EvaluationDto {
        EvaluationDto {
            strategy: strategy.into(),
            bucket: bucket.into(),
            side: "YES".into(),
            signal,
            blockers: if signal { vec![] } else { vec![blocker.into()] },
            ..Default::default()
        }
    }

    #[test]
    fn the_status_shows_a_signal_then_the_slot_and_the_high_buckets_blocker() {
        let s = with_evals(vec![
            eval("F_peak_slot", "20°C", false, "ask 0.30 not above 0.90"),
            eval(
                "F_peak_slot",
                "19°C",
                false,
                "08:42 outside the autumn slot 13:25–15:26",
            ),
            eval("E_book_confirmed_high", "19°C", true, ""),
        ]);
        assert_eq!(
            s.strategy_status(&f()),
            (
                "",
                "autumn slot 13:25–15:26: before the slot: it starts in 2 h · 19°C YES: 08:42 outside the autumn slot 13:25–15:26".to_owned()
            )
        );
        let signal = with_evals(vec![eval("F_peak_slot", "19°C", true, "")]);
        assert_eq!(
            signal.strategy_status(&f()),
            ("good", "SIGNAL 19°C YES".to_owned())
        );
        let mut off = f();
        off.enabled = false;
        assert_eq!(s.strategy_status(&off).1, "disabled");
        let mut e = f();
        e.peak_slot = None;
        assert_eq!(with_evals(vec![]).strategy_status(&e).0, "muted");
    }

    #[test]
    fn strategy_counts_are_its_own() {
        let mut s = with_evals(vec![]);
        s.decisions = vec![
            DecisionDto {
                strategy: "F_peak_slot".into(),
                approved: true,
                ..Default::default()
            },
            DecisionDto {
                strategy: "F_peak_slot".into(),
                ..Default::default()
            },
            DecisionDto {
                strategy: "evaluation".into(),
                ..Default::default()
            },
        ];
        s.orders = vec![
            OrderDto {
                strategy: "F_peak_slot".into(),
                filled: 100.0,
                ..Default::default()
            },
            OrderDto {
                strategy: "E_book_confirmed_high".into(),
                filled: 5.0,
                ..Default::default()
            },
        ];
        assert_eq!(s.strategy_counts(&f()), (1, 1, 1, 1));
    }

    #[test]
    fn the_high_shows_its_first_and_last_report() {
        let v = |first: Option<&str>, last: Option<&str>, retests| ViewDto {
            high_first_local: first.map(Into::into),
            high_local: last.map(Into::into),
            retests,
            ..ViewDto::default()
        };
        // 1 Oct: 20 °C first at 02:25 (a warm night), back at it from 11:25.
        assert_eq!(
            v(Some("02:25"), Some("13:25"), 5).high_times().as_deref(),
            Some("first reported 02:25, last 13:25, 5 retests")
        );
        assert_eq!(
            v(Some("12:55"), Some("13:25"), 1).high_times().as_deref(),
            Some("first reported 12:55, last 13:25, 1 retest")
        );
        assert_eq!(
            v(Some("13:25"), Some("13:25"), 0).high_times().as_deref(),
            Some("first reported 13:25")
        );
        assert_eq!(
            v(None, Some("13:25"), 2).high_times().as_deref(),
            Some("last reported 13:25")
        );
        assert_eq!(v(Some("02:25"), None, 0).high_times(), None);
        // A view from an older server decodes without the first time.
        let old: ViewDto = serde_json::from_str(
            r#"{"label":"all","observations":3,"high_c":20.0,"high_whole":20,"high_local":"13:25","retests":0,"minutes_since_high":0,"drop_c":0.0,"lower_since_high":0,"slope_c_per_h":null,"accel":null,"trajectory":null,"minutes_after_solar_noon":null,"season":null,"windows_met":[],"distribution":null,"model_support":null,"model_source":null}"#,
        )
        .unwrap();
        assert_eq!(old.high_first_local, None);
    }

    #[test]
    fn snapshot_roundtrips_as_json() {
        let s = DashboardSnapshot {
            api_version: API_VERSION,
            mode: "paper".into(),
            demo: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: DashboardSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
