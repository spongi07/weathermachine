//! TemperatureOutcomeMapper: bucket labels → typed temperature intervals.
//!
//! Handles `18°C`, `13°C or below`, `24°C or higher`, `≤13°C`, `≥24°C`,
//! `86-87°F`, `86–87°F`, negative values (`-2°C`, `−2°C`) and falls back to
//! parsing the market question. Labels that cannot be mapped make the whole
//! event untradable (never guessed).

use wm_core::market::{TempUnit, TemperatureBucket};

/// Mapping failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot map outcome label '{0}' to a temperature bucket")]
pub struct OutcomeMapError(pub String);

fn normalize(s: &str) -> String {
    let mut out = s
        .replace(['\u{2212}', '\u{2013}', '\u{2014}'], "-")
        .replace("º", "°")
        .replace("° C", "°C")
        .replace("° F", "°F")
        .to_lowercase();
    while out.contains(" °") {
        out = out.replace(" °", "°");
    }
    out
}

/// Extract a signed integer immediately preceding `idx` in `s` (e.g. "-2" in "-2°c").
fn number_before(s: &str, idx: usize) -> Option<(i32, usize)> {
    let bytes = s.as_bytes();
    let mut start = idx;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == idx {
        return None;
    }
    let mut v: i32 = s[start..idx].parse().ok()?;
    if start > 0 && bytes[start - 1] == b'-' {
        // A minus directly attached (and not part of a range "86-87") means negative.
        let is_range = start >= 2 && bytes[start - 2].is_ascii_digit();
        if !is_range {
            v = -v;
            start -= 1;
        }
    }
    Some((v, start))
}

/// Map a label like `18°C` or `24°C or higher` to a bucket.
pub fn parse_bucket_label(label: &str) -> Result<TemperatureBucket, OutcomeMapError> {
    let s = normalize(label);
    let (unit, deg_idx) = if let Some(i) = s.find("°c") {
        (TempUnit::Celsius, i)
    } else if let Some(i) = s.find("°f") {
        (TempUnit::Fahrenheit, i)
    } else {
        return Err(OutcomeMapError(label.to_owned()));
    };
    let (value, start) = number_before(&s, deg_idx).ok_or_else(|| OutcomeMapError(label.to_owned()))?;
    let before = &s[..start];
    let after = &s[deg_idx..];
    // Range "86-87°f": number, '-', number.
    if let Some(stripped) = before.strip_suffix('-')
        && let Some((lo, _)) = number_before(stripped, stripped.len())
    {
        return Ok(TemperatureBucket::range(lo, value, unit));
    }
    let lower_words = ["or below", "or lower", "or less", "and below", "or colder"];
    let upper_words = ["or higher", "or above", "or more", "and above", "or warmer"];
    if before.contains('≤') || before.contains("<=") || lower_words.iter().any(|w| after.contains(w)) {
        return Ok(TemperatureBucket::at_or_below(value, unit));
    }
    if before.contains('≥') || before.contains(">=") || upper_words.iter().any(|w| after.contains(w)) {
        return Ok(TemperatureBucket::at_or_above(value, unit));
    }
    if before.contains('<') || before.contains('>') {
        // Strict inequalities are ambiguous for integer settlement: refuse.
        return Err(OutcomeMapError(label.to_owned()));
    }
    Ok(TemperatureBucket::exact(value, unit))
}

/// Map using the group title first, then the question text.
pub fn map_outcome(group_item_title: Option<&str>, question: Option<&str>) -> Result<TemperatureBucket, OutcomeMapError> {
    if let Some(t) = group_item_title.filter(|t| !t.trim().is_empty())
        && let Ok(b) = parse_bucket_label(t)
    {
        return Ok(b);
    }
    if let Some(q) = question {
        // "Will the highest temperature in Amsterdam be 18°C on September 25?"
        let s = normalize(q);
        if let Some(pos) = s.find(" be ") {
            let tail = &s[pos + 4..];
            let end = tail.find(" on ").unwrap_or(tail.len());
            if let Ok(b) = parse_bucket_label(&tail[..end]) {
                return Ok(b);
            }
        }
    }
    Err(OutcomeMapError(group_item_title.or(question).unwrap_or_default().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn celsius_labels() {
        assert_eq!(parse_bucket_label("18°C").unwrap(), TemperatureBucket::exact(18, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("13°C or below").unwrap(), TemperatureBucket::at_or_below(13, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("24°C or higher").unwrap(), TemperatureBucket::at_or_above(24, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("≤13°C").unwrap(), TemperatureBucket::at_or_below(13, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("≥24°C").unwrap(), TemperatureBucket::at_or_above(24, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("-2°C").unwrap(), TemperatureBucket::exact(-2, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("−3°C or below").unwrap(), TemperatureBucket::at_or_below(-3, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("18 °C").unwrap(), TemperatureBucket::exact(18, TempUnit::Celsius));
        assert_eq!(parse_bucket_label("18º C").unwrap(), TemperatureBucket::exact(18, TempUnit::Celsius));
    }

    #[test]
    fn fahrenheit_ranges() {
        assert_eq!(parse_bucket_label("86-87°F").unwrap(), TemperatureBucket::range(86, 87, TempUnit::Fahrenheit));
        assert_eq!(parse_bucket_label("86–87°F").unwrap(), TemperatureBucket::range(86, 87, TempUnit::Fahrenheit));
        assert_eq!(parse_bucket_label("95°F or higher").unwrap(), TemperatureBucket::at_or_above(95, TempUnit::Fahrenheit));
    }

    #[test]
    fn refuses_ambiguous_or_unknown() {
        assert!(parse_bucket_label("<18°C").is_err());
        assert!(parse_bucket_label("hot").is_err());
        assert!(parse_bucket_label("18").is_err());
    }

    #[test]
    fn question_fallback() {
        let b = map_outcome(Some(""), Some("Will the highest temperature in Amsterdam be 18°C on September 25?")).unwrap();
        assert_eq!(b, TemperatureBucket::exact(18, TempUnit::Celsius));
        let b = map_outcome(None, Some("Will the highest temperature in Amsterdam be 24°C or higher on September 25?")).unwrap();
        assert_eq!(b, TemperatureBucket::at_or_above(24, TempUnit::Celsius));
        assert!(map_outcome(None, Some("Will it rain?")).is_err());
    }

    proptest! {
        #[test]
        fn exact_roundtrip(v in -60i32..60) {
            let label = format!("{v}°C");
            prop_assert_eq!(parse_bucket_label(&label).unwrap(), TemperatureBucket::exact(v, TempUnit::Celsius));
        }

        #[test]
        fn never_panics(s in "\\PC{0,40}") {
            let _ = parse_bucket_label(&s);
        }
    }
}
