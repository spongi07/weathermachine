//! Historical observation import.
//!
//! Iowa Environmental Mesonet (IEM) ASOS/METAR archive CSV (network
//! `NL__ASOS` for EHAM). Request only the raw METAR column (`data=metar`):
//!
//! ```text
//! station,valid,metar
//! EHAM,2024-07-01 00:25,EHAM 010025Z AUTO 25007KT 9999 NCD 15/13 Q1015 NOSIG
//! ```
//!
//! Temperatures are taken from *our* parse of the raw METAR (exact whole °C),
//! never from IEM's derived °F columns. Historical knowledge time is set
//! conservatively to `observed_at + publication_delay`.

use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use std::io::Read;
use wm_core::hash::sha256_hex;
use wm_core::ids::{ProviderId, StationId};
use wm_core::weather::{Observation, ObservationKey, QualityFlags, TempPrecision};
use wm_weather::metar::{PARSER_VERSION, canonicalize, parse_metar};

/// Import statistics (reported, never silently dropped).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportStats {
    pub rows: usize,
    pub imported: usize,
    pub skipped_comment: usize,
    pub skipped_station: usize,
    pub skipped_unparseable: usize,
    pub skipped_no_temperature: usize,
    pub duplicates: usize,
}

/// Parse an IEM CSV export into observations for `station`.
pub fn import_iem_csv<R: Read>(
    reader: R,
    station: &StationId,
    publication_delay: Duration,
) -> Result<(Vec<Observation>, ImportStats), String> {
    let mut stats = ImportStats::default();
    let mut rdr = csv::ReaderBuilder::new()
        .comment(Some(b'#'))
        .flexible(true)
        .from_reader(reader);
    let headers = rdr.headers().map_err(|e| e.to_string())?.clone();
    let idx = |name: &str| {
        headers
            .iter()
            .position(|h| h.trim().eq_ignore_ascii_case(name))
    };
    let (Some(i_station), Some(i_valid), Some(i_metar)) =
        (idx("station"), idx("valid"), idx("metar"))
    else {
        return Err(format!(
            "expected columns station,valid,metar; got {headers:?}"
        ));
    };
    let provider = ProviderId::new("iem").map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| e.to_string())?;
        stats.rows += 1;
        let st = rec.get(i_station).unwrap_or_default().trim();
        if !st.eq_ignore_ascii_case(station.as_str()) {
            stats.skipped_station += 1;
            continue;
        }
        let valid = rec.get(i_valid).unwrap_or_default().trim();
        let Ok(naive) = NaiveDateTime::parse_from_str(valid, "%Y-%m-%d %H:%M") else {
            stats.skipped_unparseable += 1;
            continue;
        };
        let valid_at = Utc.from_utc_datetime(&naive);
        let raw = rec.get(i_metar).unwrap_or_default().trim();
        let Ok(m) = parse_metar(raw) else {
            stats.skipped_unparseable += 1;
            continue;
        };
        let observed_at = m
            .observed_at(valid_at + Duration::minutes(30))
            .unwrap_or(valid_at);
        let Some((temp, precision)) = m.temperature() else {
            stats.skipped_no_temperature += 1;
            continue;
        };
        let canonical = canonicalize(raw);
        if !seen.insert((observed_at, m.report_type, canonical.clone())) {
            stats.duplicates += 1;
            continue;
        }
        out.push(Observation {
            key: ObservationKey {
                station: station.clone(),
                observed_at,
                report_type: m.report_type,
            },
            version: 1,
            temperature: Some(temp),
            dewpoint: m.dewpoint(),
            precision,
            raw_text: raw.to_owned(),
            content_hash: sha256_hex(canonical.as_bytes()),
            provider: provider.clone(),
            provider_receipt_at: None,
            fetched_at: observed_at + publication_delay,
            parser_version: PARSER_VERSION,
            quality: QualityFlags {
                auto: m.auto,
                correction_marker: m.cor,
                ..QualityFlags::default()
            },
        });
        stats.imported += 1;
    }
    out.sort_by_key(|o| (o.key.observed_at, o.key.report_type));
    let _ = TempPrecision::WholeDegree;
    Ok((out, stats))
}

/// Latest observation time in a set (for gap reporting).
pub fn coverage(observations: &[Observation]) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some((
        observations.first()?.key.observed_at,
        observations.last()?.key.observed_at,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_iem_metar_csv() {
        let csv = "#DEBUG: Format Typ    -> comma\nstation,valid,metar\nEHAM,2024-07-01 12:25,EHAM 011225Z 24012KT 9999 FEW030 21/12 Q1016 NOSIG\nEHAM,2024-07-01 12:55,EHAM 011255Z 24012KT 9999 FEW030 22/12 Q1016 NOSIG\nEHAM,2024-07-01 12:55,EHAM 011255Z 24012KT 9999 FEW030 22/12 Q1016 NOSIG\nEGLL,2024-07-01 12:50,EGLL 011250Z 24012KT 9999 19/11 Q1015\nEHAM,2024-07-01 13:25,EHAM 011325Z NIL\nEHAM,bad,EHAM 011355Z 22/12\n";
        let (obs, st) = import_iem_csv(
            csv.as_bytes(),
            &StationId::new("EHAM").unwrap(),
            Duration::minutes(5),
        )
        .unwrap();
        assert_eq!(obs.len(), 2);
        assert_eq!(st.duplicates, 1);
        assert_eq!(st.skipped_station, 1);
        assert_eq!(st.skipped_no_temperature, 1);
        assert_eq!(st.skipped_unparseable, 1);
        assert_eq!(obs[1].temperature.unwrap().tenths(), 220);
        assert_eq!(
            obs[1].fetched_at - obs[1].key.observed_at,
            Duration::minutes(5)
        );
        assert!(
            import_iem_csv(
                "a,b\n1,2\n".as_bytes(),
                &StationId::new("EHAM").unwrap(),
                Duration::zero()
            )
            .is_err()
        );
    }
}
