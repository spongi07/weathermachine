//! Zero-JavaScript dashboard (`/lite`): server-rendered HTML that refreshes
//! itself. Works in any browser, over SSH tunnels, and when the WASM bundle is
//! unavailable. Everything shown is escaped; nothing is interactive.

use std::fmt::Write;
use wm_dashboard_api::{DashboardSnapshot, LadderRowDto, LocationDto};

/// HTML-escape text content and attribute values.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn opt_f(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "—".to_owned(), |x| format!("{x:.digits$}"))
}

fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| "—".to_owned(), |x| format!("{:.1}%", x * 100.0))
}

fn signed(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| "—".to_owned(), |x| format!("{x:+.digits$}"))
}

fn ms_to_utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || "—".to_owned(),
        |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn class_for(ok: bool) -> &'static str {
    if ok { "ok" } else { "bad" }
}

const STYLE: &str = r#"
:root{color-scheme:dark;--bg:#0b0e13;--panel:#121722;--line:#222b3a;--text:#d6deeb;--muted:#7f8ea3;--ok:#3fb950;--warn:#d29922;--bad:#f85149;--accent:#58a6ff}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:13px/1.45 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
header{display:flex;flex-wrap:wrap;gap:12px;align-items:center;padding:10px 16px;border-bottom:1px solid var(--line);background:#0e131c}
header h1{font-size:14px;margin:0;letter-spacing:.08em}
.pill{padding:2px 8px;border-radius:999px;border:1px solid var(--line);color:var(--muted)}
.pill.ok{color:var(--ok);border-color:#2b5a33}.pill.bad{color:var(--bad);border-color:#6b2a2a}.pill.warn{color:var(--warn);border-color:#6b5217}
.banner{background:#3b2a05;color:#f2cc60;padding:8px 16px;border-bottom:1px solid #6b5217}
.kill{background:#4a0f0f;color:#ffb3ad;padding:8px 16px;border-bottom:1px solid #7a1f1f;font-weight:bold}
.note{background:#0c1a2b;color:#9cc7ff;padding:6px 16px;border-bottom:1px solid #1d3553;font-size:12px}
main{padding:12px 16px;display:grid;gap:12px}
section{background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:10px 12px;overflow-x:auto}
h2{font-size:12px;margin:0 0 8px;color:var(--muted);text-transform:uppercase;letter-spacing:.1em}
table{border-collapse:collapse;width:100%}th,td{padding:3px 8px;border-bottom:1px solid var(--line);text-align:right;white-space:nowrap}
th{color:var(--muted);font-weight:normal}td:first-child,th:first-child{text-align:left}
.ok{color:var(--ok)}.bad{color:var(--bad)}.warn{color:var(--warn)}.muted{color:var(--muted)}.hl{background:#16233a}
.kpis{display:flex;flex-wrap:wrap;gap:18px}.kpi b{display:block;font-size:18px}.kpi span{color:var(--muted)}
.wrap{white-space:normal;text-align:left}
footer{padding:10px 16px;color:var(--muted)}
a{color:var(--accent)}
"#;

fn ladder_row(out: &mut String, r: &LadderRowDto) {
    let edge_class = match r.edge {
        Some(e) if e > 0.02 => "ok",
        Some(e) if e < -0.02 => "bad",
        _ => "muted",
    };
    let _ = write!(
        out,
        "<tr class=\"{}\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"{edge_class}\">{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"wrap\">{}</td></tr>",
        if r.contains_high { "hl" } else { "" },
        esc(&r.label),
        opt_f(r.yes_bid, 3),
        opt_f(r.yes_ask, 3),
        opt_f(r.no_bid, 3),
        opt_f(r.no_ask, 3),
        pct(r.implied_p),
        pct(r.model_p),
        signed(r.edge.map(|e| e * 100.0), 1),
        signed(r.yes_ev, 4),
        signed(r.no_ev, 4),
        if r.position_shares.abs() > 0.0 {
            format!("{:+.2}", r.position_shares)
        } else {
            "—".into()
        },
        esc(&if r.signals.is_empty() {
            r.blockers.first().cloned().unwrap_or_default()
        } else {
            r.signals.join(", ")
        }),
    );
}

fn location(out: &mut String, l: &LocationDto) {
    let _ = write!(
        out,
        "<section><h2>{} · {} · {} {}</h2><div class=\"kpis\">",
        esc(&l.location),
        esc(&l.station),
        esc(&l.local_date),
        esc(&l.local_time)
    );
    let current = opt_f(l.current_temp_c, 1);
    let age = l
        .last_observation_age_s
        .map_or_else(|| "no data".to_owned(), |s| format!("{} min ago", s / 60));
    let _ = write!(
        out,
        "<div class=\"kpi\"><b>{current} °C</b><span>current · {age}</span></div>"
    );
    for v in &l.views {
        let _ = write!(
            out,
            "<div class=\"kpi\"><b>{} °C</b><span>{} high @ {} · {} · +{} min · windows {}</span></div>",
            v.high_whole
                .map_or_else(|| "—".to_owned(), |h| h.to_string()),
            esc(&v.label),
            esc(v.high_local.as_deref().unwrap_or("—")),
            esc(v.trajectory.as_deref().unwrap_or("—")),
            v.minutes_since_high
                .map_or_else(|| "—".to_owned(), |m| m.to_string()),
            if v.windows_met.is_empty() {
                "none".to_owned()
            } else {
                v.windows_met
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("/")
            },
        );
    }
    let _ = write!(
        out,
        "<div class=\"kpi\"><b class=\"{}\">{}</b><span>peak watch</span></div>",
        if l.peak_watch { "warn" } else { "muted" },
        if l.peak_watch { "ON" } else { "off" }
    );
    out.push_str("</div>");
    if let Some(raw) = &l.last_raw {
        let _ = write!(out, "<p class=\"muted\">{}</p>", esc(raw));
    }
    match &l.market {
        Some(m) => {
            let _ = write!(
                out,
                "<p>{} · resolution: {} · filters: {} ({}) · rules <span class=\"{}\">{}</span> · review: {}</p>",
                esc(&m.title),
                esc(&m.resolution_source),
                esc(&m.filters.join(", ")),
                if m.filter_confirmed {
                    "confirmed"
                } else {
                    "unconfirmed"
                },
                class_for(m.machine_tradable),
                if m.machine_tradable {
                    "machine-tradable"
                } else {
                    "NOT tradable"
                },
                esc(&m.review_status),
            );
            out.push_str("<table><tr><th>Bucket</th><th>YES bid</th><th>YES ask</th><th>NO bid</th><th>NO ask</th><th>Implied</th><th>Model</th><th>Edge pp</th><th>EV yes</th><th>EV no</th><th>Pos</th><th>Signal / blocker</th></tr>");
            for r in &m.rows {
                ladder_row(out, r);
            }
            out.push_str("</table>");
        }
        None => out.push_str("<p class=\"warn\">No market discovered for today.</p>"),
    }
    out.push_str("<table><tr><th>Obs (UTC)</th><th>Local</th><th>°C</th><th>Type</th><th>Provider</th><th>Delay s</th><th>Flags</th><th>Raw</th></tr>");
    for o in l.observations.iter().take(12) {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"wrap muted\">{}</td></tr>",
            esc(&ms_to_utc(o.t_ms)),
            esc(&o.local),
            opt_f(o.temp_c, 1),
            esc(&o.report_type),
            esc(&o.provider),
            o.knowledge_delay_s,
            esc(&o.flags.join(" ")),
            esc(&o.raw),
        );
    }
    out.push_str("</table></section>");
}

/// Render the page.
pub fn render(s: &DashboardSnapshot) -> String {
    let mut out = String::with_capacity(32 * 1024);
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta http-equiv=\"refresh\" content=\"5\"><title>Weather Machine · lite</title><style>");
    out.push_str(STYLE);
    out.push_str("</style></head><body>");
    let _ = write!(
        out,
        "<header><h1>WEATHER MACHINE</h1><span class=\"pill\">{} · {}</span><span class=\"pill\">engine {}</span><span class=\"pill {}\">storage {}</span><span class=\"pill {}\">model {}</span><span class=\"pill\">run {}</span><span class=\"pill\">v{}</span><a href=\"/\">full dashboard</a></header>",
        esc(&s.mode),
        esc(&s.instance),
        esc(&ms_to_utc(s.engine_time_ms)),
        class_for(s.storage_ok),
        if s.storage_ok { "ok" } else { "DOWN" },
        class_for(s.model.loaded() && s.model_id != "no-edge"),
        esc(&if s.model.loaded() || s.model.state.is_empty() {
            s.model_id.clone()
        } else {
            s.model.state.clone()
        }),
        esc(&s.run_id),
        esc(&s.version),
    );
    if s.demo {
        out.push_str("<div class=\"banner\">DEMO — synthetic data in accelerated time. Nothing here is real market or weather data.</div>");
    }
    if !s.demo && !s.model.loaded() && !s.model.detail.is_empty() {
        let _ = write!(
            out,
            "<div class=\"banner\">No probability model yet, so no weather trades: {}</div>",
            esc(&s.model.detail)
        );
    }
    if let Some(v) = &s.model.forecast {
        let used = s
            .locations
            .iter()
            .any(|l| l.forecast.as_ref().is_some_and(|f| f.in_use));
        let _ = write!(
            out,
            "<div class=\"note\">Day-1 forecast {}: {}</div>",
            if used { "in use" } else { "not in use" },
            esc(v)
        );
    }
    if let Some(k) = &s.kill_switch {
        let _ = write!(
            out,
            "<div class=\"kill\">KILL SWITCH ENGAGED — {}</div>",
            esc(k)
        );
    }
    out.push_str("<main>");
    let r = &s.risk;
    let _ = write!(
        out,
        "<section><h2>Risk</h2><div class=\"kpis\"><div class=\"kpi\"><b>${:.2} / ${:.2}</b><span>worst-case exposure</span></div><div class=\"kpi\"><b>${:.2}</b><span>capital deployed</span></div><div class=\"kpi\"><b class=\"{}\">${:+.2}</b><span>realized today</span></div><div class=\"kpi\"><b class=\"{}\">${:+.2}</b><span>realized total</span></div><div class=\"kpi\"><b>${:.2}</b><span>per position</span></div></div><table><tr><th>Check</th><th>State</th><th>Detail</th></tr>",
        r.global_worst_case_usd,
        r.global_limit_usd,
        r.capital_deployed_usd,
        if r.daily_realized_pnl_usd >= 0.0 {
            "ok"
        } else {
            "bad"
        },
        r.daily_realized_pnl_usd,
        if r.realized_pnl_total_usd >= 0.0 {
            "ok"
        } else {
            "bad"
        },
        r.realized_pnl_total_usd,
        r.position_size_usd,
    );
    for c in &r.checks {
        let _ = write!(
            out,
            "<tr><td>{}</td><td class=\"{}\">{}</td><td class=\"wrap\">{}</td></tr>",
            esc(&c.name),
            class_for(c.ok),
            if c.ok { "PASS" } else { "BLOCK" },
            esc(&c.detail)
        );
    }
    out.push_str("</table></section>");
    for l in &s.locations {
        location(&mut out, l);
    }
    out.push_str("<section><h2>Providers</h2><table><tr><th>Provider</th><th>Scope</th><th>State</th><th>Circuit</th><th>Req today</th><th>Budget</th><th>429s</th><th>Latency ms</th><th>Last success</th><th>Reason</th></tr>");
    for p in &s.providers {
        let class = match p.state.as_str() {
            "healthy" => "ok",
            "degraded" | "stale" => "warn",
            "standby" => "muted",
            _ => "bad",
        };
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td class=\"{class}\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"wrap\">{}</td></tr>",
            esc(&p.provider),
            esc(p.scope.as_deref().unwrap_or("—")),
            esc(&p.state),
            esc(&p.circuit),
            p.requests_today,
            p.daily_budget
                .map_or_else(|| "—".to_owned(), |b| b.to_string()),
            p.throttle_events,
            opt_f(p.latency_ms_ewma, 0),
            p.last_success_ms.map_or_else(|| "—".to_owned(), ms_to_utc),
            esc(&p.reason),
        );
    }
    out.push_str("</table></section>");
    out.push_str("<section><h2>Positions</h2><table><tr><th>Event</th><th>Bucket</th><th>Side</th><th>Shares</th><th>Avg</th><th>Mark</th><th>Cost $</th><th>Unrealized $</th><th>Realized $</th></tr>");
    for p in &s.positions {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.2}</td><td>{:.3}</td><td>{}</td><td>{:.2}</td><td>{}</td><td>{:+.2}</td></tr>",
            esc(&p.event_slug),
            esc(&p.bucket),
            esc(&p.side),
            p.shares,
            p.avg_cost,
            opt_f(p.mark, 3),
            p.cost_usd,
            signed(p.unrealized_usd, 2),
            p.realized_usd,
        );
    }
    out.push_str("</table></section>");
    out.push_str("<section><h2>Orders</h2><table><tr><th>Order</th><th>Strategy</th><th>Bucket</th><th>Side</th><th>Limit</th><th>Shares</th><th>Filled</th><th>Avg</th><th>Status</th><th>Reason</th></tr>");
    for o in s.orders.iter().take(25) {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{} {}</td><td>{:.3}</td><td>{:.2}</td><td>{:.2}</td><td>{}</td><td>{}</td><td class=\"wrap\">{}</td></tr>",
            esc(&o.client_order_id),
            esc(&o.strategy),
            esc(&o.bucket),
            esc(&o.side),
            esc(&o.outcome),
            o.limit,
            o.shares,
            o.filled,
            opt_f(o.avg_price, 3),
            esc(&o.status),
            esc(o.reason.as_deref().unwrap_or("")),
        );
    }
    out.push_str("</table></section>");
    out.push_str("<section><h2>Decisions</h2><table><tr><th>#</th><th>Time (UTC)</th><th>Strategy</th><th>Result</th><th>Summary / reasons</th></tr>");
    for d in s
        .decisions
        .iter()
        .filter(|d| d.strategy != "evaluation")
        .take(25)
    {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"{}\">{}</td><td class=\"wrap\">{}{}</td></tr>",
            d.id,
            esc(&ms_to_utc(d.at_ms)),
            esc(&d.strategy),
            class_for(d.approved),
            if d.approved { "APPROVED" } else { "REJECTED" },
            esc(&d.summary),
            if d.reasons.is_empty() {
                String::new()
            } else {
                format!(
                    "<br><span class=\"muted\">{}</span>",
                    esc(&d.reasons.join(" · "))
                )
            },
        );
    }
    out.push_str("</table></section>");
    out.push_str(
        "<section><h2>Alerts</h2><table><tr><th>Time (UTC)</th><th>Level</th><th>Message</th></tr>",
    );
    for a in s.alerts.iter().rev().take(20) {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td class=\"wrap\">{}</td></tr>",
            esc(&ms_to_utc(a.at_ms)),
            esc(&a.level),
            esc(&a.message)
        );
    }
    out.push_str("</table></section></main>");
    let _ = write!(
        out,
        "<footer>Generated {} · refreshes every 5 s · <a href=\"/api/v1/snapshot\">JSON</a> · <a href=\"/metrics\">metrics</a></footer></body></html>",
        esc(&ms_to_utc(s.generated_at_ms))
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        assert_eq!(
            esc("<script>alert('x')&\"</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&amp;&quot;&lt;/script&gt;"
        );
    }

    #[test]
    fn renders_hostile_snapshot_safely() {
        let s = DashboardSnapshot {
            mode: "<b>paper</b>".into(),
            demo: true,
            kill_switch: Some("<img src=x onerror=1>".into()),
            ..Default::default()
        };
        let html = render(&s);
        assert!(html.contains("&lt;b&gt;paper&lt;/b&gt;"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("DEMO"));
        assert!(html.contains("KILL SWITCH ENGAGED"));
        assert!(!html.contains("<script"));
    }
}
