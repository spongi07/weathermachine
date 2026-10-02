//! The weather groups of a METAR: wind, visibility, present and recent
//! weather, clouds, QNH and the TREND (`NOSIG`, `BECMG …`, `TEMPO …`).
//!
//! Context for the strategy lab (`wm_backtest::market_lab`): sea breezes,
//! showers, fog and fronts show here before they show in the temperature.
//! The temperature itself stays with [`crate::metar`], whose exact parse the
//! observations, the resolution check and every strategy rely on. This
//! parser never fails: a group it does not recognise is skipped, so an odd
//! report yields less context, never an error.
//!
//! Supported (WMO FM 15 as Schiphol writes it): wind `24012KT`, `21015G28KT`,
//! `VRB03KT`, `00000KT`, `05005MPS`, its variation `180V250`; visibility
//! `9999`, `0300`, `9999NDV`, `CAVOK`; present weather (`-SHRA`, `TS`,
//! `VCSH`, `FG`, `+TSRA`); clouds `FEW030`, `SCT020CB`, `BKN008///`,
//! `VV001`, `NSC`/`NCD`/`SKC`/`CLR`; `Q1016`; recent weather (`RESHRA`);
//! wind shear (`WS R18C`, `WS ALL RWY`, skipped); and the trend groups with
//! their time marks (`FM1230`, `TL1400`, `AT1300`), wind, visibility,
//! weather, `NSW` and clouds.

use serde::{Deserialize, Serialize};

/// Surface wind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wind {
    /// Degrees true the wind blows from; `None` when variable (`VRB`) or
    /// calm.
    pub direction: Option<u16>,
    /// Mean speed, knots (`MPS` converted).
    pub speed_kt: u16,
    pub gust_kt: Option<u16>,
}

impl Wind {
    /// Whether the wind blows from inside the arc `from` → `to` (degrees,
    /// clockwise, both inclusive; `from > to` wraps through north).
    pub fn from_arc(&self, from: u16, to: u16) -> bool {
        self.direction.is_some_and(|d| in_arc(d, from, to))
    }
}

/// Whether `d` lies on the clockwise arc `from` → `to` (inclusive).
pub fn in_arc(d: u16, from: u16, to: u16) -> bool {
    let d = d % 360;
    if from <= to {
        (from..=to).contains(&d)
    } else {
        d >= from || d <= to
    }
}

/// Cloud amount of a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cover {
    Few,
    Scattered,
    Broken,
    Overcast,
    /// Sky obscured: vertical visibility (`VV`).
    Obscured,
}

impl Cover {
    /// Broken, overcast or obscured: a ceiling.
    pub fn is_ceiling(self) -> bool {
        matches!(self, Cover::Broken | Cover::Overcast | Cover::Obscured)
    }
}

/// One cloud layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudLayer {
    pub cover: Cover,
    /// Base in feet above the aerodrome; `None` when not measured (`///`).
    pub base_ft: Option<u32>,
    /// Cumulonimbus or towering cumulus (`CB`, `TCU`).
    pub convective: bool,
}

/// Kind of a trend group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendKind {
    /// No significant change expected in the next two hours.
    Nosig,
    /// A lasting change.
    Becoming,
    /// A temporary change.
    Tempo,
}

/// The weather a group (the body or a trend group) describes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WxGroups {
    pub wind: Option<Wind>,
    pub visibility_m: Option<u32>,
    pub cavok: bool,
    /// Weather groups as written (`-SHRA`, `BR`, `VCTS`).
    pub weather: Vec<String>,
    pub clouds: Vec<CloudLayer>,
    /// `NSC`, `NCD`, `SKC`, `CLR` or `CAVOK`: no cloud of operational
    /// significance.
    pub no_significant_cloud: bool,
    /// `NSW`: the significant weather ends (trend groups only).
    pub nsw: bool,
}

impl WxGroups {
    /// Rain, drizzle, snow, hail and the like in a weather group (not in the
    /// vicinity).
    pub fn precipitation(&self) -> bool {
        self.weather.iter().any(|w| has_precipitation(w))
    }

    /// Thunder in a weather group, or a cumulonimbus layer.
    pub fn thunder(&self) -> bool {
        self.weather
            .iter()
            .any(|w| !w.starts_with("VC") && w.contains("TS"))
            || self.clouds.iter().any(|c| c.convective)
    }

