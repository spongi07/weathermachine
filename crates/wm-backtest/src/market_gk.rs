//! Strategies G–K replayed at traded prices (`research market`).
//!
//! The decisions are those of the other replays — every report, at its
//! knowledge time, with the models as trained on the days before — but from
//! local midnight, since J quotes in the morning. Prices come from the tape:
//!
//! * **Takers** (H, I, K) pay the latest taker trade of the kind they need
//!   when it is at most `fresh_quote_minutes` old, else the first such trade
//!   after the decision while the report stands: YES at a taker's buy price,
//!   NO at one minus a taker's sell price of YES. Plus the slippage
//!   allowance and the taker fee. I needs a midpoint, so it trades only on a
//!   fresh two-sided quote.
//! * **Makers** (G, J and the maker variants) rest one tick better than the
//!   latest trade on their side, and fill only when a later trade goes
//!   through that price before the order expires (`cancel_before_report_min`
//!   before the next routine report). No fee; the rebate is earned.
//! * **K** needs KNMI's ten-minute readings for the day; a reading becomes
//!   known `knmi_delay_minutes` after its interval ends — an assumption
//!   that the live logs measure.
//!
//! Depth is not archived: every fill assumes the whole stake was available
//! at its price. Each rule takes at most one trade per day and bucket (J:
//! per bucket and side). Many rules are tried, so the family's best row
//! overstates what to expect; the out-of-sample line chooses on the first
//! half of the market days and judges on the second.

use crate::market_eval::MarketTrade;
use crate::market_makers::{Resting, cancel_time, next_routine, through_fill};
use crate::market_sim::{Decision, MarketSimConfig, SimTrade, StrategyRow, local_hm, row};
use crate::research::wilson;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use wm_core::market::TemperatureBucket;
use wm_core::time::local_day_bounds;
use wm_core::weather::TenMinuteObservation;
use wm_strategy::{
    KnmiNowcastConfig, MiddleFadeConfig, MorningMakerConfig, NextDegreeConfig, PeakTimes,
    TailSellerConfig,
};

/// KNMI's ten-minute readings by local date (oldest first within a day).
pub type KnmiHistory = BTreeMap<NaiveDate, Vec<TenMinuteObservation>>;

/// G–K carry no model structure in the table (G, H and I use the current
/// structure's probabilities; J and K none).
pub(crate) const STRUCTURE: &str = "–";

/// Fewest replayed market days for the out-of-sample split.
const MIN_SPLIT_DAYS: usize = 20;

/// G–K's replay settings: the live sections, and the replay's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GkSim {
    pub g: TailSellerConfig,
    pub h: NextDegreeConfig,
    pub i: MiddleFadeConfig,
    pub j: MorningMakerConfig,
    pub k: KnmiNowcastConfig,
    /// A taker's price comes from a trade at most this old at the decision;
    /// otherwise the next trade of the kind prices it.
    pub fresh_quote_minutes: i64,
    /// KNMI readings become known this long after their interval ends.
    pub knmi_delay_minutes: i64,
}

impl Default for GkSim {
    fn default() -> Self {
        Self {
            g: TailSellerConfig::default(),
            h: NextDegreeConfig::default(),
            i: MiddleFadeConfig::default(),
            j: MorningMakerConfig::default(),
            k: KnmiNowcastConfig::default(),
            fresh_quote_minutes: 10,
            knmi_delay_minutes: 5,
        }
    }
}

impl GkSim {
    /// The replay of the live sections.
    pub fn from_live(
        g: &TailSellerConfig,
        h: &NextDegreeConfig,
        i: &MiddleFadeConfig,
        j: &MorningMakerConfig,
        k: &KnmiNowcastConfig,
    ) -> Self {
        Self {
            g: g.clone(),
            h: h.clone(),
            i: i.clone(),
            j: j.clone(),
            k: k.clone(),
            ..Self::default()
        }
    }
}

/// Which sides J quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sides {
    Both,
    Yes,
    No,
}

/// One replayed rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GkRule {
    G {
        label: String,
        live: bool,
        cfg: TailSellerConfig,
    },
    H {
        label: String,
        live: bool,
        cfg: NextDegreeConfig,
        maker: bool,
    },
    I {
        label: String,
        live: bool,
        cfg: MiddleFadeConfig,
        maker: bool,
    },
    J {
        label: String,
        live: bool,
        cfg: MorningMakerConfig,
        sides: Sides,
    },
    K {
        label: String,
        live: bool,
        cfg: KnmiNowcastConfig,
        delay_minutes: i64,
        yes_above: bool,
    },
}

impl GkRule {
    pub(crate) fn label(&self) -> &str {
        match self {
            GkRule::G { label, .. }
            | GkRule::H { label, .. }
            | GkRule::I { label, .. }
            | GkRule::J { label, .. }
            | GkRule::K { label, .. } => label,
        }
    }

    pub(crate) fn live(&self) -> bool {
        match self {
            GkRule::G { live, .. }
            | GkRule::H { live, .. }
            | GkRule::I { live, .. }
            | GkRule::J { live, .. }
            | GkRule::K { live, .. } => *live,
        }
    }

    pub(crate) fn family(&self) -> char {
        self.label().chars().next().unwrap_or('?')
    }

    /// The price range the table shows.
    pub(crate) fn range(&self) -> String {
        match self {
            GkRule::G { cfg, .. } => format!(
                "YES {:.2}–{:.2}",
                cfg.min_yes_price.as_f64(),
                cfg.max_yes_price.as_f64()
            ),
            GkRule::H { cfg, .. } => format!(
                "YES {:.2}–{:.2}",
                cfg.min_price.as_f64(),
                cfg.max_price.as_f64()
            ),
            GkRule::I { cfg, .. } => format!("mid {:.2}–{:.2}", cfg.min_mid, cfg.max_mid),
            GkRule::J { cfg, .. } => format!("mid {:.2}–{:.2}", cfg.min_mid, cfg.max_mid),
            GkRule::K { cfg, .. } => format!(
                "NO {:.2}–{:.2}",
                cfg.min_price.as_f64(),
                cfg.max_price.as_f64()
            ),
        }
    }

    fn key(&self) -> (&'static str, &str, u32, String) {
        (STRUCTURE, self.label(), 0, self.range())
    }

    /// Stake per trade (USD at the price paid).
    fn stake(&self) -> f64 {
        match self {
            GkRule::G { cfg, .. } => cfg.notional.as_f64(),
            GkRule::H { cfg, .. } => cfg.notional.as_f64(),
            GkRule::I { cfg, .. } => cfg.notional.as_f64(),
            GkRule::J { cfg, .. } => cfg.notional.as_f64(),
            GkRule::K { cfg, .. } => cfg.notional.as_f64(),
        }
    }

