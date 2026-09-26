//! Gamma API (market discovery/metadata) — read-only, rate-limited.
//!
//! Base URL `https://gamma-api.polymarket.com`; `GET /events?slug=<slug>`
//! returns an array of events with nested markets. Several list fields
//! (`outcomes`, `outcomePrices`, `clobTokenIds`) are JSON-encoded *strings*;
//! the model accepts both encodings.

use crate::outcomes::{OutcomeMapError, map_outcome};
use crate::rules::parse_resolution_spec;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer};
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ConditionId, EventSlug, LocationId, QuestionId, StationId, TokenId};
use wm_core::market::{
    DailyTemperatureMarket, FeeSchedule, MarketOutcome, PartitionError, TempUnit,
};
use wm_core::resolution::RulesText;
use wm_core::units::{Price, Shares};
use wm_net::{FetchError, FetchRequest, HttpFetcher};

fn string_or_array<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        List(Vec<serde_json::Value>),
        Null,
    }
    let to_s = |v: serde_json::Value| match v {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    };
    match Raw::deserialize(d)? {
        Raw::Null => Ok(Vec::new()),
        Raw::List(l) => Ok(l.into_iter().map(to_s).collect()),
        Raw::Text(t) if t.trim().is_empty() => Ok(Vec::new()),
        Raw::Text(t) => {
            let v: Vec<serde_json::Value> =
                serde_json::from_str(&t).map_err(serde::de::Error::custom)?;
            Ok(v.into_iter().map(to_s).collect())
        }
    }
}

fn number_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }))
}

/// A Gamma market (one bucket).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaMarket {
    pub id: Option<String>,
    pub question: Option<String>,
    pub condition_id: Option<String>,
    #[serde(rename = "questionID")]
    pub question_id: Option<String>,
    pub slug: Option<String>,
    pub group_item_title: Option<String>,
    pub description: Option<String>,
    pub resolution_source: Option<String>,
    pub end_date: Option<String>,
    #[serde(default, deserialize_with = "string_or_array")]
    pub outcomes: Vec<String>,
    #[serde(default, deserialize_with = "string_or_array")]
    pub outcome_prices: Vec<String>,
    #[serde(default, deserialize_with = "string_or_array")]
    pub clob_token_ids: Vec<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    pub accepting_orders: Option<bool>,
    #[serde(default, deserialize_with = "number_or_string")]
    pub order_price_min_tick_size: Option<String>,
    #[serde(default, deserialize_with = "number_or_string")]
    pub order_min_size: Option<String>,
    pub neg_risk: Option<bool>,
    pub uma_resolution_status: Option<String>,
}

/// A Gamma event (daily temperature question with bucket markets).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaEvent {
    pub id: Option<String>,
    pub slug: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub resolution_source: Option<String>,
    pub end_date: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    pub neg_risk: Option<bool>,
    #[serde(default)]
    pub markets: Vec<GammaMarket>,
}

/// Parse a `/events` response body.
pub fn parse_events(body: &[u8]) -> Result<Vec<GammaEvent>, String> {
    serde_json::from_slice(body).map_err(|e| format!("invalid Gamma events JSON: {e}"))
}

