//! The sun's position and the clear sky (L25's clear-sky index), and the
//! bearing between two stations (L24's upwind neighbours). Shared by the
//! paper strategies and `research market`'s replay.

use chrono::{DateTime, Datelike, Timelike, Utc};

/// Cosine of the solar zenith angle at `t` (NOAA's fractional-year
/// approximation, good to a few tenths of a degree).
pub fn cos_zenith(t: DateTime<Utc>, latitude: f64, longitude: f64) -> f64 {
    use std::f64::consts::PI;
    let hour = f64::from(t.hour()) + f64::from(t.minute()) / 60.0 + f64::from(t.second()) / 3600.0;
    let g = 2.0 * PI / 365.0 * (f64::from(t.ordinal()) - 1.0 + (hour - 12.0) / 24.0);
    let decl = 0.006918 - 0.399912 * g.cos() + 0.070257 * g.sin() - 0.006758 * (2.0 * g).cos()
        + 0.000907 * (2.0 * g).sin()
        - 0.002697 * (3.0 * g).cos()
        + 0.00148 * (3.0 * g).sin();
    let eq_time = 229.18
        * (0.000075 + 0.001868 * g.cos()
            - 0.032077 * g.sin()
            - 0.014615 * (2.0 * g).cos()
            - 0.040849 * (2.0 * g).sin());
    let true_solar_min = hour * 60.0 + eq_time + 4.0 * longitude;
    let hour_angle = (true_solar_min / 4.0 - 180.0).to_radians();
    let lat = latitude.to_radians();
    lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos()
}

/// Clear-sky global horizontal irradiance (W/m²), Haurwitz (1945):
/// 1098 · cos z · exp(−0.057 / cos z); zero with the sun down.
pub fn clear_sky_ghi(t: DateTime<Utc>, latitude: f64, longitude: f64) -> f64 {
    let c = cos_zenith(t, latitude, longitude);
    if c <= 0.0 {
        0.0
    } else {
        1098.0 * c * (-0.057 / c).exp()
    }
}

/// Initial bearing (degrees true, 0–360) from `(lat1, lon1)` to
/// `(lat2, lon2)`.
pub fn bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Smallest angle between two bearings (degrees, 0–180).
pub fn angle_between(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_clear_sky_peaks_near_noon_and_vanishes_at_night() {
        // Schiphol, 21 June: the sun ~61° up at solar noon (~11:40 UTC).
        let (lat, lon) = (52.318, 4.790);
        let noon = clear_sky_ghi(utc("2026-06-21T11:40:00Z"), lat, lon);
        assert!((800.0..950.0).contains(&noon), "{noon}");
        assert!(clear_sky_ghi(utc("2026-06-21T07:00:00Z"), lat, lon) < noon);
        assert_eq!(clear_sky_ghi(utc("2026-06-21T23:00:00Z"), lat, lon), 0.0);
        // Winter noon is much lower.
        let winter = clear_sky_ghi(utc("2026-12-21T11:45:00Z"), lat, lon);
        assert!((150.0..350.0).contains(&winter), "{winter}");
    }

    #[test]
    fn bearings_and_angles() {
        // Schiphol → Voorschoten (south-west), De Bilt (south-east), Berkhout (north).
        let b = |lat: f64, lon: f64| bearing(52.318, 4.790, lat, lon);
        assert!((b(52.141, 4.437) - 231.0).abs() < 2.0);
        assert!((b(52.100, 5.180) - 132.0).abs() < 3.0);
        assert!((b(52.644, 4.979) - 19.0).abs() < 3.0);
        assert_eq!(angle_between(350.0, 10.0), 20.0);
        assert_eq!(angle_between(10.0, 350.0), 20.0);
        assert_eq!(angle_between(90.0, 270.0), 180.0);
    }
}
