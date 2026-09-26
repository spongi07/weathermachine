//! # wm-polymarket
//!
//! Read-only Polymarket integration: Gamma discovery, verbatim rules capture
//! and conservative resolution-spec parsing, temperature bucket mapping, CLOB
//! books/price history, the market-channel WebSocket and CTF economics.
//!
//! There is deliberately **no order placement** in this crate yet: live
//! execution is Phase 14 and requires signing (EIP-712, V2 order struct,
//! pUSD collateral) plus a compliance gate. Strategies never call this crate.

pub mod clob;
pub mod ctf;
pub mod gamma;
pub mod outcomes;
pub mod rules;
pub mod ws;

pub use clob::{ClobClient, ClobError, parse_book};
pub use gamma::{GammaClient, GammaError, GammaEvent, LocationMarketSpec, MappingError, build_market, event_slug, parse_events};
pub use outcomes::{map_outcome, parse_bucket_label};
pub use rules::parse_resolution_spec;
pub use ws::{LocalBook, MarketStream, MarketStreamConfig, StreamStatus, WsEvent, parse_ws_message};
