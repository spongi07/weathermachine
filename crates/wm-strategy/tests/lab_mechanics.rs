#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The strategy lab's L26–L65 on synthetic days: each family's main rule
//! proposes its order, its blockers say why not otherwise, and its variant
//! differs where it should. Each family holds its signal for a minute
//! before it trades and buys at up to a cent above the ask it saw: both are
//! checked here too.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Europe::Amsterdam;
use std::collections::{HashMap, HashSet};
use wm_core::event::{WalletScore, WalletScoresEvent};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, StationId, TokenId};
use wm_core::market::{
    BookLevel, DailyTemperatureMarket, OrderBook, OutcomeSide, Side, TakerTrade,
};
use wm_core::portfolio::{InstrumentRef, PositionBook};
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{Fill, IntentKind, Liquidity, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    Observation, ObservationKey, QualityFlags, ReportType, TempPrecision, TenMinuteExtras,
    TenMinuteObservation, UpstreamReport,
};
use wm_strategy::lab::{
    LabConfig, LabInputs, LabStrategy, LadderQuote, LadderSample, NeighbourReadings, Profile,
    UpstreamReadings, WalletScores, WxReport, code, profile,
};
use wm_strategy::{
    ForecastDay, PeakDetectionEngine, PeakSlotConfig, Proposal, Strategy, StrategyContext,
    StrategyOutput, TemperatureStateEngine, ViewEvaluation, ViewKind,
};

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// `HH:MM` on 1 July 2026, UTC (local time is two hours later).
fn z(hm: &str) -> DateTime<Utc> {
    utc(&format!("2026-07-01T{hm}:00Z"))
}

/// `HH:MM` on 30 June 2026, UTC.
fn y(hm: &str) -> DateTime<Utc> {
    utc(&format!("2026-06-30T{hm}:00Z"))
}

fn p(s: &str) -> Price {
    Price::parse(s).unwrap()
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
}

fn observation(t: DateTime<Utc>, tenths: i32, dew: Option<i32>, raw: &str) -> Observation {
    Observation {
        key: ObservationKey {
            station: eham(),
            observed_at: t,
            report_type: ReportType::Metar,
        },
        version: 1,
        temperature: Some(TempC::from_tenths(tenths)),
        dewpoint: dew.map(TempC::from_tenths),
        precision: TempPrecision::WholeDegree,
        raw_text: raw.to_owned(),
        content_hash: format!("{t}{raw}"),
        provider: ProviderId::awc(),
        provider_receipt_at: None,
        fetched_at: t + Duration::minutes(3),
        parser_version: 1,
        quality: QualityFlags::default(),
    }
}

struct World {
    m: DailyTemperatureMarket,
    books: HashMap<TokenId, OrderBook>,
    positions: PositionBook,
    pending: HashSet<TokenId>,
    loc: LocationId,
    now: DateTime<Utc>,
    metars: Vec<Observation>,
    knmi: Vec<TenMinuteObservation>,
    neighbours: Vec<(String, f64, Vec<TenMinuteObservation>)>,
    forecast: Option<ForecastDay>,
    takers: Vec<TakerTrade>,
    wallets: Option<WalletScores>,
    extras: Vec<TenMinuteExtras>,
    upstream: Vec<(String, f64, f64, Vec<WxReport>)>,
    ladder: Vec<LadderSample>,
    yesterday: Vec<WxReport>,
}

impl World {
    fn new(now: &str) -> Self {
        let now = z(now);
        Self {
            m: synthetic_temperature_market(
                &LocationId::new("amsterdam").unwrap(),
                &eham(),
                date(),
                Amsterdam,
                13,
                24,
                now - Duration::hours(12),
            ),
            books: HashMap::new(),
            positions: PositionBook::new(),
            pending: HashSet::new(),
            loc: LocationId::new("amsterdam").unwrap(),
            now,
            metars: Vec::new(),
            knmi: Vec::new(),
            neighbours: Vec::new(),
            forecast: None,
            takers: Vec::new(),
            wallets: None,
            extras: Vec::new(),
            upstream: Vec::new(),
            ladder: Vec::new(),
            yesterday: Vec::new(),
        }
    }

    /// A METAR at `hm` UTC, known three minutes later.
    fn metar(&mut self, hm: &str, tenths: i32, dew: Option<i32>, raw: &str) {
        self.metars.push(observation(z(hm), tenths, dew, raw));
    }

    fn temps(&mut self, series: &[(&str, i32)]) {
        for (hm, t) in series {
            self.metar(hm, *t, None, "");
        }
    }

    fn book(&mut self, token: TokenId, bid: Option<&str>, ask: Option<&str>) {
        let b = synthetic_book(&token, bid, ask, 200, self.now);
        self.books.insert(token, b);
    }

    /// A book with levels `(price, shares)`.
    fn levels(&mut self, token: TokenId, bids: &[(&str, i64)], asks: &[(&str, i64)]) {
        let lv = |l: &[(&str, i64)]| {
            l.iter()
                .map(|(pr, s)| BookLevel {
                    price: p(pr),
                    size: Shares::from_whole(*s),
                })
                .collect::<Vec<_>>()
        };
        let mut b = synthetic_book(&token, None, None, 1, self.now);
        b.bids = lv(bids);
        b.asks = lv(asks);
        self.books.insert(token, b);
    }

    fn yes_book(&mut self, v: i32, bid: &str, ask: &str) {
        self.book(self.yes(v), Some(bid), Some(ask));
    }

    fn no_book(&mut self, v: i32, bid: &str, ask: &str) {
        self.book(self.no(v), Some(bid), Some(ask));
    }

    fn yes(&self, v: i32) -> TokenId {
        self.m.outcome_for_value(v).unwrap().yes_token.clone()
    }

    fn no(&self, v: i32) -> TokenId {
        self.m.outcome_for_value(v).unwrap().no_token.clone()
    }

    fn reading(&mut self, hm: &str, mean: i32) {
        self.knmi.push(knmi(hm, mean));
        self.knmi.sort_by_key(|r| r.interval_end);
    }

    /// KNMI's other parameters for the interval ending at `hm` UTC.
    fn extra(&mut self, hm: &str, values: &[(&str, f64)]) {
        let t = z(hm);
        match self.extras.iter_mut().find(|e| e.interval_end == t) {
            Some(e) => {
                for (k, v) in values {
                    e.values.insert((*k).to_owned(), *v);
                }
            }
            None => self.extras.push(TenMinuteExtras {
                station: eham(),
                interval_end: t,
                values: values.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
                received_at: t + Duration::minutes(4),
            }),
        }
        self.extras.sort_by_key(|e| e.interval_end);
    }

    fn taker(&mut self, at: &str, token: TokenId, side: Side, price: f64, size: f64, who: &str) {
        self.takers.push(TakerTrade {
            token,
            side,
            price,
            size,
            at: utc(&format!("2026-07-01T{at}Z")),
            taker: Some(who.to_owned()),
            id: format!("{at}{who}{price}{size}"),
        });
        self.takers.sort_by_key(|t| t.at);
    }

    /// A ladder sample at `hm` UTC with YES quotes `(value, bid, ask, depth)`.
    fn sample(&mut self, hm: &str, quotes: &[(i32, f64, f64, f64)]) {
        self.sample_at(z(hm), quotes);
    }

