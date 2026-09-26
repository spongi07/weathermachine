//! Observation providers.
//!
//! Source preference (blueprint §7, Phase-0 findings):
//! 1. [`AwcMetarSource`] — NOAA Aviation Weather Center Data API: official,
//!    documented (JSON, documented limit of 100 req/min), carries `receiptTime`.
//! 2. [`TgftpMetarSource`] — NWS Telecommunication Gateway station file:
//!    official product distribution, supports `If-Modified-Since`.
//! 3. [`NwsApiSource`] — api.weather.gov station observations (coverage of
//!    non-US stations such as EHAM must be verified in Phase 0).
//!
//! The WRH time-series page itself is rendered client-side from the Synoptic
//! Data API with a token issued to the NWS; Weather Machine does **not** reuse
//! that token. See `docs/blueprint/01-phase0-nws-wrh-investigation.md`.

use crate::metar::{self, MetarReport};
use crate::source::{ObservationSource, ParsedReport, SourceError, SourceFetch, raw_record};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ProviderId, StationId};
use wm_core::ingest::BoxFuture;
use wm_core::weather::ReportType;
use wm_net::{FetchRequest, HttpFetcher};

/// Lenient timestamp parsing for provider-supplied strings.
pub fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y/%m/%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    None
}

fn report_type_of(metar: Option<&MetarReport>, hint: Option<&str>) -> ReportType {
    match hint.map(str::trim) {
        Some(h) if h.eq_ignore_ascii_case("SPECI") => ReportType::Speci,
        Some(h) if h.eq_ignore_ascii_case("METAR") => ReportType::Metar,
        _ => metar.map_or(ReportType::Metar, |m| m.report_type),
    }
}

fn malformed(detail: String, raw: wm_core::ingest::RawPayloadRecord, request: wm_core::ingest::ProviderRequestRecord) -> SourceError {
    SourceError::Malformed { detail, raw: Box::new(raw), request: Box::new(request) }
}

// ---------------------------------------------------------------------------
// AWC Data API
// ---------------------------------------------------------------------------

/// NOAA Aviation Weather Center Data API METAR source.
pub struct AwcMetarSource {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
    hours: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AwcMetar {
    icao_id: Option<String>,
    receipt_time: Option<String>,
    obs_time: Option<i64>,
    temp: Option<f64>,
    metar_type: Option<String>,
    raw_ob: Option<String>,
}

impl AwcMetarSource {
    pub const DEFAULT_BASE: &'static str = "https://aviationweather.gov";

    /// `hours` of history requested per poll (≥ 2 recovers reports missed during short outages).
    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>, hours: u32) -> Self {
        Self { fetcher, base_url: base_url.into().trim_end_matches('/').to_owned(), hours: hours.clamp(1, 24) }
    }

    pub fn endpoint(&self, station: &StationId) -> String {
        format!("/api/data/metar?ids={station}&format=json&hours={}", self.hours)
    }

    /// Parse an AWC JSON body. Public for fixture tests.
    pub fn parse_body(body: &[u8], station: &StationId, fetched_at: DateTime<Utc>) -> Result<(Vec<ParsedReport>, Vec<String>), String> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok((Vec::new(), Vec::new()));
        }
        let entries: Vec<AwcMetar> = serde_json::from_slice(body).map_err(|e| format!("invalid AWC JSON: {e}"))?;
        let mut reports = Vec::new();
        let mut warnings = Vec::new();
        for (i, e) in entries.into_iter().enumerate() {
            let Some(raw) = e.raw_ob.filter(|r| !r.trim().is_empty()) else {
                warnings.push(format!("entry {i}: missing rawOb"));
                continue;
            };
            let parsed = metar::parse_metar(&raw);
            let icao = e.icao_id.as_deref().map(str::to_ascii_uppercase);
            let st = icao.clone().or_else(|| parsed.as_ref().ok().map(|m| m.station.clone()));
            if st.as_deref() != Some(station.as_str()) {
                warnings.push(format!("entry {i}: station {st:?} != {station}"));
                continue;
            }
            let reference = e.obs_time.and_then(|t| Utc.timestamp_opt(t, 0).single()).unwrap_or(fetched_at);
            let observed_at = match parsed.as_ref().ok().and_then(|m| m.observed_at(reference)) {
                Some(t) => t,
                None => match e.obs_time.and_then(|t| Utc.timestamp_opt(t, 0).single()) {
                    Some(t) => t,
                    None => {
                        warnings.push(format!("entry {i}: no usable observation time"));
                        continue;
                    }
                },
            };
            if let Err(err) = &parsed {
                warnings.push(format!("entry {i}: METAR parse error: {err}"));
            }
            let metar = parsed.ok();
            reports.push(ParsedReport {
                station: station.clone(),
                observed_at,
                report_type: report_type_of(metar.as_ref(), e.metar_type.as_deref()),
                raw_text: raw,
                provider_temp_tenths: e.temp.filter(|t| t.is_finite()).map(|t| (t * 10.0).round() as i32),
                provider_receipt_at: e.receipt_time.as_deref().and_then(parse_timestamp),
                metar,
            });
        }
        Ok((reports, warnings))
    }
}

