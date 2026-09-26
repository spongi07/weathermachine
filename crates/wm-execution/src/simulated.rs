//! Simulated exchange shared by backtests and paper trading.
//!
//! Orders become effective `latency` after submission and execute against the
//! book *as it is then* (liquidity can vanish in between). Marketable parts
//! walk the book; resting parts fill only from trades printing at/through
//! their price after the queue ahead (conservative). No mid-price fills.

use crate::fill_model::{passive_fill_from_trade, simulate_taker};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use wm_core::event::OrderUpdateEvent;
use wm_core::ids::{ClientOrderId, TokenId};
use wm_core::market::{FeeSchedule, OrderBook, Side, TradePrint};
use wm_core::trading::{Fill, Liquidity, OrderStatus, OrderUpdate, TimeInForce};
use wm_core::units::{Price, Rounding, Shares, Usd, notional};
use wm_risk::ApprovedIntent;

/// Execution-simulation settings (research dimensions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimConfig {
    /// Submission-to-matching latency.
    pub latency_ms: i64,
    /// Pessimism: shift the opposite side of the book this many ticks against us.
    pub adverse_ticks: u32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            latency_ms: 250,
            adverse_ticks: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Pending {
    effective_at: DateTime<Utc>,
    id: ClientOrderId,
    token: TokenId,
    side: Side,
    limit: Price,
    shares: Shares,
    tif: TimeInForce,
    fees: FeeSchedule,
}

#[derive(Debug, Clone)]
struct Resting {
    id: ClientOrderId,
    token: TokenId,
    side: Side,
    limit: Price,
    shares: Shares,
    filled: Shares,
    queue_ahead: Shares,
    fee_paid: Usd,
    gross: Usd,
    expires_at: Option<DateTime<Utc>>,
    fees: FeeSchedule,
}

/// The simulated venue.
#[derive(Debug, Clone, Default)]
pub struct SimulatedExchange {
    cfg: SimConfig,
    books: HashMap<TokenId, OrderBook>,
    pending: Vec<Pending>,
    resting: BTreeMap<ClientOrderId, Resting>,
}

fn shifted(book: &OrderBook, ticks: u32) -> OrderBook {
    if ticks == 0 {
        return book.clone();
    }
    let delta = Price::saturating_from_micros(book.tick_size.micros().saturating_mul(ticks));
    let mut b = book.clone();
    for l in &mut b.asks {
        l.price = l.price.saturating_add(delta);
    }
    for l in &mut b.bids {
        l.price = l.price.saturating_sub(delta);
    }
    b
}

fn avg(gross: Usd, filled: Shares) -> Option<Price> {
    if filled.micros() <= 0 {
        return None;
    }
    let num = i128::from(gross.micros()) * 1_000_000;
    u32::try_from(num / i128::from(filled.micros()))
        .ok()
        .and_then(|m| Price::from_micros(m).ok())
}

impl SimulatedExchange {
    pub fn new(cfg: SimConfig) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    pub fn book(&self, token: &TokenId) -> Option<&OrderBook> {
        self.books.get(token)
    }

    /// Queue an approved order (effective after the configured latency).
    pub fn submit(
        &mut self,
        approved: &ApprovedIntent,
        fees: FeeSchedule,
        now: DateTime<Utc>,
    ) -> Vec<OrderUpdateEvent> {
        let i = approved.intent();
        self.pending.push(Pending {
            effective_at: now + Duration::milliseconds(self.cfg.latency_ms.max(0)),
            id: approved.client_order_id().clone(),
            token: i.token.clone(),
            side: i.side,
            limit: i.limit_price,
            shares: i.shares,
            tif: i.tif,
            fees,
        });
        let ack = OrderUpdate {
            client_order_id: approved.client_order_id().clone(),
            venue_order_id: Some(format!("sim-{}", approved.client_order_id())),
            status: OrderStatus::Live,
            filled: Shares::ZERO,
            avg_price: None,
            fee_paid: Usd::ZERO,
            reason: None,
            ts: now,
        };
        let mut out = vec![OrderUpdateEvent {
            update: ack,
            fill: None,
        }];
        out.extend(self.process_due(now));
        out
    }

