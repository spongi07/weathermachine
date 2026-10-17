//! Push messages through Pushover: a settled market's result for each
//! strategy that held it, a live order that ended having bought shares, and
//! the disk filling up (whatever strategies the messages follow).
//!
//! `WM_PUSHOVER_TOKEN` (the API token of an application created at
//! pushover.net) and `WM_PUSHOVER_USER` (the user key) switch it on. Both
//! are secrets: read once by name, never printed, and sent nowhere but to
//! Pushover's own API, in the body of the request (https only; a redirect is
//! never followed, since it would post the keys again to wherever it points,
//! and the endpoint is not a setting). `WM_PUSHOVER_STRATEGIES` says whose
//! results: `live` (the default: the strategies that trade real money,
//! G_live), `all`, or strategy ids or letters separated by commas (`GL,G`
//! adds paper G, the twin to compare against). A bought message is for a
//! live strategy only.
//!
//! The engine loop never waits for it: messages go to a sender task through
//! a bounded queue, and one that cannot be queued is counted as dropped (the
//! alerts and the database have the trade). A message Pushover refuses (a
//! wrong token or user key, a used-up quota, a 2xx whose `status` is not 1)
//! is not repeated, as its API asks: notifications are then off until a
//! restart, and an alert and the dashboard say so. One that got no answer, a 5xx
//! or a redirect is tried twice more (5 and 20 s later), and no request
//! follows a failed one sooner than 5 s. An answer lost on its way back
//! cannot be told from a request that never arrived, so a retry can deliver
//! a message twice: a second copy is preferred to none.

use crate::paper_report::{price_text, usd};
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use wm_backtest::{SessionOutput, Settlement};
use wm_core::event::WeatherMachineEvent;
use wm_core::market::{OutcomeSide, TemperatureBucket};
use wm_core::trading::OrderStatus;
use wm_core::units::{Rounding, Shares, Usd, notional};
use wm_engine::{Engine, SettledHolding};
use wm_execution::OrderRecord;

/// Pushover's message endpoint. Not a setting: the token and the user key
/// go to this host and no other.
pub const PUSHOVER_URL: &str = "https://api.pushover.net/1/messages.json";

/// Messages waiting to be sent, at most (a settled market is a message a
/// day per strategy; the three tries of one hold the queue 55 s at most).
const QUEUE: usize = 64;

/// Seconds before the second and the third try of a message that could not
/// be delivered.
const RETRY_AFTER_S: [u64; 2] = [5, 20];

/// No request sooner than this after one that failed, whichever message it
/// is for (Pushover: "no sooner than 5 seconds from your last request").
const AFTER_A_FAILURE: Duration = Duration::from_secs(5);

/// One request's time limit.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Pushover's limits, in characters.
const TITLE_MAX: usize = 250;
const MESSAGE_MAX: usize = 1024;

/// A token and a user key: 30 letters and digits each.
const KEY_LEN: usize = 30;

/// A key read from the environment: printed as `***`, whatever the format.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Whose results are pushed (`WM_PUSHOVER_STRATEGIES`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Who {
    /// the strategies that trade real money (the default)
    Live,
    All,
    /// engine ids
    Ids(BTreeSet<String>),
}

impl Who {
    /// `known`: every strategy's engine id and letter. An error never
    /// repeats the value: a key pasted into the wrong field would be printed
    /// with it.
    pub fn parse(raw: &str, known: &[(String, String)]) -> Result<Who, String> {
        const SHAPE: &str = "live | all | strategy ids or letters separated by commas";
        let v = raw.trim();
        match v.to_ascii_lowercase().as_str() {
            "" | "live" => Ok(Who::Live),
            "all" => Ok(Who::All),
            _ => {
                let names: Vec<&str> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if names.is_empty() {
                    return Err(format!("it names no strategy ({SHAPE})"));
                }
                if names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case("live") || n.eq_ignore_ascii_case("all"))
                {
                    return Err(format!("live and all stand alone, not in a list ({SHAPE})"));
                }
                let mut ids = BTreeSet::new();
                for n in names {
                    match known.iter().find(|(id, letter)| {
                        id.eq_ignore_ascii_case(n) || letter.eq_ignore_ascii_case(n)
                    }) {
                        Some((id, _)) => {
                            ids.insert(id.clone());
                        }
                        None => {
                            return Err(format!(
                                "one of its names, {} characters long, is no strategy of this build ({SHAPE})",
                                n.chars().count()
                            ));
                        }
                    }
                }
                Ok(Who::Ids(ids))
            }
        }
    }

    /// A result of `strategy`, which trades real money if `live`.
    pub fn wants(&self, strategy: &str, live: bool) -> bool {
        match self {
            Who::Live => live,
            Who::All => true,
            Who::Ids(ids) => ids.contains(strategy),
        }
    }
}

impl fmt::Display for Who {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Who::Live => f.write_str("the strategies that trade real money"),
            Who::All => f.write_str("every strategy"),
            Who::Ids(ids) => f.write_str(
                &ids.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }
}

/// The Pushover account messages go to. `{:?}` prints no secret.
#[derive(Clone, Debug)]
pub struct Pushover {
    token: Secret,
    user: Secret,
    pub who: Who,
}

