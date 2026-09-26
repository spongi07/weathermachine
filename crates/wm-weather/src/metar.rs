//! Minimal, exact METAR/SPECI parser for the fields Weather Machine needs.
//!
//! We deliberately implement this ourselves (≈300 lines, property-tested)
//! rather than depend on a third-party decoder: the temperature group is the
//! single most important input of the system and must be parsed exactly.
//!
//! Supported: report type prefix, station, `DDHHMMZ`, `AUTO`/`COR`/`NIL`,
//! the temperature/dew-point group (`18/12`, `M02/M05`, `05/`, `/////`),
//! QNH/altimeter, and the US remark `T`-group with tenths (`T01780122`).

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use wm_core::units::TempC;
use wm_core::weather::ReportType;

/// Bump when parsing semantics change (stored with every observation).
pub const PARSER_VERSION: u16 = 1;

/// Parse failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetarError {
    #[error("empty report")]
    Empty,
    #[error("missing station identifier")]
    MissingStation,
    #[error("missing or invalid DDHHMMZ group")]
    MissingTime,
}

/// Parsed report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetarReport {
    pub report_type: ReportType,
    /// Whether the report type was explicit in the text.
    pub report_type_explicit: bool,
    pub station: String,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub auto: bool,
    pub cor: bool,
    pub nil: bool,
    /// Whole-degree temperature from the main body.
    pub temperature_whole: Option<i32>,
    pub dewpoint_whole: Option<i32>,
    /// Tenths from the US remark T-group, when present.
    pub temperature_tenths: Option<i32>,
    pub dewpoint_tenths: Option<i32>,
    pub qnh_hpa: Option<u16>,
    pub altimeter_inhg_hundredths: Option<u16>,
    /// Canonical text: prefix removed, whitespace collapsed, trailing `=` dropped.
    pub canonical: String,
}

impl MetarReport {
    /// Best available temperature: T-group tenths if present, else whole degrees.
    pub fn temperature(&self) -> Option<(TempC, wm_core::weather::TempPrecision)> {
        use wm_core::weather::TempPrecision;
        if let Some(t) = self.temperature_tenths {
            return Some((TempC::from_tenths(t), TempPrecision::Tenth));
        }
        self.temperature_whole.map(|t| (TempC::from_whole(t), TempPrecision::WholeDegree))
    }

    pub fn dewpoint(&self) -> Option<TempC> {
        self.dewpoint_tenths
            .map(TempC::from_tenths)
            .or_else(|| self.dewpoint_whole.map(TempC::from_whole))
    }

    /// Resolve `DDHHMM` against a reference instant (normally the provider's
    /// timestamp or our fetch time). Picks the most recent matching instant not
    /// more than one day after the reference, handling month rollovers.
    pub fn observed_at(&self, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
        resolve_day_time(self.day, self.hour, self.minute, reference)
    }
}

/// Resolve a METAR day/hour/minute to a full UTC timestamp near `reference`.
pub fn resolve_day_time(day: u8, hour: u8, minute: u8, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let (mut year, mut month) = (reference.year(), reference.month());
    // Try this month, then previous months (a day like 31 may not exist in the
    // immediately preceding month), then the next month for small skews.
    let mut best: Option<DateTime<Utc>> = None;
    for step in 0..3 {
        if step > 0 {
            if month == 1 {
                month = 12;
                year -= 1;
            } else {
                month -= 1;
            }
        }
        if let Some(t) = make(year, month, day, hour, minute)
            && t <= reference + Duration::days(1)
        {
            best = Some(t);
            break;
        }
    }
    if let Some(t) = best
        && reference - t > Duration::days(25)
    {
        // Probably next month with a skewed reference (e.g. reference 30th 23:59, report on the 1st).
        let (ny, nm) = if reference.month() == 12 { (reference.year() + 1, 1) } else { (reference.year(), reference.month() + 1) };
        if let Some(n) = make(ny, nm, day, hour, minute)
            && n - reference <= Duration::days(1)
        {
            return Some(n);
        }
    }
    best
}

fn make(year: i32, month: u32, day: u8, hour: u8, minute: u8) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year, month, u32::from(day))?;
    let naive = date.and_hms_opt(u32::from(hour), u32::from(minute), 0)?;
    Utc.from_local_datetime(&naive).single()
}

fn parse_signed_two_digits(s: &str) -> Option<i32> {
    let (neg, digits) = match s.strip_prefix('M') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: i32 = digits.parse().ok()?;
    Some(if neg { -v } else { v })
}