    /// Cancel a resting or pending order.
    pub fn cancel(&mut self, id: &ClientOrderId, now: DateTime<Utc>) -> Vec<OrderUpdateEvent> {
        self.pending.retain(|p| &p.id != id);
        match self.resting.remove(id) {
            Some(r) => vec![self.update(
                &r,
                OrderStatus::Canceled,
                Some("canceled".into()),
                now,
                None,
            )],
            None => Vec::new(),
        }
    }

    fn update(
        &self,
        r: &Resting,
        status: OrderStatus,
        reason: Option<String>,
        ts: DateTime<Utc>,
        fill: Option<Fill>,
    ) -> OrderUpdateEvent {
        OrderUpdateEvent {
            update: OrderUpdate {
                client_order_id: r.id.clone(),
                venue_order_id: Some(format!("sim-{}", r.id)),
                status,
                filled: r.filled,
                avg_price: avg(r.gross, r.filled),
                fee_paid: r.fee_paid,
                reason,
                ts,
            },
            fill,
        }
    }

    /// Execute orders whose latency has elapsed.
    pub fn process_due(&mut self, now: DateTime<Utc>) -> Vec<OrderUpdateEvent> {
        let (due, later): (Vec<Pending>, Vec<Pending>) =
            self.pending.drain(..).partition(|p| p.effective_at <= now);
        self.pending = later;
        let mut out = Vec::new();
        for p in due {
            let mut r = Resting {
                id: p.id.clone(),
                token: p.token.clone(),
                side: p.side,
                limit: p.limit,
                shares: p.shares,
                filled: Shares::ZERO,
                queue_ahead: Shares::ZERO,
                fee_paid: Usd::ZERO,
                gross: Usd::ZERO,
                expires_at: match p.tif {
                    TimeInForce::Gtd { expires_at } => Some(expires_at),
                    _ => None,
                },
                fees: p.fees,
            };
            let Some(book) = self
                .books
                .get(&p.token)
                .map(|b| shifted(b, self.cfg.adverse_ticks))
            else {
                out.push(self.update(
                    &r,
                    OrderStatus::Rejected,
                    Some("no order book".into()),
                    now,
                    None,
                ));
                continue;
            };
            let fok = matches!(p.tif, TimeInForce::Fok);
            let t = simulate_taker(&book, p.side, p.limit, p.shares, &p.fees, fok);
            let mut fill = None;
            if t.filled.micros() > 0 {
                r.filled = t.filled;
                r.gross = t.gross;
                r.fee_paid = t.fee;
                fill = Some(Fill {
                    client_order_id: p.id.clone(),
                    token: p.token.clone(),
                    side: p.side,
                    price: t.avg_price.unwrap_or(p.limit),
                    shares: t.filled,
                    fee: t.fee,
                    liquidity: Liquidity::Taker,
                    ts: now,
                });
            }
            let complete = r.filled >= r.shares;
            match p.tif {
                _ if complete => out.push(self.update(&r, OrderStatus::Filled, None, now, fill)),
                TimeInForce::Fak | TimeInForce::Fok => {
                    let reason = if r.filled.micros() > 0 {
                        "partially filled; remainder canceled (FAK)"
                    } else {
                        "no liquidity at limit"
                    };
                    out.push(self.update(
                        &r,
                        OrderStatus::Canceled,
                        Some(reason.into()),
                        now,
                        fill,
                    ));
                }
                TimeInForce::Gtc | TimeInForce::Gtd { .. } => {
                    // Rest the remainder behind the displayed size at our price.
                    let same_side = match p.side {
                        Side::Buy => book
                            .bids
                            .iter()
                            .find(|l| l.price == p.limit)
                            .map(|l| l.size),
                        Side::Sell => book
                            .asks
                            .iter()
                            .find(|l| l.price == p.limit)
                            .map(|l| l.size),
                    };
                    r.queue_ahead = same_side.unwrap_or(Shares::ZERO);
                    let status = if r.filled.micros() > 0 {
                        OrderStatus::PartiallyFilled
                    } else {
                        OrderStatus::Live
                    };
                    let ev = self.update(&r, status, None, now, fill);
                    if r.filled.micros() > 0 {
                        out.push(ev);
                    }
                    self.resting.insert(r.id.clone(), r);
                }
            }
        }
        out.extend(self.expire(now));
        out
    }

