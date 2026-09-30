//! Dashboard application: state, live connection, and all panels.

use crate::chart::{distribution_bars, temperature_chart};
use crate::fmt::{self, DASH};
use crate::strategies::{ReportsPanel, StrategiesPanel, StrategyPage};
use leptos::prelude::*;
use std::sync::Arc;
use std::time::Duration;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wm_dashboard_api::*;

pub(crate) type Snap = RwSignal<Option<Arc<DashboardSnapshot>>>;

/// What the page shows: the dashboard, or one strategy (`#/strategy/<id>`).
#[derive(Debug, Clone, PartialEq)]
enum Route {
    Dashboard,
    Strategy(String),
}

fn route_from_hash(hash: &str) -> Route {
    match hash.strip_prefix("#/strategy/") {
        Some(id) if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            Route::Strategy(id.to_owned())
        }
        _ => Route::Dashboard,
    }
}

fn current_route() -> Route {
    let hash = web_sys::window()
        .and_then(|w| w.location().hash().ok())
        .unwrap_or_default();
    route_from_hash(&hash)
}

/// Live-connection state of the snapshot stream.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    Connecting,
    Live,
    Reconnecting,
}

pub(crate) fn js_err(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}

/// Subscribe to `/api/v1/stream` (the browser reconnects automatically).
fn connect(
    snap: Snap,
    link: RwSignal<Link>,
    last_msg: RwSignal<f64>,
    parse_err: RwSignal<Option<String>>,
) {
    let Ok(es) = web_sys::EventSource::new("api/v1/stream") else {
        link.set(Link::Reconnecting);
        return;
    };
    let on_snapshot =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let Some(text) = ev.data().as_string() else {
                return;
            };
            match serde_json::from_str::<DashboardSnapshot>(&text) {
                Ok(s) => {
                    snap.set(Some(Arc::new(s)));
                    link.set(Link::Live);
                    last_msg.set(js_sys::Date::now());
                    parse_err.set(None);
                }
                Err(e) => parse_err.set(Some(format!("snapshot decode error: {e}"))),
            }
        });
    let _ = es.add_event_listener_with_callback("snapshot", on_snapshot.as_ref().unchecked_ref());
    on_snapshot.forget();
    let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
        link.set(Link::Reconnecting)
    });
    es.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    on_error.forget();
    // Keep the EventSource alive for the page's lifetime.
    std::mem::forget(es);
}

async fn post_kill_switch(token: String, engaged: bool, reason: String) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    let headers = web_sys::Headers::new().map_err(js_err)?;
    headers
        .set("content-type", "application/json")
        .map_err(js_err)?;
    headers.set("x-wm-admin-token", &token).map_err(js_err)?;
    init.set_headers(&headers);
    let body =
        serde_json::to_string(&KillSwitchRequest { engaged, reason }).map_err(|e| e.to_string())?;
    init.set_body(&JsValue::from_str(&body));
    let req =
        web_sys::Request::new_with_str_and_init("api/v1/kill-switch", &init).map_err(js_err)?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(js_err)?
        .dyn_into()
        .map_err(js_err)?;
    if resp.ok() {
        return Ok(());
    }
    let status = resp.status();
    let text = match resp.text() {
        Ok(p) => JsFuture::from(p)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    Err(format!("HTTP {status}: {text}"))
}

fn state_class(state: &str) -> &'static str {
    match state {
        "healthy" => "good",
        "degraded" | "stale" => "warn",
        // Not contacted yet (fallback source): neutral, not an alarm.
        "standby" => "",
        _ => "bad",
    }
}

fn pnl_class(v: f64) -> &'static str {
    if v > 0.0 {
        "good"
    } else if v < 0.0 {
        "bad"
    } else {
        ""
    }
}

#[component]
pub fn App() -> impl IntoView {
    let snap: Snap = RwSignal::new(None);
    let link = RwSignal::new(Link::Connecting);
    let last_msg = RwSignal::new(0.0_f64);
    let now = RwSignal::new(js_sys::Date::now());
    let parse_err = RwSignal::new(None::<String>);
    connect(snap, link, last_msg, parse_err);
    set_interval(
        move || now.set(js_sys::Date::now()),
        Duration::from_millis(1000),
    );
    let show_kill = RwSignal::new(false);
    let route = RwSignal::new(current_route());
    if let Some(w) = web_sys::window() {
        let on_hash = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
            route.set(current_route());
            if let Some(w) = web_sys::window() {
                w.scroll_to_with_x_and_y(0.0, 0.0);
            }
        });
        let _ = w.add_event_listener_with_callback("hashchange", on_hash.as_ref().unchecked_ref());
        on_hash.forget();
    }

    let loaded = move || snap.with(Option::is_some);
    view! {
        <div class="app">
            <StatusBar snap=snap link=link last_msg=last_msg now=now show_kill=show_kill />
            {move || parse_err.get().map(|e| view! { <div class="banner bad">{e}</div> })}
            <Show when=loaded fallback=|| view! { <div class="loading">"Connecting to the Weather Machine engine…"</div> }>
                <Banners snap=snap />
                {move || match route.get() {
                    Route::Dashboard => view! { <Dashboard snap=snap /> }.into_any(),
                    Route::Strategy(id) => view! { <StrategyPage snap=snap id=id /> }.into_any(),
                }}
                <footer class="muted small">
                    "Weather Machine · Rust end to end (engine, API and this WebAssembly UI) · "
                    <a href="lite">"lite view"</a>" · "<a href="api/v1/snapshot">"snapshot JSON"</a>" · "<a href="metrics">"metrics"</a>
                </footer>
            </Show>
            <KillSwitchModal snap=snap open=show_kill />
        </div>
    }
}

#[component]
fn Dashboard(snap: Snap) -> impl IntoView {
    view! {
        <KpiStrip snap=snap />
        <StrategiesPanel snap=snap />
        <Locations snap=snap />
        <div class="grid-2">
            <RiskPanel snap=snap />
            <ProvidersPanel snap=snap />
        </div>
        <Blotter snap=snap />
        <div class="grid-2">
            <DecisionLog snap=snap />
            <AlertsPanel snap=snap />
        </div>
        <div class="grid-2">
            <EnginePanel snap=snap />
            <BreakEvenPanel snap=snap />
        </div>
        <ReportsPanel snap=snap />
    }
}

