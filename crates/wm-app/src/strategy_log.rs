//! One strategy's log: everything about it on one page of Markdown, made to
//! be copied from the dashboard and pasted into a conversation.
//!
//! * **Now** (the latest snapshot): its settings, strategy F's slot, the
//!   gates that block trading, its evaluation of every bucket of today's
//!   market with the prices and what blocks it.
//! * **This run** (the snapshot's decision log): its proposals with the risk
//!   verdicts, its lines of the routine evaluations, its orders.
//! * **History** (the database, `report paper`): per day its evaluations,
//!   signals, blockers, closest calls, proposals, orders and settled P&L.

use crate::paper_report::{self, DayReport, PaperReport};
use std::fmt::Write as _;
use wm_dashboard_api::{DashboardSnapshot, EvaluationDto, LocationDto, StrategyDto, ViewDto};

/// Routine-evaluation lines of this run listed at most.
const TRAIL_LINES: usize = 30;

/// What the log says about the database history.
pub enum History<'a> {
    /// The latest `days` days of the report.
    Report {
        report: &'a PaperReport,
        days: usize,
    },
    /// No database configured (demo, or no `WM_DATABASE_URL`).
    Unavailable,
    Failed(String),
}

/// The clock of "This run": the stations' time zone when they share one —
/// the clock of "Now" and of the history — else UTC.
fn clock(snap: &DashboardSnapshot) -> Option<chrono_tz::Tz> {
    let first = &snap.locations.first()?.timezone;
    if snap.locations.iter().any(|l| &l.timezone != first) {
        return None;
    }
    first.parse().ok()
}

fn zone(tz: Option<chrono_tz::Tz>) -> &'static str {
    tz.map_or("UTC", |z| z.name())
}

/// Date and time of a decision on that clock; a run spans days.
fn stamp(ms: i64, tz: Option<chrono_tz::Tz>) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(ms) else {
        return "?".to_owned();
    };
    match tz {
        Some(z) => t.with_timezone(&z).format("%m-%d %H:%M:%S").to_string(),
        None => t.format("%m-%d %H:%M:%S UTC").to_string(),
    }
}

fn utc_datetime(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || "?".to_owned(),
        |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "–".to_owned(), |x| format!("{x:.digits$}"))
}

fn signed(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "–".to_owned(), |x| format!("{x:+.digits$}"))
}

/// Table cells must not break the table.
fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

/// Does a strategy id (`F_peak_slot`) or letter (`F`) name this strategy?
pub fn matches(s: &StrategyDto, key: &str) -> bool {
    s.id.eq_ignore_ascii_case(key) || s.letter.eq_ignore_ascii_case(key)
}

/// Render the log of `strategy`.
pub fn render(snap: &DashboardSnapshot, strategy: &StrategyDto, history: History<'_>) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# Strategy {} — {} (`{}`)\n",
        strategy.letter, strategy.name, strategy.id
    );
    let _ = writeln!(
        s,
        "Weather Machine {} · {}{} · run {} · engine time {} · model `{}`\n",
        snap.version,
        snap.mode,
        if snap.demo {
            " (DEMO, synthetic data)"
        } else {
            ""
        },
        snap.run_id.chars().take(8).collect::<String>(),
        utc_datetime(snap.engine_time_ms),
        snap.model_id
    );
    let _ = writeln!(
        s,
        "**{}.** {}\n",
        if strategy.enabled {
            "Enabled"
        } else {
            "Disabled — it evaluates and trades nothing"
        },
        strategy.summary
    );
    settings(&mut s, strategy);
    peak_slot(&mut s, strategy);
    for l in &snap.locations {
        now(&mut s, snap, l, strategy);
    }
    this_run(&mut s, snap, strategy);
    match history {
        History::Report { report, days } => history_section(&mut s, report, days, strategy),
        History::Unavailable => {
            s.push_str("\n## History\n\nNo database: the day-by-day history needs the paper service's database.\n");
        }
        History::Failed(e) => {
            let _ = writeln!(s, "\n## History\n\nThe database history failed: {e}");
        }
    }
    s
}