impl ObservationSource for AwcMetarSource {
    fn provider(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    fn gate(&self) -> &Arc<wm_net::ProviderGate> {
        self.fetcher.gate()
    }

    fn fetch<'a>(&'a self, station: &'a StationId, max_gate_wait: Duration) -> BoxFuture<'a, Result<SourceFetch, SourceError>> {
        Box::pin(async move {
            let endpoint = self.endpoint(station);
            let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint.clone())
                .station(station.clone())
                .accept("application/json")
                .max_gate_wait(max_gate_wait);
            let resp = self.fetcher.get(&req).await?;
            let raw = raw_record(self.provider(), station, &endpoint, resp.fetched_at, resp.status, resp.content_type.clone(), &resp.body);
            match Self::parse_body(&resp.body, station, resp.fetched_at) {
                Ok((reports, warnings)) => Ok(SourceFetch { reports, raw, request: resp.record, cache: resp.cache, warnings }),
                Err(detail) => Err(malformed(detail, raw, resp.record)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// NWS TGFTP station files
// ---------------------------------------------------------------------------

/// NWS Telecommunication Gateway latest-METAR station file source.
pub struct TgftpMetarSource {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
}

impl TgftpMetarSource {
    pub const DEFAULT_BASE: &'static str = "https://tgftp.nws.noaa.gov";

    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>) -> Self {
        Self { fetcher, base_url: base_url.into().trim_end_matches('/').to_owned() }
    }

    pub fn endpoint(station: &StationId) -> String {
        format!("/data/observations/metar/stations/{station}.TXT")
    }

    /// Parse `YYYY/MM/DD HH:MM\n<METAR>\n`.
    pub fn parse_body(body: &[u8], station: &StationId) -> Result<Vec<ParsedReport>, String> {
        let text = std::str::from_utf8(body).map_err(|_| "station file is not UTF-8".to_owned())?;
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let stamp = lines.next().ok_or("empty station file")?;
        let header_time = parse_timestamp(stamp).ok_or_else(|| format!("bad timestamp line '{stamp}'"))?;
        let raw = lines.collect::<Vec<_>>().join(" ");
        if raw.is_empty() {
            return Err("missing report line".into());
        }
        let m = metar::parse_metar(&raw).map_err(|e| format!("METAR parse error: {e}"))?;
        if m.station != station.as_str() {
            return Err(format!("station mismatch: {} != {station}", m.station));
        }
        let observed_at = m.observed_at(header_time).ok_or("unresolvable observation time")?;
        Ok(vec![ParsedReport {
            station: station.clone(),
            observed_at,
            report_type: m.report_type,
            raw_text: raw,
            provider_temp_tenths: None,
            provider_receipt_at: None,
            metar: Some(m),
        }])
    }
}

impl ObservationSource for TgftpMetarSource {
    fn provider(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    fn gate(&self) -> &Arc<wm_net::ProviderGate> {
        self.fetcher.gate()
    }

    fn fetch<'a>(&'a self, station: &'a StationId, max_gate_wait: Duration) -> BoxFuture<'a, Result<SourceFetch, SourceError>> {
        Box::pin(async move {
            let endpoint = Self::endpoint(station);
            let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint.clone())
                .station(station.clone())
                .max_gate_wait(max_gate_wait);
            let resp = self.fetcher.get(&req).await?;
            let raw = raw_record(self.provider(), station, &endpoint, resp.fetched_at, resp.status, resp.content_type.clone(), &resp.body);
            match Self::parse_body(&resp.body, station) {
                Ok(reports) => Ok(SourceFetch { reports, raw, request: resp.record, cache: resp.cache, warnings: Vec::new() }),
                Err(detail) => Err(malformed(detail, raw, resp.record)),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// api.weather.gov
// ---------------------------------------------------------------------------

/// api.weather.gov station-observations source.
pub struct NwsApiSource {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct NwsCollection {
    features: Vec<NwsFeature>,
}

#[derive(Debug, Deserialize)]
struct NwsFeature {
    properties: NwsProps,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NwsProps {
    timestamp: Option<String>,
    raw_message: Option<String>,
    temperature: Option<NwsValue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NwsValue {
    value: Option<f64>,
    unit_code: Option<String>,
}

impl NwsApiSource {
    pub const DEFAULT_BASE: &'static str = "https://api.weather.gov";

    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>, limit: u32) -> Self {
        Self { fetcher, base_url: base_url.into().trim_end_matches('/').to_owned(), limit: limit.clamp(1, 50) }
    }

    pub fn endpoint(&self, station: &StationId) -> String {
        format!("/stations/{station}/observations?limit={}", self.limit)
    }

    pub fn parse_body(body: &[u8], station: &StationId) -> Result<(Vec<ParsedReport>, Vec<String>), String> {
        let col: NwsCollection = serde_json::from_slice(body).map_err(|e| format!("invalid NWS GeoJSON: {e}"))?;
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        for (i, f) in col.features.into_iter().enumerate() {
            let p = f.properties;
            let Some(raw) = p.raw_message.filter(|r| !r.trim().is_empty()) else {
                warnings.push(format!("feature {i}: no rawMessage"));
                continue;
            };
            let Some(ts) = p.timestamp.as_deref().and_then(parse_timestamp) else {
                warnings.push(format!("feature {i}: bad timestamp"));
                continue;
            };
            let parsed = metar::parse_metar(&raw).ok();
            let observed_at = parsed.as_ref().and_then(|m| m.observed_at(ts)).unwrap_or(ts);
            let celsius = p.temperature.and_then(|t| {
                let is_c = t.unit_code.as_deref().is_none_or(|u| u.ends_with("degC"));
                t.value.filter(|v| v.is_finite() && is_c)
            });
            out.push(ParsedReport {
                station: station.clone(),
                observed_at,
                report_type: parsed.as_ref().map_or(ReportType::Metar, |m| m.report_type),
                raw_text: raw,
                provider_temp_tenths: celsius.map(|c| (c * 10.0).round() as i32),
                provider_receipt_at: None,
                metar: parsed,
            });
        }
        Ok((out, warnings))
    }
}

impl ObservationSource for NwsApiSource {
    fn provider(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    fn gate(&self) -> &Arc<wm_net::ProviderGate> {
        self.fetcher.gate()
    }

    fn fetch<'a>(&'a self, station: &'a StationId, max_gate_wait: Duration) -> BoxFuture<'a, Result<SourceFetch, SourceError>> {
        Box::pin(async move {
            let endpoint = self.endpoint(station);
            let req = FetchRequest::get(format!("{}{}", self.base_url, endpoint), endpoint.clone())
                .station(station.clone())
                .accept("application/geo+json")
                .max_gate_wait(max_gate_wait);
            let resp = self.fetcher.get(&req).await?;
            let raw = raw_record(self.provider(), station, &endpoint, resp.fetched_at, resp.status, resp.content_type.clone(), &resp.body);
            match Self::parse_body(&resp.body, station) {
                Ok((reports, warnings)) => Ok(SourceFetch { reports, raw, request: resp.record, cache: resp.cache, warnings }),
                Err(detail) => Err(malformed(detail, raw, resp.record)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eham() -> StationId {
        StationId::new("EHAM").unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Synthetic fixture modelled on the documented AWC JSON field names
    /// (icaoId, receiptTime, obsTime, temp, metarType, rawOb). Replace with a
    /// captured sample during Phase 0 (see docs/blueprint/01-…).
    const AWC_FIXTURE: &str = r#"[
      {"icaoId":"EHAM","receiptTime":"2026-09-26 12:57:41","obsTime":1790427300,"reportTime":"2026-09-26T13:00:00.000Z",
       "temp":18,"dewp":12,"wdir":240,"wspd":12,"visib":"6+","altim":1016,"metarType":"METAR",
       "rawOb":"METAR EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG","name":"Amsterdam/Schiphol Arpt, NH, NL"},
      {"icaoId":"EHAM","receiptTime":"2026-09-26 12:27:40","obsTime":1790425500,"temp":17,"metarType":"METAR",
       "rawOb":"METAR EHAM 261225Z 23011KT 9999 FEW028 17/12 Q1016 NOSIG"},
      {"icaoId":"EHAM","obsTime":1790423700,"temp":17,"metarType":"METAR"}
    ]"#;

    #[test]
    fn awc_fixture_parses() {
        let (reports, warnings) = AwcMetarSource::parse_body(AWC_FIXTURE.as_bytes(), &eham(), utc("2026-09-26T12:58:10Z")).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(warnings.len(), 1, "entry without rawOb is skipped with a warning");
        let r = &reports[0];
        assert_eq!(r.observed_at, utc("2026-09-26T12:55:00Z"));
        assert_eq!(r.report_type, ReportType::Metar);
        assert_eq!(r.provider_temp_tenths, Some(180));
        assert_eq!(r.provider_receipt_at, Some(utc("2026-09-26T12:57:41Z")));
        assert_eq!(r.metar.as_ref().unwrap().temperature_whole, Some(18));
        assert_eq!(reports[1].observed_at, utc("2026-09-26T12:25:00Z"));
    }

    #[test]
    fn awc_rejects_non_array_and_filters_foreign_stations() {
        assert!(AwcMetarSource::parse_body(b"<html>error</html>", &eham(), Utc::now()).is_err());
        assert!(AwcMetarSource::parse_body(b"{\"error\":1}", &eham(), Utc::now()).is_err());
        let (r, _) = AwcMetarSource::parse_body(b"   ", &eham(), Utc::now()).unwrap();
        assert!(r.is_empty());
        let foreign = r#"[{"icaoId":"EGLL","obsTime":1790427300,"rawOb":"EGLL 261250Z 24012KT 9999 19/11 Q1015"}]"#;
        let (r, w) = AwcMetarSource::parse_body(foreign.as_bytes(), &eham(), Utc::now()).unwrap();
        assert!(r.is_empty());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn tgftp_station_file_parses() {
        let body = b"2026/09/26 12:55\nEHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG\n";
        let reports = TgftpMetarSource::parse_body(body, &eham()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].observed_at, utc("2026-09-26T12:55:00Z"));
        assert!(TgftpMetarSource::parse_body(b"garbage", &eham()).is_err());
        assert!(TgftpMetarSource::parse_body(b"2026/09/26 12:55\n", &eham()).is_err());
        assert!(TgftpMetarSource::parse_body(b"2026/09/26 12:55\nEGLL 261250Z 19/11 Q1015\n", &eham()).is_err());
    }

    #[test]
    fn nws_geojson_parses() {
        let body = r#"{"type":"FeatureCollection","features":[
          {"properties":{"timestamp":"2026-09-26T12:55:00+00:00","rawMessage":"EHAM 261255Z 24012KT 9999 FEW030 18/12 Q1016 NOSIG",
           "temperature":{"unitCode":"wmoUnit:degC","value":18,"qualityControl":"V"}}},
          {"properties":{"timestamp":"2026-09-26T12:25:00+00:00","rawMessage":""}}
        ]}"#;
        let (r, w) = NwsApiSource::parse_body(body.as_bytes(), &eham()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(w.len(), 1);
        assert_eq!(r[0].provider_temp_tenths, Some(180));
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp("2026-09-26 12:57:41"), Some(utc("2026-09-26T12:57:41Z")));
        assert_eq!(parse_timestamp("2026-09-26T12:57:41.5Z").map(|t| t.timestamp()), Some(utc("2026-09-26T12:57:41Z").timestamp()));
        assert_eq!(parse_timestamp("2026/09/26 12:55"), Some(utc("2026-09-26T12:55:00Z")));
        assert_eq!(parse_timestamp("nope"), None);
    }
}
