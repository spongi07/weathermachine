//! Resolution-rules capture and conservative parsing.
//!
//! The verbatim rules text of every market is persisted with its SHA-256.
//! This parser maps *known* clause patterns to a structured
//! [`ResolutionSpec`]. Any sentence that mentions a decision-relevant concept
//! it cannot map (rounding, time zones, report types, 50-50 outcomes, …) is
//! recorded as an unrecognized clause, which makes the market non-tradable
//! until a human approves that exact rules hash.

use chrono_tz::Tz;
use wm_core::market::{MarketExtreme, TempUnit};
use wm_core::resolution::{
    FilterCertainty, ObservationFilter, ResolutionSourceKind, ResolutionSpec, RevisionPolicy, RulesText, SpecReviewStatus,
};

/// Bump when parsing semantics change.
pub const RULES_PARSER_VERSION: u16 = 1;

/// Word prefixes that signal decision-relevant semantics (matched per word).
const RISK_WORD_PREFIXES: &[&str] = &[
    "round", "decimal", "utc", "gmt", "timezone", "exclud", "except", "averag", "metar", "sensor", "void", "cancel", "tenth",
];
/// Exact risk words.
const RISK_WORDS: &[&str] = &["mean", "speci", "specis"];
/// Risk phrases (matched on the lower-cased sentence).
const RISK_PHRASES: &[&str] = &["time zone", "local time", "different station", "station change", "50-50", "50/50"];

fn is_risky(lower_sentence: &str) -> bool {
    let words = lower_sentence.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty());
    for w in words {
        if RISK_WORDS.contains(&w) || RISK_WORD_PREFIXES.iter().any(|p| w.starts_with(p)) {
            return true;
        }
    }
    RISK_PHRASES.iter().any(|p| lower_sentence.contains(p))
}

fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        cur.push(*c);
        let boundary = matches!(c, '.' | '!' | '?' | '\n')
            && chars.get(i + 1).is_none_or(|n| n.is_whitespace())
            // Do not split inside numbers/URLs like "0.5" or "weather.gov".
            && !(i > 0 && chars[i - 1].is_ascii_digit() && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()));
        if boundary {
            let s = cur.trim().to_owned();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        }
    }
    let s = cur.trim().to_owned();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