impl Pushover {
    /// From the environment's three values: `Ok(None)` when neither key is
    /// set. An error says what is wrong without the value.
    pub fn from_env(
        token: Option<&str>,
        user: Option<&str>,
        strategies: Option<&str>,
        known: &[(String, String)],
    ) -> Result<Option<Pushover>, String> {
        let (token, user) = match (token, user) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err("WM_PUSHOVER_TOKEN is set but WM_PUSHOVER_USER is not".into());
            }
            (None, Some(_)) => {
                return Err("WM_PUSHOVER_USER is set but WM_PUSHOVER_TOKEN is not".into());
            }
            (Some(t), Some(u)) => (t.trim(), u.trim()),
        };
        for (name, v) in [("WM_PUSHOVER_TOKEN", token), ("WM_PUSHOVER_USER", user)] {
            if v.chars().count() != KEY_LEN || !v.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(format!(
                    "{name} is no Pushover key: {} characters, where a key has {KEY_LEN} letters and digits",
                    v.chars().count()
                ));
            }
        }
        let who = Who::parse(strategies.unwrap_or(""), known)
            .map_err(|e| format!("WM_PUSHOVER_STRATEGIES: {e}"))?;
        Ok(Some(Pushover {
            token: Secret(token.to_owned()),
            user: Secret(user.to_owned()),
            who,
        }))
    }
}

/// One push message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub title: String,
    pub message: String,
    /// When it happened, unix s (the message shows this time, not the
    /// delivery's).
    pub at_s: i64,
}

/// What the sender did so far.
#[derive(Default)]
pub struct NotifyStats {
    sent: AtomicU64,
    /// given up on: refused, or not delivered in three tries
    failed: AtomicU64,
    /// never tried: the queue was full, or notifications were off
    dropped: AtomicU64,
    /// Pushover refused a message: nothing more is sent until a restart
    off: AtomicBool,
    /// why the last message given up on was, and when
    last_error: Mutex<Option<(String, DateTime<Utc>)>>,
}

/// The notifications' state for the dashboard: `ok` is false while they are
/// off for a problem (the runtime alerts it once).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyState {
    pub ok: bool,
    pub detail: String,
}

impl NotifyStats {
    fn gave_up(&self, why: String) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((why, Utc::now()));
    }

    pub fn state(&self, who: &Who) -> NotifyState {
        let last = self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if self.off.load(Ordering::Acquire) {
            return NotifyState {
                ok: false,
                detail: format!(
                    "off until a restart: Pushover refused a message ({})",
                    last.map_or_else(|| "no reason given".to_owned(), |(why, _)| why)
                ),
            };
        }
        let mut detail = format!(
            "Pushover, for {who} · {} sent",
            self.sent.load(Ordering::Relaxed)
        );
        let failed = self.failed.load(Ordering::Relaxed);
        if failed > 0 {
            detail.push_str(&format!(", {failed} not delivered"));
            if let Some((why, at)) = last {
                detail.push_str(&format!(
                    " (last at {}: {why})",
                    at.format("%d %b %H:%M UTC")
                ));
            }
        }
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            detail.push_str(&format!(", {dropped} dropped"));
        }
        NotifyState { ok: true, detail }
    }
}

/// The engine loop's end: it queues a message and never waits.
#[derive(Clone)]
pub struct Notifier {
    who: Who,
    tx: mpsc::Sender<Note>,
    pub stats: Arc<NotifyStats>,
}

impl Notifier {
    pub fn new(who: Who) -> (Notifier, mpsc::Receiver<Note>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        (
            Notifier {
                who,
                tx,
                stats: Arc::new(NotifyStats::default()),
            },
            rx,
        )
    }

    pub fn who(&self) -> &Who {
        &self.who
    }

    /// Queue `note`; a full queue drops it and says so.
    pub fn push(&self, note: Note) {
        if self.tx.try_send(note).is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                "notifications: the queue is full or closed, a message was dropped (the alerts have it)"
            );
        }
    }
}

// -- The messages ---------------------------------------------------------------

/// The live twin's state for its messages.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveContext {
    /// Its realized result since going live, earlier runs' settled days in.
    pub since_live: Option<Usd>,
    /// The loss that stops it.
    pub stop: Option<Usd>,
    /// The venue's collateral, as last read.
    pub balance: Option<Usd>,
    /// What its open positions cost, and the most they may.
    pub open: Usd,
    pub max_open: Option<Usd>,
}

fn signed_usd(v: Usd) -> String {
    let x = v.as_f64();
    if x >= 0.005 {
        format!("+{}", usd(x))
    } else {
        usd(x)
    }
}

fn shares_text(s: Shares) -> String {
    let x = s.as_f64();
    if (x - x.round()).abs() < 1e-9 {
        format!("{x:.0}")
    } else {
        format!("{x:.2}")
    }
}

