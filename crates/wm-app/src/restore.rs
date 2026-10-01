//! Carry the paper book across restarts.
//!
//! A restart begins a new run, and a new run would start flat: the positions
//! of the runs before it, the strategy that opened each of them, the
//! exposure caps and the daily limits would be forgotten. Strategy F could
//! buy a second 100 shares the same day; the daily loss stop would reset.
//! Before the first live event the runtime loads from the database:
//!
//! * the fills of every market that settles today (UTC) or later, i.e. whose
//!   local day ends at most `grace` before today's UTC midnight;
//! * those markets, rebuilt from their latest stored Gamma payload with the
//!   mapping discovery uses (and the stored review of their rules);
//! * the filled cost (at the limit) of today's (UTC) opening orders: what
//!   their approvals left in the daily new exposure once the unfilled parts
//!   ended — an order still resting when the earlier run stopped ended with
//!   it.
//!
//! The engine replays the fills into its position book. A market whose
//! settlement time has passed (the previous run settled it today, or missed
//! it) settles again at the next step, so its P&L counts toward today's loss
//! limit once, as it did before the restart.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use std::collections::{BTreeMap, HashMap};
use wm_core::ids::{ClientOrderId, StrategyId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OutcomeSide, Side};
use wm_core::portfolio::InstrumentRef;
use wm_core::trading::{Fill, Liquidity};
use wm_core::units::{Price, Rounding, Shares, Usd, notional};
use wm_engine::{RestoreState, RestoreSummary, RestoredFill};
use wm_polymarket::LocationMarketSpec;
use wm_storage::{PgStore, RestoreFillRow};

use crate::config::AppConfig;
use crate::setup;

/// Local days before today the query reaches back; the settlement filter
/// below is exact.
const LOOKBACK_DAYS: i64 = 3;

/// Load the paper book of earlier runs. A market or fill that cannot be
/// rebuilt is skipped with a warning; a database error fails the load.
pub async fn load(
    store: &PgStore,
    cfg: &AppConfig,
    now: DateTime<Utc>,
    grace: Duration,
) -> Result<(RestoreState, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut locations: HashMap<String, (chrono_tz::Tz, LocationMarketSpec)> = HashMap::new();
    for l in &cfg.locations {
        let ids = setup::location_ids(l)?;
        locations.insert(
            ids.location.as_str().to_owned(),
            (ids.timezone, setup::market_spec(l)?),
        );
    }
    let Some(from) = locations
        .values()
        .map(|(tz, _)| wm_core::time::local_date(now, *tz) - Duration::days(LOOKBACK_DAYS))
        .min()
    else {
        return Ok((RestoreState::default(), warnings));
    };
    let today_utc = now.date_naive().and_time(NaiveTime::MIN).and_utc();

    let mut by_market: BTreeMap<String, Vec<RestoreFillRow>> = BTreeMap::new();
    for r in store.restore_fills(from).await.context("loading fills")? {
        let Some((tz, _)) = locations.get(&r.location_id) else {
            warnings.push(format!(
                "order {} on {} is for location {}, which is not configured: not restored",
                r.client_order_id, r.event_slug, r.location_id
            ));
            continue;
        };
        if settles_before(r.local_date, *tz, grace, today_utc) {
            continue;
        }
        by_market.entry(r.event_slug.clone()).or_default().push(r);
    }

    let mut state = RestoreState::default();
    for (slug, rows) in by_market {
        let (_, spec) = &locations[&rows[0].location_id];
        let market = match rebuild_market(store, &slug, spec, rows[0].local_date).await? {
            Ok(m) => m,
            Err(why) => {
                warnings.push(format!(
                    "market {slug}: {why}; its {} fill(s) are not restored",
                    rows.len()
                ));
                continue;
            }
        };
        for r in &rows {
            match restored_fill(r, &market) {
                Ok(f) => state.fills.push(f),
                Err(why) => warnings.push(format!(
                    "fill of order {} on {slug}: {why}; not restored",
                    r.client_order_id
                )),
            }
        }
        state.markets.push(market);
    }
    // Oldest first across markets (stable: the query's order within a time).
    state.fills.sort_by_key(|f| f.fill.ts);
    state.new_exposure_today = store
        .opening_orders_since(today_utc)
        .await
        .context("loading today's orders")?
        .into_iter()
        .filter_map(|(price, filled)| {
            let p = Price::from_micros(u32::try_from(price).ok()?).ok()?;
            Some(notional(p, Shares::from_micros(filled), Rounding::Up))
        })
        .sum();
    Ok((state, warnings))
}

/// Whether a market of local day `date` settled before today's UTC midnight
/// (its P&L belongs to an earlier day's limits and its positions are gone).
fn settles_before(
    date: NaiveDate,
    tz: chrono_tz::Tz,
    grace: Duration,
    today_utc: DateTime<Utc>,
) -> bool {
    let (_, end) = wm_core::time::local_day_bounds(date, tz);
    end + grace < today_utc
}

