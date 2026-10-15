//! L28, L33, L34, L39, L40, L45, L48, L54, L58, L59, L60 and L64: the
//! price ladder itself. A day's buckets are one distribution, priced bucket
//! by bucket by different people at different times; where the ladder
//! contradicts itself, lags, or shows who is about to trade, the rule takes
//! the side the market's own prices imply. Every one of them reads the book
//! as it stands (or a minute-by-minute history of it) and holds its signal
//! for a minute before it trades, so none races a faster trader.

use super::day::{Buy, Day, Quote, implied};
use crate::quoting::{quote_expiry, tick_of};
use crate::strategy::StrategyOutput;
use chrono::{DateTime, Duration, Utc};
use wm_core::ids::TokenId;
use wm_core::market::{MarketOutcome, OutcomeSide};
use wm_core::units::{Price, Rounding, Shares, round_shares_to_lot, shares_for_notional};

/// L28 (`variant`: a dip ≥ 0.08 between neighbours ≥ 0.20). A dip in the
/// ladder: 00:00–16:00, a bucket that can still win priced ≥ 0.05 under
/// both its neighbours, each ≥ 0.12 — a day's maximum has one peak, so the
/// bucket between two likely ones is likely too → its YES at 0.03–0.30.
pub(crate) fn dip(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (min_drop, min_side) = if variant { (0.08, 0.20) } else { (0.05, 0.12) };
    if let Some(why) = day.outside(0, 16 * 60) {
        day.idle(why, out);
        return;
    }
    let alive: Vec<&MarketOutcome> = day.ordered().into_iter().filter(|o| day.alive(o)).collect();
    let mut best: Option<(&MarketOutcome, f64, f64, f64)> = None;
    for w in alive.windows(3) {
        let (Some(l), Some(m), Some(r)) = (
            day.yes_price(w[0]),
            day.yes_price(w[1]),
            day.yes_price(w[2]),
        ) else {
            continue;
        };
        let depth = l.min(r) - m;
        if l >= min_side
            && r >= min_side
            && depth + 1e-9 >= min_drop
            && best.is_none_or(|b| depth > b.1.min(b.2) - b.3)
        {
            best = Some((w[1], l, r, m));
        }
    }
    let Some((o, l, r, m)) = best else {
        day.idle(
            format!(
                "no dip: no bucket ≥ {min_drop:.2} under both neighbours ≥ {min_side:.2} ({})",
                ladder_text(day)
            ),
            out,
        );
        return;
    };
    day.taker(
        Buy {
            outcome: o,
            side: OutcomeSide::Yes,
            band: (0.03, 0.30),
            p_win: None,
            rationale: vec![format!(
                "{} at {m:.3} between neighbours at {l:.3} and {r:.3}: one peak, not two",
                o.label
            )],
        },
        Vec::new(),
        out,
    );
}

/// The ladder of the buckets that can still win: `20°C 0.31 · 21°C 0.42 …`.
fn ladder_text(day: &Day<'_, '_>) -> String {
    let v: Vec<String> = day
        .alive_prices()
        .iter()
        .map(|(o, p)| format!("{} {p:.2}", o.label))
        .collect();
    if v.is_empty() {
        "no fresh books".into()
    } else {
        v.join(" · ")
    }
}

/// L33 (`variant`: ≥ $500 and 6×). A whale's bid wall: 08:00–18:00, a best
/// YES bid worth ≥ $250 and ≥ 4 × the largest best bid of the same bucket
/// 2–10 minutes before (≥ 3 samples) — someone sure enough to show size
/// before buying → its YES at ≤ the wall + 0.03 (0.05–0.45).
pub(crate) fn whale_bid(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (min_usd, mult) = if variant { (500.0, 6.0) } else { (250.0, 4.0) };
    if let Some(why) = day.outside(8 * 60, 18 * 60) {
        day.idle(why, out);
        return;
    }
    let now = day.now();
    let before = day.samples_between(now - Duration::minutes(10), now - Duration::minutes(2));
    if before.len() < 3 {
        day.idle(
            format!(
                "{} ladder samples 2–10 min ago < 3 (one a minute)",
                before.len()
            ),
            out,
        );
        return;
    }
    let mut best: Option<(&MarketOutcome, f64, f64, f64)> = None;
    for o in day.ordered().into_iter().filter(|o| day.alive(o)) {
        let Some(bid) = day.yes_book(o).and_then(|b| b.best_bid()) else {
            continue;
        };
        let usd = bid.price.as_f64() * bid.size.as_f64();
        let earlier: Vec<f64> = before
            .iter()
            .filter_map(|s| s.quote(&o.yes_token))
            .map(|q| q.bid.map_or(0.0, |(p, sz)| p * sz))
            .collect();
        if earlier.len() < 3 {
            continue;
        }
        let max = earlier.iter().copied().fold(0.0, f64::max);
        if usd + 1e-9 >= min_usd && usd + 1e-9 >= mult * max {
            let ratio = usd / max.max(1.0);
            if best.is_none_or(|b| ratio > b.3) {
                best = Some((o, bid.price.as_f64(), usd, ratio));
            }
        }
    }
    let Some((o, price, usd, _)) = best else {
        day.idle(
            format!("no bid wall: no best YES bid ≥ ${min_usd:.0} and ≥ {mult:.0} × its size 2–10 min ago"),
            out,
        );
        return;
    };
    day.taker(
        Buy {
            outcome: o,
            side: OutcomeSide::Yes,
            band: (0.05, (price + 0.03).min(0.45)),
            p_win: None,
            rationale: vec![format!(
                "a ${usd:.0} YES bid at {price:.3} on {}, ≥ {mult:.0} × the largest of the 2–10 minutes before",
                o.label
            )],
        },
        Vec::new(),
        out,
    );
}

