//! Deterministic synthetic market data for tests and the dashboard demo mode.
//!
//! Everything produced here is labelled `synthetic` (slugs, ids, rules text)
//! and must never be mixed with real data. Demo mode shows a banner.

use crate::ids::{ConditionId, EventSlug, LocationId, StationId, TokenId};
use crate::market::{BookLevel, DailyTemperatureMarket, FeeSchedule, MarketExtreme, MarketOutcome, OrderBook, TempUnit, TemperatureBucket};
use crate::resolution::{
    FilterCertainty, ObservationFilter, ResolutionSourceKind, ResolutionSpec, RevisionPolicy, RulesText, SpecReviewStatus,
};
use crate::units::{Price, Shares};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;

/// Synthetic daily-high event with buckets `≤lo`, `lo+1 … hi−1`, `≥hi` (°C).
pub fn synthetic_temperature_market(
    location: &LocationId,
    station: &StationId,
    date: NaiveDate,
    tz: Tz,
    lo: i32,
    hi: i32,
    now: DateTime<Utc>,
) -> DailyTemperatureMarket {
    let slug = format!("synthetic-highest-temperature-in-{location}-on-{date}");
    let mut outcomes = Vec::new();
    let unit = TempUnit::Celsius;
    let mut push = |bucket: TemperatureBucket, label: String, i: i32| {
        outcomes.push(MarketOutcome {
            condition_id: ConditionId::from_static_string(format!("0xsynthetic{date}{i:03}")),
            question_id: None,
            market_slug: Some(format!("{slug}-{i}")),
            label,
            bucket,
            yes_token: TokenId::from_static_string(format!("syn-{date}-{i:03}-yes")),
            no_token: TokenId::from_static_string(format!("syn-{date}-{i:03}-no")),
            tick_size: Price::saturating_from_micros(10_000),
            min_order_size: Shares::from_whole(5),
            accepting_orders: true,
            closed: false,
        });
    };
    push(TemperatureBucket::at_or_below(lo, unit), format!("{lo}°C or below"), lo);
    for v in (lo + 1)..hi {
        push(TemperatureBucket::exact(v, unit), format!("{v}°C"), v);
    }
    push(TemperatureBucket::at_or_above(hi, unit), format!("{hi}°C or higher"), hi);
    let rules = RulesText::new(
        "SYNTHETIC DEMO MARKET — not a real Polymarket market. Resolves to the highest reading under the \"Temp\" column (whole °C).",
        Some(format!("https://www.weather.gov/wrh/timeseries?site={}", station.as_str().to_ascii_lowercase())),
    );
    let resolution = ResolutionSpec {
        source: ResolutionSourceKind::NoaaWrhTimeseries {
            site: station.to_string(),
            url: rules.resolution_source_url.clone().unwrap_or_default(),
        },
        fallback: None,
        extreme: MarketExtreme::DailyMax,
        unit,
        whole_degrees: true,
        day_timezone: tz,
        filters: vec![ObservationFilter::AllRows, ObservationFilter::WRH_HOURLY_NWS_FAA],
        filter_certainty: FilterCertainty::Unconfirmed,
        revision_policy: RevisionPolicy::UntilFirstDatapointOfNextDay,
        rules_sha256: rules.sha256.clone(),
        parser_version: 1,
        review: SpecReviewStatus::AutoParsed,
        unrecognized_clauses: Vec::new(),
        notes: vec!["synthetic".into()],
    };
    DailyTemperatureMarket {
        event_slug: EventSlug::from_static_string(slug),
        event_id: format!("synthetic-{date}"),
        title: format!("[SYNTHETIC] Highest temperature in {location} on {date}?"),
        location: location.clone(),
        station: station.clone(),
        local_date: date,
        timezone: tz,
        extreme: MarketExtreme::DailyMax,
        unit,
        neg_risk: true,
        outcomes,
        end_time: None,
        rules,
        resolution,
        fees: FeeSchedule::taker(50_000),
        active: true,
        closed: false,
        discovered_at: now,
    }
}

/// Two-level synthetic book around `bid`/`ask`.
pub fn synthetic_book(token: &TokenId, bid: Option<&str>, ask: Option<&str>, size: i64, now: DateTime<Utc>) -> OrderBook {
    let lvl = |p: &str| Price::parse(p).ok().map(|price| BookLevel { price, size: Shares::from_whole(size) });
    OrderBook {
        token: token.clone(),
        bids: bid.and_then(lvl).into_iter().collect(),
        asks: ask.and_then(lvl).into_iter().collect(),
        tick_size: Price::saturating_from_micros(10_000),
        min_order_size: Shares::from_whole(5),
        exchange_ts: Some(now),
        received_at: now,
        hash: None,
    }
}
