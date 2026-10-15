//! L38, L44, L47 and L63: who trades, and how. A calm tape leaves tails
//! to whoever quotes them; a wallet nobody has seen before buying a
//! longshot, and a crowd of small evening buyers, are not informed; a
//! seller who keeps refilling the same ask hides more than the book shows.

use super::day::{Buy, Day, Flow, Quote, c};
use crate::quoting::{quote_expiry, tick_of};
use crate::strategy::StrategyOutput;
use chrono::Duration;
use std::collections::BTreeMap;
use wm_core::market::{MarketOutcome, OutcomeSide};
use wm_core::units::Price;

/// The expiry of a quote resting until shortly before the next report.
fn until_report(day: &Day<'_, '_>) -> Option<chrono::DateTime<chrono::Utc>> {
    quote_expiry(
        day.now(),
        day.ctx.routine_minutes,
        Duration::minutes(day.cfg.cancel_before_report_minutes),
        Duration::minutes(day.cfg.min_rest_minutes),
    )
}

/// L38 (`variant`: three and four places up). Calm-tape tail maker:
/// 09:00–21:00, a tape that has gone quiet — ≤ 1500 shares in 30 minutes,
/// buyers and sellers within 35 % of each other, no trade of ≥ 200 shares
/// in 15 minutes — so nobody is informed right now → NO bids on the buckets
/// two and three places above the favourite (YES offered at 0.02–0.12),
/// one tick inside the spread, until 10 minutes before the next report.
pub(crate) fn calm_tails(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let places: [i32; 2] = if variant { [3, 4] } else { [2, 3] };
    let mut blockers: Vec<String> = day.outside(9 * 60, 21 * 60).into_iter().collect();
    let now = day.now();
    if day.lab.takers.is_empty() {
        blockers.push("no taker trades received (the Data API poll)".into());
    }
    let flow: Vec<Flow<'_>> = day
        .flow()
        .into_iter()
        .filter(|t| t.at >= now - Duration::minutes(30))
        .collect();
    let total: f64 = flow.iter().map(|t| t.shares).sum();
    let buys: f64 = flow
        .iter()
        .filter(|t| t.taker_buys_yes)
        .map(|t| t.shares)
        .sum();
    let imbalance = if total > 0.0 {
        (2.0 * buys - total).abs() / total
    } else {
        0.0
    };
    let big = flow
        .iter()
        .filter(|t| t.at >= now - Duration::minutes(15))
        .map(|t| t.shares)
        .fold(0.0, f64::max);
    let mut rationale = Vec::new();
    if total > 1500.0 {
        blockers.push(format!("{total:.0} shares traded in 30 min > 1500"));
    } else if imbalance > 0.35 + 1e-9 {
        blockers.push(format!(
            "one-sided tape: {:.0} % imbalance in 30 min > 35 %",
            100.0 * imbalance
        ));
    } else if big >= 200.0 {
        blockers.push(format!("a trade of {big:.0} shares in 15 min (≥ 200)"));
    } else {
        rationale.push(format!(
            "calm tape: {total:.0} shares in 30 min, {:.0} % imbalance, largest {big:.0}",
            100.0 * imbalance
        ));
    }
    let Some((fav, p)) = day.favourite() else {
        day.idle("no fresh book to name a favourite".into(), out);
        return;
    };
    rationale.push(format!("favourite {} at {p:.2}", fav.label));
    let expires_at = until_report(day);
    let mut any = false;
    for n in places {
        let Some(o) = day.neighbour(fav, n).filter(|o| day.alive(o)) else {
            continue;
        };
        any = true;
        day.maker_no(
            Quote {
                outcome: o,
                yes_band: (0.02, 0.12),
                price: None,
                expires_at,
                rationale: rationale.clone(),
            },
            blockers.clone(),
            out,
        );
    }
    if !any {
        day.idle(
            format!(
                "no bucket {} and {} places above the favourite {}",
                places[0], places[1], fav.label
            ),
            out,
        );
    }
}