/// When a position on `side` of `bucket` pays out.
fn pays_if(side: OutcomeSide, b: &TemperatureBucket) -> String {
    let u = b.unit.symbol();
    match (side, b.lower, b.upper) {
        (OutcomeSide::Yes, ..) => format!("the high is {}", b.label()),
        (OutcomeSide::No, Some(lo), Some(hi)) if lo == hi => format!("the high is not {lo}{u}"),
        (OutcomeSide::No, Some(lo), Some(hi)) => format!("the high is outside {lo}-{hi}{u}"),
        (OutcomeSide::No, Some(lo), None) => format!("the high stays under {lo}{u}"),
        (OutcomeSide::No, None, Some(hi)) => format!("the high ends above {hi}{u}"),
        (OutcomeSide::No, None, None) => "the market resolves NO".to_owned(),
    }
}

fn clip(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        text
    } else {
        let mut t: String = text.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn note(title: String, message: String, at: DateTime<Utc>) -> Note {
    Note {
        title: clip(title, TITLE_MAX),
        message: clip(message, MESSAGE_MAX),
        at_s: at.timestamp(),
    }
}

/// A message about the machine rather than a strategy (the disk filling up):
/// sent whoever's results the messages follow.
pub fn system_note(title: String, message: String, at: DateTime<Utc>) -> Note {
    note(title, message, at)
}

fn day_text(d: NaiveDate) -> String {
    d.format("%-d %b").to_string()
}

/// The messages of one settled market, one per strategy `who` wants that
/// held a position in it (in strategy order), each with its key.
pub fn settled_notes(
    s: &Settlement,
    place: &str,
    who: &Who,
    live: &LiveContext,
) -> Vec<(String, Note)> {
    let mut by_strategy: BTreeMap<(&str, bool), Vec<&SettledHolding>> = BTreeMap::new();
    for h in &s.positions {
        by_strategy
            .entry((h.strategy.as_str(), h.live))
            .or_default()
            .push(h);
    }
    let mut notes = Vec::new();
    for ((strategy, is_live), held) in by_strategy {
        if !who.wants(strategy, is_live) {
            continue;
        }
        let net: Usd = held.iter().map(|h| h.position.pnl()).sum();
        let name = if is_live {
            strategy.to_owned()
        } else {
            format!("{strategy} (paper)")
        };
        let title = format!(
            "{name} {} {}",
            if net.micros() >= 0 { "won" } else { "lost" },
            signed_usd(net)
        );
        let unit = held
            .first()
            .map_or("°C", |h| h.position.instrument.bucket.unit.symbol());
        let mut lines = vec![format!(
            "{place} {} settled at {}{unit} (the observed high; the venue resolves the market itself).",
            day_text(s.local_date),
            s.final_value
        )];
        for h in &held {
            let p = &h.position;
            let price = if p.shares.micros() > 0 {
                p.cost.as_f64() / p.shares.as_f64()
            } else {
                0.0
            };
            lines.push(format!(
                "{} {}: {} shares at {} ({}), {}.",
                p.instrument.outcome_side.as_str(),
                p.instrument.bucket.label(),
                shares_text(p.shares),
                price_text(price),
                usd(p.cost.as_f64()),
                if p.won() {
                    format!("won: {} back", usd(p.payout.as_f64()))
                } else {
                    "lost".to_owned()
                }
            ));
        }
        if is_live {
            let mut line = String::new();
            if let Some(total) = live.since_live {
                line.push_str(&format!("Since going live {}", signed_usd(total)));
                match live.stop {
                    Some(stop) if total <= -stop => line.push_str(&format!(
                        ": the stop is reached, {strategy} opens nothing more."
                    )),
                    Some(stop) => {
                        line.push_str(&format!("; it stops at a loss of {}.", usd(stop.as_f64())))
                    }
                    None => line.push('.'),
                }
            }
            if let Some(b) = live.balance {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&format!("Venue balance {}", usd(b.as_f64())));
                line.push_str(if held.iter().any(|h| h.position.won()) {
                    "; a win is added once it is claimed there."
                } else {
                    "."
                });
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
        notes.push((
            format!("settled|{}|{strategy}", s.event_slug),
            note(title, lines.join("\n"), s.at),
        ));
    }
    notes
}

/// The message of a live order that ended having bought shares, with its
/// key (`None` for one that did not).
pub fn bought_note(
    o: &OrderRecord,
    place: &str,
    day: Option<NaiveDate>,
    live: &LiveContext,
) -> Option<(String, Note)> {
    if !o.status.is_terminal() || o.filled.micros() <= 0 {
        return None;
    }
    let price = o.avg_price.unwrap_or(o.limit_price);
    let cost = notional(price, o.filled, Rounding::Up) + o.fees;
    let label = o.bucket.label();
    let side = o.outcome_side.as_str();
    let title = format!(
        "{} bought {} {side} {label} at {}",
        o.strategy,
        shares_text(o.filled),
        price_text(price.as_f64())
    );
    let ended = match o.status {
        OrderStatus::Filled => "filled",
        OrderStatus::Expired => "expired with the rest unfilled",
        OrderStatus::Canceled => "was cancelled with the rest unfilled",
        _ => "ended",
    };
    let on = match day {
        Some(d) => format!("{place} {}", day_text(d)),
        None => place.to_owned(),
    };
    let mut message = format!(
        "{} on {on}; its bid of {} shares at {} {ended}. It pays {} if {}.",
        usd(cost.as_f64()),
        shares_text(o.shares),
        price_text(o.limit_price.as_f64()),
        usd(o.filled.as_f64()),
        pays_if(o.outcome_side, &o.bucket)
    );
    let mut line = format!("Open now {}", usd(live.open.as_f64()));
    if let Some(max) = live.max_open {
        line.push_str(&format!(" of {}", usd(max.as_f64())));
    }
    line.push('.');
    if let Some(b) = live.balance {
        line.push_str(&format!(" Venue balance {}.", usd(b.as_f64())));
    }
    message.push('\n');
    message.push_str(&line);
    Some((
        format!("bought|{}", o.client_order_id),
        note(title, message, o.updated_at),
    ))
}

/// What the runtime hands each step's output to: it makes the step's
/// messages and skips those already sent (by this run, or by an earlier one
/// that settled the same market before a restart).
pub struct Outbox {
    pub notifier: Notifier,
    /// location id → its name
    places: HashMap<String, String>,
    sent: HashSet<String>,
}

impl Outbox {
    pub fn new(
        notifier: Notifier,
        places: HashMap<String, String>,
        sent: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            notifier,
            places,
            sent: sent.into_iter().collect(),
        }
    }

    fn place(&self, location: &str) -> String {
        self.places
            .get(location)
            .cloned()
            .unwrap_or_else(|| location.to_owned())
    }

    /// The step's new messages with their keys (queue them with
    /// [`Notifier::push`]; record the keys so a restart skips them).
    pub fn notes(
        &mut self,
        engine: &Engine,
        out: &SessionOutput,
        live: &LiveContext,
    ) -> Vec<(String, Note)> {
        let mut notes = Vec::new();
        for env in &out.processed {
            let WeatherMachineEvent::OrderUpdate(u) = &env.event else {
                continue;
            };
            if !u.update.status.is_terminal() {
                continue;
            }
            let Some(o) = engine.orders().get(&u.update.client_order_id) else {
                continue;
            };
            if !engine.is_live_strategy(&o.strategy)
                || !self.notifier.who().wants(o.strategy.as_str(), true)
            {
                continue;
            }
            let day = engine.markets().get(&o.event_slug).map(|m| m.local_date);
            notes.extend(bought_note(o, &self.place(o.location.as_str()), day, live));
        }
        for s in &out.settlements {
            let place = engine.markets().get(&s.event_slug).map_or_else(
                || s.event_slug.to_string(),
                |m| self.place(m.location.as_str()),
            );
            notes.extend(settled_notes(s, &place, self.notifier.who(), live));
        }
        notes.retain(|(key, _)| self.sent.insert(key.clone()));
        notes
    }
}

// -- Sending ---------------------------------------------------------------------

/// Why a message was not delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Pushover answered 4xx, or 2xx with a status that is not 1: the
    /// request is wrong (token, user key, quota) and must not be repeated
    Refused(String),
    /// no answer, a redirect (never followed) or 5xx: worth another try
    Later(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Refused(why) => write!(f, "Pushover refused it: {why}"),
            SendError::Later(why) => write!(f, "not delivered: {why}"),
        }
    }
}

