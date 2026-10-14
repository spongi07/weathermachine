//! The live executor: one task that places, follows and cancels the live
//! book's orders, and says whether a new one may go out.
//!
//! **Readiness** is a list of blockers, and the venue is ready only when it
//! is empty: API credentials derived; a protocol this signer knows (2 or 3);
//! the account not in closed-only mode; this server's location allowed to
//! trade on the API; the venue taking the heartbeats (its dead-man's switch,
//! which cancels every open order of these credentials ten seconds after
//! they stop); the leftovers of an earlier run cancelled; no shares at the
//! venue in the open weather markets beyond the books'; no order of
//! unknown fate; the collateral read. Whenever it is not ready, the orders
//! resting in the book are pulled: a bid nobody can watch is cancelled.
//!
//! **An order** is post-only and good till its date: the venue rests it or
//! refuses it, it never takes. It is followed through its record
//! (`GET /data/order/{id}`, the id being the EIP-712 digest computed before
//! it is sent): fills as they come (a maker is filled at its own price and
//! pays no fee), and its end. At the end the strategy gave it (G: ten
//! minutes before the next report) it is cancelled, and the venue ends it
//! there too (its stated expiration is a minute later: the venue ends GTD
//! orders a minute early). Each change reaches the engine as an order update,
//! exactly as the simulated venue reports paper orders.
//!
//! **An answer that proves nothing** (a timeout, a 5xx) leaves the order's
//! fate unknown: no new order goes out until its record settles it, and an
//! order the venue still has no record of ten seconds after it was sent was
//! never placed.

use crate::answer::{self, CancelAnswer, PostOutcome};
use crate::geo::{Geo, GeoStatus};
use crate::order::{self, OrderSpec, Wallet};
use crate::rest::{self, Auth, Clob};
use crate::sign::{OrderSigner, Protocol};
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use wm_core::event::{EventEnvelope, EventSource, OrderUpdateEvent, WeatherMachineEvent};
use wm_core::ids::{ClientOrderId, TokenId};
use wm_core::market::Side;
use wm_core::trading::{Fill, Liquidity, OrderStatus, OrderUpdate};
use wm_core::units::{Price, Rounding, Shares, Usd, notional};

/// How often the heartbeat goes out (the venue waits ten seconds).
pub const HEARTBEAT_EVERY_S: u64 = 5;
/// A heartbeat taken longer ago than this counts as none.
pub const HEARTBEAT_FRESH_S: i64 = 12;
/// An order of unknown fate without a record this long after it was sent
/// was never placed.
pub const ABSENT_AFTER_S: i64 = 10;

/// What the operator configured.
#[derive(Debug, Clone)]
pub struct Settings {
    pub clob_url: String,
    pub data_url: String,
    pub geoblock_url: String,
    /// The funder: the wallet that holds the money.
    pub funder: String,
    /// 0 EOA, 1 POLY_PROXY, 2 POLY_GNOSIS_SAFE.
    pub signature_type: u8,
    /// Which API key the L1 signature derives.
    pub api_key_nonce: u64,
    /// Replaces the built-in close-only list of the geoblock check.
    pub geo_close_only: Option<Vec<String>>,
    /// No order goes out unless the venue takes the heartbeats.
    pub require_heartbeat: bool,
    /// What one order may cost at most (a last guard behind the risk check).
    pub max_order_usd: Usd,
    pub user_agent: String,
    /// How often an order's record is read.
    pub poll_every_s: i64,
}

/// One approved order of the live book, as the engine priced it.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveOrder {
    pub client_order_id: ClientOrderId,
    /// What it buys, for the alerts (e.g. `NO 18°C on 13 Oct`).
    pub label: String,
    pub token: TokenId,
    pub side: Side,
    pub price: Price,
    pub tick: Price,
    pub shares: Shares,
    pub min_size: Shares,
    pub neg_risk: bool,
    /// The end the strategy gave it.
    pub expires_at: DateTime<Utc>,
}

/// The open weather markets and what the live book holds of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookView {
    /// Every token (YES and NO) of the weather markets not yet settled.
    pub tokens: HashSet<String>,
    /// Shares the live book holds, per token.
    pub held: HashMap<String, Shares>,
    /// What each token is, for the alerts (e.g. `NO 18°C on 13 Oct`).
    pub labels: HashMap<String, String>,
}