/// L44 (`variant`: ≥ 50 shares). Fade the unknown longshot buyer: a wallet
/// with no record on the settled days buys ≥ 20 YES shares at 0.02–0.10 of
/// a bucket ≥ 2 °C above the high in the last 30 minutes (and ≥ 30 seconds
/// ago) → a NO bid one tick under their price until 30 minutes after their
/// trade. A newcomer's longshot is a ticket, not information.
pub(crate) fn fresh_wallet(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(wallets) = day.lab.wallets else {
        day.idle(
            "no taker records yet (loaded from settled days after the start)".into(),
            out,
        );
        return;
    };
    let min_shares = if variant { 50.0 } else { 20.0 };
    let now = day.now();
    let mut latest: BTreeMap<String, Flow<'_>> = BTreeMap::new();
    for t in day.flow() {
        let Some(w) = t.taker else { continue };
        let far = t.outcome.bucket.lower.is_some_and(|lo| lo >= day.high + 2);
        if !t.taker_buys_yes
            || !(0.02..=0.10).contains(&t.yes_price)
            || t.shares < min_shares
            || !far
            || t.at < now - Duration::minutes(30)
            || now < t.at + Duration::seconds(30)
            || wallets.get(w).is_some()
        {
            continue;
        }
        latest.insert(t.outcome.label.clone(), t);
    }
    if latest.is_empty() {
        day.idle(
            format!(
                "no buy of ≥ {min_shares:.0} YES at 0.02–0.10 ≥ 2 °C above the high by a wallet without a record in the last 30 min"
            ),
            out,
        );
        return;
    }
    for t in latest.values() {
        let tick = day
            .ctx
            .books
            .get(&t.outcome.no_token)
            .map_or(0.01, |b| tick_of(b).as_f64());
        let offer = (t.yes_price - tick).max(0.01);
        let price = Price::from_f64(1.0 - offer)
            .ok()
            .map(|p| p.floor_to_tick(Price::from_f64(tick).unwrap_or(p)));
        day.maker_no(
            Quote {
                outcome: t.outcome,
                yes_band: (0.0, 0.10),
                price,
                expires_at: Some(t.at + Duration::minutes(30)),
                rationale: vec![format!(
                    "wallet {} without a record bought {:.0} YES {} at {:.3}, {} UTC",
                    t.taker.unwrap_or("?"),
                    t.shares,
                    t.outcome.label,
                    t.yes_price,
                    t.at.format("%H:%M:%S")
                )],
            },
            Vec::new(),
            out,
        );
    }
}

/// L47 (`variant`: ≥ 8 buys). Evening lottery tickets: 17:00–23:00, ≥ 5
/// small YES buys (≤ 20 shares at 0.02–0.15) in 30 minutes on the next
/// degree or the one above it, KNMI ≥ 1 °C under the edge → a NO bid on
/// that bucket one tick inside the spread until 10 minutes before the next
/// report.
pub(crate) fn lottery_hour(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_buys = if variant { 8 } else { 5 };
    let mut blockers: Vec<String> = day.outside(17 * 60, 23 * 60).into_iter().collect();
    let now = day.now();
    let targets: Vec<&MarketOutcome> = [day.next_outcome()]
        .into_iter()
        .flatten()
        .flat_map(|j| [Some(j), day.neighbour(j, 1)])
        .flatten()
        .filter(|o| day.alive(o))
        .collect();
    let mut counts: BTreeMap<String, (usize, &MarketOutcome)> = BTreeMap::new();
    for t in day.flow() {
        if t.at < now - Duration::minutes(30)
            || !t.taker_buys_yes
            || t.shares > 20.0
            || !(0.02..=0.15).contains(&t.yes_price)
        {
            continue;
        }
        if targets
            .iter()
            .any(|o| o.condition_id == t.outcome.condition_id)
        {
            counts
                .entry(t.outcome.label.clone())
                .or_insert((0, t.outcome))
                .0 += 1;
        }
    }
    let Some((n, o)) = counts
        .values()
        .copied()
        .max_by_key(|(n, o)| (*n, o.bucket.sort_key()))
    else {
        day.idle(
            "no small YES buys (≤ 20 shares at 0.02–0.15) on the next two degrees in 30 min".into(),
            out,
        );
        return;
    };
    let mut rationale = vec![format!("{n} small YES buys of {} in 30 min", o.label)];
    if n < min_buys {
        blockers.push(format!(
            "{n} small YES buys of {} in 30 min < {min_buys}",
            o.label
        ));
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => match r.mean.map(|m| m.tenths()) {
            Some(m) if m <= day.edge() - 10 => rationale.push(Day::describe(r)),
            Some(m) => blockers.push(format!(
                "KNMI mean {} not ≥ 1 °C under the edge {}",
                c(m),
                c(day.edge())
            )),
            None => blockers.push("KNMI mean missing".into()),
        },
    }
    day.maker_no(
        Quote {
            outcome: o,
            yes_band: (0.02, 0.15),
            price: None,
            expires_at: until_report(day),
            rationale,
        },
        blockers,
        out,
    );
}

