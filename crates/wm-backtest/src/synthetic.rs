//! Deterministic synthetic weather (demo mode and tests only).
//!
//! A diurnal temperature cycle with seeded noise, reported as whole-degree
//! METARs at HH:25/HH:55 UTC. Clearly labelled; never mixed with real data.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use wm_core::hash::sha256_hex;
use wm_core::ids::{ProviderId, StationId};
use wm_core::rng::SplitMix64;
use wm_core::time::local_day_bounds;
use wm_core::units::TempC;
use wm_core::weather::{
    Observation, ObservationKey, QualityFlags, ReportType, TempPrecision, TenMinuteObservation,
};

/// Parameters of a synthetic day.
#[derive(Debug, Clone, Copy)]
pub struct SyntheticDay {
    pub date: NaiveDate,
    pub tz: Tz,
    /// Daily minimum/maximum of the underlying (tenths) signal.
    pub min_tenths: i32,
    pub max_tenths: i32,
    /// Local hour of the underlying maximum.
    pub peak_hour: f64,
    pub noise_tenths: i32,
    pub seed: u64,
}

impl SyntheticDay {
    /// Plausible Amsterdam summer/winter defaults derived from the date and seed.
    pub fn amsterdam(date: NaiveDate, seed: u64) -> Self {
        let mut rng =
            SplitMix64::new(seed ^ u64::from(date.ordinal()) ^ (date.year() as u64) << 20);
        let doy = f64::from(date.ordinal());
        let seasonal = (2.0 * std::f64::consts::PI * (doy - 200.0) / 365.0).cos();
        let base_max = 140.0 + 90.0 * seasonal + (rng.next_f64() - 0.5) * 60.0;
        let range = 60.0 + rng.next_f64() * 50.0;
        Self {
            date,
            tz: chrono_tz::Europe::Amsterdam,
            min_tenths: (base_max - range) as i32,
            max_tenths: base_max as i32,
            peak_hour: 14.0 + (rng.next_f64() - 0.5) * 3.0,
            noise_tenths: 4,
            seed,
        }
    }

    /// Underlying signal at a local fractional hour (tenths).
    pub fn signal(&self, local_hour: f64) -> f64 {
        let amp = f64::from(self.max_tenths - self.min_tenths) / 2.0;
        let mid = f64::from(self.max_tenths + self.min_tenths) / 2.0;
        // Asymmetric diurnal curve: slow rise from ~05:00, faster decline after the peak.
        let x = local_hour - self.peak_hour;
        let phase = if x <= 0.0 { x / 9.0 } else { x / 13.0 };
        mid + amp * (std::f64::consts::PI * phase).cos()
    }

    /// Observations at HH:25 and HH:55 UTC covering the local day.
    pub fn observations(
        &self,
        station: &StationId,
        publication_delay: Duration,
    ) -> Vec<Observation> {
        let (start, end) = local_day_bounds(self.date, self.tz);
        let mut rng =
            SplitMix64::new(self.seed.wrapping_mul(0x9E37_79B9) ^ u64::from(self.date.ordinal()));
        let mut t = start - Duration::minutes(i64::from(start.minute())) + Duration::minutes(25);
        let mut out = Vec::new();
        let provider = ProviderId::synthetic();
        while t < end {
            if t >= start {
                let local = t.with_timezone(&self.tz);
                let hour = f64::from(local.hour()) + f64::from(local.minute()) / 60.0;
                let noise = (rng.next_f64() - 0.5) * 2.0 * f64::from(self.noise_tenths);
                let tenths = self.signal(hour) + noise;
                let whole = TempC::from_tenths(tenths.round() as i32).round_half_up_whole();
                out.push(make_obs(station, t, whole, &provider, publication_delay));
            }
            t = next_slot(t);
        }
        out
    }

    /// Ten-minute readings of the same signal, as an automatic weather
    /// station publishes them (KNMI-style): the mean and the maximum of the
    /// interval's one-minute values, to a tenth, one per interval ending on
    /// the local day's tens of minutes, known `delay` after the interval.
    /// Labelled synthetic.
    pub fn ten_minute_readings(
        &self,
        station: &StationId,
        delay: Duration,
    ) -> Vec<TenMinuteObservation> {
        let (start, end) = local_day_bounds(self.date, self.tz);
        let mut rng =
            SplitMix64::new(self.seed.wrapping_mul(0x2545_F491) ^ u64::from(self.date.ordinal()));
        let hour = |t: DateTime<Utc>| {
            let local = t.with_timezone(&self.tz);
            f64::from(local.hour()) + f64::from(local.minute()) / 60.0
        };
        let mut out = Vec::new();
        let mut t = start + Duration::minutes(10);
        while t < end {
            let minutes: Vec<f64> = (0..10)
                .map(|m| self.signal(hour(t - Duration::minutes(9 - m))))
                .collect();
            let mean = minutes.iter().sum::<f64>() / 10.0;
            let max = minutes.iter().copied().fold(f64::MIN, f64::max);
            // An average of ten values: less noise than a METAR's single
            // reading; the maximum a little above the smooth curve.
            let noise = (rng.next_f64() - 0.5) * 2.0;
            let gust = rng.next_f64() * 3.0;
            let mean = TempC::from_tenths((mean + noise).round() as i32);
            let max = TempC::from_tenths((max + noise + gust).round() as i32).max(mean);
            out.push(TenMinuteObservation {
                station: station.clone(),
                provider: ProviderId::synthetic(),
                interval_end: t,
                mean: Some(mean),
                max: Some(max),
                radiation: None,
                received_at: t + delay,
            });
            t += Duration::minutes(10);
        }
        out
    }
}