impl BookView {
    fn name(&self, token: &str) -> String {
        self.labels
            .get(token)
            .cloned()
            .unwrap_or_else(|| format!("token {token}"))
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Place(LiveOrder),
    /// Cancel every order resting in the book (kill switch, a stop).
    CancelAll(String),
    /// The markets and the live book, as the engine has them.
    Book(BookView),
}

/// What the dashboard and the engine's gate read.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LiveStatus {
    pub ready: bool,
    pub blockers: Vec<String>,
    pub signer: String,
    pub funder: String,
    pub protocol: Option<u32>,
    /// Where the venue sees this server ("NL-NH: API allowed").
    pub location: Option<String>,
    /// The location allows opening positions through the API.
    pub location_ok: bool,
    pub balance: Option<Usd>,
    /// What the resting orders may still cost.
    pub reserved: Usd,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub resting: usize,
    pub unresolved: usize,
}

/// Where the executor's output goes.
pub struct Channels {
    pub commands: mpsc::Receiver<Command>,
    /// Order updates, as engine events.
    pub events: mpsc::Sender<EventEnvelope>,
    pub status: watch::Sender<LiveStatus>,
    /// (level, text): "info", "warning" or "critical".
    pub alerts: mpsc::Sender<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Sent, but the answer proved nothing.
    Unknown {
        sent_at: DateTime<Utc>,
    },
    Resting,
    /// Asked to leave the book.
    Cancelling {
        asked_at: DateTime<Utc>,
        tries: u8,
        why: CancelWhy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelWhy {
    /// Its end came.
    Expiry,
    /// A stop, the kill switch, or the venue no longer ready.
    Pulled,
}

#[derive(Debug, Clone)]
struct Tracked {
    order: LiveOrder,
    order_id: String,
    filled: Shares,
    state: State,
    /// Consecutive reads without a record.
    missing: u8,
    next_read: DateTime<Utc>,
}

impl Tracked {
    fn remaining_cost(&self) -> Usd {
        notional(
            self.order.price,
            self.order.shares - self.filled,
            Rounding::Up,
        )
    }
}

/// The heartbeat chain's latest word.
#[derive(Debug, Clone, Default)]
struct Beat {
    taken_at: Option<DateTime<Utc>>,
    error: Option<String>,
}

struct Exec {
    s: Settings,
    clob: Clob,
    signer: Arc<OrderSigner>,
    auth: Option<Auth>,
    wallet: Option<Wallet>,
    setup_error: Option<String>,
    next_setup: DateTime<Utc>,
    protocol: Option<u32>,
    closed_only: Option<bool>,
    next_venue_check: DateTime<Utc>,
    geo: Option<Geo>,
    geo_error: Option<String>,
    next_geo: DateTime<Utc>,
    balance: Option<Usd>,
    next_balance: DateTime<Utc>,
    balance_reads: u32,
    beat: watch::Receiver<Beat>,
    beat_tx: Option<watch::Sender<Beat>>,
    book: Option<BookView>,
    leftovers_cleared: bool,
    next_leftovers: DateTime<Utc>,
    positions: Option<Result<(), String>>,
    /// The venue's list short of the books, as last said.
    positions_short: Option<String>,
    next_positions: DateTime<Utc>,
    orders: BTreeMap<String, Tracked>,
    was_ready: bool,
    events: mpsc::Sender<EventEnvelope>,
    alerts: mpsc::Sender<(&'static str, String)>,
}

/// Run the executor until `shutdown`: then every resting order is cancelled.
pub async fn run(
    settings: Settings,
    signer: OrderSigner,
    mut ch: Channels,
    mut shutdown: watch::Receiver<bool>,
) {
    let clob = match Clob::new(&settings.clob_url, &settings.user_agent) {
        Ok(c) => c,
        Err(e) => {
            let _ = ch
                .alerts
                .send(("critical", format!("live venue: {e}")))
                .await;
            return;
        }
    };
    let (beat_tx, beat_rx) = watch::channel(Beat::default());
    let now = Utc::now();
    let mut x = Exec {
        s: settings,
        clob,
        signer: Arc::new(signer),
        auth: None,
        wallet: None,
        setup_error: None,
        next_setup: now,
        protocol: None,
        closed_only: None,
        next_venue_check: now,
        geo: None,
        geo_error: None,
        next_geo: now,
        balance: None,
        next_balance: now,
        balance_reads: 0,
        beat: beat_rx,
        beat_tx: Some(beat_tx),
        book: None,
        leftovers_cleared: false,
        next_leftovers: now,
        positions: None,
        positions_short: None,
        next_positions: now,
        orders: BTreeMap::new(),
        was_ready: false,
        events: ch.events.clone(),
        alerts: ch.alerts.clone(),
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            c = ch.commands.recv() => match c {
                Some(Command::Place(o)) => x.place(o).await,
                Some(Command::CancelAll(why)) => x.pull_all(&why).await,
                Some(Command::Book(b)) => x.book = Some(b),
                None => break,
            },
            _ = tick.tick() => {}
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { break; },
        }
        x.housekeeping().await;
        // the heartbeat chain runs on its own task once the credentials exist
        if heartbeat.is_none()
            && let (Some(a), Some(tx)) = (x.auth.clone(), x.beat_tx.take())
        {
            let clob = x.clob.clone();
            let stop = shutdown.clone();
            heartbeat = Some(tokio::spawn(heartbeat_loop(clob, a, tx, stop)));
        }
        let status = x.status();
        if status.ready != x.was_ready {
            x.was_ready = status.ready;
            if status.ready {
                x.alert("info", "live venue ready: strategy G may place real orders")
                    .await;
            } else {
                x.alert(
                    "warning",
                    format!("live venue not ready: {}", status.blockers.join("; ")),
                )
                .await;
            }
        }
        if !status.ready && x.orders.values().any(|t| t.state == State::Resting) {
            let why = format!("live venue not ready: {}", status.blockers.join("; "));
            x.pull_all(&why).await;
        }
        ch.status.send_if_modified(|s| {
            if *s == status {
                false
            } else {
                *s = status;
                true
            }
        });
    }
    // Shutdown: nothing of ours rests in the book of a stopped process.
    let resting = x
        .orders
        .values()
        .filter(|t| t.state == State::Resting)
        .count();
    if resting > 0 {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            x.pull_all("the weather machine is stopping"),
        )
        .await;
    }
    if let Some(h) = heartbeat {
        h.abort();
    }
}

impl Exec {
    async fn alert(&self, level: &'static str, text: impl Into<String>) {
        let text = text.into();
        match level {
            "critical" => tracing::error!("{text}"),
            "warning" => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        let _ = self.alerts.send((level, text)).await;
    }

    fn reserved(&self) -> Usd {
        self.orders.values().map(Tracked::remaining_cost).sum()
    }

    /// Why no new order may go out now (empty: it may).
    fn blockers(&self) -> Vec<String> {
        let mut b = Vec::new();
        let now = Utc::now();
        if self.auth.is_none() {
            b.push(match &self.setup_error {
                Some(e) => format!("API credentials: {e}"),
                None => "API credentials not derived yet".to_owned(),
            });
        }
        match self.protocol {
            None => b.push("the CLOB's protocol version not read yet".into()),
            Some(v) if Protocol::from_version(v).is_none() => {
                b.push(format!(
                    "the CLOB runs protocol {v}, which this build does not sign"
                ));
            }
            Some(_) => {}
        }
        if self.closed_only == Some(true) {
            b.push("the account is in closed-only mode at the venue".into());
        }
        match (&self.geo, &self.geo_error) {
            (Some(g), _) if g.status != GeoStatus::Allowed => b.push(g.describe()),
            (Some(_), _) => {}
            (None, Some(e)) => b.push(format!("location check: {e}")),
            (None, None) => b.push("location not checked yet".into()),
        }
        if self.s.require_heartbeat {
            let beat = self.beat.borrow();
            let fresh = beat
                .taken_at
                .is_some_and(|t| now - t <= Duration::seconds(HEARTBEAT_FRESH_S));
            if !fresh {
                b.push(match &beat.error {
                    Some(e) => format!("heartbeat not taken by the venue: {e}"),
                    None => "no heartbeat taken by the venue yet".into(),
                });
            }
        }
        if self.book.is_none() {
            b.push("waiting for the weather markets".into());
        } else if !self.leftovers_cleared {
            b.push("orders of an earlier run still open at the venue".into());
        }
        match &self.positions {
            None if self.book.is_some() => b.push("positions not compared yet".into()),
            Some(Err(e)) => b.push(e.clone()),
            _ => {}
        }
        let unknown: Vec<&str> = self
            .orders
            .values()
            .filter(|t| matches!(t.state, State::Unknown { .. }))
            .map(|t| t.order_id.as_str())
            .collect();
        if !unknown.is_empty() {
            b.push(format!("order(s) of unknown fate: {}", unknown.join(", ")));
        }
        if self.balance.is_none() {
            b.push("collateral not read yet".into());
        }
        b
    }

    fn status(&self) -> LiveStatus {
        let blockers = self.blockers();
        LiveStatus {
            ready: blockers.is_empty(),
            blockers,
            signer: self.signer.address.clone(),
            funder: self.s.funder.clone(),
            protocol: self.protocol,
            location: self
                .geo
                .as_ref()
                .map(Geo::describe)
                .or_else(|| self.geo_error.clone()),
            location_ok: self
                .geo
                .as_ref()
                .is_some_and(|g| g.status == GeoStatus::Allowed),
            balance: self.balance,
            reserved: self.reserved(),
            heartbeat_at: self.beat.borrow().taken_at,
            resting: self
                .orders
                .values()
                .filter(|t| t.state == State::Resting)
                .count(),
            unresolved: self
                .orders
                .values()
                .filter(|t| matches!(t.state, State::Unknown { .. }))
                .count(),
        }
    }

    /// Every periodic read and the orders' follow-up.
    async fn housekeeping(&mut self) {
        let now = Utc::now();
        if self.auth.is_none() && now >= self.next_setup {
            self.setup().await;
        }
        if now >= self.next_venue_check {
            self.next_venue_check = now + Duration::minutes(10);
            match self.clob.version().await {
                Ok(v) => {
                    if self.protocol != Some(v) {
                        self.alert("info", format!("the CLOB runs protocol version {v}"))
                            .await;
                    }
                    self.protocol = Some(v);
                }
                Err(e) => {
                    self.next_venue_check = now + Duration::seconds(30);
                    tracing::warn!("CLOB version: {e}");
                }
            }
            if let Some(a) = &self.auth {
                match self.clob.closed_only(a).await {
                    Ok(c) => self.closed_only = Some(c),
                    Err(e) => tracing::warn!("closed-only status: {e}"),
                }
            }
        }
        if now >= self.next_geo {
            self.next_geo = now + Duration::minutes(10);
            match rest::geoblock(
                self.clob.http(),
                &self.s.geoblock_url,
                self.s.geo_close_only.as_deref(),
            )
            .await
            {
                Ok(g) => {
                    if self.geo.as_ref() != Some(&g) {
                        let level = if g.status == GeoStatus::Allowed {
                            "info"
                        } else {
                            "critical"
                        };
                        self.alert(
                            level,
                            format!("the venue sees this server in {}", g.describe()),
                        )
                        .await;
                    }
                    self.geo = Some(g);
                    self.geo_error = None;
                }
                Err(e) => {
                    self.next_geo = now + Duration::minutes(1);
                    self.geo = None;
                    self.geo_error = Some(e);
                }
            }
        }
        if let Some(a) = self.auth.clone()
            && now >= self.next_balance
        {
            self.next_balance = now + Duration::seconds(60);
            let refresh = self.balance_reads.is_multiple_of(5);
            self.balance_reads = self.balance_reads.wrapping_add(1);
            match self.clob.balance(&a, self.s.signature_type, refresh).await {
                Ok(b) => self.balance = Some(b),
                Err(e) => tracing::warn!("collateral read: {e}"),
            }
        }
        if self.auth.is_some()
            && self.book.is_some()
            && !self.leftovers_cleared
            && now >= self.next_leftovers
        {
            self.next_leftovers = now + Duration::seconds(5);
            self.clear_leftovers().await;
        }
        if self.book.is_some()
            && self.leftovers_cleared
            && self.positions.as_ref().is_none_or(Result::is_err)
            && now >= self.next_positions
        {
            self.next_positions = now + Duration::minutes(2);
            self.compare_positions().await;
        }
        self.follow_orders().await;
    }

    async fn setup(&mut self) {
        let now = Utc::now();
        self.next_setup = now + Duration::seconds(30);
        let wallet = Wallet {
            maker: self.s.funder.clone(),
            signature_type: self.s.signature_type,
            owner: String::new(),
        };
        if let Err(e) = wallet.check(&self.signer) {
            self.setup_error = Some(e.to_string());
            self.next_setup = now + Duration::minutes(10);
            self.alert(
                "critical",
                format!("live venue: the wallet settings are wrong: {e}"),
            )
            .await;
            return;
        }
        match self
            .clob
            .credentials(&self.signer, self.s.api_key_nonce)
            .await
        {
            Ok(creds) => {
                self.wallet = Some(Wallet {
                    owner: creds.api_key.clone(),
                    ..wallet
                });
                self.auth = Some(Auth {
                    signer: Arc::clone(&self.signer),
                    creds,
                });
                self.setup_error = None;
                self.next_venue_check = now;
                self.next_balance = now;
                self.alert(
                    "info",
                    format!(
                        "live venue: API credentials derived for signer {} (funder {}, key nonce {})",
                        self.signer.address, self.s.funder, self.s.api_key_nonce
                    ),
                )
                .await;
            }
            Err(e) => {
                if self.setup_error.as_ref() != Some(&e) {
                    self.alert("warning", format!("live venue: {e}")).await;
                }
                self.setup_error = Some(e);
            }
        }
    }

    /// Cancel what an earlier run left resting on the weather markets, and
    /// leave every other order of the account alone.
    async fn clear_leftovers(&mut self) {
        let (Some(a), Some(book)) = (self.auth.clone(), self.book.clone()) else {
            return;
        };
        let open = match self.clob.open_orders(&a).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("open orders: {e}");
                return;
            }
        };
        let ours: Vec<_> = open
            .iter()
            .filter(|o| book.tokens.contains(&o.asset_id))
            .filter(|o| !self.orders.contains_key(&o.order_id))
            .collect();
        if ours.is_empty() {
            self.leftovers_cleared = true;
            let others = open.len();
            if others > 0 {
                self.alert(
                    "info",
                    format!("{others} open order(s) of the account on other markets: left alone"),
                )
                .await;
            }
            return;
        }
        for o in ours {
            let ans = self.clob.cancel(&a, &o.order_id).await;
            let verdict = answer::cancel_answer(ans.status, &ans.body, &o.order_id);
            let what = self
                .book
                .as_ref()
                .map_or_else(|| format!("token {}", o.asset_id), |b| b.name(&o.asset_id));
            self.alert(
                "warning",
                format!(
                    "an earlier run's order {} on {what} was still open: cancel {:?}",
                    o.order_id, verdict
                ),
            )
            .await;
        }
    }

