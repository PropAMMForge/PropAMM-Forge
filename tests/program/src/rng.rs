//! A deterministic generator for the SC-004 campaign.
//!
//! Fifteen lines of our own instead of a dependency: the campaign needs not
//! "random" but **reproducible** — a thousand swaps that are the same on every
//! machine and in every run. A broken run has to reproduce from the step
//! number, not from a regression file.
//!
//! `proptest` is unsuitable here for another reason: each of its cases is a real
//! BPF execution on state the cases change for one another, and shrinking on
//! such state narrows nothing.

/// xorshift64 — exactly as much as is needed to spread intents over the steps.
pub struct Xorshift(u64);

impl Xorshift {
    /// # Panics
    ///
    /// A zero seed in xorshift yields an endless zero — that is not "a bad seed"
    /// but a stopped generator, and staying silent about it is not an option.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        assert!(seed != 0, "a zero seed stops xorshift");
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `[0, bound)`.
    ///
    /// # Panics
    ///
    /// At `bound == 0` — the remainder of zero is undefined.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "empty range");
        self.next_u64() % bound
    }

    /// A number in `[low, high]`.
    ///
    /// # Panics
    ///
    /// At `low > high`.
    pub fn between(&mut self, low: u64, high: u64) -> u64 {
        assert!(low <= high, "inverted range");
        low + self.below(high - low + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed — the same sequence. Without this a 1 000-step campaign
    /// would not be evidence: "green" would mean "got lucky this time".
    #[test]
    fn the_same_seed_gives_the_same_run() {
        let take = |seed| {
            let mut rng = Xorshift::new(seed);
            (0..16).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(take(0x5EED_1234_ABCD_0001), take(0x5EED_1234_ABCD_0001));
        assert_ne!(take(0x5EED_1234_ABCD_0001), take(0x5EED_1234_ABCD_0002));
    }

    /// The bounds are inclusive on both sides — "exactly 200 stale" in the
    /// campaign rests on that.
    #[test]
    fn the_range_covers_both_ends() {
        let mut rng = Xorshift::new(7);
        let mut low = false;
        let mut high = false;
        for _ in 0..2_000 {
            let value = rng.between(10, 12);
            assert!((10..=12).contains(&value));
            low |= value == 10;
            high |= value == 12;
        }
        assert!(low && high, "the range does not reach its own ends");
    }
}