    fn sample_at(&mut self, at: DateTime<Utc>, quotes: &[(i32, f64, f64, f64)]) {
        let quotes = quotes
            .iter()
            .map(|(v, bid, ask, depth)| LadderQuote {
                token: self.yes(*v),
                bid: Some((*bid, *depth)),
                ask: Some((*ask, *depth)),
                bid_depth: *depth,
                ask_depth: *depth,
            })
            .collect();
        self.ladder.push(LadderSample { at, quotes });
        self.ladder.sort_by_key(|s| s.at);
    }

    fn upstream(&mut self, icao: &str, bearing: f64, km: f64, reports: &[(&str, i32, &str)]) {
        let rs = reports
            .iter()
            .map(|(hm, t, raw)| {
                WxReport::from_upstream(&UpstreamReport {
                    station: StationId::new(icao).unwrap(),
                    observed_at: z(hm),
                    raw_text: (*raw).to_owned(),
                    temperature: Some(TempC::from_tenths(*t)),
                    dewpoint: None,
                    received_at: z(hm) + Duration::minutes(3),
                })
            })
            .collect();
        self.upstream.push((icao.to_owned(), bearing, km, rs));
    }

    /// Yesterday's METAR at `hm` UTC on 30 June.
    fn yesterday(&mut self, hm: &str, tenths: i32, raw: &str) {
        self.yesterday.push(WxReport::from_observation(&observation(
            y(hm),
            tenths,
            None,
            raw,
        )));
    }

    /// A day-1 forecast known from 06:00 UTC: `f(local hour)` in tenths.
    fn forecast(&mut self, f: impl Fn(i64) -> i32) {
        let start = y("22:00");
        let series: Vec<(DateTime<Utc>, i32)> = (0..=24)
            .map(|h| (start + Duration::hours(h), f(h)))
            .collect();
        self.forecast = ForecastDay::from_series(date(), Amsterdam, &series, z("06:00"));
        assert!(self.forecast.is_some());
    }

    fn views(&self) -> Vec<ViewEvaluation> {
        let mut e = TemperatureStateEngine::new(5);
        e.register_station(eham(), Amsterdam);
        for o in self.metars.iter().filter(|o| o.key.observed_at <= self.now) {
            e.apply_observation(o);
        }
        let Some(s) = e.day_state(&eham(), date(), ViewKind::All, self.now) else {
            return Vec::new();
        };
        PeakDetectionEngine::default()
            .assess(&s, Amsterdam, self.now)
            .map(|a| {
                vec![ViewEvaluation {
                    view: ViewKind::All,
                    assessment: a,
                    distribution: None,
                }]
            })
            .unwrap_or_default()
    }

    fn cfg(family: u8, variant: bool) -> LabConfig {
        LabConfig {
            variants: if variant {
                vec![code(family)]
            } else {
                Vec::new()
            },
            confirm_seconds: 0,
            // The synthetic days skip reports; data age is A–K's and L1–L25's
            // common check, tested there.
            max_data_age_minutes: 24 * 60,
            ..LabConfig::default()
        }
    }

    /// One evaluation, the signal confirmed at once.
    fn run(&self, family: u8, variant: bool) -> StrategyOutput {
        let mut s = LabStrategy::new(
            family,
            &Self::cfg(family, variant),
            &PeakSlotConfig::default(),
        );
        self.run_with(&mut s)
    }

    fn run_with(&self, s: &mut dyn Strategy) -> StrategyOutput {
        let views = self.views();
        let reports: Vec<WxReport> = self
            .metars
            .iter()
            .filter(|o| o.key.observed_at <= self.now)
            .map(WxReport::from_observation)
            .collect();
        let neighbours: Vec<NeighbourReadings<'_>> = self
            .neighbours
            .iter()
            .map(|(name, bearing, readings)| NeighbourReadings {
                name,
                bearing_deg: *bearing,
                readings,
            })
            .collect();
        let upstream: Vec<UpstreamReadings<'_>> = self
            .upstream
            .iter()
            .map(|(icao, bearing, km, reports)| UpstreamReadings {
                icao,
                bearing_deg: *bearing,
                distance_km: *km,
                reports,
            })
            .collect();
        let lab = LabInputs {
            knmi: &self.knmi,
            neighbours: &neighbours,
            reports: &reports,
            forecast: self.forecast.as_ref(),
            forecast_ready: None,
            yesterday_error_tenths: None,
            takers: &self.takers,
            wallets: self.wallets.as_ref(),
            position: Some((52.318, 4.790)),
            knmi_extra: &self.extras,
            upstream: &upstream,
            ladder: &self.ladder,
            yesterday_reports: &self.yesterday,
        };
        let ctx = StrategyContext {
            now: self.now,
            mode: RunMode::Paper,
            location: &self.loc,
            market: &self.m,
            books: &self.books,
            views: &views,
            positions: &self.positions,
            pending_tokens: &self.pending,
            peak_times: None,
            routine_minutes: &[25, 55],
            nowcast: self.knmi.last(),
            lab: &lab,
        };
        s.evaluate(&ctx)
    }
}

fn knmi(hm: &str, mean: i32) -> TenMinuteObservation {
    TenMinuteObservation {
        station: eham(),
        provider: ProviderId::knmi(),
        interval_end: z(hm),
        mean: Some(TempC::from_tenths(mean)),
        max: Some(TempC::from_tenths(mean + 2)),
        radiation: None,
        received_at: z(hm) + Duration::minutes(4),
    }
}

fn proposal(out: &StrategyOutput) -> &Proposal {
    assert_eq!(
        out.proposals.len(),
        1,
        "expected one proposal; evaluations: {:#?}",
        out.evaluations
            .iter()
            .map(|e| (e.bucket_label.clone(), e.outcome_side, e.blockers.clone()))
            .collect::<Vec<_>>()
    );
    &out.proposals[0]
}