#[component]
fn StatusBar(
    snap: Snap,
    link: RwSignal<Link>,
    last_msg: RwSignal<f64>,
    now: RwSignal<f64>,
    show_kill: RwSignal<bool>,
) -> impl IntoView {
    let field = move |f: fn(&DashboardSnapshot) -> String| {
        move || snap.with(|s| s.as_ref().map(|s| f(s)).unwrap_or_default())
    };
    let link_view = move || {
        let age = if last_msg.get() > 0.0 {
            ((now.get() - last_msg.get()) / 1000.0).max(0.0)
        } else {
            f64::INFINITY
        };
        let (class, text) = match link.get() {
            Link::Connecting => ("pill warn", "CONNECTING".to_owned()),
            Link::Reconnecting => ("pill bad", "RECONNECTING".to_owned()),
            Link::Live if age > 5.0 => ("pill warn", format!("STREAM STALE {age:.0}s")),
            Link::Live => ("pill good", "STREAM".to_owned()),
        };
        view! { <span class=class title="Dashboard connection to the server (live snapshot stream). Trading mode is the PAPER badge.">{text}</span> }
    };
    let flags = move || {
        snap.with(|s| {
            s.as_ref().map(|s| {
                let pill = |ok: bool, label: String, title: String| view! { <span class={if ok { "pill good" } else { "pill bad" }} title=title>{label}</span> };
                let model = {
                    let m = &s.model;
                    let (class, label) = match m.state.as_str() {
                        "loaded" if m.retraining.is_some() => ("pill good", "MODEL ↻".to_owned()),
                        "loaded" => ("pill good", "MODEL".to_owned()),
                        "training" => ("pill warn", m.progress.map_or_else(|| "MODEL TRAINING".to_owned(), |(d, t)| format!("MODEL TRAINING {d}/{t}"))),
                        "failed" | "invalid" => ("pill bad", "MODEL ERROR".to_owned()),
                        _ => ("pill warn", "NO MODEL".to_owned()),
                    };
                    let mut title = if m.detail.is_empty() { s.model_id.clone() } else { m.detail.clone() };
                    if let Some(v) = &m.structure {
                        title.push_str(&format!(" — structure: {v}"));
                    }
                    if let Some(r) = &m.retraining {
                        title.push_str(&format!(" — {r} (the current model keeps trading)"));
                    }
                    view! { <span class=class title=title>{label}</span> }
                };
                // Day-1 forecast: used only when the evaluation adopted it.
                let in_use = s.locations.iter().any(|l| l.forecast.as_ref().is_some_and(|f| f.in_use));
                let forecast = s.model.forecast.clone().map(|verdict| {
                    let adopted = verdict.starts_with("adopted");
                    let (class, label) = match (in_use, adopted) {
                        (true, _) => ("pill good", "FORECAST"),
                        (false, true) => ("pill warn", "FORECAST WAITING"),
                        (false, false) => ("pill", "forecast not used"),
                    };
                    view! { <span class=class title=format!("Day-1 forecast evaluation: {verdict}")>{label}</span> }.into_any()
                });
                let mut v = vec![
                    pill(s.storage_ok, "STORAGE".into(), "Audit storage (PostgreSQL); required for trading".into()).into_any(),
                    pill(s.execution_ok, "EXECUTION".into(), "Simulated paper venue".into()).into_any(),
                    model.into_any(),
                ];
                v.extend(forecast);
                v.push(pill(s.market_stream.as_ref().is_none_or(|m| m.connected), "MARKET DATA".into(), "Polymarket market stream".into()).into_any());
                v
            })
        })
    };
    let kill = move || snap.with(|s| s.as_ref().and_then(|s| s.kill_switch.clone()));
    view! {
        <header class="statusbar">
            <div class="brand">"WEATHER MACHINE"</div>
            <span class="pill mode">{field(|s| if s.demo { format!("{} · DEMO", s.mode.to_uppercase()) } else { s.mode.to_uppercase() })}</span>
            {link_view}
            {flags}
            <span class="sb-item" title="Engine knowledge time (UTC)">"engine "<b>{field(|s| fmt::utc_datetime(s.engine_time_ms))}</b></span>
            <span class="sb-item" title="Last / max event handling time in the kernel">"kernel "<b>{field(|s| fmt::micros(s.engine.last_handle_micros))}</b>" / "{field(|s| fmt::micros(s.engine.max_handle_micros))}</span>
            <span class="sb-item muted">{field(|s| format!("{} · run {} · v{}", s.instance, s.run_id.chars().take(8).collect::<String>(), s.version))}</span>
            <span class="spacer"></span>
            {move || match kill() {
                Some(reason) => view! { <button class="btn kill engaged" on:click=move |_| show_kill.set(true)>{format!("KILL SWITCH ENGAGED — {reason}")}</button> }.into_any(),
                None => view! { <button class="btn kill" on:click=move |_| show_kill.set(true)>"KILL SWITCH"</button> }.into_any(),
            }}
        </header>
    }
}

#[component]
fn Banners(snap: Snap) -> impl IntoView {
    move || {
        snap.with(|s| {
            let s = s.as_ref()?;
            let mut v = Vec::new();
            if s.demo {
                v.push(view! { <div class="banner demo">"DEMO — synthetic weather and market data in accelerated time. Nothing on this screen is real or evidence of edge."</div> }.into_any());
            }
            if let Some(k) = &s.kill_switch {
                v.push(view! { <div class="banner bad">{format!("KILL SWITCH ENGAGED: {k} — no new orders")}</div> }.into_any());
            }
            if !s.live_trading_enabled {
                v.push(view! { <div class="banner info">"Paper trading — live order placement is disabled in this build."</div> }.into_any());
            }
            if !s.demo && !s.model.loaded() {
                let (class, text) = match s.model.state.as_str() {
                    "training" => ("banner info", format!("No probability model yet, so no weather trades (by design). Training automatically from real METAR history — {}. It is loaded automatically when ready.", s.model.detail)),
                    "failed" | "invalid" => ("banner bad", fmt::sentence(&s.model.detail)),
                    _ => ("banner info", format!("No probability model, so no weather trades (by design): {}", s.model.detail)),
                };
                v.push(view! { <div class=class>{text}</div> }.into_any());
            }
            Some(v)
        })
    }
}

