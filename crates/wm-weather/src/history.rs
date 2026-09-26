//! Historical METARs from the Iowa Environmental Mesonet (IEM) ASOS/METAR
//! archive (`/cgi-bin/request/asos.py`), used only to train the probability
//! model — never as live or resolution data.
//!
//! IEM throttles each IP to one request per second and caps a request at
//! 1,000 station-years. Weather Machine asks for one station-year per request,
//! strictly one at a time, through the provider gate, whose configured spacing
//! is far above IEM's floor. Completed years are cached by the caller, so each
//! is downloaded once.

use chrono::{Datelike, NaiveDate};
use std::sync::Arc;
use std::time::Duration;
use wm_core::ids::{ProviderId, StationId};
use wm_core::ingest::ProviderRequestRecord;
use wm_net::{FetchError, FetchRequest, HttpFetcher};

/// Column header of an IEM `data=metar` CSV (`format=onlycomma`).
pub const IEM_HEADER: &str = "station,valid,metar";

/// Why a year could not be downloaded.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// HTTP succeeded but the body is not an IEM METAR CSV (e.g. an error page).
    #[error("unexpected response: {0}")]
    Malformed(String),
}

impl HistoryError {
    /// Audit record of the request, when one reached the network.
    pub fn record(&self) -> Option<&ProviderRequestRecord> {
        match self {
            HistoryError::Fetch(e) => e.record(),
            HistoryError::Malformed(_) => None,
        }
    }
}

/// One downloaded station-year.
#[derive(Debug, Clone)]
pub struct HistoryYear {
    pub year: i32,
    /// Exclusive end date of the request (1 January of the next year, or the
    /// requested cut-off for the current year).
    pub until: NaiveDate,
    pub csv: Vec<u8>,
    /// Data rows (excluding the header).
    pub rows: usize,
    pub record: ProviderRequestRecord,
}

/// Client for the IEM ASOS/METAR archive.
pub struct IemArchive {
    fetcher: Arc<HttpFetcher>,
    base_url: String,
}

impl IemArchive {
    pub fn new(fetcher: Arc<HttpFetcher>, base_url: impl Into<String>) -> Self {
        Self {
            fetcher,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn provider(&self) -> &ProviderId {
        self.fetcher.provider()
    }

    /// Request URL for `[1 Jan year, until)` in UTC. Report types 3 (routine)
    /// and 4 (specials) together cover every METAR and SPECI.
    pub fn year_url(&self, station: &StationId, year: i32, until: NaiveDate) -> String {
        format!(
            "{}/cgi-bin/request/asos.py?station={station}&data=metar&year1={year}&month1=1&day1=1&year2={}&month2={}&day2={}&tz=Etc/UTC&format=onlycomma&latlon=no&elev=no&missing=M&trace=T&direct=no&report_type=3&report_type=4",
            self.base_url,
            until.year(),
            until.month(),
            until.day(),
        )
    }

    /// Download one station-year. `until` is the exclusive end: 1 January of
    /// the next year for a complete year, or today for the current one.
    /// `max_gate_wait` bounds the wait for the rate-limit gate.
    pub async fn fetch_year(
        &self,
        station: &StationId,
        year: i32,
        until: NaiveDate,
        max_gate_wait: Duration,
    ) -> Result<HistoryYear, HistoryError> {
        let url = self.year_url(station, year, until);
        let req = FetchRequest::get(
            url,
            format!("/cgi-bin/request/asos.py?station={station}&year1={year}"),
        )
        .station(station.clone())
        .accept("text/csv, text/plain")
        .max_gate_wait(max_gate_wait)
        .unconditional();
        let resp = self.fetcher.get(&req).await?;
        let rows = validate_csv(&resp.body)?;
        Ok(HistoryYear {
            year,
            until,
            csv: resp.body.to_vec(),
            rows,
            record: resp.record,
        })
    }
}

/// Check that a body is an IEM METAR CSV and count its data rows. IEM answers
/// some errors with HTTP 200 and a text message, which must never be cached
/// as "a year without data".
pub fn validate_csv(body: &[u8]) -> Result<usize, HistoryError> {
    let text = std::str::from_utf8(body)
        .map_err(|_| HistoryError::Malformed("body is not UTF-8".into()))?;
    let mut lines = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    match lines.next() {
        Some(h) if h.trim().eq_ignore_ascii_case(IEM_HEADER) => Ok(lines.count()),
        Some(h) => Err(HistoryError::Malformed(format!(
            "expected header {IEM_HEADER:?}, got {:?}",
            h.chars().take(120).collect::<String>()
        ))),
        None => Err(HistoryError::Malformed("empty body".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_validation_counts_rows_and_rejects_error_pages() {
        let ok = b"station,valid,metar\nEHAM,2024-07-01 00:25,EHAM 010025Z AUTO 25007KT 9999 NCD 15/13 Q1015 NOSIG\n";
        assert_eq!(validate_csv(ok).unwrap(), 1);
        assert_eq!(validate_csv(b"station,valid,metar\n").unwrap(), 0);
        assert_eq!(
            validate_csv(b"#DEBUG: x\nstation,valid,metar\n").unwrap(),
            0
        );
        assert!(validate_csv(b"ERROR: too many requests").is_err());
        assert!(validate_csv(b"").is_err());
        assert!(validate_csv(&[0xff, 0xfe]).is_err());
    }
}