fn blockers(out: &StrategyOutput) -> String {
    out.evaluations
        .iter()
        .flat_map(|e| e.blockers.iter().cloned())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn assert_blocked(out: &StrategyOutput, needle: &str) {
    assert!(
        out.proposals.is_empty(),
        "unexpected proposal: {:?}",
        out.proposals
    );
    let b = blockers(out);
    assert!(b.contains(needle), "'{needle}' not in: {b}");
}

/// A taker buy of `token` at `ask` with the cent of tolerance.
fn assert_buy(pr: &Proposal, token: &TokenId, ask: &str) {
    assert_eq!(&pr.token, token, "{pr:?}");
    assert_eq!(pr.side, Side::Buy);
    assert_eq!(pr.tif, TimeInForce::Fak);
    assert_eq!(pr.kind, IntentKind::Open);
    assert_eq!(
        pr.limit_price,
        p(ask).saturating_add(p("0.01")),
        "the limit lies a cent above the ask"
    );
}

/// The high reaches 20 °C at 09:55 UTC.
fn warm_morning(w: &mut World) {
    w.temps(&[
        ("06:55", 150),
        ("07:55", 170),
        ("08:55", 190),
        ("09:55", 200),
    ]);
}

/// The high reaches 18 °C at 08:25 UTC.
fn mild_morning(w: &mut World) {
    w.temps(&[("06:55", 160), ("08:25", 180)]);
}

/// The high reaches 21 °C at 11:55 UTC.
fn high_21(w: &mut World) {
    w.temps(&[
        ("09:55", 190),
        ("10:25", 200),
        ("10:55", 200),
        ("11:25", 200),
        ("11:55", 210),
    ]);
}

// ----------------------------------------------------------- general ---

#[test]
fn every_new_family_has_a_profile_and_a_stake() {
    for f in 26..=65u8 {
        assert!(profile(f).is_some(), "L{f}");
    }
    assert!(profile(25).is_none());
    let cfg = LabConfig::default();
    assert_eq!(cfg.stake(4), Usd::from_whole(20));
    assert_eq!(cfg.stake(26), Usd::from_whole(15));
    assert_eq!(profile(26), Some(Profile::Aggressive));
    assert_eq!(cfg.stake(36), Usd::from_whole(25));
    assert_eq!(profile(36), Some(Profile::Defensive));
    assert_eq!(cfg.stake(49), Usd::from_whole(20));
    assert_eq!(profile(49), Some(Profile::Balanced));
    let (a, b, d) = (26..=65u8).fold((0, 0, 0), |(a, b, d), f| match profile(f) {
        Some(Profile::Aggressive) => (a + 1, b, d),
        Some(Profile::Balanced) => (a, b + 1, d),
        _ => (a, b, d + 1),
    });
    assert_eq!((a, b, d), (12, 14, 14));
}

/// L28's dip, used to show the hold and the tolerance.
fn dip_world(now: &str) -> World {
    let mut w = World::new(now);
    mild_morning(&mut w);
    w.yes_book(19, "0.14", "0.16");
    w.yes_book(20, "0.08", "0.10");
    w.yes_book(21, "0.24", "0.26");
    w.yes_book(22, "0.10", "0.12");
    w
}

#[test]
fn a_signal_trades_only_once_it_has_held_a_minute() {
    let mut w = dip_world("10:00");
    let cfg = LabConfig {
        max_book_age_ms: 600_000,
        max_data_age_minutes: 24 * 60,
        ..LabConfig::default()
    };
    assert_eq!(cfg.confirm_seconds, 60);
    let mut s = LabStrategy::new(28, &cfg, &PeakSlotConfig::default());
    assert_blocked(&w.run_with(&mut s), "trades once it has held 60 s");
    w.now += Duration::seconds(30);
    assert_blocked(&w.run_with(&mut s), "signal since 10:00:00 UTC");
    w.now += Duration::seconds(31);
    let out = w.run_with(&mut s);
    assert_buy(proposal(&out), &w.yes(20), "0.10");
    // A gap of more than 15 s starts the clock again …
    let mut gone = dip_world("10:00");
    let mut s = LabStrategy::new(28, &cfg, &PeakSlotConfig::default());
    assert!(gone.run_with(&mut s).proposals.is_empty());
    gone.yes_book(20, "0.18", "0.20");
    gone.now += Duration::seconds(20);
    assert!(gone.run_with(&mut s).proposals.is_empty());
    gone.yes_book(20, "0.08", "0.10");
    gone.now += Duration::seconds(1);
    assert_blocked(&gone.run_with(&mut s), "signal since 10:00:21 UTC");
    gone.now += Duration::seconds(49);
    assert_blocked(&gone.run_with(&mut s), "signal since 10:00:21 UTC");
    gone.now += Duration::seconds(11);
    assert_buy(proposal(&gone.run_with(&mut s)), &gone.yes(20), "0.10");
    // … a shorter one (a book refreshing) does not.
    let mut blip = dip_world("10:00");
    let mut s = LabStrategy::new(28, &cfg, &PeakSlotConfig::default());
    assert!(blip.run_with(&mut s).proposals.is_empty());
    blip.yes_book(20, "0.18", "0.20");
    blip.now += Duration::seconds(10);
    assert!(blip.run_with(&mut s).proposals.is_empty());
    blip.yes_book(20, "0.08", "0.10");
    blip.now += Duration::seconds(50);
    assert_buy(proposal(&blip.run_with(&mut s)), &blip.yes(20), "0.10");
}

/// A bought position of `v` °C's YES in `w`'s book.
fn hold(w: &mut World, v: i32) {
    let o = w.m.outcome_for_value(v).unwrap().clone();
    w.positions
        .apply_fill(
            &Fill {
                client_order_id: ClientOrderId::new(format!("held-{v}")).unwrap(),
                token: o.yes_token.clone(),
                side: Side::Buy,
                price: p("0.10"),
                shares: Shares::from_whole(10),
                fee: Usd::ZERO,
                liquidity: Liquidity::Taker,
                ts: w.now,
            },
            &InstrumentRef {
                token: o.yes_token.clone(),
                condition_id: o.condition_id.clone(),
                event_slug: w.m.event_slug.clone(),
                outcome_side: OutcomeSide::Yes,
                bucket: o.bucket,
            },
        )
        .unwrap();
}

#[test]
fn a_taker_family_opens_one_position_a_day_a_maker_rests_again() {
    let mut w = dip_world("10:00");
    hold(&mut w, 23);
    assert_blocked(&w.run(28, false), "one trade a day");
    // L38's quotes rest and expire before each report: they come back.
    let mut m = World::new("12:00");
    warm_morning(&mut m);
    m.yes_book(20, "0.50", "0.52");
    m.no_book(22, "0.90", "0.94");
    m.no_book(23, "0.94", "0.97");
    let t21 = m.yes(21);
    m.taker("11:40:00", t21.clone(), Side::Buy, 0.30, 20.0, "a");
    m.taker("11:45:00", t21, Side::Sell, 0.29, 10.0, "b");
    hold(&mut m, 24);
    assert_eq!(m.run(38, false).proposals.len(), 2);
}

#[test]
fn every_new_family_evaluates_a_bare_day_and_never_panics() {
    // A high, books on every token, no lab input at all: only the rules
    // that read the book alone may trade.
    let mut w = World::new("12:15");
    high_21(&mut w);
    for v in 13..=24 {
        w.yes_book(v, "0.10", "0.12");
        w.no_book(v, "0.88", "0.90");
    }
    let book_only = [28u8, 39, 40, 45, 48, 54, 58, 59, 60];
    for family in 26..=65u8 {
        for variant in [false, true] {
            let out = w.run(family, variant);
            assert!(!out.evaluations.is_empty(), "L{family} says nothing");
            for e in &out.evaluations {
                assert_eq!(
                    wm_strategy::lab::family_of(e.strategy.as_str()),
                    Some(family)
                );
                assert!(e.signal || !e.blockers.is_empty(), "L{family}: {e:?}");
            }
            for pr in &out.proposals {
                assert!(
                    book_only.contains(&family),
                    "L{family} traded without its inputs: {pr:?}"
                );
            }
        }
    }
}

#[test]
fn a_taker_counts_its_profit_at_the_limit_and_sizes_to_its_profile() {
    let w = dip_world("10:00");
    let out = w.run(28, false);
    let pr = proposal(&out);
    assert_buy(pr, &w.yes(20), "0.10");
    // $15 (aggressive) at the limit 0.11.
    assert_eq!(pr.shares, Shares::from_whole(136));
    assert!(pr.rationale.iter().any(|r| r.contains("limit 0.11")));
    // Its break-even is counted at the limit, fee and slippage included.
    assert!(pr.break_even > 0.11);
}

// ---------------------------------------------------------- upstream ---

#[test]
fn l26_buys_the_next_degree_when_warm_air_is_on_its_way() {
    let mut w = World::new("10:00");
    w.temps(&[("08:55", 180)]);
    w.metar(
        "09:55",
        190,
        Some(100),
        "EHAM 010955Z 20012KT 9999 FEW030 19/10 Q1015",
    );
    w.upstream(
        "EBAW",
        185.0,
        126.0,
        &[("09:50", 220, "EBAW 010950Z 19010KT CAVOK 22/12 Q1014")],
    );
    w.yes_book(20, "0.14", "0.16");
    let out = w.run(26, false);
    let pr = proposal(&out);
    assert_buy(pr, &w.yes(20), "0.16");
    assert_eq!(pr.shares, Shares::from_whole(88), "$15 at 0.17");
    assert!(pr.rationale[0].contains("EBAW"));
    assert_blocked(&w.run(26, true), "needs ≥ 3.5 °C warmer");
    // A northerly wind has no warm airport upstream.
    let mut north = World::new("10:00");
    north.temps(&[("08:55", 180)]);
    north.metar(
        "09:55",
        190,
        None,
        "EHAM 010955Z 02012KT 9999 FEW030 19/10 Q1015",
    );
    north.upstream(
        "EBAW",
        185.0,
        126.0,
        &[("09:50", 220, "EBAW 010950Z 19010KT CAVOK 22/12 Q1014")],
    );
    north.yes_book(20, "0.14", "0.16");
    assert_blocked(&north.run(26, false), "no airport within 35°");
}

#[test]
fn l31_buys_the_next_degree_when_the_clear_sky_upwind_arrives_in_time() {
    let mut w = World::new("09:30");
    w.temps(&[("06:55", 150), ("08:55", 170)]);
    w.metar(
        "09:25",
        180,
        Some(140),
        "EHAM 010925Z 24016KT 9999 BKN015 18/14 Q1012",
    );
    w.upstream(
        "EBOS",
        230.0,
        120.0,
        &[
            ("08:50", 190, "EBOS 010850Z 24012KT CAVOK 19/12 Q1013"),
            ("09:20", 190, "EBOS 010920Z 24012KT 9999 FEW040 19/12 Q1013"),
        ],
    );
    w.yes_book(19, "0.12", "0.14");
    assert_buy(proposal(&w.run(31, false)), &w.yes(19), "0.14");
    assert_blocked(&w.run(31, true), "within 2 h");
    let mut cloudy = World::new("09:30");
    cloudy.temps(&[("06:55", 150), ("08:55", 170)]);
    cloudy.metar(
        "09:25",
        180,
        None,
        "EHAM 010925Z 24016KT 9999 BKN015 18/14 Q1012",
    );
    cloudy.upstream(
        "EBOS",
        230.0,
        120.0,
        &[
            ("08:50", 190, "EBOS 010850Z 24012KT 9999 BKN012 19/12 Q1013"),
            ("09:20", 190, "EBOS 010920Z 24012KT 9999 FEW040 19/12 Q1013"),
        ],
    );
    cloudy.yes_book(19, "0.12", "0.14");
    assert_blocked(&cloudy.run(31, false), "not clear in its last two reports");
}

fn front_world(drop_to: i32) -> World {
    let mut w = World::new("09:00");
    mild_morning(&mut w);
    w.metar(
        "08:55",
        180,
        None,
        "EHAM 010855Z 25025KT 9999 SCT030 18/12 Q1012",
    );
    let after = format!(
        "EGSH 010820Z 30018KT 9999 -RA BKN012 {}/13 Q1010",
        drop_to / 10
    );
    w.upstream(
        "EGSH",
        265.0,
        240.0,
        &[
            ("07:50", 180, "EGSH 010750Z 21012KT 9999 SCT030 18/14 Q1008"),
            ("08:20", drop_to, &after),
        ],
    );
    w.no_book(19, "0.70", "0.72");
    w
}

#[test]
fn l36_sells_the_next_degree_when_a_front_upstream_is_due_before_the_peak() {
    let w = front_world(150);
    let out = w.run(36, false);
    let pr = proposal(&out);
    assert_buy(pr, &w.no(19), "0.72");
    assert_eq!(pr.shares, Shares::from_whole(34), "$25 (defensive) at 0.73");
    assert!(pr.rationale[0].contains("front at EGSH"));
    assert_eq!(proposal(&w.run(36, true)).token, w.no(19), "3 °C down");
    assert_blocked(&front_world(160).run(36, true), "no front");
    assert_eq!(proposal(&front_world(160).run(36, false)).token, w.no(19));
}

// --------------------------------------------------------------- air ---

#[test]
fn l27_buys_above_the_favourite_when_the_dew_point_drops_as_it_warms() {
    let mut w = World::new("09:00");
    w.temps(&[("06:55", 160), ("08:25", 180)]);
    w.reading("07:50", 180);
    w.reading("08:50", 192);
    w.extra("07:50", &[("td", 12.0)]);
    w.extra("08:50", &[("td", 10.2)]);
    w.yes_book(19, "0.40", "0.42");
    w.yes_book(20, "0.20", "0.22");
    w.yes_book(21, "0.08", "0.10");
    assert_buy(proposal(&w.run(27, false)), &w.yes(20), "0.22");
    assert_blocked(&w.run(27, true), "down 1.8 < 2.5");
    w.extras.clear();
    assert_blocked(&w.run(27, false), "no KNMI 'td' reading");
}

#[test]
fn l29_buys_the_next_degree_when_the_wind_drops_under_the_sun() {
    let mut w = World::new("11:00");
    warm_morning(&mut w);
    w.reading("10:50", 196);
    w.extra("09:50", &[("ff", 5.0)]);
    w.extra("10:30", &[("ss", 8.0)]);
    w.extra("10:40", &[("ss", 7.0)]);
    w.extra("10:50", &[("ff", 2.8), ("ss", 3.0)]);
    w.yes_book(21, "0.18", "0.20");
    assert_buy(proposal(&w.run(29, false)), &w.yes(21), "0.20");
    assert_blocked(&w.run(29, true), "needs ≤ 2.5");
    w.extra("10:40", &[("ss", 2.0)]);
    assert_blocked(&w.run(29, false), "sunshine ≥ 7 min in 1 of the last 3");
}

#[test]
fn l32_buys_the_next_degree_when_gusts_begin() {
    let mut w = World::new("10:30");
    warm_morning(&mut w);
    w.extra("09:20", &[("ff", 4.0), ("fx", 5.5)]);
    w.extra("10:00", &[("ff", 4.0), ("fx", 8.5)]);
    w.extra("10:10", &[("ff", 4.2), ("fx", 8.6)]);
    w.extra("10:20", &[("ff", 4.1), ("fx", 8.4)]);
    w.yes_book(21, "0.10", "0.12");
    assert_buy(proposal(&w.run(32, false)), &w.yes(21), "0.12");
    assert_blocked(&w.run(32, true), "needs ≥ 5.0 m/s");
    w.extra("09:20", &[("fx", 7.0)]);
    assert_blocked(&w.run(32, false), "already gusty an hour before");
}

#[test]
fn l37_sells_the_next_degree_when_the_pressure_falls() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.reading("11:50", 195);
    w.extra("08:50", &[("pp", 1015.0)]);
    w.extra("11:50", &[("pp", 1012.6)]);
    w.no_book(21, "0.80", "0.82");
    assert_buy(proposal(&w.run(37, false)), &w.no(21), "0.82");
    assert_blocked(&w.run(37, true), "down 2.4 < 3.0");
    w.reading("11:50", 200);
    assert_blocked(&w.run(37, false), "not ≥ 0.8 °C under the edge");
}

