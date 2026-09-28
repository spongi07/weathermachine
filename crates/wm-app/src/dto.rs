//! Engine snapshot → dashboard DTO (display-only floats; decisions stay exact).

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::collections::HashMap;
use wm_core::health::ProviderHealthSnapshot;
use wm_core::ids::{StationId, TokenId};
use wm_core::market::{OrderBook, OutcomeSide};
use wm_core::resolution::{ObservationFilter, ResolutionSourceKind};
use wm_core::units::Price;
use wm_dashboard_api::*;
use wm_engine::{EngineSnapshot, LocationSnapshot};
use wm_polymarket::StreamStatus;
use wm_strategy::Pooling;
use wm_strategy::ev::{
    break_even_probability, break_even_table, ev_per_share, research_price_grid,
};
use wm_weather::CollectorStatus;

/// Extra inputs that live outside the engine.
pub struct DtoInputs<'a> {
    pub demo: bool,
    pub instance: &'a str,
    pub collectors: &'a HashMap<StationId, CollectorStatus>,
    pub stream: Option<&'a StreamStatus>,
    pub alerts: &'a [AlertDto],
    pub confirmed_filters: &'a HashMap<StationId, ObservationFilter>,
    pub extra_providers: &'a [ProviderHealthSnapshot],
    pub rules_review: &'a HashMap<String, String>,
    pub model: &'a ModelDto,
    /// How strategies A (YES) and B (NO) pool the model with the market.
    pub yes_pooling: Pooling,
    pub no_pooling: Pooling,
}