    /// Fog or mist at the aerodrome.
    pub fn fog_or_mist(&self) -> bool {
        self.weather
            .iter()
            .any(|w| !w.starts_with("VC") && (w.contains("FG") || w.contains("BR")))
    }

    /// Base of the lowest broken, overcast or obscured layer.
    pub fn ceiling_ft(&self) -> Option<u32> {
        self.clouds
            .iter()
            .filter(|c| c.cover.is_ceiling())
            .filter_map(|c| c.base_ft)
            .min()
    }
}

/// One trend group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrendGroup {
    pub kind: TrendKind,
    pub wx: WxGroups,
}

/// The weather groups of one report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetarWx {
    pub body: WxGroups,
    /// Extremes of a varying wind direction (`180V250`).
    pub variable_from_to: Option<(u16, u16)>,
    /// Recent weather without its `RE` (`SHRA` from `RESHRA`).
    pub recent: Vec<String>,
    pub qnh_hpa: Option<u16>,
    /// `NOSIG`, or the `BECMG`/`TEMPO` groups in order.
    pub trend: Vec<TrendGroup>,
}

impl MetarWx {
    pub fn wind(&self) -> Option<Wind> {
        self.body.wind
    }

    /// Precipitation now, or since the previous report (`RE…`).
    pub fn precipitation_now_or_recent(&self) -> bool {
        self.body.precipitation() || self.recent.iter().any(|w| has_precipitation(w))
    }

    /// Thunder now or recently, or a cumulonimbus layer.
    pub fn thunder_now_or_recent(&self) -> bool {
        self.body.thunder() || self.recent.iter().any(|w| w.contains("TS"))
    }

    /// No precipitation, fog or mist, and no ceiling below `min_ceiling_ft`
    /// (CAVOK and `NSC` count as clear).
    pub fn clear(&self, min_ceiling_ft: u32) -> bool {
        !self.body.precipitation()
            && !self.body.fog_or_mist()
            && !self.body.thunder()
            && self.body.ceiling_ft().is_none_or(|c| c >= min_ceiling_ft)
    }

    /// The trend says `NOSIG`.
    pub fn nosig(&self) -> bool {
        self.trend.iter().any(|t| t.kind == TrendKind::Nosig)
    }

    /// A `BECMG` or `TEMPO` group forecasts precipitation or thunder.
    pub fn trend_precipitation(&self) -> bool {
        self.trend
            .iter()
            .any(|t| t.kind != TrendKind::Nosig && (t.wx.precipitation() || t.wx.thunder()))
    }

    /// A `BECMG` or `TEMPO` group forecasts wind from the arc `from` → `to`.
    pub fn trend_wind_from(&self, from: u16, to: u16) -> bool {
        self.trend
            .iter()
            .any(|t| t.kind != TrendKind::Nosig && t.wx.wind.is_some_and(|w| w.from_arc(from, to)))
    }

    /// A `BECMG` or `TEMPO` group forecasts a ceiling below `ft`.
    pub fn trend_ceiling_below(&self, ft: u32) -> bool {
        self.trend
            .iter()
            .any(|t| t.kind != TrendKind::Nosig && t.wx.ceiling_ft().is_some_and(|c| c < ft))
    }
}

/// Precipitation phenomena (WMO code table 4678).
const PRECIPITATION: [&str; 9] = ["DZ", "RA", "SN", "SG", "IC", "PL", "GR", "GS", "UP"];

/// Descriptors, then phenomena, of a weather group.
const DESCRIPTORS: [&str; 8] = ["MI", "PR", "BC", "DR", "BL", "SH", "TS", "FZ"];
const PHENOMENA: [&str; 21] = [
    "DZ", "RA", "SN", "SG", "IC", "PL", "GR", "GS", "UP", "BR", "FG", "FU", "VA", "DU", "SA", "HZ",
    "PY", "PO", "SQ", "FC", "SS",
];

fn has_precipitation(w: &str) -> bool {
    if w.starts_with("VC") {
        return false;
    }
    let body = w.trim_start_matches(['+', '-']);
    // Two-letter codes after the descriptor: "SHRA" → "SH", "RA".
    pairs(body).any(|p| PRECIPITATION.contains(&p))
}

/// The two-letter codes of a group (`TSRA` → `TS`, `RA`).
fn pairs(s: &str) -> impl Iterator<Item = &str> {
    (0..s.len() / 2).filter_map(move |k| s.get(2 * k..2 * k + 2))
}