#[test]
fn l42_sells_the_next_degree_in_saturated_air() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.reading("10:50", 190);
    w.reading("11:50", 192);
    for hm in ["11:00", "11:10", "11:20", "11:30", "11:40", "11:50"] {
        w.extra(hm, &[("rh", 95.0)]);
    }
    w.no_book(21, "0.85", "0.87");
    assert_buy(proposal(&w.run(42, false)), &w.no(21), "0.87");
    assert_blocked(&w.run(42, true), "< 97 %");
    w.reading("11:50", 194);
    assert_blocked(&w.run(42, false), "> 0.3 °C");
}

#[test]
fn l43_sells_the_next_degree_on_an_onshore_day() {
    let mut w = World::new("13:00");
    warm_morning(&mut w);
    w.metar(
        "12:55",
        190,
        None,
        "EHAM 011255Z 30012KT 9999 SCT030 19/13 Q1016",
    );
    w.reading("12:50", 192);
    let mut t = z("07:10");
    let mut i = 0;
    while t <= z("12:50") {
        let d = if i % 12 == 5 { 180.0 } else { 300.0 };
        w.extra(&t.format("%H:%M").to_string(), &[("dd", d)]);
        t += Duration::minutes(10);
        i += 1;
    }
    w.no_book(21, "0.88", "0.90");
    assert_buy(proposal(&w.run(43, false)), &w.no(21), "0.90");
    assert_blocked(&w.run(43, true), "< 95 %");
}