fn wrh_site(text: &str) -> Option<(String, String)> {
    let lower = text.to_lowercase();
    let idx = lower.find("weather.gov/wrh/timeseries")?;
    let rest = &lower[idx..];
    let site_idx = rest.find("site=")?;
    let site: String = rest[site_idx + 5..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    if site.is_empty() {
        return None;
    }
    let url_end = rest.find(|c: char| c.is_whitespace() || c == ')' || c == '"').unwrap_or(rest.len());
    let url = format!("https://www.{}", rest[..url_end].trim_end_matches(['.', ',']));
    Some((site.to_ascii_uppercase(), url))
}

fn wunderground_url(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let idx = lower.find("wunderground.com/history")?;
    let rest = &text[idx..];
    let end = rest.find(|c: char| c.is_whitespace() || c == ')' || c == '"').unwrap_or(rest.len());
    Some(format!("https://www.{}", rest[..end].trim_end_matches(['.', ','])))
}

/// Parse market rules into a resolution spec.
pub fn parse_resolution_spec(rules: &RulesText, location_tz: Tz, default_unit: TempUnit) -> ResolutionSpec {
    let full = match &rules.resolution_source_url {
        Some(u) => format!("{}\n{}", rules.text, u),
        None => rules.text.clone(),
    };
    let lower = full.to_lowercase();
    let mut notes = Vec::new();
    let mut unrecognized = Vec::new();

    let extreme = if lower.contains("lowest temperature") || lower.contains("lowest reading") {
        MarketExtreme::DailyMin
    } else {
        MarketExtreme::DailyMax
    };
    let unit = if lower.contains("degrees fahrenheit") || lower.contains("°f") {
        TempUnit::Fahrenheit
    } else if lower.contains("degrees celsius") || lower.contains("°c") {
        TempUnit::Celsius
    } else {
        notes.push("unit not stated; using location default".into());
        default_unit
    };
    let whole_degrees = lower.contains("whole degree");

    // Primary source.
    let wrh = wrh_site(&full);
    let wu = wunderground_url(&full);
    let noaa_primary = wrh.is_some() && (lower.contains("information from noaa") || lower.contains("recorded by noaa") || lower.contains("from noaa"));
    let source = match (&wrh, &wu) {
        (Some((site, url)), _) if noaa_primary || wu.is_none() => ResolutionSourceKind::NoaaWrhTimeseries { site: site.clone(), url: url.clone() },
        (_, Some(url)) if lower.contains("information from wunderground") || lower.contains("resolution source for this market will be information from weather underground") || wrh.is_none() => {
            ResolutionSourceKind::WundergroundDaily { url: url.clone() }
        }
        (Some((site, url)), _) => ResolutionSourceKind::NoaaWrhTimeseries { site: site.clone(), url: url.clone() },
        _ if lower.contains("knmi") => ResolutionSourceKind::Knmi { description: "KNMI official observations".into() },
        _ => ResolutionSourceKind::Unrecognized { description: "no known resolution source found".into() },
    };

    // Fallback source.
    let fallback = if matches!(source, ResolutionSourceKind::NoaaWrhTimeseries { .. })
        && (lower.contains("wunderground") || lower.contains("weather underground"))
        && (lower.contains("unavailable") || lower.contains("not available"))
    {
        Some(ResolutionSourceKind::WundergroundDaily { url: wu.clone().unwrap_or_default() })
    } else {
        None
    };

    // Observation filter candidates.
    let hourly = lower.contains("show hourly data") || lower.contains("hourly data");
    let filters = if hourly {
        notes.push("rules reference WRH hourly data: candidate minute windows :51-:59 (NWS/FAA) and :56-:04 (other platforms)".into());
        vec![ObservationFilter::WRH_HOURLY_NWS_FAA, ObservationFilter::WRH_HOURLY_OTHER]
    } else if matches!(source, ResolutionSourceKind::NoaaWrhTimeseries { .. }) {
        notes.push("no hourly clause: all rows count, but WRH display semantics are unverified (Phase 0)".into());
        vec![ObservationFilter::AllRows]
    } else {
        vec![ObservationFilter::AllRows]
    };

    let revision_policy = if lower.contains("first datapoint for the following date") || lower.contains("first data point for the following date") {
        RevisionPolicy::UntilFirstDatapointOfNextDay
    } else if lower.contains("finalized") {
        RevisionPolicy::UntilFinalized
    } else {
        RevisionPolicy::Unknown
    };

    // Clause audit.
    let recognized_markers = [
        "resolution source", "resolve", "whole degree", "revisions", "hourly data", "unavailable", "not available", "temp\" column",
        "temp column", "available here", "finalized", "11:59", "measures temperatures", "level of precision", "http", "highest temperature",
        "lowest temperature", "highest reading", "lowest reading",
    ];
    for s in sentences(&rules.text) {
        let l = s.to_lowercase();
        let risky = is_risky(&l);
        let known = recognized_markers.iter().any(|m| l.contains(m));
        if risky {
            unrecognized.push(s.clone());
        } else if !known && s.len() > 30 {
            notes.push(format!("unclassified sentence: {s}"));
        }
    }
    if matches!(source, ResolutionSourceKind::Unrecognized { .. }) {
        unrecognized.push("resolution source not recognized".into());
    }
    notes.push(format!("day time zone taken from location configuration ({location_tz}); verify against the source display"));

    ResolutionSpec {
        source,
        fallback,
        extreme,
        unit,
        whole_degrees,
        day_timezone: location_tz,
        filters,
        filter_certainty: FilterCertainty::Unconfirmed,
        revision_policy,
        rules_sha256: rules.sha256.clone(),
        parser_version: RULES_PARSER_VERSION,
        review: SpecReviewStatus::AutoParsed,
        unrecognized_clauses: unrecognized,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Europe::Amsterdam;

    /// Paraphrase of the Amsterdam rules reported by public sources (2026-09);
    /// the exact text is captured from Gamma at runtime.
    const AMSTERDAM_NOAA: &str = "This market will resolve to the temperature range that contains the highest temperature recorded by NOAA at the Amsterdam Airport Schiphol Station in degrees Celsius on 25 Sep '26. The resolution source for this market will be information from NOAA, specifically the highest reading under the \"Temp\" column for all times on the specified day, available here: https://www.weather.gov/wrh/timeseries?site=eham. This market resolves off of the Hourly Data provided using the \"Show Hourly Data\" button. The resolution source for this market measures temperatures to whole degrees Celsius (eg, 9°C), which is the level of precision that will be used when resolving the market. Revisions to temperatures recorded within the market's timeframe will be considered until the first datapoint for the following date has been published, after which any alterations will not be considered. If NOAA data for the observation date is unavailable by 11:59 PM ET on the day following the observation date, the Weather Underground Daily Observations table will be used as the resolution source.";

    fn spec(text: &str) -> ResolutionSpec {
        parse_resolution_spec(&RulesText::new(text, None), Amsterdam, TempUnit::Celsius)
    }

    #[test]
    fn amsterdam_noaa_rules() {
        let s = spec(AMSTERDAM_NOAA);
        match &s.source {
            ResolutionSourceKind::NoaaWrhTimeseries { site, url } => {
                assert_eq!(site, "EHAM");
                assert!(url.contains("weather.gov/wrh/timeseries?site=eham"), "{url}");
            }
            other => panic!("unexpected source {other:?}"),
        }
        assert!(matches!(s.fallback, Some(ResolutionSourceKind::WundergroundDaily { .. })));
        assert_eq!(s.extreme, MarketExtreme::DailyMax);
        assert_eq!(s.unit, TempUnit::Celsius);
        assert!(s.whole_degrees);
        assert_eq!(s.filters, vec![ObservationFilter::WRH_HOURLY_NWS_FAA, ObservationFilter::WRH_HOURLY_OTHER]);
        assert_eq!(s.filter_certainty, FilterCertainty::Unconfirmed);
        assert_eq!(s.revision_policy, RevisionPolicy::UntilFirstDatapointOfNextDay);
        assert!(s.unrecognized_clauses.is_empty(), "{:?}", s.unrecognized_clauses);
        assert!(s.is_machine_tradable());
    }

    #[test]
    fn wunderground_rules() {
        let t = "This market will resolve to the temperature range that contains the highest temperature recorded at the Amsterdam Airport Schiphol Station in degrees Celsius on 12 May '26. The resolution source for this market will be information from Wunderground, specifically the highest temperature recorded for all times on this day by the Forecast for the Amsterdam Airport Schiphol Station once information is finalized, available here: https://www.wunderground.com/history/daily/nl/amsterdam/EHAM.";
        let s = spec(t);
        assert!(matches!(s.source, ResolutionSourceKind::WundergroundDaily { .. }), "{:?}", s.source);
        assert_eq!(s.revision_policy, RevisionPolicy::UntilFinalized);
        assert!(!s.whole_degrees);
    }

    #[test]
    fn risky_clauses_block_auto_trading() {
        let t = format!("{AMSTERDAM_NOAA} Temperatures will be rounded to the nearest tenth before resolution.");
        let s = spec(&t);
        assert_eq!(s.unrecognized_clauses.len(), 1);
        assert!(!s.is_machine_tradable());
        let t = format!("{AMSTERDAM_NOAA} If the station changes, this market will resolve 50-50.");
        assert!(!spec(&t).is_machine_tradable());
        let t = format!("{AMSTERDAM_NOAA} Only METAR reports issued at the top of the hour count.");
        assert!(!spec(&t).is_machine_tradable());
    }

    #[test]
    fn unknown_source_is_not_tradable() {
        let s = spec("Resolves according to my cousin's thermometer.");
        assert!(matches!(s.source, ResolutionSourceKind::Unrecognized { .. }));
        assert!(!s.is_machine_tradable());
    }

    #[test]
    fn lowest_temperature_markets() {
        let t = AMSTERDAM_NOAA.replace("highest temperature", "lowest temperature").replace("highest reading", "lowest reading");
        assert_eq!(spec(&t).extreme, MarketExtreme::DailyMin);
    }

    #[test]
    fn risk_words_match_whole_words_only() {
        assert!(!is_risky("the weather underground daily observations table"));
        assert!(!is_risky("the specified day"));
        assert!(is_risky("values are rounded"));
        assert!(is_risky("only speci reports"));
        assert!(is_risky("times are in utc"));
        assert!(is_risky("resolves 50-50"));
    }

    #[test]
    fn sentence_splitter_keeps_urls_and_decimals() {
        let s = sentences("See https://www.weather.gov/wrh/timeseries?site=eham. Values like 0.5 matter. Done");
        assert_eq!(s.len(), 3);
        assert!(s[0].contains("weather.gov"));
        assert!(s[1].contains("0.5"));
    }
}
