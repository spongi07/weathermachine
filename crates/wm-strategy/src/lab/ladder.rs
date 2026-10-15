//! L28, L33, L34, L39, L40, L45, L48, L54, L58, L59, L60 and L64: the
//! price ladder itself. A day's buckets are one distribution, priced bucket
//! by bucket by different people at different times; where the ladder
//! contradicts itself, lags, or shows who is about to trade, the rule takes
//! the side the market's own prices imply. Every one of them reads the book
//! as it stands (or a minute-by-minute history of it) and holds its signal
//! for a minute before it trades, so none races a faster trader.

use super::day::{Buy, Day, Quote, implied, phi};
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

/// L58 (`variant`: ≥ 0.12 over). Round-number premium: 07:00–20:00, a
/// bucket of a multiple of 5 °C (that can still win, ≥ 0.15 of the ladder)
/// priced ≥ 0.08 above the share a normal fitted to the other buckets
/// gives it — people like round numbers → its NO at 0.40–0.85.
pub(crate) fn round_number(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_residual = if variant { 0.12 } else { 0.08 };
    if let Some(why) = day.outside(7 * 60, 20 * 60) {
        day.idle(why, out);
        return;
    }
    let prices = day.alive_prices();
    let total: f64 = prices.iter().map(|(_, p)| p).sum();
    let mut best: Option<(&MarketOutcome, f64, f64)> = None;
    let mut seen = Vec::new();
    for (o, p) in &prices {
        let (Some(lo), Some(hi)) = (o.bucket.lower, o.bucket.upper) else {
            continue;
        };
        if lo != hi || lo.rem_euclid(5) != 0 || total <= 0.0 {
            continue;
        }
        let q = p / total;
        let others: Vec<(&MarketOutcome, f64)> = prices
            .iter()
            .filter(|(x, _)| x.condition_id != o.condition_id)
            .copied()
            .collect();
        if others.len() < 2 {
            continue;
        }
        let Some((mu, sd)) = implied(&others) else {
            continue;
        };
        let sd = sd.max(0.6);
        let v = f64::from(lo);
        let fair = phi((v + 0.5 - mu) / sd) - phi((v - 0.5 - mu) / sd);
        let residual = q - fair;
        seen.push(format!("{} {q:.2} vs {fair:.2}", o.label));
        if q >= 0.15 && best.is_none_or(|b| residual > b.2) {
            best = Some((o, q, residual));
        }
    }
    let Some((o, q, residual)) = best else {
        day.idle(
            if seen.is_empty() {
                "no round-number bucket (a multiple of 5 °C) that can still win".into()
            } else {
                format!(
                    "round-number buckets under 0.15 of the ladder: {}",
                    seen.join(", ")
                )
            },
            out,
        );
        return;
    };
    let mut blockers = Vec::new();
    if residual + 1e-9 < min_residual {
        blockers.push(format!(
            "{} holds {q:.2} of the ladder, {residual:+.2} over the normal of the others < {min_residual:.2}",
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
                "{} holds {q:.2} of the ladder, {residual:+.2} over what a normal fitted to the other buckets gives it",
                o.label
            )],
        },
        blockers,
        out,
    );
}

/// L59 (`variant`: bids ≥ 5 × the ask). The order book's imbalance, one
/// round trip a day: 09:00–18:00 away from reports (10 minutes before, 3
/// after), the favourite (0.20–0.80) with a spread ≤ 0.03 and ≥ 3 × as many
/// shares bid as offered (≥ 100 bid) → its YES; sold at the bid once it is
/// 0.03 above the average cost, 0.05 under it, after 45 minutes, or 10
/// minutes before a report. Returns when the position was first seen.
pub(crate) fn imbalance_scalp(
    day: &Day<'_, '_>,
    variant: bool,
    entered: Option<(TokenId, DateTime<Utc>)>,
    out: &mut StrategyOutput,
) -> Option<(TokenId, DateTime<Utc>)> {
    let now = day.now();
    let slug = &day.ctx.market.event_slug;
    if let Some(pos) = day.ctx.positions.for_event(slug).next() {
        let token = pos.instrument.token.clone();
        let since = entered
            .filter(|(t, _)| *t == token)
            .map_or(now, |(_, at)| at);
        let Some(o) = day
            .ctx
            .market
            .outcomes
            .iter()
            .find(|o| o.yes_token == token)
        else {
            return Some((token, since));
        };
        let avg = pos.avg_cost();
        let bid = day
            .ctx
            .books
            .get(&token)
            .and_then(|b| b.best_bid())
            .map(|l| l.price.as_f64());
        let held = now - since;
        let report_soon = day.minutes_to_report().is_some_and(|m| m <= 10);
        let why = match bid {
            Some(b) if b >= avg + 0.03 - 1e-9 => Some(format!(
                "bid {b:.3} ≥ the average cost {avg:.3} + 0.03: take the profit"
            )),
            Some(b) if b <= avg - 0.05 + 1e-9 => Some(format!(
                "bid {b:.3} ≤ the average cost {avg:.3} − 0.05: stop"
            )),
            _ if held >= Duration::minutes(45) => {
                Some(format!("held {} min ≥ 45", held.num_minutes()))
            }
            _ if report_soon => Some("10 minutes or less before a report".to_owned()),
            _ => None,
        };
        match why {
            Some(w) => day.sell_yes(o, pos.shares, vec![w], Vec::new(), out),
            None => day.note(
                o,
                OutcomeSide::Yes,
                vec![format!(
                    "holding {} YES {} since {} UTC (average {avg:.3}, bid {})",
                    pos.shares,
                    o.label,
                    since.format("%H:%M"),
                    bid.map_or_else(|| "—".to_owned(), |b| format!("{b:.3}"))
                )],
                out,
            ),
        }
        return Some((token, since));
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
            if spread > 0.03 + 1e-9 {
                blockers.push(format!("spread {spread:.3} > 0.03"));
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
/// 08:00–20:00, not in the 10 minutes before a report, on the favourite or
/// a neighbour (YES 0.10–0.60) whose best ask holds ≤ 50 shares with the
/// next ask ≥ 0.10 above it → offer YES halfway into the gap (a NO bid)
/// until 10 minutes before the next report: whoever sweeps the thin ask
/// pays our price instead of the far one.
pub(crate) fn gap_filler(day: &Day<'_, '_>, variant: bool, out: &mut StrategyOutput) {
    let min_gap = if variant { 0.15 } else { 0.10 };
    let mut blockers: Vec<String> = day.outside(8 * 60, 20 * 60).into_iter().collect();
    blockers.extend(day.near_report(10, 0));
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
/// reports (6 minutes before, 3 after), the next degree's YES asks down ≥
/// 60 % in shares against 4–7 minutes before (≥ 100 then) while its price
/// moved ≤ +0.03 — the sellers know something the price does not show yet
/// → its YES at 0.05–0.40.
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
            } else if moved > 0.03 + 1e-9 {
                blockers.push(format!("the price already moved {moved:+.3} > 0.03"));
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