// ------------------------------------------------------------ ladder ---

#[test]
fn l28_buys_the_dip_between_two_likelier_neighbours() {
    let w = dip_world("10:00");
    assert_buy(proposal(&w.run(28, false)), &w.yes(20), "0.10");
    assert_blocked(&w.run(28, true), "no dip");
    assert_blocked(&dip_world("14:30").run(28, false), "outside 00:00–16:00");
}

fn whale_world(size: i64) -> World {
    let mut w = World::new("10:00");
    mild_morning(&mut w);
    for m in 45..60 {
        w.sample(&format!("09:{m}"), &[(19, 0.30, 0.32, 100.0)]);
    }
    w.levels(w.yes(19), &[("0.30", size)], &[("0.32", 100)]);
    w
}

#[test]
fn l33_follows_a_new_bid_wall() {
    let w = whale_world(1000);
    assert_buy(proposal(&w.run(33, false)), &w.yes(19), "0.32");
    assert_blocked(&w.run(33, true), "no bid wall");
    assert_blocked(&whale_world(300).run(33, false), "no bid wall");
}

#[test]
fn l34_takes_an_ask_the_warming_left_behind() {
    let mut w = World::new("11:00");
    warm_morning(&mut w);
    let mut t = z("10:05");
    while t <= z("11:00") {
        w.sample_at(t, &[(21, 0.18, 0.20, 150.0)]);
        t += Duration::minutes(1);
    }
    w.levels(w.yes(21), &[("0.18", 150)], &[("0.20", 150)]);
    w.reading("09:50", 190);
    w.reading("10:50", 201);
    assert_buy(proposal(&w.run(34, false)), &w.yes(21), "0.20");
    assert_blocked(&w.run(34, true), "KNMI up 1.1 °C in an hour < 1.5 °C");
    w.levels(w.yes(21), &[("0.18", 150)], &[("0.20", 120)]);
    assert_blocked(&w.run(34, false), "changed in the last 55 min");
}

fn pair_world(other: &str) -> World {
    let mut w = World::new("18:00");
    high_21(&mut w);
    w.yes_book(21, "0.77", "0.79");
    w.yes_book(22, "0.13", "0.15");
    w.book(w.yes(23), None, Some(other));
    w.book(w.yes(24), None, Some("0.01"));
    w
}

#[test]
fn l39_locks_the_pair_of_the_high_and_the_next_degree() {
    let w = pair_world("0.01");
    let out = w.run(39, false);
    assert_eq!(out.proposals.len(), 2, "{}", blockers(&out));
    assert_eq!(out.proposals[0].token, w.yes(21));
    assert_eq!(out.proposals[1].token, w.yes(22));
    assert!(
        out.proposals
            .iter()
            .all(|p| p.shares == Shares::from_whole(26))
    );
    assert_blocked(&w.run(39, true), "> 0.98");
    assert_blocked(&pair_world("0.03").run(39, false), "still priced 0.030");
    // Both legs or neither: a wide book on one leg stops the other.
    let mut wide = pair_world("0.01");
    wide.yes_book(22, "0.01", "0.15");
    assert_blocked(&wide.run(39, false), "the other leg cannot trade");
}

#[test]
fn l40_sells_a_tail_priced_above_the_step_before_it() {
    let mut w = World::new("13:00");
    high_21(&mut w);
    w.yes_book(22, "0.04", "0.06");
    w.yes_book(23, "0.07", "0.09");
    w.no_book(23, "0.90", "0.92");
    assert_buy(proposal(&w.run(40, false)), &w.no(23), "0.92");
    w.yes_book(23, "0.05", "0.07");
    assert_blocked(&w.run(40, false), "not ≥");
    assert_eq!(proposal(&w.run(40, true)).token, w.no(23));
}

fn gap_world(last: &str) -> World {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.no_book(20, "0.53", "0.55");
    w.yes_book(21, "0.40", "0.42");
    w.yes_book(22, "0.15", "0.17");
    w.yes_book(23, "0.05", "0.07");
    w.book(w.yes(24), Some(last), Some("0.03"));
    w
}

#[test]
fn l45_buys_a_no_cheaper_than_the_other_buckets_yes() {
    let w = gap_world("0.02");
    let out = w.run(45, false);
    assert_buy(proposal(&out), &w.no(20), "0.55");
    assert_eq!(proposal(&w.run(45, true)).token, w.no(20));
    let mut narrow = gap_world("0.01");
    assert_eq!(proposal(&narrow.run(45, false)).token, w.no(20));
    assert_blocked(&narrow.run(45, true), "edge");
    // A dead bucket's leftover bid is no chance of the NO paying.
    narrow.yes_book(19, "0.05", "0.07");
    assert_blocked(&narrow.run(45, true), "edge");
}

#[test]
fn l48_sells_the_high_s_bucket_the_ladder_s_own_mean_rules_out() {
    let mut w = World::new("07:00");
    w.temps(&[("05:55", 150)]);
    for (v, b, a) in [
        (15, "0.06", "0.08"),
        (16, "0.02", "0.04"),
        (17, "0.05", "0.07"),
        (18, "0.15", "0.17"),
        (19, "0.30", "0.32"),
        (20, "0.25", "0.27"),
        (21, "0.10", "0.12"),
    ] {
        w.yes_book(v, b, a);
    }
    w.no_book(15, "0.90", "0.92");
    assert_buy(proposal(&w.run(48, false)), &w.no(15), "0.92");
    w.yes_book(15, "0.02", "0.04");
    assert_blocked(&w.run(48, false), "no contradiction");
}

fn overround_world(top: &str) -> World {
    let mut w = World::new("10:00");
    mild_morning(&mut w);
    for (v, b, a) in [
        (18, "0.09", "0.11"),
        (19, "0.29", "0.31"),
        (20, top, "0.41"),
        (21, "0.19", "0.21"),
        (22, "0.05", "0.07"),
        (23, "0.02", "0.04"),
    ] {
        w.yes_book(v, b, a);
    }
    w.book(w.yes(24), None, Some("0.01"));
    w.no_book(20, "0.58", "0.60");
    w
}

#[test]
fn l54_sells_the_bucket_that_carries_the_overround() {
    let w = overround_world("0.39");
    assert_buy(proposal(&w.run(54, false)), &w.no(20), "0.60");
    assert_blocked(&w.run(54, true), "< 1.12");
    let mut fair = overround_world("0.31");
    assert_blocked(&fair.run(54, false), "< 1.08");
    // A dead bucket's quotes are no overround.
    fair.yes_book(17, "0.09", "0.11");
    assert_blocked(&fair.run(54, false), "< 1.08");
}

