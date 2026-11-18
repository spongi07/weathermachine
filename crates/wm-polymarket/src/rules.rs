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
    FilterCertainty, ObservationFilter, ResolutionSourceKind, ResolutionSpec, RevisionPolicy,
    RulesText, SpecReviewStatus,
};

/// Bump when parsing semantics change. 2: the AWC METAR rules (Polymarket's
/// from the market of 23 October 2026 on).
pub const RULES_PARSER_VERSION: u16 = 2;

/// Word prefixes that signal decision-relevant semantics (matched per word).
const RISK_WORD_PREFIXES: &[&str] = &[
    "round", "decimal", "utc", "gmt", "timezone", "exclud", "except", "averag", "metar", "sensor",
    "void", "cancel", "tenth",
];
/// Exact risk words.
const RISK_WORDS: &[&str] = &["mean", "speci", "specis"];
/// Risk phrases (matched on the lower-cased sentence).
const RISK_PHRASES: &[&str] = &[
    "time zone",
    "local time",
    "different station",
    "station change",
    "50-50",
    "50/50",
];

fn is_risky(lower_sentence: &str) -> bool {
    let words = lower_sentence
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty());
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
    let site: String = rest[site_idx + 5..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if site.is_empty() {
        return None;
    }
    let url_end = rest
        .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
        .unwrap_or(rest.len());
    let url = format!(
        "https://www.{}",
        rest[..url_end].trim_end_matches(['.', ','])
    );
    Some((site.to_ascii_uppercase(), url))
}

/// The AWC METAR source in `text`: the station of `ids=` (upper case) and
/// the URL as written.
fn awc_metar(text: &str) -> Option<(String, String)> {
    // ASCII lower-casing keeps every byte where it was: the URL is cut from
    // the text as written.
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("https://aviationweather.gov/api/data/metar")?;
    let end = lower[idx..]
        .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
        .map_or(lower.len(), |e| idx + e);
    let url = text[idx..end].trim_end_matches(['.', ',']).to_owned();
    let ids = lower[idx..end].find("ids=")? + idx + 4;
    let station: String = lower[ids..end]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (!station.is_empty()).then(|| (station.to_ascii_uppercase(), url))
}

/// Polymarket's rules on the AWC METARs (Amsterdam's from the market of 23
/// October 2026 on), sentence by sentence in [`clause_form`]. A `{…}` part
/// stands for the station as the rules name it, the market's date, the
/// station's code and a link, each checked in [`placeholder_fits`]. A
/// market on these rules is machine-tradable only when its text holds every
/// one of them and no other sentence the audit finds risky (the erroneous-data
/// paragraph, Polymarket's boilerplate, is not among them). What follows
/// from them is what the engine does: routine reports only (no SPECI), the
/// T-group's tenths, else the body's whole degrees, rounded half up, on
/// the station's local day.
const AWC_METAR_CLAUSES: &[&str] = &[
    "this market will resolve to the temperature range that contains the highest temperature reported at {station} on {date}.",
    "the qualifying value is the highest temperature reported in the hourly temperature remark of any routine (metar) observation for {station} on that date, in tenths of a degree celsius as reported.",
    "a report whose line begins with speci is a special observation and does not count.",
    "every other report is routine, including one issued off the nominal hour and one that carries no metar or speci label.",
    "for how to read the metar report, see: {link}",
    "the hourly temperature remark is the group beginning with the letter t followed by exactly eight digits.",
    "after the t, one digit for sign (0 positive, 1 negative) and three for tenths of a degree celsius give the temperature; the following four are the dew point and are not used.",
    "t01390133 is 13.9°c; t10171022 is \u{2212}1.7°c.",
    "if a routine observation does not carry the hourly temperature remark, the temperature group in the body of that observation governs for that observation, in whole degrees celsius as reported.",
    "it appears as temperature and dew point separated by a slash, with m marking a negative value: 18/12 is 18°c; m11/m17 is \u{2212}11°c.",
    "an observation that reports no temperature in either place is disregarded.",
    "each market in this event states its range in whole degrees celsius.",
    "the qualifying value is rounded to the nearest whole degree, with a value ending in exactly half rounded up to the next higher whole degree, and that whole celsius value determines which range contains it.",
    "for example, 13.9°c counts as 14°c; 17.4°c counts as 17°c; 16.5°c counts as 17°c; \u{2212}1.7°c counts as \u{2212}2°c; \u{2212}8.5°c counts as \u{2212}8°c.",
    "an observation counts if its time stamp, converted to the station's local time (including any daylight-saving shift in effect), falls on the observation date.",
    "this market will resolve at the issuance of the first metar observation for the station on the day following the observation date or by 11:59 pm et on the day following the observation date, whichever comes first.",
    "resolution is determined from the observations the resolution source shows for the observation date.",
    "in the event that the aviation weather center has published no routine observation reporting a temperature for the observation date by 11:59 pm et on the day following the observation date, the weather underground daily observations table will be used as the resolution source.",
    "in the event that neither source has published data sufficient to determine a temperature for the observation date by 11:59 pm et on the day following the observation date, this market will resolve to the lowest bracket.",
    "the report the resolution source shows for a given observation time is the one that counts, whether or not it is marked as a correction (cor).",
    "changes to observations within this market's timeframe will be considered until the first datapoint for the following date has been published, after which any alterations will not be considered.",
    "where the first routine observation for the following date is published before that deadline, resolution is determined from a read taken after it.",
    "where it is not, resolution is determined from a read taken at that deadline.",
    "the resolution source for this market will be the routine observations for {station} published here: https://aviationweather.gov/api/data/metar?ids={icao}&format=raw&date={date}&hours=30",
];

