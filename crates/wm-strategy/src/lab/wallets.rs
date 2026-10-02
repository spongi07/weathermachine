//! Takers' track records (L19 follows the skilled ones, L20 fades the
//! losing ones).
//!
//! Every taker trade on a settled market day is one observation: its P&L a
//! share after the taker fee, held to settlement. A taker counts as skilled
//! with ≥ [`MIN_TRADES`] trades making ≥ 0.02 a share at t ≥ 2 (L19's
//! variant: t ≥ 3), as losing with ≥ 30 trades losing ≥ 0.05 a share at
//! t ≤ −2. `research market` scores prequentially (only the days before the
//! one replayed); the paper strategies read the scores of the settled days
//! before today.

use std::collections::HashMap;
use wm_core::event::{WalletScore, WalletScoresEvent};

/// Fewest earlier trades before a taker is judged.
pub const MIN_TRADES: u64 = 30;
/// A skilled taker makes at least this a share …
pub const SKILLED_MEAN: f64 = 0.02;
/// … a losing one loses at least this.
pub const LOSING_MEAN: f64 = -0.05;
/// A losing taker's t is at most this.
pub const LOSING_T: f64 = -2.0;

/// A taker's record: P&L per share after the taker fee, one observation
/// per trade.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WalletStats {
    pub n: u64,
    sum: f64,
    sum_sq: f64,
}

impl WalletStats {
    /// A record of `n` trades with this mean and standard deviation (a
    /// record summarised elsewhere, or a test's).
    pub fn from_moments(n: u64, mean: f64, sd: f64) -> Self {
        let nf = n as f64;
        Self {
            n,
            sum: mean * nf,
            sum_sq: sd * sd * (nf - 1.0).max(0.0) + nf * mean * mean,
        }
    }

    pub fn add(&mut self, pnl: f64) {
        self.n += 1;
        self.sum += pnl;
        self.sum_sq += pnl * pnl;
    }

    pub fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }

    /// The mean over its standard error.
    pub fn t(&self) -> f64 {
        if self.n < 2 {
            return 0.0;
        }
        let n = self.n as f64;
        let var = ((self.sum_sq - n * self.mean().powi(2)) / (n - 1.0)).max(0.0);
        self.mean() / (var.sqrt().max(1e-6) / n.sqrt())
    }
}

/// One taker trade of a settled day, in YES terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredTrade<'a> {
    pub taker: &'a str,
    /// The YES price of the trade.
    pub yes_price: f64,
    pub taker_buys_yes: bool,
    /// Whether the bucket traded resolved YES.
    pub bucket_won: bool,
}

/// Every taker's record on the market days learned so far.
#[derive(Debug, Clone, Default)]
pub struct WalletBook {
    stats: HashMap<String, WalletStats>,
}

impl WalletBook {
    /// Learn one settled day's taker trades.
    pub fn add_trades<'a>(
        &mut self,
        trades: impl IntoIterator<Item = ScoredTrade<'a>>,
        fee_rate: f64,
    ) {
        for t in trades {
            let p = t.yes_price;
            let fee = fee_rate * p * (1.0 - p);
            let pnl = if t.taker_buys_yes {
                f64::from(u8::from(t.bucket_won)) - p - fee
            } else {
                f64::from(u8::from(!t.bucket_won)) - (1.0 - p) - fee
            };
            self.stats.entry(t.taker.to_owned()).or_default().add(pnl);
        }
    }

    pub fn get(&self, wallet: &str) -> Option<&WalletStats> {
        self.stats.get(wallet)
    }

    /// Set a taker's record.
    pub fn insert(&mut self, wallet: impl Into<String>, stats: WalletStats) {
        self.stats.insert(wallet.into(), stats);
    }

    /// Made ≥ 0.02 a share over ≥ 30 trades with t ≥ `min_t`.
    pub fn skilled(&self, wallet: &str, min_t: f64) -> bool {
        self.get(wallet)
            .is_some_and(|s| skilled(s.n, s.mean(), s.t(), min_t))
    }

    /// Lost ≥ 0.05 a share over ≥ 30 trades with t ≤ −2.
    pub fn losing(&self, wallet: &str) -> bool {
        self.get(wallet)
            .is_some_and(|s| losing(s.n, s.mean(), s.t()))
    }

    /// (takers, skilled at t ≥ 2, losing).
    pub fn counts(&self) -> (u64, u64, u64) {
        let skilled = self.stats.keys().filter(|w| self.skilled(w, 2.0)).count();
        let losing = self.stats.keys().filter(|w| self.losing(w)).count();
        (self.stats.len() as u64, skilled as u64, losing as u64)
    }

    /// Every taker with at least [`MIN_TRADES`] trades, as the engine
    /// receives them (sorted by wallet, so the event is deterministic).
    pub fn scores(&self) -> Vec<WalletScore> {
        let mut out: Vec<WalletScore> = self
            .stats
            .iter()
            .filter(|(_, s)| s.n >= MIN_TRADES)
            .map(|(w, s)| WalletScore {
                wallet: w.clone(),
                trades: s.n,
                mean: s.mean(),
                t: s.t(),
            })
            .collect();
        out.sort_by(|a, b| a.wallet.cmp(&b.wallet));
        out
    }

    pub fn len(&self) -> usize {
        self.stats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }
}

