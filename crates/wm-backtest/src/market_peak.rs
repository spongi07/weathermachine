//! Strategy F replayed at traded prices (`research market`).
//!
//! F's slot on a market day comes from the peak times of the METAR history
//! *before* that day (prequential, like the models); a season that history
//! does not cover yet uses the fallback slot, as live does until the model
//! carries the peak times. Between two reports the weather is what the
//! earlier report said while the book keeps moving; the live strategy
//! watches the book and buys the first time the high's bucket is offered
//! above `min_price` inside the slot. The replay follows the tape the same
//! way: at a report's knowledge time the latest taker buy of YES stands for
//! the ask, after it every taker buy does, until the next report is known
//! (or the report is older than live's data-age limit). The first one inside
//! the slot and within (`min_price`, `max_price`] is the fill — plus the
//! slippage allowance and the taker fee — for F's fixed number of shares.
//! Depth is not archived, so filling 100 shares at a price where fewer were
//! offered is optimistic.
//!
//! Variants of the rule are replayed beside it: to 0.99, an earlier and a
//! later slot, only once the temperature is 1 °C below the high, and as a
//! resting bid (maker). The best of several variants on the same days
//! overstates what to expect, so the variant that did best on the first half
//! of the market days is judged again on the second half — out of sample.

use crate::market_eval::MarketTrade;
use crate::market_makers::{Resting, cancel_time, through_fill};
use crate::market_sim::{
    Decision, MarketSimConfig, SimTrade, StrategyRow, local_hm, losses_markdown, range_label, row,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use wm_core::market::TemperatureBucket;
use wm_core::time::{local_day_bounds, local_minute_of_day};
use wm_strategy::{PeakTimes, SeasonSlots};

/// Strategy F replayed (the live `[strategies.peak_slot]` settings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeakSlotSim {
    /// F is enabled live (its rule is marked **live**).
    pub live: bool,
    pub slot_from_quantile: f64,
    pub slot_to_quantile: f64,
    /// Slots of a season the history before the day does not cover.
    pub fallback_slots: SeasonSlots,
    /// Exclusive: bought only above it.
    pub min_price: f64,
    pub max_price: f64,
    /// Shares per trade (the table's P&L is for this many).
    pub shares: f64,
    pub min_drop_tenths: i32,
    /// No buy on a report older than this (live's data-age limit).
    pub max_data_age_minutes: i64,
}

impl PeakSlotSim {
    /// The replay of the live strategy's settings.
    pub fn from_live(c: &wm_strategy::PeakSlotConfig) -> Self {
        Self {
            live: c.enabled,
            slot_from_quantile: c.slot_from_quantile,
            slot_to_quantile: c.slot_to_quantile,
            fallback_slots: c.fallback_slots,
            min_price: c.min_price.as_f64(),
            max_price: c.max_price.as_f64(),
            shares: c.shares.as_f64(),
            min_drop_tenths: c.min_drop_tenths,
            max_data_age_minutes: c.max_data_age_minutes,
        }
    }

    /// The configured rule first, then the variants that differ from it.
    pub(crate) fn rules(&self) -> Vec<FRule> {
        let live = FRule {
            label: "F".into(),
            from_q: self.slot_from_quantile,
            to_q: self.slot_to_quantile,
            min_price: self.min_price,
            max_price: self.max_price,
            min_drop_tenths: self.min_drop_tenths,
            maker: false,
            live: self.live,
        };
        let variant = |label: &str| FRule {
            label: label.into(),
            live: false,
            ..live.clone()
        };
        let mut rules = vec![live.clone()];
        for r in [
            FRule {
                max_price: 0.99,
                ..variant("F · to 0.99")
            },
            FRule {
                from_q: 0.25,
                to_q: 0.75,
                ..variant("F · earlier slot")
            },
            FRule {
                from_q: 0.50,
                to_q: 0.90,
                ..variant("F · median slot")
            },
            FRule {
                from_q: 0.75,
                to_q: 0.95,
                ..variant("F · later slot")
            },
            FRule {
                min_drop_tenths: self.min_drop_tenths.max(10),
                ..variant("F · 1 °C below")
            },
            FRule {
                maker: true,
                ..variant("F maker")
            },
        ] {
            if !rules.iter().any(|x| x.same_rule(&r)) {
                rules.push(r);
            }
        }
        rules
    }
}

impl Default for PeakSlotSim {
    fn default() -> Self {
        Self::from_live(&wm_strategy::PeakSlotConfig::default())
    }
}

/// One replayed rule of F.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FRule {
    pub(crate) label: String,
    pub(crate) from_q: f64,
    pub(crate) to_q: f64,
    pub(crate) min_price: f64,
    pub(crate) max_price: f64,
    pub(crate) min_drop_tenths: i32,
    /// A resting YES bid instead of paying the ask.
    pub(crate) maker: bool,
    pub(crate) live: bool,
}

impl FRule {
    fn same_rule(&self, o: &FRule) -> bool {
        (self.from_q - o.from_q).abs() < 1e-9
            && (self.to_q - o.to_q).abs() < 1e-9
            && (self.min_price - o.min_price).abs() < 1e-9
            && (self.max_price - o.max_price).abs() < 1e-9
            && self.min_drop_tenths == o.min_drop_tenths
            && self.maker == o.maker
    }

    pub(crate) fn range(&self) -> String {
        range_label((self.min_price, self.max_price))
    }

