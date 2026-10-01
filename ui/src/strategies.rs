//! Strategy cards, one page per strategy, the reports panel, and copying
//! logs and reports to the clipboard (to paste them into a conversation).

use crate::app::{Snap, js_err};
use crate::fmt::{self, DASH};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use wm_dashboard_api::*;

/// URL of a strategy's log (Markdown as text).
pub fn log_url(id: &str) -> String {
    format!("api/v1/strategies/{id}/log")
}

/// Hash route of a strategy's page.
pub fn page_href(id: &str) -> String {
    format!("#/strategy/{id}")
}

// ---------------------------------------------------------------------------
// Fetching and copying
// ---------------------------------------------------------------------------

/// GET `url` as text; an HTTP error becomes `Err` with its body.
pub async fn fetch_text(url: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(js_err)?
        .dyn_into()
        .map_err(js_err)?;
    let text = JsFuture::from(resp.text().map_err(js_err)?)
        .await
        .map_err(js_err)?
        .as_string()
        .unwrap_or_default();
    if resp.ok() {
        Ok(text)
    } else {
        Err(format!("HTTP {}: {}", resp.status(), text.trim()))
    }
}

/// Copy `text`: the clipboard API where the page is a secure context
/// (https, localhost), else a selected textarea and `execCommand("copy")`
/// (a dashboard on plain http in the LAN).
async fn copy_text(text: &str) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let navigator = window.navigator();
    let has_api = window.is_secure_context()
        && js_sys::Reflect::get(&navigator, &JsValue::from_str("clipboard"))
            .is_ok_and(|c| c.is_object());
    if has_api
        && JsFuture::from(navigator.clipboard().write_text(text))
            .await
            .is_ok()
    {
        return Ok(());
    }
    copy_by_selection(text)
}

