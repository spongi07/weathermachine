//! Unwind engine: exits positions whose win probability has deteriorated.
//!
//! Exit style is a research dimension (blueprint §19): immediate marketable
//! exit, best-bid exit, passive limit, progressively aggressive limit,
//! time-based and probability-based triggers. Mid-price fills are never assumed.

use crate::strategy::{Proposal, ViewEvaluation, common_high, max_p_in_bucket, min_p_in_bucket};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use wm_core::ids::{StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side};
use wm_core::portfolio::PositionBook;
use wm_core::trading::{IntentKind, TimeInForce};
use wm_core::units::Price;

/// How to exit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum UnwindStyle {
    /// Sell into the book down to `floor` (walks levels; FAK).
    Marketable { floor: Price },
    /// Sell at the best bid only (FAK).
    BestBid,
    /// Rest `offset` above the best bid (GTC, maker).
    Passive { offset: Price },
    /// Start `start_offset` above the bid, step down by `step` every `step_secs`.
    Progressive {
        start_offset: Price,
        step: Price,
        step_secs: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnwindConfig {
    pub enabled: bool,
    pub style: UnwindStyle,
    /// Exit when the conservative win probability falls below this.
    pub exit_below_probability: f64,
    /// Exit after holding this long (None = hold to settlement).
    pub max_hold_minutes: Option<i64>,
}

impl Default for UnwindConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            style: UnwindStyle::BestBid,
            exit_below_probability: 0.50,
            max_hold_minutes: None,
        }
    }
}

/// Stateful unwind engine (remembers entry times and first exit signals).
#[derive(Debug, Clone)]
pub struct UnwindEngine {
    id: StrategyId,
    pub config: UnwindConfig,
    entries: HashMap<TokenId, DateTime<Utc>>,
    first_signal: HashMap<TokenId, DateTime<Utc>>,
}

impl UnwindEngine {
    pub fn new(config: UnwindConfig) -> Self {
        Self {
            id: StrategyId::from_static("U_unwind"),
            config,
            entries: HashMap::new(),
            first_signal: HashMap::new(),
        }
    }

    pub fn id(&self) -> &StrategyId {
        &self.id
    }

    /// Record a buy fill (entry time for time-based exits).
    pub fn note_entry(&mut self, token: &TokenId, at: DateTime<Utc>) {
        self.entries.entry(token.clone()).or_insert(at);
    }

    /// Forget a token after it is flat.
    pub fn forget(&mut self, token: &TokenId) {
        self.entries.remove(token);
        self.first_signal.remove(token);
    }

    /// Current conservative win probability of a held leg, using deterministic
    /// facts first (a YES bucket entirely below the observed high cannot win).
    pub fn p_win(
        views: &[ViewEvaluation],
        high: i32,
        side: OutcomeSide,
        bucket: &wm_core::market::TemperatureBucket,
    ) -> Option<f64> {
        let below_high = bucket.upper.is_some_and(|hi| hi < high);
        match side {
            OutcomeSide::Yes if below_high => Some(0.0),
            OutcomeSide::No if below_high => Some(1.0),
            OutcomeSide::Yes => min_p_in_bucket(views, high, bucket).map(|x| x.0),
            OutcomeSide::No => max_p_in_bucket(views, high, bucket).map(|x| 1.0 - x.0),
        }
    }

    fn exit_price(
        &mut self,
        token: &TokenId,
        book: &OrderBook,
        now: DateTime<Utc>,
    ) -> Option<(Price, TimeInForce)> {
        let bid = book.best_bid()?.price;
        let tick = if book.tick_size.micros() == 0 {
            Price::saturating_from_micros(10_000)
        } else {
            book.tick_size
        };
        let ask_cap = book.best_ask().map(|a| a.price.saturating_sub(tick));
        match &self.config.style {
            UnwindStyle::Marketable { floor } => Some(((*floor).min(bid), TimeInForce::Fak)),
            UnwindStyle::BestBid => Some((bid, TimeInForce::Fak)),
            UnwindStyle::Passive { offset } => {
                let mut p = bid.saturating_add(*offset);
                if let Some(cap) = ask_cap {
                    p = p.min(cap).max(bid);
                }
                Some((p.floor_to_tick(tick).max(bid), TimeInForce::Gtc))
            }
            UnwindStyle::Progressive {
                start_offset,
                step,
                step_secs,
            } => {
                let first = *self.first_signal.entry(token.clone()).or_insert(now);
                let steps = ((now - first).num_seconds() / (*step_secs).max(1)).max(0) as u32;
                let reduction = step.micros().saturating_mul(steps);
                let offset =
                    Price::saturating_from_micros(start_offset.micros().saturating_sub(reduction));
                let mut p = bid.saturating_add(offset);
                if let Some(cap) = ask_cap {
                    p = p.min(cap).max(bid);
                }
                let tif = if offset.micros() == 0 {
                    TimeInForce::Fak
                } else {
                    TimeInForce::Gtc
                };
                Some((p.floor_to_tick(tick).max(bid), tif))
            }
        }
    }

    /// Propose exits for positions in `market`.
    pub fn evaluate(
        &mut self,
        market: &DailyTemperatureMarket,
        positions: &PositionBook,
        books: &HashMap<TokenId, OrderBook>,
        views: &[ViewEvaluation],
        pending_tokens: &HashSet<TokenId>,
        now: DateTime<Utc>,
    ) -> Vec<Proposal> {
        let mut out = Vec::new();
        if !self.config.enabled {
            return out;
        }
        let Ok(high) = common_high(views) else {
            return out;
        };
        let held: Vec<_> = positions.for_event(&market.event_slug).cloned().collect();
        for pos in held {
            let token = &pos.instrument.token;
            if pending_tokens.contains(token) {
                continue;
            }
            let p = Self::p_win(
                views,
                high,
                pos.instrument.outcome_side,
                &pos.instrument.bucket,
            );
            let held_too_long = match (self.config.max_hold_minutes, self.entries.get(token)) {
                (Some(max), Some(at)) => (now - *at).num_minutes() >= max,
                _ => false,
            };
            let deteriorated = p.is_some_and(|x| x < self.config.exit_below_probability);
            if !(deteriorated || held_too_long) {
                self.first_signal.remove(token);
                continue;
            }
            let Some(book) = books.get(token) else {
                continue;
            };
            let Some((limit, tif)) = self.exit_price(token, book, now) else {
                continue;
            };
            let reason = if deteriorated {
                format!(
                    "p_win {:.3} < {:.3}",
                    p.unwrap_or(0.0),
                    self.config.exit_below_probability
                )
            } else {
                "max hold time reached".to_owned()
            };
            out.push(Proposal {
                strategy: self.id.clone(),
                bucket_label: pos.instrument.bucket.label(),
                bucket: pos.instrument.bucket,
                condition_id: pos.instrument.condition_id.clone(),
                token: token.clone(),
                outcome_side: pos.instrument.outcome_side,
                side: Side::Sell,
                kind: IntentKind::Reduce,
                weather_dependent: false,
                limit_price: limit,
                shares: pos.shares,
                tif,
                p_win: p.unwrap_or(0.0),
                ev_per_share: limit.as_f64() - p.unwrap_or(0.0),
                break_even: limit.as_f64(),
                research_only: false,
                rationale: vec![reason],
            });
        }
        out
    }
}