fn settings(s: &mut String, strategy: &StrategyDto) {
    if strategy.settings.is_empty() {
        return;
    }
    s.push_str("## Settings\n\n| setting | value |\n|---|---|\n");
    for (k, v) in &strategy.settings {
        let _ = writeln!(s, "| {} | {} |", cell(k), cell(v));
    }
    s.push('\n');
}

fn peak_slot(s: &mut String, strategy: &StrategyDto) {
    let Some(p) = &strategy.peak_slot else {
        return;
    };
    let _ = writeln!(s, "## Peak slot\n\nSlots: {}.\n", p.source);
    for t in &p.today {
        let _ = writeln!(
            s,
            "- Today in {}: the {} slot is {}–{} local; it is {} — {}.",
            t.location, t.season, t.start, t.end, t.local_time, t.status
        );
    }
    s.push_str("\n| season | slot | days | mean | median | 90% | later than the slot |\n|---|---|---:|---:|---:|---:|---:|\n");
    for x in &p.seasons {
        let _ = writeln!(
            s,
            "| {} | {}–{} | {} | {} | {} | {} | {} |",
            x.season,
            x.start,
            x.end,
            if x.days == 0 {
                "fallback".to_owned()
            } else {
                x.days.to_string()
            },
            x.mean.as_deref().unwrap_or("–"),
            x.median.as_deref().unwrap_or("–"),
            x.q90.as_deref().unwrap_or("–"),
            x.later_than_slot
                .map_or_else(|| "–".to_owned(), |v| format!("{:.0}%", 100.0 * v))
        );
    }
    s.push('\n');
}

fn now(s: &mut String, snap: &DashboardSnapshot, l: &LocationDto, strategy: &StrategyDto) {
    let _ = writeln!(
        s,
        "## Now — {} · {} · {} {} local\n",
        l.location.to_uppercase(),
        l.station,
        l.local_date,
        l.local_time
    );
    let view = l.views.first();
    let _ = writeln!(
        s,
        "- Temperature {} °C; high so far {} ({}); last report {}.",
        opt(l.current_temp_c, 1),
        view.and_then(|v| v.high_whole)
            .map_or_else(|| "–".to_owned(), |h| format!("{h} °C")),
        view.and_then(ViewDto::high_times)
            .unwrap_or_else(|| "–".to_owned()),
        l.last_observation_age_s
            .map_or_else(|| "none yet".to_owned(), |a| format!("{} min ago", a / 60))
    );
    if let Some(f) = &l.forecast {
        let _ = writeln!(
            s,
            "- Day-1 forecast: day max {} °C, {} °C above the high still to come ({}).",
            opt(f.day_max_c, 1),
            signed(f.headroom_c, 1),
            if f.in_use {
                "used by the model"
            } else {
                "not used by the model"
            }
        );
    }
    let blocking: Vec<String> = snap
        .risk
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| format!("{} ({})", c.name, c.detail))
        .collect();
    if blocking.is_empty() {
        s.push_str("- Pre-trade gates: all pass.\n");
    } else {
        let _ = writeln!(
            s,
            "- Pre-trade gates blocking every new position: {}.",
            blocking.join("; ")
        );
    }
    let Some(m) = &l.market else {
        s.push_str("- No market discovered for today: nothing to evaluate.\n\n");
        return;
    };
    let _ = writeln!(s, "- Market: `{}`.\n", m.event_slug);
    let high_bucket = m
        .rows
        .iter()
        .find(|r| r.contains_high)
        .map(|r| r.label.as_str());
    let evals: Vec<&EvaluationDto> = l
        .evaluations
        .iter()
        .filter(|e| e.strategy == strategy.id)
        .collect();
    if evals.is_empty() {
        s.push_str("No evaluation of this strategy right now (disabled, no model, incomplete views, or no report yet since the start).\n\n");
        return;
    }
    s.push_str("| bucket | side | ask | bid | p used | model | market | EV/share | verdict |\n|---|---|---:|---:|---:|---:|---:|---:|---|\n");
    for e in evals {
        let _ = writeln!(
            s,
            "| {}{} | {} | {} | {} | {} | {} | {} | {} | {} |",
            cell(&e.bucket),
            if high_bucket == Some(e.bucket.as_str()) {
                " ◀ high"
            } else {
                ""
            },
            e.side,
            opt(e.ask, 3),
            opt(e.bid, 3),
            opt(e.p_win, 3),
            opt(e.model_p, 3),
            opt(e.market_p, 3),
            signed(e.ev, 4),
            if e.signal {
                "**SIGNAL**".to_owned()
            } else {
                cell(&e.blockers.join("; "))
            }
        );
    }
    s.push('\n');
}