    /// The venue's positions in the open weather markets against the books.
    /// Shares the venue holds beyond the books are exposure no book tracks
    /// (an order of another run, or one by hand): no new order until they
    /// are gone. Fewer shares at the venue than the books add no exposure,
    /// and the venue's list can leave positions out (13 October 2026, a
    /// restart at 22:58 UTC: neither of the two live positions, of the
    /// market that had ended at 22:00, was on the one page of 500 then read,
    /// the most shares first), so that is said and does not block.
    async fn compare_positions(&mut self) {
        let Some(book) = self.book.clone() else {
            return;
        };
        let venue = match rest::positions(self.clob.http(), &self.s.data_url, &self.s.funder).await
        {
            Ok(v) => v,
            Err(e) => {
                self.next_positions = Utc::now() + Duration::seconds(30);
                tracing::warn!("positions: {e}");
                return;
            }
        };
        let tolerance = Shares::from_micros(10_000);
        let mut more = Vec::new();
        let mut less = Vec::new();
        let mut seen = HashSet::new();
        for (token, shares) in &venue {
            if !book.tokens.contains(token) {
                continue;
            }
            seen.insert(token.clone());
            let held = book.held.get(token).copied().unwrap_or(Shares::ZERO);
            let line = format!("{}: venue {shares}, books {held}", book.name(token));
            if *shares > held + tolerance {
                more.push(line);
            } else if held > *shares + tolerance {
                less.push(line);
            }
        }
        for (token, held) in &book.held {
            if !seen.contains(token) && book.tokens.contains(token) && *held > tolerance {
                less.push(format!("{}: venue 0, books {held}", book.name(token)));
            }
        }
        let short = (!less.is_empty()).then(|| less.join("; "));
        if short != self.positions_short {
            if let Some(s) = &short {
                self.alert(
                    "warning",
                    format!(
                        "the venue lists fewer shares than the books (its list can leave positions out; not blocking): {s}"
                    ),
                )
                .await;
            }
            self.positions_short = short;
        }
        let verdict = if more.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the venue holds shares the books do not (an order of another run, or one by hand; checked again every 2 minutes): {}",
                more.join("; ")
            ))
        };
        if verdict != self.positions.clone().unwrap_or(Ok(())) || self.positions.is_none() {
            match &verdict {
                Ok(()) => {
                    let text = if self.positions_short.is_none() {
                        "live positions at the venue match the books"
                    } else {
                        "the venue holds no shares beyond the books"
                    };
                    self.alert("info", text).await
                }
                Err(e) => self.alert("critical", e.clone()).await,
            }
        }
        self.positions = Some(verdict);
    }

    async fn place(&mut self, o: LiveOrder) {
        let blockers = self.blockers();
        if !blockers.is_empty() {
            let why = format!("live venue not ready: {}", blockers.join("; "));
            self.emit(
                &o,
                None,
                OrderStatus::Rejected,
                Shares::ZERO,
                Some(why),
                None,
            )
            .await;
            return;
        }
        let (Some(a), Some(wallet), Some(protocol)) = (
            self.auth.clone(),
            self.wallet.clone(),
            self.protocol.and_then(Protocol::from_version),
        ) else {
            return;
        };
        let now = Utc::now();
        let cost = notional(o.price, o.shares, Rounding::Up);
        let free = self.balance.unwrap_or(Usd::ZERO) - self.reserved();
        if cost > free {
            let why = format!(
                "collateral {} with {} reserved by resting orders: under the order's {cost}",
                self.balance.unwrap_or(Usd::ZERO),
                self.reserved()
            );
            self.emit(
                &o,
                None,
                OrderStatus::Rejected,
                Shares::ZERO,
                Some(why),
                None,
            )
            .await;
            return;
        }
        let spec = OrderSpec {
            token_id: o.token.to_string(),
            side: o.side,
            price: o.price,
            tick: o.tick,
            shares: o.shares,
            min_size: o.min_size,
            neg_risk: o.neg_risk,
            expiration_s: (o.expires_at + Duration::seconds(order::GTD_SECURITY_S)).timestamp(),
            cap: self.s.max_order_usd,
        };
        let salt = order::salt_for(o.client_order_id.as_str());
        let ts = u64::try_from(now.timestamp_millis()).unwrap_or(0);
        let signed = match order::build(
            &self.signer,
            &wallet,
            protocol,
            &spec,
            salt,
            ts,
            now.timestamp(),
        ) {
            Ok(s) => s,
            Err(e) => {
                let why = format!("not built: {e}");
                self.emit(
                    &o,
                    None,
                    OrderStatus::Rejected,
                    Shares::ZERO,
                    Some(why),
                    None,
                )
                .await;
                return;
            }
        };
        let ans = self.clob.post_order(&a, &signed.body).await;
        let outcome = answer::classify_post(ans.status, &ans.body, ans.transport.clone());
        let label = format!(
            "{} BUY {} × {} @ {} ({}) until {} UTC",
            o.client_order_id,
            o.label,
            o.shares,
            o.price,
            signed.cost,
            o.expires_at.format("%H:%M")
        );
        match outcome {
            PostOutcome::Resting { order_id } => {
                if let Some(id) = &order_id
                    && !id.eq_ignore_ascii_case(&signed.order_id)
                {
                    self.alert(
                        "warning",
                        format!(
                            "the venue's order id {id} differs from the EIP-712 digest {}",
                            signed.order_id
                        ),
                    )
                    .await;
                }
                self.alert(
                    "info",
                    format!("LIVE order placed: {label} → {}", signed.order_id),
                )
                .await;
                let t = Tracked {
                    order: o.clone(),
                    order_id: signed.order_id.clone(),
                    filled: Shares::ZERO,
                    state: State::Resting,
                    missing: 0,
                    next_read: now + Duration::seconds(2),
                };
                self.emit(
                    &o,
                    Some(&t.order_id),
                    OrderStatus::Live,
                    Shares::ZERO,
                    None,
                    None,
                )
                .await;
                self.orders.insert(signed.order_id, t);
            }
            PostOutcome::NotSent(why) => {
                self.emit(
                    &o,
                    None,
                    OrderStatus::Rejected,
                    Shares::ZERO,
                    Some(format!("not sent: {why}")),
                    None,
                )
                .await;
            }
            PostOutcome::Refused(why) => {
                self.alert(
                    "warning",
                    format!("LIVE order refused by the venue: {label}: {why}"),
                )
                .await;
                self.emit(
                    &o,
                    None,
                    OrderStatus::Rejected,
                    Shares::ZERO,
                    Some(why),
                    None,
                )
                .await;
            }
            PostOutcome::Unknown(why) => {
                self.alert(
                    "critical",
                    format!(
                        "LIVE order of unknown fate: {label} ({why}); its record {} decides, no new order until then",
                        signed.order_id
                    ),
                )
                .await;
                self.orders.insert(
                    signed.order_id.clone(),
                    Tracked {
                        order: o,
                        order_id: signed.order_id,
                        filled: Shares::ZERO,
                        state: State::Unknown { sent_at: now },
                        missing: 0,
                        next_read: now + Duration::seconds(1),
                    },
                );
            }
        }
    }

    /// Pull every resting order (and keep reading their records).
    async fn pull_all(&mut self, why: &str) {
        let ids: Vec<String> = self
            .orders
            .values()
            .filter(|t| t.state == State::Resting)
            .map(|t| t.order_id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        self.alert(
            "warning",
            format!("pulling {} live order(s): {why}", ids.len()),
        )
        .await;
        for id in ids {
            self.cancel(&id, CancelWhy::Pulled).await;
        }
    }

    async fn cancel(&mut self, id: &str, why: CancelWhy) {
        let Some(a) = self.auth.clone() else {
            return;
        };
        let ans = self.clob.cancel(&a, id).await;
        let verdict = answer::cancel_answer(ans.status, &ans.body, id);
        let now = Utc::now();
        if let Some(t) = self.orders.get_mut(id) {
            let tries = match &t.state {
                State::Cancelling { tries, .. } => tries.saturating_add(1),
                _ => 1,
            };
            t.state = State::Cancelling {
                asked_at: now,
                tries,
                why,
            };
            // read the record at once: it says what the order bought until now
            t.next_read = now;
        }
        if let CancelAnswer::Unknown(e) = verdict {
            tracing::warn!(order = %id, "cancel not confirmed: {e} (asked again)");
        }
    }

    /// Read the records that are due and act on what they say.
    async fn follow_orders(&mut self) {
        let Some(a) = self.auth.clone() else {
            return;
        };
        let now = Utc::now();
        let due: Vec<String> = self
            .orders
            .values()
            .filter(|t| {
                t.next_read <= now || (t.state == State::Resting && t.order.expires_at <= now)
            })
            .map(|t| t.order_id.clone())
            .collect();
        for id in due {
            // its end came: cancel it (the venue ends it there too)
            if let Some(t) = self.orders.get(&id)
                && t.state == State::Resting
                && t.order.expires_at <= now
            {
                self.cancel(&id, CancelWhy::Expiry).await;
            }
            let record = self.clob.record(&a, &id).await;
            let Some(t) = self.orders.get_mut(&id) else {
                continue;
            };
            t.next_read = now + Duration::seconds(self.s.poll_every_s.max(1));
            match record {
                Err(e) => tracing::warn!(order = %id, "order record: {e}"),
                Ok(None) => self.no_record(&id, now).await,
                Ok(Some(r)) => self.on_record(&id, r, now).await,
            }
            // a cancel not confirmed by an end is asked again, a few times
            if let Some(t) = self.orders.get(&id)
                && let State::Cancelling {
                    asked_at,
                    tries,
                    why,
                } = t.state.clone()
                && now - asked_at >= Duration::seconds(3)
                && tries < 6
            {
                self.cancel(&id, why).await;
            }
        }
    }

    async fn no_record(&mut self, id: &str, now: DateTime<Utc>) {
        let Some(t) = self.orders.get_mut(id) else {
            return;
        };
        t.missing = t.missing.saturating_add(1);
        match t.state.clone() {
            State::Unknown { sent_at } if now - sent_at >= Duration::seconds(ABSENT_AFTER_S) => {
                let t = self.orders.remove(id);
                if let Some(t) = t {
                    self.alert(
                        "warning",
                        format!("LIVE order {id}: no record at the venue {ABSENT_AFTER_S} s after it was sent: never placed"),
                    )
                    .await;
                    let o = t.order.clone();
                    self.emit(
                        &o,
                        Some(id),
                        OrderStatus::Rejected,
                        Shares::ZERO,
                        Some("never placed (no record at the venue)".into()),
                        None,
                    )
                    .await;
                }
            }
            State::Resting | State::Cancelling { .. } if t.missing >= 3 => {
                // a record that is gone: the order is out of the book with what it had bought
                if let Some(t) = self.orders.remove(id) {
                    let status = if t.order.expires_at <= now {
                        OrderStatus::Expired
                    } else {
                        OrderStatus::Canceled
                    };
                    let o = t.order.clone();
                    self.emit(
                        &o,
                        Some(id),
                        status,
                        t.filled,
                        Some("its record is gone at the venue".into()),
                        None,
                    )
                    .await;
                }
            }
            _ => {}
        }
    }

    async fn on_record(&mut self, id: &str, r: answer::OrderRecord, now: DateTime<Utc>) {
        let Some(t) = self.orders.get_mut(id) else {
            return;
        };
        t.missing = 0;
        let was_unknown = matches!(t.state, State::Unknown { .. });
        if was_unknown {
            t.state = State::Resting;
        }
        let o = t.order.clone();
        let mut matched = r.size_matched;
        if matched > o.shares {
            let excess = matched;
            matched = o.shares;
            let text = format!(
                "LIVE order {id}: the venue reports {excess} shares matched of an order for {}",
                o.shares
            );
            self.alert("critical", text).await;
        }
        let Some(t) = self.orders.get_mut(id) else {
            return;
        };
        let before = t.filled;
        if was_unknown {
            self.alert(
                "info",
                format!("LIVE order {id} is at the venue after all ({})", r.status),
            )
            .await;
            self.emit(&o, Some(id), OrderStatus::Live, before, None, None)
                .await;
        }
        let ended = r.ended();
        if matched > before {
            let delta = matched - before;
            if let Some(t) = self.orders.get_mut(id) {
                t.filled = matched;
            }
            let fill = Fill {
                client_order_id: o.client_order_id.clone(),
                token: o.token.clone(),
                side: o.side,
                // a maker is filled at its own price, without a fee
                price: o.price,
                shares: delta,
                fee: Usd::ZERO,
                liquidity: Liquidity::Maker,
                ts: now,
            };
            let status = if matched >= o.shares {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            };
            self.alert(
                "info",
                format!(
                    "LIVE fill: {} {delta} shares of {} @ {} (order {id})",
                    o.client_order_id, o.label, o.price
                ),
            )
            .await;
            self.emit(&o, Some(id), status, matched, None, Some(fill))
                .await;
            if status == OrderStatus::Filled {
                self.orders.remove(id);
                return;
            }
        }
        if ended {
            let Some(t) = self.orders.remove(id) else {
                return;
            };
            let expired = matches!(
                t.state,
                State::Cancelling {
                    why: CancelWhy::Expiry,
                    ..
                }
            ) || t.order.expires_at <= now;
            let status = if t.filled >= t.order.shares {
                OrderStatus::Filled
            } else if expired {
                OrderStatus::Expired
            } else {
                OrderStatus::Canceled
            };
            let reason = Some(format!("venue: {}", r.status.to_ascii_lowercase()));
            self.emit(&o, Some(id), status, t.filled, reason, None)
                .await;
        }
    }

    async fn emit(
        &self,
        o: &LiveOrder,
        venue_id: Option<&str>,
        status: OrderStatus,
        filled: Shares,
        reason: Option<String>,
        fill: Option<Fill>,
    ) {
        let now = Utc::now();
        let update = OrderUpdate {
            client_order_id: o.client_order_id.clone(),
            venue_order_id: venue_id.map(str::to_owned),
            status,
            filled,
            avg_price: (filled.micros() > 0).then_some(o.price),
            fee_paid: Usd::ZERO,
            reason,
            ts: now,
        };
        let env = EventEnvelope::new(
            now,
            EventSource::Live,
            WeatherMachineEvent::OrderUpdate(OrderUpdateEvent { update, fill }),
        );
        if self.events.send(env).await.is_err() {
            tracing::error!(order = %o.client_order_id, "the engine no longer takes live order updates");
        }
    }
}