/// Location-specific market conventions (from configuration).
#[derive(Debug, Clone, PartialEq)]
pub struct LocationMarketSpec {
    pub location: LocationId,
    pub station: StationId,
    pub timezone: Tz,
    /// e.g. `highest-temperature-in-amsterdam-on-{month}-{day}-{year}`.
    pub slug_template: String,
    pub unit: TempUnit,
    pub fees: FeeSchedule,
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Render the event slug for a local date: month name, unpadded day, year.
pub fn event_slug(template: &str, date: NaiveDate) -> String {
    template
        .replace("{month}", MONTHS[date.month0() as usize])
        .replace("{day}", &date.day().to_string())
        .replace("{year}", &date.year().to_string())
}

/// Mapping failures (the event is ignored, never guessed).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MappingError {
    #[error("event has no markets")]
    NoMarkets,
    #[error("market {0} lacks condition id or token ids")]
    MissingIds(String),
    #[error("market {0}: outcomes {1:?} are not [Yes, No]")]
    NotBinary(String, Vec<String>),
    #[error(transparent)]
    Outcome(#[from] OutcomeMapError),
    #[error("bucket partition invalid: {0}")]
    Partition(#[from] PartitionError),
    #[error("invalid id: {0}")]
    Id(String),
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Convert a Gamma event into a typed [`DailyTemperatureMarket`].
pub fn build_market(
    event: &GammaEvent,
    spec: &LocationMarketSpec,
    date: NaiveDate,
    now: DateTime<Utc>,
) -> Result<DailyTemperatureMarket, MappingError> {
    if event.markets.is_empty() {
        return Err(MappingError::NoMarkets);
    }
    let first = &event.markets[0];
    let rules_text = event
        .description
        .clone()
        .filter(|d| !d.trim().is_empty())
        .or_else(|| first.description.clone())
        .unwrap_or_default();
    let source_url = event
        .resolution_source
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            first
                .resolution_source
                .clone()
                .filter(|s| !s.trim().is_empty())
        });
    let rules = RulesText::new(rules_text, source_url);
    let resolution = parse_resolution_spec(&rules, spec.timezone, spec.unit);

    let mut outcomes = Vec::new();
    for m in &event.markets {
        let label_id = m.id.clone().or_else(|| m.slug.clone()).unwrap_or_default();
        let bucket = map_outcome(m.group_item_title.as_deref(), m.question.as_deref())?;
        let cond = m
            .condition_id
            .clone()
            .ok_or_else(|| MappingError::MissingIds(label_id.clone()))?;
        if m.clob_token_ids.len() != 2 || m.outcomes.len() != 2 {
            return Err(MappingError::MissingIds(label_id));
        }
        let yes_idx = m
            .outcomes
            .iter()
            .position(|o| o.eq_ignore_ascii_case("yes"));
        let no_idx = m.outcomes.iter().position(|o| o.eq_ignore_ascii_case("no"));
        let (Some(yi), Some(ni)) = (yes_idx, no_idx) else {
            return Err(MappingError::NotBinary(label_id, m.outcomes.clone()));
        };
        let tick = m
            .order_price_min_tick_size
            .as_deref()
            .and_then(|t| Price::parse(t).ok())
            .unwrap_or(Price::saturating_from_micros(10_000));
        let min_size = m
            .order_min_size
            .as_deref()
            .and_then(|t| Shares::parse(t).ok())
            .unwrap_or(Shares::from_whole(5));
        outcomes.push(MarketOutcome {
            condition_id: ConditionId::new(cond).map_err(|e| MappingError::Id(e.to_string()))?,
            question_id: m.question_id.clone().and_then(|q| QuestionId::new(q).ok()),
            market_slug: m.slug.clone(),
            label: m.group_item_title.clone().unwrap_or_else(|| bucket.label()),
            bucket,
            yes_token: TokenId::new(m.clob_token_ids[yi].clone())
                .map_err(|e| MappingError::Id(e.to_string()))?,
            no_token: TokenId::new(m.clob_token_ids[ni].clone())
                .map_err(|e| MappingError::Id(e.to_string()))?,
            tick_size: tick,
            min_order_size: min_size,
            accepting_orders: m.accepting_orders.unwrap_or(false),
            closed: m.closed.unwrap_or(false),
        });
    }
    outcomes.sort_by_key(|o| o.bucket.sort_key());
    let unit = outcomes.first().map_or(spec.unit, |o| o.bucket.unit);
    let market = DailyTemperatureMarket {
        event_slug: EventSlug::new(event.slug.clone())
            .map_err(|e| MappingError::Id(e.to_string()))?,
        event_id: event.id.clone().unwrap_or_default(),
        title: event.title.clone().unwrap_or_default(),
        location: spec.location.clone(),
        station: spec.station.clone(),
        local_date: date,
        timezone: spec.timezone,
        extreme: resolution.extreme,
        unit,
        neg_risk: event.neg_risk.unwrap_or(false),
        outcomes,
        end_time: parse_ts(event.end_date.as_deref()),
        rules,
        resolution,
        fees: spec.fees,
        active: event.active.unwrap_or(false),
        closed: event.closed.unwrap_or(false),
        discovered_at: now,
    };
    market.validate_partition()?;
    Ok(market)
}

/// Read-only Gamma client.
pub struct GammaClient {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
}

impl GammaClient {
    pub const DEFAULT_BASE: &'static str = "https://gamma-api.polymarket.com";

    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn fetcher(&self) -> &Arc<HttpFetcher> {
        &self.fetcher
    }

    /// Fetch events for a slug. Returns the raw body too (persisted verbatim).
    pub async fn events_by_slug(
        &self,
        slug: &str,
        max_gate_wait: Duration,
    ) -> Result<(Vec<GammaEvent>, bytes::Bytes), GammaError> {
        let endpoint = format!("/events?slug={slug}");
        let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint)
            .accept("application/json")
            .max_gate_wait(max_gate_wait);
        let resp = self.fetcher.get(&req).await?;
        let events = parse_events(&resp.body).map_err(GammaError::Malformed)?;
        Ok((events, resp.body))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GammaError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("malformed response: {0}")]
    Malformed(String),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;