/// The per-bucket lines of a routine evaluation (`outputs.evaluations`).
fn evaluation_lines(outputs: &serde_json::Value) -> Vec<String> {
    outputs
        .get("evaluations")
        .and_then(serde_json::Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn local(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

fn provider_dto(s: &ProviderHealthSnapshot) -> ProviderDto {
    ProviderDto {
        provider: s.provider.to_string(),
        scope: s.scope.as_ref().map(ToString::to_string),
        state: s.state.as_str().to_owned(),
        reason: s.reason.clone(),
        last_success_ms: s.last_success_at.map(ms),
        last_new_observation_ms: s.last_new_observation_at.map(ms),
        last_observation_ms: s.last_observation_time.map(ms),
        last_status: s.last_http_status,
        last_error: s.last_error.clone(),
        consecutive_failures: s.consecutive_failures,
        backoff_s: s.current_backoff_ms as f64 / 1000.0,
        blocked_until_ms: s.blocked_until.map(ms),
        latency_ms_ewma: s.latency_ms_ewma,
        latency_ms_last: s.latency_ms_last,
        throttle_events: s.throttle_events,
        requests_total: s.requests_total,
        requests_today: s.requests_today,
        daily_budget: s.daily_budget,
        circuit: format!("{:?}", s.circuit).to_lowercase(),
    }
}

fn book_top(b: Option<&OrderBook>) -> (Option<f64>, Option<f64>) {
    (
        b.and_then(OrderBook::best_bid).map(|l| l.price.as_f64()),
        b.and_then(OrderBook::best_ask).map(|l| l.price.as_f64()),
    )
}

fn location_dto(l: &LocationSnapshot, snap: &EngineSnapshot, inp: &DtoInputs<'_>) -> LocationDto {
    let tz: Tz = l.timezone.parse().unwrap_or(chrono_tz::UTC);
    let now = snap.now;
    let last = l.series.last();
    let market = l.market.as_ref();
    let primary_filter = inp
        .confirmed_filters
        .get(&l.station)
        .copied()
        .or_else(|| {
            market.and_then(|m| {
                m.resolution
                    .filters
                    .iter()
                    .copied()
                    .find(|f| *f != ObservationFilter::AllRows)
            })
        })
        .or_else(|| market.and_then(|m| m.resolution.filters.first().copied()))
        .unwrap_or(ObservationFilter::AllRows);
    let series = l
        .series
        .iter()
        .map(|p| PointDto {
            t_ms: ms(p.observed_at),
            local: local(p.observed_at, tz),
            temp_c: p.temp.as_f64(),
            speci: p.report_type == wm_core::weather::ReportType::Speci,
            eligible: primary_filter.admits_minute(p.local_minute_of_hour),
        })
        .collect();
    let views: Vec<ViewDto> = l
        .views
        .iter()
        .map(|v| {
            let st = v.state.as_ref();
            let f = v.features.as_ref();
            ViewDto {
                label: v.label.clone(),
                observations: st.map_or(0, |s| s.observation_count),
                high_c: st.and_then(|s| s.high).map(|h| h.value.as_f64()),
                high_whole: f.map(|f| f.high_whole),
                high_local: st.and_then(|s| s.high).map(|h| local(h.last_at, tz)),
                retests: st.and_then(|s| s.high).map_or(0, |h| h.retests),
                minutes_since_high: f.map(|f| f.minutes_since_high),
                drop_c: f.map(|f| f64::from(f.drop_tenths) / 10.0),
                lower_since_high: st.map_or(0, |s| s.lower_since_high),
                slope_c_per_h: st.and_then(|s| s.slope_c_per_hour),
                accel: st.and_then(|s| s.accel_c_per_hour2),
                trajectory: f.map(|f| f.trajectory.as_str().to_owned()),
                minutes_after_solar_noon: f.map(|f| f.minutes_after_solar_noon),
                season: f.map(|f| f.season.as_str().to_owned()),
                windows_met: v.windows_met.clone(),
                distribution: v.distribution.as_ref().map(|d| d.probs.clone()),
                model_support: v.distribution.as_ref().map(|d| d.support),
                model_source: v.distribution.as_ref().map(|d| d.source.clone()),
            }
        })
        .collect();
    let collector = inp.collectors.get(&l.station);
    let observations: Vec<ObservationRowDto> = match collector {
        Some(c) => c
            .recent_observations
            .iter()
            .rev()
            .take(24)
            .map(|o| {
                let mut flags = Vec::new();
                if o.quality.auto {
                    flags.push("AUTO".to_owned());
                }
                if o.quality.correction_marker {
                    flags.push("COR".to_owned());
                }
                if o.quality.from_failover {
                    flags.push("FAILOVER".to_owned());
                }
                if o.quality.decoded_mismatch {
                    flags.push("DECODE≠".to_owned());
                }
                if o.version > 1 {
                    flags.push(format!("v{}", o.version));
                }
                ObservationRowDto {
                    t_ms: ms(o.key.observed_at),
                    local: local(o.key.observed_at, tz),
                    temp_c: o.temperature.map(|t| t.as_f64()),
                    report_type: o.key.report_type.as_str().to_owned(),
                    version: o.version,
                    provider: o.provider.to_string(),
                    raw: o.raw_text.clone(),
                    knowledge_delay_s: o.knowledge_delay_secs(),
                    flags,
                }
            })
            .collect(),
        None => l
            .series
            .iter()
            .rev()
            .take(24)
            .map(|p| ObservationRowDto {
                t_ms: ms(p.observed_at),
                local: local(p.observed_at, tz),
                temp_c: Some(p.temp.as_f64()),
                report_type: p.report_type.as_str().to_owned(),
                version: p.version,
                provider: if inp.demo {
                    "synthetic".into()
                } else {
                    "engine".into()
                },
                raw: format!(
                    "{} {:02}{:02}Z … {}/…",
                    l.station,
                    p.observed_at.format("%d%H"),
                    p.observed_at.format("%M"),
                    p.temp.round_half_up_whole()
                ),
                knowledge_delay_s: 0,
                flags: if primary_filter.admits_minute(p.local_minute_of_hour) {
                    vec!["ELIGIBLE".into()]
                } else {
                    vec![]
                },
            })
            .collect(),
    };
    let market_dto = market.map(|m| {
        let books: HashMap<&TokenId, &OrderBook> = l.books.iter().map(|b| (&b.token, b)).collect();
        let primary_view = l.views.iter().find(|v| v.distribution.is_some());
        let high = primary_view
            .and_then(|v| v.features.as_ref())
            .map(|f| f.high_whole);
        let dist = primary_view.and_then(|v| v.distribution.clone());
        let fee = m.fees;
        let slip = Price::saturating_from_micros(5_000);
        let rows = m
            .sorted_outcomes()
            .into_iter()
            .rev()
            .map(|o| {
                let yb = books.get(&o.yes_token).copied();
                let nb = books.get(&o.no_token).copied();
                let (yes_bid, yes_ask) = book_top(yb);
                let (no_bid, no_ask) = book_top(nb);
                let model_p = match (&dist, high) {
                    (Some(d), Some(h)) => Some(d.p_in_bucket_lower(h, &o.bucket)),
                    _ => None,
                };
                let model_loss = match (&dist, high) {
                    (Some(d), Some(h)) => Some(d.p_in_bucket_upper(h, &o.bucket)),
                    _ => None,
                };
                let implied = match (yes_bid, yes_ask) {
                    (Some(b), Some(a)) => Some((a + b) / 2.0),
                    _ => None,
                };
                let yes_ask_p = yb.and_then(OrderBook::best_ask).map(|l| l.price);
                let no_ask_p = nb.and_then(OrderBook::best_ask).map(|l| l.price);
                let yes_market = inp.yes_pooling.market_probability(yb, nb, now);
                let no_market = inp.no_pooling.market_probability(nb, yb, now);
                let used_p = model_p.map(|p| inp.yes_pooling.win_probability(p, yes_market));
                let no_used =
                    model_loss.map(|pl| inp.no_pooling.win_probability(1.0 - pl, no_market));
                let yes_ev = match (used_p, yes_ask_p) {
                    (Some(p), Some(a)) => Some(ev_per_share(p, a, &fee, slip)),
                    _ => None,
                };
                let no_ev = match (no_used, no_ask_p) {
                    (Some(p), Some(a)) => Some(ev_per_share(p, a, &fee, slip)),
                    _ => None,
                };
                let evals: Vec<_> = l
                    .evaluations
                    .iter()
                    .filter(|e| e.bucket_label == o.label)
                    .collect();
                let signals = evals
                    .iter()
                    .filter(|e| e.signal)
                    .map(|e| format!("{} {}", e.strategy, e.outcome_side.as_str()))
                    .collect();
                let blockers = evals
                    .iter()
                    .flat_map(|e| {
                        e.blockers
                            .iter()
                            .take(2)
                            .map(move |b| format!("{}: {b}", e.outcome_side.as_str()))
                    })
                    .collect();
                let position_shares = snap
                    .positions
                    .iter()
                    .filter(|p| {
                        p.instrument.token == o.yes_token || p.instrument.token == o.no_token
                    })
                    .map(|p| {
                        if p.instrument.outcome_side == OutcomeSide::Yes {
                            p.shares.as_f64()
                        } else {
                            -p.shares.as_f64()
                        }
                    })
                    .sum();
                LadderRowDto {
                    label: o.label.clone(),
                    lower: o.bucket.lower,
                    upper: o.bucket.upper,
                    yes_bid,
                    yes_ask,
                    no_bid,
                    no_ask,
                    yes_spread: yb.and_then(OrderBook::spread).map(|s| s.as_f64()),
                    yes_ask_depth_usd: yb
                        .and_then(OrderBook::best_ask)
                        .map(|l| l.price.as_f64() * l.size.as_f64()),
                    implied_p: implied,
                    model_p,
                    used_p,
                    edge: match (model_p, implied) {
                        (Some(p), Some(i)) => Some(p - i),
                        _ => None,
                    },
                    yes_ev,
                    no_ev,
                    yes_break_even: yes_ask_p.map(|a| break_even_probability(a, &fee, slip)),
                    signals,
                    blockers,
                    position_shares,
                    contains_high: high.is_some_and(|h| o.bucket.contains(h)),
                    book_age_ms: yb.map(|b| b.age_ms(now)),
                }
            })
            .collect();
        let (source, url) = match &m.resolution.source {
            ResolutionSourceKind::NoaaWrhTimeseries { site, url } => {
                (format!("NOAA WRH timeseries ({site})"), Some(url.clone()))
            }
            ResolutionSourceKind::WundergroundDaily { url } => {
                ("Weather Underground".to_owned(), Some(url.clone()))
            }
            other => (other.short_name().to_owned(), None),
        };
        MarketDto {
            event_slug: m.event_slug.to_string(),
            title: m.title.clone(),
            local_date: m.local_date.to_string(),
            resolution_source: source,
            resolution_url: url,
            rules_sha256: m.rules.sha256.clone(),
            rules_excerpt: m.rules.text.chars().take(600).collect(),
            filters: m
                .resolution
                .filters
                .iter()
                .map(ObservationFilter::label)
                .collect(),
            filter_confirmed: inp.confirmed_filters.contains_key(&l.station),
            machine_tradable: m.resolution.is_machine_tradable(),
            review_status: inp
                .rules_review
                .get(&m.rules.sha256)
                .cloned()
                .unwrap_or_else(|| format!("{:?}", m.resolution.review).to_lowercase()),
            unrecognized_clauses: m.resolution.unrecognized_clauses.clone(),
            taker_fee_rate: f64::from(m.fees.taker_rate_micros) / 1_000_000.0,
            neg_risk: m.neg_risk,
            end_time_ms: m.end_time.map(ms),
            rows,
        }
    });
    LocationDto {
        location: l.location.to_string(),
        station: l.station.to_string(),
        timezone: l.timezone.clone(),
        local_date: l.local_date.to_string(),
        local_time: local(now, tz),
        current_temp_c: last.map(|p| p.temp.as_f64()),
        last_observation_ms: last.map(|p| ms(p.observed_at)),
        last_observation_age_s: last.map(|p| (now - p.observed_at).num_seconds()),
        last_raw: collector.and_then(|c| c.last_observation.as_ref().map(|o| o.raw_text.clone())),
        peak_watch: l.hint.peak_watch,
        has_exposure: l.hint.has_exposure,
        views,
        series,
        observations,
        collector: collector.map(|c| CollectorDto {
            active_provider: c.active_provider.as_ref().map(ToString::to_string),
            polls_total: c.polls_total,
            gate_closed_total: c.gate_closed_total,
            new_observations_total: c.new_observations_total,
            duplicates_total: c.duplicates_total,
            corrections_total: c.corrections_total,
            out_of_order_total: c.out_of_order_total,
            persist_failures_total: c.persist_failures_total,
            storage_ok: c.storage_ok,
            next_poll: c.next_poll.map(|d| PollDto {
                at_ms: ms(d.at),
                mode: format!("{:?}", d.mode).to_lowercase(),
                reason: format!("{:?}", d.reason).to_lowercase(),
                expected_report_ms: d.expected_report.map(ms),
            }),
        }),
        market: market_dto,
        forecast: l.forecast.as_ref().map(|f| ForecastDto {
            product: f.product.clone(),
            received_ms: ms(f.received_at),
            in_use: f.in_use,
            status: f.status.clone(),
            day_max_c: f.day_max_tenths.map(|t| f64::from(t) / 10.0),
            remaining_max_c: f.remaining_max_tenths.map(|t| f64::from(t) / 10.0),
            rise_c: f.rise_tenths.map(|t| f64::from(t) / 10.0),
            headroom_c: f
                .remaining_max_tenths
                .zip(l.views.iter().find_map(|v| {
                    v.state
                        .as_ref()
                        .and_then(|s| s.high.as_ref())
                        .map(|h| h.value.tenths())
                }))
                .map(|(m, h)| f64::from(m - h) / 10.0),
            hourly: {
                let day_start = wm_core::time::local_day_start(l.local_date, tz);
                f.hourly
                    .iter()
                    .map(|(t, v)| ForecastPointDto {
                        t_ms: ms(*t),
                        minute: i32::try_from((*t - day_start).num_minutes()).unwrap_or(0),
                        temp_c: f64::from(*v) / 10.0,
                    })
                    .collect()
            },
        }),
    }
}

/// Build the dashboard snapshot.
pub fn build(
    snap: &EngineSnapshot,
    inp: &DtoInputs<'_>,
    generated_at: DateTime<Utc>,
) -> DashboardSnapshot {
    let locations: Vec<LocationDto> = snap
        .locations
        .iter()
        .map(|l| location_dto(l, snap, inp))
        .collect();
    // Providers: collector detail wins over engine copies of the same (provider, scope).
    let mut providers: Vec<ProviderDto> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in inp.collectors.values() {
        for p in &c.providers {
            if seen.insert((
                p.provider.to_string(),
                p.scope.as_ref().map(ToString::to_string),
            )) {
                providers.push(provider_dto(p));
            }
        }
    }
    for p in snap.health.iter().chain(inp.extra_providers.iter()) {
        if seen.insert((
            p.provider.to_string(),
            p.scope.as_ref().map(ToString::to_string),
        )) {
            providers.push(provider_dto(p));
        }
    }
    providers.sort_by(|a, b| a.provider.cmp(&b.provider));

    let books: HashMap<&TokenId, &OrderBook> = snap
        .locations
        .iter()
        .flat_map(|l| l.books.iter())
        .map(|b| (&b.token, b))
        .collect();
    let positions = snap
        .positions
        .iter()
        .filter(|p| p.shares.micros() > 0 || !p.realized_pnl.is_zero())
        .map(|p| {
            let mark = books
                .get(&p.instrument.token)
                .and_then(|b| b.best_bid())
                .map(|l| l.price.as_f64());
            PositionDto {
                event_slug: p.instrument.event_slug.to_string(),
                bucket: p.instrument.bucket.label(),
                side: p.instrument.outcome_side.as_str().to_owned(),
                shares: p.shares.as_f64(),
                cost_usd: p.cost_basis.as_f64(),
                avg_cost: p.avg_cost(),
                mark,
                unrealized_usd: mark.map(|m| m * p.shares.as_f64() - p.cost_basis.as_f64()),
                realized_usd: p.realized_pnl.as_f64(),
            }
        })
        .collect();
    let orders = snap
        .orders
        .iter()
        .map(|o| OrderDto {
            client_order_id: o.client_order_id.to_string(),
            strategy: o.strategy.to_string(),
            bucket: o.bucket.label(),
            outcome: o.outcome_side.as_str().to_owned(),
            side: format!("{:?}", o.side).to_uppercase(),
            limit: o.limit_price.as_f64(),
            shares: o.shares.as_f64(),
            filled: o.filled.as_f64(),
            avg_price: o.avg_price.map(|p| p.as_f64()),
            fees_usd: o.fees.as_f64(),
            status: o.status.as_str().to_owned(),
            reason: o.reason.clone(),
            created_ms: ms(o.created_at),
            updated_ms: ms(o.updated_at),
        })
        .collect();
    let decisions = snap
        .decisions
        .iter()
        .take(80)
        .map(|d| DecisionDto {
            id: d.decision_id.0,
            at_ms: ms(d.at),
            strategy: d.strategy.to_string(),
            summary: d.summary.clone(),
            approved: d.approved,
            reasons: d.reasons.clone(),
            details: evaluation_lines(&d.outputs),
        })
        .collect();

    // Risk checks summary (what would block a new weather-dependent position right now).
    let mut checks = vec![
        CheckDto {
            name: "Kill switch".into(),
            ok: snap.kill_switch.is_none(),
            detail: snap
                .kill_switch
                .clone()
                .unwrap_or_else(|| "released".into()),
        },
        CheckDto {
            name: "Audit storage".into(),
            ok: snap.storage_ok,
            detail: if snap.storage_ok {
                "ok".into()
            } else {
                "unavailable — trading blocked".into()
            },
        },
        CheckDto {
            name: "Execution venue".into(),
            ok: snap.execution_ok,
            detail: format!("{} (simulated fills)", snap.mode),
        },
        CheckDto {
            name: "Probability model".into(),
            ok: inp.model.loaded() && snap.model_id != "no-edge",
            detail: if inp.model.loaded() {
                snap.model_id.clone()
            } else {
                inp.model.detail.clone()
            },
        },
        CheckDto {
            name: "Live trading".into(),
            ok: true,
            detail: "disabled in this build (Phase 14 gate)".into(),
        },
    ];
    for l in &locations {
        let age = l
            .last_observation_age_s
            .map_or("no data".into(), |s| format!("{} min", s / 60));
        checks.push(CheckDto {
            name: format!("{} weather freshness", l.station),
            ok: l
                .last_observation_age_s
                .is_some_and(|s| s <= snap.risk.max_weather_age_minutes * 60),
            detail: age,
        });
        let healthy = providers
            .iter()
            .any(|p| p.scope.as_deref() == Some(&l.station) && p.state == "healthy");
        checks.push(CheckDto {
            name: format!("{} observation source", l.station),
            ok: healthy,
            detail: if healthy {
                "healthy".into()
            } else {
                "no healthy source — fail closed".into()
            },
        });
        if let Some(m) = &l.market {
            checks.push(CheckDto {
                name: format!("{} resolution rules", l.station),
                ok: m.machine_tradable,
                detail: if m.filter_confirmed {
                    "filter confirmed".into()
                } else {
                    "filter unconfirmed (all views must agree)".into()
                },
            });
            let fresh = m
                .rows
                .iter()
                .filter_map(|r| r.book_age_ms)
                .min()
                .is_some_and(|a| a <= snap.risk.max_book_age_ms);
            checks.push(CheckDto {
                name: format!("{} market data", l.station),
                ok: fresh,
                detail: if fresh {
                    "fresh".into()
                } else {
                    "stale or missing books".into()
                },
            });
        } else {
            checks.push(CheckDto {
                name: format!("{} market", l.station),
                ok: false,
                detail: "no market discovered for today".into(),
            });
        }
    }
    let risk = RiskDto {
        position_size_usd: snap.risk.position_size_usd.as_f64(),
        global_worst_case_usd: snap.exposure.global_worst_case.as_f64(),
        global_limit_usd: snap.risk.global_max_exposure_usd.as_f64(),
        capital_deployed_usd: snap.exposure.capital_deployed.as_f64(),
        daily_new_exposure_usd: snap.daily_new_exposure.as_f64(),
        daily_new_limit_usd: snap.risk.max_daily_new_exposure_usd.map(|u| u.as_f64()),
        daily_realized_pnl_usd: snap.daily_realized_pnl.as_f64(),
        daily_loss_limit_usd: snap.risk.max_daily_loss_usd.map(|u| u.as_f64()),
        realized_pnl_total_usd: snap.realized_pnl_total.as_f64(),
        max_price: snap.risk.max_price.as_f64(),
        max_spread: snap.risk.max_spread.as_f64(),
        max_weather_age_min: snap.risk.max_weather_age_minutes,
        per_event: snap
            .exposure
            .per_event
            .iter()
            .map(|(s, u)| (s.to_string(), u.as_f64()))
            .collect(),
        checks,
    };
    let fee = wm_core::market::FeeSchedule::taker(50_000);
    DashboardSnapshot {
        api_version: API_VERSION,
        generated_at_ms: ms(generated_at),
        engine_time_ms: ms(snap.now.max(DateTime::<Utc>::UNIX_EPOCH)),
        mode: snap.mode.as_str().to_owned(),
        demo: inp.demo,
        version: wm_core::VERSION.to_owned(),
        instance: inp.instance.to_owned(),
        run_id: snap.run_id.to_string(),
        model_id: snap.model_id.clone(),
        model: inp.model.clone(),
        kill_switch: snap.kill_switch.clone(),
        storage_ok: snap.storage_ok,
        execution_ok: snap.execution_ok,
        live_trading_enabled: false,
        engine: EngineDto {
            events_total: snap.stats.events_total,
            evaluations_total: snap.stats.evaluations_total,
            proposals_total: snap.stats.proposals_total,
            approvals_total: snap.stats.approvals_total,
            rejections_total: snap.stats.rejections_total,
            fills_total: snap.stats.fills_total,
            last_handle_micros: snap.stats.last_handle_micros,
            max_handle_micros: snap.stats.max_handle_micros,
            last_seq: snap.stats.last_seq,
            events_by_kind: snap
                .stats
                .events_by_kind
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        },
        locations,
        providers,
        market_stream: inp.stream.map(|s| StreamDto {
            connected: s.connected,
            subscribed_assets: s.subscribed_assets,
            messages_total: s.messages_total,
            reconnects_total: s.reconnects_total,
            last_message_ms: s.last_message_at.map(ms),
            last_error: s.last_error.clone(),
        }),
        risk,
        positions,
        orders,
        decisions,
        alerts: inp.alerts.to_vec(),
        break_even: break_even_table(&research_price_grid(), &fee, Price::ZERO)
            .into_iter()
            .map(|r| BreakEvenDto {
                price: r.price.as_f64(),
                fee_per_share: r.fee_per_share,
                break_even_probability: r.break_even_probability,
                wins_to_recover_one_loss: r.wins_to_recover_one_loss,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn evaluation_lines_reach_the_dashboard() {
        let out = serde_json::json!({ "evaluations": [
            "A 21°C YES · ask 0.97 · p 0.955 (model 0.970, market 0.940) · EV -0.0215 — edge",
            7,
            "D 20°C NO · no ask — no ask"
        ]});
        assert_eq!(
            super::evaluation_lines(&out),
            vec![
                "A 21°C YES · ask 0.97 · p 0.955 (model 0.970, market 0.940) · EV -0.0215 — edge"
                    .to_owned(),
                "D 20°C NO · no ask — no ask".to_owned()
            ]
        );
        assert!(super::evaluation_lines(&serde_json::json!({ "risk": [] })).is_empty());
        assert!(super::evaluation_lines(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn dashboard_windows_match_the_kernel() {
        assert_eq!(
            wm_dashboard_api::CONFIRMATION_WINDOWS,
            wm_strategy::CONFIRMATION_WINDOWS
        );
    }
}