/// Temperature group `TT/DD`. Returns `Some((temp, dew))` if the token *is* a
/// temperature group (either side may be missing), `None` otherwise.
fn parse_temp_group(token: &str) -> Option<(Option<i32>, Option<i32>)> {
    // Both values missing is encoded as exactly five slashes. Other all-slash
    // tokens are missing visibility (`////`), cloud (`//////`) or weather (`//`).
    if token == "/////" {
        return Some((None, None));
    }
    let (temp, rest) = if let Some(rest) = token.strip_prefix("//") {
        (None, rest)
    } else {
        let (neg, body) = match token.strip_prefix('M') {
            Some(b) => (true, b),
            None => (false, token),
        };
        let digits = body.get(..2)?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: i32 = digits.parse().ok()?;
        (Some(if neg { -v } else { v }), &body[2..])
    };
    let rest = rest.strip_prefix('/')?;
    let dew = match rest {
        "" | "//" => None,
        _ => Some(parse_signed_two_digits(rest)?),
    };
    if temp.is_none() && dew.is_none() {
        return None;
    }
    Some((temp, dew))
}

/// US remark `TsnnnSnnn` (temperature and dew point in tenths).
fn parse_t_group(token: &str) -> Option<(i32, Option<i32>)> {
    let b = token.as_bytes();
    if !(b.len() == 5 || b.len() == 9) || b[0] != b'T' || !b[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let sign = |c: u8| -> Option<i32> {
        match c {
            b'0' => Some(1),
            b'1' => Some(-1),
            _ => None,
        }
    };
    let num = |s: &[u8]| -> Option<i32> { std::str::from_utf8(s).ok()?.parse().ok() };
    let t = sign(b[1])? * num(&b[2..5])?;
    let d = if b.len() == 9 { Some(sign(b[5])? * num(&b[6..9])?) } else { None };
    Some((t, d))
}

fn parse_day_time(token: &str) -> Option<(u8, u8, u8)> {
    let b = token.as_bytes();
    if b.len() != 7 || b[6] != b'Z' || !b[..6].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = |i: usize| (b[i] - b'0') * 10 + (b[i + 1] - b'0');
    let (d, h, m) = (n(0), n(2), n(4));
    ((1..=31).contains(&d) && h <= 23 && m <= 59).then_some((d, h, m))
}

fn is_station(token: &str) -> bool {
    token.len() == 4
        && token.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && token.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Canonical text used for hashing: prefix removed, whitespace collapsed,
/// trailing `=` removed. Two relays of the same report hash identically.
pub fn canonicalize(raw: &str) -> String {
    let normalized = wm_core::hash::normalize_report_text(raw);
    let mut tokens: Vec<&str> = normalized.split(' ').collect();
    if matches!(tokens.first(), Some(&"METAR") | Some(&"SPECI")) {
        tokens.remove(0);
    }
    tokens.join(" ")
}

/// Parse a METAR/SPECI report.
pub fn parse_metar(raw: &str) -> Result<MetarReport, MetarError> {
    let normalized = wm_core::hash::normalize_report_text(raw);
    if normalized.is_empty() {
        return Err(MetarError::Empty);
    }
    let tokens: Vec<&str> = normalized.split(' ').collect();
    let mut idx = 0;
    let mut report_type = ReportType::Metar;
    let mut explicit = false;
    let mut cor = false;
    match tokens.first() {
        Some(&"METAR") => {
            explicit = true;
            idx = 1;
        }
        Some(&"SPECI") => {
            report_type = ReportType::Speci;
            explicit = true;
            idx = 1;
        }
        _ => {}
    }
    // WMO allows "METAR COR EHAM …".
    if tokens.get(idx) == Some(&"COR") {
        cor = true;
        idx += 1;
    }
    let station = match tokens.get(idx) {
        Some(t) if is_station(t) => (*t).to_owned(),
        _ => return Err(MetarError::MissingStation),
    };
    idx += 1;
    let (day, hour, minute) = tokens.get(idx).and_then(|t| parse_day_time(t)).ok_or(MetarError::MissingTime)?;
    idx += 1;

    let mut report = MetarReport {
        report_type,
        report_type_explicit: explicit,
        station,
        day,
        hour,
        minute,
        auto: false,
        cor,
        nil: false,
        temperature_whole: None,
        dewpoint_whole: None,
        temperature_tenths: None,
        dewpoint_tenths: None,
        qnh_hpa: None,
        altimeter_inhg_hundredths: None,
        canonical: canonicalize(raw),
    };

    let mut in_remarks = false;
    let mut in_trend = false;
    let mut temp_found = false;
    for token in &tokens[idx..] {
        let t = *token;
        if in_remarks {
            if report.temperature_tenths.is_none()
                && let Some((tt, dd)) = parse_t_group(t)
            {
                report.temperature_tenths = Some(tt);
                report.dewpoint_tenths = dd;
            }
            continue;
        }
        match t {
            "RMK" => {
                in_remarks = true;
                continue;
            }
            "AUTO" => report.auto = true,
            "COR" => report.cor = true,
            "NIL" => report.nil = true,
            "NOSIG" | "BECMG" | "TEMPO" => in_trend = true,
            _ => {}
        }
        if in_trend {
            continue;
        }
        if !temp_found && let Some((tt, dd)) = parse_temp_group(t) {
            report.temperature_whole = tt;
            report.dewpoint_whole = dd;
            temp_found = true;
            continue;
        }
        if report.qnh_hpa.is_none()
            && let Some(rest) = t.strip_prefix('Q')
            && rest.len() == 4
            && let Ok(v) = rest.parse::<u16>()
        {
            report.qnh_hpa = Some(v);
        }
        if report.altimeter_inhg_hundredths.is_none()
            && let Some(rest) = t.strip_prefix('A')
            && rest.len() == 4
            && let Ok(v) = rest.parse::<u16>()
        {
            report.altimeter_inhg_hundredths = Some(v);
        }
    }
    // A T-group must agree with the whole-degree group (±0.5 °C after ICAO
    // rounding); if it does not, trust the main body and drop the tenths.
    if let (Some(tenths), Some(whole)) = (report.temperature_tenths, report.temperature_whole)
        && TempC::from_tenths(tenths).round_half_up_whole().abs_diff(whole) > 1
    {
        report.temperature_tenths = None;
        report.dewpoint_tenths = None;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn eham_routine_metar() {
        let r = parse_metar("METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG=").unwrap();
        assert_eq!(r.station, "EHAM");
        assert_eq!(r.report_type, ReportType::Metar);
        assert!(r.report_type_explicit);
        assert_eq!((r.day, r.hour, r.minute), (26, 12, 55));
        assert_eq!(r.temperature_whole, Some(18));
        assert_eq!(r.dewpoint_whole, Some(12));
        assert_eq!(r.qnh_hpa, Some(1016));
        assert_eq!(r.temperature().unwrap().0, TempC::from_whole(18));
        assert_eq!(r.canonical, "EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG");
    }

    #[test]
    fn negative_and_missing_values() {
        let r = parse_metar("EHAM 150225Z AUTO 03005KT 9999 NCD M02/M05 Q1030").unwrap();
        assert!(r.auto);
        assert!(!r.report_type_explicit);
        assert_eq!(r.temperature_whole, Some(-2));
        assert_eq!(r.dewpoint_whole, Some(-5));
        let r = parse_metar("EHAM 150225Z AUTO 03005KT 9999 NCD 05/ Q1030").unwrap();
        assert_eq!(r.temperature_whole, Some(5));
        assert_eq!(r.dewpoint_whole, None);
        let r = parse_metar("EHAM 150225Z AUTO 03005KT 9999 ///////// ///// Q1030").unwrap();
        assert_eq!(r.temperature_whole, None);
        let r = parse_metar("EHAM 150225Z NIL").unwrap();
        assert!(r.nil);
        assert_eq!(r.temperature(), None);
        // Missing visibility (////) must not be mistaken for the temperature group.
        let r = parse_metar("EHAM 150225Z AUTO 03005KT //// NCD 05/03 Q1030").unwrap();
        assert_eq!(r.temperature_whole, Some(5));
        let r = parse_metar("EHAM 150225Z AUTO 03005KT 9999 ///12 Q1030").unwrap();
        assert_eq!((r.temperature_whole, r.dewpoint_whole), (None, Some(12)));
    }

    #[test]
    fn m00_is_zero() {
        let r = parse_metar("EHAM 010155Z 00000KT CAVOK M00/M01 Q1020").unwrap();
        assert_eq!(r.temperature_whole, Some(0));
        assert_eq!(r.dewpoint_whole, Some(-1));
    }

    #[test]
    fn speci_and_cor_variants() {
        let r = parse_metar("SPECI EHAM 261307Z 25015G27KT 3000 TSRA BKN012CB 16/14 Q1017").unwrap();
        assert_eq!(r.report_type, ReportType::Speci);
        assert_eq!(r.temperature_whole, Some(16));
        let r = parse_metar("METAR COR EHAM 261255Z 24012KT 9999 FEW030 17/12 Q1016").unwrap();
        assert!(r.cor);
        let r = parse_metar("KJFK 261251Z COR 18010KT 10SM FEW250 24/13 A3002 RMK AO2 T02440133").unwrap();
        assert!(r.cor);
        assert_eq!(r.temperature_whole, Some(24));
        assert_eq!(r.temperature_tenths, Some(244));
        assert_eq!(r.dewpoint_tenths, Some(133));
        assert_eq!(r.altimeter_inhg_hundredths, Some(3002));
        assert_eq!(r.temperature().unwrap(), (TempC::from_tenths(244), wm_core::weather::TempPrecision::Tenth));
    }

    #[test]
    fn us_visibility_fractions_are_not_temperatures() {
        let r = parse_metar("KORD 261251Z 27008KT 1 1/2SM BR OVC004 12/11 A2990").unwrap();
        assert_eq!(r.temperature_whole, Some(12));
        let r = parse_metar("KORD 261251Z 27008KT M1/4SM FG VV001 11/11 A2990").unwrap();
        assert_eq!(r.temperature_whole, Some(11));
    }

    #[test]
    fn inconsistent_t_group_is_dropped() {
        let r = parse_metar("KJFK 261251Z 18010KT 10SM FEW250 24/13 A3002 RMK AO2 T01440133").unwrap();
        assert_eq!(r.temperature_whole, Some(24));
        assert_eq!(r.temperature_tenths, None);
    }

    #[test]
    fn negative_t_group() {
        let r = parse_metar("KMSP 151253Z 31012KT 10SM CLR M12/M20 A3050 RMK AO2 T11221200").unwrap();
        assert_eq!(r.temperature_tenths, Some(-122));
        assert_eq!(r.dewpoint_tenths, Some(-200));
    }

    #[test]
    fn trend_groups_are_ignored() {
        let r = parse_metar("EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 BECMG 25015KT").unwrap();
        assert_eq!(r.temperature_whole, Some(18));
    }

    #[test]
    fn errors() {
        assert_eq!(parse_metar("   "), Err(MetarError::Empty));
        assert_eq!(parse_metar("METAR 261255Z"), Err(MetarError::MissingStation));
        assert_eq!(parse_metar("EHAM 26125Z 18/12"), Err(MetarError::MissingTime));
        assert_eq!(parse_metar("EHAM 321255Z 18/12"), Err(MetarError::MissingTime));
    }

    #[test]
    fn canonical_form_is_relay_independent() {
        assert_eq!(
            canonicalize("METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG="),
            canonicalize("EHAM 261255Z 24012KT  9999 FEW030 18/12 Q1016 NOSIG\n")
        );
    }

    #[test]
    fn day_time_resolution_handles_month_rollover() {
        let r = utc("2026-10-01T00:05:00Z");
        assert_eq!(resolve_day_time(30, 23, 55, r), Some(utc("2026-09-30T23:55:00Z")));
        assert_eq!(resolve_day_time(1, 0, 0, r), Some(utc("2026-10-01T00:00:00Z")));
        // Day 31 after a 30-day month: go back two months (Aug 31).
        let r = utc("2026-10-01T00:05:00Z");
        assert_eq!(resolve_day_time(31, 23, 55, r), Some(utc("2026-08-31T23:55:00Z")));
        // Slightly skewed reference before the report.
        let r = utc("2026-09-30T23:59:00Z");
        assert_eq!(resolve_day_time(1, 0, 25, r), Some(utc("2026-10-01T00:25:00Z")));
        // Year rollover.
        let r = utc("2027-01-01T00:10:00Z");
        assert_eq!(resolve_day_time(31, 23, 55, r), Some(utc("2026-12-31T23:55:00Z")));
        assert_eq!(resolve_day_time(0, 0, 0, r), None);
    }

    proptest! {
        #[test]
        fn parser_never_panics(s in "\\PC{0,200}") {
            let _ = parse_metar(&s);
        }

        #[test]
        fn temperature_group_roundtrip(t in -60i32..=60, d in -60i32..=60) {
            let fmt = |v: i32| if v < 0 { format!("M{:02}", -v) } else { format!("{v:02}") };
            let raw = format!("EHAM 261255Z 24012KT 9999 FEW030 {}/{} Q1016", fmt(t), fmt(d));
            let r = parse_metar(&raw).unwrap();
            prop_assert_eq!(r.temperature_whole, Some(t));
            prop_assert_eq!(r.dewpoint_whole, Some(d));
        }
    }
}