/// A sentence as [`AWC_METAR_CLAUSES`] holds it: lower case, whitespace
/// collapsed.
fn clause_form(sentence: &str) -> String {
    sentence
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` against `pattern`, whose `{name}` parts each match some text:
/// `(name, text)` per part, in order, or `None`.
fn glob<'p, 't>(pattern: &'p str, text: &'t str) -> Option<Vec<(&'p str, &'t str)>> {
    let mut literals = Vec::new();
    let mut names = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let close = open + rest[open..].find('}')?;
        literals.push(&rest[..open]);
        names.push(&rest[open + 1..close]);
        rest = &rest[close + 1..];
    }
    literals.push(rest);
    let mut t = text.strip_prefix(literals[0])?;
    let mut parts = Vec::new();
    for (k, name) in names.iter().enumerate() {
        let next = literals[k + 1];
        let at = if k + 1 == names.len() {
            t.strip_suffix(next)?.len()
        } else {
            // The shortest part that is not empty.
            let from = t.chars().next()?.len_utf8();
            from + t[from..].find(next)?
        };
        if at == 0 {
            return None;
        }
        parts.push((*name, &t[..at]));
        t = &t[at + next.len()..];
    }
    t.is_empty().then_some(parts)
}

/// Whether `part` is what a `{name}` of [`AWC_METAR_CLAUSES`] may stand for:
/// a station (`amsterdam airport schiphol (eham)`), its code, a date
/// (`19 nov '26`, `2026-11-20t02:00:00z`) or a link; nothing that could hide
/// a clause.
fn placeholder_fits(name: &str, part: &str) -> bool {
    let code =
        |c: &str| (3..=4).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_alphanumeric());
    match name {
        "station" => {
            part.len() <= 80
                && part
                    .strip_suffix(')')
                    .and_then(|p| p.rsplit_once(" ("))
                    .is_some_and(|(n, c)| {
                        code(c)
                            && n.chars()
                                .all(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '\''))
                    })
        }
        "icao" => code(part),
        "date" => {
            let b = part.as_bytes();
            let ddmmmyy = part.split(' ').collect::<Vec<_>>();
            let short = ddmmmyy.len() == 3
                && (1..=2).contains(&ddmmmyy[0].len())
                && ddmmmyy[0].bytes().all(|c| c.is_ascii_digit())
                && ddmmmyy[1].len() == 3
                && ddmmmyy[1].bytes().all(|c| c.is_ascii_lowercase())
                && ddmmmyy[2].len() == 3
                && ddmmmyy[2].starts_with('\'')
                && ddmmmyy[2][1..].bytes().all(|c| c.is_ascii_digit());
            let iso = b.len() == 20
                && b.iter().enumerate().all(|(i, c)| match i {
                    4 | 7 => *c == b'-',
                    10 => *c == b't',
                    13 | 16 => *c == b':',
                    19 => *c == b'z',
                    _ => c.is_ascii_digit(),
                });
            short || iso
        }
        "link" => part.starts_with("https://") && !part.contains(char::is_whitespace),
        _ => false,
    }
}