/// The venue's dead-man's switch: a heartbeat every five seconds, each with
/// the id the last was answered with (the first without one). When they
/// stop the venue cancels every open order of these credentials within
/// some ten to fifteen seconds. A heartbeat not taken is sent again a second
/// later: one miss would leave ten seconds between two.
async fn heartbeat_loop(
    clob: Clob,
    a: Auth,
    tx: watch::Sender<Beat>,
    mut stop: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_EVERY_S));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (mut id, mut failed): (Option<String>, u32) = (None, 0);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            r = stop.changed() => if r.is_err() || *stop.borrow() { return; },
        }
        // a chain the venue keeps refusing without naming the id it expects starts anew
        if failed > 0 && failed.is_multiple_of(3) {
            id = None;
        }
        let mut answer = Err((None, String::new()));
        for again in [false, true] {
            if again {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            answer = clob.heartbeat(&a, id.as_deref()).await;
            if let Err((Some(expected), _)) = &answer {
                id = Some(expected.clone());
                answer = clob.heartbeat(&a, id.as_deref()).await;
            }
            if answer.is_ok() {
                break;
            }
        }
        match answer {
            Ok(next) => {
                if failed > 0 || tx.borrow().taken_at.is_none() {
                    tracing::info!(
                        "the venue takes the heartbeats: it cancels our open orders when they stop"
                    );
                }
                id = Some(next);
                failed = 0;
                tx.send_replace(Beat {
                    taken_at: Some(Utc::now()),
                    error: None,
                });
            }
            Err((_, why)) => {
                failed += 1;
                if failed == 1 || failed.is_multiple_of(12) {
                    tracing::warn!(missed = failed, "heartbeat not taken by the venue: {why}");
                }
                tx.send_modify(|b| b.error = Some(why));
            }
        }
    }
}