fn skilled(n: u64, mean: f64, t: f64, min_t: f64) -> bool {
    n >= MIN_TRADES && mean >= SKILLED_MEAN && t >= min_t
}

fn losing(n: u64, mean: f64, t: f64) -> bool {
    n >= MIN_TRADES && mean <= LOSING_MEAN && t <= LOSING_T
}

/// The scores the engine holds: the latest [`WalletScoresEvent`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalletScores {
    pub through: Option<chrono::NaiveDate>,
    pub days: u32,
    pub takers: u64,
    by_wallet: HashMap<String, WalletScore>,
}

impl WalletScores {
    pub fn from_event(e: &WalletScoresEvent) -> Self {
        Self {
            through: Some(e.through),
            days: e.days,
            takers: e.takers,
            by_wallet: e
                .scores
                .iter()
                .map(|s| (s.wallet.clone(), s.clone()))
                .collect(),
        }
    }

    pub fn get(&self, wallet: &str) -> Option<&WalletScore> {
        self.by_wallet.get(wallet)
    }

    pub fn skilled(&self, wallet: &str, min_t: f64) -> bool {
        self.get(wallet)
            .is_some_and(|s| skilled(s.trades, s.mean, s.t, min_t))
    }

    pub fn losing(&self, wallet: &str) -> bool {
        self.get(wallet)
            .is_some_and(|s| losing(s.trades, s.mean, s.t))
    }

    /// (skilled at `min_t`, losing).
    pub fn counts(&self, min_t: f64) -> (usize, usize) {
        let skilled = self
            .by_wallet
            .values()
            .filter(|s| skilled(s.trades, s.mean, s.t, min_t))
            .count();
        let losing = self
            .by_wallet
            .values()
            .filter(|s| losing(s.trades, s.mean, s.t))
            .count();
        (skilled, losing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trades(taker: &str, n: usize, yes_price: f64, won: bool) -> Vec<ScoredTrade<'_>> {
        (0..n)
            .map(|i| ScoredTrade {
                taker,
                // A little spread, so the t has a standard error.
                yes_price: yes_price + 0.001 * (i % 3) as f64,
                taker_buys_yes: true,
                bucket_won: won,
            })
            .collect()
    }

    #[test]
    fn takers_are_scored_after_the_fee_and_judged_with_thirty_trades() {
        let mut book = WalletBook::default();
        // "a" buys a 0.60 favourite that wins: +0.388 a share after the fee.
        book.add_trades(trades("a", 30, 0.60, true), 0.05);
        // "b" buys 0.10 longshots that lose: −0.1045 a share.
        book.add_trades(trades("b", 40, 0.10, false), 0.05);
        // "c" has too few trades to judge.
        book.add_trades(trades("c", 29, 0.60, true), 0.05);
        let a = book.get("a").unwrap();
        assert_eq!(a.n, 30);
        assert!((a.mean() - (1.0 - 0.601 - 0.05 * 0.601 * 0.399)).abs() < 0.01);
        assert!(book.skilled("a", 2.0) && book.skilled("a", 3.0));
        assert!(book.losing("b") && !book.skilled("b", 2.0));
        assert!(!book.skilled("c", 2.0), "29 trades are too few");
        assert_eq!(book.counts(), (3, 1, 1));
        // The engine's view: only takers with enough trades, the same verdicts.
        let scores = book.scores();
        assert_eq!(
            scores.iter().map(|s| s.wallet.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let ev = WalletScoresEvent {
            through: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            days: 30,
            takers: book.len() as u64,
            scores,
        };
        let view = WalletScores::from_event(&ev);
        assert!(view.skilled("a", 3.0) && view.losing("b"));
        assert!(!view.skilled("c", 2.0) && !view.losing("unknown"));
        assert_eq!(view.counts(2.0), (1, 1));
        // A NO taker's P&L: selling YES at 0.30 on a bucket that lost.
        let mut no = WalletBook::default();
        no.add_trades(
            [ScoredTrade {
                taker: "n",
                yes_price: 0.30,
                taker_buys_yes: false,
                bucket_won: false,
            }],
            0.05,
        );
        let n = no.get("n").unwrap();
        assert!((n.mean() - (1.0 - 0.70 - 0.05 * 0.30 * 0.70)).abs() < 1e-9);
    }
}
