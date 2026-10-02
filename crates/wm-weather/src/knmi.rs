//! KNMI Data Platform EDR API: the ten-minute readings of a Dutch automatic
//! weather station.
//!
//! Collection `10-minute-in-situ-meteorological-observations` (published
//! 2025, replacing the deprecated `Actuele10mindataKNMIstations`), queried
//! per station by its WIGOS id — Schiphol, WMO 06240, is `0-20000-0-06240`
//! — for `ta`, the mean 1.5 m air temperature over the ten minutes, and
//! `tx`, its maximum, as CoverageJSON. Each value is stamped with the end of
//! its interval (UTC) and published a few minutes after it.
//!
//! A free API key from the KNMI Developer Portal goes in the
//! `Authorization` header. It never appears in a URL, a log line or an
//! audit record.
//!
//! Predictive input only: the METAR remains the observation, resolution and
//! settlement source. Strategy K reads these values to anticipate the next
//! METAR.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ProviderId, StationId};
use wm_core::units::TempC;
use wm_core::weather::TenMinuteObservation;
use wm_net::{FetchError, FetchRequest, HttpFetcher, Secret};

/// The collection's EDR endpoint.
pub const DEFAULT_BASE_URL: &str = "https://api.dataplatform.knmi.nl/edr/v1/collections/10-minute-in-situ-meteorological-observations";

/// WIGOS id of a Dutch station from its WMO number (`06240` →
/// `0-20000-0-06240`).
pub fn wigos_id(wmo: &str) -> String {
    format!("0-20000-0-{wmo}")
}

/// Physically plausible 1.5 m temperatures (°C); anything else is missing.
const PLAUSIBLE_C: std::ops::RangeInclusive<f64> = -60.0..=55.0;

/// Why readings could not be obtained.
#[derive(Debug, thiserror::Error)]
pub enum KnmiError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("unexpected KNMI response: {0}")]
    Malformed(String),
}

/// Client for one EDR collection.
pub struct KnmiTenMinute {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
    api_key: Secret,
    mean_parameter: String,
    max_parameter: String,
}

impl std::fmt::Debug for KnmiTenMinute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnmiTenMinute")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("mean_parameter", &self.mean_parameter)
            .field("max_parameter", &self.max_parameter)
            .finish()
    }
}

impl KnmiTenMinute {
    pub fn new(
        fetcher: Arc<HttpFetcher>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        mean_parameter: impl Into<String>,
        max_parameter: impl Into<String>,
    ) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: Secret::new(api_key),
            mean_parameter: mean_parameter.into(),
            max_parameter: max_parameter.into(),
        }
    }

    pub fn provider_id(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    fn query(&self, location: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> String {
        format!(
            "/locations/{location}?datetime={}/{}&parameter-name={},{}",
            from.format("%Y-%m-%dT%H:%M:%SZ"),
            to.format("%Y-%m-%dT%H:%M:%SZ"),
            self.mean_parameter,
            self.max_parameter
        )
    }

    /// The readings of `location` (WIGOS id) whose intervals end in
    /// `[from, to]`, oldest first, as `station`'s.
    pub async fn fetch(
        &self,
        station: &StationId,
        location: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max_gate_wait: Duration,
        attempts: u32,
    ) -> Result<Vec<TenMinuteObservation>, KnmiError> {
        let query = self.query(location, from, to);
        let req = FetchRequest::get(format!("{}{query}", self.base_url), query)
            .accept("application/json")
            .authorization(self.api_key.clone())
            .max_gate_wait(max_gate_wait)
            .unconditional();
        let resp = self.fetcher.get_retrying(&req, attempts.max(1)).await?;
        parse_coverage(
            &resp.body,
            station,
            &self.mean_parameter,
            &self.max_parameter,
            resp.fetched_at,
        )
        .map_err(KnmiError::Malformed)
    }

    /// Any of the collection's parameters (`qg`, `ta`, `dd`, …) of
    /// `location` whose intervals end in `[from, to]`, oldest first, in the
    /// collection's units. Research input: the strategy lab reads global
    /// radiation and the temperatures of neighbouring stations this way.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_series(
        &self,
        location: &str,
        parameters: &[&str],
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        max_gate_wait: Duration,
        attempts: u32,
    ) -> Result<Vec<SeriesPoint>, KnmiError> {
        let query = format!(
            "/locations/{location}?datetime={}/{}&parameter-name={}",
            from.format("%Y-%m-%dT%H:%M:%SZ"),
            to.format("%Y-%m-%dT%H:%M:%SZ"),
            parameters.join(",")
        );
        let req = FetchRequest::get(format!("{}{query}", self.base_url), query)
            .accept("application/json")
            .authorization(self.api_key.clone())
            .max_gate_wait(max_gate_wait)
            .unconditional();
        let resp = self.fetcher.get_retrying(&req, attempts.max(1)).await?;
        parse_series(&resp.body, parameters).map_err(KnmiError::Malformed)
    }
}