fn round_world(q: &str, a: &str) -> World {
    let mut w = World::new("10:00");
    mild_morning(&mut w);
    for (v, b, ask) in [
        (18, "0.04", "0.06"),
        (19, "0.24", "0.26"),
        (20, q, a),
        (21, "0.19", "0.21"),
        (22, "0.04", "0.06"),
    ] {
        w.yes_book(v, b, ask);
    }
    w.no_book(20, "0.54", "0.56");
    w
}

#[test]
fn l58_sells_a_round_number_priced_over_its_neighbours_curve() {
    let w = round_world("0.44", "0.46");
    let out = w.run(58, false);
    assert_buy(proposal(&out), &w.no(20), "0.56");
    assert!(proposal(&out).rationale[0].contains("normal"));
    assert_blocked(&round_world("0.34", "0.36").run(58, false), "< 0.08");
}

fn scalp_world(bid_size: i64) -> World {
    let mut w = World::new("10:10");
    mild_morning(&mut w);
    w.levels(w.yes(20), &[("0.50", bid_size)], &[("0.52", 100)]);
    w.yes_book(19, "0.20", "0.22");
    w
}

#[test]
fn l59_makes_one_round_trip_on_an_imbalanced_book() {
    let w = scalp_world(400);
    let out = w.run(59, false);
    let pr = proposal(&out);
    assert_buy(pr, &w.yes(20), "0.52");
    assert_eq!(pr.shares, Shares::from_whole(37), "$20 (balanced) at 0.53");
    assert_blocked(&w.run(59, true), "needs ≥ 100 and ≥ 5 ×");
    // Held: sold at the bid once it pays 0.03 over the average cost.
    let mut held = scalp_world(400);
    let o = held.m.outcome_for_value(20).unwrap().clone();
    held.positions
        .apply_fill(
            &Fill {
                client_order_id: ClientOrderId::new("l59-1").unwrap(),
                token: o.yes_token.clone(),
                side: Side::Buy,
                price: p("0.50"),
                shares: Shares::from_whole(37),
                fee: Usd::ZERO,
                liquidity: Liquidity::Taker,
                ts: z("10:00"),
            },
            &InstrumentRef {
                token: o.yes_token.clone(),
                condition_id: o.condition_id.clone(),
                event_slug: held.m.event_slug.clone(),
                outcome_side: OutcomeSide::Yes,
                bucket: o.bucket,
            },
        )
        .unwrap();
    let mut s = LabStrategy::new(59, &World::cfg(59, false), &PeakSlotConfig::default());
    assert_blocked(&held.run_with(&mut s), "holding 37 YES");
    held.levels(held.yes(20), &[("0.53", 400)], &[("0.55", 100)]);
    let out = held.run_with(&mut s);
    let pr = proposal(&out);
    assert_eq!((pr.side, pr.kind), (Side::Sell, IntentKind::Reduce));
    assert_eq!(pr.limit_price, p("0.53"));
    // After 45 minutes it sells whatever the bid.
    held.levels(held.yes(20), &[("0.51", 400)], &[("0.53", 100)]);
    held.now += Duration::minutes(46);
    held.levels(held.yes(20), &[("0.51", 400)], &[("0.53", 100)]);
    let out = held.run_with(&mut s);
    assert!(proposal(&out).rationale[0].contains("≥ 45"));
}

fn gapfill_world(second: &str) -> World {
    let mut w = World::new("10:05");
    mild_morning(&mut w);
    w.levels(w.yes(19), &[("0.38", 200)], &[("0.40", 30), (second, 200)]);
    w.yes_book(20, "0.20", "0.22");
    w.no_book(19, "0.52", "0.62");
    w
}

#[test]
fn l60_offers_yes_in_the_gap_of_a_thin_ask_ladder() {
    let w = gapfill_world("0.52");
    let out = w.run(60, false);
    let pr = proposal(&out);
    assert_eq!(pr.token, w.no(19));
    assert_eq!(pr.limit_price, p("0.54"), "YES offered halfway: 0.46");
    assert_eq!(
        pr.tif,
        TimeInForce::Gtd {
            expires_at: z("10:15")
        }
    );
    assert_blocked(&w.run(60, true), "< 0.15");
    assert_blocked(&gapfill_world("0.45").run(60, false), "< 0.10");
}

fn vacuum_world(now_size: i64) -> World {
    let mut w = World::new("10:10");
    mild_morning(&mut w);
    w.sample("10:05", &[(19, 0.19, 0.21, 500.0)]);
    w.levels(w.yes(19), &[("0.19", 200)], &[("0.21", now_size)]);
    w
}

#[test]
fn l64_follows_sellers_who_withdraw() {
    let w = vacuum_world(150);
    assert_buy(proposal(&w.run(64, false)), &w.yes(19), "0.21");
    assert_blocked(&w.run(64, true), "< 75 %");
    assert_blocked(&vacuum_world(300).run(64, false), "down 40 %");
}

// -------------------------------------------------------------- tape ---

#[test]
fn l38_quotes_the_tails_on_a_calm_tape() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.yes_book(20, "0.50", "0.52");
    w.no_book(22, "0.90", "0.94");
    w.no_book(23, "0.94", "0.97");
    let t21 = w.yes(21);
    w.taker("11:40:00", t21.clone(), Side::Buy, 0.30, 20.0, "a");
    w.taker("11:45:00", t21.clone(), Side::Sell, 0.29, 10.0, "b");
    let out = w.run(38, false);
    assert_eq!(out.proposals.len(), 2, "{}", blockers(&out));
    assert_eq!(out.proposals[0].token, w.no(22));
    assert_eq!(out.proposals[0].limit_price, p("0.91"));
    assert_eq!(out.proposals[1].token, w.no(23));
    assert!(
        matches!(out.proposals[0].tif, TimeInForce::Gtd { expires_at } if expires_at == z("12:15"))
    );
    w.taker("11:55:00", t21.clone(), Side::Buy, 0.31, 250.0, "c");
    w.taker("11:56:00", t21, Side::Sell, 0.30, 230.0, "d");
    assert_blocked(&w.run(38, false), "a trade of 250 shares");
}

#[test]
fn l44_fades_a_longshot_bought_by_a_wallet_without_a_record() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.wallets = Some(scores(&[("known", 40, -0.01, -0.5)]));
    w.no_book(22, "0.92", "0.97");
    let t22 = w.yes(22);
    w.taker("11:45:00", t22.clone(), Side::Buy, 0.06, 30.0, "newbie");
    let out = w.run(44, false);
    let pr = proposal(&out);
    assert_eq!((pr.token.clone(), pr.limit_price), (w.no(22), p("0.95")));
    assert_blocked(&w.run(44, true), "no buy of ≥ 50 YES");
    let mut known = World::new("12:00");
    warm_morning(&mut known);
    known.wallets = Some(scores(&[("known", 40, -0.01, -0.5)]));
    known.no_book(22, "0.92", "0.97");
    known.taker("11:45:00", t22, Side::Buy, 0.06, 30.0, "known");
    assert_blocked(&known.run(44, false), "without a record");
}