fn next_slot(t: DateTime<Utc>) -> DateTime<Utc> {
    t + Duration::minutes(30)
}

fn make_obs(
    station: &StationId,
    t: DateTime<Utc>,
    whole: i32,
    provider: &ProviderId,
    delay: Duration,
) -> Observation {
    let temp = if whole < 0 {
        format!("M{:02}", -whole)
    } else {
        format!("{whole:02}")
    };
    let raw = format!(
        "{station} {} 24010KT 9999 FEW035 {temp}/{:02} Q1015 NOSIG",
        t.format("%d%H%MZ"),
        (whole - 6).clamp(0, 99)
    );
    Observation {
        key: ObservationKey {
            station: station.clone(),
            observed_at: t,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_whole(whole)),
        dewpoint: None,
        precision: TempPrecision::WholeDegree,
        content_hash: sha256_hex(raw.as_bytes()),
        raw_text: raw,
        provider: provider.clone(),
        provider_receipt_at: None,
        fetched_at: t + delay,
        parser_version: 1,
        quality: QualityFlags::default(),
    }
}

/// Synthetic multi-day history (seeded, deterministic).
pub fn synthetic_history(
    station: &StationId,
    from: NaiveDate,
    days: u32,
    seed: u64,
    delay: Duration,
) -> Vec<Observation> {
    (0..days)
        .filter_map(|i| from.checked_add_days(chrono::Days::new(u64::from(i))))
        .flat_map(|d| SyntheticDay::amsterdam(d, seed).observations(station, delay))
        .collect()
}

// ---------------------------------------------------------------------------
// Synthetic market data (plumbing tests and demo only — NOT evidence of edge)
// ---------------------------------------------------------------------------

use wm_core::event::{
    EventEnvelope, EventSource, MarketSnapshotEvent, ObservationEvent, OrderBookEvent,
    WeatherMachineEvent,
};
use wm_core::ids::LocationId;
use wm_core::market::DailyTemperatureMarket;
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::units::Price;
use wm_core::weather::DedupClass;

/// Naive "market" belief over the final high given the running high `x` at a
/// local hour: mass shifts from x+1/x+2 toward x as the afternoon progresses.
fn naive_market_probs(x: i32, local_hour: f64, rng: &mut SplitMix64) -> Vec<(i32, f64)> {
    let lateness = ((local_hour - 11.0) / 7.0).clamp(0.0, 1.0);
    let p0 = 0.35 + 0.60 * lateness + (rng.next_f64() - 0.5) * 0.06;
    let p1 = (1.0 - p0) * 0.65;
    let p2 = (1.0 - p0) * 0.25;
    let p3 = (1.0 - p0) * 0.10;
    vec![
        (x, p0.clamp(0.02, 0.98)),
        (x + 1, p1),
        (x + 2, p2),
        (x + 3, p3),
    ]
}

fn quote(p: f64) -> (String, String) {
    let mid = (p * 100.0).round().clamp(2.0, 98.0);
    (
        format!("{:.2}", (mid - 1.0) / 100.0),
        format!("{:.2}", (mid + 1.0) / 100.0),
    )
}

