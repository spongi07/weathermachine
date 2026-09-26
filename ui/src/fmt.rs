//! Display formatting (all decisions are exact server-side; these are views).

pub const DASH: &str = "—";

pub fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| DASH.to_owned(), |x| format!("{x:.digits$}"))
}

pub fn price(v: Option<f64>) -> String {
    v.map_or_else(|| DASH.to_owned(), |x| format!("{x:.3}"))
}

pub fn pct(v: Option<f64>) -> String {
    v.map_or_else(|| DASH.to_owned(), |x| format!("{:.1}%", x * 100.0))
}

/// Capitalise the first letter of a log-style message.
pub fn sentence(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Break-even probability; at or above 100 % no probability can profit.
pub fn break_even(v: Option<f64>) -> String {
    match v {
        Some(x) if x >= 1.0 => "n/a".to_owned(),
        _ => pct(v),
    }
}

pub fn pp(v: Option<f64>) -> String {
    v.map_or_else(|| DASH.to_owned(), |x| format!("{:+.1}", x * 100.0))
}

pub fn signed(v: Option<f64>, digits: usize) -> String {
    v.map_or_else(|| DASH.to_owned(), |x| format!("{x:+.digits$}"))
}

pub fn usd(v: f64) -> String {
    if v < 0.0 {
        format!("−${:.2}", -v)
    } else {
        format!("${v:.2}")
    }
}

pub fn usd_signed(v: f64) -> String {
    if v < 0.0 {
        format!("−${:.2}", -v)
    } else {
        format!("+${v:.2}")
    }
}

/// Unix ms → `HH:MM:SS` UTC.
pub fn utc_time(ms: i64) -> String {
    if ms <= 0 {
        return DASH.to_owned();
    }
    let secs = ms.div_euclid(1000);
    let day = secs.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}Z",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

/// Unix ms → `YYYY-MM-DD HH:MM:SS UTC` (civil-from-days, no time-zone data needed).
pub fn utc_datetime(ms: i64) -> String {
    if ms <= 0 {
        return DASH.to_owned();
    }
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {}", utc_time(ms))
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Human age of a duration in seconds.
pub fn age(secs: i64) -> String {
    match secs {
        s if s < 0 => "in future".to_owned(),
        s if s < 90 => format!("{s} s"),
        s if s < 90 * 60 => format!("{} min", s / 60),
        s if s < 48 * 3600 => format!("{:.1} h", s as f64 / 3600.0),
        s => format!("{} d", s / 86_400),
    }
}

/// `HH:MM` → minutes of day.
pub fn minutes_of(hhmm: &str) -> Option<f64> {
    let (h, m) = hhmm.split_once(':')?;
    Some(f64::from(h.parse::<u32>().ok()?) * 60.0 + f64::from(m.parse::<u32>().ok()?))
}

pub fn micros(us: u64) -> String {
    if us >= 10_000 {
        format!("{:.1} ms", us as f64 / 1000.0)
    } else {
        format!("{us} µs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times() {
        assert_eq!(utc_datetime(1_790_427_300_000), "2026-09-26 12:55:00Z");
        assert_eq!(utc_time(0), DASH);
        assert_eq!(minutes_of("13:55"), Some(835.0));
        assert_eq!(age(3700), "61 min");
        assert_eq!(usd(-1.5), "−$1.50");
        assert_eq!(pp(Some(0.0123)), "+1.2");
    }

    #[test]
    fn break_even_above_one_is_not_a_percentage() {
        assert_eq!(break_even(Some(0.00605)), "0.6%");
        assert_eq!(break_even(Some(1.004)), "n/a");
        assert_eq!(break_even(Some(1.0)), "n/a");
        assert_eq!(break_even(None), DASH);
        assert_eq!(sentence("model training failed"), "Model training failed");
        assert_eq!(sentence(""), "");
    }
}