#[component]
fn KpiStrip(snap: Snap) -> impl IntoView {
    let risk = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref().map(|s| {
                (
                    s.risk.clone(),
                    s.positions.iter().filter(|p| p.shares > 0.0).count(),
                    s.engine.approvals_total,
                    s.engine.rejections_total,
                    s.engine.fills_total,
                )
            })
        })
    });
    move || {
        risk.get().map(|(r, positions, approvals, rejections, fills)| {
            let util = if r.global_limit_usd > 0.0 { (r.global_worst_case_usd / r.global_limit_usd).clamp(0.0, 1.0) } else { 0.0 };
            let bar = format!("width:{:.1}%", util * 100.0);
            let bar_class = if util > 0.8 { "fill bad" } else if util > 0.5 { "fill warn" } else { "fill" };
            let daily_new_limit = r.daily_new_limit_usd.map_or_else(|| "no limit".to_owned(), |l| format!("limit {}", fmt::usd(l)));
            let loss = r.daily_loss_limit_usd.map_or_else(|| DASH.to_owned(), fmt::usd);
            view! {
                <section class="kpis">
                    <div class="kpi wide">
                        <label>"Worst-case exposure"</label>
                        <b>{format!("{} / {}", fmt::usd(r.global_worst_case_usd), fmt::usd(r.global_limit_usd))}</b>
                        <div class="meter"><span class=bar_class style=bar></span></div>
                    </div>
                    <div class="kpi"><label>"Capital deployed"</label><b>{fmt::usd(r.capital_deployed_usd)}</b></div>
                    <div class="kpi"><label>"Realized today"</label><b class=pnl_class(r.daily_realized_pnl_usd)>{fmt::usd_signed(r.daily_realized_pnl_usd)}</b><small>{format!("loss limit {loss}")}</small></div>
                    <div class="kpi"><label>"Realized total"</label><b class=pnl_class(r.realized_pnl_total_usd)>{fmt::usd_signed(r.realized_pnl_total_usd)}</b></div>
                    <div class="kpi"><label>"New exposure today"</label><b>{fmt::usd(r.daily_new_exposure_usd)}</b><small>{daily_new_limit}</small></div>
                    <div class="kpi"><label>"Per position"</label><b>{fmt::usd(r.position_size_usd)}</b><small>{format!("max price {:.2} · spread ≤ {:.2}", r.max_price, r.max_spread)}</small></div>
                    <div class="kpi"><label>"Open positions"</label><b>{positions}</b></div>
                    <div class="kpi"><label>"Orders approved / filled"</label><b>{format!("{approvals} / {fills}")}</b><small>{format!("{rejections} rejected by risk")}</small></div>
                </section>
            }
        })
    }
}

#[component]
fn Locations(snap: Snap) -> impl IntoView {
    let ids = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.locations
                        .iter()
                        .map(|l| l.location.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    view! {
        <For each=move || ids.get() key=|id| id.clone() children=move |id| view! { <LocationSection snap=snap id=id /> } />
    }
}

#[component]
fn LocationSection(snap: Snap, id: String) -> impl IntoView {
    let key = id.clone();
    let loc = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref()
                .and_then(|s| s.locations.iter().find(|l| l.location == key).cloned())
        })
    });
    let chart = move || loc.with(|l| l.as_ref().map(temperature_chart));
    let header = move || {
        loc.with(|l| {
            l.as_ref().map(|l| {
                let age = l.last_observation_age_s.map_or_else(|| "no data".to_owned(), |a| format!("{} ago", fmt::age(a)));
                view! {
                    <div class="loc-head">
                        <h2>{format!("{} · {}", l.location.to_uppercase(), l.station)}</h2>
                        <span class="muted">{format!("{} · local {} {}", l.timezone, l.local_date, l.local_time)}</span>
                        <span class="big">{l.current_temp_c.map_or_else(|| DASH.to_owned(), |t| format!("{t:.0} °C"))}</span>
                        <span class="muted">{format!("last report {age}")}</span>
                        <span class={if l.peak_watch { "pill warn" } else { "pill" }}>{if l.peak_watch { "PEAK WATCH" } else { "peak watch off" }}</span>
                        <span class={if l.has_exposure { "pill info" } else { "pill" }} title="open paper position in this location's market">{if l.has_exposure { "POSITION OPEN" } else { "no position" }}</span>
                    </div>
                    {l.last_raw.clone().map(|r| view! { <div class="raw" title="latest raw report">{r}</div> })}
                }
            })
        })
    };
    view! {
        <section class="panel location">
            {header}
            <div class="loc-grid">
                <div class="chart-box">
                    {chart}
                    <div class="legend small muted">
                        <span><i class="dot"></i>"counted by resolution filter"</span>
                        <span><i class="dot hollow"></i>"outside filter window"</span>
                        <span><i class="dot speci"></i>"SPECI"</span>
                        <span><i class="ln run"></i>"running high"</span>
                        <span><i class="ln high"></i>"day high"</span>
                        <span><i class="ln noon"></i>"solar noon"</span>
                        <span><i class="ln fc"></i>"day-1 forecast"</span>
                        <span>"30′…180′ confirmation windows after the last touch of the high"</span>
                    </div>
                </div>
                <PeakPanel loc=loc />
            </div>
            <Ladder loc=loc />
            <div class="grid-3">
                <ResolutionPanel loc=loc />
                <CollectorPanel loc=loc />
                <ObservationTape loc=loc />
            </div>
        </section>
    }
}