/// L63 (`variant`: ≥ 5 × the size shown). A hidden seller: 08:00–20:00, ≥ 3
/// taker buys of one bucket's YES at one price in 30 minutes adding up to ≥
/// 3 × the size the best ask showed at that price before them, and that
/// ask still standing — a seller refilling an order larger than it shows
/// → its NO at 0.20–0.80.
pub(crate) fn iceberg(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let mult = if variant { 5.0 } else { 3.0 };
    if let Some(why) = day.outside(8 * 60, 20 * 60) {
        day.idle(why, out);
        return;
    }
    let now = day.now();
    // Per bucket and price (to the 0.001): the buys, oldest first.
    let mut groups: BTreeMap<(String, i64), Vec<Flow<'_>>> = BTreeMap::new();
    for t in day.flow() {
        if t.at < now - Duration::minutes(30) || !t.taker_buys_yes || !day.alive(t.outcome) {
            continue;
        }
        #[allow(clippy::cast_possible_truncation)]
        let key = (
            t.outcome.label.clone(),
            (t.yes_price * 1000.0).round() as i64,
        );
        groups.entry(key).or_default().push(t);
    }
    let mut best: Option<(&MarketOutcome, f64, f64, f64, usize)> = None;
    for ((_, milli), buys) in &groups {
        if buys.len() < 3 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let price = *milli as f64 / 1000.0;
        let o = buys[0].outcome;
        let shown = day
            .sample_at(buys[0].at - Duration::seconds(1))
            .and_then(|s| s.quote(&o.yes_token))
            .and_then(|q| q.ask)
            .filter(|(p, _)| (p - price).abs() < 1e-6)
            .map(|(_, s)| s);
        let still = day
            .yes_book(o)
            .and_then(|b| b.best_ask())
            .is_some_and(|a| (a.price.as_f64() - price).abs() < 1e-6);
        let total: f64 = buys.iter().map(|t| t.shares).sum();
        if let Some(s) = shown
            && still
            && total + 1e-9 >= mult * s
        {
            let ratio = total / s.max(1.0);
            if best.is_none_or(|b| ratio > b.3 / b.2.max(1.0)) {
                best = Some((o, price, s, total, buys.len()));
            }
        }
    }
    let Some((o, price, shown, total, n)) = best else {
        day.idle(
            format!(
                "no ask refilled: no 3 buys at one price in 30 min adding up to ≥ {mult:.0} × the size shown, the ask still there"
            ),
            out,
        );
        return;
    };
    day.taker(
        Buy {
            outcome: o,
            side: OutcomeSide::No,
            band: (0.20, 0.80),
            p_win: None,
            rationale: vec![format!(
                "{n} buys of {} YES at {price:.3} took {total:.0} shares where {shown:.0} showed, and the ask still stands",
                o.label
            )],
        },
        Vec::new(),
        out,
    );
}