    /// How the rule differs, for the report.
    pub(crate) fn describe(&self) -> String {
        let hm = |m: u16| format!("{:02}:{:02}", m / 60, m % 60);
        match self {
            GkRule::G { cfg, .. } => format!(
                "a resting NO bid on buckets ≥ {} °C above the high, the YES offered at {:.3}–{:.2}, {}–{} local{}",
                cfg.min_distance,
                cfg.min_yes_price.as_f64(),
                cfg.max_yes_price.as_f64(),
                hm(cfg.start_local_minute),
                hm(cfg.end_local_minute),
                if cfg.max_model_ratio >= 1e6 {
                    ", no model condition".to_owned()
                } else {
                    format!(", the model ≤ {:.2} × that price", cfg.max_model_ratio)
                }
            ),
            GkRule::H { cfg, maker, .. } => format!(
                "YES of the next degree at {:.2}–{:.2}, {} local until the {:.0}% peak time, within {:.1} °C of the high{}{}",
                cfg.min_price.as_f64(),
                cfg.max_price.as_f64(),
                hm(cfg.start_local_minute),
                100.0 * cfg.until_quantile,
                f64::from(cfg.max_drop_tenths) / 10.0,
                if cfg.min_model_ratio > 0.0 {
                    format!(", the model ≥ {:.2} × the ask", cfg.min_model_ratio)
                } else {
                    ", no model condition".to_owned()
                },
                if *maker {
                    ", as a resting bid one tick above the latest taker sell"
                } else {
                    ""
                }
            ),
            GkRule::I { cfg, maker, .. } => format!(
                "NO of buckets with a fresh YES midpoint {:.2}–{:.2} (spread ≤ {:.2}){}, {:.2} taken off the midpoint, {}–{} local{}",
                cfg.min_mid,
                cfg.max_mid,
                cfg.max_spread.as_f64(),
                if cfg.min_model_gap >= -0.5 {
                    format!(", the model ≥ {:.2} below it", cfg.min_model_gap)
                } else {
                    ", no model condition".to_owned()
                },
                cfg.calibration_bias,
                hm(cfg.start_local_minute),
                hm(cfg.end_local_minute),
                if *maker {
                    ", as a resting NO bid one tick inside"
                } else {
                    ""
                }
            ),
            GkRule::J { cfg, sides, .. } => format!(
                "{} one tick inside a fresh {:.2}–{:.2} spread, midpoint {:.2}–{:.2}, {}–{} local",
                match sides {
                    Sides::Both => "a YES bid and a NO bid",
                    Sides::Yes => "a YES bid",
                    Sides::No => "a NO bid",
                },
                cfg.min_spread.as_f64(),
                cfg.max_spread.as_f64(),
                cfg.min_mid,
                cfg.max_mid,
                hm(cfg.start_local_minute),
                hm(cfg.end_local_minute)
            ),
            GkRule::K {
                cfg,
                delay_minutes,
                yes_above,
                ..
            } => format!(
                "{} once KNMI's ten-minute mean is ≥ the high + 0.5 + {:.1} °C (known {delay_minutes}′ after the interval), ≤ {}′ before the report",
                if *yes_above {
                    "YES of the next degree"
                } else {
                    "NO of the high's bucket"
                },
                f64::from(cfg.mean_margin_tenths) / 10.0,
                cfg.max_lead_minutes
            ),
        }
    }
}

/// The configured rules first, then the variants that differ from them.
pub(crate) fn rules(sim: &GkSim) -> Vec<GkRule> {
    let mut v: Vec<GkRule> = Vec::new();
    let mut push = |r: GkRule| {
        let same = v.iter().any(|x| match (x, &r) {
            (GkRule::G { cfg: a, .. }, GkRule::G { cfg: b, .. }) => a == b,
            (
                GkRule::H {
                    cfg: a, maker: m, ..
                },
                GkRule::H {
                    cfg: b, maker: n, ..
                },
            ) => a == b && m == n,
            (
                GkRule::I {
                    cfg: a, maker: m, ..
                },
                GkRule::I {
                    cfg: b, maker: n, ..
                },
            ) => a == b && m == n,
            (
                GkRule::J {
                    cfg: a, sides: s, ..
                },
                GkRule::J {
                    cfg: b, sides: t, ..
                },
            ) => a == b && s == t,
            (
                GkRule::K {
                    cfg: a,
                    delay_minutes: d,
                    yes_above: y,
                    ..
                },
                GkRule::K {
                    cfg: b,
                    delay_minutes: e,
                    yes_above: z,
                    ..
                },
            ) => a == b && d == e && y == z,
            _ => false,
        });
        if !same {
            v.push(r);
        }
    };
    let g = &sim.g;
    for (label, live, cfg) in [
        ("G", g.enabled, g.clone()),
        (
            "G · ≤ 3¢",
            false,
            TailSellerConfig {
                max_yes_price: wm_core::units::Price::saturating_from_micros(30_000),
                ..g.clone()
            },
        ),
        // The other distance: 3 when the configured rule starts at 2 (or
        // closer), 2 when it starts at 3 or more.
        (
            if g.min_distance >= 3 {
                "G · ≥ 2 above"
            } else {
                "G · ≥ 3 above"
            },
            false,
            TailSellerConfig {
                min_distance: if g.min_distance >= 3 { 2 } else { 3 },
                ..g.clone()
            },
        ),
        (
            "G · no model",
            false,
            TailSellerConfig {
                max_model_ratio: 1e9,
                ..g.clone()
            },
        ),
        (
            "G · from 14:00",
            false,
            TailSellerConfig {
                start_local_minute: g.start_local_minute.max(14 * 60),
                ..g.clone()
            },
        ),
    ] {
        push(GkRule::G {
            label: label.into(),
            live,
            cfg,
        });
    }
    let h = &sim.h;
    for (label, live, cfg, maker) in [
        ("H", h.enabled, h.clone(), false),
        (
            "H · no model",
            false,
            NextDegreeConfig {
                min_model_ratio: 0.0,
                ..h.clone()
            },
            false,
        ),
        (
            "H · until 90%",
            false,
            NextDegreeConfig {
                until_quantile: 0.90,
                ..h.clone()
            },
            false,
        ),
        ("H maker", false, h.clone(), true),
    ] {
        push(GkRule::H {
            label: label.into(),
            live,
            cfg,
            maker,
        });
    }
    let i = &sim.i;
    for (label, live, cfg, maker) in [
        ("I", i.enabled, i.clone(), false),
        (
            "I · no model",
            false,
            MiddleFadeConfig {
                min_model_gap: -1.0,
                ..i.clone()
            },
            false,
        ),
        (
            "I · no bias",
            false,
            MiddleFadeConfig {
                calibration_bias: 0.0,
                ..i.clone()
            },
            false,
        ),
        ("I maker", false, i.clone(), true),
    ] {
        push(GkRule::I {
            label: label.into(),
            live,
            cfg,
            maker,
        });
    }
    let j = &sim.j;
    for (label, live, cfg, sides) in [
        ("J", j.enabled, j.clone(), Sides::Both),
        ("J · YES bids", false, j.clone(), Sides::Yes),
        ("J · NO bids", false, j.clone(), Sides::No),
        (
            "J · until 09:00",
            false,
            MorningMakerConfig {
                end_local_minute: j.end_local_minute.min(9 * 60),
                ..j.clone()
            },
            Sides::Both,
        ),
    ] {
        push(GkRule::J {
            label: label.into(),
            live,
            cfg,
            sides,
        });
    }
    let k = &sim.k;
    let d = sim.knmi_delay_minutes;
    for (label, live, cfg, delay_minutes, yes_above) in [
        ("K", k.enabled, k.clone(), d, false),
        (
            "K · margin 0.5 °C",
            false,
            KnmiNowcastConfig {
                mean_margin_tenths: 5,
                ..k.clone()
            },
            d,
            false,
        ),
        (
            "K · margin 0.1 °C",
            false,
            KnmiNowcastConfig {
                mean_margin_tenths: 1,
                ..k.clone()
            },
            d,
            false,
        ),
        ("K · YES above", false, k.clone(), d, true),
        ("K · known after 2′", false, k.clone(), 2, false),
        ("K · known after 8′", false, k.clone(), 8, false),
    ] {
        push(GkRule::K {
            label: label.into(),
            live,
            cfg,
            delay_minutes,
            yes_above,
        });
    }
    v
}

fn tick(p: f64) -> f64 {
    if !(0.04..=0.96).contains(&p) {
        0.001
    } else {
        0.01
    }
}