#[component]
fn PeakPanel(loc: Memo<Option<LocationDto>>) -> impl IntoView {
    move || {
        loc.with(|l| {
            let l = l.as_ref()?;
            let views = l
                .views
                .iter()
                .map(|v| {
                    let windows = CONFIRMATION_WINDOWS
                        .iter()
                        .map(|w| view! { <span class={if v.windows_met.contains(w) { "chip on" } else { "chip" }}>{format!("{w}′")}</span> })
                        .collect::<Vec<_>>();
                    view! {
                        <div class="view">
                            <div class="view-head">
                                <b>{format!("view: {}", v.label)}</b>
                                <span class="muted">{format!("{} obs", v.observations)}</span>
                            </div>
                            <div class="stats">
                                <div><label>"High"</label><b>{v.high_whole.map_or_else(|| DASH.to_owned(), |h| format!("{h} °C"))}</b><small>{v.high_c.map_or_else(String::new, |h| format!("{h:.1} raw"))}</small></div>
                                <div><label>"At"</label><b>{v.high_local.clone().unwrap_or_else(|| DASH.to_owned())}</b><small>{format!("{} retest(s)", v.retests)}</small></div>
                                <div><label>"Since high"</label><b>{v.minutes_since_high.map_or_else(|| DASH.to_owned(), |m| format!("{m} min"))}</b><small>{format!("{} lower since", v.lower_since_high)}</small></div>
                                <div><label>"Drop"</label><b>{fmt::opt(v.drop_c, 1)}</b><small>"°C below high"</small></div>
                                <div><label>"Slope"</label><b>{fmt::signed(v.slope_c_per_h, 2)}</b><small>{format!("°C/h · accel {}", fmt::signed(v.accel, 2))}</small></div>
                                <div><label>"Trajectory"</label><b>{v.trajectory.clone().unwrap_or_else(|| DASH.to_owned())}</b><small>{v.minutes_after_solar_noon.map_or_else(String::new, |m| format!("{m:+} min vs solar noon"))}</small></div>
                            </div>
                            <div class="windows">"confirmation "{windows}</div>
                            {distribution_bars(v)}
                        </div>
                    }
                })
                .collect::<Vec<_>>();
            let forecast = l.forecast.as_ref().map(|f| {
                let c = |v: Option<f64>| v.map_or_else(|| DASH.to_owned(), |t| format!("{t:.1} °C"));
                view! {
                    <div class="fc-box" title="Each hourly value was forecast 24 h before its time — the same product the model was evaluated on. Predictive input only: never the observed high or settlement.">
                        <div class="view-head">
                            <b>"Day-1 forecast"</b>
                            <span class={if f.in_use { "pill good" } else { "pill" }}>{if f.in_use { "USED BY MODEL" } else { "not used" }}</span>
                        </div>
                        <div class="muted small">{format!("{} · received {}", f.product, fmt::utc_time(f.received_ms))}</div>
                        <div class="stats">
                            <div><label>"Day max"</label><b>{c(f.day_max_c)}</b><small>"forecast"</small></div>
                            <div><label>"Rest of day"</label><b>{c(f.remaining_max_c)}</b><small>"max from now"</small></div>
                            <div><label>"Rise"</label><b>{fmt::signed(f.rise_c, 1)}</b><small>"°C later vs so far"</small></div>
                            <div><label>"Headroom"</label><b>{fmt::signed(f.headroom_c, 1)}</b><small>"°C above the high"</small></div>
                        </div>
                        <div class="muted fc-status">{fmt::sentence(&f.status)}</div>
                    </div>
                }
            });
            Some(view! { <div class="peak">{views}{forecast}</div> })
        })
    }
}

