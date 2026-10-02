//! The strategy lab's live inputs beyond A–K's: who trades today (the Data
//! API's taker trades with hashed wallets, for L18–L21) and how each taker
//! fared on the settled days before (their scores, for L19 and L20).
//! Read-only, through the shared Data API and Gamma gates; a failure only
//! leaves the flow rules idle, and says so on the dashboard.

use super::{SharedSide, with_side};
use crate::market_research::{market_day, short_hash};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use wm_backtest::MarketDay;
use wm_core::event::{
    EventEnvelope, EventSource, TakerTradesEvent, WalletScoresEvent, WeatherMachineEvent,
};
use wm_core::ids::{ConditionId, EventSlug};
use wm_core::market::{DailyTemperatureMarket, TakerTrade};
use wm_core::time::{Clock, local_date, local_day_bounds};
use wm_polymarket::{DataApiClient, DataTrade, GammaClient, LocationMarketSpec};
use wm_strategy::lab::WalletBook;
use wm_strategy::lab::wallets::ScoredTrade;

/// Longest wait for a gate per request (the flow rules act within minutes).
const POLL_GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// Each poll re-reads this long before the latest trade seen: the Data API
/// may index a trade after a later one. Repeats are dropped here and in the
/// engine.
const OVERLAP: Duration = Duration::minutes(5);
/// The first poll of a market reads from this long before its local day.
const FIRST_READ_BEFORE_DAY: Duration = Duration::hours(6);
/// Wallet scoring stops after this many days in a row that failed to load.
const MAX_FAILED_DAYS_IN_A_ROW: u32 = 3;

/// A Data API trade as the engine keeps it: the wallet hashed as in
/// `research market`'s cache.
pub(super) fn to_taker_trade(t: &DataTrade) -> TakerTrade {
    TakerTrade {
        token: t.asset.clone(),
        side: t.side,
        price: t.price,
        size: t.size,
        at: t.at,
        taker: t.taker.as_deref().map(short_hash),
        id: format!(
            "{}:{}:{:?}:{}:{}:{}",
            t.transaction_hash,
            t.asset,
            t.side,
            t.price,
            t.size,
            t.at.timestamp()
        ),
    }
}

/// Learn one settled day's taker trades.
pub(super) fn learn_day(book: &mut WalletBook, day: &MarketDay, fee_rate: f64) {
    book.add_trades(
        day.trades.iter().filter_map(|t| {
            Some(ScoredTrade {
                taker: t.taker.as_deref()?,
                yes_price: t.yes_price,
                taker_buys_yes: t.taker_buys_yes,
                bucket_won: t.bucket == day.winner,
            })
        }),
        fee_rate,
    );
}

/// When to score again: the next 07:00 local (yesterday's market has
/// settled on Gamma by then).
pub(super) fn next_scoring(now: DateTime<Utc>, tz: chrono_tz::Tz) -> DateTime<Utc> {
    let today = local_date(now, tz);
    let seven = |d: NaiveDate| local_day_bounds(d, tz).0 + Duration::hours(7);
    let t = seven(today);
    if t > now {
        t
    } else {
        seven(today.succ_opt().unwrap_or(today))
    }
}

