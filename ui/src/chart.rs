//! Intraday temperature chart and model distribution, rendered as SVG from Rust.

use crate::fmt::{self, minutes_of};
use leptos::prelude::*;
use wm_dashboard_api::{CONFIRMATION_WINDOWS, LocationDto, ViewDto};

const W: f64 = 960.0;
const H: f64 = 300.0;
const ML: f64 = 40.0;
const MR: f64 = 14.0;
const MT: f64 = 22.0;
const MB: f64 = 24.0;

struct Scale {
    y_min: f64,
    y_max: f64,
}

impl Scale {
    fn x(&self, minute: f64) -> f64 {
        ML + (minute.clamp(0.0, 1440.0) / 1440.0) * (W - ML - MR)
    }

    fn y(&self, temp: f64) -> f64 {
        let span = (self.y_max - self.y_min).max(1.0);
        MT + (1.0 - (temp - self.y_min) / span) * (H - MT - MB)
    }
}

fn f1(v: f64) -> String {
    format!("{v:.1}")
}

/// Intraday chart: observations, running high, high line, confirmation
/// windows, solar noon and "now".
pub fn temperature_chart(loc: &LocationDto) -> AnyView {
    let pts: Vec<(f64, f64, bool, bool, String)> = loc
        .series
        .iter()
        .filter_map(|p| {
            minutes_of(&p.local).map(|m| (m, p.temp_c, p.speci, p.eligible, p.local.clone()))
        })
        .collect();
    let now_min = minutes_of(&loc.local_time);
    let primary: Option<&ViewDto> = loc.views.first();
    let high = primary.and_then(|v| v.high_c);
    let (lo, hi) = pts
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    let (lo, hi) = if pts.is_empty() {
        (5.0, 25.0)
    } else {
        (lo.floor() - 1.0, hi.ceil() + 1.0)
    };
    let s = Scale {
        y_min: lo,
        y_max: hi,
    };
    let step = if hi - lo > 14.0 { 2.0 } else { 1.0 };

    // Grid.
    let mut grid = Vec::new();
    let mut t = lo;
    while t <= hi + 1e-9 {
        let y = s.y(t);
        grid.push(view! {
            <line class="grid" x1=f1(ML) x2=f1(W - MR) y1=f1(y) y2=f1(y)></line>
            <text class="axis" x=f1(ML - 6.0) y=f1(y + 3.5) text-anchor="end">{format!("{t:.0}°")}</text>
        }.into_any());
        t += step;
    }
    for h in (0..=24).step_by(3) {
        let x = s.x(f64::from(h) * 60.0);
        grid.push(view! {
            <line class="grid" x1=f1(x) x2=f1(x) y1=f1(MT) y2=f1(H - MB)></line>
            <text class="axis" x=f1(x) y=f1(H - 8.0) text-anchor="middle">{format!("{h:02}")}</text>
        }.into_any());
    }

    // Running high (step line) over the primary view's eligible points.
    let mut path = String::new();
    let mut run = f64::MIN;
    for (m, temp, _, eligible, _) in &pts {
        if !*eligible && primary.is_some_and(|v| v.label != "all") {
            continue;
        }
        let x = s.x(*m);
        if run == f64::MIN {
            run = *temp;
            path.push_str(&format!("M{:.1},{:.1}", x, s.y(run)));
        } else {
            path.push_str(&format!(" H{x:.1}"));
            if *temp > run {
                run = *temp;
                path.push_str(&format!(" V{:.1}", s.y(run)));
            }
        }
    }
    if let (Some(n), false) = (now_min, path.is_empty()) {
        path.push_str(&format!(" H{:.1}", s.x(n)));
    }

    // Solar noon (from the primary view's features).
    let solar_noon = match (now_min, primary.and_then(|v| v.minutes_after_solar_noon)) {
        (Some(n), Some(after)) => Some(n - f64::from(after)),
        _ => None,
    };
    // Confirmation windows after the last touch of the high.
    let high_at = primary
        .and_then(|v| v.high_local.as_deref())
        .and_then(minutes_of);
    let met: Vec<u32> = primary.map(|v| v.windows_met.clone()).unwrap_or_default();
    let windows = high_at
        .map(|h0| {
            CONFIRMATION_WINDOWS
                .iter()
                .map(|w| {
                    let x = s.x(h0 + f64::from(*w));
                    let ok = met.contains(w);
                    // Label every hour (30-minute labels collide at day scale);
                    // the other windows are ticks only.
                    let label = if w % 60 == 0 {
                        format!("{w}′")
                    } else {
                        String::new()
                    };
                    view! {
                        <g class={if ok { "win met" } else { "win" }}>
                            <line x1=f1(x) x2=f1(x) y1=f1(MT) y2=f1(MT + 8.0)></line>
                            <text x=f1(x) y=f1(MT - 4.0) text-anchor="middle">{label}</text>
                        </g>
                    }
                    .into_any()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let dots = pts
        .iter()
        .map(|(m, temp, speci, eligible, label)| {
            let class = match (speci, eligible) {
                (true, _) => "obs speci",
                (false, true) => "obs",
                (false, false) => "obs dim",
            };
            let tip = format!("{label} local · {temp:.1} °C{}{}", if *speci { " · SPECI" } else { "" }, if *eligible { "" } else { " · outside resolution window" });
            view! { <circle class=class cx=f1(s.x(*m)) cy=f1(s.y(*temp)) r="3.4"><title>{tip}</title></circle> }.into_any()
        })
        .collect::<Vec<_>>();

    let high_line = high.map(|h| {
        let y = s.y(h);
        view! {
            <line class="high" x1=f1(ML) x2=f1(W - MR) y1=f1(y) y2=f1(y)></line>
            <text class="high-label" x=f1(W - MR - 4.0) y=f1(y - 5.0) text-anchor="end">{format!("HIGH {h:.0}°")}</text>
        }
    });
    let noon = solar_noon.map(|n| {
        let x = s.x(n);
        view! {
            <line class="noon" x1=f1(x) x2=f1(x) y1=f1(MT) y2=f1(H - MB)></line>
            <text class="axis" x=f1(x + 4.0) y=f1(H - MB - 6.0)>"solar noon"</text>
        }
    });
    let now_line = now_min.map(|n| {
        let x = s.x(n);
        view! {
            <line class="now" x1=f1(x) x2=f1(x) y1=f1(MT) y2=f1(H - MB)></line>
            <text class="now-label" x=f1(x + 4.0) y=f1(MT + 36.0)>{format!("now {}", loc.local_time)}</text>
        }
    });
    let current = pts.last().map(|(m, temp, ..)| {
        view! { <text class="cur-label" x=f1(s.x(*m) + 7.0) y=f1(s.y(*temp) + 4.0)>{format!("{temp:.0}°")}</text> }
    });
    let empty = pts.is_empty().then(|| view! { <text class="empty" x=f1(W / 2.0) y=f1(H / 2.0) text-anchor="middle">"no observations for the local day yet"</text> });

    view! {
        <svg class="chart" viewBox=format!("0 0 {W} {H}") role="img" aria-label="Intraday temperature">
            {grid}
            {noon}
            {windows}
            <path class="runhigh" d=path></path>
            {high_line}
            {dots}
            {current}
            {now_line}
            {empty}
        </svg>
    }
    .into_any()
}

/// P(final = high + k) bars for one view.
pub fn distribution_bars(v: &ViewDto) -> AnyView {
    let Some(d) = v.distribution.as_ref().filter(|d| !d.is_empty()) else {
        return view! { <div class="muted small">"no model estimate (no edge ⇒ no trade)"</div> }
            .into_any();
    };
    let n = d.len();
    let rows = d
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let label = match (k, k + 1 == n) {
                (0, _) => "final = high".to_owned(),
                (k, true) => format!("≥ high+{k}"),
                (k, false) => format!("high+{k}"),
            };
            let width = format!("width:{:.1}%", (p * 100.0).clamp(0.0, 100.0));
            view! {
                <div class="dist-row">
                    <span class="dist-label">{label}</span>
                    <span class="dist-bar"><span class={if k == 0 { "fill final" } else { "fill" }} style=width></span></span>
                    <span class="dist-val">{fmt::pct(Some(*p))}</span>
                </div>
            }
            .into_any()
        })
        .collect::<Vec<_>>();
    view! {
        <div class="dist">
            {rows}
            <div class="muted small">{format!("support {} · {}", v.model_support.unwrap_or(0), v.model_source.clone().unwrap_or_default())}</div>
        </div>
    }
    .into_any()
}
