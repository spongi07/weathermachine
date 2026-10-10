#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The strategy lab's paper strategies L1–L25 on synthetic days: each
//! family's main rule proposes the order the replay would make, its
//! blockers say why not otherwise, and its variant differs where the
//! replay's does.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Europe::Amsterdam;
use std::collections::{HashMap, HashSet};
use wm_core::event::{WalletScore, WalletScoresEvent};
use wm_core::ids::{ClientOrderId, LocationId, ProviderId, StationId, TokenId};
use wm_core::market::{DailyTemperatureMarket, OrderBook, OutcomeSide, Side, TakerTrade};
use wm_core::portfolio::{InstrumentRef, PositionBook};
use wm_core::synthetic::{synthetic_book, synthetic_temperature_market};
use wm_core::trading::{Fill, IntentKind, Liquidity, RunMode, TimeInForce};
use wm_core::units::{Price, Shares, TempC, Usd};
use wm_core::weather::{
    Observation, ObservationKey, QualityFlags, ReportType, TempPrecision, TenMinuteObservation,
};
use wm_strategy::lab::sky::clear_sky_index;
use wm_strategy::lab::solar::clear_sky_ghi;
use wm_strategy::lab::{
    LabConfig, LabInputs, LabStrategy, NeighbourReadings, WalletScores, WxReport, code,
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

fn p(s: &str) -> Price {
    Price::parse(s).unwrap()
}

fn eham() -> StationId {
    StationId::new("EHAM").unwrap()
}

fn date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
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
    forecast_ready: Option<DateTime<Utc>>,
    yesterday_error: Option<i32>,
    takers: Vec<TakerTrade>,
    wallets: Option<WalletScores>,
    position: Option<(f64, f64)>,
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
            forecast_ready: None,
            yesterday_error: None,
            takers: Vec::new(),
            wallets: None,
            position: Some((52.318, 4.790)),
        }
    }

    /// A METAR at `hm` UTC, known three minutes later.
    fn metar(&mut self, hm: &str, tenths: i32, dew: Option<i32>, raw: &str) {
        let t = z(hm);
        self.metars.push(Observation {
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
            content_hash: format!("{hm}{raw}"),
            provider: ProviderId::awc(),
            provider_receipt_at: None,
            fetched_at: t + Duration::minutes(3),
            parser_version: 1,
            quality: QualityFlags::default(),
        });
    }

    /// Plain METARs (no weather groups) at `(hh:mm, tenths)`.
    fn temps(&mut self, series: &[(&str, i32)]) {
        for (hm, t) in series {
            self.metar(hm, *t, None, "");
        }
    }

    fn book(&mut self, token: TokenId, bid: Option<&str>, ask: Option<&str>) {
        let b = synthetic_book(&token, bid, ask, 200, self.now);
        self.books.insert(token, b);
    }

    fn yes_book(&mut self, v: i32, bid: &str, ask: &str) {
        let t = self.m.outcome_for_value(v).unwrap().yes_token.clone();
        self.book(t, Some(bid), Some(ask));
    }

    fn no_book(&mut self, v: i32, bid: &str, ask: &str) {
        let t = self.m.outcome_for_value(v).unwrap().no_token.clone();
        self.book(t, Some(bid), Some(ask));
    }

    fn yes(&self, v: i32) -> TokenId {
        self.m.outcome_for_value(v).unwrap().yes_token.clone()
    }

    fn no(&self, v: i32) -> TokenId {
        self.m.outcome_for_value(v).unwrap().no_token.clone()
    }

    /// A KNMI reading ending at `hm` UTC, received four minutes later.
    fn reading(&mut self, hm: &str, mean: i32, max: i32) {
        self.knmi.push(knmi(hm, mean, max));
        self.knmi.sort_by_key(|r| r.interval_end);
    }

    fn taker(&mut self, at: &str, token: TokenId, side: Side, price: f64, size: f64, who: &str) {
        self.takers.push(TakerTrade {
            token,
            side,
            price,
            size,
            at: utc(&format!("2026-07-01T{at}Z")),
            taker: Some(who.to_owned()),
            id: format!("{at}{who}{price}"),
        });
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

    fn run(&self, family: u8, variant: bool) -> StrategyOutput {
        let cfg = LabConfig {
            variants: if variant {
                vec![code(family)]
            } else {
                Vec::new()
            },
            ..LabConfig::default()
        };
        let mut s = LabStrategy::new(family, &cfg, &PeakSlotConfig::default());
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
        let lab = LabInputs {
            knmi: &self.knmi,
            neighbours: &neighbours,
            reports: &reports,
            forecast: self.forecast.as_ref(),
            forecast_ready: self.forecast_ready,
            yesterday_error_tenths: self.yesterday_error,
            takers: &self.takers,
            wallets: self.wallets.as_ref(),
            position: self.position,
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

fn knmi(hm: &str, mean: i32, max: i32) -> TenMinuteObservation {
    TenMinuteObservation {
        station: eham(),
        provider: ProviderId::knmi(),
        interval_end: z(hm),
        mean: Some(TempC::from_tenths(mean)),
        max: Some(TempC::from_tenths(max)),
        radiation: None,
        received_at: z(hm) + Duration::minutes(4),
    }
}

/// The only proposal, or a failure listing every evaluation's blockers.
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

/// Every blocker of every evaluation, joined.
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

/// A high of 21 °C reached at 11:55 UTC (13:55 local).
fn high_21(w: &mut World) {
    w.temps(&[
        ("09:55", 195),
        ("10:25", 200),
        ("10:55", 203),
        ("11:25", 207),
        ("11:55", 210),
    ]);
}

// ------------------------------------------------------------- KNMI ---

#[test]
fn l1_rests_a_shielded_no_bid_on_the_next_degree() {
    let mut w = World::new("12:06");
    high_21(&mut w);
    // Means ≥ 0.5 °C under the edge 21.5 °C for 30 minutes, not rising.
    w.reading("11:40", 205, 207);
    w.reading("11:50", 204, 206);
    w.reading("12:00", 203, 205);
    w.no_book(22, "0.88", "0.92");
    let out = w.run(1, false);
    let pr = proposal(&out);
    assert_eq!(pr.strategy.as_str(), "L1_shielded_maker");
    assert_eq!((pr.token.clone(), pr.side), (w.no(22), Side::Buy));
    assert_eq!(pr.limit_price, p("0.89"), "one tick inside the NO spread");
    assert_eq!(pr.shares, Shares::from_whole(22));
    // Until the next reading (received 12:04 + 10 min), before 12:25 + 3.
    assert_eq!(
        pr.tif,
        TimeInForce::Gtd {
            expires_at: z("12:14")
        }
    );
    assert_eq!(pr.kind, IntentKind::Open);
    // A rising mean lifts the shield; the control quotes anyway.
    let mut rising = World::new("12:06");
    high_21(&mut rising);
    rising.reading("11:40", 200, 202);
    rising.reading("11:50", 202, 203);
    rising.reading("12:00", 204, 205);
    rising.no_book(22, "0.88", "0.92");
    assert_blocked(&rising.run(1, false), "KNMI mean rising");
    assert_eq!(proposal(&rising.run(1, true)).limit_price, p("0.89"));
    // Too close to the edge.
    let mut close = World::new("12:06");
    high_21(&mut close);
    close.reading("11:40", 212, 213);
    close.reading("11:50", 211, 212);
    close.reading("12:00", 211, 212);
    close.no_book(22, "0.88", "0.92");
    assert_blocked(&close.run(1, false), "not ≥ 0.5 °C under the edge");
    // Outside 10:00–20:00 local.
    let mut early = World::new("07:06");
    early.temps(&[("06:55", 150)]);
    early.reading("06:40", 140, 141);
    early.reading("06:50", 139, 140);
    early.reading("07:00", 138, 139);
    early.no_book(16, "0.88", "0.92");
    assert_blocked(&early.run(1, false), "outside 10:00–20:00");
}

#[test]
fn l2_quotes_the_doomed_bucket_once_knmi_is_above_the_edge() {
    let mut w = World::new("12:15");
    high_21(&mut w);
    w.reading("12:10", 219, 220);
    w.no_book(21, "0.30", "0.34");
    let pr = proposal(&w.run(2, false)).clone();
    assert_eq!(pr.token, w.no(21));
    assert_eq!(pr.limit_price, p("0.31"));
    assert_eq!(
        pr.tif,
        TimeInForce::Gtd {
            expires_at: z("12:28")
        },
        "until the 12:25 report is known"
    );
    // The variant needs 0.6 °C above the edge.
    assert_blocked(&w.run(2, true), "KNMI mean 21.9 °C < 22.1 °C");
    // A reading older than the METAR says nothing about the next one.
    let mut old = World::new("12:15");
    high_21(&mut old);
    old.reading("11:50", 219, 220);
    old.no_book(21, "0.30", "0.34");
    assert_blocked(&old.run(2, false), "latest KNMI reading 25 min old");
}

#[test]
fn l3_enters_as_f_and_sells_before_the_new_high() {
    // Holding F's 100 YES of 21 °C: KNMI 0.3 °C above the bucket's edge
    // before the 12:25 METAR → sell at the bid.
    let mut w = World::new("12:15");
    high_21(&mut w);
    w.reading("12:10", 218, 219);
    w.yes_book(21, "0.90", "0.93");
    let yes21 = w.yes(21);
    let o = w.m.outcome_for_value(21).unwrap().clone();
    w.positions
        .apply_fill(
            &Fill {
                client_order_id: ClientOrderId::from_static_string("f1".into()),
                token: yes21.clone(),
                side: Side::Buy,
                price: p("0.93"),
                shares: Shares::from_whole(100),
                fee: Usd::ZERO,
                liquidity: Liquidity::Taker,
                ts: z("11:58"),
            },
            &InstrumentRef {
                token: yes21.clone(),
                condition_id: o.condition_id.clone(),
                event_slug: w.m.event_slug.clone(),
                outcome_side: OutcomeSide::Yes,
                bucket: o.bucket,
            },
        )
        .unwrap();
    let out = w.run(3, false);
    let pr = proposal(&out);
    assert_eq!(pr.strategy.as_str(), "L3_f_escape");
    assert_eq!((pr.side, pr.kind), (Side::Sell, IntentKind::Reduce));
    assert_eq!(
        (pr.limit_price, pr.shares),
        (p("0.90"), Shares::from_whole(100))
    );
    assert_eq!(pr.tif, TimeInForce::Fak);
    // Under the margin: hold. The variant exits at the edge itself.
    let mut hold = World::new("12:15");
    high_21(&mut hold);
    hold.reading("12:10", 216, 217);
    hold.yes_book(21, "0.90", "0.93");
    hold.positions = w.positions.clone();
    assert_blocked(&hold.run(3, false), "hold");
    assert_eq!(proposal(&hold.run(3, true)).side, Side::Sell);
    // Sold: F's rule does not buy again the same day.
    let mut sold = World::new("12:20");
    high_21(&mut sold);
    sold.yes_book(21, "0.90", "0.93");
    sold.positions = w.positions.clone();
    sold.positions
        .apply_fill(
            &Fill {
                client_order_id: ClientOrderId::from_static_string("l3".into()),
                token: yes21.clone(),
                side: Side::Sell,
                price: p("0.90"),
                shares: Shares::from_whole(100),
                fee: Usd::ZERO,
                liquidity: Liquidity::Taker,
                ts: z("12:16"),
            },
            &InstrumentRef {
                token: yes21,
                condition_id: o.condition_id,
                event_slug: sold.m.event_slug.clone(),
                outcome_side: OutcomeSide::Yes,
                bucket: o.bucket,
            },
        )
        .unwrap();
    let again = sold.run(3, false);
    assert!(again.proposals.is_empty());
    assert!(!again.evaluations.is_empty(), "F's evaluation is shown");
    assert!(
        again
            .evaluations
            .iter()
            .all(|e| e.strategy.as_str() == "L3_f_escape"
                && e.blockers.iter().any(|b| b.contains("exited today")))
    );
}

#[test]
fn l4_buys_the_high_bucket_when_knmi_shows_the_cooling() {
    let mut w = World::new("14:06");
    w.temps(&[
        ("10:55", 200),
        ("11:25", 205),
        ("11:55", 208),
        ("12:25", 210),
        ("12:55", 200),
        ("13:25", 195),
        ("13:55", 190),
    ]);
    // Nine readings in 90 minutes, all maxima ≤ 21.2 °C, the mean down
    // 1.0 °C in the last hour.
    for (i, hm) in [
        "12:40", "12:50", "13:00", "13:10", "13:20", "13:30", "13:40", "13:50", "14:00",
    ]
    .iter()
    .enumerate()
    {
        let mean = 205 - 2 * i32::try_from(i).unwrap();
        w.reading(hm, mean, mean + 2);
    }
    w.yes_book(21, "0.80", "0.82");
    let out = w.run(4, false);
    let pr = proposal(&out);
    assert_eq!((pr.token.clone(), pr.side), (w.yes(21), Side::Buy));
    assert_eq!(pr.limit_price, p("0.82"));
    assert_eq!(pr.shares, Shares::from_whole(24));
    assert_eq!(pr.tif, TimeInForce::Fak);
    assert!((pr.p_win - 0.97).abs() < 1e-9);
    // F's band (0.90–0.98) is the variant.
    assert_blocked(&w.run(4, true), "YES ask 0.82 outside [0.90, 0.98]");
    // One warm maximum in the window keeps it out.
    w.knmi[3].max = Some(TempC::from_tenths(214));
    assert_blocked(&w.run(4, false), "KNMI maximum 21.4 °C");
}

#[test]
fn l5_sells_the_next_degree_late_under_knmi_cooling() {
    let mut w = World::new("14:36");
    w.temps(&[("11:55", 210), ("13:55", 200), ("14:25", 198)]);
    for (i, hm) in [
        "13:30", "13:40", "13:50", "14:00", "14:10", "14:20", "14:30",
    ]
    .iter()
    .enumerate()
    {
        let mean = 204 - i32::try_from(i).unwrap();
        w.reading(hm, mean, mean + 3);
    }
    w.no_book(22, "0.88", "0.90");
    let pr = proposal(&w.run(5, false)).clone();
    assert_eq!((pr.token, pr.limit_price), (w.no(22), p("0.90")));
    // From 17:00 local in the variant.
    assert_blocked(&w.run(5, true), "outside 17:00–21:00");
}

#[test]
fn l6_buys_the_new_degree_late_when_the_rise_flattens() {
    let mut w = World::new("13:15");
    w.temps(&[("11:55", 205), ("12:25", 208), ("12:55", 210)]);
    w.reading("12:40", 222, 223);
    w.reading("12:50", 223, 224);
    w.reading("13:00", 223, 224);
    w.reading("13:10", 224, 225);
    w.yes_book(22, "0.50", "0.55");
    assert_eq!(proposal(&w.run(6, false)).token, w.yes(22));
    // Still rising fast: the filter holds it, the control does not.
    w.knmi[0].mean = Some(TempC::from_tenths(215));
    assert_blocked(&w.run(6, false), "KNMI still rising");
    assert_eq!(proposal(&w.run(6, true)).token, w.yes(22));
}

#[test]
fn l7_takes_k_one_reading_early_on_the_slope() {
    let mut w = World::new("12:15");
    high_21(&mut w);
    w.reading("11:50", 201, 202);
    w.reading("12:00", 207, 208);
    w.reading("12:10", 213, 214);
    w.no_book(21, "0.36", "0.40");
    let pr = proposal(&w.run(7, false)).clone();
    assert_eq!((pr.token, pr.limit_price), (w.no(21), p("0.40")));
    // To the edge + 1.0 °C (the variant) the slope falls short.
    assert_blocked(&w.run(7, true), "projected 22.2 °C at the report < 22.5 °C");
}

// ------------------------------------------------------------ METAR ---

#[test]
fn l8_sells_the_next_degree_when_the_sea_breeze_arrives() {
    let mut w = World::new("12:30");
    w.metar(
        "06:25",
        160,
        Some(120),
        "EHAM 010625Z 12008KT 9999 FEW030 16/12 Q1018 NOSIG",
    );
    w.metar(
        "10:55",
        215,
        Some(118),
        "EHAM 011055Z 14010KT 9999 FEW040 22/12 Q1017 NOSIG",
    );
    w.metar(
        "11:25",
        220,
        Some(120),
        "EHAM 011125Z 16008KT 9999 FEW040 22/12 Q1017 NOSIG",
    );
    w.metar(
        "12:25",
        210,
        Some(135),
        "EHAM 011225Z 30010KT 9999 FEW020 21/14 Q1018 NOSIG",
    );
    w.no_book(23, "0.78", "0.80");
    let pr = proposal(&w.run(8, false)).clone();
    assert_eq!((pr.token, pr.limit_price), (w.no(23), p("0.80")));
    // The variant buys YES of the high's bucket instead.
    w.yes_book(22, "0.70", "0.72");
    assert_eq!(proposal(&w.run(8, true)).token, w.yes(22));
    // Without the moister air it is not the sea breeze.
    let mut dry = World::new("12:30");
    dry.metar(
        "06:25",
        160,
        Some(120),
        "EHAM 010625Z 12008KT 9999 FEW030 16/12 Q1018 NOSIG",
    );
    dry.metar(
        "11:25",
        220,
        Some(120),
        "EHAM 011125Z 16008KT 9999 FEW040 22/12 Q1017 NOSIG",
    );
    dry.metar(
        "12:25",
        210,
        Some(125),
        "EHAM 011225Z 30010KT 9999 FEW020 21/12 Q1018 NOSIG",
    );
    dry.no_book(23, "0.78", "0.80");
    assert_blocked(&dry.run(8, false), "dew point 12.5 °C not ≥ 1 °C above");
}

#[test]
fn l9_caps_the_day_after_a_shower() {
    let mut w = World::new("13:00");
    w.metar(
        "11:55",
        220,
        Some(150),
        "EHAM 011155Z 21010KT 9999 SCT030 22/15 Q1012 NOSIG",
    );
    w.metar(
        "12:55",
        195,
        Some(170),
        "EHAM 011255Z 25015G25KT 4000 SHRA BKN015CB 20/17 Q1013 NOSIG",
    );
    w.no_book(23, "0.85", "0.88");
    assert_eq!(proposal(&w.run(9, false)).token, w.no(23));
    // The variant waits for thunder; a cumulonimbus counts.
    assert_eq!(proposal(&w.run(9, true)).token, w.no(23));
    let mut rain = World::new("13:00");
    rain.metar(
        "11:55",
        220,
        Some(150),
        "EHAM 011155Z 21010KT 9999 SCT030 22/15 Q1012 NOSIG",
    );
    rain.metar(
        "12:55",
        195,
        Some(170),
        "EHAM 011255Z 25012KT 6000 -RA BKN015 20/17 Q1013 NOSIG",
    );
    rain.no_book(23, "0.85", "0.88");
    assert_eq!(proposal(&rain.run(9, false)).token, rain.no(23));
    assert_blocked(&rain.run(9, true), "no thunder");
}

/// A book the lab's risk check would refuse (wider than its 0.10 limit, or
/// one-sided) is a blocker of the rule itself, not a proposal rejected on
/// every update: for a taker (L9) and for a maker (L1) alike.
#[test]
fn lab_rules_wait_out_a_book_their_risk_check_refuses() {
    let shower = |w: &mut World| {
        w.metar(
            "11:55",
            220,
            Some(150),
            "EHAM 011155Z 21010KT 9999 SCT030 22/15 Q1012 NOSIG",
        );
        w.metar(
            "12:55",
            195,
            Some(170),
            "EHAM 011255Z 25015G25KT 4000 SHRA BKN015CB 20/17 Q1013 NOSIG",
        );
    };
    let mut wide = World::new("13:00");
    shower(&mut wide);
    wide.no_book(23, "0.75", "0.88");
    let out = wide.run(9, false);
    assert!(out.proposals.is_empty());
    assert_blocked(&out, "NO spread 0.13 > 0.10 (the lab book's limit)");
    let mut one_sided = World::new("13:00");
    shower(&mut one_sided);
    let t = one_sided.no(23);
    one_sided.book(t, None, Some("0.88"));
    assert_blocked(&one_sided.run(9, false), "NO book one-sided");
    // L1 rests one tick inside the spread: still refused in a wide book.
    let mut maker = World::new("12:06");
    high_21(&mut maker);
    maker.reading("11:40", 205, 207);
    maker.reading("11:50", 204, 206);
    maker.reading("12:00", 203, 205);
    maker.no_book(22, "0.70", "0.92");
    let out = maker.run(1, false);
    assert!(out.proposals.is_empty());
    assert_blocked(&out, "NO spread 0.22 > 0.10 (the lab book's limit)");
}

#[test]
fn l10_reads_the_trend() {
    let mut w = World::new("11:00");
    w.metar(
        "10:25",
        200,
        Some(140),
        "EHAM 011025Z 21010KT 9999 FEW030 20/14 Q1012 TEMPO SHRA",
    );
    w.metar(
        "10:55",
        205,
        Some(140),
        "EHAM 011055Z 21010KT 9999 FEW030 21/14 Q1012 TEMPO SHRA",
    );
    w.no_book(22, "0.70", "0.75");
    assert_eq!(proposal(&w.run(10, false)).token, w.no(22));
    // NOSIG: no cap.
    let mut calm = World::new("11:00");
    calm.metar(
        "10:55",
        205,
        Some(140),
        "EHAM 011055Z 21010KT CAVOK 21/14 Q1012 NOSIG",
    );
    calm.no_book(22, "0.70", "0.75");
    assert_blocked(&calm.run(10, false), "the TREND announces no");
    // The NOSIG lock: after the median peak time, clear, 1 °C under the high.
    let mut lock = World::new("13:30");
    lock.metar(
        "11:55",
        210,
        Some(120),
        "EHAM 011155Z 24008KT CAVOK 21/12 Q1015 NOSIG",
    );
    lock.metar(
        "13:25",
        195,
        Some(120),
        "EHAM 011325Z 24008KT CAVOK 20/12 Q1015 NOSIG",
    );
    lock.yes_book(21, "0.80", "0.84");
    assert_eq!(proposal(&lock.run(10, true)).token, lock.yes(21));
}

#[test]
fn l11_fades_the_favourite_under_fog() {
    let mut w = World::new("07:00");
    w.metar(
        "06:25",
        115,
        Some(115),
        "EHAM 010625Z 22003KT 1500 BR OVC003 12/12 Q1015 BECMG 5000",
    );
    w.metar(
        "06:55",
        120,
        Some(120),
        "EHAM 010655Z 22003KT 2000 BR OVC003 12/12 Q1015 BECMG 5000",
    );
    w.yes_book(19, "0.40", "0.44");
    w.yes_book(18, "0.20", "0.24");
    w.no_book(19, "0.56", "0.60");
    let pr = proposal(&w.run(11, false)).clone();
    assert_eq!((pr.token, pr.limit_price), (w.no(19), p("0.60")));
    // A favourite only 4–6 °C above: the variant (4 °C) still trades.
    let mut near = World::new("07:00");
    near.metar(
        "06:55",
        130,
        Some(130),
        "EHAM 010655Z 22003KT 2000 BR OVC003 13/13 Q1015 BECMG 5000",
    );
    near.yes_book(18, "0.40", "0.44");
    near.no_book(18, "0.56", "0.60");
    assert_blocked(&near.run(11, false), "only 5.0 °C above");
    assert_eq!(proposal(&near.run(11, true)).token, near.no(18));
}

#[test]
fn l12_buys_above_the_favourite_on_a_clear_dry_morning() {
    let mut w = World::new("09:00");
    w.metar(
        "08:55",
        220,
        Some(100),
        "EHAM 010855Z 12008KT CAVOK 22/10 Q1022 NOSIG",
    );
    w.yes_book(23, "0.38", "0.42");
    w.yes_book(24, "0.12", "0.15");
    assert_eq!(proposal(&w.run(12, false)).token, w.yes(24));
    // A sea wind: only the variant (any wind) trades.
    let mut sea = World::new("09:00");
    sea.metar(
        "08:55",
        220,
        Some(100),
        "EHAM 010855Z 30008KT CAVOK 22/10 Q1022 NOSIG",
    );
    sea.yes_book(23, "0.38", "0.42");
    sea.yes_book(24, "0.12", "0.15");
    assert_blocked(&sea.run(12, false), "not continental");
    assert_eq!(proposal(&sea.run(12, true)).token, sea.yes(24));
}

#[test]
fn l13_locks_an_early_high_after_a_cold_front() {
    let mut w = World::new("10:30");
    w.metar(
        "06:25",
        200,
        Some(150),
        "EHAM 010625Z 18010KT 9999 BKN020 20/15 Q1008 NOSIG",
    );
    w.metar(
        "08:25",
        190,
        Some(120),
        "EHAM 010825Z 25012KT 9999 FEW020 19/12 Q1010 NOSIG",
    );
    w.metar(
        "10:25",
        180,
        Some(100),
        "EHAM 011025Z 28012KT 9999 FEW030 18/10 Q1012 NOSIG",
    );
    w.yes_book(20, "0.66", "0.70");
    assert_eq!(proposal(&w.run(13, false)).token, w.yes(20));
    // The variant sells the next degree.
    w.no_book(21, "0.80", "0.83");
    assert_eq!(proposal(&w.run(13, true)).token, w.no(21));
    // No pressure rise: no front.
    let mut flat = World::new("10:30");
    flat.metar(
        "06:25",
        200,
        Some(150),
        "EHAM 010625Z 18010KT 9999 BKN020 20/15 Q1012 NOSIG",
    );
    flat.metar(
        "10:25",
        180,
        Some(100),
        "EHAM 011025Z 28012KT 9999 FEW030 18/10 Q1012 NOSIG",
    );
    flat.yes_book(20, "0.66", "0.70");
    assert_blocked(&flat.run(13, false), "QNH not up");
}

// --------------------------------------------------------- forecast ---

/// The local day's hourly forecast (from 00:00 local = 22:00 UTC the day
/// before), `f(utc hour index from 22:00)`.
fn forecast(known: &str, f: impl Fn(i64) -> i32) -> ForecastDay {
    let start = utc("2026-06-30T22:00:00Z");
    let series: Vec<(DateTime<Utc>, i32)> = (0..=24)
        .map(|h| (start + Duration::hours(h), f(h)))
        .collect();
    ForecastDay::from_series(date(), Amsterdam, &series, z(known)).unwrap()
}

#[test]
fn l14_buys_the_bucket_the_morning_departure_points_to() {
    let mut w = World::new("08:30");
    w.temps(&[("07:55", 170), ("08:25", 175)]);
    // The forecast: 15.4 °C at 08:25 UTC, rising to a 20.0 °C peak at
    // 13:00 UTC. Observed 17.5 °C, 2.1 °C above it:
    // 20.0 + 0.7 × 2.1 = 21.5 → 21 °C (λ = 1.0: 22.1 → 22 °C).
    w.forecast = Some(forecast("04:00", |h| {
        let utc_hour = (22 + h) % 24;
        200 - 10 * (utc_hour - 13).abs() as i32
    }));
    w.yes_book(20, "0.43", "0.47");
    w.yes_book(21, "0.22", "0.25");
    w.yes_book(22, "0.08", "0.10");
    let out = w.run(14, false);
    let pr = proposal(&out);
    assert_eq!((pr.token.clone(), pr.side), (w.yes(21), Side::Buy));
    assert!(pr.rationale.iter().any(|r| r.contains("remaining maximum")));
    assert_eq!(proposal(&w.run(14, true)).token, w.yes(22));
    // The market's own favourite is no departure.
    w.yes_book(21, "0.50", "0.52");
    assert_blocked(&w.run(14, false), "already the market's favourite");
    // Without a forecast the rule says so.
    let mut none = World::new("08:30");
    none.temps(&[("08:25", 190)]);
    assert_blocked(&none.run(14, false), "no day-1 forecast");
    // Before today's ready time it says from when: yesterday's fetch does
    // not count for today.
    none.forecast_ready = Some(z("09:00"));
    assert_blocked(
        &none.run(14, false),
        "the forecast is usable from 09:00 UTC",
    );
    none.forecast_ready = Some(z("08:00"));
    assert_blocked(&none.run(14, false), "no day-1 forecast");
}

#[test]
fn l15_buys_the_high_well_past_the_forecast_peak() {
    let mut w = World::new("13:30");
    w.temps(&[
        ("10:55", 205),
        ("11:25", 210),
        ("12:55", 200),
        ("13:25", 195),
    ]);
    // The forecast peaks at 11:00 UTC (13:00 local) and falls after.
    w.forecast = Some(forecast("04:00", |h| {
        let utc_hour = (22 + h) % 24;
        200 - 10 * (utc_hour - 11).abs() as i32
    }));
    w.yes_book(21, "0.82", "0.85");
    assert_eq!(proposal(&w.run(15, false)).token, w.yes(21));
    // Thirty minutes earlier only the variant (+60′) may trade.
    w.now = z("12:58");
    w.yes_book(21, "0.82", "0.85");
    assert_blocked(&w.run(15, false), "before 15:00");
    assert_eq!(proposal(&w.run(15, true)).token, w.yes(21));
}

/// The service refetches the forecast every hour: a refresh after the
/// latest report (or a restart) must not hide the forecast from the rules,
/// which read it at that report.
#[test]
fn a_forecast_refetched_after_the_latest_report_is_still_read_there() {
    let mut w = World::new("13:30");
    w.temps(&[
        ("10:55", 205),
        ("11:25", 210),
        ("12:55", 200),
        ("13:25", 195),
    ]);
    // As in L15's test, but known at 13:28, after the 13:25 report.
    let peak_at_11 = |h: i64| {
        let utc_hour = (22 + h) % 24;
        200 - 10 * (utc_hour - 11).abs() as i32
    };
    w.forecast = Some(forecast("13:28", peak_at_11));
    w.yes_book(21, "0.82", "0.85");
    assert_eq!(proposal(&w.run(15, false)).token, w.yes(21));
    // L16 reads the rise too, and L14 says why it waits instead of nothing.
    let l16 = w.run(16, false);
    assert_blocked(&l16, "no evening high");
    assert_blocked(&l16, "forecast rise -3.0 °C < 1.0 °C");
    assert_blocked(&w.run(14, false), "outside 10:00–12:30");
    // Not before the forecast is known at all.
    w.forecast = Some(forecast("13:31", peak_at_11));
    assert_blocked(&w.run(15, false), "the forecast is usable from 13:31 UTC");
}

#[test]
fn l16_buys_the_next_degree_on_an_evening_high_day() {
    let mut w = World::new("12:30");
    w.temps(&[("11:55", 195), ("12:25", 200)]);
    // Rising all day to 21.0 °C at 19:00 UTC (21:00 local).
    w.forecast = Some(forecast("04:00", |h| {
        let utc_hour = (22 + h) % 24;
        if (5..=19).contains(&utc_hour) {
            140 + 5 * (utc_hour - 5) as i32
        } else {
            140
        }
    }));
    w.yes_book(21, "0.08", "0.10");
    assert_eq!(proposal(&w.run(16, false)).token, w.yes(21));
    // A day that peaks in the afternoon is no evening-high day.
    w.forecast = Some(forecast("04:00", |h| {
        let utc_hour = (22 + h) % 24;
        200 - 10 * (utc_hour - 13).abs() as i32
    }));
    assert_blocked(&w.run(16, false), "no evening high");
}

#[test]
fn l17_carries_yesterdays_error_to_the_ladder() {
    let mut w = World::new("06:30");
    w.temps(&[("05:55", 140), ("06:25", 145)]);
    w.forecast = Some(forecast("04:00", |h| {
        let utc_hour = (22 + h) % 24;
        200 - 10 * (utc_hour - 13).abs() as i32
    }));
    // Yesterday came in 2.0 °C above its forecast: 20.0 + 1.0 → 21 °C.
    w.yesterday_error = Some(20);
    w.yes_book(21, "0.17", "0.20");
    assert_eq!(proposal(&w.run(17, false)).token, w.yes(21));
    // Half the error of 0.6 °C stays in the raw forecast's bucket.
    w.yesterday_error = Some(6);
    assert_blocked(&w.run(17, false), "the raw forecast's bucket too");
    // Without yesterday's error the rule waits.
    w.yesterday_error = None;
    assert_blocked(&w.run(17, false), "yesterday's forecast error unknown");
}

// ------------------------------------------------------------- flow ---

#[test]
fn l18_follows_a_burst_before_the_report_when_knmi_agrees() {
    let mut w = World::new("12:21");
    high_21(&mut w);
    w.reading("12:10", 219, 220);
    let (y21, y22) = (w.yes(21), w.yes(22));
    w.taker("12:20:00", y21, Side::Sell, 0.70, 30.0, "a");
    w.taker("12:20:30", y22, Side::Buy, 0.25, 15.0, "b");
    w.no_book(21, "0.35", "0.40");
    assert_eq!(proposal(&w.run(18, false)).token, w.no(21));
    // One taker is not a burst.
    let mut one = World::new("12:21");
    high_21(&mut one);
    one.reading("12:10", 219, 220);
    let y21 = one.yes(21);
    one.taker("12:20:00", y21.clone(), Side::Sell, 0.70, 30.0, "a");
    one.taker("12:20:30", y21, Side::Sell, 0.69, 15.0, "a");
    one.no_book(21, "0.35", "0.40");
    assert_blocked(&one.run(18, false), "no burst");
    // KNMI under the edge + 0.3: only the control follows.
    w.knmi[0].mean = Some(TempC::from_tenths(214));
    assert_blocked(&w.run(18, false), "KNMI mean 21.4 °C < 21.8 °C");
    assert_eq!(proposal(&w.run(18, true)).token, w.no(21));
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

#[test]
fn l19_follows_skilled_takers_thirty_seconds_later() {
    let mut w = World::new("12:01");
    high_21(&mut w);
    w.wallets = Some(scores(&[("pro", 40, 0.05, 3.5), ("ok", 40, 0.03, 2.5)]));
    let y22 = w.yes(22);
    w.taker("12:00:00", y22, Side::Buy, 0.30, 50.0, "pro");
    w.yes_book(22, "0.30", "0.32");
    let pr = proposal(&w.run(19, false)).clone();
    assert_eq!((pr.token, pr.limit_price), (w.yes(22), p("0.32")));
    // Too dear: more than their price + 0.03.
    w.yes_book(22, "0.33", "0.35");
    assert_blocked(&w.run(19, false), "YES ask 0.35 outside [0.01, 0.33]");
    // t 2.5 follows at t ≥ 2, not at t ≥ 3 (the variant).
    let mut ok = World::new("12:01");
    high_21(&mut ok);
    ok.wallets = Some(scores(&[("ok", 40, 0.03, 2.5)]));
    let y22 = ok.yes(22);
    ok.taker("12:00:00", y22, Side::Buy, 0.30, 50.0, "ok");
    ok.yes_book(22, "0.30", "0.32");
    assert_eq!(proposal(&ok.run(19, false)).token, ok.yes(22));
    assert_blocked(&ok.run(19, true), "no trade by one of the 0 skilled takers");
    // Without records nothing is followed.
    ok.wallets = None;
    assert_blocked(&ok.run(19, false), "no taker records yet");
}

#[test]
fn l20_rests_a_no_bid_at_the_losing_takers_price() {
    let mut w = World::new("12:01");
    high_21(&mut w);
    w.wallets = Some(scores(&[("loser", 50, -0.08, -3.0)]));
    let y24 = w.yes(24);
    w.taker("12:00:00", y24, Side::Buy, 0.05, 100.0, "loser");
    w.no_book(24, "0.94", "0.97");
    let pr = proposal(&w.run(20, false)).clone();
    assert_eq!(pr.token, w.no(24));
    assert_eq!(
        pr.limit_price,
        p("0.96"),
        "YES offered one tick under their 0.05"
    );
    assert_eq!(
        pr.tif,
        TimeInForce::Gtd {
            expires_at: z("12:30")
        }
    );
    // The variant takes NO at 0.80–0.98.
    assert_eq!(proposal(&w.run(20, true)).limit_price, p("0.97"));
    // A taker without a losing record is no signal.
    w.wallets = Some(scores(&[("loser", 50, -0.01, -0.5)]));
    assert_blocked(&w.run(20, false), "no longshot buy");
}

#[test]
fn l21_fades_the_jump_two_degrees_above_a_new_high() {
    let mut w = World::new("12:05");
    w.temps(&[("11:25", 200), ("11:55", 210)]);
    w.reading("12:00", 214, 215);
    let y23 = w.yes(23);
    w.taker("11:59:00", y23, Side::Buy, 0.10, 40.0, "x");
    w.no_book(23, "0.82", "0.85");
    assert_eq!(proposal(&w.run(21, false)).token, w.no(23));
    // KNMI still climbing: the jump may be right.
    w.knmi[0].mean = Some(TempC::from_tenths(220));
    assert_blocked(&w.run(21, false), "the rise goes on");
    // Twenty minutes after the report: too late.
    let mut late = World::new("12:20");
    late.temps(&[("11:25", 200), ("11:55", 210)]);
    late.reading("12:10", 214, 215);
    let y23 = late.yes(23);
    late.taker("11:59:00", y23, Side::Buy, 0.10, 40.0, "x");
    late.no_book(23, "0.82", "0.85");
    assert_blocked(&late.run(21, false), "outside 2–15 min");
}

#[test]
fn l22_quotes_the_far_tails_overnight() {
    let mut w = World::new("04:10");
    w.temps(&[("03:55", 140)]);
    w.yes_book(20, "0.38", "0.42");
    w.no_book(23, "0.96", "0.99");
    let out = w.run(22, false);
    let pr = proposal(&out).clone();
    assert_eq!(pr.token, w.no(23));
    assert_eq!(pr.limit_price, p("0.97"), "YES offered at 0.03");
    assert_eq!(
        pr.tif,
        TimeInForce::Gtd {
            expires_at: z("04:15")
        },
        "withdrawn 10 min before 04:25"
    );
    // 14–17 °C and 23–24 °C are quoted where their books allow.
    assert_eq!(out.evaluations.len(), 6);
    // 23 °C is three places from the favourite 20 °C; the variant needs four.
    assert!(w.run(22, true).proposals.is_empty());
    // Dead buckets (below the high) are not quoted; buckets next to the
    // favourite are not either.
    assert!(
        out.evaluations
            .iter()
            .all(|e| e.bucket_label != "13°C or below"
                && e.bucket_label != "21°C"
                && e.bucket_label != "22°C")
    );
    // After 09:00 local: no quotes.
    w.now = z("07:10");
    w.no_book(23, "0.96", "0.99");
    w.yes_book(20, "0.38", "0.42");
    assert_blocked(&w.run(22, false), "after 09:00");
}

// ---------------------------------------------- weather during the day ---

#[test]
fn l23_buys_the_recovery_after_a_shower() {
    let mut w = World::new("12:30");
    w.metar(
        "10:55",
        205,
        Some(160),
        "EHAM 011055Z 23010KT 6000 -SHRA BKN015 21/16 Q1012 NOSIG",
    );
    w.metar(
        "11:25",
        220,
        Some(150),
        "EHAM 011125Z 24010KT 9999 SCT025 22/15 Q1012 NOSIG",
    );
    w.metar(
        "11:55",
        200,
        Some(160),
        "EHAM 011155Z 24012KT 8000 -SHRA BKN020 20/16 Q1012 NOSIG",
    );
    w.metar(
        "12:25",
        210,
        Some(150),
        "EHAM 011225Z 24008KT 9999 FEW045 21/15 Q1012 NOSIG",
    );
    w.reading("11:50", 205, 207);
    w.reading("12:20", 212, 214);
    w.yes_book(23, "0.17", "0.20");
    assert_eq!(proposal(&w.run(23, false)).token, w.yes(23));
    // Still raining: no recovery.
    let mut wet = World::new("12:30");
    wet.metar(
        "11:25",
        220,
        Some(150),
        "EHAM 011125Z 24010KT 9999 SCT025 22/15 Q1012 NOSIG",
    );
    wet.metar(
        "12:25",
        210,
        Some(150),
        "EHAM 011225Z 24008KT 9999 -RA FEW045 21/15 Q1012 NOSIG",
    );
    wet.reading("11:50", 205, 207);
    wet.reading("12:20", 212, 214);
    wet.yes_book(23, "0.17", "0.20");
    assert_blocked(&wet.run(23, false), "still raining");
}

#[test]
fn l24_reads_the_upwind_station() {
    let mut w = World::new("12:15");
    w.metar(
        "11:25",
        207,
        Some(140),
        "EHAM 011125Z 23010KT 9999 FEW030 21/14 Q1012 NOSIG",
    );
    w.metar(
        "11:55",
        210,
        Some(140),
        "EHAM 011155Z 23010KT 9999 FEW030 21/14 Q1012 NOSIG",
    );
    w.reading("12:10", 212, 213);
    w.neighbours
        .push(("Voorschoten".into(), 231.0, vec![knmi("12:10", 222, 224)]));
    w.neighbours
        .push(("De Bilt".into(), 132.0, vec![knmi("12:10", 190, 191)]));
    w.no_book(21, "0.26", "0.30");
    let pr = proposal(&w.run(24, false)).clone();
    assert_eq!(pr.token, w.no(21));
    assert!(pr.rationale.iter().any(|r| r.contains("Voorschoten")));
    // The wind from De Bilt (cooler) is the cool side's signal.
    let mut cool = World::new("12:15");
    cool.metar(
        "11:55",
        200,
        Some(140),
        "EHAM 011155Z 13010KT 9999 FEW030 20/14 Q1012 NOSIG",
    );
    cool.temps(&[("10:55", 210)]);
    cool.metars.sort_by_key(|o| o.key.observed_at);
    cool.reading("12:10", 200, 201);
    cool.neighbours
        .push(("De Bilt".into(), 132.0, vec![knmi("12:10", 185, 186)]));
    cool.no_book(22, "0.86", "0.88");
    assert_eq!(proposal(&cool.run(24, true)).token, cool.no(22));
    assert_blocked(&cool.run(24, false), "needs ≥ 0.8 °C warmer");
}

#[test]
fn l25_sells_the_next_degree_when_the_sun_goes_in() {
    let mut w = World::new("11:06");
    w.temps(&[("09:55", 195), ("10:25", 205), ("10:55", 210)]);
    let (lat, lon) = (52.318, 4.790);
    let ends = [
        "09:40", "09:50", "10:00", "10:10", "10:20", "10:30", "10:40", "10:50", "11:00",
    ];
    for (i, hm) in ends.iter().enumerate() {
        let index = if i < 6 { 0.85 } else { 0.25 };
        let cs = clear_sky_ghi(z(hm) - Duration::minutes(5), lat, lon);
        let mut r = knmi(hm, 200, 202);
        #[allow(clippy::cast_possible_truncation)]
        {
            r.radiation = Some((index * cs).round() as i32);
        }
        w.knmi.push(r);
    }
    let idx = clear_sky_index(&w.knmi, lat, lon);
    assert_eq!(idx.len(), 9);
    assert!((idx[8].1 - 0.25).abs() < 0.01);
    w.no_book(22, "0.78", "0.80");
    assert_eq!(proposal(&w.run(25, false)).token, w.no(22));
    // The clearing (the variant) wants the opposite.
    assert_blocked(&w.run(25, true), "needs ≥ 80 % after ≤ 40 %");
    // Without the station's position there is no clear sky to compare.
    w.position = None;
    assert_blocked(&w.run(25, false), "position is unknown");
}

// ----------------------------------------------------------- common ---

#[test]
fn a_taker_buys_no_more_than_the_ask_offers_and_trades_once_a_day() {
    // L9's signal with only 10 NO shares at the ask: 10 shares, not 22.
    let mut w = World::new("13:00");
    w.metar(
        "11:55",
        220,
        Some(150),
        "EHAM 011155Z 21010KT 9999 SCT030 22/15 Q1012 NOSIG",
    );
    w.metar(
        "12:55",
        195,
        Some(170),
        "EHAM 011255Z 25015G25KT 4000 SHRA BKN015CB 20/17 Q1013 NOSIG",
    );
    let t = w.no(23);
    let mut b = synthetic_book(&t, Some("0.85"), Some("0.88"), 10, w.now);
    b.min_order_size = Shares::from_whole(5);
    w.books.insert(t.clone(), b);
    assert_eq!(proposal(&w.run(9, false)).shares, Shares::from_whole(10));
    // Three shares are below the market's minimum of five.
    let b = synthetic_book(&t, Some("0.85"), Some("0.88"), 3, w.now);
    w.books.insert(t.clone(), b);
    assert_blocked(&w.run(9, false), "size below the market minimum");
    // A live order on any of today's tokens: one trade a day.
    w.pending.insert(w.yes(15));
    assert_blocked(&w.run(9, false), "already traded today");
}

#[test]
fn a_stale_reading_or_book_blocks_and_says_so() {
    let mut w = World::new("12:40");
    high_21(&mut w);
    w.reading("12:10", 219, 220);
    w.no_book(21, "0.30", "0.34");
    assert_blocked(&w.run(2, false), "latest KNMI reading 30 min old > 15");
    let mut s = World::new("12:15");
    high_21(&mut s);
    s.reading("12:10", 219, 220);
    let t = s.no(21);
    let b = synthetic_book(
        &t,
        Some("0.30"),
        Some("0.34"),
        200,
        s.now - Duration::minutes(5),
    );
    s.books.insert(t, b);
    assert_blocked(&s.run(2, false), "order book stale");
}

#[test]
fn every_family_evaluates_without_inputs_and_never_panics() {
    // A bare day: a high, books on every token, no lab input at all.
    let mut w = World::new("12:15");
    high_21(&mut w);
    w.position = None;
    for v in 13..=24 {
        w.yes_book(v, "0.10", "0.12");
        w.no_book(v, "0.88", "0.90");
    }
    for family in 1..=25u8 {
        for variant in [false, true] {
            let out = w.run(family, variant);
            for e in &out.evaluations {
                assert_eq!(
                    wm_strategy::lab::family_of(e.strategy.as_str()),
                    Some(family)
                );
            }
            // Only the rules that need nothing beyond the METAR and the
            // book may trade here.
            for pr in &out.proposals {
                assert!(
                    [3u8, 22].contains(&family),
                    "L{family} traded without its inputs: {pr:?}"
                );
            }
        }
    }
}