/// A present-weather group: optional intensity or `VC`, at most one
/// descriptor, then phenomena; at least one code.
fn is_weather(token: &str) -> bool {
    let body = token
        .strip_prefix('+')
        .or_else(|| token.strip_prefix('-'))
        .or_else(|| token.strip_prefix("VC"))
        .unwrap_or(token);
    if body.is_empty()
        || !body.len().is_multiple_of(2)
        || !body.bytes().all(|b| b.is_ascii_uppercase())
    {
        return false;
    }
    let codes: Vec<&str> = pairs(body).collect();
    let rest = match codes.first() {
        Some(d) if DESCRIPTORS.contains(d) => &codes[1..],
        _ => &codes[..],
    };
    // "TS" and "SH" may stand alone ("VCSH", "TS").
    (rest.is_empty() && codes.len() == 1) || rest.iter().all(|c| PHENOMENA.contains(c))
}

fn parse_wind(token: &str) -> Option<Wind> {
    let (body, factor) = if let Some(b) = token.strip_suffix("KT") {
        (b, 1.0)
    } else if let Some(b) = token.strip_suffix("MPS") {
        (b, 1.943_844)
    } else {
        return None;
    };
    if body.len() < 5 || !body.is_ascii() {
        return None;
    }
    let (dir, rest) = body.split_at(3);
    let (speed, gust) = match rest.split_once('G') {
        Some((s, g)) => (s, Some(g)),
        None => (rest, None),
    };
    let num = |s: &str| -> Option<u16> {
        if (2..=3).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<u16>().ok()
        } else {
            None
        }
    };
    let knots = |v: u16| (f64::from(v) * factor).round() as u16;
    let speed = knots(num(speed)?);
    let gust = match gust {
        Some(g) => Some(knots(num(g)?)),
        None => None,
    };
    let direction = if dir == "VRB" {
        None
    } else if dir.len() == 3 && dir.bytes().all(|b| b.is_ascii_digit()) {
        let d: u16 = dir.parse().ok()?;
        if d > 360 {
            return None;
        }
        (speed > 0 || d > 0).then_some(d % 360)
    } else {
        return None;
    };
    Some(Wind {
        direction,
        speed_kt: speed,
        gust_kt: gust,
    })
}

/// `180V250`.
fn parse_variation(token: &str) -> Option<(u16, u16)> {
    let (a, b) = token.split_once('V')?;
    let deg = |s: &str| -> Option<u16> {
        (s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u16>().ok())
            .flatten()
            .filter(|d| *d <= 360)
    };
    Some((deg(a)?, deg(b)?))
}

/// `9999`, `0300`, `9999NDV`, `4000NE` (the prevailing value).
fn parse_visibility(token: &str) -> Option<u32> {
    let digits = token.get(..4)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let suffix = &token[4..];
    let ok = suffix.is_empty()
        || suffix == "NDV"
        || ["N", "NE", "E", "SE", "S", "SW", "W", "NW"].contains(&suffix);
    if !ok {
        return None;
    }
    let v: u32 = digits.parse().ok()?;
    // 9999 means 10 km or more.
    Some(if v == 9999 { 10_000 } else { v })
}

fn parse_cloud(token: &str) -> Option<CloudLayer> {
    let (cover, rest) = if let Some(r) = token.strip_prefix("FEW") {
        (Cover::Few, r)
    } else if let Some(r) = token.strip_prefix("SCT") {
        (Cover::Scattered, r)
    } else if let Some(r) = token.strip_prefix("BKN") {
        (Cover::Broken, r)
    } else if let Some(r) = token.strip_prefix("OVC") {
        (Cover::Overcast, r)
    } else if let Some(r) = token.strip_prefix("VV") {
        (Cover::Obscured, r)
    } else {
        return None;
    };
    let height = rest.get(..3)?;
    let base_ft = if height == "///" {
        None
    } else if height.bytes().all(|b| b.is_ascii_digit()) {
        Some(height.parse::<u32>().ok()? * 100)
    } else {
        return None;
    };
    let convective = match &rest[3..] {
        "" | "///" => false,
        "CB" | "TCU" => true,
        _ => return None,
    };
    Some(CloudLayer {
        cover,
        base_ft,
        convective,
    })
}

/// A runway designator: `R18`, `R18C`, `R36L`.
fn is_runway(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('R').filter(|r| r.is_ascii()) else {
        return false;
    };
    let (digits, side) = rest.split_at(rest.len().min(2));
    digits.len() == 2
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(side, "" | "L" | "C" | "R")
}