/// One market day's inputs.
pub(crate) struct GkDay<'a> {
    pub(crate) date: NaiveDate,
    pub(crate) buckets: &'a [TemperatureBucket],
    pub(crate) labels: &'a [String],
    pub(crate) winner: usize,
    /// Every report of the day, from local midnight.
    pub(crate) decisions: &'a [Decision],
    pub(crate) per_bucket: &'a [Vec<&'a MarketTrade>],
    /// Peak times of the days before.
    pub(crate) peak: &'a PeakTimes,
    pub(crate) knmi: Option<&'a [TenMinuteObservation]>,
    pub(crate) tz: Tz,
}

/// What every rule shares.
struct Costs<'a> {
    sim: &'a MarketSimConfig,
    fee_rate: f64,
    routine: &'a [u8],
}

impl Costs<'_> {
    fn fee(&self, p: f64) -> f64 {
        self.fee_rate * p * (1.0 - p)
    }

    fn taker(&self, p: f64, won: bool) -> f64 {
        f64::from(u8::from(won)) - p - self.fee(p) - self.sim.slippage
    }

    fn maker(&self, p: f64, won: bool) -> f64 {
        f64::from(u8::from(won)) - p + self.sim.maker.rebate_share * self.fee(p)
    }

    /// When an order resting from `at` expires (`None`: too close to the
    /// report to post).
    fn expiry(&self, at: DateTime<Utc>, min_rest_minutes: i64) -> Option<DateTime<Utc>> {
        let c = cancel_time(at, self.routine, self.sim.maker.cancel_before_report_min)?;
        (c - at >= Duration::minutes(min_rest_minutes)).then_some(c)
    }
}

/// The price a taker pays at `at`: the latest trade of the kind (a taker
/// buying YES for a YES purchase, selling YES for a NO purchase) when it is
/// at most `fresh` old and acceptable, else the first acceptable one in
/// `(at, until]`. With its time when later than `at`.
fn taker_price(
    trades: &[&MarketTrade],
    at: DateTime<Utc>,
    until: DateTime<Utc>,
    fresh: Duration,
    buy_yes: bool,
    ok: impl Fn(f64) -> bool,
) -> Option<(f64, Option<DateTime<Utc>>)> {
    let price = |t: &MarketTrade| {
        if buy_yes {
            t.yes_price
        } else {
            1.0 - t.yes_price
        }
    };
    let end = trades.partition_point(|t| t.at <= at);
    if let Some(t) = trades[..end]
        .iter()
        .rev()
        .find(|t| t.taker_buys_yes == buy_yes)
        && at - t.at <= fresh
        && ok(price(t))
    {
        return Some((price(t), None));
    }
    trades[end..]
        .iter()
        .take_while(|t| t.at <= until)
        .find(|t| t.taker_buys_yes == buy_yes && ok(price(t)))
        .map(|t| (price(t), Some(t.at)))
}

/// The latest taker buy and sell of YES, each at most `fresh` old.
fn fresh_quote(
    trades: &[&MarketTrade],
    at: DateTime<Utc>,
    fresh: Duration,
) -> (Option<f64>, Option<f64>) {
    let end = trades.partition_point(|t| t.at <= at);
    let (mut ask, mut bid) = (None, None);
    for t in trades[..end].iter().rev() {
        if at - t.at > fresh {
            break;
        }
        if t.taker_buys_yes {
            ask.get_or_insert(t.yes_price);
        } else {
            bid.get_or_insert(t.yes_price);
        }
        if ask.is_some() && bid.is_some() {
            break;
        }
    }
    (ask, bid)
}

#[allow(clippy::too_many_arguments)]
fn trade(
    day: &GkDay<'_>,
    rule: &GkRule,
    d_at: DateTime<Utc>,
    bucket: usize,
    yes: bool,
    price: f64,
    p_model: f64,
    per_share: f64,
    filled: Option<DateTime<Utc>>,
) -> SimTrade {
    let won = (bucket == day.winner) == yes;
    SimTrade {
        date: day.date,
        report: local_hm(d_at, day.tz),
        structure: STRUCTURE.to_owned(),
        strategy: rule.label().to_owned(),
        window: 0,
        range: rule.range(),
        bucket: day.labels[bucket].clone(),
        side: if yes { "YES" } else { "NO" }.to_owned(),
        price,
        p_model,
        p_used: p_model,
        won,
        pnl_usd: rule.stake() / price.max(1e-6) * per_share,
        resolved: day.labels[day.winner].clone(),
        filled: filled.map(|t| local_hm(t, day.tz)),
    }
}

/// Every G–K rule's trades on one market day.
pub(crate) fn simulate_gk(
    day: &GkDay<'_>,
    sim: &MarketSimConfig,
    fee_rate: f64,
    routine: &[u8],
) -> Vec<SimTrade> {
    let costs = Costs {
        sim,
        fee_rate,
        routine,
    };
    let mut out = Vec::new();
    for rule in rules(&sim.gk) {
        match &rule {
            GkRule::G { cfg, .. } => tail_seller(day, &rule, cfg, &costs, &mut out),
            GkRule::H { cfg, maker, .. } => {
                next_degree(day, &rule, cfg, *maker, &costs, &mut out);
            }
            GkRule::I { cfg, maker, .. } => {
                middle_fade(day, &rule, cfg, *maker, &costs, &mut out);
            }
            GkRule::J { cfg, sides, .. } => {
                morning_maker(day, &rule, cfg, *sides, &costs, &mut out)
            }
            GkRule::K {
                cfg,
                delay_minutes,
                yes_above,
                ..
            } => knmi_nowcast(
                day,
                &rule,
                cfg,
                *delay_minutes,
                *yes_above,
                &costs,
                &mut out,
            ),
        }
    }
    out
}

fn in_window(minute: u16, start: u16, end: u16) -> bool {
    (start..end).contains(&minute)
}

fn trades_of<'a>(day: &'a GkDay<'a>, i: usize) -> &'a [&'a MarketTrade] {
    day.per_bucket.get(i).map_or(&[][..], Vec::as_slice)
}

fn tail_seller(
    day: &GkDay<'_>,
    rule: &GkRule,
    cfg: &TailSellerConfig,
    costs: &Costs<'_>,
    out: &mut Vec<SimTrade>,
) {
    let mut done: HashSet<usize> = HashSet::new();
    for d in day.decisions {
        let Some(dist) = &d.dists[0] else { continue };
        if !in_window(
            d.f.local_minute_now,
            cfg.start_local_minute,
            cfg.end_local_minute,
        ) {
            continue;
        }
        let Some(cancel) = costs.expiry(d.knowledge, cfg.min_rest_minutes) else {
            continue;
        };
        let high = d.f.high_whole;
        for (i, b) in day.buckets.iter().enumerate() {
            if done.contains(&i) || b.lower.is_none_or(|lo| lo < high + cfg.min_distance) {
                continue;
            }
            // The YES offered: one tick under the latest taker buy, never
            // below the minimum (then it joins that price).
            let Some(ask) = d.quotes[i].yes_ask else {
                continue;
            };
            let offer = (ask - tick(ask)).max(cfg.min_yes_price.as_f64()).min(ask);
            if offer < cfg.min_yes_price.as_f64() - 1e-9
                || offer > cfg.max_yes_price.as_f64() + 1e-9
            {
                continue;
            }
            let p_bucket = dist.p_in_bucket_upper(high, b);
            if p_bucket > cfg.max_model_ratio * offer + 1e-12 {
                continue;
            }
            let Some(at) = through_fill(
                trades_of(day, i),
                d.knowledge,
                cancel,
                Resting::YesAsk(offer),
            ) else {
                continue;
            };
            done.insert(i);
            let no_price = 1.0 - offer;
            let won = i != day.winner;
            out.push(trade(
                day,
                rule,
                d.at,
                i,
                false,
                no_price,
                1.0 - p_bucket,
                costs.maker(no_price, won),
                Some(at),
            ));
        }
    }
}