fn this_run(s: &mut String, snap: &DashboardSnapshot, strategy: &StrategyDto) {
    let tz = clock(snap);
    let _ = writeln!(
        s,
        "## This run (since the service started; newest first; {} time)\n\n### Proposals and risk verdicts\n",
        zone(tz)
    );
    let proposals: Vec<_> = snap
        .decisions
        .iter()
        .filter(|d| d.strategy == strategy.id)
        .collect();
    if proposals.is_empty() {
        s.push_str("None.\n");
    }
    for d in proposals {
        let _ = writeln!(
            s,
            "- {} **{}** — {}{}",
            stamp(d.at_ms, tz),
            if d.approved { "APPROVED" } else { "REJECTED" },
            d.summary,
            if d.reasons.is_empty() {
                String::new()
            } else {
                format!(" — {}", d.reasons.join("; "))
            }
        );
    }
    let _ = writeln!(
        s,
        "\n### Its lines in the routine evaluations (latest {TRAIL_LINES})\n"
    );
    let trail: Vec<(i64, &String)> = snap
        .decisions
        .iter()
        .filter(|d| d.strategy == "evaluation")
        .flat_map(|d| {
            d.details
                .iter()
                .filter(|l| strategy.owns_line(l))
                .map(move |l| (d.at_ms, l))
        })
        .take(TRAIL_LINES)
        .collect();
    if trail.is_empty() {
        s.push_str("None in the dashboard's decision log.\n");
    }
    for (at, line) in trail {
        let _ = writeln!(s, "- {} {line}", stamp(at, tz));
    }
    s.push_str("\n### Orders\n\n");
    let orders: Vec<_> = snap
        .orders
        .iter()
        .filter(|o| o.strategy == strategy.id)
        .collect();
    if orders.is_empty() {
        s.push_str("None.\n");
    } else {
        let _ = writeln!(
            s,
            "| created ({}) | bucket | outcome | side | limit | shares | filled | avg | fees | status | reason |\n|---|---|---|---|---:|---:|---:|---:|---:|---|---|",
            zone(tz)
        );
        for o in orders {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {:.3} | {:.0} | {:.0} | {} | {} | {} | {} |",
                stamp(o.created_ms, tz),
                cell(&o.bucket),
                o.outcome,
                o.side,
                o.limit,
                o.shares,
                o.filled,
                opt(o.avg_price, 3),
                paper_report::usd(o.fees_usd),
                o.status,
                cell(o.reason.as_deref().unwrap_or(""))
            );
        }
    }
}