#[test]
fn l47_quotes_against_the_evening_lottery_tickets() {
    let mut w = World::new("17:00");
    high_21(&mut w);
    w.reading("16:50", 180);
    w.no_book(22, "0.93", "0.96");
    let t22 = w.yes(22);
    for (i, at) in ["16:35:00", "16:40:00", "16:45:00", "16:50:00", "16:55:00"]
        .into_iter()
        .enumerate()
    {
        w.taker(at, t22.clone(), Side::Buy, 0.05, 10.0, &format!("r{i}"));
    }
    let out = w.run(47, false);
    let pr = proposal(&out);
    assert_eq!((pr.token.clone(), pr.limit_price), (w.no(22), p("0.94")));
    assert_blocked(&w.run(47, true), "5 small YES buys of 22°C in 30 min < 8");
}

#[test]
fn l63_sells_a_bucket_whose_ask_keeps_refilling() {
    let mut w = World::new("12:00");
    mild_morning(&mut w);
    w.sample("11:39", &[(20, 0.28, 0.30, 50.0)]);
    let t20 = w.yes(20);
    for (i, at) in ["11:40:00", "11:45:00", "11:50:00"].into_iter().enumerate() {
        w.taker(at, t20.clone(), Side::Buy, 0.30, 60.0, &format!("b{i}"));
    }
    w.yes_book(20, "0.28", "0.30");
    w.no_book(20, "0.69", "0.71");
    assert_buy(proposal(&w.run(63, false)), &w.no(20), "0.71");
    assert_blocked(&w.run(63, true), "no ask refilled");
    w.yes_book(20, "0.30", "0.32");
    assert_blocked(&w.run(63, false), "no ask refilled");
}

// ---------------------------------------------------------- reaction ---

fn drift_world(now_bid: &str, now_ask: &str) -> World {
    let mut w = World::new("10:05");
    w.temps(&[("08:55", 180), ("09:25", 190), ("09:55", 200)]);
    w.sample("09:58", &[(21, 0.11, 0.13, 200.0)]);
    w.yes_book(21, now_bid, now_ask);
    w.reading("09:30", 196);
    w.reading("10:00", 201);
    w
}

#[test]
fn l35_buys_the_next_degree_the_market_did_not_move_after_a_new_high() {
    let w = drift_world("0.15", "0.17");
    assert_buy(proposal(&w.run(35, false)), &w.yes(21), "0.17");
    assert_blocked(&w.run(35, true), "> 0.02: priced in");
    let mut early = drift_world("0.15", "0.17");
    early.now = z("10:00");
    assert_blocked(&early.run(35, false), "3–15 wanted");
}

#[test]
fn l46_buys_the_evening_discount_on_a_settled_high() {
    let mut w = World::new("19:30");
    high_21(&mut w);
    w.reading("19:10", 172);
    w.reading("19:20", 170);
    w.yes_book(21, "0.95", "0.97");
    assert_buy(proposal(&w.run(46, false)), &w.yes(21), "0.97");
    assert_blocked(&w.run(46, true), "outside 22:00");
    w.reading("19:20", 175);
    assert_blocked(&w.run(46, false), "KNMI rising");
}

fn newsless_world() -> World {
    let mut w = World::new("10:00");
    w.temps(&[("06:55", 150), ("08:55", 180), ("09:25", 190)]);
    w.sample("09:51", &[(20, 0.39, 0.41, 200.0)]);
    w.yes_book(20, "0.24", "0.26");
    w.reading("09:30", 184);
    w.reading("09:50", 185);
    w
}

#[test]
fn l50_fades_a_move_without_news() {
    let w = newsless_world();
    assert_buy(proposal(&w.run(50, false)), &w.yes(20), "0.26");
    assert_blocked(&w.run(50, true), "< 0.18");
    let mut news = newsless_world();
    news.temps(&[("09:55", 190)]);
    assert_blocked(&news.run(50, false), "is news");
}

#[test]
fn l51_buys_against_prices_that_fall_while_the_thermometer_rises() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.sample(
        "11:30",
        &[
            (20, 0.19, 0.21, 200.0),
            (21, 0.49, 0.51, 200.0),
            (22, 0.29, 0.31, 200.0),
        ],
    );
    w.yes_book(20, "0.59", "0.61");
    w.yes_book(21, "0.29", "0.31");
    w.yes_book(22, "0.04", "0.06");
    w.reading("11:20", 195);
    w.reading("11:50", 201);
    assert_buy(proposal(&w.run(51, false)), &w.yes(21), "0.31");
    assert_blocked(&w.run(51, true), "needs ≥ 0.9");
}

fn runup_world(now_bid: &str, now_ask: &str) -> World {
    let mut w = World::new("12:15");
    warm_morning(&mut w);
    w.temps(&[("11:55", 200)]);
    w.sample("12:05", &[(21, 0.19, 0.21, 200.0)]);
    w.yes_book(21, now_bid, now_ask);
    w.no_book(21, "0.72", "0.74");
    w.reading("12:10", 199);
    w
}

#[test]
fn l52_fades_a_pre_report_run_up_knmi_does_not_back() {
    let w = runup_world("0.25", "0.27");
    assert_buy(proposal(&w.run(52, false)), &w.no(21), "0.74");
    assert_blocked(&w.run(52, true), "up < 0.08");
    let mut backed = runup_world("0.25", "0.27");
    backed.reading("12:10", 203);
    assert_blocked(&backed.run(52, false), "the run-up may be right");
}

#[test]
fn l53_buys_where_the_day_s_curve_points() {
    let mut w = World::new("09:00");
    w.temps(&[
        ("02:55", 120),
        ("05:55", 140),
        ("07:55", 170),
        ("08:55", 190),
    ]);
    w.reading("08:50", 192);
    w.yes_book(19, "0.39", "0.41");
    w.yes_book(20, "0.29", "0.31");
    w.yes_book(21, "0.04", "0.06");
    let out = w.run(53, false);
    assert_buy(proposal(&out), &w.yes(20), "0.31");
    assert!(proposal(&out).rationale[0].contains("the curve peaks at 20.4"));
    assert_blocked(&w.run(53, true), "off by < 1.2");
}

fn hover_world(first: i32) -> World {
    let mut w = World::new("12:00");
    high_21(&mut w);
    w.reading("11:30", first);
    w.reading("11:40", 216);
    w.reading("11:50", 215);
    w.yes_book(21, "0.70", "0.72");
    w.yes_book(22, "0.20", "0.22");
    w
}

#[test]
fn l55_buys_the_cheap_side_of_a_coin_flip_on_the_edge() {
    let w = hover_world(213);
    assert_buy(proposal(&w.run(55, false)), &w.yes(22), "0.22");
    assert_blocked(&w.run(55, true), "not all within 0.1 °C");
    assert_blocked(&hover_world(211).run(55, false), "not all within 0.2 °C");
}

#[test]
fn l56_sells_the_favourite_when_the_stations_disagree() {
    let mut w = World::new("12:00");
    warm_morning(&mut w);
    w.reading("11:50", 190);
    w.neighbours = vec![
        ("Voorschoten".into(), 225.0, vec![knmi("11:50", 175)]),
        ("De Bilt".into(), 120.0, vec![knmi("11:50", 210)]),
    ];
    w.yes_book(20, "0.60", "0.62");
    w.no_book(20, "0.37", "0.39");
    assert_buy(proposal(&w.run(56, false)), &w.no(20), "0.39");
    assert_blocked(&w.run(56, true), "apart < 4.0 °C");
}