/// L34 (`variant`: KNMI up ≥ 1.5 °C). A stale ask: 10:00–17:00, the next
/// degree's best YES ask unchanged (price and size) in every sample of the
/// last 55 minutes while KNMI rose ≥ 1.0 °C in the hour and is ≥ 0.8 °C
/// under the edge at most — a forgotten order the weather has overtaken →
/// that ask at ≤ 0.40.
pub(crate) fn stale_ask(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_rise = if variant { 15 } else { 10 };
    let mut blockers: Vec<String> = day.outside(10 * 60, 17 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    let now = day.now();
    let ask = day.yes_book(j).and_then(|b| b.best_ask());
    let samples = day.samples_between(now - Duration::minutes(55), now);
    match ask {
        None => blockers.push("no fresh YES ask on the next degree".into()),
        Some(a) => {
            let (p, s) = (a.price.as_f64(), a.size.as_f64());
            let covered = samples
                .first()
                .is_some_and(|x| x.at <= now - Duration::minutes(53));
            let same = samples.iter().all(|x| {
                x.quote(&j.yes_token)
                    .and_then(|q| q.ask)
                    .is_some_and(|(qp, qs)| (qp - p).abs() < 1e-9 && (qs - s).abs() < 1e-6)
            });
            if !covered || samples.len() < 30 {
                blockers.push(format!(
                    "{} ladder samples over the last 55 min (needs ≥ 30 from 53 min back)",
                    samples.len()
                ));
            } else if !same {
                blockers.push(format!(
                    "the next degree's ask {p:.3} × {s:.0} changed in the last 55 min"
                ));
            } else {
                rationale.push(format!(
                    "{}'s ask {p:.3} × {s:.0} unchanged for 55 min",
                    j.label
                ));
            }
        }
    }
    match day.latest_reading() {
        Err(why) => blockers.push(why),
        Ok(r) => {
            let m = r.mean.map(|m| m.tenths());
            let before = day.mean_at(r.interval_end - Duration::minutes(60));
            match (m, before) {
                (Some(m), Some(b)) => {
                    if m - b < min_rise {
                        blockers.push(format!(
                            "KNMI up {} in an hour < {}",
                            super::day::c(m - b),
                            super::day::c(min_rise)
                        ));
                    } else if m < day.edge() - 8 {
                        blockers.push(format!(
                            "KNMI mean {} more than 0.8 °C under the edge {}",
                            super::day::c(m),
                            super::day::c(day.edge())
                        ));
                    } else {
                        rationale.push(format!(
                            "{}: up {} in an hour",
                            Day::describe(r),
                            super::day::c(m - b)
                        ));
                    }
                }
                _ => blockers.push("no KNMI mean now and an hour before".into()),
            }
        }
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::Yes,
            band: (0.03, 0.40),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}

/// L39 (`variant`: ≤ 0.98 for the pair). Pair lock: from the season's 75 %
/// peak time to 22:00, every bucket above the next degree priced ≤ 0.015,
/// so the day ends on the high or the next degree; when buying both YES at
/// their limits costs ≤ 0.99 a pair with the fees, the same number of
/// shares of each locks in the rest — whichever wins.
pub(crate) fn pair_lock(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let (Some(h), Some(j)) = (day.high_outcome(), day.next_outcome()) else {
        return;
    };
    let max_cost = if variant { 0.98 } else { 0.99 };
    let from = day
        .ctx
        .peak_times
        .and_then(|p| p.season(day.f.season))
        .and_then(|s| s.quantile(0.75))
        .unwrap_or(15 * 60 + 30);
    let mut blockers: Vec<String> = day.outside(from, 22 * 60).into_iter().collect();
    let mut rationale = Vec::new();
    for o in day.ordered().into_iter().filter(|o| day.alive(o)) {
        if o.condition_id == h.condition_id || o.condition_id == j.condition_id {
            continue;
        }
        match day.yes_price(o) {
            Some(p) if p <= 0.015 => {}
            Some(p) => blockers.push(format!("{} still priced {p:.3} > 0.015", o.label)),
            None => blockers.push(format!("no fresh book on {}", o.label)),
        }
    }
    let fees = day.ctx.market.fees;
    let slip = day.cfg.slippage_allowance.as_f64();
    let legs = [h, j].map(|o| {
        let book = day.yes_book(o)?;
        let a = book.best_ask()?.price;
        Some((a, day.limit_for(a, book)))
    });
    let mut shares = None;
    match legs {
        [Some((ah, lh)), Some((aj, lj))] => {
            let cost = lh.as_f64()
                + fees.taker_fee_per_share(lh)
                + lj.as_f64()
                + fees.taker_fee_per_share(lj)
                + 2.0 * slip;
            if cost > max_cost + 1e-9 {
                blockers.push(format!(
                    "the pair costs {cost:.3} at its limits ({lh} + {lj}, fees, slippage) > {max_cost:.2}"
                ));
            } else {
                rationale.push(format!(
                    "YES {} at {ah} and {} at {aj}: {cost:.3} a pair at the limits, one of them pays 1",
                    h.label, j.label
                ));
            }
            let pair = wm_core::units::Price::saturating_from_micros(
                lh.micros().saturating_add(lj.micros()),
            );
            let n = round_shares_to_lot(
                shares_for_notional(day.stake, pair, Rounding::Down),
                Shares::from_whole(1),
                Rounding::Down,
            );
            shares = Some(n);
        }
        _ => blockers.push("no fresh YES ask on the high's bucket and the next degree".into()),
    }
    let Some(n) = shares else {
        day.note(h, OutcomeSide::Yes, blockers, out);
        return;
    };
    // Both legs or neither.
    let mut legs_out = StrategyOutput::default();
    for o in [h, j] {
        day.taker_shares(
            Buy {
                outcome: o,
                side: OutcomeSide::Yes,
                band: (0.01, 0.99),
                p_win: None,
                rationale: rationale.clone(),
            },
            n,
            blockers.clone(),
            &mut legs_out,
        );
    }
    if legs_out.proposals.len() < 2 {
        legs_out.proposals.clear();
        for e in &mut legs_out.evaluations {
            if e.signal {
                e.signal = false;
                e.blockers
                    .push("the other leg cannot trade: both or neither".into());
            }
        }
    }
    out.proposals.extend(legs_out.proposals);
    out.evaluations.extend(legs_out.evaluations);
}

/// L40 (`variant`: by ≥ 0.01). The tail out of order: after the season's
/// median peak time until 23:00, the bucket two degrees above the high
/// priced ≥ 0.02 above the next degree — a warmer day cannot be likelier
/// than the step to it → its NO at 0.85–0.99 (p = 0.985).
pub(crate) fn tail_order(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let Some(k) = day.neighbour(j, 1).filter(|o| day.alive(o)) else {
        return;
    };
    let min_gap = if variant { 0.01 } else { 0.02 };
    let mut blockers: Vec<String> = day
        .outside(day.season_median(), 23 * 60)
        .into_iter()
        .collect();
    let mut rationale = Vec::new();
    match (day.yes_price(j), day.yes_price(k)) {
        (Some(pj), Some(pk)) => {
            if pk + 1e-9 < pj + min_gap {
                blockers.push(format!(
                    "{} {pk:.3} not ≥ {} {pj:.3} + {min_gap:.2}",
                    k.label, j.label
                ));
            } else {
                rationale.push(format!(
                    "{} at {pk:.3} above {} at {pj:.3}: out of order",
                    k.label, j.label
                ));
            }
        }
        _ => blockers.push("no fresh books on the two buckets above the high".into()),
    }
    day.taker(
        Buy {
            outcome: k,
            side: OutcomeSide::No,
            band: (0.85, 0.99),
            p_win: Some((0.985, 0.003)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L45 (`variant`: by ≥ 0.04). NO cheaper than the others' YES: 07:00–22:00,
/// a bucket's NO at its limit, with the fee and slippage, ≥ 0.02 under what
/// the best YES bids of the other buckets that can still win add up to —
/// the NO pays when any of them wins, and those buyers pay more for exactly
/// that → its NO (0.50–0.99), the rule's p being that sum.
pub(crate) fn complement_gap(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_edge = if variant { 0.04 } else { 0.02 };
    if let Some(why) = day.outside(7 * 60, 22 * 60) {
        day.idle(why, out);
        return;
    }
    let fees = day.ctx.market.fees;
    let slip = day.cfg.slippage_allowance.as_f64();
    let all = day.ordered();
    let mut best: Option<(&MarketOutcome, f64, f64)> = None;
    let mut missing = None;
    for k in all.iter().copied().filter(|o| day.alive(o)) {
        let Some(no) = day
            .ctx
            .books
            .get(&k.no_token)
            .filter(|b| b.age_ms(day.now()) <= day.cfg.max_book_age_ms)
        else {
            continue;
        };
        let Some(a) = no.best_ask() else { continue };
        // Only the buckets that can still win: a dead bucket's leftover
        // bids are no chance of the NO paying.
        let mut sum = 0.0;
        let mut ok = true;
        for o in all
            .iter()
            .filter(|o| o.condition_id != k.condition_id && day.alive(o))
        {
            match day.yes_book(o) {
                Some(b) => sum += b.best_bid().map_or(0.0, |l| l.price.as_f64()),
                None => {
                    ok = false;
                    missing = Some(o.label.clone());
                }
            }
        }
        if !ok {
            continue;
        }
        let l = day.limit_for(a.price, no);
        let edge = sum - l.as_f64() - fees.taker_fee_per_share(l) - slip;
        if best.is_none_or(|b| edge > b.2) {
            best = Some((k, sum, edge));
        }
    }
    let Some((k, sum, edge)) = best else {
        day.idle(
            missing.map_or_else(
                || "no NO ask on a bucket that can still win".to_owned(),
                |l| format!("no fresh YES book on {l}: the others' bids are not all known"),
            ),
            out,
        );
        return;
    };
    let mut blockers = Vec::new();
    if edge + 1e-9 < min_edge {
        blockers.push(format!(
            "the others' YES bids add up to {sum:.3}: {edge:+.3} over {}'s NO at its limit, fee and slippage < {min_edge:.2}",
            k.label
        ));
    }
    day.taker(
        Buy {
            outcome: k,
            side: OutcomeSide::No,
            band: (0.50, 0.99),
            p_win: Some((sum.min(0.999), min_edge)),
            rationale: vec![format!(
                "the other buckets' YES bids add up to {sum:.3}: {}'s NO pays when any of them wins",
                k.label
            )],
        },
        blockers,
        out,
    );
}

/// L48 (`variant`: ≥ 3 °C above). The market contradicts itself:
/// 07:00–11:30, the mean of the ladder's own distribution ≥ 2 °C above the
/// high while the high's bucket — no further warming at all — is still
/// priced ≥ 0.05 → its NO at 0.80–0.95 (p = 0.95).
pub(crate) fn self_contradiction(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(h) = day.high_outcome() else {
        return;
    };
    let gap = if variant { 3.0 } else { 2.0 };
    let mut blockers: Vec<String> = day.outside(7 * 60, 11 * 60 + 30).into_iter().collect();
    let mut rationale = Vec::new();
    match (implied(&day.alive_prices()), day.yes_price(h)) {
        (Some((mu, _)), Some(ph)) => {
            let high = f64::from(day.high);
            if mu + 1e-9 < high + gap {
                blockers.push(format!(
                    "the ladder's mean {mu:.1} °C less than {gap:.0} °C above the high {high:.0} °C"
                ));
            } else if ph < 0.05 {
                blockers.push(format!(
                    "{} priced {ph:.3} < 0.05: no contradiction",
                    h.label
                ));
            } else {
                rationale.push(format!(
                    "the ladder's mean {mu:.1} °C is {:.1} °C above the high, yet {} (no warming at all) is priced {ph:.3}",
                    mu - high,
                    h.label
                ));
            }
        }
        _ => blockers.push("no fresh books for the ladder".into()),
    }
    day.taker(
        Buy {
            outcome: h,
            side: OutcomeSide::No,
            band: (0.80, 0.95),
            p_win: Some((0.95, 0.005)),
            rationale,
        },
        blockers,
        out,
    );
}

/// L54 (`variant`: ≥ 1.12). Overround squeeze: 06:00–22:00, the YES prices
/// of the buckets that can still win adding up to ≥ 1.08 — more than a
/// certain outcome is worth;
/// the bucket that carries most of the excess (≥ 0.03, priced 0.20–0.70)
/// → its NO at 0.30–0.80.
pub(crate) fn overround(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_sum = if variant { 1.12 } else { 1.08 };
    if let Some(why) = day.outside(6 * 60, 22 * 60) {
        day.idle(why, out);
        return;
    }
    // The buckets that can still win: a dead bucket's leftover quotes are
    // no overround.
    let prices = day.alive_prices();
    let alive = day.ordered().into_iter().filter(|o| day.alive(o)).count();
    let alive_priced = prices.len();
    if alive_priced < alive {
        day.idle(
            format!("{alive_priced} of {alive} live buckets have a fresh book"),
            out,
        );
        return;
    }
    let sum: f64 = prices.iter().map(|(_, p)| p).sum();
    let best = prices
        .iter()
        .filter(|(o, p)| day.alive(o) && (0.20..=0.70).contains(p))
        .map(|(o, p)| (*o, *p, p - p / sum))
        .fold(None, |b: Option<(&MarketOutcome, f64, f64)>, x| match b {
            Some(y) if y.2 >= x.2 => Some(y),
            _ => Some(x),
        });
    let Some((o, p, excess)) = best else {
        day.idle(
            format!("the ladder adds up to {sum:.3}; no bucket priced 0.20–0.70"),
            out,
        );
        return;
    };
    let mut blockers = Vec::new();
    if sum + 1e-9 < min_sum {
        blockers.push(format!("the ladder adds up to {sum:.3} < {min_sum:.2}"));
    } else if excess + 1e-9 < 0.03 {
        blockers.push(format!(
            "{}'s share of the excess {excess:.3} < 0.03",
            o.label
        ));
    }
    day.taker(
        Buy {
            outcome: o,
            side: OutcomeSide::No,
            band: (0.30, 0.80),
            p_win: None,
            rationale: vec![format!(
                "the ladder adds up to {sum:.3}: {} at {p:.3} carries {excess:.3} of the excess",
                o.label
            )],
        },
        blockers,
        out,
    );
}

/// The share a curve through the ladder gives the bucket of `v` °C, read
/// off its single-degree neighbours `(value, share)` within 2 °C (shares ≥
/// 0.02, at least three, one on each side): a least-squares fit of ln(share)
/// = a + b·x + c·x² at x = their distance from `v`, read at x = 0. A day's
/// maximum is close to normal, whose log is a parabola: the fit is exact
/// for one, and the bucket judged stays out of it. `None` without enough
/// neighbours or without a hump (c ≥ 0).
pub fn curve_share(shares: &[(i32, f64)], v: i32) -> Option<f64> {
    let pts: Vec<(f64, f64)> = shares
        .iter()
        .filter(|(k, q)| *k != v && (k - v).abs() <= 2 && *q >= 0.02)
        .map(|(k, q)| (f64::from(k - v), q.ln()))
        .collect();
    if pts.len() < 3 || !pts.iter().any(|(x, _)| *x < 0.0) || !pts.iter().any(|(x, _)| *x > 0.0) {
        return None;
    }
    let mut s = [0.0_f64; 5];
    let mut t = [0.0_f64; 3];
    for (x, y) in &pts {
        let pw = [1.0, *x, x * x, x * x * x, x * x * x * x];
        for (sk, p) in s.iter_mut().zip(pw) {
            *sk += p;
        }
        for (tk, p) in t.iter_mut().zip(pw) {
            *tk += p * y;
        }
    }
    let m = [[s[0], s[1], s[2]], [s[1], s[2], s[3]], [s[2], s[3], s[4]]];
    let d = det3(&m);
    if d.abs() < 1e-9 {
        return None;
    }
    // Cramer's rule for a (column 0) and c (column 2).
    let solve = |i: usize| {
        let mut n = m;
        for (row, ti) in n.iter_mut().zip(t) {
            row[i] = ti;
        }
        det3(&n) / d
    };
    (solve(2) < 0.0).then(|| solve(0).exp())
}

fn det3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// L58 (`variant`: ≥ 0.12 over). Round-number premium: 07:00–20:00, a
/// bucket of a multiple of 5 °C that can still win, ≥ 0.15 of the ladder
/// and not its peak (a neighbour priced higher: a curve through the others
/// cannot judge a peak, and a confident forecast makes a peak taller than
/// any curve), priced ≥ 0.08 above the share a curve through its
/// neighbours gives it ([`curve_share`]) — people like round numbers → its
/// NO at 0.40–0.85.
pub(crate) fn round_number(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_residual = if variant { 0.12 } else { 0.08 };
    if let Some(why) = day.outside(7 * 60, 20 * 60) {
        day.idle(why, out);
        return;
    }
    let prices = day.alive_prices();
    let total: f64 = prices.iter().map(|(_, p)| p).sum();
    if total <= 0.0 {
        day.idle("no fresh book on a bucket that can still win".into(), out);
        return;
    }
    let shares: Vec<(i32, f64)> = prices
        .iter()
        .filter_map(|(o, p)| match (o.bucket.lower, o.bucket.upper) {
            (Some(lo), Some(hi)) if lo == hi => Some((lo, p / total)),
            _ => None,
        })
        .collect();
    let mut best: Option<(&MarketOutcome, f64, f64)> = None;
    let mut seen = Vec::new();
    for &(v, q) in shares.iter().filter(|(v, _)| v.rem_euclid(5) == 0) {
        let Some(o) = day.outcome_of(v) else {
            continue;
        };
        if q < 0.15 {
            seen.push(format!("{} {q:.2} of the ladder < 0.15", o.label));
            continue;
        }
        let peak = [-1, 1]
            .into_iter()
            .filter_map(|step| day.neighbour(o, step))
            .filter(|n| day.alive(n))
            .filter_map(|n| day.yes_price(n))
            .all(|pn| pn <= q * total);
        if peak {
            seen.push(format!(
                "{} {q:.2} is the ladder's peak (no curve can judge it)",
                o.label
            ));
            continue;
        }
        let Some(fair) = curve_share(&shares, v) else {
            seen.push(format!(
                "{} {q:.2}: too few buckets ≥ 0.02 within 2 °C on both sides for a curve",
                o.label
            ));
            continue;
        };
        if best.is_none_or(|b| q - fair > b.1 - b.2) {
            best = Some((o, q, fair));
        }
    }
    let Some((o, q, fair)) = best else {
        day.idle(
            if seen.is_empty() {
                "no round-number bucket (a multiple of 5 °C) that can still win".into()
            } else {
                seen.join("; ")
            },
            out,
        );
        return;
    };
    let residual = q - fair;
    let mut blockers = Vec::new();
    if residual + 1e-9 < min_residual {
        blockers.push(format!(
            "{} holds {q:.2} of the ladder, {residual:+.2} over the {fair:.2} a curve through its neighbours gives it < {min_residual:.2}",
            o.label
        ));
    }
    day.taker(
        Buy {
            outcome: o,
            side: OutcomeSide::No,
            band: (0.40, 0.85),
            p_win: None,
            rationale: vec![format!(
                "{} holds {q:.2} of the ladder, {residual:+.2} over the {fair:.2} a curve through its neighbours gives it (a shoulder, not the peak)",
                o.label
            )],
        },
        blockers,
        out,
    );
}

/// L59's round trip: the YES it holds, since when, and the YES bid when it
/// first saw the position (the stop's reference).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub token: TokenId,
    pub since: DateTime<Utc>,
    pub bid: Option<f64>,
}

/// L59 takes its profit once a sale at the bid nets this much a share over
/// the average cost (the entry's fee and slippage included), after the
/// sale's own fee.
const SCALP_TAKE: f64 = 0.02;
/// L59 stops once the bid lies this far under the bid at entry: the bids
/// that made the imbalance gave way.
const SCALP_GIVE: f64 = 0.02;
/// L59's widest spread at entry: a round trip pays it, two fees and a tick
/// of slippage.
const SCALP_SPREAD: f64 = 0.02;

/// L59 (`variant`: bids ≥ 5 × the ask). The order book's imbalance, one
/// round trip a day: 09:00–18:00 away from reports (10 minutes before one,
/// until 3 after it came in), the favourite (0.20–0.80) with a spread ≤
/// 0.02 and ≥ 3 × as many shares bid as offered (≥ 100 bid) → its YES.
/// Sold at the bid once that nets ≥ 0.02 a share over the average cost
/// after the fee, once the bid falls 0.02 under the bid at entry (the bids
/// gave way), after 45 minutes, or 10 minutes before a report. Returns the
/// round trip while it holds the position.
pub(crate) fn imbalance_scalp(
    day: &Day<'_, '_>,
    variant: bool,
    entered: Option<Entry>,
    out: &mut StrategyOutput,
) -> Option<Entry> {
    let now = day.now();
    let slug = &day.ctx.market.event_slug;
    if let Some(pos) = day.ctx.positions.for_event(slug).next() {
        let token = pos.instrument.token.clone();
        let bid = day
            .ctx
            .books
            .get(&token)
            .and_then(|b| b.best_bid())
            .map(|l| l.price.as_f64());
        // First seen now (just filled, or after a restart): the bid now is
        // the stop's reference and the clock starts.
        let entry = match entered.filter(|e| e.token == token) {
            Some(e) => Entry {
                bid: e.bid.or(bid),
                ..e
            },
            None => Entry {
                token: token.clone(),
                since: now,
                bid,
            },
        };
        let Some(o) = day
            .ctx
            .market
            .outcomes
            .iter()
            .find(|o| o.yes_token == token)
        else {
            return Some(entry);
        };
        let avg = pos.avg_cost();
        let held = now - entry.since;
        let report_soon = day.minutes_to_report().is_some_and(|m| m <= 10);
        let fees = day.ctx.market.fees;
        let why = match bid {
            Some(b) => {
                let net = b - Price::from_f64(b).map_or(0.0, |p| fees.taker_fee_per_share(p));
                match entry.bid {
                    _ if net >= avg + SCALP_TAKE - 1e-9 => Some(format!(
                        "the bid {b:.3} nets {net:.3} after the fee, ≥ the average cost {avg:.3} + {SCALP_TAKE:.2}: take the profit"
                    )),
                    Some(r) if b <= r - SCALP_GIVE + 1e-9 => Some(format!(
                        "the bid {b:.3} is {SCALP_GIVE:.2} or more under {r:.3} at entry: the bids gave way"
                    )),
                    _ => None,
                }
            }
            None => None,
        }
        .or_else(|| {
            (held >= Duration::minutes(45))
                .then(|| format!("held {} min ≥ 45", held.num_minutes()))
        })
        .or_else(|| report_soon.then(|| "10 minutes or less before a report".to_owned()));
        match why {
            Some(w) => day.sell_yes(o, pos.shares, vec![w], Vec::new(), out),
            None => day.note(
                o,
                OutcomeSide::Yes,
                vec![format!(
                    "holding {} YES {} since {} UTC (average {avg:.3}, bid {}, stop under {})",
                    pos.shares,
                    o.label,
                    entry.since.format("%H:%M"),
                    bid.map_or_else(|| "—".to_owned(), |b| format!("{b:.3}")),
                    entry
                        .bid
                        .map_or_else(|| "—".to_owned(), |r| format!("{:.3}", r - SCALP_GIVE))
                )],
                out,
            ),
        }
        return Some(entry);
    }
    if day.traded_today() {
        day.idle("one round trip a day: done".into(), out);
        return None;
    }
    let mult = if variant { 5.0 } else { 3.0 };
    let Some((fav, mid)) = day.favourite() else {
        day.idle("no fresh book to name a favourite".into(), out);
        return None;
    };
    let mut blockers: Vec<String> = day.outside(9 * 60, 18 * 60).into_iter().collect();
    blockers.extend(day.near_report(10, 3));
    let mut rationale = Vec::new();
    match day
        .yes_book(fav)
        .and_then(|b| b.best_bid().zip(b.best_ask()))
    {
        None => blockers.push(format!("no two-sided YES book on {}", fav.label)),
        Some((b, a)) => {
            let spread = a.price.as_f64() - b.price.as_f64();
            let (bs, asz) = (b.size.as_f64(), a.size.as_f64());
            if spread > SCALP_SPREAD + 1e-9 {
                blockers.push(format!("spread {spread:.3} > {SCALP_SPREAD:.2}"));
            }
            if bs < 100.0 || bs + 1e-9 < mult * asz {
                blockers.push(format!(
                    "{bs:.0} shares bid vs {asz:.0} offered (needs ≥ 100 and ≥ {mult:.0} ×)"
                ));
            } else {
                rationale.push(format!(
                    "{} favourite at {mid:.3}: {bs:.0} shares bid vs {asz:.0} offered",
                    fav.label
                ));
            }
        }
    }
    day.taker(
        Buy {
            outcome: fav,
            side: OutcomeSide::Yes,
            band: (0.20, 0.80),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
    None
}

/// L60 (`variant`: a gap ≥ 0.15). Fill the gap in a thin ask ladder:
/// 08:00–20:00, not in the 10 minutes before a report nor until 3 minutes
/// after it came in (a quote the news can pick off), on the favourite or
/// a neighbour (YES 0.10–0.60) whose best ask holds ≤ 50 shares with the
/// next ask ≥ 0.10 above it → offer YES halfway into the gap (a NO bid)
/// until 10 minutes before the next report: whoever sweeps the thin ask
/// pays our price instead of the far one.
pub(crate) fn gap_filler(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_gap = if variant { 0.15 } else { 0.10 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 20 * 60).into_iter().collect();
    blockers.extend(day.near_report(10, 3));
    let Some((fav, _)) = day.favourite() else {
        day.idle("no fresh book to name a favourite".into(), out);
        return;
    };
    let mut best: Option<(&MarketOutcome, f64, f64, f64)> = None;
    for o in [day.neighbour(fav, -1), Some(fav), day.neighbour(fav, 1)]
        .into_iter()
        .flatten()
        .filter(|o| day.alive(o))
    {
        let (Some(mid), Some(b)) = (day.yes_price(o), day.yes_book(o)) else {
            continue;
        };
        if !(0.10..=0.60).contains(&mid) || b.asks.len() < 2 {
            continue;
        }
        let (a1, a2) = (b.asks[0], b.asks[1]);
        let gap = a2.price.as_f64() - a1.price.as_f64();
        if a1.size.as_f64() <= 50.0 && best.is_none_or(|x| gap > x.3) {
            best = Some((o, a1.price.as_f64(), a2.price.as_f64(), gap));
        }
    }
    let Some((o, p1, p2, gap)) = best else {
        day.idle(
            "no thin best ask (≤ 50 shares) on the favourite or its neighbours (YES 0.10–0.60)"
                .into(),
            out,
        );
        return;
    };
    if gap + 1e-9 < min_gap {
        blockers.push(format!(
            "{}'s asks {p1:.3} then {p2:.3}: a gap of {gap:.3} < {min_gap:.2}",
            o.label
        ));
    }
    let tick = day
        .ctx
        .books
        .get(&o.no_token)
        .map_or_else(|| Price::saturating_from_micros(10_000), tick_of);
    let offer = f64::midpoint(p1, p2);
    let price = Price::from_f64(1.0 - offer)
        .ok()
        .map(|p| p.floor_to_tick(tick));
    let expires_at = quote_expiry(
        day.now(),
        day.ctx.routine_minutes,
        Duration::minutes(day.cfg.cancel_before_report_minutes),
        Duration::minutes(day.cfg.min_rest_minutes),
    );
    day.maker_no(
        Quote {
            outcome: o,
            yes_band: (0.10, 0.70),
            price,
            expires_at,
            rationale: vec![format!(
                "{}'s best ask {p1:.3} is thin, the next {p2:.3}: offering YES at {offer:.3} in the gap",
                o.label
            )],
        },
        blockers,
        out,
    );
}

/// L64 (`variant`: ≥ 75 % gone). Sellers withdraw: 09:00–18:00 away from
/// reports (6 minutes before one, until 3 after it came in), the next
/// degree's YES asks down ≥ 60 % in shares against 4–7 minutes before (≥
/// 100 then) while its price moved no more than 0.03 either way — the
/// sellers know something the price does not show yet (a price that moved
/// is the book repricing, not sellers stepping back) → its YES at
/// 0.05–0.40.
pub(crate) fn ask_vacuum(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let Some(j) = day.next_outcome() else {
        return;
    };
    let min_drop = if variant { 0.75 } else { 0.60 };
    let mut blockers: Vec<String> = day.outside(9 * 60, 18 * 60).into_iter().collect();
    blockers.extend(day.near_report(6, 3));
    let mut rationale = Vec::new();
    let now = day.now();
    let then = day
        .sample_at(now - Duration::minutes(4))
        .filter(|s| s.at >= now - Duration::minutes(7))
        .and_then(|s| s.quote(&j.yes_token));
    match (day.yes_book(j), then) {
        (Some(b), Some(q)) => {
            let depth: f64 = b.asks.iter().map(|l| l.size.as_f64()).sum();
            let drop = if q.ask_depth > 0.0 {
                1.0 - depth / q.ask_depth
            } else {
                0.0
            };
            let moved = match (day.yes_price(j), q.mid()) {
                (Some(a), Some(b)) => a - b,
                _ => 0.0,
            };
            if q.ask_depth < 100.0 {
                blockers.push(format!(
                    "{:.0} shares offered 4–7 min ago < 100",
                    q.ask_depth
                ));
            } else if drop + 1e-9 < min_drop {
                blockers.push(format!(
                    "offered {depth:.0} shares vs {:.0} 4–7 min ago: down {:.0} % < {:.0} %",
                    q.ask_depth,
                    100.0 * drop,
                    100.0 * min_drop
                ));
            } else if moved.abs() > 0.03 + 1e-9 {
                blockers.push(format!(
                    "the price moved {moved:+.3} (more than 0.03 either way): a repricing, not sellers stepping back"
                ));
            } else {
                rationale.push(format!(
                    "{}'s offered shares {:.0} → {depth:.0} (−{:.0} %) with the price {moved:+.3}",
                    j.label,
                    q.ask_depth,
                    100.0 * drop
                ));
            }
        }
        _ => blockers.push("no fresh book now and a ladder sample 4–7 min ago".into()),
    }
    day.taker(
        Buy {
            outcome: j,
            side: OutcomeSide::Yes,
            band: (0.05, 0.40),
            p_win: None,
            rationale,
        },
        blockers,
        out,
    );
}