fn copy_by_selection(text: &str) -> Result<(), String> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let body = document.body().ok_or("no body")?;
    let area: web_sys::HtmlTextAreaElement = document
        .create_element("textarea")
        .map_err(js_err)?
        .dyn_into()
        .map_err(|_| "no textarea".to_owned())?;
    area.set_value(text);
    let _ = area.set_attribute("readonly", "");
    let _ = area.set_attribute(
        "style",
        "position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;",
    );
    body.append_child(&area).map_err(js_err)?;
    area.select();
    let copied = document
        .dyn_ref::<web_sys::HtmlDocument>()
        .and_then(|d| d.exec_command("copy").ok())
        .unwrap_or(false);
    let _ = body.remove_child(&area);
    if copied {
        Ok(())
    } else {
        Err("the browser did not allow copying".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CopyState {
    Idle,
    Busy,
    Copied(usize),
    Failed(String),
}

/// A button that fetches `url` and copies the text; if the browser refuses,
/// the text opens selected in a dialog to copy with Ctrl+C / ⌘C.
#[component]
pub fn CopyButton(url: String, #[prop(into)] label: String) -> impl IntoView {
    let state = RwSignal::new(CopyState::Idle);
    let manual = RwSignal::new(None::<String>);
    let click = move |_| {
        let url = url.clone();
        state.set(CopyState::Busy);
        leptos::task::spawn_local(async move {
            match fetch_text(&url).await {
                Err(e) => state.set(CopyState::Failed(e)),
                Ok(text) => match copy_text(&text).await {
                    Ok(()) => state.set(CopyState::Copied(text.chars().count())),
                    Err(_) => {
                        state.set(CopyState::Idle);
                        manual.set(Some(text));
                    }
                },
            }
        });
    };
    let status = move || {
        match state.get() {
        CopyState::Idle => view! { <span></span> }.into_any(),
        CopyState::Busy => view! { <span class="small muted">"copying…"</span> }.into_any(),
        CopyState::Copied(n) => view! { <span class="small good">{format!("copied ✓ {n} characters — paste it into the chat")}</span> }.into_any(),
        CopyState::Failed(e) => view! { <span class="small bad">{e}</span> }.into_any(),
    }
    };
    view! {
        <span class="copy">
            <button class="btn" prop:disabled=move || state.get() == CopyState::Busy on:click=click>{label}</button>
            {status}
            {move || manual.get().map(|text| view! { <ManualCopy text=text open=manual /> })}
        </span>
    }
}

/// The text selected in a dialog, for browsers that refuse to copy.
#[component]
fn ManualCopy(text: String, open: RwSignal<Option<String>>) -> impl IntoView {
    let area = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move |_| {
        if let Some(el) = area.get() {
            let _ = el.focus();
            el.select();
        }
    });
    view! {
        <div class="modal-backdrop" on:click=move |_| open.set(None)></div>
        <div class="modal copy-modal" role="dialog" aria-label="Copy the text">
            <h3>"COPY THE TEXT"</h3>
            <p class="small muted">"This browser did not allow copying automatically. The text is selected: press Ctrl+C (⌘C on a Mac), then paste it into the chat."</p>
            <textarea node_ref=area class="copy-area" readonly=true>{text}</textarea>
            <div class="modal-actions"><button class="btn" on:click=move |_| open.set(None)>"Close"</button></div>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Dashboard: strategy cards and reports
// ---------------------------------------------------------------------------

#[component]
pub fn StrategiesPanel(snap: Snap) -> impl IntoView {
    let ids = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.strategies
                        .iter()
                        .map(|x| x.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    view! {
        <section class="panel">
            <div class="panel-title">
                <span>"STRATEGIES"</span>
                <span class="muted">"open a strategy to monitor it · “Copy log” copies everything about it, ready to paste"</span>
            </div>
            <Show when=move || !ids.with(Vec::is_empty) fallback=|| view! { <div class="muted small">"The engine has not reported its strategies yet."</div> }>
                <div class="strategy-cards">
                    <For each=move || ids.get() key=|id| id.clone() children=move |id| view! { <StrategyCard snap=snap id=id /> } />
                </div>
            </Show>
        </section>
    }
}

#[component]
fn StrategyCard(snap: Snap, id: String) -> impl IntoView {
    let key = id.clone();
    let data = Memo::new(move |_| {
        snap.with(|s| {
            let s = s.as_ref()?;
            let st = s.strategies.iter().find(|x| x.id == key)?;
            Some((st.clone(), s.strategy_status(st), s.strategy_counts(st)))
        })
    });
    let href = page_href(&id);
    let url = log_url(&id);
    move || {
        let (st, (class, status), (approved, rejected, orders, filled)) = data.get()?;
        Some(view! {
            <div class={if st.enabled { "strategy-card" } else { "strategy-card off" }}>
                <div class="card-head">
                    <a class="card-letter" href=href.clone()>{st.letter.clone()}</a>
                    <a class="card-name" href=href.clone()>{st.name.clone()}</a>
                    <span class={if st.enabled { "pill good" } else { "pill" }}>{if st.enabled { "ON" } else { "OFF" }}</span>
                </div>
                <div class=format!("card-status small {class}")>{status}</div>
                <div class="card-counts small muted">{format!("this run: {approved} approved · {rejected} rejected · {orders} order(s), {filled} filled")}</div>
                <div class="card-actions">
                    <a class="btn" href=href.clone()>"Open"</a>
                    <CopyButton url=url.clone() label="Copy log" />
                </div>
            </div>
        })
    }
}

#[component]
pub fn ReportsPanel(snap: Snap) -> impl IntoView {
    let list = RwSignal::new(None::<Result<Vec<ResearchReportDto>, String>>);
    let load = move || {
        leptos::task::spawn_local(async move {
            let r = fetch_text("api/v1/research").await.and_then(|t| {
                serde_json::from_str::<Vec<ResearchReportDto>>(&t).map_err(|e| e.to_string())
            });
            list.set(Some(r));
        });
    };
    load();
    let demo = move || snap.with(|s| s.as_ref().is_some_and(|s| s.demo));
    let rows = move || match list.get() {
        None => view! { <tr><td colspan="3" class="empty">"Loading…"</td></tr> }.into_any(),
        Some(Err(e)) => view! { <tr><td colspan="3" class="empty">{e}</td></tr> }.into_any(),
        Some(Ok(v)) if v.is_empty() => {
            view! { <tr><td colspan="3" class="empty">"No research reports configured."</td></tr> }
                .into_any()
        }
        Some(Ok(v)) => v
            .into_iter()
            .map(|r| {
                let url = format!("api/v1/research/{}", r.name);
                let state = if r.available {
                    format!(
                        "{} KB · {}",
                        r.bytes.div_ceil(1024),
                        r.modified_ms
                            .map_or_else(|| DASH.to_owned(), fmt::utc_datetime)
                    )
                } else {
                    "not produced yet".to_owned()
                };
                let actions = if r.available {
                    view! {
                        <CopyButton url=url.clone() label="Copy" />
                        " "<a class="btn" href=format!("{url}?download=1")>"Download"</a>
                        " "<a class="btn" href=url.clone() target="_blank" rel="noopener">"Open"</a>
                    }
                    .into_any()
                } else {
                    view! { <span class="small muted">{r.how.clone()}</span> }.into_any()
                };
                view! {
                    <tr>
                        <td class="text"><b>{r.title.clone()}</b></td>
                        <td class="small muted">{state}</td>
                        <td class="text">{actions}</td>
                    </tr>
                }
            })
            .collect::<Vec<_>>()
            .into_any(),
    };
    view! {
        <section class="panel">
            <div class="panel-title">
                <span>"REPORTS TO PASTE"</span>
                <button class="btn small-btn" on:click=move |_| load()>"Refresh"</button>
            </div>
            <div class="table-wrap"><table class="reports">
                <thead><tr><th>"Report"</th><th>"File"</th><th>"Copy · save · open"</th></tr></thead>
                <tbody>
                    {rows}
                    <tr>
                        <td class="text"><b>"Paper run day by day (database): weather, every strategy's evaluations and blockers, proposals, orders, P&L per strategy"</b></td>
                        <td class="small muted">{move || if demo() { "not in demo mode" } else { "live, last 31 days" }}</td>
                        <td class="text">
                            <CopyButton url="api/v1/report/paper".to_owned() label="Copy" />
                            " "<a class="btn" href="api/v1/report/paper" target="_blank" rel="noopener">"Open"</a>
                        </td>
                    </tr>
                </tbody>
            </table></div>
        </section>
    }
}

// ---------------------------------------------------------------------------
// One strategy's page
// ---------------------------------------------------------------------------

#[component]
pub fn StrategyPage(snap: Snap, id: String) -> impl IntoView {
    let key = id.clone();
    let strategy = Memo::new(move |_| {
        snap.with(|s| {
            s.as_ref()
                .and_then(|s| s.strategies.iter().find(|x| x.id == key).cloned())
        })
    });
    let url = log_url(&id);
    // The log as the Copy button copies it, refreshed every minute.
    let log = RwSignal::new(None::<Result<String, String>>);
    let log_at = RwSignal::new(0.0_f64);
    let fetch_url = url.clone();
    let refresh = move || {
        let u = fetch_url.clone();
        leptos::task::spawn_local(async move {
            log.set(Some(fetch_text(&u).await));
            log_at.set(js_sys::Date::now());
        });
    };
    refresh();
    if let Ok(handle) =
        set_interval_with_handle(refresh.clone(), std::time::Duration::from_secs(60))
    {
        on_cleanup(move || handle.clear());
    }
    let header = move || {
        strategy.get().map(|s| {
            view! {
                <div class="page-head">
                    <a class="btn" href="#/">"← Dashboard"</a>
                    <h2>{format!("STRATEGY {} · {}", s.letter, s.name)}</h2>
                    <span class={if s.enabled { "pill good" } else { "pill" }}>{if s.enabled { "ENABLED" } else { "DISABLED" }}</span>
                    <span class="muted mono small">{s.id.clone()}</span>
                </div>
                <p class="summary">{s.summary.clone()}</p>
            }
        })
    };
    let not_found = {
        let id = id.clone();
        move || {
            strategy.with(Option::is_none).then(|| {
                view! { <div class="banner bad">{format!("No strategy '{id}'. ")}<a href="#/">"Back to the dashboard"</a></div> }
            })
        }
    };
    let preview = move || match log.get() {
        None => view! { <div class="muted small">"Loading the log…"</div> }.into_any(),
        Some(Err(e)) => view! { <div class="bad small">{e}</div> }.into_any(),
        Some(Ok(text)) => view! { <pre class="log-preview">{text}</pre> }.into_any(),
    };
    let r3 = refresh.clone();
    view! {
        <section class="panel strategy-page">
            {not_found}
            {header}
            <div class="log-actions">
                <CopyButton url=url.clone() label="Copy log" />
                <a class="btn" href=format!("{url}?download=1")>"Download .md"</a>
                <a class="btn" href=url.clone() target="_blank" rel="noopener">"Open as text"</a>
                <span class="small muted">"The log holds this strategy's live state, this run's decisions and orders, and its last 7 days from the database."</span>
            </div>
        </section>
        <PeakSlotPanel strategy=strategy />
        <NowPanel snap=snap strategy=strategy />
        <div class="grid-2">
            <RunPanel snap=snap strategy=strategy />
            <SettingsPanel strategy=strategy />
        </div>
        <section class="panel">
            <div class="panel-title">
                <span>"LOG PREVIEW"</span>
                <span class="small muted">
                    {move || if log_at.get() > 0.0 { format!("as of {} · refreshes every minute ", fmt::utc_time(log_at.get() as i64)) } else { String::new() }}
                    <button class="btn small-btn" on:click=move |_| r3()>"Refresh"</button>
                </span>
            </div>
            {preview}
        </section>
    }
}

#[component]
fn PeakSlotPanel(strategy: Memo<Option<StrategyDto>>) -> impl IntoView {
    move || {
        strategy.with(|s| {
            let p = s.as_ref()?.peak_slot.clone()?;
            let today = p
                .today
                .iter()
                .map(|t| {
                    view! {
                        <div class="slot-today">
                            <span class="big">{format!("{}–{}", t.start, t.end)}</span>
                            <span class={if t.inside { "pill good" } else { "pill" }}>{if t.inside { "INSIDE THE SLOT" } else { "OUTSIDE THE SLOT" }}</span>
                            <span>{format!("{} · {} · local {} — {}", t.location, t.season, t.local_time, t.status)}</span>
                        </div>
                    }
                })
                .collect::<Vec<_>>();
            let rows = p
                .seasons
                .iter()
                .map(|x| {
                    view! {
                        <tr>
                            <td>{x.season.clone()}</td>
                            <td class="num">{format!("{}–{}", x.start, x.end)}</td>
                            <td class="num">{if x.days == 0 { "fallback".to_owned() } else { x.days.to_string() }}</td>
                            <td class="num">{x.mean.clone().unwrap_or_else(|| DASH.to_owned())}</td>
                            <td class="num">{x.median.clone().unwrap_or_else(|| DASH.to_owned())}</td>
                            <td class="num">{x.q90.clone().unwrap_or_else(|| DASH.to_owned())}</td>
                            <td class="num">{x.later_than_slot.map_or_else(|| DASH.to_owned(), |v| format!("{:.0}%", 100.0 * v))}</td>
                        </tr>
                    }
                })
                .collect::<Vec<_>>();
            Some(view! {
                <section class="panel">
                    <div class="panel-title"><span>"PEAK SLOT"</span><span class="muted small">{p.source.clone()}</span></div>
                    {today}
                    <div class="table-wrap"><table>
                        <thead><tr><th>"Season"</th><th>"Slot"</th><th>"Days"</th><th>"Mean"</th><th>"Median"</th><th>"90%"</th><th>"Later than the slot"</th></tr></thead>
                        <tbody>{rows}</tbody>
                    </table></div>
                    <div class="muted small">"Local time at which the day's whole-degree METAR high was first reported, per season. F trades only inside the slot."</div>
                </section>
            })
        })
    }
}

#[component]
fn NowPanel(snap: Snap, strategy: Memo<Option<StrategyDto>>) -> impl IntoView {
    let data = Memo::new(move |_| {
        let id = strategy.with(|s| s.as_ref().map(|s| s.id.clone()))?;
        snap.with(|s| {
            let s = s.as_ref()?;
            let blocking: Vec<CheckDto> = s.risk.checks.iter().filter(|c| !c.ok).cloned().collect();
            let locations: Vec<(LocationDto, Vec<EvaluationDto>)> = s
                .locations
                .iter()
                .map(|l| {
                    let evals = l
                        .evaluations
                        .iter()
                        .filter(|e| e.strategy == id)
                        .cloned()
                        .collect();
                    (l.clone(), evals)
                })
                .collect();
            Some((blocking, locations))
        })
    });
    move || {
        let (blocking, locations) = data.get()?;
        let gates = if blocking.is_empty() {
            view! { <div class="good small">"All pre-trade gates pass."</div> }.into_any()
        } else {
            let items = blocking
                .iter()
                .map(|c| view! { <li><b>{c.name.clone()}</b>" — "{c.detail.clone()}</li> })
                .collect::<Vec<_>>();
            view! { <div class="bad small">"Blocking every new position:"<ul class="gates">{items}</ul></div> }.into_any()
        };
        let places = locations
            .into_iter()
            .map(|(l, evals)| {
                let high = l
                    .market
                    .as_ref()
                    .and_then(|m| m.rows.iter().find(|r| r.contains_high).map(|r| r.label.clone()));
                let view_ = l.views.first().cloned();
                let rows = evals
                    .iter()
                    .map(|e| {
                        let hl = high.as_deref() == Some(e.bucket.as_str());
                        view! {
                            <tr class={if hl { "hl" } else { "" }}>
                                <td class="bucket">{e.bucket.clone()}</td>
                                <td class={if e.side == "YES" { "good strong" } else { "bad strong" }}>{e.side.clone()}</td>
                                <td class="num ask">{fmt::price(e.ask)}</td>
                                <td class="num bid">{fmt::price(e.bid)}</td>
                                <td class="num">{fmt::opt(e.p_win, 3)}</td>
                                <td class="num">{fmt::opt(e.model_p, 3)}</td>
                                <td class="num">{fmt::opt(e.market_p, 3)}</td>
                                <td class="num">{fmt::signed(e.ev, 4)}</td>
                                <td class="why">{if e.signal { view! { <span class="chip sig">"SIGNAL"</span> }.into_any() } else { view! { <span class="muted">{e.blockers.join("; ")}</span> }.into_any() }}</td>
                            </tr>
                        }
                    })
                    .collect::<Vec<_>>();
                let body = if rows.is_empty() {
                    view! { <tr><td colspan="9" class="empty">"No evaluation of this strategy right now (disabled, no market, no model, incomplete views, or no report since the start)."</td></tr> }.into_any()
                } else {
                    rows.into_any()
                };
                view! {
                    <div class="now-loc">
                        <div class="muted small">{format!(
                            "{} · {} · local {} {} · {} · high so far {} ({}) · last report {}",
                            l.location.to_uppercase(),
                            l.station,
                            l.local_date,
                            l.local_time,
                            l.current_temp_c.map_or_else(|| DASH.to_owned(), |t| format!("{t:.0} °C now")),
                            view_.as_ref().and_then(|v| v.high_whole).map_or_else(|| DASH.to_owned(), |h| format!("{h} °C")),
                            view_.as_ref().and_then(ViewDto::high_times).unwrap_or_else(|| DASH.to_owned()),
                            l.last_observation_age_s.map_or_else(|| "none".to_owned(), |a| format!("{} ago", fmt::age(a))),
                        )}</div>
                        <div class="table-wrap"><table class="ladder">
                            <thead><tr><th>"Bucket"</th><th>"Side"</th><th>"Ask"</th><th>"Bid"</th><th>"P used"</th><th>"Model"</th><th>"Market"</th><th>"EV/share"</th><th>"Signal / blockers"</th></tr></thead>
                            <tbody>{body}</tbody>
                        </table></div>
                    </div>
                }
            })
            .collect::<Vec<_>>();
        Some(view! {
            <section class="panel">
                <div class="panel-title"><span>"NOW"</span><span class="muted small">"this strategy's evaluation of every bucket of today's market"</span></div>
                {gates}
                {places}
            </section>
        })
    }
}

#[component]
fn RunPanel(snap: Snap, strategy: Memo<Option<StrategyDto>>) -> impl IntoView {
    let data = Memo::new(move |_| {
        let s = strategy.get()?;
        snap.with(|snap| {
            let snap = snap.as_ref()?;
            let proposals: Vec<DecisionDto> = snap
                .decisions
                .iter()
                .filter(|d| d.strategy == s.id)
                .cloned()
                .collect();
            let trail: Vec<(i64, String)> = snap
                .decisions
                .iter()
                .filter(|d| d.strategy == "evaluation")
                .flat_map(|d| {
                    d.details
                        .iter()
                        .filter(|l| s.owns_line(l))
                        .map(move |l| (d.at_ms, l.clone()))
                })
                .take(30)
                .collect();
            let orders: Vec<OrderDto> = snap
                .orders
                .iter()
                .filter(|o| o.strategy == s.id)
                .cloned()
                .collect();
            Some((proposals, trail, orders))
        })
    });
    move || {
        let (proposals, trail, orders) = data.get()?;
        let prop_rows = proposals
            .iter()
            .map(|d| {
                view! {
                    <li>
                        <span class="small muted">{fmt::utc_time(d.at_ms)}" "</span>
                        <b class={if d.approved { "good" } else { "bad" }}>{if d.approved { "APPROVED " } else { "REJECTED " }}</b>
                        {d.summary.clone()}
                        {(!d.reasons.is_empty()).then(|| view! { <div class="reason">{d.reasons.join("; ")}</div> })}
                    </li>
                }
            })
            .collect::<Vec<_>>();
        let trail_rows = trail
            .iter()
            .map(|(at, line)| view! { <li><span class="small muted">{fmt::utc_time(*at)}" "</span><span class="mono small">{line.clone()}</span></li> })
            .collect::<Vec<_>>();
        let order_rows = orders
            .iter()
            .map(|o| {
                view! {
                    <tr>
                        <td class="small muted">{fmt::utc_time(o.created_ms)}</td>
                        <td>{format!("{} {} {}", o.side, o.outcome, o.bucket)}</td>
                        <td class="num">{format!("{:.3}", o.limit)}</td>
                        <td class="num">{format!("{:.0} / {:.0}", o.filled, o.shares)}</td>
                        <td class="num">{fmt::price(o.avg_price)}</td>
                        <td>{o.status.to_uppercase()}</td>
                    </tr>
                }
            })
            .collect::<Vec<_>>();
        Some(view! {
            <section class="panel">
                <div class="panel-title"><span>"THIS RUN"</span><span class="muted small">"since the service started"</span></div>
                <div class="panel-title sub"><span>"PROPOSALS AND RISK VERDICTS"</span></div>
                {if prop_rows.is_empty() { view! { <div class="muted small">"None yet."</div> }.into_any() } else { view! { <ul class="plain">{prop_rows}</ul> }.into_any() }}
                <div class="panel-title sub"><span>"ITS LINES IN THE ROUTINE EVALUATIONS"</span><span class="muted small">"latest 30"</span></div>
                {if trail_rows.is_empty() { view! { <div class="muted small">"None in the decision log yet (one evaluation per weather report while a market is open)."</div> }.into_any() } else { view! { <ul class="plain trail">{trail_rows}</ul> }.into_any() }}
                <div class="panel-title sub"><span>"ORDERS"</span></div>
                {if order_rows.is_empty() {
                    view! { <div class="muted small">"None."</div> }.into_any()
                } else {
                    view! { <div class="table-wrap"><table><thead><tr><th>"Created"</th><th>"Instrument"</th><th>"Limit"</th><th>"Filled"</th><th>"Avg"</th><th>"Status"</th></tr></thead><tbody>{order_rows}</tbody></table></div> }.into_any()
                }}
            </section>
        })
    }
}

#[component]
fn SettingsPanel(strategy: Memo<Option<StrategyDto>>) -> impl IntoView {
    move || {
        strategy.with(|s| {
            let s = s.as_ref()?;
            let rows = s
                .settings
                .iter()
                .map(|(k, v)| view! { <tr><td class="mono small">{k.clone()}</td><td class="wrap mono">{v.clone()}</td></tr> })
                .collect::<Vec<_>>();
            Some(view! {
                <section class="panel">
                    <div class="panel-title"><span>"SETTINGS"</span><span class="muted small">"from the configuration file"</span></div>
                    <div class="table-wrap"><table><tbody>{rows}</tbody></table></div>
                </section>
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_routes() {
        assert_eq!(log_url("F_peak_slot"), "api/v1/strategies/F_peak_slot/log");
        assert_eq!(page_href("F_peak_slot"), "#/strategy/F_peak_slot");
    }
}