fn history_section(s: &mut String, r: &PaperReport, days: usize, strategy: &StrategyDto) {
    // Newest first.
    let shown: Vec<&DayReport> = r.days.iter().rev().take(days.max(1)).collect();
    let (Some(last), Some(first)) = (shown.first(), shown.last()) else {
        s.push_str("\n## History\n\nNo day in the database yet.\n");
        return;
    };
    let _ = writeln!(
        s,
        "\n## History — {} → {} (database; {} time; outcomes judged by the METAR high)\n",
        first.date, last.date, r.timezone
    );
    let signals: usize = shown
        .iter()
        .flat_map(|d| d.strategies.iter())
        .filter(|x| x.strategy == strategy.letter)
        .map(|x| x.signals.len())
        .sum();
    let proposals: Vec<_> = shown
        .iter()
        .flat_map(|d| d.proposals.iter())
        .filter(|p| p.strategy == strategy.id)
        .collect();
    let orders = shown
        .iter()
        .flat_map(|d| d.orders.iter())
        .filter(|o| o.strategy == strategy.id)
        .count();
    let pnl: f64 = shown
        .iter()
        .filter_map(|d| d.strategy_pnl.get(&strategy.id))
        .sum();
    let _ = writeln!(
        s,
        "Totals: {signals} signal(s), {} proposal(s) ({} approved), {orders} order(s), settled paper P&L {}.\n",
        proposals.len(),
        proposals.iter().filter(|p| p.approved).count(),
        paper_report::usd(pnl)
    );
    for d in shown {
        day(s, d, strategy);
    }
}