/// Events for one synthetic trading day: a market snapshot, observations and
/// books re-quoted two minutes after each observation.
pub fn synthetic_trading_day(
    location: &LocationId,
    station: &StationId,
    day: &SyntheticDay,
    pub_delay: Duration,
) -> (DailyTemperatureMarket, Vec<EventEnvelope>) {
    let (start, _) = local_day_bounds(day.date, day.tz);
    let obs = day.observations(station, pub_delay);
    let max_whole = obs
        .iter()
        .filter_map(|o| o.temperature)
        .map(|t| t.round_half_up_whole())
        .max()
        .unwrap_or(15);
    let lo = max_whole - 6;
    let hi = max_whole + 5;
    let market = synthetic_temperature_market(location, station, day.date, day.tz, lo, hi, start);
    let mut rng = SplitMix64::new(day.seed ^ 0xB00C);
    let mut events = vec![EventEnvelope::new(
        start + Duration::hours(6),
        EventSource::Synthetic,
        WeatherMachineEvent::MarketSnapshot(MarketSnapshotEvent {
            market: market.clone(),
        }),
    )];
    let mut running = i32::MIN;
    for o in obs {
        let whole = o
            .temperature
            .map(|t| t.round_half_up_whole())
            .unwrap_or(running);
        running = running.max(whole);
        let local = o.key.observed_at.with_timezone(&day.tz);
        let hour = f64::from(local.hour()) + f64::from(local.minute()) / 60.0;
        let avail = o.fetched_at;
        events.push(EventEnvelope::new(
            avail,
            EventSource::Synthetic,
            WeatherMachineEvent::WeatherObservation(ObservationEvent {
                observation: o,
                class: DedupClass::New,
            }),
        ));
        if o_is_trading_hour(hour) {
            let quotes_at = avail + Duration::minutes(2);
            let probs = naive_market_probs(running, hour, &mut rng);
            for outcome in &market.outcomes {
                let p: f64 = probs
                    .iter()
                    .filter(|(v, _)| outcome.bucket.contains(*v))
                    .map(|(_, p)| p)
                    .sum::<f64>()
                    .clamp(0.01, 0.99);
                let (yb, ya) = quote(p);
                let (nb, na) = quote(1.0 - p);
                for (tok, b, a) in [(&outcome.yes_token, yb, ya), (&outcome.no_token, nb, na)] {
                    let book = synthetic_book(tok, Some(&b), Some(&a), 250, quotes_at);
                    events.push(EventEnvelope::new(
                        quotes_at,
                        EventSource::Synthetic,
                        WeatherMachineEvent::OrderBookUpdate(OrderBookEvent { book }),
                    ));
                }
            }
        }
    }
    let _ = Price::ZERO;
    (market, events)
}

fn o_is_trading_hour(hour: f64) -> bool {
    (8.0..22.0).contains(&hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_plausible() {
        let st = StationId::new("EHAM").unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let a = SyntheticDay::amsterdam(d, 7).observations(&st, Duration::minutes(3));
        let b = SyntheticDay::amsterdam(d, 7).observations(&st, Duration::minutes(3));
        assert_eq!(a, b);
        assert_eq!(a.len(), 48);
        assert!(
            a.iter()
                .all(|o| matches!(o.key.observed_at.minute(), 25 | 55))
        );
        let max = a.iter().filter_map(|o| o.temperature).max().unwrap();
        let min = a.iter().filter_map(|o| o.temperature).min().unwrap();
        assert!(max > min);
        let peak = a
            .iter()
            .max_by_key(|o| o.temperature)
            .unwrap()
            .key
            .observed_at
            .with_timezone(&chrono_tz::Europe::Amsterdam)
            .hour();
        assert!((10..=18).contains(&peak), "peak hour {peak}");
        for o in &a {
            assert!(
                wm_weather::metar::parse_metar(&o.raw_text).is_ok(),
                "{}",
                o.raw_text
            );
        }
    }

    #[test]
    fn ten_minute_readings_follow_the_reported_curve() {
        let st = StationId::new("EHAM").unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let day = SyntheticDay::amsterdam(d, 7);
        let r = day.ten_minute_readings(&st, Duration::minutes(5));
        assert_eq!(r, day.ten_minute_readings(&st, Duration::minutes(5)));
        assert_eq!(r.len(), 24 * 6 - 1, "every ten minutes, midnight excluded");
        assert!(r.iter().all(|o| o.interval_end.minute() % 10 == 0
            && o.received_at == o.interval_end + Duration::minutes(5)
            && o.max >= o.mean
            && o.provider == ProviderId::synthetic()));
        // The day's highest ten-minute mean is the METARs' high within the
        // METAR's noise and rounding.
        let mean_high = r.iter().filter_map(|o| o.mean).max().unwrap();
        let metar_high = day
            .observations(&st, Duration::minutes(3))
            .iter()
            .filter_map(|o| o.temperature)
            .max()
            .unwrap();
        assert!(
            (mean_high.tenths() - metar_high.tenths()).abs() <= 10,
            "{mean_high:?} vs {metar_high:?}"
        );
    }

    #[test]
    fn multi_day_history() {
        let st = StationId::new("EHAM").unwrap();
        let h = synthetic_history(
            &st,
            NaiveDate::from_ymd_opt(2026, 3, 28).unwrap(),
            3,
            1,
            Duration::minutes(3),
        );
        // 47 + 46 (23 h DST day) + 48 reports.
        assert!(h.len() >= 140 && h.len() <= 144, "{}", h.len());
    }
}