/// The clause audit of a text on the AWC METAR rules: the sentences it
/// does not know (risky ones; the others are noted), each of the rules'
/// clauses it lacks, and a station that is not the source's.
fn awc_metar_audit(text: &str, station: &str) -> (Vec<String>, Vec<String>) {
    let mut unrecognized = Vec::new();
    let mut notes = Vec::new();
    let mut seen = vec![false; AWC_METAR_CLAUSES.len()];
    let mut stations = std::collections::BTreeSet::new();
    for s in sentences(text) {
        let form = clause_form(&s);
        let matched = AWC_METAR_CLAUSES.iter().position(|c| {
            glob(c, &form).is_some_and(|parts| {
                parts.iter().all(|(n, p)| placeholder_fits(n, p)) && {
                    for (n, p) in &parts {
                        match *n {
                            "station" => {
                                stations.insert(
                                    p.rsplit_once(" (")
                                        .map_or("", |(_, c)| c.trim_end_matches(')'))
                                        .to_ascii_uppercase(),
                                );
                            }
                            "icao" => {
                                stations.insert(p.to_ascii_uppercase());
                            }
                            _ => {}
                        }
                    }
                    true
                }
            })
        });
        match matched {
            Some(k) => seen[k] = true,
            None if is_risky(&form) => unrecognized.push(s),
            None if s.len() > 30 => notes.push(format!("unclassified sentence: {s}")),
            None => {}
        }
    }
    for (k, c) in AWC_METAR_CLAUSES.iter().enumerate() {
        if !seen[k] {
            unrecognized.push(format!("missing a clause of the AWC METAR rules: {c}"));
        }
    }
    if stations.iter().any(|s| s != station) {
        unrecognized.push(format!(
            "the rules name another station ({}) than their source ({station})",
            stations.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    (unrecognized, notes)
}

fn wunderground_url(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let idx = lower.find("wunderground.com/history")?;
    let rest = &text[idx..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ')' || c == '"')
        .unwrap_or(rest.len());
    Some(format!(
        "https://www.{}",
        rest[..end].trim_end_matches(['.', ','])
    ))
}

/// The source of rules that are not on the AWC METARs.
fn noaa_or_wunderground(
    lower: &str,
    noaa_primary: bool,
    wrh: Option<&(String, String)>,
    wu: Option<&String>,
) -> ResolutionSourceKind {
    match (wrh, wu) {
        (Some((site, url)), _) if noaa_primary || wu.is_none() => ResolutionSourceKind::NoaaWrhTimeseries { site: site.clone(), url: url.clone() },
        (_, Some(url)) if lower.contains("information from wunderground") || lower.contains("resolution source for this market will be information from weather underground") || wrh.is_none() => {
            ResolutionSourceKind::WundergroundDaily { url: url.clone() }
        }
        (Some((site, url)), _) => ResolutionSourceKind::NoaaWrhTimeseries { site: site.clone(), url: url.clone() },
        _ if lower.contains("knmi") => ResolutionSourceKind::Knmi { description: "KNMI official observations".into() },
        _ => ResolutionSourceKind::Unrecognized { description: "no known resolution source found".into() },
    }
}

/// Parse market rules into a resolution spec.
pub fn parse_resolution_spec(
    rules: &RulesText,
    location_tz: Tz,
    default_unit: TempUnit,
) -> ResolutionSpec {
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
    // The market's source field first: the station the text names must be
    // the one it settles on.
    let awc = rules
        .resolution_source_url
        .as_deref()
        .and_then(awc_metar)
        .or_else(|| awc_metar(&rules.text));
    let noaa_primary = wrh.is_some()
        && (lower.contains("information from noaa")
            || lower.contains("recorded by noaa")
            || lower.contains("from noaa"));
    let source = match (&awc, &wrh, &wu) {
        (Some((station, url)), ..) if lower.contains("routine observations for") => {
            ResolutionSourceKind::AwcMetar {
                station: station.clone(),
                url: url.clone(),
            }
        }
        (_, wrh, wu) => noaa_or_wunderground(&lower, noaa_primary, wrh.as_ref(), wu.as_ref()),
    };

    // Fallback source.
    let wunderground_named =
        lower.contains("wunderground") || lower.contains("weather underground");
    let fallback = if (matches!(source, ResolutionSourceKind::NoaaWrhTimeseries { .. })
        && wunderground_named
        && (lower.contains("unavailable") || lower.contains("not available")))
        || (matches!(source, ResolutionSourceKind::AwcMetar { .. })
            && wunderground_named
            && lower.contains("has published no routine observation"))
    {
        Some(ResolutionSourceKind::WundergroundDaily {
            url: wu.clone().unwrap_or_default(),
        })
    } else {
        None
    };

    // Observation filter candidates.
    let hourly = lower.contains("show hourly data") || lower.contains("hourly data");
    let filters = if matches!(source, ResolutionSourceKind::AwcMetar { .. }) {
        notes.push("AWC METAR rules: routine reports only (SPECI do not count), the T-group's tenths else the body's whole degrees, rounded half up".into());
        vec![ObservationFilter::RoutineOnly]
    } else if hourly {
        notes.push("rules reference WRH hourly data: candidate minute windows :51-:59 (NWS/FAA) and :56-:04 (other platforms)".into());
        vec![
            ObservationFilter::WRH_HOURLY_NWS_FAA,
            ObservationFilter::WRH_HOURLY_OTHER,
        ]
    } else if matches!(source, ResolutionSourceKind::NoaaWrhTimeseries { .. }) {
        notes.push(
            "no hourly clause: all rows count, but WRH display semantics are unverified (Phase 0)"
                .into(),
        );
        vec![ObservationFilter::AllRows]
    } else {
        vec![ObservationFilter::AllRows]
    };

    let revision_policy = if lower.contains("first datapoint for the following date")
        || lower.contains("first data point for the following date")
    {
        RevisionPolicy::UntilFirstDatapointOfNextDay
    } else if lower.contains("finalized") {
        RevisionPolicy::UntilFinalized
    } else {
        RevisionPolicy::Unknown
    };

    // Clause audit.
    let recognized_markers = [
        "resolution source",
        "resolve",
        "whole degree",
        "revisions",
        "hourly data",
        "unavailable",
        "not available",
        "temp\" column",
        "temp column",
        "available here",
        "finalized",
        "11:59",
        "measures temperatures",
        "level of precision",
        "http",
        "highest temperature",
        "lowest temperature",
        "highest reading",
        "lowest reading",
    ];
    if let ResolutionSourceKind::AwcMetar { station, .. } = &source {
        let (u, n) = awc_metar_audit(&rules.text, station);
        unrecognized.extend(u);
        notes.extend(n);
    } else {
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
                assert!(
                    url.contains("weather.gov/wrh/timeseries?site=eham"),
                    "{url}"
                );
            }
            other => panic!("unexpected source {other:?}"),
        }
        assert!(matches!(
            s.fallback,
            Some(ResolutionSourceKind::WundergroundDaily { .. })
        ));
        assert_eq!(s.extreme, MarketExtreme::DailyMax);
        assert_eq!(s.unit, TempUnit::Celsius);
        assert!(s.whole_degrees);
        assert_eq!(
            s.filters,
            vec![
                ObservationFilter::WRH_HOURLY_NWS_FAA,
                ObservationFilter::WRH_HOURLY_OTHER
            ]
        );
        assert_eq!(s.filter_certainty, FilterCertainty::Unconfirmed);
        assert_eq!(
            s.revision_policy,
            RevisionPolicy::UntilFirstDatapointOfNextDay
        );
        assert!(
            s.unrecognized_clauses.is_empty(),
            "{:?}",
            s.unrecognized_clauses
        );
        assert!(s.is_machine_tradable());
    }

    #[test]
    fn wunderground_rules() {
        let t = "This market will resolve to the temperature range that contains the highest temperature recorded at the Amsterdam Airport Schiphol Station in degrees Celsius on 12 May '26. The resolution source for this market will be information from Wunderground, specifically the highest temperature recorded for all times on this day by the Forecast for the Amsterdam Airport Schiphol Station once information is finalized, available here: https://www.wunderground.com/history/daily/nl/amsterdam/EHAM.";
        let s = spec(t);
        assert!(
            matches!(s.source, ResolutionSourceKind::WundergroundDaily { .. }),
            "{:?}",
            s.source
        );
        assert_eq!(s.revision_policy, RevisionPolicy::UntilFinalized);
        assert!(!s.whole_degrees);
    }

    #[test]
    fn risky_clauses_block_auto_trading() {
        let t = format!(
            "{AMSTERDAM_NOAA} Temperatures will be rounded to the nearest tenth before resolution."
        );
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
        assert!(matches!(
            s.source,
            ResolutionSourceKind::Unrecognized { .. }
        ));
        assert!(!s.is_machine_tradable());
    }

    #[test]
    fn lowest_temperature_markets() {
        let t = AMSTERDAM_NOAA
            .replace("highest temperature", "lowest temperature")
            .replace("highest reading", "lowest reading");
        assert_eq!(spec(&t).extreme, MarketExtreme::DailyMin);
    }

    #[test]
    fn risk_words_match_whole_words_only() {
        assert!(!is_risky(
            "the weather underground daily observations table"
        ));
        assert!(!is_risky("the specified day"));
        assert!(is_risky("values are rounded"));
        assert!(is_risky("only speci reports"));
        assert!(is_risky("times are in utc"));
        assert!(is_risky("resolves 50-50"));
    }

    /// Amsterdam's rules of 19 November 2026, as stored (Polymarket's from
    /// the market of 23 October on), and their source URL.
    const AWC_RULES: &str = include_str!("fixtures/rules_eham_awc_metar.txt");
    const AWC_URL: &str = "https://aviationweather.gov/api/data/metar?ids=EHAM&format=raw&date=2026-11-20T02:00:00Z&hours=30";

    fn awc_spec(text: &str, url: &str) -> ResolutionSpec {
        parse_resolution_spec(
            &RulesText::new(text, Some(url.to_owned())),
            Amsterdam,
            TempUnit::Celsius,
        )
    }

    /// From 23 October 2026 every market's rules were "unrecognized": the
    /// risk check refused every order, paper and live, for four weeks. The
    /// AWC METAR rules are machine-tradable, and say what the engine
    /// counts: routine reports only, whole degrees, until the next day's
    /// first report, Weather Underground as the fallback.
    #[test]
    fn the_awc_metar_rules_are_machine_tradable() {
        let s = awc_spec(AWC_RULES, AWC_URL);
        assert_eq!(
            s.source,
            ResolutionSourceKind::AwcMetar {
                station: "EHAM".into(),
                url: AWC_URL.into(),
            }
        );
        assert!(
            s.unrecognized_clauses.is_empty(),
            "{:#?}",
            s.unrecognized_clauses
        );
        assert!(s.is_machine_tradable());
        assert_eq!(s.filters, vec![ObservationFilter::RoutineOnly]);
        assert_eq!(s.extreme, MarketExtreme::DailyMax);
        assert_eq!(s.unit, TempUnit::Celsius);
        assert!(s.whole_degrees);
        assert_eq!(
            s.revision_policy,
            RevisionPolicy::UntilFirstDatapointOfNextDay
        );
        assert!(matches!(
            s.fallback,
            Some(ResolutionSourceKind::WundergroundDaily { .. })
        ));
        assert_eq!(s.parser_version, 2);
        // Every day's text names its own date, in the text and the URL.
        let dec = AWC_RULES
            .replace("on 19 Nov '26.", "on 3 Dec '26.")
            .replace("2026-11-20T02:00:00Z", "2026-12-04T02:00:00Z");
        let url = AWC_URL.replace("2026-11-20T02:00:00Z", "2026-12-04T02:00:00Z");
        let s = awc_spec(&dec, &url);
        assert!(s.is_machine_tradable(), "{:#?}", s.unrecognized_clauses);
    }

    /// Only these rules, word for word: a clause that changes, goes or is
    /// added makes the market untradable again until it is reviewed, and so
    /// does another station.
    #[test]
    fn the_awc_metar_rules_change_nothing_unseen() {
        let blocked = |text: &str, url: &str| {
            let s = awc_spec(text, url);
            assert!(!s.is_machine_tradable(), "{text}");
            s.unrecognized_clauses
        };
        // A changed clause: SPECI counts after all.
        let u = blocked(
            &AWC_RULES.replace(
                "is a special observation and does not count.",
                "is a special observation and counts as well.",
            ),
            AWC_URL,
        );
        assert!(
            u.iter()
                .any(|c| c.contains("SPECI is a special observation and counts")),
            "{u:#?}"
        );
        assert!(
            u.iter().any(|c| c.starts_with("missing a clause")),
            "{u:#?}"
        );
        // A clause gone.
        let u = blocked(
            &AWC_RULES.replace(
                "A report whose line begins with SPECI is a special observation and does not count. ",
                "",
            ),
            AWC_URL,
        );
        assert_eq!(u.len(), 1, "{u:#?}");
        assert!(u[0].contains("begins with speci"), "{u:#?}");
        // A risky clause added, or the rounding changed.
        blocked(
            &format!("{AWC_RULES}\nTemperatures are truncated to the tenth below."),
            AWC_URL,
        );
        blocked(
            &AWC_RULES.replace(
                "rounded up to the next higher",
                "rounded down to the next lower",
            ),
            AWC_URL,
        );
        // A date that hides a clause.
        blocked(
            &AWC_RULES.replace("on 19 Nov '26.", "on 19 Nov '26 in UTC."),
            AWC_URL,
        );
        // Another station in the source URL, or in the rules.
        let u = blocked(AWC_RULES, &AWC_URL.replace("ids=EHAM", "ids=EGLL"));
        assert!(u.iter().any(|c| c.contains("another station")), "{u:#?}");
        blocked(
            &AWC_RULES.replace(
                "routine (METAR) observation for Amsterdam Airport Schiphol (EHAM)",
                "routine (METAR) observation for Rotterdam The Hague Airport (EHRD)",
            ),
            AWC_URL,
        );
        // Polymarket's boilerplate on erroneous data may change: noted only.
        let s = awc_spec(
            &AWC_RULES.replace("seven (7) calendar days", "ten (10) calendar days"),
            AWC_URL,
        );
        assert!(s.is_machine_tradable(), "{:#?}", s.unrecognized_clauses);
        assert!(s.notes.iter().any(|n| n.contains("ten (10) calendar days")));
    }

    #[test]
    fn placeholders_take_only_what_they_stand_for() {
        assert_eq!(
            glob("at {station} on {date}.", "at a b (eham) on 19 nov '26."),
            Some(vec![("station", "a b (eham)"), ("date", "19 nov '26")])
        );
        assert_eq!(
            glob("x {link}", "x https://a.b/c"),
            Some(vec![("link", "https://a.b/c")])
        );
        assert_eq!(glob("exact.", "exact."), Some(vec![]));
        assert_eq!(glob("exact.", "exact. more"), None);
        assert_eq!(glob("a {x} b", "a   b"), Some(vec![("x", " ")]));
        assert_eq!(glob("a {x} b", "a  b"), None);
        assert_eq!(glob("a {x}b", "a b"), None, "nothing is not a part");
        assert!(placeholder_fits("date", "19 nov '26"));
        assert!(placeholder_fits("date", "2026-11-20t02:00:00z"));
        assert!(!placeholder_fits("date", "19 nov '26 in utc"));
        assert!(placeholder_fits(
            "station",
            "amsterdam airport schiphol (eham)"
        ));
        assert!(!placeholder_fits("station", "eham, rounded down (eham)"));
        assert!(placeholder_fits("icao", "eham"));
        assert!(!placeholder_fits("icao", "eham&x"));
        assert!(!placeholder_fits("link", "https://a b"));
        assert_eq!(
            awc_metar("see https://aviationweather.gov/api/data/metar?ids=EHAM&hours=30."),
            Some((
                "EHAM".into(),
                "https://aviationweather.gov/api/data/metar?ids=EHAM&hours=30".into()
            ))
        );
        assert_eq!(
            awc_metar("https://www.weather.gov/wrh/timeseries?site=eham"),
            None
        );
    }

    #[test]
    fn sentence_splitter_keeps_urls_and_decimals() {
        let s = sentences(
            "See https://www.weather.gov/wrh/timeseries?site=eham. Values like 0.5 matter. Done",
        );
        assert_eq!(s.len(), 3);
        assert!(s[0].contains("weather.gov"));
        assert!(s[1].contains("0.5"));
    }
}