fn next_degree(
    day: &GkDay<'_>,
    rule: &GkRule,
    cfg: &NextDegreeConfig,
    maker: bool,
    costs: &Costs<'_>,
    out: &mut Vec<SimTrade>,
) {
    let (_, day_end) = local_day_bounds(day.date, day.tz);
    let fresh = Duration::minutes(costs.sim.gk.fresh_quote_minutes);
    let mut done: HashSet<usize> = HashSet::new();
    for (k, d) in day.decisions.iter().enumerate() {
        let Some(dist) = &d.dists[0] else { continue };
        let high = d.f.high_whole;
        let end = day
            .peak
            .season(d.f.season)
            .and_then(|s| s.quantile(cfg.until_quantile))
            .unwrap_or(cfg.fallback_end_local_minute);
        if !in_window(d.f.local_minute_now, cfg.start_local_minute, end)
            || d.f.drop_tenths > cfg.max_drop_tenths
        {
            continue;
        }
        let Some(i) = day.buckets.iter().position(|b| b.contains(high + 1)) else {
            continue;
        };
        if day.buckets[i].contains(high) || done.contains(&i) {
            continue;
        }
        let p_model = dist.p_in_bucket_lower(high, &day.buckets[i]);
        let (lo, hi) = (cfg.min_price.as_f64(), cfg.max_price.as_f64());
        let ok = |p: f64| p >= lo - 1e-9 && p <= hi + 1e-9 && p_model >= cfg.min_model_ratio * p;
        let until = day
            .decisions
            .get(k + 1)
            .map_or(day_end, |n| n.knowledge)
            .min(day_end);
        let fill = if maker {
            let Some(bid) = d.quotes[i].yes_bid else {
                continue;
            };
            let price = bid + tick(bid);
            if !ok(price) || d.quotes[i].yes_ask.is_some_and(|a| price >= a - 1e-12) {
                continue;
            }
            let Some(cancel) = costs.expiry(d.knowledge, 3) else {
                continue;
            };
            through_fill(
                trades_of(day, i),
                d.knowledge,
                cancel.min(until),
                Resting::YesBid(price),
            )
            .map(|at| (price, Some(at)))
        } else {
            taker_price(trades_of(day, i), d.knowledge, until, fresh, true, ok)
        };
        let Some((price, filled)) = fill else {
            continue;
        };
        done.insert(i);
        let won = i == day.winner;
        let per_share = if maker {
            costs.maker(price, won)
        } else {
            costs.taker(price, won)
        };
        out.push(trade(
            day, rule, d.at, i, true, price, p_model, per_share, filled,
        ));
    }
}

fn middle_fade(
    day: &GkDay<'_>,
    rule: &GkRule,
    cfg: &MiddleFadeConfig,
    maker: bool,
    costs: &Costs<'_>,
    out: &mut Vec<SimTrade>,
) {
    let fresh = Duration::minutes(costs.sim.gk.fresh_quote_minutes);
    let mut done: HashSet<usize> = HashSet::new();
    for d in day.decisions {
        let Some(dist) = &d.dists[0] else { continue };
        if !in_window(
            d.f.local_minute_now,
            cfg.start_local_minute,
            cfg.end_local_minute,
        ) {
            continue;
        }
        let high = d.f.high_whole;
        for (i, b) in day.buckets.iter().enumerate() {
            if done.contains(&i) || b.upper.is_some_and(|u| u < high) {
                continue;
            }
            let (Some(ask), Some(bid)) = fresh_quote(trades_of(day, i), d.knowledge, fresh) else {
                continue;
            };
            if ask < bid || ask - bid > cfg.max_spread.as_f64() + 1e-9 {
                continue;
            }
            let mid = (ask + bid) / 2.0;
            if !(cfg.min_mid..=cfg.max_mid).contains(&mid) {
                continue;
            }
            let p_bucket = dist.p_in_bucket_upper(high, b);
            if p_bucket > mid - cfg.min_model_gap {
                continue;
            }
            let p_no = (1.0 - (mid - cfg.calibration_bias)).min(1.0 - p_bucket);
            let won = i != day.winner;
            if maker {
                // A NO bid one tick inside: the YES offered a tick under the ask.
                let offer = ask - tick(ask);
                if offer <= bid + 1e-12 {
                    continue;
                }
                let Some(cancel) = costs.expiry(d.knowledge, 3) else {
                    continue;
                };
                let Some(at) = through_fill(
                    trades_of(day, i),
                    d.knowledge,
                    cancel,
                    Resting::YesAsk(offer),
                ) else {
                    continue;
                };
                done.insert(i);
                let price = 1.0 - offer;
                out.push(trade(
                    day,
                    rule,
                    d.at,
                    i,
                    false,
                    price,
                    p_no,
                    costs.maker(price, won),
                    Some(at),
                ));
            } else {
                let price = 1.0 - bid;
                let ev = p_no - price - costs.fee(price) - costs.sim.slippage;
                if ev < cfg.min_edge {
                    continue;
                }
                done.insert(i);
                out.push(trade(
                    day,
                    rule,
                    d.at,
                    i,
                    false,
                    price,
                    p_no,
                    costs.taker(price, won),
                    None,
                ));
            }
        }
    }
}