/// The market as discovery built it, from its latest stored Gamma payload.
/// The outer error is the database's; the inner one says why the market
/// cannot be rebuilt.
async fn rebuild_market(
    store: &PgStore,
    slug: &str,
    spec: &LocationMarketSpec,
    date: NaiveDate,
) -> Result<std::result::Result<DailyTemperatureMarket, String>> {
    let Some((captured_at, body)) = store
        .latest_market_payload(slug)
        .await
        .context("loading a market payload")?
    else {
        return Ok(Err("no stored Gamma payload".into()));
    };
    let events = match wm_polymarket::parse_events(&body) {
        Ok(e) => e,
        Err(e) => return Ok(Err(e)),
    };
    let Some(event) = events.iter().find(|e| e.slug == slug) else {
        return Ok(Err("the stored payload does not hold the event".into()));
    };
    let mut market = match wm_polymarket::build_market(event, spec, date, captured_at) {
        Ok(m) => m,
        Err(e) => return Ok(Err(format!("cannot be mapped: {e}"))),
    };
    if let Some(status) = store
        .rules_review_status(&market.rules.sha256)
        .await
        .context("loading a rules review")?
    {
        market.resolution.review = crate::runtime::review_status_of(&status);
    }
    Ok(Ok(market))
}

/// A stored fill with its instrument (from the market) and strategy.
fn restored_fill(
    r: &RestoreFillRow,
    m: &DailyTemperatureMarket,
) -> std::result::Result<RestoredFill, String> {
    let token = TokenId::new(r.token_id.clone()).map_err(|e| e.to_string())?;
    let (outcome, outcome_side) = m
        .outcomes
        .iter()
        .find_map(|o| {
            if o.yes_token == token {
                Some((o, OutcomeSide::Yes))
            } else if o.no_token == token {
                Some((o, OutcomeSide::No))
            } else {
                None
            }
        })
        .ok_or_else(|| format!("token {token} is not in the market"))?;
    let side = match r.side.as_str() {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        other => return Err(format!("unknown side {other}")),
    };
    let price = u32::try_from(r.price_micros)
        .ok()
        .and_then(|p| Price::from_micros(p).ok())
        .ok_or_else(|| format!("invalid price {} µ", r.price_micros))?;
    let fill = Fill {
        client_order_id: ClientOrderId::new(r.client_order_id.clone())
            .map_err(|e| e.to_string())?,
        token: token.clone(),
        side,
        price,
        shares: Shares::from_micros(r.shares_micros),
        fee: Usd::from_micros(r.fee_micros),
        liquidity: if r.liquidity == "maker" {
            Liquidity::Maker
        } else {
            Liquidity::Taker
        },
        ts: r.ts,
    };
    Ok(RestoredFill {
        fill,
        instrument: InstrumentRef {
            token,
            condition_id: outcome.condition_id.clone(),
            event_slug: m.event_slug.clone(),
            outcome_side,
            bucket: outcome.bucket,
        },
        strategy: StrategyId::new(r.strategy.clone()).map_err(|e| e.to_string())?,
    })
}

/// The alert of a restore.
pub fn describe(s: &RestoreSummary) -> String {
    let mut text = format!(
        "restored the paper book of earlier runs: {} open position(s) ({} shares, cost {}) from {} fill(s) in {} market(s); today's new exposure {} counts toward the daily limit",
        s.open_positions, s.open_shares, s.open_cost, s.fills, s.markets, s.new_exposure_today
    );
    if !s.realized_today.is_zero() {
        text.push_str(&format!(
            ", and so does {} realized by today's restored sales",
            s.realized_today
        ));
    }
    if !s.rejected.is_empty() {
        text.push_str(&format!(
            "; {} fill(s) refused: {}",
            s.rejected.len(),
            s.rejected.join("; ")
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn only_markets_settling_today_or_later_are_restored() {
        let tz = chrono_tz::Europe::Amsterdam;
        let grace = Duration::hours(2);
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        // 2 Oct 10:00 UTC: 1 Oct (CEST) ended 22:00 UTC and settled at
        // 00:00 UTC on 2 Oct — today; 30 Sep settled on 1 Oct.
        let today = utc("2026-10-02T00:00:00Z");
        assert!(!settles_before(d("2026-10-01"), tz, grace, today));
        assert!(!settles_before(d("2026-10-02"), tz, grace, today));
        assert!(settles_before(d("2026-09-30"), tz, grace, today));
        // Winter (CET, UTC+1): 15 Jan ends 23:00 UTC and settles at 01:00 UTC
        // on 16 Jan, which is "today" from 16 Jan on.
        let jan16 = utc("2027-01-16T00:00:00Z");
        assert!(!settles_before(d("2027-01-15"), tz, grace, jan16));
        assert!(settles_before(d("2027-01-14"), tz, grace, jan16));
    }

    #[test]
    fn the_alert_names_positions_limits_and_refusals() {
        let mut s = RestoreSummary {
            markets: 1,
            fills: 1,
            open_positions: 1,
            open_shares: Shares::from_whole(100),
            open_cost: Usd::from_micros(95_237_500),
            new_exposure_today: Usd::from_whole(95),
            ..RestoreSummary::default()
        };
        let text = describe(&s);
        assert!(
            text.contains(
                "1 open position(s) (100 shares, cost $95.2375) from 1 fill(s) in 1 market(s)"
            ),
            "{text}"
        );
        assert!(text.contains("today's new exposure $95.00 counts toward the daily limit"));
        assert!(!text.contains("refused") && !text.contains("sales"));
        s.realized_today = Usd::from_whole(-3);
        s.rejected
            .push("wm-x on m: sell of 5 exceeds held 0 shares".into());
        let text = describe(&s);
        assert!(
            text.contains("and so does $-3.00 realized by today's restored sales"),
            "{text}"
        );
        assert!(text.contains("1 fill(s) refused: wm-x on m"), "{text}");
    }
}