/// Reads today's markets' taker trades every `poll` and hands the new ones
/// to the engine.
pub(super) async fn taker_loop(
    data: DataApiClient,
    mut today: watch::Receiver<Vec<DailyTemperatureMarket>>,
    poll: std::time::Duration,
    events: mpsc::Sender<EventEnvelope>,
    side: SharedSide,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut since: HashMap<EventSlug, DateTime<Utc>> = HashMap::new();
    let mut seen: HashMap<EventSlug, HashSet<String>> = HashMap::new();
    let mut failing = false;
    loop {
        let markets: Vec<DailyTemperatureMarket> = today
            .borrow_and_update()
            .iter()
            .filter(|m| !m.closed)
            .cloned()
            .collect();
        since.retain(|slug, _| markets.iter().any(|m| &m.event_slug == slug));
        seen.retain(|slug, _| markets.iter().any(|m| &m.event_slug == slug));
        for m in &markets {
            let now = clock.now();
            let from = since.get(&m.event_slug).map_or_else(
                || local_day_bounds(m.local_date, m.timezone).0 - FIRST_READ_BEFORE_DAY,
                |t| *t - OVERLAP,
            );
            let conditions: Vec<ConditionId> =
                m.outcomes.iter().map(|o| o.condition_id.clone()).collect();
            let fetched = tokio::select! {
                r = data.trades(&conditions, from, now, POLL_GATE_WAIT) => r,
                _ = shutdown.changed() => return,
            };
            match fetched {
                Ok(h) => {
                    if failing {
                        failing = false;
                        with_side(&side, |s| {
                            s.alert("info", "taker trades (Data API) available again")
                        });
                    }
                    let known = seen.entry(m.event_slug.clone()).or_default();
                    let fresh: Vec<TakerTrade> = h
                        .trades
                        .iter()
                        .map(to_taker_trade)
                        .filter(|t| known.insert(t.id.clone()))
                        .collect();
                    let latest = h.trades.iter().map(|t| t.at).max();
                    since.insert(
                        m.event_slug.clone(),
                        latest.map_or(now - Duration::minutes(1), |l| l.max(from + OVERLAP)),
                    );
                    if fresh.is_empty() {
                        continue;
                    }
                    tracing::debug!(market = %m.event_slug, trades = fresh.len(), "taker trades");
                    let env = EventEnvelope::new(
                        clock.now(),
                        EventSource::Live,
                        WeatherMachineEvent::TakerTrades(TakerTradesEvent { trades: fresh }),
                    );
                    if events.send(env).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(market = %m.event_slug, error = %e, "taker trades unavailable");
                    if !failing {
                        failing = true;
                        with_side(&side, |s| {
                            s.alert(
                                "warning",
                                format!(
                                    "taker trades (Data API) unavailable ({e}); the lab's flow rules L18–L21 wait"
                                ),
                            )
                        });
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(poll) => {}
            r = today.changed() => if r.is_err() { return; },
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
        }
    }
}

/// One location's settled markets and where they are cached.
pub(super) struct WalletSource {
    pub spec: LocationMarketSpec,
    pub cache_dir: PathBuf,
}

/// Scores every taker on the settled market days before today, hands the
/// scores to the engine, and does it again each morning.
#[allow(clippy::too_many_arguments)]
pub(super) async fn wallet_loop(
    gamma: GammaClient,
    data: DataApiClient,
    sources: Vec<WalletSource>,
    days: u32,
    events: mpsc::Sender<EventEnvelope>,
    side: SharedSide,
    clock: Arc<dyn Clock>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Let the service settle (books, the first reports) before downloading.
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
        _ = shutdown.changed() => return,
    }
    loop {
        let now = clock.now();
        let mut book = WalletBook::default();
        let (mut scored, mut downloaded) = (0u32, 0u32);
        let mut through: Option<NaiveDate> = None;
        let mut failed = 0u32;
        'sources: for src in &sources {
            let fee_rate = f64::from(src.spec.fees.taker_rate_micros) / 1e6;
            let today = local_date(now, src.spec.timezone);
            for back in (1..=i64::from(days)).rev() {
                let date = today - Duration::days(back);
                let r = tokio::select! {
                    r = market_day(&gamma, &data, &src.spec, &src.cache_dir, date, now) => r,
                    _ = shutdown.changed() => return,
                };
                match r {
                    Ok((Ok(day), cached)) => {
                        failed = 0;
                        learn_day(&mut book, &day, fee_rate);
                        scored += 1;
                        downloaded += u32::from(!cached);
                        through = through.max(Some(date));
                    }
                    Ok((Err(why), _)) => {
                        failed = 0;
                        tracing::debug!(%date, why, "no settled market to score");
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(%date, error = %e, "settled market day unavailable for the wallet scores");
                        if failed >= MAX_FAILED_DAYS_IN_A_ROW {
                            with_side(&side, |s| {
                                s.alert(
                                    "warning",
                                    format!(
                                        "takers' records incomplete: settled days would not download ({e}); L19 and L20 use what loaded"
                                    ),
                                )
                            });
                            break 'sources;
                        }
                    }
                }
            }
        }
        if let Some(through) = through {
            let (takers, skilled, losing) = book.counts();
            tracing::info!(
                days = scored,
                downloaded,
                takers,
                skilled,
                losing,
                "takers scored for the strategy lab"
            );
            let env = EventEnvelope::new(
                clock.now(),
                EventSource::Live,
                WeatherMachineEvent::WalletScores(WalletScoresEvent {
                    through,
                    days: scored,
                    takers,
                    scores: book.scores(),
                }),
            );
            if events.send(env).await.is_err() {
                return;
            }
        }
        let next = sources.first().map_or(now + Duration::days(1), |s| {
            next_scoring(clock.now(), s.spec.timezone)
        });
        let wait = if through.is_none() {
            std::time::Duration::from_secs(3600)
        } else {
            (next - clock.now())
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600))
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_backtest::MarketTrade;
    use wm_core::ids::TokenId;
    use wm_core::market::Side;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn trades_keep_a_hashed_wallet_and_a_stable_id() {
        let t = DataTrade {
            at: utc("2026-07-01T12:00:00Z"),
            condition_id: "0xc".into(),
            asset: TokenId::new("123").unwrap(),
            side: Side::Buy,
            price: 0.31,
            size: 40.0,
            transaction_hash: "0xabc".into(),
            taker: Some("0xWALLET".into()),
        };
        let a = to_taker_trade(&t);
        assert_eq!(a.taker.as_deref(), Some(short_hash("0xWALLET").as_str()));
        assert_eq!(a.taker.as_deref().map(str::len), Some(16));
        assert_eq!(a.id, to_taker_trade(&t).id, "the same trade, the same id");
        let mut other = t.clone();
        other.size = 41.0;
        assert_ne!(a.id, to_taker_trade(&other).id);
        assert!(!a.id.contains("WALLET"), "the wallet never appears");
    }

    #[test]
    fn a_settled_day_scores_its_takers() {
        let trade = |bucket: usize, yes: bool, taker: &str| MarketTrade {
            at: utc("2026-07-01T12:00:00Z"),
            bucket,
            yes_price: 0.40,
            taker_buys_yes: yes,
            shares: 10.0,
            taker: Some(taker.into()),
        };
        let day = MarketDay {
            date: NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            event_slug: "x".into(),
            buckets: Vec::new(),
            labels: Vec::new(),
            winner: 1,
            trades: (0..30)
                .map(|_| trade(1, true, "good"))
                .chain((0..30).map(|_| trade(1, false, "bad")))
                .chain([MarketTrade {
                    taker: None,
                    ..trade(0, true, "")
                }])
                .collect(),
            truncated: false,
        };
        let mut book = WalletBook::default();
        learn_day(&mut book, &day, 0.05);
        assert_eq!(book.len(), 2, "trades without a wallet are not scored");
        let good = book.get("good").unwrap();
        assert!((good.mean() - (0.60 - 0.05 * 0.4 * 0.6)).abs() < 1e-9);
        assert!(book.get("bad").unwrap().mean() < -0.6);
    }

    #[test]
    fn scoring_runs_again_at_seven_tomorrow_morning() {
        let tz = chrono_tz::Europe::Amsterdam;
        // 1 July 15:00 local → 2 July 07:00 local (05:00 UTC).
        assert_eq!(
            next_scoring(utc("2026-07-01T13:00:00Z"), tz),
            utc("2026-07-02T05:00:00Z")
        );
        // Just after midnight: this morning's 07:00.
        assert_eq!(
            next_scoring(utc("2026-07-01T22:30:00Z"), tz),
            utc("2026-07-02T05:00:00Z")
        );
    }
}
