//! The paper runtime's link to the live venue: strategy G's live twin.
//!
//! The engine approves the live book's orders like any other; the session
//! hands them out ([`SessionOutput::live_orders`](wm_backtest::SessionOutput))
//! and they go to the live executor instead of the simulated exchange. The
//! executor's order updates come back as engine events. In the other
//! direction the executor's readiness becomes the engine's live gate, the
//! kill switch pulls every resting live order, and the engine's markets and
//! live positions go to the executor for its start-up checks.

use crate::config::AppConfig;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use wm_backtest::SimulationSession;
use wm_core::event::{EventEnvelope, EventSource, OrderUpdateEvent, WeatherMachineEvent};
use wm_core::trading::{OrderStatus, OrderUpdate, TimeInForce};
use wm_core::units::{Shares, Usd};
use wm_engine::Engine;
use wm_live::{BookView, Channels, Command, LiveOrder, LiveStatus, OrderSigner, Settings};
use wm_risk::ApprovedIntent;

/// The environment variable that holds the signer key. It is read once, at
/// start, and kept nowhere but in the live venue's signer.
pub(crate) const PRIVATE_KEY_VAR: &str = "WM_POLY_PRIVATE_KEY";

pub(crate) struct LiveLink {
    commands: mpsc::Sender<Command>,
    status: watch::Receiver<LiveStatus>,
    alerts: mpsc::Receiver<(&'static str, String)>,
    last_book: Option<BookView>,
    /// The kill switch's pull was sent (once per engagement).
    pulled: bool,
    /// A blocker of the runtime's own, beside the venue's.
    held: Option<String>,
}

/// Start the live venue for the live twin. `Err`: why the twin stays off
/// (it then blocks with that reason).
pub(crate) fn start(
    cfg: &AppConfig,
    user_agent: &str,
    events: mpsc::Sender<EventEnvelope>,
    shutdown: watch::Receiver<bool>,
) -> Result<(LiveLink, JoinHandle<()>), String> {
    let l = &cfg.file.live;
    let funder = cfg
        .env
        .poly_funder
        .clone()
        .ok_or("WM_POLY_FUNDER (the Polymarket wallet that holds the money) is not set")?;
    let signer = OrderSigner::from_env(PRIVATE_KEY_VAR)?;
    let settings = Settings {
        clob_url: l.clob_url.clone(),
        data_url: cfg.file.providers.polymarket_data.base_url.clone(),
        geoblock_url: l.geoblock_url.clone(),
        funder,
        signature_type: l.signature_type,
        api_key_nonce: l.api_key_nonce,
        geo_close_only: l.geo_api_close_only.clone(),
        require_heartbeat: l.require_heartbeat,
        // a last guard behind the risk check's position size
        max_order_usd: l.notional + Usd::from_micros(10_000),
        user_agent: user_agent.to_owned(),
        poll_every_s: 5,
    };
    tracing::info!(
        signer = %signer.address,
        funder = %settings.funder,
        signature_type = settings.signature_type,
        "starting the live venue for strategy G's live twin"
    );
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (status_tx, status_rx) = watch::channel(LiveStatus::default());
    let (alerts_tx, alerts_rx) = mpsc::channel(256);
    let channels = Channels {
        commands: cmd_rx,
        events,
        status: status_tx,
        alerts: alerts_tx,
    };
    let task = tokio::spawn(wm_live::executor::run(settings, signer, channels, shutdown));
    Ok((
        LiveLink {
            commands: cmd_tx,
            status: status_rx,
            alerts: alerts_rx,
            last_book: None,
            pulled: false,
            held: None,
        },
        task,
    ))
}

impl LiveLink {
    /// The executor's latest status.
    pub(crate) fn status(&self) -> LiveStatus {
        self.status.borrow().clone()
    }

    /// Keep the twin closed for a reason of the runtime's own (until a
    /// restart).
    pub(crate) fn hold(&mut self, session: &mut SimulationSession, why: String) {
        self.held = Some(why);
        self.push_gate(session);
    }

    fn push_gate(&self, session: &mut SimulationSession) {
        let st = self.status.borrow();
        let mut blockers = st.blockers.clone();
        blockers.extend(self.held.clone());
        session.engine_mut().set_live_gate(blockers, st.location_ok);
    }

    /// Hand the session's approved live orders to the venue. One that cannot
    /// be handed over ends at once, so the engine releases its exposure.
    pub(crate) fn forward(
        &mut self,
        session: &mut SimulationSession,
        orders: Vec<ApprovedIntent>,
        at: DateTime<Utc>,
    ) {
        for a in orders {
            let refusal = match order_of(session.engine(), &a) {
                Ok(o) => match self.commands.try_send(Command::Place(o)) {
                    Ok(()) => None,
                    Err(_) => Some("the live venue is not taking orders (queue full or stopped)"),
                },
                Err(why) => {
                    tracing::warn!(order = %a.client_order_id(), "live order not placed: {why}");
                    Some(why)
                }
            };
            if let Some(why) = refusal {
                session.push(rejected(&a, why, at));
            }
        }
    }

