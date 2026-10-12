//! May this server open positions through the API?
//!
//! `GET https://polymarket.com/api/geoblock` gives the country and region
//! of the calling address, and a `blocked` flag that describes the
//! **website** (it is true from the Netherlands, where the API is allowed).
//! API trading is classified from the country and region against the
//! venue's documented lists, as dailyprofit does (`crates/app/src/geo.rs`,
//! docs.polymarket.com/api-reference/geoblock as read on 2026-09-24):
//!
//! * blocked on the API: IR SY CU KP UA-43 UA-14 UA-09;
//! * close-only on the API: AU BY BE BI BR CA-BC CA-ON CA-AB CA-QC CF CD ET
//!   FR DE IQ IT LB LY MM NZ NI PL RU SG SO SK SS SD TW TH GB US UM VE YE ZW;
//! * website only (the API allowed): IE JP MT NL KR.
//!
//! The lists change; `[live].geo_api_restricted` replaces the close-only one.
//! The address in the answer is never logged.

use serde::Serialize;

const API_BLOCKED: &[&str] = &["IR", "SY", "CU", "KP", "UA-43", "UA-14", "UA-09"];
const API_CLOSE_ONLY: &[&str] = &[
    "AU", "BY", "BE", "BI", "BR", "CA-BC", "CA-ON", "CA-AB", "CA-QC", "CF", "CD", "ET", "FR", "DE",
    "IQ", "IT", "LB", "LY", "MM", "NZ", "NI", "PL", "RU", "SG", "SO", "SK", "SS", "SD", "TW", "TH",
    "GB", "US", "UM", "VE", "YE", "ZW",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoStatus {
    /// The API may open positions from here.
    Allowed,
    /// The API only lets positions be closed.
    CloseOnly,
    Blocked,
}

impl GeoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "API allowed",
            Self::CloseOnly => "close-only on the API",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Geo {
    pub status: GeoStatus,
    pub country: String,
    pub region: String,
    /// The endpoint's own flag (the website).
    pub website_blocked: bool,
}

impl Geo {
    pub fn describe(&self) -> String {
        let place = if self.region.is_empty() {
            self.country.clone()
        } else {
            format!("{}-{}", self.country, self.region)
        };
        format!("{place}: {}", self.status.as_str())
    }
}

/// Classify by country, and by `COUNTRY-REGION` for sub-national rules.
pub fn classify(country: &str, region: &str, close_only_override: Option<&[String]>) -> GeoStatus {
    let c = country.trim().to_ascii_uppercase();
    let cr = format!("{c}-{}", region.trim().to_ascii_uppercase());
    let hit = |list: &[&str]| list.iter().any(|x| *x == c || *x == cr);
    if hit(API_BLOCKED) {
        return GeoStatus::Blocked;
    }
    match close_only_override {
        Some(list) => {
            if list
                .iter()
                .any(|x| x.eq_ignore_ascii_case(&c) || x.eq_ignore_ascii_case(&cr))
            {
                GeoStatus::CloseOnly
            } else {
                GeoStatus::Allowed
            }
        }
        None if hit(API_CLOSE_ONLY) => GeoStatus::CloseOnly,
        None => GeoStatus::Allowed,
    }
}

/// Read the endpoint's answer.
pub fn parse(body: &[u8], close_only_override: Option<&[String]>) -> Result<Geo, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("json: {e}"))?;
    let country = v["country"]
        .as_str()
        .ok_or("no country in the answer")?
        .to_owned();
    let region = v["region"].as_str().unwrap_or("").to_owned();
    Ok(Geo {
        status: classify(&country, &region, close_only_override),
        country,
        region,
        website_blocked: v["blocked"].as_bool().unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn api_trading_is_by_country_not_the_website_flag() {
        assert_eq!(
            classify("NL", "NH", None),
            GeoStatus::Allowed,
            "website blocked, API allowed"
        );
        assert_eq!(classify("IE", "L", None), GeoStatus::Allowed);
        assert_eq!(classify("GB", "ENG", None), GeoStatus::CloseOnly);
        assert_eq!(classify("de", "HE", None), GeoStatus::CloseOnly);
        assert_eq!(
            classify("CA", "ON", None),
            GeoStatus::CloseOnly,
            "a provincial rule"
        );
        assert_eq!(classify("CA", "NS", None), GeoStatus::Allowed);
        assert_eq!(classify("UA", "43", None), GeoStatus::Blocked);
        let ov = vec!["NL".to_owned()];
        assert_eq!(
            classify("NL", "NH", Some(&ov)),
            GeoStatus::CloseOnly,
            "the override replaces the list"
        );
        assert_eq!(classify("GB", "ENG", Some(&ov)), GeoStatus::Allowed);
        assert_eq!(
            classify("IR", "", Some(&ov)),
            GeoStatus::Blocked,
            "full blocks always apply"
        );
        let g = parse(
            br#"{"blocked":true,"ip":"192.0.2.1","country":"NL","region":"NH"}"#,
            None,
        )
        .unwrap();
        assert_eq!(g.status, GeoStatus::Allowed);
        assert!(g.website_blocked);
        assert_eq!(g.describe(), "NL-NH: API allowed");
        assert!(
            !format!("{g:?}").contains("192.0.2.1"),
            "the address is not kept"
        );
        assert!(parse(br#"{"blocked":false}"#, None).is_err());
    }
}