fn day(s: &mut String, d: &DayReport, strategy: &StrategyDto) {
    let _ = writeln!(
        s,
        "### {}{} — METAR high {}, winner {}\n",
        d.date,
        if d.in_progress { " (in progress)" } else { "" },
        d.weather
            .high_c
            .map_or_else(|| "–".to_owned(), |h| format!("{h} °C")),
        d.market
            .as_ref()
            .and_then(|m| m.winner.clone())
            .unwrap_or_else(|| "–".to_owned())
    );
    let mine = d.strategies.iter().find(|x| x.strategy == strategy.letter);
    match mine {
        None => s.push_str("- No evaluation of this strategy.\n"),
        Some(x) => {
            let _ = writeln!(
                s,
                "- Evaluated {} bucket line(s); {} signal(s).",
                x.lines,
                x.signals.len()
            );
            for c in &x.signals {
                let _ = writeln!(s, "  - signal: {}", paper_report::call_text(c));
            }
            if !x.blockers.is_empty() {
                let parts: Vec<String> = x
                    .blockers
                    .iter()
                    .map(|b| format!("{}× “{}”", b.count, b.example))
                    .collect();
                let _ = writeln!(s, "- Blockers (most frequent): {}.", parts.join("; "));
            }
            for c in &x.closest {
                let _ = writeln!(s, "- Closest call: {}", paper_report::call_text(c));
            }
        }
    }
    for p in d.proposals.iter().filter(|p| p.strategy == strategy.id) {
        let _ = writeln!(
            s,
            "- {} **{}** {}{}",
            p.at,
            if p.approved { "APPROVED" } else { "REJECTED" },
            p.summary,
            if p.reasons.is_empty() {
                String::new()
            } else {
                format!(" — {}", p.reasons.join("; "))
            }
        );
    }
    for o in d.orders.iter().filter(|o| o.strategy == strategy.id) {
        let _ = writeln!(
            s,
            "- Order {} {} {} {} @ {:.3}: {} of {:.0} filled{}, {}{}",
            o.at,
            o.side,
            o.outcome_side,
            o.bucket,
            o.limit,
            o.filled,
            o.shares,
            o.avg_price
                .map_or_else(String::new, |a| format!(" at {a:.3}")),
            o.status,
            o.reason
                .as_ref()
                .map_or_else(String::new, |r| format!(" ({r})"))
        );
    }
    if let Some(p) = d.strategy_pnl.get(&strategy.id) {
        let _ = writeln!(s, "- Settled paper P&L: {}", paper_report::usd(*p));
    }
    s.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_dashboard_api::*;

    fn f() -> StrategyDto {
        StrategyDto {
            id: "F_peak_slot".into(),
            letter: "F".into(),
            name: "Peak slot".into(),
            enabled: true,
            summary: "Buys YES on the high's bucket inside the peak slot.".into(),
            settings: vec![
                ("max_price".into(), "0.95".into()),
                ("shares".into(), "100".into()),
            ],
            peak_slot: Some(PeakSlotDto {
                source: "median → 90% of the local time the day's high was first reported".into(),
                today: vec![SlotTodayDto {
                    location: "amsterdam".into(),
                    season: "autumn".into(),
                    local_time: "11:12".into(),
                    start: "13:25".into(),
                    end: "15:26".into(),
                    inside: false,
                    status: "before the slot: it starts in 2 h 13 min".into(),
                }],
                seasons: vec![SeasonSlotDto {
                    season: "autumn".into(),
                    start: "13:25".into(),
                    end: "15:26".into(),
                    days: 1987,
                    mean: Some("13:40".into()),
                    median: Some("13:25".into()),
                    q90: Some("15:25".into()),
                    later_than_slot: Some(0.1),
                }],
            }),
        }
    }

    fn eval(strategy: &str, bucket: &str, ask: f64, blockers: &[&str]) -> EvaluationDto {
        EvaluationDto {
            strategy: strategy.into(),
            bucket: bucket.into(),
            side: "YES".into(),
            ask: Some(ask),
            bid: None,
            p_win: Some(0.5),
            model_p: None,
            market_p: None,
            ev: Some(-0.1),
            break_even: None,
            signal: blockers.is_empty(),
            blockers: blockers.iter().map(|b| (*b).to_owned()).collect(),
        }
    }

    fn snapshot() -> DashboardSnapshot {
        DashboardSnapshot {
            version: "0.1.0".into(),
            mode: "paper".into(),
            run_id: "01a0f109-aaaa".into(),
            model_id: "empirical-EHAM".into(),
            engine_time_ms: 1_790_000_000_000,
            risk: RiskDto {
                checks: vec![
                    CheckDto {
                        name: "Kill switch".into(),
                        ok: true,
                        detail: "released".into(),
                    },
                    CheckDto {
                        name: "EHAM observation source".into(),
                        ok: false,
                        detail: "no healthy source — fail closed".into(),
                    },
                ],
                ..RiskDto::default()
            },
            locations: vec![LocationDto {
                location: "amsterdam".into(),
                station: "EHAM".into(),
                local_date: "2026-09-30".into(),
                local_time: "11:12".into(),
                current_temp_c: Some(21.0),
                last_observation_age_s: Some(17 * 60),
                views: vec![ViewDto {
                    high_whole: Some(21),
                    high_local: Some("10:55".into()),
                    high_first_local: Some("09:25".into()),
                    retests: 2,
                    ..ViewDto::default()
                }],
                market: Some(MarketDto {
                    event_slug: "highest-temperature-in-amsterdam-on-september-30-2026".into(),
                    rows: vec![LadderRowDto {
                        label: "21°C".into(),
                        contains_high: true,
                        ..LadderRowDto::default()
                    }],
                    ..MarketDto::default()
                }),
                evaluations: vec![
                    eval(
                        "F_peak_slot",
                        "21°C",
                        0.31,
                        &[
                            "11:12 outside the autumn slot 13:25–15:26",
                            "ask 0.31 not above 0.90",
                        ],
                    ),
                    eval("E_book_confirmed_high", "21°C", 0.31, &["E's own blocker"]),
                ],
                ..LocationDto::default()
            }],
            decisions: vec![
                DecisionDto {
                    id: 9,
                    at_ms: 1_790_000_000_000,
                    strategy: "F_peak_slot".into(),
                    summary: "BUY YES 21°C 100 @ ≤ 0.94".into(),
                    approved: false,
                    reasons: vec!["PositionSize: cost $94.00 > position size $10.00".into()],
                    details: vec![],
                },
                DecisionDto {
                    id: 8,
                    at_ms: 1_789_999_000_000,
                    strategy: "evaluation".into(),
                    summary: "evaluated 22 bucket(s)".into(),
                    approved: false,
                    reasons: vec![],
                    details: vec![
                        "F 21°C YES · ask 0.31 — 10:55 outside the autumn slot 13:25–15:26".into(),
                        "E 21°C YES · ask 0.31 — E line".into(),
                    ],
                },
            ],
            orders: vec![OrderDto {
                client_order_id: "wm-1".into(),
                strategy: "E_book_confirmed_high".into(),
                ..OrderDto::default()
            }],
            ..DashboardSnapshot::default()
        }
    }

    #[test]
    fn the_log_holds_only_this_strategy_and_everything_about_it() {
        let md = render(&snapshot(), &f(), History::Unavailable);
        for part in [
            "# Strategy F — Peak slot (`F_peak_slot`)",
            "**Enabled.** Buys YES on the high's bucket inside the peak slot.",
            "| max_price | 0.95 |",
            "- Today in amsterdam: the autumn slot is 13:25–15:26 local; it is 11:12 — before the slot: it starts in 2 h 13 min.",
            "| autumn | 13:25–15:26 | 1987 | 13:40 | 13:25 | 15:25 | 10% |",
            "## Now — AMSTERDAM · EHAM · 2026-09-30 11:12 local",
            "- Temperature 21.0 °C; high so far 21 °C (first reported 09:25, last 10:55, 2 retests); last report 17 min ago.",
            "- Pre-trade gates blocking every new position: EHAM observation source (no healthy source — fail closed).",
            "| 21°C ◀ high | YES | 0.310 | – | 0.500 | – | – | -0.1000 | 11:12 outside the autumn slot 13:25–15:26; ask 0.31 not above 0.90 |",
            "**REJECTED** — BUY YES 21°C 100 @ ≤ 0.94 — PositionSize: cost $94.00 > position size $10.00",
            "UTC F 21°C YES · ask 0.31 — 10:55 outside the autumn slot 13:25–15:26",
            "No database: the day-by-day history",
        ] {
            assert!(md.contains(part), "missing {part:?} in\n{md}");
        }
        // Nothing of the other strategies.
        assert!(!md.contains("E's own blocker"), "{md}");
        assert!(!md.contains("E line"), "{md}");
        assert!(!md.contains("wm-1"), "{md}");
        assert!(md.contains("### Orders\n\nNone."), "{md}");
    }

    #[test]
    fn a_disabled_strategy_says_so_and_ids_or_letters_find_it() {
        let mut e = f();
        e.enabled = false;
        e.peak_slot = None;
        let md = render(
            &snapshot(),
            &e,
            History::Failed("connection refused".into()),
        );
        assert!(
            md.contains("**Disabled — it evaluates and trades nothing.**"),
            "{md}"
        );
        assert!(!md.contains("## Peak slot"));
        assert!(md.contains("The database history failed: connection refused"));
        assert!(matches(&f(), "F_peak_slot"));
        assert!(matches(&f(), "f"));
        assert!(!matches(&f(), "E"));
    }

    #[test]
    fn this_run_and_the_history_share_the_stations_clock() {
        let mut snap = snapshot();
        snap.locations[0].timezone = "Europe/Amsterdam".into();
        let md = render(&snap, &f(), History::Unavailable);
        // 1_790_000_000_000 ms = 2026-09-21 14:13:20 UTC = 16:13:20 CEST.
        for part in [
            "## This run (since the service started; newest first; Europe/Amsterdam time)",
            "- 09-21 16:13:20 **REJECTED** — BUY YES 21°C",
            "- 09-21 15:56:40 F 21°C YES · ask 0.31 — 10:55 outside",
        ] {
            assert!(md.contains(part), "missing {part:?} in\n{md}");
        }
        assert!(!md.contains("UTC **"), "{md}");
        // Stations on different clocks: UTC, and every time says so.
        let mut two = snap.clone();
        two.locations.push(LocationDto {
            location: "new-york".into(),
            timezone: "America/New_York".into(),
            ..LocationDto::default()
        });
        let md = render(&two, &f(), History::Unavailable);
        assert!(md.contains("newest first; UTC time)"), "{md}");
        assert!(md.contains("- 09-21 14:13:20 UTC **REJECTED**"), "{md}");
    }

    #[test]
    fn cells_cannot_break_the_table() {
        assert_eq!(cell("a|b\nc"), "a/b c");
    }
}