/// `application/x-www-form-urlencoded`, every byte but the unreserved ones
/// escaped.
fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::config::url_component(k),
                crate::config::url_component(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A line that is safe to log: never more than 240 characters, and never
/// the token or the user key, should an answer or an error repeat one.
fn scrub(text: &str, cfg: &Pushover) -> String {
    let mut text = text.to_owned();
    for secret in [cfg.token.expose(), cfg.user.expose()] {
        if !secret.is_empty() {
            text = text.replace(secret, "***");
        }
    }
    text.chars().take(240).collect()
}

/// What Pushover says is wrong (its `errors` list).
fn api_errors(body: &str, cfg: &Pushover) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let errs: Vec<&str> = v
        .get("errors")?
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if errs.is_empty() {
        return None;
    }
    Some(scrub(&errs.join("; "), cfg))
}

/// Why a request got no answer: the client's text and its causes (a name
/// that does not resolve, a refused connection, a timeout), never the body.
fn transport_error(e: reqwest::Error, cfg: &Pushover) -> String {
    let e = e.without_url();
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(&e);
    while let Some(c) = cause {
        let part = c.to_string();
        if !part.is_empty() && !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        cause = c.source();
    }
    if e.is_timeout() && !text.contains("timed out") {
        text.push_str(" (timed out)");
    }
    scrub(&text, cfg)
}

/// One request to `url` (Pushover's endpoint; a test's own server). Taken
/// means what Pushover's page says: a 2xx answer whose `status` is 1.
pub async fn send(
    http: &reqwest::Client,
    url: &str,
    cfg: &Pushover,
    note: &Note,
) -> Result<(), SendError> {
    let at = note.at_s.to_string();
    let body = form(&[
        ("token", cfg.token.expose()),
        ("user", cfg.user.expose()),
        ("title", &note.title),
        ("message", &note.message),
        ("timestamp", &at),
    ]);
    let resp = http
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| SendError::Later(transport_error(e, cfg)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let said = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("status").and_then(serde_json::Value::as_i64));
        return match said {
            // (an answer without the field is taken at its 2xx)
            Some(1) | None => Ok(()),
            Some(_) => Err(SendError::Refused(
                api_errors(&text, cfg).unwrap_or_else(|| "its answer's status is not 1".into()),
            )),
        };
    }
    let why = api_errors(&text, cfg).unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    if status.is_client_error() {
        Err(SendError::Refused(why))
    } else {
        Err(SendError::Later(why))
    }
}