    /// The table key of the rule's trades.
    fn key(&self) -> (&'static str, &str, u32, String) {
        (STRUCTURE, &self.label, 0, self.range())
    }

    fn slot(&self) -> String {
        format!("{} → {}", quantile(self.from_q), quantile(self.to_q))
    }

    /// How the rule differs, for the report.
    pub(crate) fn describe(&self) -> String {
        format!(
            "slot {} quantile of the peak times, asks above {:.2} and ≤ {:.2}{}{}",
            self.slot(),
            self.min_price,
            self.max_price,
            if self.min_drop_tenths > 0 {
                format!(
                    ", the report ≥ {:.1} °C below the high",
                    f64::from(self.min_drop_tenths) / 10.0
                )
            } else {
                String::new()
            },
            if self.maker {
                ", as a resting bid at the latest taker sell"
            } else {
                ""
            }
        )
    }
}

fn quantile(q: f64) -> String {
    format!("{:.0}%", 100.0 * q)
}

/// F does not use the model's probabilities (it only needs a model), so its
/// trades carry no model structure.
pub(crate) const STRUCTURE: &str = "–";

/// Fewest replayed market days for the out-of-sample split.
const MIN_SPLIT_DAYS: usize = 20;

/// The UTC instant of a local minute of `date` (`None` in a DST gap).
fn local_to_utc(date: NaiveDate, minute: u16, tz: Tz) -> Option<DateTime<Utc>> {
    let naive = date.and_hms_opt(0, 0, 0)? + Duration::minutes(i64::from(minute));
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

/// Every F rule's trade on one market day (at most one per rule: live, the
/// risk caps leave room for one position). `peak` holds the days before.
#[allow(clippy::too_many_arguments)]
pub(crate) fn simulate_peak_slot(
    date: NaiveDate,
    buckets: &[TemperatureBucket],
    labels: &[String],
    winner: usize,
    decisions: &[Decision],
    per_bucket: &[Vec<&MarketTrade>],
    peak: &PeakTimes,
    sim: &MarketSimConfig,
    fee_rate: f64,
    routine_minutes: &[u8],
    tz: Tz,
) -> Vec<SimTrade> {
    let fee = |p: f64| fee_rate * p * (1.0 - p);
    let rebate = |p: f64| sim.maker.rebate_share * fee(p);
    let (_, day_end) = local_day_bounds(date, tz);
    let mut out = Vec::new();
    for rule in sim.f.rules() {
        if rule.maker && !sim.maker.enabled {
            continue;
        }
        for (k, d) in decisions.iter().enumerate() {
            // Like live: a model is required, not a model edge.
            let Some(dist) = &d.dists[0] else { continue };
            if d.f.drop_tenths < rule.min_drop_tenths {
                continue;
            }
            let (start, end) = peak
                .slot(d.f.season, rule.from_q, rule.to_q)
                .unwrap_or_else(|| sim.f.fallback_slots.get(d.f.season));
            let high = d.f.high_whole;
            let Some(i) = buckets.iter().position(|b| b.contains(high)) else {
                continue;
            };
            // The report stands until the next one is known, the day ends or
            // it is too old to trade on.
            let until = decisions
                .get(k + 1)
                .map_or(day_end, |n| n.knowledge)
                .min(day_end)
                .min(d.at + Duration::minutes(sim.f.max_data_age_minutes + 1));
            if until <= d.knowledge {
                continue;
            }
            let in_slot = |t: DateTime<Utc>| (start..end).contains(&local_minute_of_day(t, tz));
            let in_range = |p: f64| p > rule.min_price + 1e-9 && p <= rule.max_price + 1e-9;
            let trades = per_bucket.get(i).map_or(&[][..], Vec::as_slice);
            let q = &d.quotes[i];
            // (price, fill time when later than the knowledge time)
            let fill: Option<(f64, Option<DateTime<Utc>>)> = if rule.maker {
                // A YES bid at the latest taker sell (below any ask), from the
                // knowledge time until the slot ends or shortly before the next
                // routine report, whichever comes first.
                let Some(b) = q.yes_bid else { continue };
                if !in_slot(d.knowledge)
                    || !in_range(b)
                    || q.yes_ask.is_some_and(|a| b >= a - 1e-12)
                {
                    continue;
                }
                let cancel = [
                    cancel_time(
                        d.knowledge,
                        routine_minutes,
                        sim.maker.cancel_before_report_min,
                    ),
                    local_to_utc(date, end, tz).filter(|e| *e > d.knowledge),
                    Some(until),
                ]
                .into_iter()
                .flatten()
                .min();
                let Some(cancel) = cancel else { continue };
                through_fill(trades, d.knowledge, cancel, Resting::YesBid(b))
                    .map(|at| (b, Some(at)))
            } else {
                // The ask when the report is known, then every taker buy
                // until the next report is.
                q.yes_ask
                    .filter(|a| in_slot(d.knowledge) && in_range(*a))
                    .map(|a| (a, None))
                    .or_else(|| {
                        let from = trades.partition_point(|t| t.at <= d.knowledge);
                        trades[from..]
                            .iter()
                            .take_while(|t| t.at < until)
                            .find(|t| t.taker_buys_yes && in_slot(t.at) && in_range(t.yes_price))
                            .map(|t| (t.yes_price, Some(t.at)))
                    })
            };
            let Some((price, filled_at)) = fill else {
                continue;
            };
            let won = i == winner;
            let per_share = if rule.maker {
                f64::from(u8::from(won)) - price + rebate(price)
            } else {
                f64::from(u8::from(won)) - price - fee(price) - sim.slippage
            };
            let p_model = dist.p_in_bucket_lower(high, &buckets[i]);
            out.push(SimTrade {
                date,
                report: local_hm(d.at, tz),
                structure: STRUCTURE.to_owned(),
                strategy: rule.label.clone(),
                window: 0,
                range: rule.range(),
                bucket: labels[i].clone(),
                side: "YES".to_owned(),
                price,
                p_model,
                p_used: p_model,
                won,
                pnl_usd: sim.f.shares * per_share,
                resolved: labels[winner].clone(),
                filled: filled_at.map(|t| local_hm(t, tz)),
            });
            break;
        }
    }
    out
}

/// Rows of the F rules, the configured one first.
pub(crate) fn peak_rows(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    iterations: usize,
    seed: u64,
) -> Vec<StrategyRow> {
    sim.f
        .rules()
        .iter()
        .map(|r| row(trades, r.key(), r.live, iterations, seed))
        .collect()
}

/// The out-of-sample check: the F rule with the best P&L on the first half
/// of the replayed market days (the configured rule on a tie), and what it
/// made on the second half. `None` without a replayed day.
pub(crate) fn out_of_sample(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    replayed: &[NaiveDate],
    iterations: usize,
    seed: u64,
) -> Option<String> {
    let mut dates = replayed.to_vec();
    dates.sort_unstable();
    dates.dedup();
    if dates.is_empty() {
        return None;
    }
    if dates.len() < MIN_SPLIT_DAYS {
        return Some(format!(
            "Strategy F out of sample: {} replayed market days are too few to choose a rule on one half and test it on the other (at least {MIN_SPLIT_DAYS} needed).",
            dates.len()
        ));
    }
    let before = dates.len() / 2;
    let split = dates[before];
    let half = |label: &str, later: bool| -> Vec<SimTrade> {
        trades
            .iter()
            .filter(|t| {
                t.structure == STRUCTURE && t.strategy == label && (t.date >= split) == later
            })
            .cloned()
            .collect()
    };
    let total = |ts: &[SimTrade]| ts.iter().map(|t| t.pnl_usd).sum::<f64>();
    let rules = sim.f.rules();
    let mut best: Option<(&FRule, Vec<SimTrade>)> = None;
    for r in &rules {
        let first = half(&r.label, false);
        if first.is_empty() {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(_, b)| total(&first) > total(b) + 1e-9)
        {
            best = Some((r, first));
        }
    }
    let Some((best, first)) = best else {
        return Some(format!(
            "Strategy F out of sample: no F rule traded on the first {before} market days ({} → {}), so none could be chosen.",
            dates[0],
            dates[before - 1]
        ));
    };
    let r = row(
        &half(&best.label, true),
        best.key(),
        false,
        iterations,
        seed,
    );
    Some(format!(
        "Strategy F out of sample: on the first {before} market days ({} → {}) the best F rule was *{}* ({} trades, {} won, ${:+.2}); on the {} later days ({} → {}) the same rule made {} trades, {} won, ${:+.2} (${:+.2} per trade, 95% CI {:+.2} … {:+.2}). Judge F by this line, not by the best row of its table.",
        dates[0],
        dates[before - 1],
        best.label,
        first.len(),
        first.iter().filter(|t| t.won).count(),
        total(&first),
        dates.len() - before,
        split,
        dates[dates.len() - 1],
        r.trades,
        r.wins,
        r.total_usd,
        r.pnl_per_trade,
        r.ci_low,
        r.ci_high
    ))
}

/// F's line of the strategy verdict: the configured rule and its variants.
pub(crate) fn verdict(rows: &[StrategyRow], sim: &MarketSimConfig) -> Vec<String> {
    let rules = sim.f.rules();
    let find = |r: &FRule| {
        rows.iter()
            .find(|x| x.structure == STRUCTURE && x.strategy == r.label && x.range == r.range())
    };
    let Some((rule, live)) = rules.first().and_then(|r| find(r).map(|x| (r, x))) else {
        return Vec::new();
    };
    let variants: Vec<String> = rules
        .iter()
        .skip(1)
        .filter_map(|r| {
            find(r).map(|x| {
                if x.trades == 0 {
                    format!("{} no trade", r.label)
                } else {
                    format!(
                        "{} {} trades, {} won, ${:+.2}",
                        r.label, x.trades, x.wins, x.total_usd
                    )
                }
            })
        })
        .collect();
    vec![format!(
        "Strategy F{} ({}; {:.0} shares a trade): {} trades, {} won, ${:+.2} (${:+.2} per trade, 95% CI {:+.2} … {:+.2}){}{}.",
        if sim.f.live { "" } else { " (disabled live)" },
        rule.describe(),
        sim.f.shares,
        live.trades,
        live.wins,
        live.total_usd,
        live.pnl_per_trade,
        live.ci_low,
        live.ci_high,
        if variants.is_empty() { "" } else { "; " },
        variants.join("; ")
    )]
}

/// F's section of the market report.
pub(crate) fn markdown(
    rows: &[StrategyRow],
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    verdict: &[String],
    out_of_sample: Option<&str>,
) -> String {
    let f = &sim.f;
    let rules = f.rules();
    let mut s = format!(
        "\n## Strategy F at traded prices\n\nF buys YES on the bucket holding the day's high inside the season's *peak slot* — between two quantiles of the local time at which the METAR days before the market day first reported their high (the table above, as it stood on each day; the fallback slot where a season had no such day) — once that bucket is offered above {:.2}: {:.0} shares at once, one trade a day. Between reports it watches the tape as live watches the book: the latest taker buy of YES when a report is known, then every taker buy until the next report is known, fills it (plus {:.3} slippage and the taker fee). Depth is not archived, so a fill of {:.0} shares where fewer were offered is optimistic. Its rows are for **{:.0} shares a trade** (up to ${:.0} at risk), not $10. Rules: {}.\n\n",
        f.min_price,
        f.shares,
        sim.slippage,
        f.shares,
        f.shares,
        f.shares * f.max_price,
        rules
            .iter()
            .map(|r| format!("*{}* — {}", r.label, r.describe()))
            .collect::<Vec<_>>()
            .join("; ")
    );
    for line in verdict.iter().map(String::as_str).chain(out_of_sample) {
        let _ = writeln!(s, "* {line}");
    }
    s.push_str("\n| rule | slot | asks | trades | won | days | mean price | P&L per trade | 95% CI | total |\n|---|---|---|---:|---:|---:|---:|---:|---|---:|\n");
    for rule in &rules {
        let Some(r) = rows
            .iter()
            .find(|x| x.strategy == rule.label && x.range == rule.range())
        else {
            continue;
        };
        let _ = writeln!(
            s,
            "| {}{} | {} | {} | {} | {} | {} | {:.3} | {:+.2} | [{:+.2}, {:+.2}] | {:+.2} |",
            rule.label,
            if r.live { " **live**" } else { "" },
            rule.slot(),
            r.range,
            r.trades,
            r.wins,
            r.days,
            r.mean_price,
            r.pnl_per_trade,
            r.ci_low,
            r.ci_high,
            r.total_usd
        );
    }
    s.push_str(&losses_markdown(
        trades,
        'F',
        &format!(
            "Strategy F's losing trades (each costs about the price of its {:.0} shares)",
            f.shares
        ),
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_sim::{Flow, Quote};
    use wm_core::market::TempUnit;
    use wm_core::time::Season;
    use wm_strategy::{IncrementDistribution, PeakFeatures, PeakTimesBuilder};

    const TZ: Tz = chrono_tz::Europe::Amsterdam;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
    }

    /// A local time of the test day.
    fn at(h: u32, m: u32) -> DateTime<Utc> {
        TZ.with_ymd_and_hms(2026, 7, 15, h, m, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// Buckets 20 … 24 °C.
    fn buckets() -> (Vec<TemperatureBucket>, Vec<String>) {
        let b = (20..=24)
            .map(|t| TemperatureBucket::exact(t, TempUnit::Celsius))
            .collect();
        let labels = (20..=24).map(|t| format!("{t}°C")).collect();
        (b, labels)
    }

    /// Summer peak times: one day per entry, its high first reported at
    /// that local minute (half-hourly reports at :25 and :55).
    fn peaks(minutes: &[u16]) -> PeakTimes {
        let mut b = PeakTimesBuilder::new();
        for &peak in minutes {
            let points: Vec<wm_strategy::ObsPoint> = (0..48u16)
                .map(|i| {
                    let minute = 25 + 30 * i;
                    wm_strategy::ObsPoint {
                        observed_at: at(0, 25) + Duration::minutes(30 * i64::from(i)),
                        local_minute_of_day: minute,
                        local_minute_of_hour: (minute % 60) as u8,
                        temp: wm_core::units::TempC::from_whole(if minute == peak {
                            25
                        } else {
                            15
                        }),
                        report_type: wm_core::weather::ReportType::Metar,
                        version: 1,
                    }
                })
                .collect();
            b.add_day(day(), Season::Summer, &points, 25);
        }
        b.build()
    }

    /// Median 15:25, 90th percentile 16:55: the slot is 15:25–16:56.
    fn summer() -> PeakTimes {
        peaks(&[925, 925, 925, 925, 925, 1015, 1015, 1015, 1015, 1015])
    }

    /// Real features of a report, then the fields F reads set.
    fn features(t: DateTime<Utc>, high: i32, drop_tenths: i32) -> PeakFeatures {
        let o = wm_core::weather::Observation {
            key: wm_core::weather::ObservationKey {
                station: wm_core::ids::StationId::new("EHAM").unwrap(),
                observed_at: t,
                report_type: wm_core::weather::ReportType::Metar,
            },
            version: 1,
            temperature: Some(wm_core::units::TempC::from_whole(high)),
            dewpoint: None,
            precision: wm_core::weather::TempPrecision::WholeDegree,
            raw_text: String::new(),
            content_hash: "x".into(),
            provider: wm_core::ids::ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: t,
            parser_version: 1,
            quality: wm_core::weather::QualityFlags::default(),
        };
        let mut e = wm_strategy::TemperatureStateEngine::new(3);
        let station = o.key.station.clone();
        e.register_station(station.clone(), TZ);
        e.apply_observation(&o);
        let s = e
            .day_state(&station, day(), wm_strategy::ViewKind::All, t)
            .unwrap();
        let mut f = wm_strategy::PeakDetectionEngine::default()
            .assess(&s, TZ, t)
            .unwrap()
            .features;
        f.high_whole = high;
        f.drop_tenths = drop_tenths;
        f.local_minute_now = local_minute_of_day(t, TZ);
        f.season = Season::Summer;
        f
    }

    fn dist() -> IncrementDistribution {
        IncrementDistribution {
            probs: vec![0.95, 0.05],
            support: 100,
            source: "test".into(),
        }
    }

    /// A report at local `h:m`, known a minute later, with the high at
    /// `high` and every bucket's YES (ask, bid) proxies.
    fn decision(
        h: u32,
        m: u32,
        high: i32,
        drop_tenths: i32,
        (yes_ask, yes_bid): (Option<f64>, Option<f64>),
    ) -> Decision {
        let t = at(h, m);
        Decision {
            at: t,
            knowledge: t + Duration::minutes(1),
            f: features(t, high, drop_tenths),
            dists: [Some(dist()), Some(dist())],
            quotes: vec![
                Quote {
                    mid: None,
                    yes_ask,
                    yes_bid,
                };
                5
            ],
            flows: vec![Flow::default(); 5],
        }
    }

    fn tape(bucket: usize, t: DateTime<Utc>, yes_price: f64, taker_buys_yes: bool) -> MarketTrade {
        MarketTrade {
            at: t,
            bucket,
            yes_price,
            taker_buys_yes,
            shares: 100.0,
            taker: None,
        }
    }

    fn taker_only() -> MarketSimConfig {
        let mut sim = MarketSimConfig::default();
        sim.maker.enabled = false;
        sim
    }

    /// Replay F on the test day; `winner` is a bucket index (2 = 22 °C).
    fn run(
        decisions: &[Decision],
        tape_trades: &[MarketTrade],
        peak: &PeakTimes,
        sim: &MarketSimConfig,
        winner: usize,
    ) -> Vec<SimTrade> {
        let (b, labels) = buckets();
        let mut per_bucket: Vec<Vec<&MarketTrade>> = vec![Vec::new(); b.len()];
        for t in tape_trades {
            per_bucket[t.bucket].push(t);
        }
        simulate_peak_slot(
            day(),
            &b,
            &labels,
            winner,
            decisions,
            &per_bucket,
            peak,
            sim,
            0.05,
            &[25, 55],
            TZ,
        )
    }

    fn only<'a>(trades: &'a [SimTrade], label: &str) -> Vec<&'a SimTrade> {
        trades.iter().filter(|t| t.strategy == label).collect()
    }

    #[test]
    fn variants_that_equal_the_live_rule_are_not_replayed_twice() {
        let sim = PeakSlotSim::default();
        let labels: Vec<String> = sim.rules().into_iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            [
                "F",
                "F · to 0.99",
                "F · earlier slot",
                "F · later slot",
                "F · 1 °C below",
                "F maker"
            ]
        );
        assert!(sim.rules()[0].live && sim.rules().iter().skip(1).all(|r| !r.live));
        // Configured up to 0.99 and 1 °C below: those variants are the rule.
        let wide = PeakSlotSim {
            max_price: 0.99,
            min_drop_tenths: 10,
            ..PeakSlotSim::default()
        };
        let labels: Vec<String> = wide.rules().into_iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            ["F", "F · earlier slot", "F · later slot", "F maker"]
        );
        // Live on the later slot (as shipped since 1 Oct): the median slot,
        // the original rule, is replayed beside it.
        let later = PeakSlotSim {
            slot_from_quantile: 0.75,
            slot_to_quantile: 0.95,
            ..PeakSlotSim::default()
        };
        let labels: Vec<String> = later.rules().into_iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            [
                "F",
                "F · to 0.99",
                "F · earlier slot",
                "F · median slot",
                "F · 1 °C below",
                "F maker"
            ]
        );
        // Disabled live: no row is marked live.
        let off = PeakSlotSim {
            live: false,
            ..PeakSlotSim::default()
        };
        assert!(off.rules().iter().all(|r| !r.live));
    }

    #[test]
    fn the_ask_at_the_report_fills_inside_the_slot_and_the_range() {
        let peak = summer();
        assert_eq!(peak.slot(Season::Summer, 0.5, 0.9), Some((925, 1016)));
        let sim = taker_only();
        // 14:55 is known before the slot; 15:25 inside it at 0.93.
        let d = [
            decision(14, 55, 22, 0, (Some(0.93), None)),
            decision(15, 25, 22, 0, (Some(0.93), None)),
            decision(15, 55, 22, 0, (Some(0.93), None)),
        ];
        let trades = run(&d, &[], &peak, &sim, 2);
        let f = only(&trades, "F");
        assert_eq!(f.len(), 1, "one trade a day: {trades:?}");
        let t = f[0];
        assert_eq!((t.report.as_str(), t.filled.as_deref()), ("15:25", None));
        assert_eq!((t.bucket.as_str(), t.side.as_str()), ("22°C", "YES"));
        assert_eq!((t.structure.as_str(), t.range.as_str()), ("–", "0.90–0.95"));
        assert!(t.won);
        let fee = 0.05 * 0.93 * 0.07;
        assert!((t.pnl_usd - 100.0 * (1.0 - 0.93 - fee - 0.005)).abs() < 1e-9);
        // Lost when a later report beats the high: the 100 shares are gone.
        let lost = run(&d, &[], &peak, &sim, 3);
        let t = only(&lost, "F")[0];
        assert!(!t.won);
        assert!((t.pnl_usd + 100.0 * (0.93 + fee + 0.005)).abs() < 1e-9);
        assert_eq!(t.resolved, "23°C");
        // The earlier slot (25% → 75% = 15:25–16:56 here too) and the later
        // one (75% → 95% = 16:55–16:56) are replayed on the same reports.
        assert_eq!(only(&trades, "F · earlier slot").len(), 1);
        assert!(only(&trades, "F · later slot").is_empty());
    }

    #[test]
    fn the_price_rule_is_above_the_minimum_and_at_most_the_cap() {
        let peak = summer();
        let sim = taker_only();
        let fills = |ask: f64| {
            let d = [decision(15, 25, 22, 0, (Some(ask), None))];
            let trades = run(&d, &[], &peak, &sim, 2);
            (only(&trades, "F").len(), only(&trades, "F · to 0.99").len())
        };
        assert_eq!(fills(0.90), (0, 0), "0.90 is not above 0.90");
        assert_eq!(fills(0.905), (1, 1));
        assert_eq!(fills(0.95), (1, 1));
        assert_eq!(fills(0.96), (0, 1), "above the cap only for the variant");
        assert_eq!(fills(0.995), (0, 0));
    }

    #[test]
    fn between_reports_the_tape_fills_until_the_next_report_is_known() {
        let peak = summer();
        let sim = taker_only();
        let d = [
            decision(15, 25, 22, 0, (Some(0.85), None)),
            decision(15, 55, 22, 0, (Some(0.97), None)),
        ];
        // After the 15:25 report is known: a sale (not an ask), a buy below
        // the range, a buy of another bucket, then the fill.
        let tape_trades = [
            tape(2, at(15, 30), 0.93, false),
            tape(2, at(15, 32), 0.88, true),
            tape(1, at(15, 35), 0.93, true),
            tape(2, at(15, 41), 0.92, true),
            tape(2, at(15, 50), 0.94, true),
        ];
        let trades = run(&d, &tape_trades, &peak, &sim, 2);
        let t = only(&trades, "F")[0];
        assert_eq!(
            (t.report.as_str(), t.filled.as_deref(), t.price),
            ("15:25", Some("15:41"), 0.92)
        );
        // A trade once the next report is known follows that report.
        let late = [tape(2, at(15, 57), 0.93, true)];
        let trades = run(&d, &late, &peak, &sim, 2);
        let t = only(&trades, "F")[0];
        assert_eq!(
            (t.report.as_str(), t.filled.as_deref()),
            ("15:55", Some("15:57"))
        );
        // Nothing once the slot has ended (15:25–15:26 here).
        let narrow = peaks(&[925; 10]);
        assert_eq!(narrow.slot(Season::Summer, 0.5, 0.9), Some((925, 926)));
        let inside_tape = [tape(2, at(15, 41), 0.92, true)];
        assert!(only(&run(&d, &inside_tape, &narrow, &sim, 2), "F").is_empty());
    }

    #[test]
    fn a_stale_report_stops_the_tape_as_live_data_age_does() {
        let peak = summer();
        let sim = taker_only();
        assert_eq!(sim.f.max_data_age_minutes, 40);
        // The next report comes 90 minutes later: the 15:25 report is too
        // old to trade on from 16:06.
        let d = [
            decision(15, 25, 22, 0, (Some(0.85), None)),
            decision(16, 55, 22, 0, (None, None)),
        ];
        let within = [tape(2, at(16, 5), 0.93, true)];
        assert_eq!(only(&run(&d, &within, &peak, &sim, 2), "F").len(), 1);
        let stale = [tape(2, at(16, 7), 0.93, true)];
        assert!(only(&run(&d, &stale, &peak, &sim, 2), "F").is_empty());
    }

    #[test]
    fn seasons_without_history_use_the_fallback_slot() {
        let sim = taker_only();
        assert_eq!(sim.f.fallback_slots.summer, (900, 1080));
        let d = [
            decision(14, 55, 22, 0, (Some(0.93), None)),
            decision(15, 25, 22, 0, (Some(0.93), None)),
        ];
        let trades = run(&d, &[], &PeakTimes::default(), &sim, 2);
        assert_eq!(only(&trades, "F")[0].report, "15:25");
    }

    #[test]
    fn the_drop_variant_waits_for_the_temperature_to_fall() {
        let peak = summer();
        let sim = taker_only();
        let d = [
            decision(15, 25, 22, 5, (Some(0.93), None)),
            decision(15, 55, 22, 12, (Some(0.94), None)),
        ];
        let trades = run(&d, &[], &peak, &sim, 2);
        assert_eq!(only(&trades, "F")[0].report, "15:25");
        let drop = only(&trades, "F · 1 °C below");
        assert_eq!((drop[0].report.as_str(), drop[0].price), ("15:55", 0.94));
    }

    #[test]
    fn no_model_no_trade_and_no_bucket_no_trade() {
        let peak = summer();
        let mut d = decision(15, 25, 22, 0, (Some(0.93), Some(0.92)));
        d.dists = [None, None];
        assert!(run(&[d], &[], &peak, &MarketSimConfig::default(), 2).is_empty());
        // A high outside every listed bucket.
        let d = decision(15, 25, 30, 0, (Some(0.93), Some(0.92)));
        assert!(run(&[d], &[], &peak, &MarketSimConfig::default(), 2).is_empty());
    }

    #[test]
    fn the_maker_rests_a_bid_until_shortly_before_the_next_routine_report() {
        let peak = summer();
        let sim = MarketSimConfig::default();
        assert!(sim.maker.enabled);
        assert_eq!(sim.maker.cancel_before_report_min, 10);
        // Bid 0.92 below the ask 0.96 (above the taker cap).
        let d = [
            decision(15, 25, 22, 0, (Some(0.96), Some(0.92))),
            decision(15, 55, 22, 0, (Some(0.96), Some(0.92))),
        ];
        // A taker sells through the bid at 15:40: filled at the bid.
        let through = [tape(2, at(15, 40), 0.91, false)];
        let trades = run(&d, &through, &peak, &sim, 2);
        assert!(only(&trades, "F").is_empty(), "the ask is above the cap");
        let m = only(&trades, "F maker");
        assert_eq!(
            (m[0].report.as_str(), m[0].filled.as_deref(), m[0].price),
            ("15:25", Some("15:40"), 0.92)
        );
        let rebate = sim.maker.rebate_share * 0.05 * 0.92 * 0.08;
        assert!((m[0].pnl_usd - 100.0 * (1.0 - 0.92 + rebate)).abs() < 1e-9);
        // A sale at the bid does not go through it.
        let at_bid = [tape(2, at(15, 40), 0.92, false)];
        assert!(only(&run(&d, &at_bid, &peak, &sim, 2), "F maker").is_empty());
        // Cancelled at 15:45, ten minutes before the 15:55 report.
        let late = [tape(2, at(15, 50), 0.91, false)];
        assert!(only(&run(&d, &late, &peak, &sim, 2), "F maker").is_empty());
        // No bid at or above the ask, none outside the range.
        let crossed = [decision(15, 25, 22, 0, (Some(0.92), Some(0.92)))];
        assert!(only(&run(&crossed, &through, &peak, &sim, 2), "F maker").is_empty());
        let low = [decision(15, 25, 22, 0, (Some(0.96), Some(0.90)))];
        let below = [tape(2, at(15, 40), 0.85, false)];
        assert!(only(&run(&low, &below, &peak, &sim, 2), "F maker").is_empty());
        // Not replayed when makers are off.
        assert!(only(&run(&d, &through, &peak, &taker_only(), 2), "F maker").is_empty());
    }

    fn trade(date: NaiveDate, label: &str, pnl: f64) -> SimTrade {
        SimTrade {
            date,
            report: "15:25".into(),
            structure: STRUCTURE.into(),
            strategy: label.into(),
            window: 0,
            range: if label == "F · to 0.99" {
                "0.90–0.99".into()
            } else {
                "0.90–0.95".into()
            },
            bucket: "22°C".into(),
            side: "YES".into(),
            price: 0.93,
            p_model: 0.95,
            p_used: 0.95,
            won: pnl > 0.0,
            pnl_usd: pnl,
            resolved: if pnl > 0.0 { "22°C" } else { "23°C" }.into(),
            filled: None,
        }
    }

    #[test]
    fn the_rule_chosen_on_the_first_half_is_judged_on_the_second() {
        let sim = taker_only();
        let dates: Vec<NaiveDate> = (0..40).map(|i| day() + Duration::days(i)).collect();
        let mut trades = Vec::new();
        for (i, d) in dates.iter().enumerate() {
            // F wins every day; "to 0.99" wins more early and loses later.
            trades.push(trade(*d, "F", 5.0));
            trades.push(trade(*d, "F · to 0.99", if i < 20 { 7.0 } else { -3.0 }));
        }
        let line = out_of_sample(&trades, &sim, &dates, 200, 1).unwrap();
        assert!(
            line.contains("on the first 20 market days (2026-07-15 → 2026-08-03) the best F rule was *F · to 0.99* (20 trades, 20 won, $+140.00)"),
            "{line}"
        );
        assert!(
            line.contains("on the 20 later days (2026-08-04 → 2026-08-23) the same rule made 20 trades, 0 won, $-60.00"),
            "{line}"
        );
        // A tie keeps the configured rule.
        let tie: Vec<SimTrade> = dates
            .iter()
            .flat_map(|d| [trade(*d, "F", 5.0), trade(*d, "F · to 0.99", 5.0)])
            .collect();
        let line = out_of_sample(&tie, &sim, &dates, 200, 1).unwrap();
        assert!(line.contains("was *F* ("), "{line}");
        // Too few days, no day, no first-half trade.
        let line = out_of_sample(&trades, &sim, &dates[..19], 200, 1).unwrap();
        assert!(
            line.contains("19 replayed market days are too few"),
            "{line}"
        );
        assert!(out_of_sample(&trades, &sim, &[], 200, 1).is_none());
        let late_only: Vec<SimTrade> = trades
            .iter()
            .filter(|t| t.date >= dates[20])
            .cloned()
            .collect();
        let line = out_of_sample(&late_only, &sim, &dates, 200, 1).unwrap();
        assert!(
            line.contains("no F rule traded on the first 20 market days"),
            "{line}"
        );
    }

    #[test]
    fn rows_verdict_and_section_follow_the_rules() {
        let sim = taker_only();
        let trades = vec![
            trade(day(), "F", 6.0),
            trade(day() + Duration::days(1), "F", -95.0),
            trade(day(), "F · to 0.99", 4.0),
        ];
        let rows = peak_rows(&trades, &sim, 200, 1);
        assert_eq!(rows.len(), 6);
        assert!(rows[0].live && rows[0].strategy == "F");
        assert_eq!((rows[0].trades, rows[0].wins, rows[0].days), (2, 1, 2));
        assert!((rows[0].total_usd + 89.0).abs() < 1e-9);
        let v = verdict(&rows, &sim);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].starts_with("Strategy F (slot 50% → 90% quantile of the peak times, asks above 0.90 and ≤ 0.95; 100 shares a trade): 2 trades, 1 won, $-89.00"),
            "{}",
            v[0]
        );
        assert!(
            v[0].contains("F · to 0.99 1 trades, 1 won, $+4.00; F · earlier slot no trade"),
            "{}",
            v[0]
        );
        let md = markdown(
            &rows,
            &trades,
            &sim,
            &v,
            Some("Strategy F out of sample: …"),
        );
        assert!(md.contains("## Strategy F at traded prices"));
        assert!(
            md.contains("| F **live** | 50% → 90% | 0.90–0.95 | 2 | 1 | 2 |"),
            "{md}"
        );
        assert!(md.contains("* Strategy F out of sample: …"));
        assert!(
            md.contains(
                "Strategy F's losing trades (each costs about the price of its 100 shares)"
            )
        );
        assert!(
            md.contains("| 2026-07-16 | 15:25 | YES 22°C | 0.930 | 23°C | F |"),
            "{md}"
        );
    }

    /// A and B are replayed like live: with weight on the market they need a
    /// midpoint (both sides traded, close together) to check the model.
    /// (Here because this module has the day-replay helpers.)
    #[test]
    fn a_and_b_replays_need_the_market_to_check_the_model() {
        let (b, labels) = buckets();
        let sim = taker_only();
        let sure = || IncrementDistribution {
            probs: vec![0.995, 0.004, 0.001],
            support: 100,
            source: "test".into(),
        };
        // Taker trades of A (window 0, 0.90–0.99) and of B at a high of 22 °C.
        let count = |quote, weight, model: Option<IncrementDistribution>| {
            let mut d = decision(15, 25, 22, 0, quote);
            if let Some(m) = model {
                d.dists = [Some(m.clone()), Some(m)];
            }
            let trades = crate::market_sim::simulate_day(
                day(),
                &b,
                &labels,
                2,
                &[d],
                &sim,
                0.05,
                weight,
                50,
                TZ,
            );
            let n = |s: &str| {
                trades
                    .iter()
                    .filter(|t| {
                        t.structure == "current"
                            && t.strategy == s
                            && t.window == 0
                            && t.range == "0.90–0.99"
                    })
                    .count()
            };
            (n("A"), n("B"))
        };
        // A: model 0.95 pooled with a tight 0.88/0.90 book still clears the edge.
        assert_eq!(count((Some(0.90), Some(0.88)), 0.5, None), (1, 0));
        // Too wide or one-sided: the market cannot check, so no trade …
        assert_eq!(count((Some(0.90), Some(0.70)), 0.5, None), (0, 0));
        assert_eq!(count((Some(0.90), None), 0.5, None), (0, 0));
        // … unless the weight is 0 (model only by choice).
        assert_eq!(count((Some(0.90), Some(0.70)), 0.0, None), (1, 0));
        // B: NO on 23 and 24 °C at 0.95 (YES 0.05/0.07) — and nothing on a
        // wide YES book (0.05/0.30), unless model only.
        assert_eq!(count((Some(0.07), Some(0.05)), 0.5, Some(sure())), (0, 2));
        assert_eq!(count((Some(0.30), Some(0.05)), 0.5, Some(sure())), (0, 0));
        assert_eq!(count((Some(0.30), Some(0.05)), 0.0, Some(sure())), (0, 2));
    }

    /// The maker replay of A keeps the same market check.
    #[test]
    fn the_a_maker_replay_needs_the_market_to_check_the_model() {
        let (b, labels) = buckets();
        let sim = MarketSimConfig::default();
        let fills = |quote, weight| {
            let mut d = decision(15, 25, 22, 0, quote);
            d.f.minutes_since_high = 60;
            let sale = tape(2, at(15, 30), 0.91, false);
            let mut per_bucket: Vec<Vec<&MarketTrade>> = vec![Vec::new(); b.len()];
            per_bucket[2].push(&sale);
            crate::market_makers::simulate_makers(
                day(),
                &b,
                &labels,
                2,
                &[d],
                &per_bucket,
                &sim,
                0.05,
                weight,
                50,
                &[25, 55],
                TZ,
            )
            .iter()
            .filter(|t| t.structure == "current" && t.strategy == "A maker")
            .count()
        };
        // A YES bid at 0.92 under a tight 0.92/0.95 book, sold through at 0.91.
        assert_eq!(fills((Some(0.95), Some(0.92)), 0.5), 1);
        // The same bid with no offer above it: nothing checks the model.
        assert_eq!(fills((None, Some(0.92)), 0.5), 0);
        assert_eq!(fills((None, Some(0.92)), 0.0), 1);
    }
}