    /// Update the book; process due orders and resting orders crossed by the book.
    pub fn on_book(&mut self, book: &OrderBook, now: DateTime<Utc>) -> Vec<OrderUpdateEvent> {
        self.books.insert(book.token.clone(), book.clone());
        let mut out = self.process_due(now);
        let ids: Vec<ClientOrderId> = self
            .resting
            .iter()
            .filter(|(_, r)| r.token == book.token)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(mut r) = self.resting.remove(&id) else {
                continue;
            };
            // Opposite side crossing our resting price: we are the maker at our limit.
            let crossing: Shares = match r.side {
                Side::Buy => book
                    .asks
                    .iter()
                    .filter(|l| l.price <= r.limit)
                    .map(|l| l.size)
                    .sum(),
                Side::Sell => book
                    .bids
                    .iter()
                    .filter(|l| l.price >= r.limit)
                    .map(|l| l.size)
                    .sum(),
            };
            let qty = crossing.min(r.shares - r.filled);
            if qty.micros() > 0 {
                out.push(self.maker_fill(&mut r, qty, now));
            }
            if r.filled < r.shares {
                self.resting.insert(id, r);
            }
        }
        out
    }

    /// Public trade prints can fill resting orders (queue model).
    pub fn on_trade(&mut self, trade: &TradePrint, now: DateTime<Utc>) -> Vec<OrderUpdateEvent> {
        let mut out = Vec::new();
        let ids: Vec<ClientOrderId> = self
            .resting
            .iter()
            .filter(|(_, r)| r.token == trade.token)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            let Some(mut r) = self.resting.remove(&id) else {
                continue;
            };
            let (qty, q) = passive_fill_from_trade(
                r.side,
                r.limit,
                r.shares - r.filled,
                r.queue_ahead,
                trade.price,
                trade.size,
            );
            r.queue_ahead = q;
            if qty.micros() > 0 {
                out.push(self.maker_fill(&mut r, qty, now));
            }
            if r.filled < r.shares {
                self.resting.insert(id, r);
            }
        }
        out
    }

    fn maker_fill(&self, r: &mut Resting, qty: Shares, now: DateTime<Utc>) -> OrderUpdateEvent {
        let rounding = if r.side == Side::Buy {
            Rounding::Up
        } else {
            Rounding::Down
        };
        let fee = r.fees.maker_fee(r.limit, qty);
        r.filled += qty;
        r.gross += notional(r.limit, qty, rounding);
        r.fee_paid += fee;
        let fill = Fill {
            client_order_id: r.id.clone(),
            token: r.token.clone(),
            side: r.side,
            price: r.limit,
            shares: qty,
            fee,
            liquidity: Liquidity::Maker,
            ts: now,
        };
        let status = if r.filled >= r.shares {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        self.update(r, status, None, now, Some(fill))
    }

    /// Expire GTD orders.
    pub fn expire(&mut self, now: DateTime<Utc>) -> Vec<OrderUpdateEvent> {
        let expired: Vec<ClientOrderId> = self
            .resting
            .iter()
            .filter(|(_, r)| r.expires_at.is_some_and(|e| now >= e))
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in expired {
            if let Some(r) = self.resting.remove(&id) {
                out.push(self.update(&r, OrderStatus::Expired, Some("expired".into()), now, None));
            }
        }
        out
    }

    pub fn resting_count(&self) -> usize {
        self.resting.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}