/// Sends the queued messages, one at a time. `retry_after_s`: the waits
/// before a message's further tries; `after_a_failure`: no request sooner
/// than this after one that failed.
pub async fn run(
    cfg: Pushover,
    http: reqwest::Client,
    url: String,
    stats: Arc<NotifyStats>,
    mut rx: mpsc::Receiver<Note>,
    retry_after_s: &[u64],
    after_a_failure: Duration,
) {
    let mut failed_at: Option<tokio::time::Instant> = None;
    while let Some(note) = rx.recv().await {
        if stats.off.load(Ordering::Acquire) {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let mut waits = retry_after_s.iter();
        loop {
            if let Some(t) = failed_at {
                tokio::time::sleep_until(t + after_a_failure).await;
            }
            match send(&http, &url, &cfg, &note).await {
                Ok(()) => {
                    failed_at = None;
                    stats.sent.fetch_add(1, Ordering::Relaxed);
                    tracing::info!("notification sent: {}", note.title);
                    break;
                }
                Err(SendError::Refused(why)) => {
                    stats.off.store(true, Ordering::Release);
                    tracing::error!(
                        "notifications are off until a restart: Pushover refused a message ({why}). Check WM_PUSHOVER_TOKEN and WM_PUSHOVER_USER"
                    );
                    stats.gave_up(why);
                    break;
                }
                Err(SendError::Later(why)) => {
                    failed_at = Some(tokio::time::Instant::now());
                    match waits.next() {
                        Some(s) => {
                            tracing::warn!(
                                "notification not delivered ({why}); another try in {s} s"
                            );
                            tokio::time::sleep(Duration::from_secs(*s)).await;
                        }
                        None => {
                            tracing::error!(
                                "notification given up after {} tries ({why}): {}",
                                retry_after_s.len() + 1,
                                note.title
                            );
                            stats.gave_up(why);
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// The client the keys travel with: https only, and no redirect followed.
fn client(https_only: bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(https_only)
        .user_agent(concat!("weather-machine/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// The sender as the service runs it.
pub async fn run_pushover(cfg: Pushover, stats: Arc<NotifyStats>, rx: mpsc::Receiver<Note>) {
    match client(true) {
        Ok(http) => {
            run(
                cfg,
                http,
                PUSHOVER_URL.to_owned(),
                stats,
                rx,
                &RETRY_AFTER_S,
                AFTER_A_FAILURE,
            )
            .await;
        }
        Err(e) => {
            stats.off.store(true, Ordering::Release);
            stats.gave_up(e.clone());
            tracing::error!("notifications are off: {e}");
        }
    }
}

/// `weather-machine notify-test`: one message now, so the two keys can be
/// checked without waiting for a trade. Says what happened, never a secret.
pub async fn test(cfg: &Pushover, now: DateTime<Utc>) -> Result<String, String> {
    let n = Note {
        title: "Weather Machine can reach you".into(),
        message: format!(
            "A message like this follows every settled market of {}, and every live order that bought shares.",
            cfg.who
        ),
        at_s: now.timestamp(),
    };
    let http = client(true)?;
    match send(&http, PUSHOVER_URL, cfg, &n).await {
        Ok(()) => Ok(format!(
            "sent: \"{}\" (messages follow the settled markets of {})",
            n.title, cfg.who
        )),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wm_core::ids::{
        ClientOrderId, ConditionId, DecisionId, EventSlug, LocationId, StrategyId, TokenId,
    };
    use wm_core::market::{Side, TempUnit};
    use wm_core::portfolio::{InstrumentRef, SettledPosition};
    use wm_core::trading::{IntentKind, TimeInForce};
    use wm_core::units::Price;

    const TOKEN: &str = "apptoken0123456789abcdefghij12";
    const USER: &str = "userkey0123456789abcdefghijk12";

    fn known() -> Vec<(String, String)> {
        [
            ("G_tail_seller", "G"),
            ("G_live", "GL"),
            ("F_peak_slot", "F"),
        ]
        .iter()
        .map(|(i, l)| ((*i).to_owned(), (*l).to_owned()))
        .collect()
    }

    fn account() -> Pushover {
        Pushover::from_env(Some(TOKEN), Some(USER), None, &known())
            .unwrap()
            .unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn holding(
        strategy: &str,
        live: bool,
        v: i32,
        price: &str,
        shares: i64,
        won: bool,
    ) -> SettledHolding {
        let s = Shares::from_whole(shares);
        SettledHolding {
            strategy: StrategyId::new(strategy).unwrap(),
            live,
            position: SettledPosition {
                instrument: InstrumentRef {
                    token: TokenId::new(format!("tok-{v}")).unwrap(),
                    condition_id: ConditionId::new(format!("0xc{v}")).unwrap(),
                    event_slug: EventSlug::new("highest-temperature-in-amsterdam-on-october-12")
                        .unwrap(),
                    outcome_side: OutcomeSide::No,
                    bucket: TemperatureBucket::exact(v, TempUnit::Celsius),
                },
                shares: s,
                cost: notional(Price::parse(price).unwrap(), s, Rounding::Up),
                payout: if won {
                    Usd::from_micros(s.micros())
                } else {
                    Usd::ZERO
                },
            },
        }
    }

    fn settlement(positions: Vec<SettledHolding>) -> Settlement {
        Settlement {
            event_slug: EventSlug::new("highest-temperature-in-amsterdam-on-october-12").unwrap(),
            local_date: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
            final_value: 14,
            pnl: Usd::ZERO,
            lab_pnl: Usd::ZERO,
            live_pnl: Usd::ZERO,
            positions,
            at: utc("2026-10-13T00:00:01Z"),
        }
    }

    #[test]
    fn whose_results_are_pushed() {
        let k = known();
        assert_eq!(Who::parse("", &k), Ok(Who::Live));
        assert_eq!(Who::parse(" LIVE ", &k), Ok(Who::Live));
        assert_eq!(Who::parse("all", &k), Ok(Who::All));
        // ids and letters, the letters resolved to the engine's ids
        let ids = Who::parse("GL, g,", &k).unwrap();
        assert_eq!(
            ids,
            Who::Ids(["G_live".to_owned(), "G_tail_seller".to_owned()].into())
        );
        assert_eq!(ids.to_string(), "G_live, G_tail_seller");
        assert!(Who::Live.wants("G_live", true) && !Who::Live.wants("G_tail_seller", false));
        assert!(Who::All.wants("F_peak_slot", false));
        assert!(ids.wants("G_tail_seller", false) && !ids.wants("F_peak_slot", false));
        for nobody in [",", " , ,, "] {
            assert!(
                Who::parse(nobody, &k)
                    .unwrap_err()
                    .contains("names no strategy")
            );
        }
        assert!(
            Who::parse("live,G", &k)
                .unwrap_err()
                .contains("stand alone")
        );
        // what is no strategy is never repeated: a key pasted here must not reach a log
        let e = Who::parse(USER, &k).unwrap_err();
        assert!(e.contains("30 characters long") && !e.contains(USER), "{e}");
    }

    #[test]
    fn the_keys_are_checked_and_never_printed() {
        let k = known();
        assert!(
            Pushover::from_env(None, None, Some("all"), &k)
                .unwrap()
                .is_none()
        );
        let half = Pushover::from_env(Some(TOKEN), None, None, &k).unwrap_err();
        assert!(half.contains("WM_PUSHOVER_USER is not"), "{half}");
        let short = Pushover::from_env(Some("abc"), Some(USER), None, &k).unwrap_err();
        assert!(
            short.contains("3 characters") && !short.contains("abc"),
            "{short}"
        );
        let odd = format!("{}!", &TOKEN[..29]);
        let e = Pushover::from_env(Some(&odd), Some(USER), None, &k).unwrap_err();
        assert!(!e.contains(&odd[..29]), "{e}");
        let e = Pushover::from_env(Some(TOKEN), Some(USER), Some(TOKEN), &k).unwrap_err();
        assert!(
            e.starts_with("WM_PUSHOVER_STRATEGIES") && !e.contains(TOKEN),
            "{e}"
        );
        let p = account();
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("apptoken") && !shown.contains("userkey"),
            "{shown}"
        );
        assert_eq!(p.who, Who::Live);
        // an answer that repeated a key is scrubbed
        let body = format!(r#"{{"status":0,"errors":["application token {TOKEN} is invalid"]}}"#);
        assert_eq!(
            api_errors(&body, &p).unwrap(),
            "application token *** is invalid"
        );
        assert_eq!(api_errors("<html>bad gateway</html>", &p), None);
        assert_eq!(scrub(&"x".repeat(1_000), &p).len(), 240);
    }

    #[test]
    fn a_settled_market_is_one_message_per_strategy() {
        let s = settlement(vec![
            holding("G_live", true, 18, "0.94", 10, true),
            holding("G_live", true, 19, "0.95", 10, true),
            holding("G_tail_seller", false, 13, "0.93", 31, false),
        ]);
        let live = LiveContext {
            since_live: Some(Usd::parse("1.10").unwrap()),
            stop: Some(Usd::from_whole(20)),
            balance: Some(Usd::parse("31.10").unwrap()),
            open: Usd::ZERO,
            max_open: Some(Usd::from_whole(40)),
        };
        // the default: the live twin only
        let notes = settled_notes(&s, "Amsterdam", &Who::Live, &live);
        assert_eq!(notes.len(), 1);
        let (key, n) = &notes[0];
        assert_eq!(
            key,
            "settled|highest-temperature-in-amsterdam-on-october-12|G_live"
        );
        assert_eq!(n.title, "G_live won +$1.10");
        assert_eq!(
            n.message,
            "Amsterdam 12 Oct settled at 14°C (the observed high; the venue resolves the market itself).\n\
             NO 18°C: 10 shares at 0.94 ($9.40), won: $10.00 back.\n\
             NO 19°C: 10 shares at 0.95 ($9.50), won: $10.00 back.\n\
             Since going live +$1.10; it stops at a loss of $20.00. Venue balance $31.10; a win is added once it is claimed there."
        );
        assert_eq!(n.at_s, utc("2026-10-13T00:00:01Z").timestamp());
        // all: the paper twin too, as paper, without the live line
        let notes = settled_notes(&s, "Amsterdam", &Who::All, &live);
        assert_eq!(notes.len(), 2);
        let (_, paper) = &notes[1];
        assert_eq!(paper.title, "G_tail_seller (paper) lost −$28.83");
        assert!(
            paper
                .message
                .ends_with("NO 13°C: 31 shares at 0.93 ($28.83), lost."),
            "{}",
            paper.message
        );
        // the stop reached says so
        let stopped = LiveContext {
            since_live: Some(Usd::parse("-20.40").unwrap()),
            balance: None,
            ..live
        };
        let lost = settlement(vec![holding("G_live", true, 14, "0.94", 10, false)]);
        let (_, n) = &settled_notes(&lost, "Amsterdam", &Who::Live, &stopped)[0];
        assert_eq!(n.title, "G_live lost −$9.40");
        assert!(
            n.message.ends_with(
                "Since going live −$20.40: the stop is reached, G_live opens nothing more."
            ),
            "{}",
            n.message
        );
    }

    fn order(status: OrderStatus, filled: i64) -> OrderRecord {
        OrderRecord {
            client_order_id: ClientOrderId::new("wm-run-7-0").unwrap(),
            decision_id: DecisionId(7),
            strategy: StrategyId::new("G_live").unwrap(),
            location: LocationId::new("amsterdam").unwrap(),
            event_slug: EventSlug::new("highest-temperature-in-amsterdam-on-october-12").unwrap(),
            token: TokenId::new("tok-18").unwrap(),
            condition_id: ConditionId::new("0xc18").unwrap(),
            outcome_side: OutcomeSide::No,
            bucket: TemperatureBucket::exact(18, TempUnit::Celsius),
            side: Side::Buy,
            kind: IntentKind::Open,
            limit_price: Price::parse("0.94").unwrap(),
            shares: Shares::from_whole(10),
            tif: TimeInForce::Gtd {
                expires_at: utc("2026-10-12T12:45:00Z"),
            },
            status,
            filled: Shares::from_whole(filled),
            avg_price: (filled > 0).then(|| Price::parse("0.94").unwrap()),
            fees: Usd::ZERO,
            venue_order_id: Some("0xabc".into()),
            reason: None,
            created_at: utc("2026-10-12T12:32:00Z"),
            updated_at: utc("2026-10-12T12:45:00Z"),
        }
    }

    #[test]
    fn a_live_order_that_bought_is_one_message_when_it_ends() {
        let live = LiveContext {
            since_live: None,
            stop: None,
            balance: Some(Usd::parse("36.24").unwrap()),
            open: Usd::parse("3.76").unwrap(),
            max_open: Some(Usd::from_whole(40)),
        };
        let day = NaiveDate::from_ymd_opt(2026, 10, 12);
        let (key, n) =
            bought_note(&order(OrderStatus::Expired, 4), "Amsterdam", day, &live).unwrap();
        assert_eq!(key, "bought|wm-run-7-0");
        assert_eq!(n.title, "G_live bought 4 NO 18°C at 0.94");
        assert_eq!(
            n.message,
            "$3.76 on Amsterdam 12 Oct; its bid of 10 shares at 0.94 expired with the rest unfilled. \
             It pays $4.00 if the high is not 18°C.\nOpen now $3.76 of $40.00. Venue balance $36.24."
        );
        let (_, full) =
            bought_note(&order(OrderStatus::Filled, 10), "Amsterdam", day, &live).unwrap();
        assert!(
            full.message
                .contains("its bid of 10 shares at 0.94 filled."),
            "{}",
            full.message
        );
        // resting, or ended without a share: no message
        assert!(
            bought_note(
                &order(OrderStatus::PartiallyFilled, 4),
                "Amsterdam",
                day,
                &live
            )
            .is_none()
        );
        assert!(bought_note(&order(OrderStatus::Expired, 0), "Amsterdam", day, &live).is_none());
        // when each side of a bucket pays
        let b = |lo: Option<i32>, hi: Option<i32>| TemperatureBucket {
            lower: lo,
            upper: hi,
            unit: TempUnit::Celsius,
        };
        assert_eq!(
            pays_if(OutcomeSide::No, &b(Some(24), None)),
            "the high stays under 24°C"
        );
        assert_eq!(
            pays_if(OutcomeSide::No, &b(None, Some(10))),
            "the high ends above 10°C"
        );
        assert_eq!(
            pays_if(OutcomeSide::Yes, &b(Some(17), Some(18))),
            "the high is 17-18°C"
        );
    }

    #[test]
    fn long_texts_stay_within_pushover_limits() {
        let n = note(
            "t".repeat(300),
            "m".repeat(2_000),
            utc("2026-10-12T12:00:00Z"),
        );
        assert_eq!(n.title.chars().count(), TITLE_MAX);
        assert_eq!(n.message.chars().count(), MESSAGE_MAX);
        assert!(n.message.ends_with('…'));
    }

    #[test]
    fn the_form_escapes_everything_that_could_break_it() {
        assert_eq!(
            form(&[
                ("title", "G_live lost \u{2212}$9.40"),
                ("message", "a&b=c\n100%")
            ]),
            "title=G_live%20lost%20%E2%88%92%249.40&message=a%26b%3Dc%0A100%25"
        );
    }

    fn sample() -> Note {
        Note {
            title: "G_live won +$0.60".into(),
            message: "NO 18°C".into(),
            at_s: 1_791_763_201,
        }
    }

    fn test_client() -> reqwest::Client {
        client(false).unwrap()
    }

    #[tokio::test]
    async fn a_message_is_one_form_post_with_the_keys_in_its_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/1/messages.json"))
            .and(body_string_contains(format!(
                "token={TOKEN}&user={USER}&title=G_live"
            )))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"status":1,"request":"r1"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/1/messages.json", server.uri());
        assert_eq!(
            send(&test_client(), &url, &account(), &sample()).await,
            Ok(())
        );
        let req = &server.received_requests().await.unwrap()[0];
        assert!(
            !req.url.as_str().contains("apptoken"),
            "the keys are in the body only"
        );
        assert!(
            req.headers
                .iter()
                .all(|(_, v)| !v.to_str().unwrap_or("").contains("userkey"))
        );
        let body = String::from_utf8(req.body.clone()).unwrap();
        assert!(body.ends_with("&timestamp=1791763201"), "{body}");
    }

    #[tokio::test]
    async fn taken_is_a_2xx_whose_status_is_one_and_a_refusal_switches_it_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"token":"invalid","errors":["application token is invalid"],"status":0}"#,
            ))
            .mount(&server)
            .await;
        let url = format!("{}/1/messages.json", server.uri());
        assert_eq!(
            send(&test_client(), &url, &account(), &sample()).await,
            Err(SendError::Refused("application token is invalid".into()))
        );
        // the sender: one request for the first message, none for the second
        let (n, rx) = Notifier::new(Who::Live);
        n.push(sample());
        n.push(sample());
        let stats = Arc::clone(&n.stats);
        drop(n);
        run(
            account(),
            test_client(),
            url,
            Arc::clone(&stats),
            rx,
            &[],
            Duration::ZERO,
        )
        .await;
        assert!(stats.off.load(Ordering::Acquire));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "one by send, one by the sender"
        );
        let st = stats.state(&Who::Live);
        assert!(!st.ok);
        assert_eq!(
            st.detail,
            "off until a restart: Pushover refused a message (application token is invalid)"
        );
        // a 200 that says status 0 is a refusal too
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":0}"#))
            .mount(&server)
            .await;
        let url = format!("{}/1/messages.json", server.uri());
        assert_eq!(
            send(&test_client(), &url, &account(), &sample()).await,
            Err(SendError::Refused("its answer's status is not 1".into()))
        );
    }

    #[tokio::test]
    async fn a_server_error_or_a_redirect_is_tried_again_and_never_followed() {
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":1}"#))
            .mount(&elsewhere)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/x", elsewhere.uri())),
            )
            .mount(&server)
            .await;
        let url = format!("{}/1/messages.json", server.uri());
        assert_eq!(
            send(&test_client(), &url, &account(), &sample()).await,
            Err(SendError::Later("HTTP 307".into()))
        );
        assert!(
            elsewhere.received_requests().await.unwrap().is_empty(),
            "the keys went nowhere else"
        );
        // three tries, then given up; the sender stays on
        let (n, rx) = Notifier::new(Who::Live);
        n.push(sample());
        let stats = Arc::clone(&n.stats);
        drop(n);
        run(
            account(),
            test_client(),
            url.clone(),
            Arc::clone(&stats),
            rx,
            &[0, 0],
            Duration::ZERO,
        )
        .await;
        assert!(!stats.off.load(Ordering::Acquire));
        assert_eq!(server.received_requests().await.unwrap().len(), 1 + 3);
        let st = stats.state(&Who::Live);
        assert!(
            st.ok && st.detail.contains("0 sent, 1 not delivered"),
            "{}",
            st.detail
        );
        // the service's own client sends nothing over plain http
        let plain = send(&client(true).unwrap(), &url, &account(), &sample()).await;
        assert!(matches!(plain, Err(SendError::Later(_))), "{plain:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        let (n, _rx) = Notifier::new(Who::Live);
        for _ in 0..QUEUE + 3 {
            n.push(sample());
        }
        assert_eq!(n.stats.dropped.load(Ordering::Relaxed), 3);
        let st = n.stats.state(&Who::Live);
        assert_eq!(
            st.detail,
            "Pushover, for the strategies that trade real money · 0 sent, 3 dropped"
        );
    }
}