    /// The executor's readiness into the engine's gate, its alerts onto the
    /// dashboard, the kill switch's pull, and the markets to the executor.
    pub(crate) fn sync(
        &mut self,
        session: &mut SimulationSession,
        mut alert: impl FnMut(&'static str, String),
    ) {
        if self.status.has_changed().unwrap_or(false) {
            self.status.mark_unchanged();
            self.push_gate(session);
        }
        while let Ok((level, text)) = self.alerts.try_recv() {
            alert(level, text);
        }
        match session.engine().kill_switch() {
            Some(why) if !self.pulled => {
                let why = format!("kill switch engaged: {why}");
                if self.commands.try_send(Command::CancelAll(why)).is_ok() {
                    self.pulled = true;
                }
            }
            None => self.pulled = false,
            _ => {}
        }
        let book = book_view(session.engine());
        if self.last_book.as_ref() != Some(&book)
            && self.commands.try_send(Command::Book(book.clone())).is_ok()
        {
            self.last_book = Some(book);
        }
    }
}

/// The venue order of an approved live intent: the book's tick (the price
/// is on it), the market's minimum size and neg-risk flag, the GTD end.
pub(crate) fn order_of(engine: &Engine, a: &ApprovedIntent) -> Result<LiveOrder, &'static str> {
    let i = a.intent();
    let market = engine
        .markets()
        .get(&i.event_slug)
        .ok_or("the market is not known")?;
    let outcome = market
        .outcomes
        .iter()
        .find(|o| o.no_token == i.token || o.yes_token == i.token)
        .ok_or("the token is not in its market")?;
    let tick = engine
        .book(&i.token)
        .map(|b| b.tick_size)
        .filter(|t| t.micros() > 0)
        .unwrap_or(outcome.tick_size);
    let TimeInForce::Gtd { expires_at } = i.tif else {
        return Err("a live order rests until a date (GTD), and this one does not");
    };
    Ok(LiveOrder {
        client_order_id: a.client_order_id().clone(),
        token: i.token.clone(),
        side: i.side,
        price: i.limit_price,
        tick,
        shares: i.shares,
        min_size: outcome.min_order_size,
        neg_risk: market.neg_risk,
        expires_at,
    })
}

/// The open weather markets' tokens and the live book's holdings.
pub(crate) fn book_view(engine: &Engine) -> BookView {
    let mut v = BookView::default();
    for m in engine.markets().values().filter(|m| !m.closed) {
        for o in &m.outcomes {
            v.tokens.insert(o.yes_token.to_string());
            v.tokens.insert(o.no_token.to_string());
        }
    }
    if let Some(p) = engine.live_positions() {
        for pos in p.open_positions() {
            if pos.shares > Shares::ZERO {
                v.held.insert(pos.instrument.token.to_string(), pos.shares);
            }
        }
    }
    v
}

fn rejected(a: &ApprovedIntent, why: &str, at: DateTime<Utc>) -> EventEnvelope {
    EventEnvelope::new(
        at,
        EventSource::Live,
        WeatherMachineEvent::OrderUpdate(OrderUpdateEvent {
            update: OrderUpdate {
                client_order_id: a.client_order_id().clone(),
                venue_order_id: None,
                status: OrderStatus::Rejected,
                filled: Shares::ZERO,
                avg_price: None,
                fee_paid: Usd::ZERO,
                reason: Some(why.to_owned()),
                ts: at,
            },
            fill: None,
        }),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::Arc;
    use wm_core::ids::{LocationId, RunId, StationId};
    use wm_execution::SimConfig;

    #[test]
    fn a_live_book_view_lists_the_open_markets_tokens() {
        let cfg = crate::config::AppConfig::load(Some(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../configs/weather-machine.toml"),
        ))
        .unwrap();
        let mut engine_cfg = crate::setup::engine_config(
            &cfg,
            wm_core::trading::RunMode::Paper,
            RunId::deterministic(3),
        )
        .unwrap();
        let mut on = cfg.clone();
        on.file.live.enabled = true;
        engine_cfg.live = on.live_book();
        let mut s = SimulationSession::new(
            engine_cfg,
            SimConfig::default(),
            chrono::Duration::hours(2),
            Arc::new(wm_strategy::NoEdgeModel),
        );
        let at: DateTime<Utc> = "2026-07-01T06:00:00Z".parse().unwrap();
        let m = wm_core::synthetic::synthetic_temperature_market(
            &LocationId::new("amsterdam").unwrap(),
            &StationId::new("EHAM").unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            chrono_tz::Europe::Amsterdam,
            13,
            24,
            at,
        );
        s.push(EventEnvelope::new(
            at,
            EventSource::Replay,
            WeatherMachineEvent::MarketSnapshot(wm_core::event::MarketSnapshotEvent {
                market: m.clone(),
            }),
        ));
        s.run_until(at);
        let v = book_view(s.engine());
        assert_eq!(v.tokens.len(), m.outcomes.len() * 2);
        assert!(v.tokens.contains(m.outcomes[0].no_token.as_str()));
        assert!(v.held.is_empty(), "the live book holds nothing");
    }
}
