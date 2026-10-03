//! Tiny deterministic PRNG (SplitMix64) used for retry jitter and simulations.
//!
//! Jitter does not need cryptographic quality; it needs to be seedable so that
//! tests and backtests are reproducible. SplitMix64 passes BigCrush and is ten lines.

/// SplitMix64 generator.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform float in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform integer in `[0, bound)`; returns 0 for `bound == 0`.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // Lemire's multiply-shift; the tiny bias is irrelevant for jitter.
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_seed() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn ranges_are_respected() {
        let mut r = SplitMix64::new(1);
        for _ in 0..10_000 {
            let f = r.next_f64();
            assert!((0.0..1.0).contains(&f));
            assert!(r.next_below(7) < 7);
        }
        assert_eq!(r.next_below(0), 0);
    }

    #[test]
    fn roughly_uniform() {
        let mut r = SplitMix64::new(9);
        let mut buckets = [0u32; 10];
        for _ in 0..100_000 {
            buckets[r.next_below(10) as usize] += 1;
        }
        for b in buckets {
            assert!((9_000..11_000).contains(&b), "bucket {b}");
        }
    }
}
