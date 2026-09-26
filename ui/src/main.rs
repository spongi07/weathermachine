//! Weather Machine dashboard — Rust compiled to WebAssembly.
//!
//! Receives [`wm_dashboard_api::DashboardSnapshot`]s over Server-Sent Events
//! and renders them with Leptos' fine-grained reactivity: each panel reads a
//! memoised slice of the snapshot and re-renders only when that slice changes.

mod app;
mod chart;
mod fmt;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("weather-machine UI panic: {info}");
        web_sys::console::error_1(&msg.clone().into());
        // Make the failure visible to the operator instead of a frozen screen.
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            let _ = body.insert_adjacent_text(
                "afterbegin",
                &format!("{msg} — reload the page or use /lite"),
            );
        }
    }));
    leptos::mount::mount_to_body(app::App);
}