#[component]
fn Ladder(loc: Memo<Option<LocationDto>>) -> impl IntoView {
    let market = Memo::new(move |_| loc.with(|l| l.as_ref().and_then(|l| l.market.clone())));
    move || {
        match market.get() {
        None => view! { <div class="panel-inner warn-box">"No market discovered for today — nothing to trade."</div> }.into_any(),
        Some(m) => {
            let rows = m
                .rows
                .iter()
                .map(|r| {
                    let edge_class = match r.edge {
                        Some(e) if e >= 0.02 => "num good",
                        Some(e) if e <= -0.02 => "num bad",
                        _ => "num muted",
                    };
                    let ev_class = |v: Option<f64>| match v {
                        Some(x) if x > 0.0 => "num good",
                        Some(x) if x < 0.0 => "num bad",
                        _ => "num muted",
                    };
                    let model_w = format!("width:{:.1}%", r.model_p.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0);
                    let implied_w = format!("width:{:.1}%", r.implied_p.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0);
                    let chips = r.signals.iter().map(|s| view! { <span class="chip sig">{s.clone()}</span> }).collect::<Vec<_>>();
                    let blocker = if r.signals.is_empty() { r.blockers.first().cloned().unwrap_or_default() } else { String::new() };
                    let row_class = if r.contains_high { "hl" } else { "" };
                    view! {
                        <tr class=row_class>
                            <td class="bucket">{r.label.clone()}</td>
                            <td class="num bid">{fmt::price(r.yes_bid)}</td>
                            <td class="num ask">{fmt::price(r.yes_ask)}</td>
                            <td class="num bid">{fmt::price(r.no_bid)}</td>
                            <td class="num ask">{fmt::price(r.no_ask)}</td>
                            <td class="num">{fmt::opt(r.yes_spread, 3)}</td>
                            <td class="prob">
                                <div class="pbar"><span class="implied" style=implied_w></span><span class="model" style=model_w></span></div>
                                {match (r.implied_p, r.model_p) {
                                    (i, Some(m)) => view! { <span class="num">{fmt::pct(i)}</span>" → "<span class="num strong">{fmt::pct(Some(m))}</span> }.into_any(),
                                    (Some(i), None) => view! { <span class="num">{fmt::pct(Some(i))}</span> }.into_any(),
                                    (None, None) => view! { <span class="num muted">{DASH}</span> }.into_any(),
                                }}
                                {r.model_p.zip(r.used_p).filter(|(m, u)| m - u >= 0.0005).map(|(_, u)| view! { <div class="muted small" title="The model's P(YES) pooled with the market midpoint; EVs and strategy A use it">{format!("used {}", fmt::pct(Some(u)))}</div> })}
                            </td>
                            <td class=edge_class>{fmt::pp(r.edge)}</td>
                            <td class=ev_class(r.yes_ev)>{fmt::signed(r.yes_ev, 4)}</td>
                            <td class=ev_class(r.no_ev)>{fmt::signed(r.no_ev, 4)}</td>
                            <td class="num" title="break-even probability for buying YES at the ask, after fee and slippage allowance">{fmt::break_even(r.yes_break_even)}</td>
                            <td class="num">{if r.position_shares.abs() > 0.0 { format!("{:+.1}", r.position_shares) } else { DASH.to_owned() }}</td>
                            <td class="num muted">{r.book_age_ms.map_or_else(|| DASH.to_owned(), |a| fmt::age(a / 1000))}</td>
                            <td class="why">{chips}<span class="muted">{blocker}</span></td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            view! {
                <div class="panel-inner">
                    <div class="panel-title">
                        <span>"MARKET LADDER"</span>
                        <span class="muted">{format!("{} · fee {:.1}% · {}", m.event_slug, m.taker_fee_rate * 100.0, if m.neg_risk { "neg-risk" } else { "binary" })}</span>
                    </div>
                    <div class="table-wrap">
                        <table class="ladder">
                            <thead><tr>
                                <th>"Bucket"</th><th>"YES bid"</th><th>"YES ask"</th><th>"NO bid"</th><th>"NO ask"</th><th>"Spread"</th>
                                <th>"Implied → model"</th><th>"Edge pp"</th><th>"EV YES"</th><th>"EV NO"</th><th>"B/E"</th><th>"Pos"</th><th>"Book age"</th><th>"Signal / first blocker"</th>
                            </tr></thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                    <div class="muted small">"EV per share after fee and slippage allowance, at the model's probability pooled with the market (\"used\" — the market can lower it, never raise it). B/E: probability needed to profit buying YES at the ask (n/a = impossible at that price). Highlighted row contains the current high. Bars: grey = market-implied, blue = model."</div>
                </div>
            }
            .into_any()
        }
    }
    }
}

#[component]
fn ResolutionPanel(loc: Memo<Option<LocationDto>>) -> impl IntoView {
    move || {
        loc.with(|l| {
            let m = l.as_ref()?.market.as_ref()?;
            let url = m.resolution_url.clone().unwrap_or_default();
            let clauses = if m.unrecognized_clauses.is_empty() {
                view! { <div class="good small">"no unrecognized clauses"</div> }.into_any()
            } else {
                let items = m.unrecognized_clauses.iter().map(|c| view! { <li>{c.clone()}</li> }).collect::<Vec<_>>();
                view! { <ul class="bad small">{items}</ul> }.into_any()
            };
            Some(view! {
                <div class="panel-inner">
                    <div class="panel-title"><span>"RESOLUTION"</span><span class={if m.machine_tradable { "pill good" } else { "pill bad" }}>{if m.machine_tradable { "MACHINE-TRADABLE" } else { "REVIEW REQUIRED" }}</span></div>
                    <dl class="kv">
                        <dt>"Source"</dt><dd>{m.resolution_source.clone()}</dd>
                        <dt>"URL"</dt><dd class="wrap">{url}</dd>
                        <dt>"Filters"</dt><dd>{format!("{} ({})", m.filters.join(", "), if m.filter_confirmed { "confirmed" } else { "unconfirmed: all views must agree" })}</dd>
                        <dt>"Review"</dt><dd>{m.review_status.clone()}</dd>
                        <dt>"Rules hash"</dt><dd class="mono small">{m.rules_sha256.chars().take(16).collect::<String>()}"…"</dd>
                    </dl>
                    {clauses}
                    <p class="rules small muted">{m.rules_excerpt.clone()}</p>
                </div>
            })
        })
    }
}

#[component]
fn CollectorPanel(loc: Memo<Option<LocationDto>>) -> impl IntoView {
    move || {
        loc.with(|l| {
            let l = l.as_ref()?;
            let Some(c) = l.collector.as_ref() else {
                return Some(view! { <div class="panel-inner"><div class="panel-title"><span>"COLLECTOR"</span></div><div class="muted">"No live collector (demo or lease held elsewhere)."</div></div> }.into_any());
            };
            let next = c.next_poll.as_ref().map_or_else(
                || DASH.to_owned(),
                |p| format!("{} ({}, {}){}", fmt::utc_time(p.at_ms), p.reason, p.mode, p.expected_report_ms.map_or_else(String::new, |e| format!(" · expecting {}", fmt::utc_time(e)))),
            );
            Some(view! {
                <div class="panel-inner">
                    <div class="panel-title"><span>"COLLECTOR"</span><span class={if c.storage_ok { "pill good" } else { "pill bad" }}>{if c.storage_ok { "persisting" } else { "STORAGE FAILING" }}</span></div>
                    <dl class="kv">
                        <dt>"Active source"</dt><dd>{c.active_provider.clone().unwrap_or_else(|| DASH.to_owned())}</dd>
                        <dt>"Next poll"</dt><dd>{next}</dd>
                        <dt>"Polls"</dt><dd>{format!("{} ({} gate-deferred)", c.polls_total, c.gate_closed_total)}</dd>
                        <dt>"New / dup"</dt><dd>{format!("{} new · {} duplicate", c.new_observations_total, c.duplicates_total)}</dd>
                        <dt>"Corrections"</dt><dd>{format!("{} corrected · {} late", c.corrections_total, c.out_of_order_total)}</dd>
                        <dt>"Persist failures"</dt><dd class={if c.persist_failures_total > 0 { "bad" } else { "" }}>{c.persist_failures_total}</dd>
                    </dl>
                </div>
            }.into_any())
        })
    }
}

#[component]
fn ObservationTape(loc: Memo<Option<LocationDto>>) -> impl IntoView {
    move || {
        loc.with(|l| {
            let l = l.as_ref()?;
            let rows = l
                .observations
                .iter()
                .take(12)
                .map(|o| {
                    view! {
                        <tr>
                            <td class="mono">{o.local.clone()}</td>
                            <td class="num">{fmt::opt(o.temp_c, 1)}</td>
                            <td>{o.report_type.clone()}</td>
                            <td class="muted">{o.provider.clone()}</td>
                            <td class="num muted">{format!("{}s", o.knowledge_delay_s)}</td>
                            <td class="small">{o.flags.join(" ")}</td>
                            <td class="mono small muted rawcell" title=o.raw.clone()>{o.raw.clone()}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            Some(view! {
                <div class="panel-inner">
                    <div class="panel-title"><span>"OBSERVATION TAPE"</span><span class="muted">"knowledge delay = known − observed"</span></div>
                    <div class="table-wrap"><table class="tape"><thead><tr><th>"Local"</th><th>"°C"</th><th>"Type"</th><th>"Source"</th><th>"Delay"</th><th>"Flags"</th><th>"Raw"</th></tr></thead><tbody>{rows}</tbody></table></div>
                </div>
            })
        })
    }
}

#[component]
fn RiskPanel(snap: Snap) -> impl IntoView {
    let risk = Memo::new(move |_| snap.with(|s| s.as_ref().map(|s| s.risk.clone())));
    move || {
        risk.get().map(|r| {
            let checks = r
                .checks
                .iter()
                .map(|c| {
                    view! {
                        <tr>
                            <td>{c.name.clone()}</td>
                            <td class={if c.ok { "good strong" } else { "bad strong" }}>{if c.ok { "PASS" } else { "BLOCK" }}</td>
                            <td class="muted">{c.detail.clone()}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            let events = if r.per_event.is_empty() {
                view! { <tr><td colspan="2" class="empty">"No open exposure."</td></tr> }.into_any()
            } else {
                r.per_event.iter().map(|(e, u)| view! { <tr><td class="small">{e.clone()}</td><td class="num">{fmt::usd(*u)}</td></tr> }).collect::<Vec<_>>().into_any()
            };
            let blocked = r.checks.iter().filter(|c| !c.ok).count();
            view! {
                <section class="panel">
                    <div class="panel-title">
                        <span>"PRE-TRADE GATES"</span>
                        <span class={if blocked == 0 { "pill good" } else { "pill bad" }}>{if blocked == 0 { "ALL CLEAR".to_owned() } else { format!("{blocked} BLOCKING") }}</span>
                    </div>
                    <table class="checks"><tbody>{checks}</tbody></table>
                    <div class="panel-title sub"><span>"WORST-CASE LOSS BY EVENT"</span></div>
                    <table><tbody>{events}</tbody></table>
                    <div class="muted small">{format!("fail closed: weather age ≤ {} min, no gaps in the day series, healthy source, fresh books, storage up", r.max_weather_age_min)}</div>
                </section>
            }
        })
    }
}

#[component]
fn ProvidersPanel(snap: Snap) -> impl IntoView {
    let providers = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref()
                .map(|s| (s.providers.clone(), s.market_stream.clone()))
        })
    });
    move || {
        providers.get().map(|(ps, stream)| {
            let rows = ps
                .iter()
                .map(|p| {
                    let budget = p.daily_budget.map(|b| (f64::from(p.requests_today) / f64::from(b.max(1))).clamp(0.0, 1.0));
                    let bar = budget.map(|u| format!("width:{:.1}%", u * 100.0));
                    let blocked = p.blocked_until_ms.map(fmt::utc_time).unwrap_or_default();
                    // The detail gets its own full-width line: squeezed into a
                    // seventh column it wrapped mid-word.
                    let detail = p.last_error.clone().unwrap_or_else(|| p.reason.clone());
                    view! {
                        <tr class="prov">
                            <td><b>{p.provider.clone()}</b><div class="muted small">{p.scope.clone().unwrap_or_else(|| "global".into())}</div></td>
                            <td><span class=format!("pill {}", state_class(&p.state))>{p.state.to_uppercase()}</span><div class="muted small">{format!("circuit {}", p.circuit)}</div></td>
                            <td class="num">
                                {format!("{}{}", p.requests_today, p.daily_budget.map_or_else(String::new, |b| format!(" / {b}")))}
                                {bar.map(|b| view! { <div class="meter thin"><span class="fill" style=b></span></div> })}
                            </td>
                            <td class="num">{p.throttle_events}</td>
                            <td class="num">{fmt::opt(p.latency_ms_ewma, 0)}</td>
                            <td class="num">{if p.backoff_s > 0.0 { format!("{:.0}s", p.backoff_s) } else { DASH.to_owned() }}<div class="muted small">{blocked}</div></td>
                        </tr>
                        <tr class="prov-detail"><td colspan="6" class="small muted wrap">{detail}</td></tr>
                    }
                })
                .collect::<Vec<_>>();
            let stream_line = stream.map(|s| {
                view! {
                    <div class={if s.connected { "small good" } else { "small bad" }}>
                        {format!("market stream: {} · {} assets · {} msgs · {} reconnects{}", if s.connected { "connected" } else { "DISCONNECTED (REST fallback)" }, s.subscribed_assets, s.messages_total, s.reconnects_total, s.last_error.map_or_else(String::new, |e| format!(" · last error: {e}")))}
                    </div>
                }
            });
            view! {
                <section class="panel">
                    <div class="panel-title"><span>"DATA PROVIDERS · RATE LIMITS"</span><span class="muted">"one gate per provider · 429 ⇒ back off, never retry-spin"</span></div>
                    <div class="table-wrap">
                        <table class="providers">
                            <thead><tr><th>"Provider"</th><th>"State"</th><th>"Req today"</th><th>"429s"</th><th>"ms"</th><th>"Backoff"</th></tr></thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                    {stream_line}
                </section>
            }
        })
    }
}

#[component]
fn Blotter(snap: Snap) -> impl IntoView {
    let data = Memo::new(move |_| {
        snap.with(|s| s.as_ref().map(|s| (s.positions.clone(), s.orders.clone())))
    });
    move || {
        data.get().map(|(positions, orders)| {
            let mut positions = positions;
            positions.sort_by(|a, b| (b.shares > 0.0).cmp(&(a.shares > 0.0)).then(b.event_slug.cmp(&a.event_slug)));
            let prow = positions
                .iter()
                .map(|p| {
                    let open = p.shares > 0.0;
                    view! {
                        <tr class={if open { "" } else { "closed" }}>
                            <td class="small">{p.event_slug.clone()}{(!open).then(|| view! { " " <span class="pill">"CLOSED"</span> })}</td>
                            <td>{p.bucket.clone()}</td>
                            <td class={if p.side == "YES" { "good strong" } else { "bad strong" }}>{p.side.clone()}</td>
                            <td class="num">{format!("{:.2}", p.shares)}</td>
                            <td class="num">{format!("{:.3}", p.avg_cost)}</td>
                            <td class="num">{fmt::price(p.mark)}</td>
                            <td class="num">{fmt::usd(p.cost_usd)}</td>
                            <td class=format!("num {}", pnl_class(p.unrealized_usd.unwrap_or(0.0)))>{p.unrealized_usd.map_or_else(|| DASH.to_owned(), fmt::usd_signed)}</td>
                            <td class=format!("num {}", pnl_class(p.realized_usd))>{fmt::usd_signed(p.realized_usd)}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            let orow = orders
                .iter()
                .take(20)
                .map(|o| {
                    let status_class = match o.status.as_str() {
                        "filled" => "good",
                        "rejected" | "cancelled" | "expired" => "bad",
                        _ => "warn",
                    };
                    view! {
                        <tr>
                            <td class="mono small">{o.client_order_id.clone()}</td>
                            <td class="small">{o.strategy.clone()}</td>
                            <td>{format!("{} {} {}", o.side, o.outcome, o.bucket)}</td>
                            <td class="num">{format!("{:.3}", o.limit)}</td>
                            <td class="num">{format!("{:.2} / {:.2}", o.filled, o.shares)}</td>
                            <td class="num">{fmt::price(o.avg_price)}</td>
                            <td class="num">{fmt::usd(o.fees_usd)}</td>
                            <td class=status_class>{o.status.to_uppercase()}</td>
                            <td class="small muted">{fmt::utc_time(o.updated_ms)}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            view! {
                <section class="panel">
                    <div class="stack">
                        <div>
                            <div class="panel-title"><span>"POSITIONS"</span><span class="muted">"marked at best bid · closed positions keep realized PnL"</span></div>
                            <div class="table-wrap"><table>
                                <thead><tr><th>"Event"</th><th>"Bucket"</th><th>"Side"</th><th>"Shares"</th><th>"Avg"</th><th>"Mark"</th><th>"Cost"</th><th>"Unreal."</th><th>"Real."</th></tr></thead>
                                <tbody>{if prow.is_empty() { view! { <tr><td colspan="9" class="empty">"No positions yet. They appear once a signal passes every pre-trade gate."</td></tr> }.into_any() } else { prow.into_any() }}</tbody>
                            </table></div>
                        </div>
                        <div>
                            <div class="panel-title"><span>"ORDERS"</span><span class="muted">"simulated venue (paper)"</span></div>
                            <div class="table-wrap"><table>
                                <thead><tr><th>"Order"</th><th>"Strategy"</th><th>"Instrument"</th><th>"Limit"</th><th>"Filled"</th><th>"Avg"</th><th>"Fees"</th><th>"Status"</th><th>"Updated"</th></tr></thead>
                                <tbody>{if orow.is_empty() { view! { <tr><td colspan="9" class="empty">"No orders yet."</td></tr> }.into_any() } else { orow.into_any() }}</tbody>
                            </table></div>
                        </div>
                    </div>
                </section>
            }
        })
    }
}

#[component]
fn DecisionLog(snap: Snap) -> impl IntoView {
    let show_evaluations = RwSignal::new(false);
    let decisions = Memo::new(move |_| {
        snap.with(|s| s.as_ref().map(|s| s.decisions.clone()).unwrap_or_default())
    });
    let rows = move || {
        let all = show_evaluations.get();
        decisions.with(|ds| {
            let hidden = if all { 0 } else { ds.iter().filter(|d| d.strategy == "evaluation").count() };
            let rows = ds.iter()
                .filter(|d| all || d.strategy != "evaluation")
                .take(40)
                .map(|d| {
                    let (class, label) = match (d.approved, d.strategy.as_str()) {
                        (true, _) => ("good strong", "APPROVED"),
                        (false, "evaluation") => ("muted", "EVAL"),
                        (false, _) => ("bad strong", "REJECTED"),
                    };
                    let reasons = d
                        .reasons
                        .iter()
                        .map(|r| view! { <div class="reason">{r.clone()}</div> })
                        .collect::<Vec<_>>();
                    let details = (!d.details.is_empty()).then(|| {
                        let lines = d.details.iter().map(|l| view! { <li>{l.clone()}</li> }).collect::<Vec<_>>();
                        view! {
                            <details class="eval-lines">
                                <summary>{format!("why — {} line(s): price, probability used, EV, blocker", d.details.len())}</summary>
                                <ul>{lines}</ul>
                            </details>
                        }
                    });
                    view! {
                        <tr>
                            <td class="mono small muted">{format!("#{}", d.id)}</td>
                            <td class="small muted">{fmt::utc_time(d.at_ms)}</td>
                            <td class=class>{label}</td>
                            <td><div>{d.summary.clone()}</div>{reasons}{details}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            if rows.is_empty() {
                let text = if hidden > 0 {
                    format!("No trade decisions yet ({hidden} routine evaluations hidden — tick “show evaluations”).")
                } else {
                    "No decisions yet.".to_owned()
                };
                view! { <tr><td colspan="4" class="empty">{text}</td></tr> }.into_any()
            } else {
                rows.into_any()
            }
        })
    };
    view! {
        <section class="panel">
            <div class="panel-title">
                <span>"DECISION AUDIT LOG"</span>
                <label class="toggle small"><input type="checkbox" prop:checked=move || show_evaluations.get() on:change=move |_| show_evaluations.update(|v| *v = !*v) />" show evaluations"</label>
            </div>
            <div class="table-wrap tall"><table class="decisions"><tbody>{rows}</tbody></table></div>
        </section>
    }
}

#[component]
fn AlertsPanel(snap: Snap) -> impl IntoView {
    let alerts =
        Memo::new(move |_| snap.with(|s| s.as_ref().map(|s| s.alerts.clone()).unwrap_or_default()));
    move || {
        let rows = alerts.with(|a| {
            a.iter()
                .rev()
                .take(30)
                .map(|a| {
                    let class = match a.level.as_str() {
                        "critical" => "bad strong",
                        "warning" => "warn",
                        _ => "muted",
                    };
                    view! { <tr><td class="small muted">{fmt::utc_time(a.at_ms)}</td><td class=class>{a.level.to_uppercase()}</td><td class="small">{a.message.clone()}</td></tr> }
                })
                .collect::<Vec<_>>()
        });
        view! {
            <section class="panel">
                <div class="panel-title"><span>"ALERTS & EVENTS"</span></div>
                <div class="table-wrap tall"><table class="alerts"><tbody>{if rows.is_empty() { view! { <tr><td colspan="3" class="empty">"No alerts."</td></tr> }.into_any() } else { rows.into_any() }}</tbody></table></div>
            </section>
        }
    }
}

#[component]
fn EnginePanel(snap: Snap) -> impl IntoView {
    let engine = Memo::new(move |_| snap.with(|s| s.as_ref().map(|s| s.engine.clone())));
    move || {
        engine.get().map(|e| {
            let max = e.events_by_kind.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
            let kinds = e
                .events_by_kind
                .iter()
                .map(|(k, n)| {
                    let w = format!("width:{:.1}%", *n as f64 / max as f64 * 100.0);
                    view! { <div class="dist-row"><span class="dist-label">{k.replace('_', " ")}</span><span class="dist-bar"><span class="fill" style=w></span></span><span class="dist-val">{*n}</span></div> }
                })
                .collect::<Vec<_>>();
            view! {
                <section class="panel">
                    <div class="panel-title"><span>"DETERMINISTIC KERNEL"</span><span class="muted">"same code in backtest · paper · live"</span></div>
                    <div class="stats">
                        <div><label>"Events"</label><b>{e.events_total}</b><small>{format!("seq {}", e.last_seq)}</small></div>
                        <div><label>"Evaluations"</label><b>{e.evaluations_total}</b></div>
                        <div><label>"Proposals"</label><b>{e.proposals_total}</b></div>
                        <div><label>"Approved"</label><b class="good">{e.approvals_total}</b></div>
                        <div><label>"Rejected"</label><b class="bad">{e.rejections_total}</b></div>
                        <div><label>"Latency"</label><b>{fmt::micros(e.last_handle_micros)}</b><small>{format!("max {}", fmt::micros(e.max_handle_micros))}</small></div>
                    </div>
                    <div class="dist">{kinds}</div>
                </section>
            }
        })
    }
}

#[component]
fn BreakEvenPanel(snap: Snap) -> impl IntoView {
    let table = Memo::new(move |_| {
        snap.with(|s| s.as_ref().map(|s| s.break_even.clone()).unwrap_or_default())
    });
    move || {
        let rows = table.with(|t| {
            t.iter()
                .map(|r| {
                    view! {
                        <tr>
                            <td class="num">{format!("{:.2}", r.price)}</td>
                            <td class="num">{format!("{:.5}", r.fee_per_share)}</td>
                            <td class="num strong">{format!("{:.2}%", r.break_even_probability * 100.0)}</td>
                            <td class="num">{format!("{:.1}", r.wins_to_recover_one_loss)}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>()
        });
        view! {
            <section class="panel">
                <div class="panel-title"><span>"BREAK-EVEN REFERENCE"</span><span class="muted">"taker fee 5% × p(1−p) per share"</span></div>
                <div class="table-wrap"><table>
                    <thead><tr><th>"Price"</th><th>"Fee/share"</th><th>"Break-even p"</th><th>"Wins per loss"</th></tr></thead>
                    <tbody>{rows}</tbody>
                </table></div>
                <div class="muted small">"Buying at 0.95 needs > 95.2% true probability; one loss erases ~20 wins."</div>
            </section>
        }
    }
}

#[component]
fn KillSwitchModal(snap: Snap, open: RwSignal<bool>) -> impl IntoView {
    let token = RwSignal::new(String::new());
    let reason = RwSignal::new(String::new());
    let status = RwSignal::new(None::<Result<String, String>>);
    let busy = RwSignal::new(false);
    let engaged = move || snap.with(|s| s.as_ref().is_some_and(|s| s.kill_switch.is_some()));
    let submit = move |engage: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let (t, r) = (token.get_untracked(), reason.get_untracked());
        leptos::task::spawn_local(async move {
            let res = post_kill_switch(t, engage, r).await;
            busy.set(false);
            match res {
                Ok(()) => {
                    status.set(Some(Ok(if engage {
                        "Kill switch engaged."
                    } else {
                        "Kill switch released."
                    }
                    .to_owned())));
                    token.set(String::new());
                    reason.set(String::new());
                }
                Err(e) => status.set(Some(Err(e))),
            }
        });
    };
    view! {
        <Show when=move || open.get()>
            <div class="modal-backdrop" on:click=move |_| open.set(false)></div>
            <div class="modal" role="dialog" aria-modal="true">
                <h3>{move || if engaged() { "Release kill switch" } else { "Engage kill switch" }}</h3>
                <p class="muted small">"Stops all new orders immediately (existing positions are kept). Requires the operator token (WM_ADMIN_TOKEN); it is sent once and never stored."</p>
                <label>"Operator token"<input type="password" autocomplete="off" prop:value=move || token.get() on:input=move |ev| token.set(event_target_value(&ev)) /></label>
                <Show when=move || !engaged()>
                    <label>"Reason"<input type="text" maxlength="200" prop:value=move || reason.get() on:input=move |ev| reason.set(event_target_value(&ev)) /></label>
                </Show>
                {move || status.get().map(|s| match s {
                    Ok(m) => view! { <div class="good small">{m}</div> }.into_any(),
                    Err(e) => view! { <div class="bad small">{e}</div> }.into_any(),
                })}
                <div class="modal-actions">
                    <button class="btn" on:click=move |_| { status.set(None); open.set(false); }>"Close"</button>
                    <button class="btn kill" prop:disabled=move || busy.get() on:click=move |_| submit(!engaged())>
                        {move || if engaged() { "RELEASE" } else { "ENGAGE KILL SWITCH" }}
                    </button>
                </div>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_come_from_the_hash() {
        assert_eq!(route_from_hash(""), Route::Dashboard);
        assert_eq!(route_from_hash("#/"), Route::Dashboard);
        assert_eq!(
            route_from_hash("#/strategy/F_peak_slot"),
            Route::Strategy("F_peak_slot".into())
        );
        assert_eq!(route_from_hash("#/strategy/"), Route::Dashboard);
        assert_eq!(route_from_hash("#/strategy/<b>"), Route::Dashboard);
        assert_eq!(route_from_hash("#/strategy/a/b"), Route::Dashboard);
    }
}