/// Read one group into `g`; `false` when the token is none of the groups a
/// body or trend group holds.
fn read_group(token: &str, g: &mut WxGroups) -> bool {
    if let Some(w) = parse_wind(token) {
        if g.wind.is_none() {
            g.wind = Some(w);
        }
        return true;
    }
    match token {
        "CAVOK" => {
            g.cavok = true;
            g.no_significant_cloud = true;
            if g.visibility_m.is_none() {
                g.visibility_m = Some(10_000);
            }
            return true;
        }
        "NSC" | "NCD" | "SKC" | "CLR" => {
            g.no_significant_cloud = true;
            return true;
        }
        "NSW" => {
            g.nsw = true;
            return true;
        }
        _ => {}
    }
    if let Some(v) = parse_visibility(token) {
        if g.visibility_m.is_none() {
            g.visibility_m = Some(v);
        }
        return true;
    }
    if let Some(c) = parse_cloud(token) {
        g.clouds.push(c);
        return true;
    }
    if is_weather(token) {
        g.weather.push(token.to_owned());
        return true;
    }
    false
}

/// The weather groups of a METAR or SPECI. Never fails: unknown groups are
/// skipped.
pub fn parse_wx(raw: &str) -> MetarWx {
    let normalized = wm_core::hash::normalize_report_text(raw);
    let tokens: Vec<&str> = normalized.split(' ').filter(|t| !t.is_empty()).collect();
    let mut wx = MetarWx::default();
    // Skip the type, COR, station and time: the body starts after the
    // `DDHHMMZ` group (a report without one has no body to read).
    let Some(start) = tokens.iter().position(|t| {
        t.len() == 7 && t.ends_with('Z') && t[..6].bytes().all(|b| b.is_ascii_digit())
    }) else {
        return wx;
    };
    let mut k = start + 1;
    let mut trend: Option<TrendGroup> = None;
    while k < tokens.len() {
        let t = tokens[k];
        k += 1;
        if t == "RMK" {
            break;
        }
        match t {
            "NOSIG" => {
                if let Some(g) = trend.take() {
                    wx.trend.push(g);
                }
                wx.trend.push(TrendGroup {
                    kind: TrendKind::Nosig,
                    wx: WxGroups::default(),
                });
                continue;
            }
            "BECMG" | "TEMPO" => {
                if let Some(g) = trend.take() {
                    wx.trend.push(g);
                }
                trend = Some(TrendGroup {
                    kind: if t == "BECMG" {
                        TrendKind::Becoming
                    } else {
                        TrendKind::Tempo
                    },
                    wx: WxGroups::default(),
                });
                continue;
            }
            _ => {}
        }
        if let Some(g) = trend.as_mut() {
            // Time marks of the change.
            let timed = ["FM", "TL", "AT"].iter().any(|p| {
                t.strip_prefix(p)
                    .is_some_and(|r| r.len() == 4 && r.bytes().all(|b| b.is_ascii_digit()))
            });
            if !timed {
                read_group(t, &mut g.wx);
            }
            continue;
        }
        if t == "WS" {
            // Wind shear: "WS R18C" or "WS ALL RWY".
            while k < tokens.len()
                && (tokens[k] == "ALL" || tokens[k] == "RWY" || is_runway(tokens[k]))
            {
                k += 1;
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("RE")
            && is_weather(rest)
        {
            wx.recent.push(rest.to_owned());
            continue;
        }
        if wx.qnh_hpa.is_none()
            && let Some(rest) = t.strip_prefix('Q')
            && rest.len() == 4
        {
            wx.qnh_hpa = rest.parse::<u16>().ok();
            continue;
        }
        if wx.variable_from_to.is_none()
            && let Some(v) = parse_variation(t)
        {
            wx.variable_from_to = Some(v);
            continue;
        }
        read_group(t, &mut wx.body);
    }
    if let Some(g) = trend.take() {
        wx.trend.push(g);
    }
    wx
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_plain_schiphol_report() {
        let wx = parse_wx("EHAM 011155Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG");
        assert_eq!(
            wx.wind(),
            Some(Wind {
                direction: Some(240),
                speed_kt: 12,
                gust_kt: None
            })
        );
        assert_eq!(wx.body.visibility_m, Some(10_000));
        assert_eq!(
            wx.body.clouds,
            vec![CloudLayer {
                cover: Cover::Few,
                base_ft: Some(3000),
                convective: false
            }]
        );
        assert_eq!(wx.qnh_hpa, Some(1016));
        assert!(wx.nosig() && wx.trend.len() == 1);
        assert!(wx.body.weather.is_empty() && wx.recent.is_empty());
        assert!(wx.clear(5000) && !wx.precipitation_now_or_recent());
    }

    #[test]
    fn showers_gusts_variation_recent_weather_and_a_tempo() {
        let wx = parse_wx(
            "METAR EHAM 151425Z AUTO 21015G28KT 180V250 4000 -SHRA FEW012 SCT020CB BKN035 17/15 Q1008 RESHRA TEMPO 3000 SHRA BKN014",
        );
        assert_eq!(
            wx.wind(),
            Some(Wind {
                direction: Some(210),
                speed_kt: 15,
                gust_kt: Some(28)
            })
        );
        assert_eq!(wx.variable_from_to, Some((180, 250)));
        assert_eq!(wx.body.visibility_m, Some(4000));
        assert_eq!(wx.body.weather, vec!["-SHRA".to_owned()]);
        assert_eq!(wx.body.clouds.len(), 3);
        assert!(wx.body.clouds[1].convective);
        assert_eq!(wx.body.ceiling_ft(), Some(3500));
        assert_eq!(wx.recent, vec!["SHRA".to_owned()]);
        assert_eq!(wx.qnh_hpa, Some(1008));
        assert!(wx.body.precipitation() && wx.body.thunder());
        assert!(wx.precipitation_now_or_recent() && wx.thunder_now_or_recent());
        assert_eq!(wx.trend.len(), 1);
        let t = &wx.trend[0];
        assert_eq!(t.kind, TrendKind::Tempo);
        assert_eq!(t.wx.visibility_m, Some(3000));
        assert_eq!(t.wx.weather, vec!["SHRA".to_owned()]);
        assert_eq!(t.wx.ceiling_ft(), Some(1400));
        assert!(wx.trend_precipitation() && wx.trend_ceiling_below(3000));
        assert!(!wx.clear(5000));
    }

    #[test]
    fn fog_vertical_visibility_runway_range_and_a_becmg() {
        let wx =
            parse_wx("EHAM 020555Z 00000KT 0300 R18R/0550N FG VV001 09/09 Q1021 BECMG 2000 BR NSC");
        assert_eq!(
            wx.wind(),
            Some(Wind {
                direction: None,
                speed_kt: 0,
                gust_kt: None
            })
        );
        assert_eq!(wx.body.visibility_m, Some(300));
        assert!(wx.body.fog_or_mist());
        assert_eq!(wx.body.ceiling_ft(), Some(100));
        assert_eq!(wx.trend[0].kind, TrendKind::Becoming);
        assert_eq!(wx.trend[0].wx.visibility_m, Some(2000));
        assert!(wx.trend[0].wx.fog_or_mist() && wx.trend[0].wx.no_significant_cloud);
        assert!(!wx.trend_precipitation());
        assert!(!wx.clear(800));
    }

    #[test]
    fn cavok_and_variable_wind() {
        let wx = parse_wx("EHAM 101655Z VRB03KT CAVOK 28/14 Q1013 NOSIG");
        assert_eq!(
            wx.wind(),
            Some(Wind {
                direction: None,
                speed_kt: 3,
                gust_kt: None
            })
        );
        assert!(wx.body.cavok && wx.body.no_significant_cloud);
        assert_eq!(wx.body.visibility_m, Some(10_000));
        assert!(wx.clear(5000));
    }

    #[test]
    fn thunder_without_rain_vicinity_showers_and_a_wind_shift_trend() {
        let wx = parse_wx("EHAM 101655Z 27008KT 9999 TS VCSH SCT025CB 22/18 Q1009 BECMG 31015KT");
        assert_eq!(wx.body.weather, vec!["TS".to_owned(), "VCSH".to_owned()]);
        assert!(wx.body.thunder());
        // Showers in the vicinity are not precipitation at the aerodrome.
        assert!(!wx.body.precipitation());
        assert!(wx.trend_wind_from(270, 20) && !wx.trend_wind_from(90, 180));
        assert_eq!(wx.trend[0].wx.wind.and_then(|w| w.direction), Some(310));
    }

    #[test]
    fn wind_shear_is_skipped_and_mixed_precipitation_is_read() {
        let wx = parse_wx(
            "EHAM 101655Z 27008KT 9999 -RADZ BKN008 OVC015 14/13 Q1001 WS R18C TEMPO SHRA",
        );
        assert_eq!(wx.body.weather, vec!["-RADZ".to_owned()]);
        assert_eq!(wx.body.ceiling_ft(), Some(800));
        assert!(wx.body.precipitation());
        assert!(wx.trend_precipitation());
        let all = parse_wx("EHAM 101655Z 27008KT 9999 FEW020 14/13 Q1001 WS ALL RWY NOSIG");
        assert!(all.nosig() && all.body.weather.is_empty());
    }

    #[test]
    fn metres_per_second_minus_temperatures_and_missing_groups() {
        let wx = parse_wx("UUEE 101200Z 05005MPS 9999 BKN///CB M01/M03 Q//// NOSIG");
        assert_eq!(wx.wind().map(|w| w.speed_kt), Some(10));
        assert_eq!(wx.wind().and_then(|w| w.direction), Some(50));
        assert_eq!(
            wx.body.clouds,
            vec![CloudLayer {
                cover: Cover::Broken,
                base_ft: None,
                convective: true
            }]
        );
        assert_eq!(wx.qnh_hpa, None);
    }

    #[test]
    fn trend_time_marks_and_several_groups() {
        let wx = parse_wx(
            "EHAM 101625Z 19010KT 9999 SCT030 25/17 Q1010 BECMG FM1700 25012KT TEMPO TL1800 4000 TSRA BKN012CB",
        );
        assert_eq!(wx.trend.len(), 2);
        assert_eq!(wx.trend[0].kind, TrendKind::Becoming);
        assert_eq!(wx.trend[0].wx.wind.and_then(|w| w.direction), Some(250));
        assert_eq!(wx.trend[1].kind, TrendKind::Tempo);
        assert!(wx.trend[1].wx.thunder() && wx.trend[1].wx.precipitation());
        assert!(wx.trend_precipitation() && wx.trend_wind_from(240, 20));
        // Remarks end the report.
        let rmk = parse_wx("KJFK 101651Z 18012KT 10SM FEW050 28/19 A2992 RMK AO2 SLP132 T02830189");
        assert!(rmk.trend.is_empty() && rmk.wind().is_some());
    }

    #[test]
    fn arcs_wrap_through_north() {
        assert!(in_arc(350, 250, 20) && in_arc(10, 250, 20) && in_arc(250, 250, 20));
        assert!(!in_arc(120, 250, 20) && !in_arc(21, 250, 20));
        assert!(in_arc(120, 60, 220) && !in_arc(230, 60, 220));
        assert!(in_arc(360, 350, 10));
    }

    #[test]
    fn weather_groups_are_recognised_exactly() {
        for ok in [
            "-SHRA", "+TSRA", "TS", "VCSH", "FG", "BR", "MIFG", "-RADZ", "SHGS", "FZDZ",
        ] {
            assert!(is_weather(ok), "{ok}");
        }
        for not in [
            "NOSIG", "RMK", "AUTO", "CAVOK", "FEW030", "Q1016", "SHX", "XX", "-", "",
        ] {
            assert!(!is_weather(not), "{not}");
        }
    }

    #[test]
    fn a_report_without_a_time_group_has_no_body() {
        assert_eq!(parse_wx(""), MetarWx::default());
        assert_eq!(parse_wx("EHAM 24012KT"), MetarWx::default());
    }

    proptest! {
        #[test]
        fn never_panics(s in "\\PC{0,120}") {
            let _ = parse_wx(&s);
        }

        #[test]
        fn never_panics_on_metar_like_tokens(tokens in proptest::collection::vec(
            "(EHAM|[0-9]{6}Z|[0-9VRB]{3}[0-9]{2,3}(G[0-9]{2})?KT|[0-9]{4}|CAVOK|[-+]?(SH|TS)?(RA|DZ|FG|BR)|(FEW|SCT|BKN|OVC)[0-9/]{3}(CB)?|Q[0-9/]{4}|RE[A-Z]{2,4}|NOSIG|BECMG|TEMPO|WS|R[0-9]{2}[LCR]?|FM[0-9]{4})",
            0..20,
        )) {
            let _ = parse_wx(&tokens.join(" "));
        }
    }
}