    /// Synthetic fixture shaped like Gamma `/events?slug=` responses
    /// (token/condition ids are fake). Replace with a captured sample in Phase 4.
    pub(crate) fn fixture() -> String {
        let mut markets = Vec::new();
        let mut add = |title: &str, i: u32| {
            markets.push(format!(
                r#"{{"id":"m{i}","question":"Will the highest temperature in Amsterdam be {title} on September 25?","conditionId":"0xcond{i}","questionID":"0xq{i}","slug":"amsterdam-{i}","groupItemTitle":"{title}","outcomes":"[\"Yes\", \"No\"]","outcomePrices":"[\"0.5\", \"0.5\"]","clobTokenIds":"[\"{yes}\", \"{no}\"]","active":true,"closed":false,"acceptingOrders":true,"orderPriceMinTickSize":0.01,"orderMinSize":5,"negRisk":true}}"#,
                yes = 1000 + i,
                no = 2000 + i
            ));
        };
        add("13°C or below", 13);
        for v in 14..=23 {
            add(&format!("{v}°C"), v);
        }
        add("24°C or higher", 24);
        format!(
            r#"[{{"id":"e1","slug":"highest-temperature-in-amsterdam-on-september-25-2026","title":"Highest temperature in Amsterdam on September 25?","description":"This market will resolve to the temperature range that contains the highest temperature recorded by NOAA at the Amsterdam Airport Schiphol Station in degrees Celsius on 25 Sep '26. The resolution source for this market will be information from NOAA, specifically the highest reading under the \"Temp\" column for all times on the specified day, available here: https://www.weather.gov/wrh/timeseries?site=eham. The resolution source for this market measures temperatures to whole degrees Celsius (eg, 9°C), which is the level of precision that will be used when resolving the market.","resolutionSource":"https://www.weather.gov/wrh/timeseries?site=eham","endDate":"2026-09-25T12:00:00Z","active":true,"closed":false,"negRisk":true,"markets":[{}]}}]"#,
            markets.join(",")
        )
    }

    pub(crate) fn spec() -> LocationMarketSpec {
        LocationMarketSpec {
            location: LocationId::new("amsterdam").unwrap(),
            station: StationId::new("EHAM").unwrap(),
            timezone: Amsterdam,
            slug_template: "highest-temperature-in-amsterdam-on-{month}-{day}-{year}".into(),
            unit: TempUnit::Celsius,
            fees: FeeSchedule::taker(50_000),
        }
    }

    #[test]
    fn slug_format() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 5).unwrap();
        assert_eq!(
            event_slug(&spec().slug_template, d),
            "highest-temperature-in-amsterdam-on-september-5-2026"
        );
    }

    #[test]
    fn fixture_maps_to_valid_market() {
        let events = parse_events(fixture().as_bytes()).unwrap();
        assert_eq!(events.len(), 1);
        let m = build_market(
            &events[0],
            &spec(),
            NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(m.outcomes.len(), 12);
        assert_eq!(m.outcome_for_value(18).unwrap().yes_token.as_str(), "1018");
        assert_eq!(m.outcome_for_value(18).unwrap().no_token.as_str(), "2018");
        assert_eq!(m.outcome_for_value(-5).unwrap().label, "13°C or below");
        assert_eq!(m.outcome_for_value(30).unwrap().label, "24°C or higher");
        assert!(m.neg_risk);
        assert!(
            m.resolution.is_machine_tradable(),
            "{:?}",
            m.resolution.unrecognized_clauses
        );
        assert_eq!(m.outcomes[0].tick_size, Price::parse("0.01").unwrap());
        assert_eq!(
            m.end_time.unwrap().to_rfc3339(),
            "2026-09-25T12:00:00+00:00"
        );
    }

    #[test]
    fn swapped_outcome_order_is_respected() {
        let body = fixture().replace(r#""outcomes":"[\"Yes\", \"No\"]","outcomePrices":"[\"0.5\", \"0.5\"]","clobTokenIds":"[\"1018\", \"2018\"]""#, r#""outcomes":["No","Yes"],"outcomePrices":["0.5","0.5"],"clobTokenIds":["2018","1018"]"#);
        let events = parse_events(body.as_bytes()).unwrap();
        let m = build_market(
            &events[0],
            &spec(),
            NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(m.outcome_for_value(18).unwrap().yes_token.as_str(), "1018");
    }

    #[test]
    fn gaps_in_buckets_are_rejected() {
        let body = fixture().replace("\"groupItemTitle\":\"18°C\"", "\"groupItemTitle\":\"19°C\"");
        let events = parse_events(body.as_bytes()).unwrap();
        let err = build_market(
            &events[0],
            &spec(),
            NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(),
            Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, MappingError::Partition(_)), "{err:?}");
    }

    #[test]
    fn non_binary_markets_are_rejected() {
        let body = fixture().replacen(
            r#""outcomes":"[\"Yes\", \"No\"]""#,
            r#""outcomes":"[\"Up\", \"Down\"]""#,
            1,
        );
        let events = parse_events(body.as_bytes()).unwrap();
        assert!(
            build_market(
                &events[0],
                &spec(),
                NaiveDate::from_ymd_opt(2026, 9, 25).unwrap(),
                Utc::now()
            )
            .is_err()
        );
    }
}