fn laggard_world(next_bid: &str, next_ask: &str) -> World {
    let mut w = World::new("12:00");
    high_21(&mut w);
    w.sample("11:55", &[(21, 0.59, 0.61, 200.0), (22, 0.19, 0.21, 200.0)]);
    w.yes_book(21, "0.60", "0.62");
    w.yes_book(22, next_bid, next_ask);
    w.no_book(21, "0.38", "0.40");
    w.reading("11:50", 214);
    w
}

#[test]
fn l57_sells_the_high_s_bucket_that_lags_the_next_degree() {
    let w = laggard_world("0.26", "0.28");
    assert_buy(proposal(&w.run(57, false)), &w.no(21), "0.40");
    assert_blocked(&w.run(57, true), "< 0.10");
    assert_blocked(&laggard_world("0.22", "0.24").run(57, false), "< 0.06");
}

fn speci_world(speci: &str) -> World {
    let mut w = World::new("10:10");
    w.metar(
        "09:25",
        170,
        Some(150),
        "EHAM 010925Z 24008KT 9999 BKN008 17/15 Q1015",
    );
    w.metar(
        "09:55",
        180,
        Some(150),
        "EHAM 010955Z 24008KT 9999 BKN010 18/15 Q1015",
    );
    w.metar("10:02", 180, Some(140), speci);
    w.sample("10:05", &[(19, 0.19, 0.21, 200.0)]);
    w.yes_book(19, "0.22", "0.24");
    w.no_book(19, "0.76", "0.78");
    w
}

#[test]
fn l61_buys_the_next_degree_when_a_speci_opens_the_sky() {
    let w = speci_world("EHAM 011002Z 24009KT 9999 SCT035 18/14 Q1015");
    assert_buy(proposal(&w.run(61, false)), &w.yes(19), "0.24");
    assert_blocked(&w.run(61, true), "shows no worsening");
    let wet = speci_world("EHAM 011002Z 24009KT 6000 -RA BKN008 18/15 Q1015");
    assert_buy(proposal(&wet.run(61, true)), &w.no(19), "0.78");
    assert_blocked(&wet.run(61, false), "shows no improvement");
}

fn haze_world(raw: &str) -> World {
    let mut w = World::new("11:30");
    w.metar("10:55", 210, Some(80), raw);
    w.metar("11:25", 200, Some(80), raw);
    w.no_book(22, "0.85", "0.88");
    w
}

#[test]
fn l62_sells_the_next_degree_under_a_haze_lid() {
    let w = haze_world("EHAM 011055Z 09008KT 6000 FEW040 21/08 Q1020");
    assert_buy(proposal(&w.run(62, false)), &w.no(22), "0.88");
    assert_blocked(&w.run(62, true), "no dry haze ≤ 5 km");
    let named = haze_world("EHAM 011055Z 09008KT 6000 HZ FEW040 21/08 Q1020");
    assert_eq!(proposal(&named.run(62, true)).token, w.no(22));
    let clear = haze_world("EHAM 011055Z 09008KT 9999 FEW040 21/08 Q1020");
    assert_blocked(&clear.run(62, false), "no dry haze");
}

// ------------------------------------------------------------ memory ---

fn anchor_world(fmax: i32) -> World {
    let mut w = World::new("07:00");
    w.temps(&[("05:55", 150)]);
    for (hm, t) in [
        ("10:25", 190),
        ("11:55", 200),
        ("12:55", 210),
        ("13:55", 200),
        ("14:25", 190),
    ] {
        w.yesterday(hm, t, "");
    }
    w.forecast(|h| if h == 14 { fmax } else { 150 });
    w.yes_book(21, "0.40", "0.42");
    w.yes_book(23, "0.10", "0.12");
    w.yes_book(24, "0.10", "0.12");
    w
}

#[test]
fn l30_buys_the_forecast_s_bucket_when_the_market_anchors_on_yesterday() {
    let w = anchor_world(230);
    assert_buy(proposal(&w.run(30, false)), &w.yes(23), "0.12");
    assert_blocked(&w.run(30, true), "within 2.5 °C of yesterday's high 21 °C");
    let mut few = anchor_world(230);
    few.yesterday.truncate(2);
    assert_blocked(
        &few.run(30, false),
        "of yesterday's METARs from its afternoon",
    );
}

#[test]
fn l41_sells_above_the_forecast_after_a_wet_day() {
    let mut w = World::new("08:00");
    w.temps(&[("05:55", 150)]);
    for (hm, t, raw) in [
        (
            "08:25",
            170,
            "EHAM 300825Z 22010KT 6000 -RA BKN010 17/15 Q1008",
        ),
        (
            "09:25",
            180,
            "EHAM 300925Z 22010KT 6000 -RA BKN010 18/15 Q1008",
        ),
        (
            "10:25",
            190,
            "EHAM 301025Z 22010KT 6000 RA BKN010 19/16 Q1008",
        ),
        (
            "11:25",
            200,
            "EHAM 301125Z 22010KT 6000 -SHRA BKN010 20/16 Q1008",
        ),
        (
            "12:25",
            200,
            "EHAM 301225Z 22010KT 6000 -RA BKN010 20/16 Q1008",
        ),
        ("13:25", 190, "EHAM 301325Z 22010KT 9999 BKN020 19/15 Q1009"),
    ] {
        w.yesterday(hm, t, raw);
    }
    w.forecast(|h| if h == 14 { 220 } else { 150 });
    w.no_book(23, "0.85", "0.88");
    assert_buy(proposal(&w.run(41, false)), &w.no(23), "0.88");
    assert_blocked(&w.run(41, true), "< 8");
}

fn edge_world(b22: &str, a22: &str) -> World {
    let mut w = World::new("07:00");
    w.temps(&[("05:55", 150)]);
    w.forecast(|h| if h == 14 { 213 } else { 150 });
    w.yes_book(21, "0.50", "0.52");
    w.yes_book(22, b22, a22);
    w
}

#[test]
fn l49_buys_the_cheap_side_of_a_forecast_on_the_edge() {
    let w = edge_world("0.25", "0.27");
    assert_buy(proposal(&w.run(49, false)), &w.yes(22), "0.27");
    assert_blocked(&w.run(49, true), "more than 0.45 ×");
    assert_blocked(
        &edge_world("0.30", "0.32").run(49, false),
        "more than 0.55 ×",
    );
}

fn midnight_world(rest: i32) -> World {
    let mut w = World::new("07:00");
    w.metars.push(observation(y("22:25"), 160, None, ""));
    w.metars.push(observation(y("23:25"), 150, None, ""));
    w.temps(&[("01:25", 140), ("04:55", 120), ("06:55", 130)]);
    w.reading("06:50", 135);
    w.forecast(|h| if h == 15 { rest } else { 130 });
    w.yes_book(16, "0.40", "0.42");
    w
}

#[test]
fn l65_buys_a_high_that_came_at_midnight() {
    let w = midnight_world(158);
    assert_buy(proposal(&w.run(65, false)), &w.yes(16), "0.42");
    assert_blocked(&w.run(65, true), "the forecast still reaches 15.8 °C");
    assert_blocked(
        &midnight_world(170).run(65, false),
        "the forecast still reaches 17.0 °C",
    );
}

fn scores(list: &[(&str, u64, f64, f64)]) -> WalletScores {
    WalletScores::from_event(&WalletScoresEvent {
        through: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
        days: 30,
        takers: list.len() as u64,
        scores: list
            .iter()
            .map(|(w, n, mean, t)| WalletScore {
                wallet: (*w).to_owned(),
                trades: *n,
                mean: *mean,
                t: *t,
            })
            .collect(),
    })
}