/// One ten-minute interval's values of any parameters, in the collection's
/// units (W/m² for `qg`, °C for `ta`, degrees for `dd`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesPoint {
    /// End of the interval (UTC).
    pub interval_end: DateTime<Utc>,
    /// The parameters with a finite value at this time.
    pub values: BTreeMap<String, f64>,
}

impl SeriesPoint {
    pub fn get(&self, parameter: &str) -> Option<f64> {
        self.values.get(parameter).copied()
    }
}

/// The points of a CoverageJSON response (a `CoverageCollection` or a
/// single `Coverage`) for `parameters`: one per time on the `t` axis that
/// has a finite value of at least one of them.
pub fn parse_series(body: &[u8], parameters: &[&str]) -> Result<Vec<SeriesPoint>, String> {
    let doc: CoverageDoc =
        serde_json::from_slice(body).map_err(|e| format!("not CoverageJSON: {e}"))?;
    let mut coverages = doc.coverages;
    if let Some(domain) = doc.domain {
        coverages.push(Coverage {
            domain,
            ranges: doc.ranges.unwrap_or_default(),
        });
    }
    let mut out: Vec<SeriesPoint> = Vec::new();
    for c in coverages {
        let times = &c.domain.axes.t.values;
        let mut columns: Vec<(&str, &[Option<f64>])> = Vec::new();
        for name in parameters {
            if let Some(r) = c.ranges.get(*name) {
                if r.values.len() != times.len() {
                    return Err(format!(
                        "{name} has {} values for {} times",
                        r.values.len(),
                        times.len()
                    ));
                }
                columns.push((name, &r.values));
            }
        }
        for (k, t) in times.iter().enumerate() {
            let at = DateTime::parse_from_rfc3339(t)
                .map_err(|e| format!("time '{t}': {e}"))?
                .with_timezone(&Utc);
            let values: BTreeMap<String, f64> = columns
                .iter()
                .filter_map(|(name, v)| {
                    v[k].filter(|x| x.is_finite())
                        .map(|x| ((*name).to_owned(), x))
                })
                .collect();
            if !values.is_empty() {
                out.push(SeriesPoint {
                    interval_end: at,
                    values,
                });
            }
        }
    }
    out.sort_by_key(|p| p.interval_end);
    out.dedup_by_key(|p| p.interval_end);
    Ok(out)
}

#[derive(Deserialize)]
struct CoverageDoc {
    #[serde(default)]
    coverages: Vec<Coverage>,
    /// A single `Coverage` at the top level (older responses).
    #[serde(default)]
    domain: Option<Domain>,
    #[serde(default)]
    ranges: Option<HashMap<String, NdArray>>,
}

#[derive(Deserialize)]
struct Coverage {
    domain: Domain,
    #[serde(default)]
    ranges: HashMap<String, NdArray>,
}

#[derive(Deserialize)]
struct Domain {
    axes: Axes,
}

#[derive(Deserialize)]
struct Axes {
    t: Axis,
}