fn morning_maker(
    day: &GkDay<'_>,
    rule: &GkRule,
    cfg: &MorningMakerConfig,
    sides: Sides,
    costs: &Costs<'_>,
    out: &mut Vec<SimTrade>,
) {
    let fresh = Duration::minutes(costs.sim.gk.fresh_quote_minutes);
    let mut done: HashSet<(usize, bool)> = HashSet::new();
    for d in day.decisions {
        if !in_window(
            d.f.local_minute_now,
            cfg.start_local_minute,
            cfg.end_local_minute,
        ) {
            continue;
        }
        let Some(cancel) = costs.expiry(d.knowledge, cfg.min_rest_minutes) else {
            continue;
        };
        for i in 0..day.buckets.len() {
            let (Some(ask), Some(bid)) = fresh_quote(trades_of(day, i), d.knowledge, fresh) else {
                continue;
            };
            let spread = ask - bid;
            let mid = (ask + bid) / 2.0;
            if !(cfg.min_mid..=cfg.max_mid).contains(&mid)
                || spread < cfg.min_spread.as_f64() - 1e-9
                || spread > cfg.max_spread.as_f64() + 1e-9
            {
                continue;
            }
            for yes in [true, false] {
                let quoted = match sides {
                    Sides::Both => true,
                    Sides::Yes => yes,
                    Sides::No => !yes,
                };
                if !quoted || done.contains(&(i, yes)) {
                    continue;
                }
                // A YES bid a tick above the bid; a NO bid a tick above the
                // NO bid, i.e. YES offered a tick under the ask.
                let (order, price) = if yes {
                    let p = bid + tick(bid);
                    (Resting::YesBid(p), p)
                } else {
                    let offer = ask - tick(ask);
                    (Resting::YesAsk(offer), 1.0 - offer)
                };
                let Some(at) = through_fill(trades_of(day, i), d.knowledge, cancel, order) else {
                    continue;
                };
                done.insert((i, yes));
                let won = (i == day.winner) == yes;
                out.push(trade(
                    day,
                    rule,
                    d.at,
                    i,
                    yes,
                    price,
                    if yes { mid } else { 1.0 - mid },
                    costs.maker(price, won),
                    Some(at),
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn knmi_nowcast(
    day: &GkDay<'_>,
    rule: &GkRule,
    cfg: &KnmiNowcastConfig,
    delay_minutes: i64,
    yes_above: bool,
    costs: &Costs<'_>,
    out: &mut Vec<SimTrade>,
) {
    let Some(readings) = day.knmi else { return };
    let (_, day_end) = local_day_bounds(day.date, day.tz);
    // K's edge is minutes: a price from before its reading was known says
    // little about the price after, so it must be at most a minute old.
    let fresh = Duration::minutes(costs.sim.gk.fresh_quote_minutes.min(1));
    let mut done: HashSet<usize> = HashSet::new();
    for r in readings {
        let known = r.interval_end + Duration::minutes(delay_minutes);
        let idx = day.decisions.partition_point(|d| d.knowledge <= known);
        let Some(d) = idx.checked_sub(1).and_then(|k| day.decisions.get(k)) else {
            continue;
        };
        if r.interval_end <= d.at
            || known - r.interval_end > Duration::minutes(cfg.max_age_minutes)
            // The report the reading anticipates: the first routine one
            // after its interval, still unpublished while it is newer than
            // the last METAR (as live).
            || next_routine(r.interval_end, costs.routine)
                .is_none_or(|n| n - r.interval_end > Duration::minutes(cfg.max_lead_minutes))
        {
            continue;
        }
        let high = d.f.high_whole;
        let edge = high * 10 + 5;
        if r.mean
            .is_none_or(|m| m.tenths() < edge + cfg.mean_margin_tenths)
            || (cfg.require_max_at_edge && r.max.is_none_or(|m| m.tenths() < edge))
        {
            continue;
        }
        let Some(h) = day.buckets.iter().position(|b| b.contains(high)) else {
            continue;
        };
        if day.buckets[h].contains(high + 1) {
            continue;
        }
        let until = day
            .decisions
            .get(idx)
            .map_or(day_end, |n| n.knowledge)
            .min(day_end);
        let (lo, hi) = (cfg.min_price.as_f64(), cfg.max_price.as_f64());
        let (i, yes) = if yes_above {
            match day.buckets.iter().position(|b| b.contains(high + 1)) {
                Some(j) => (j, true),
                None => continue,
            }
        } else {
            (h, false)
        };
        if done.contains(&i) {
            continue;
        }
        let ok = |p: f64| {
            p >= lo - 1e-9
                && p <= hi + 1e-9
                && (yes || cfg.p_new_high - p - costs.fee(p) - costs.sim.slippage >= cfg.min_edge)
        };
        let Some((price, filled)) = taker_price(trades_of(day, i), known, until, fresh, yes, ok)
        else {
            continue;
        };
        done.insert(i);
        let won = (i == day.winner) == yes;
        out.push(trade(
            day,
            rule,
            r.interval_end,
            i,
            yes,
            price,
            cfg.p_new_high,
            costs.taker(price, won),
            filled.or(Some(known)),
        ));
    }
}

/// How often the next METAR raised the high, by how far KNMI's last
/// ten-minute mean before it stood above the high's rounding edge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnmiAccuracyRow {
    pub label: String,
    pub reports: u64,
    pub new_highs: u64,
    pub share: f64,
    pub ci_low: f64,
    pub ci_high: f64,
}

/// Bins of (mean − (high + 0.5 °C)) in tenths, inclusive.
const ACCURACY_BINS: [(&str, i32, i32); 7] = [
    ("≤ −1.0 °C", i32::MIN, -10),
    ("−0.9 … −0.5 °C", -9, -5),
    ("−0.4 … −0.1 °C", -4, -1),
    ("0.0 … +0.2 °C", 0, 2),
    ("+0.3 … +0.5 °C", 3, 5),
    ("+0.6 … +0.9 °C", 6, 9),
    ("≥ +1.0 °C", 10, i32::MAX),
];

/// Counts of [`KnmiAccuracyRow`] over the days studied.
#[derive(Debug, Clone, Default)]
pub(crate) struct KnmiAccuracy {
    counts: [(u64, u64); ACCURACY_BINS.len()],
    pub(crate) days: u64,
}

impl KnmiAccuracy {
    /// Add one day's routine reports: for each (after the first), the last
    /// reading whose interval ended at least five minutes before it.
    pub(crate) fn add_day(&mut self, decisions: &[Decision], readings: &[TenMinuteObservation]) {
        self.days += 1;
        for w in decisions.windows(2) {
            let (prev, d) = (&w[0], &w[1]);
            let before = d.at - Duration::minutes(5);
            let Some(r) = readings
                .iter()
                .rev()
                .find(|r| r.interval_end <= before && r.interval_end > prev.at)
            else {
                continue;
            };
            let Some(mean) = r.mean else { continue };
            let gap = mean.tenths() - (prev.f.high_whole * 10 + 5);
            let new_high = d.f.high_whole > prev.f.high_whole;
            if let Some(k) = ACCURACY_BINS
                .iter()
                .position(|(_, lo, hi)| (*lo..=*hi).contains(&gap))
            {
                self.counts[k].0 += 1;
                self.counts[k].1 += u64::from(new_high);
            }
        }
    }

    pub(crate) fn rows(&self) -> Vec<KnmiAccuracyRow> {
        ACCURACY_BINS
            .iter()
            .zip(self.counts)
            .map(|((label, _, _), (n, k))| {
                let (lo, hi) = wilson(k, n);
                KnmiAccuracyRow {
                    label: (*label).to_owned(),
                    reports: n,
                    new_highs: k,
                    share: if n > 0 { k as f64 / n as f64 } else { 0.0 },
                    ci_low: lo,
                    ci_high: hi,
                }
            })
            .collect()
    }
}

/// Rows of every rule, the configured ones first within each family.
pub(crate) fn gk_rows(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    iterations: usize,
    seed: u64,
) -> Vec<StrategyRow> {
    rules(&sim.gk)
        .iter()
        .map(|r| row(trades, r.key(), r.live(), iterations, seed))
        .collect()
}

/// One line per family: its configured rule and how the variants did.
pub(crate) fn verdict(rows: &[StrategyRow], sim: &MarketSimConfig, knmi_days: u64) -> Vec<String> {
    let rules = rules(&sim.gk);
    let mut v = Vec::new();
    for family in ['G', 'H', 'I', 'J', 'K'] {
        let fam: Vec<&GkRule> = rules.iter().filter(|r| r.family() == family).collect();
        let find = |r: &GkRule| {
            rows.iter()
                .find(|x| x.strategy == r.label() && x.range == r.range())
        };
        let Some((first, live)) = fam.first().and_then(|r| find(r).map(|x| (*r, x))) else {
            continue;
        };
        if family == 'K' && knmi_days == 0 {
            v.push("Strategy K: not replayed — no KNMI ten-minute readings (set WM_KNMI_API_KEY for `research market` to download them).".into());
            continue;
        }
        let variants: Vec<String> = fam
            .iter()
            .skip(1)
            .filter_map(|r| {
                find(r).map(|x| {
                    if x.trades == 0 {
                        format!("{} no trade", r.label())
                    } else {
                        format!(
                            "{} {} trades, {} won, ${:+.2}",
                            r.label(),
                            x.trades,
                            x.wins,
                            x.total_usd
                        )
                    }
                })
            })
            .collect();
        v.push(format!(
            "Strategy {family}{} ({}; ${:.0} a trade): {} trades, {} won, ${:+.2} (${:+.2} per trade, 95% CI {:+.2} … {:+.2}){}{}.",
            if first.live() { "" } else { " (disabled live)" },
            first.describe(),
            first.stake(),
            live.trades,
            live.wins,
            live.total_usd,
            live.pnl_per_trade,
            live.ci_low,
            live.ci_high,
            if variants.is_empty() { "" } else { "; " },
            variants.join("; ")
        ));
    }
    v
}

/// Per family: the rule with the best P&L on the first half of the replayed
/// market days (the configured one on a tie), judged on the second half;
/// when that is not the configured rule, the configured rule's second half
/// too.
pub(crate) fn out_of_sample(
    trades: &[SimTrade],
    sim: &MarketSimConfig,
    replayed: &[NaiveDate],
    knmi_days: u64,
    iterations: usize,
    seed: u64,
) -> Vec<String> {
    let mut dates = replayed.to_vec();
    dates.sort_unstable();
    dates.dedup();
    if dates.is_empty() {
        return Vec::new();
    }
    if dates.len() < MIN_SPLIT_DAYS {
        return vec![format!(
            "Strategies G–K out of sample: {} replayed market days are too few to choose a rule on one half and test it on the other (at least {MIN_SPLIT_DAYS} needed).",
            dates.len()
        )];
    }
    let before = dates.len() / 2;
    let split = dates[before];
    let rules = rules(&sim.gk);
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
    let mut v = Vec::new();
    for family in ['G', 'H', 'I', 'J', 'K'] {
        if family == 'K' && knmi_days == 0 {
            continue;
        }
        let mut best: Option<(&GkRule, Vec<SimTrade>)> = None;
        for r in rules.iter().filter(|r| r.family() == family) {
            let first = half(r.label(), false);
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
            v.push(format!(
                "Strategy {family} out of sample: no {family} rule traded on the first {before} market days ({} → {}).",
                dates[0],
                dates[before - 1]
            ));
            continue;
        };
        let r = row(
            &half(best.label(), true),
            best.key(),
            false,
            iterations,
            seed,
        );
        // The configured rule (each family's first) on the same later days,
        // when another one was chosen: what deciding about it needs.
        let configured = match rules.iter().find(|r| r.family() == family) {
            Some(c) if c.label() != best.label() => {
                let later = half(c.label(), true);
                if later.is_empty() {
                    format!(
                        " The configured rule *{}* made no trade on those later days.",
                        c.label()
                    )
                } else {
                    let c = row(&later, c.key(), false, iterations, seed);
                    format!(
                        " The configured rule *{}* made {} trades there, {} won, ${:+.2} (${:+.2} per trade, 95% CI {:+.2} … {:+.2}).",
                        c.strategy,
                        c.trades,
                        c.wins,
                        c.total_usd,
                        c.pnl_per_trade,
                        c.ci_low,
                        c.ci_high
                    )
                }
            }
            _ => String::new(),
        };
        v.push(format!(
            "Strategy {family} out of sample: on the first {before} market days ({} → {}) the best {family} rule was *{}* ({} trades, {} won, ${:+.2}); on the {} later days ({} → {}) it made {} trades, {} won, ${:+.2} (${:+.2} per trade, 95% CI {:+.2} … {:+.2}).{configured}",
            dates[0],
            dates[before - 1],
            best.label(),
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
        ));
    }
    v
}

/// G–K's section of the market report.
pub(crate) fn markdown(
    rows: &[StrategyRow],
    sim: &MarketSimConfig,
    verdict: &[String],
    out_of_sample: &[String],
    knmi: &[KnmiAccuracyRow],
    knmi_days: u64,
) -> String {
    let rules = rules(&sim.gk);
    let mut s = format!(
        "\n## Strategies G–K at traded prices\n\nDecisions at every report from local midnight, at the knowledge time, with the current structure's prequential model. **Takers** (H, I, K) pay the latest taker trade of the kind they need when it is at most {}′ old, else the first one after the decision while the report stands, plus {:.3} slippage and the taker fee; I needs a fresh two-sided quote for its midpoint. **Makers** (G, J and the *maker* rows) rest one tick better than the latest trade on their side, fill only when a later trade goes through that price before the order expires {}′ before the next routine report, pay no fee and earn {:.0}% of the taker fee. **K** uses KNMI's ten-minute readings, known {}′ after each interval (an assumption; the live logs measure it), and a price at most 1′ old when it acts. Depth is not archived, so every fill assumes the whole stake was there. Stakes: G ${:.0}, H ${:.0}, I ${:.0}, J ${:.0} a quote, K ${:.0}. Rules: {}.\n\n",
        sim.gk.fresh_quote_minutes,
        sim.slippage,
        sim.maker.cancel_before_report_min,
        100.0 * sim.maker.rebate_share,
        sim.gk.knmi_delay_minutes,
        sim.gk.g.notional.as_f64(),
        sim.gk.h.notional.as_f64(),
        sim.gk.i.notional.as_f64(),
        sim.gk.j.notional.as_f64(),
        sim.gk.k.notional.as_f64(),
        rules
            .iter()
            .map(|r| format!("*{}* — {}", r.label(), r.describe()))
            .collect::<Vec<_>>()
            .join("; ")
    );
    for line in verdict.iter().chain(out_of_sample) {
        let _ = writeln!(s, "* {line}");
    }
    s.push_str("\n| rule | prices | trades | won | days | mean price | P&L per trade | 95% CI | total |\n|---|---|---:|---:|---:|---:|---:|---|---:|\n");
    for rule in &rules {
        if rule.family() == 'K' && knmi_days == 0 {
            continue;
        }
        let Some(r) = rows
            .iter()
            .find(|x| x.strategy == rule.label() && x.range == rule.range())
        else {
            continue;
        };
        let _ = writeln!(
            s,
            "| {}{} | {} | {} | {} | {} | {:.3} | {:+.2} | [{:+.2}, {:+.2}] | {:+.2} |",
            rule.label(),
            if r.live { " **live**" } else { "" },
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
    if knmi_days > 0 {
        let _ = write!(
            s,
            "\n### KNMI's ten-minute mean before the METAR ({knmi_days} days)\n\nFor every report after the first of a day, the last KNMI reading whose interval ended at least five minutes before it, by how far its mean stood above the rounding edge of the high so far (high + 0.5 °C): how often that report raised the high. This is strategy K's `p_new_high`.\n\n| mean − (high + 0.5 °C) | reports | new high | share | 95% CI |\n|---|---:|---:|---:|---|\n"
        );
        for r in knmi {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {:.3} | [{:.3}, {:.3}] |",
                r.label, r.reports, r.new_highs, r.share, r.ci_low, r.ci_high
            );
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_sim::{Flow, Quote};
    use wm_core::ids::{ProviderId, StationId};
    use wm_core::market::TempUnit;
    use wm_core::time::Season;
    use wm_core::units::TempC;
    use wm_strategy::{IncrementDistribution, PeakFeatures, PeakTimesBuilder};

    const TZ: Tz = chrono_tz::Europe::Amsterdam;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn buckets() -> Vec<TemperatureBucket> {
        (17..=24)
            .map(|v| TemperatureBucket::exact(v, TempUnit::Celsius))
            .collect()
    }

    fn labels() -> Vec<String> {
        (17..=24).map(|v| format!("{v}°C")).collect()
    }

    fn features(at: DateTime<Utc>, high: i32, drop: i32) -> PeakFeatures {
        let local = wm_core::time::local_minute_of_day(at, TZ);
        PeakFeatures {
            station: StationId::new("EHAM").unwrap(),
            date: date(),
            view: wm_strategy::ViewKind::All,
            high_tenths: high * 10,
            high_whole: high,
            high_at: at,
            current_tenths: high * 10 - drop,
            drop_tenths: drop,
            minutes_since_high: 0,
            minutes_since_first_high: 0,
            lower_obs_since_high: 0,
            retests: 0,
            slope_c_per_hour: None,
            accel_c_per_hour2: None,
            trajectory: wm_strategy::TrajectoryClass::AtHigh,
            local_minute_now: local,
            high_local_minute: local,
            minutes_after_solar_noon: 0,
            month: 7,
            season: Season::Summer,
            observation_count: 20,
            data_age_minutes: 3,
            forecast_rise_tenths: None,
            forecast_headroom_tenths: None,
            high_jump_tenths: None,
        }
    }

    fn decision(at: &str, high: i32, drop: i32, probs: &[f64], n: usize) -> Decision {
        let at = utc(at);
        Decision {
            at,
            knowledge: at + Duration::minutes(3),
            f: features(at, high, drop),
            dists: [
                Some(IncrementDistribution {
                    probs: probs.to_vec(),
                    support: 500,
                    source: "t".into(),
                }),
                None,
            ],
            quotes: vec![Quote::default(); n],
            flows: vec![Flow::default(); n],
        }
    }

    fn t(at: &str, bucket: usize, price: f64, buy: bool) -> MarketTrade {
        MarketTrade {
            at: utc(at),
            bucket,
            yes_price: price,
            taker_buys_yes: buy,
            shares: 50.0,
            taker: None,
        }
    }

    fn by_bucket(trades: &[MarketTrade], n: usize) -> Vec<Vec<&MarketTrade>> {
        let mut v: Vec<Vec<&MarketTrade>> = vec![Vec::new(); n];
        for t in trades {
            v[t.bucket].push(t);
        }
        v
    }

    fn sim() -> MarketSimConfig {
        MarketSimConfig::default()
    }

    fn run(
        decisions: &[Decision],
        trades: &[MarketTrade],
        winner: usize,
        knmi: Option<&[TenMinuteObservation]>,
    ) -> Vec<SimTrade> {
        let (b, l) = (buckets(), labels());
        let per = by_bucket(trades, b.len());
        let peak = PeakTimesBuilder::new().build();
        let day = GkDay {
            date: date(),
            buckets: &b,
            labels: &l,
            winner,
            decisions,
            per_bucket: &per,
            peak: &peak,
            knmi,
            tz: TZ,
        };
        simulate_gk(&day, &sim(), 0.05, &[25, 55])
    }

    fn of<'a>(ts: &'a [SimTrade], label: &str) -> Vec<&'a SimTrade> {
        ts.iter().filter(|t| t.strategy == label).collect()
    }

    #[test]
    fn g_sells_a_far_tail_when_a_later_buyer_pays_through_its_offer() {
        // 13:55Z report (15:55 local): high 19 °C. Bucket 21 °C (index 4)
        // is two above; its YES last bought at 0.04.
        let mut d = decision(
            "2026-07-15T13:55:00Z",
            19,
            10,
            &[0.97, 0.02, 0.008, 0.002],
            8,
        );
        d.quotes[4] = Quote {
            mid: Some(0.035),
            yes_ask: Some(0.04),
            yes_bid: Some(0.03),
        };
        // A taker buys at 0.04 after the decision (13:58): through G's 0.03.
        let trades = [t("2026-07-15T14:05:00Z", 4, 0.04, true)];
        let out = run(&[d], &trades, 2, None);
        let g = of(&out, "G");
        assert_eq!(g.len(), 1, "{out:?}");
        assert_eq!((g[0].side.as_str(), g[0].bucket.as_str()), ("NO", "21°C"));
        assert!((g[0].price - 0.97).abs() < 1e-9, "{}", g[0].price);
        assert!(g[0].won);
        assert_eq!(g[0].filled.as_deref(), Some("16:05"));
        // $30 of NO at 0.97: 30.93 shares earning 0.03 each plus no rebate
        // worth mentioning.
        assert!(
            (g[0].pnl_usd - 30.0 / 0.97 * (1.0 - 0.97 + 0.25 * 0.05 * 0.97 * 0.03)).abs() < 1e-9
        );
        // Too late to rest before the 14:25 report? 14:15 is the cutoff and
        // 13:58 + 3′ is fine; at 14:13 knowledge it is not.
        let late = decision(
            "2026-07-15T14:10:00Z",
            19,
            10,
            &[0.97, 0.02, 0.008, 0.002],
            8,
        );
        assert!(of(&run(&[late], &trades, 2, None), "G").is_empty());
    }

    #[test]
    fn h_buys_the_next_degree_from_the_tape_and_i_needs_a_fresh_midpoint() {
        // 11:25Z (13:25 local): high 19, 0.2 °C below; the next degree (20,
        // index 3) trades at 0.15; the model says 0.30.
        let d = decision("2026-07-15T11:25:00Z", 19, 2, &[0.60, 0.30, 0.08, 0.02], 8);
        let trades = [
            t("2026-07-15T11:20:00Z", 3, 0.15, true),
            t("2026-07-15T11:21:00Z", 3, 0.13, false),
        ];
        let out = run(std::slice::from_ref(&d), &trades, 3, None);
        let h = of(&out, "H");
        assert_eq!(h.len(), 1, "{out:?}");
        assert!((h[0].price - 0.15).abs() < 1e-9 && h[0].won && h[0].filled.is_none());
        // $10 at 0.15: 66.7 shares × (1 − 0.15 − fee − 0.005).
        let fee = 0.05 * 0.15 * 0.85;
        assert!((h[0].pnl_usd - 10.0 / 0.15 * (0.85 - fee - 0.005)).abs() < 1e-9);
        // I: the 19 °C bucket (index 2) at a fresh 0.44/0.46: NO at 0.56.
        let trades_i = [
            t("2026-07-15T11:24:00Z", 2, 0.46, true),
            t("2026-07-15T11:26:00Z", 2, 0.44, false),
        ];
        let d_i = decision("2026-07-15T11:25:00Z", 19, 2, &[0.20, 0.40, 0.30, 0.10], 8);
        let out = run(std::slice::from_ref(&d_i), &trades_i, 4, None);
        let i = of(&out, "I");
        assert_eq!(i.len(), 1, "{out:?}");
        assert_eq!(i[0].side, "NO");
        assert!((i[0].price - 0.56).abs() < 1e-9 && i[0].won);
        // A stale quote (20′ old) is no midpoint: no trade.
        let stale = [
            t("2026-07-15T11:04:00Z", 2, 0.46, true),
            t("2026-07-15T11:05:00Z", 2, 0.44, false),
        ];
        assert!(of(&run(&[d_i], &stale, 4, None), "I").is_empty());
    }

    #[test]
    fn j_quotes_the_morning_and_earns_the_spread_when_both_sides_fill() {
        // 06:25Z (08:25 local), 19 °C bucket (index 2) at 0.40/0.44.
        let d = decision("2026-07-15T06:25:00Z", 14, 0, &[0.5, 0.3, 0.15, 0.05], 8);
        let trades = [
            t("2026-07-15T06:20:00Z", 2, 0.44, true),
            t("2026-07-15T06:22:00Z", 2, 0.40, false),
            // After the decision: a seller at 0.40 hits the 0.41 bid, a buyer
            // at 0.44 lifts the 0.43 offer.
            t("2026-07-15T06:35:00Z", 2, 0.40, false),
            t("2026-07-15T06:36:00Z", 2, 0.44, true),
        ];
        let out = run(&[d], &trades, 5, None);
        let j = of(&out, "J");
        assert_eq!(j.len(), 2, "{out:?}");
        let yes = j.iter().find(|x| x.side == "YES").unwrap();
        let no = j.iter().find(|x| x.side == "NO").unwrap();
        assert!((yes.price - 0.41).abs() < 1e-9 && (no.price - 0.57).abs() < 1e-9);
        // Equal stakes, so the pair is not a perfect hedge; both lost and won
        // sides are counted.
        assert!(!yes.won && no.won);
        assert_eq!(of(&out, "J · YES bids").len(), 1);
        assert_eq!(of(&out, "J · NO bids").len(), 1);
    }

    fn reading(end: &str, mean: i32, max: i32) -> TenMinuteObservation {
        TenMinuteObservation {
            station: StationId::new("EHAM").unwrap(),
            provider: ProviderId::knmi(),
            interval_end: utc(end),
            mean: Some(TempC::from_tenths(mean)),
            max: Some(TempC::from_tenths(max)),
            received_at: utc(end) + Duration::minutes(5),
        }
    }

    #[test]
    fn k_buys_the_no_of_the_high_before_the_metar_and_the_table_counts_it() {
        // 11:25Z: high 19. KNMI 11:40 mean 19.9 (known 11:45); the 11:55
        // METAR reports 20.
        let d1 = decision("2026-07-15T11:25:00Z", 19, 0, &[0.6, 0.3, 0.08, 0.02], 8);
        let d2 = decision("2026-07-15T11:55:00Z", 20, 0, &[0.6, 0.3, 0.08, 0.02], 8);
        let readings = [
            reading("2026-07-15T11:30:00Z", 192, 194),
            reading("2026-07-15T11:40:00Z", 199, 201),
        ];
        // A taker sells 19 °C YES at 0.55 at 11:46: NO at 0.45.
        let trades = [t("2026-07-15T11:46:00Z", 2, 0.55, false)];
        let out = run(&[d1.clone(), d2.clone()], &trades, 3, Some(&readings));
        let k = of(&out, "K");
        assert_eq!(k.len(), 1, "{out:?}");
        assert_eq!((k[0].side.as_str(), k[0].bucket.as_str()), ("NO", "19°C"));
        assert!((k[0].price - 0.45).abs() < 1e-9 && k[0].won);
        // Known after 8′ (11:48), the 11:46 trade is gone and nothing later
        // is in range before the 11:58 METAR is known.
        assert!(of(&out, "K · known after 8′").is_empty());
        // The 11:30 reading (mean 19.2) is below the 19.8 trigger.
        let mut acc = KnmiAccuracy::default();
        acc.add_day(&[d1, d2], &readings);
        let rows = acc.rows();
        let plus = rows.iter().find(|r| r.label == "+0.3 … +0.5 °C").unwrap();
        assert_eq!((plus.reports, plus.new_highs), (1, 1), "{rows:?}");
        // Without readings K is not replayed.
        assert!(of(&run(&[], &trades, 3, None), "K").is_empty());
    }

    #[test]
    fn k_uses_the_reading_known_as_its_report_is_taken() {
        // The 11:50 reading (mean 19.9) is known at 11:55, as the 11:55 METAR
        // is taken; that METAR is known at 11:58. Five minutes ahead of it,
        // not 35 ahead of the 12:25 one.
        let d1 = decision("2026-07-15T11:25:00Z", 19, 0, &[0.6, 0.3, 0.08, 0.02], 8);
        let d2 = decision("2026-07-15T11:55:00Z", 20, 0, &[0.6, 0.3, 0.08, 0.02], 8);
        let readings = [reading("2026-07-15T11:50:00Z", 199, 201)];
        let trades = [t("2026-07-15T11:56:00Z", 2, 0.55, false)];
        let out = run(&[d1, d2], &trades, 3, Some(&readings));
        let k = of(&out, "K");
        assert_eq!(k.len(), 1, "{out:?}");
        assert!((k[0].price - 0.45).abs() < 1e-9 && k[0].won);
    }

    #[test]
    fn the_configured_rules_come_first_and_duplicates_are_dropped() {
        let r = rules(&GkSim::default());
        let labels: Vec<&str> = r.iter().map(GkRule::label).collect();
        assert_eq!(labels[0], "G");
        assert!(labels.contains(&"H maker") && labels.contains(&"K · known after 2′"));
        let live: Vec<&str> = r.iter().filter(|x| x.live()).map(GkRule::label).collect();
        assert_eq!(live, ["G", "H", "I", "J", "K"]);
        // A live G already limited to 3¢ has no separate "≤ 3¢" row.
        let mut s = GkSim::default();
        s.g.max_yes_price = wm_core::units::Price::saturating_from_micros(30_000);
        assert!(!rules(&s).iter().any(|x| x.label() == "G · ≤ 3¢"));
        // A G configured from three above compares with two above instead.
        let mut s = GkSim::default();
        s.g.min_distance = 3;
        let labels: Vec<String> = rules(&s)
            .iter()
            .filter(|x| x.family() == 'G')
            .map(|x| x.label().to_owned())
            .collect();
        assert!(labels.contains(&"G · ≥ 2 above".to_owned()), "{labels:?}");
        assert!(!labels.contains(&"G · ≥ 3 above".to_owned()), "{labels:?}");
    }

    /// A trade of the default rule `label`, as the replay records it.
    fn sim_trade(d: NaiveDate, label: &str, pnl: f64) -> SimTrade {
        let rule = rules(&GkSim::default())
            .into_iter()
            .find(|r| r.label() == label)
            .unwrap();
        SimTrade {
            date: d,
            report: "15:55".into(),
            structure: STRUCTURE.into(),
            strategy: label.into(),
            window: 0,
            range: rule.range(),
            bucket: "22°C".into(),
            side: "NO".into(),
            price: 0.96,
            p_model: 0.0,
            p_used: 0.0,
            won: pnl > 0.0,
            pnl_usd: pnl,
            resolved: "19°C".into(),
            filled: None,
        }
    }

    #[test]
    fn out_of_sample_also_judges_the_configured_rule() {
        let sim = MarketSimConfig::default();
        let dates: Vec<NaiveDate> = (0..40).map(|i| date() + Duration::days(i)).collect();
        let mut trades = Vec::new();
        for (i, d) in dates.iter().enumerate() {
            // G gains a little every day; "G · ≤ 3¢" gains more early and
            // loses later.
            trades.push(sim_trade(*d, "G", 1.0));
            trades.push(sim_trade(*d, "G · ≤ 3¢", if i < 20 { 2.0 } else { -3.0 }));
        }
        let lines = out_of_sample(&trades, &sim, &dates, 0, 200, 1);
        let g = lines
            .iter()
            .find(|l| l.starts_with("Strategy G out of sample"))
            .unwrap();
        assert!(
            g.contains("the best G rule was *G · ≤ 3¢* (20 trades, 20 won, $+40.00)"),
            "{g}"
        );
        assert!(g.contains("it made 20 trades, 0 won, $-60.00"), "{g}");
        assert!(
            g.contains(
                "The configured rule *G* made 20 trades there, 20 won, $+20.00 ($+1.00 per trade"
            ),
            "{g}"
        );
        // No KNMI readings: no K line.
        assert!(
            !lines.iter().any(|l| l.starts_with("Strategy K")),
            "{lines:?}"
        );
        // The configured rule chosen itself: one sentence only.
        let only_g: Vec<SimTrade> = trades
            .iter()
            .filter(|t| t.strategy == "G")
            .cloned()
            .collect();
        let lines = out_of_sample(&only_g, &sim, &dates, 0, 200, 1);
        let g = lines
            .iter()
            .find(|l| l.starts_with("Strategy G out of sample"))
            .unwrap();
        assert!(
            g.contains("the best G rule was *G* (") && !g.contains("The configured rule"),
            "{g}"
        );
        // A configured rule that did not trade later says so.
        let early_g: Vec<SimTrade> = trades
            .iter()
            .filter(|t| t.strategy != "G" || t.date < dates[20])
            .cloned()
            .collect();
        let lines = out_of_sample(&early_g, &sim, &dates, 0, 200, 1);
        let g = lines
            .iter()
            .find(|l| l.starts_with("Strategy G out of sample"))
            .unwrap();
        assert!(
            g.contains("The configured rule *G* made no trade on those later days."),
            "{g}"
        );
    }
}
