//! # wm-live
//!
//! The live venue: the one crate that can move money. Strategy G's live twin
//! (`G_live`) places its orders through it on Polymarket's CLOB; every other
//! strategy stays on paper.
//!
//! * [`sign`]: EIP-712 order signing (protocol 2 or 3, the neg-risk exchange
//!   per market), ported from dailyprofit and pinned by the reference
//!   client's golden vectors (`tests/golden.rs`);
//! * [`auth`]: the L1 signature that derives the API key, the L2 HMAC on
//!   every request;
//! * [`order`]: a post-only, good-till-date buy with exact amounts;
//! * [`answer`]: what the venue's answers prove;
//! * [`geo`]: whether this server may trade through the API;
//! * [`rest`]: the endpoints;
//! * [`executor`]: the task that places, follows and cancels the orders and
//!   says when a new one may go out.
//!
//! The private key comes from the environment only and never prints; the
//! API credentials live in memory only.

pub mod answer;
pub mod auth;
pub mod executor;
pub mod geo;
pub mod order;
pub mod rest;
pub mod sign;

pub use executor::{BookView, Channels, Command, LiveOrder, LiveStatus, Settings};
pub use sign::{OrderSigner, Protocol};