#[derive(Deserialize)]
struct Axis {
    values: Vec<String>,
}

#[derive(Deserialize)]
struct NdArray {
    values: Vec<Option<f64>>,
}

fn tenths(v: Option<f64>) -> Option<TempC> {
    v.filter(|c| c.is_finite() && PLAUSIBLE_C.contains(c))
        .map(|c| TempC::from_tenths((c * 10.0).round() as i32))
}

/// The readings in a CoverageJSON response (a `CoverageCollection` or a
/// single `Coverage`): one per time on the `t` axis, the mean from
/// `mean_parameter` and the maximum from `max_parameter`, both in °C. Times
/// without either value are left out; implausible values count as missing.
pub fn parse_coverage(
    body: &[u8],
    station: &StationId,
    mean_parameter: &str,
    max_parameter: &str,
    received_at: DateTime<Utc>,
) -> Result<Vec<TenMinuteObservation>, String> {
    let doc: CoverageDoc =
        serde_json::from_slice(body).map_err(|e| format!("not CoverageJSON: {e}"))?;
    let mut coverages = doc.coverages;
    if let Some(domain) = doc.domain {
        coverages.push(Coverage {
            domain,
            ranges: doc.ranges.unwrap_or_default(),
        });
    }
    let mut out: Vec<TenMinuteObservation> = Vec::new();
    for c in coverages {
        let times = &c.domain.axes.t.values;
        let series = |name: &str| -> Result<Vec<Option<f64>>, String> {
            match c.ranges.get(name) {
                None => Ok(vec![None; times.len()]),
                Some(r) if r.values.len() == times.len() => Ok(r.values.clone()),
                Some(r) => Err(format!(
                    "{name} has {} values for {} times",
                    r.values.len(),
                    times.len()
                )),
            }
        };
        let (mean, max) = (series(mean_parameter)?, series(max_parameter)?);
        for (k, t) in times.iter().enumerate() {
            let at = DateTime::parse_from_rfc3339(t)
                .map_err(|e| format!("time '{t}': {e}"))?
                .with_timezone(&Utc);
            let (mean, max) = (tenths(mean[k]), tenths(max[k]));
            if mean.is_none() && max.is_none() {
                continue;
            }
            out.push(TenMinuteObservation {
                station: station.clone(),
                provider: ProviderId::knmi(),
                interval_end: at,
                mean,
                max,
                received_at,
            });
        }
    }
    out.sort_by_key(|o| o.interval_end);
    out.dedup_by_key(|o| o.interval_end);
    Ok(out)
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

    /// The shape of the EDR API's CoverageJSON (a collection with one
    /// point series).
    const COLLECTION: &str = r#"{
      "type": "CoverageCollection",
      "coverages": [{
        "type": "Coverage",
        "domain": {
          "type": "Domain",
          "domainType": "PointSeries",
          "axes": {
            "x": {"values": [4.79]},
            "y": {"values": [52.318]},
            "t": {"values": ["2026-07-01T11:20:00Z", "2026-07-01T11:30:00Z", "2026-07-01T11:40:00Z"]}
          }
        },
        "ranges": {
          "ta": {"type": "NdArray", "dataType": "float", "axisNames": ["t", "y", "x"], "shape": [3, 1, 1], "values": [18.3, 18.64, null]},
          "tx": {"type": "NdArray", "dataType": "float", "axisNames": ["t", "y", "x"], "shape": [3, 1, 1], "values": [18.5, 18.9, null]}
        },
        "eumetnet:locationId": "0-20000-0-06240"
      }],
      "parameters": {"ta": {"type": "Parameter"}, "tx": {"type": "Parameter"}}
    }"#;

    #[test]
    fn a_collection_becomes_readings_in_tenths() {
        let got = parse_coverage(
            COLLECTION.as_bytes(),
            &eham(),
            "ta",
            "tx",
            utc("2026-07-01T11:44:00Z"),
        )
        .unwrap();
        assert_eq!(got.len(), 2, "the empty 11:40 interval is left out");
        assert_eq!(got[0].interval_end, utc("2026-07-01T11:20:00Z"));
        assert_eq!(got[0].mean, Some(TempC::from_tenths(183)));
        assert_eq!(got[1].mean, Some(TempC::from_tenths(186)), "18.64 → 18.6");
        assert_eq!(got[1].max, Some(TempC::from_tenths(189)));
        assert_eq!(got[1].provider, ProviderId::knmi());
        assert!((got[1].delay_minutes() - 14.0).abs() < 1e-9);
    }

    #[test]
    fn a_single_coverage_and_odd_values_are_handled() {
        let single = r#"{
          "type": "Coverage",
          "domain": {"axes": {"t": {"values": ["2026-07-01T11:50:00Z", "2026-07-01T12:00:00Z"]}}},
          "ranges": {"ta": {"values": [19.0, 999.0]}}
        }"#;
        let got = parse_coverage(
            single.as_bytes(),
            &eham(),
            "ta",
            "tx",
            utc("2026-07-01T12:04:00Z"),
        )
        .unwrap();
        assert_eq!(got.len(), 1, "999 °C is not a temperature");
        assert_eq!(got[0].mean, Some(TempC::from_tenths(190)));
        assert_eq!(got[0].max, None, "no tx in the response");
        // A length mismatch or a non-coverage body is an error.
        let bad = r#"{"coverages":[{"domain":{"axes":{"t":{"values":["2026-07-01T11:50:00Z"]}}},"ranges":{"ta":{"values":[1.0,2.0]}}}]}"#;
        assert!(
            parse_coverage(
                bad.as_bytes(),
                &eham(),
                "ta",
                "tx",
                utc("2026-07-01T12:00:00Z")
            )
            .is_err()
        );
        assert!(
            parse_coverage(b"<html>", &eham(), "ta", "tx", utc("2026-07-01T12:00:00Z")).is_err()
        );
    }

    #[test]
    fn any_parameters_become_series_points() {
        let body = r#"{
          "type": "CoverageCollection",
          "coverages": [{
            "domain": {"axes": {"t": {"values": ["2026-07-01T11:20:00Z", "2026-07-01T11:30:00Z", "2026-07-01T11:40:00Z"]}}},
            "ranges": {
              "qg": {"values": [612.5, null, 88.0]},
              "dd": {"values": [240.0, 250.0, null]}
            }
          }]
        }"#;
        let got = parse_series(body.as_bytes(), &["qg", "dd", "ff"]).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].get("qg"), Some(612.5));
        assert_eq!(got[0].get("dd"), Some(240.0));
        assert_eq!(got[1].get("qg"), None, "a null is no value");
        assert_eq!(got[1].get("dd"), Some(250.0));
        assert_eq!(got[2].get("dd"), None);
        assert_eq!(got[2].interval_end, utc("2026-07-01T11:40:00Z"));
        // No value at all leaves the time out; a parameter not asked for is
        // ignored; a length mismatch is an error.
        let sparse = r#"{"domain":{"axes":{"t":{"values":["2026-07-01T11:50:00Z","2026-07-01T12:00:00Z"]}}},"ranges":{"qg":{"values":[null,10.0]},"ta":{"values":[1.0,2.0]}}}"#;
        let got = parse_series(sparse.as_bytes(), &["qg"]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].values.len(), 1);
        let bad = r#"{"coverages":[{"domain":{"axes":{"t":{"values":["2026-07-01T11:50:00Z"]}}},"ranges":{"qg":{"values":[1.0,2.0]}}}]}"#;
        assert!(parse_series(bad.as_bytes(), &["qg"]).is_err());
        assert!(parse_series(b"<html>", &["qg"]).is_err());
        // Round trip through the research cache.
        let json = serde_json::to_string(&got).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<SeriesPoint>>(&json).unwrap(),
            got
        );
    }

    #[test]
    fn the_wigos_id_of_schiphol() {
        assert_eq!(wigos_id("06240"), "0-20000-0-06240");
    }
}
