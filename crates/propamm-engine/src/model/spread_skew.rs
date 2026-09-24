//! The built-in model: a spread that widens and a mid that shifts with the
//! inventory skew (FR-013).
//!
//! # What the vault gives the model to work with
//!
//! The account carries one mid, one half-spread and one skew (FR-006): there is
//! no per-side price to set, because the router has to reproduce both sides from
//! the same bytes (FR-006a). So a model has exactly two levers:
//!
//! - `skew_bps` moves **both** sides together. That is the steering: shift the
//!   mid away from the asset the vault is long of, and the side that unwinds the
//!   position becomes the attractive one.
//! - `spread_bps` moves the sides **apart**. That is the brake: every trade gets
//!   more expensive, the unwinding one included.
//!
//! # Why the steering has to beat the brake
//!
//! Both levers are applied to the same side price — `side_price_e9` multiplies
//! by `(1 + skew)(1 ± spread)` — so as the inventory gets worse, the widening
//! pushes the unwinding side the wrong way. If the brake grew faster than the
//! steering, the vault would end up quoting a *worse* unwind price the more it
//! needed the unwind, and the inventory would never come back. That is not a
//! tuning preference but the difference between a model that satisfies FR-013
//! and one that only looks like it does, so [`SpreadSkewModel::checked`] refuses
//! a configuration where the widening is not smaller than the shift.
//!
//! # The scale everything is measured on
//!
//! Both levers grow linearly with how far the inventory has travelled toward the
//! vault's own hard bound `max_skew_bps` (FR-026), which the on-chain program
//! enforces anyway. At a balanced inventory the model quotes the base spread and
//! no shift; at the bound it quotes the widest spread and the full shift; past
//! the bound — the same, clamped. Tying the model to the same number the program
//! guards means the two cannot disagree about what "too far" is.

use propamm_quote::{inventory_skew_bps, Inventory, QuoteError, BPS_DENOM, PRICE_SCALE};
use thiserror::Error;

/// What the core hands the model (FR-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketState {
    /// The mid from the feed, in [`PRICE_SCALE`]: raw quote per raw base × 1e9.
    pub mid_e9: u128,
    /// What the vault holds right now.
    pub inventory: Inventory,
    /// The vault's hard inventory bound (FR-026) — the scale skew is measured against.
    pub max_skew_bps: u16,
}

/// What the model gives back: the four fields of the quote the engine may move.
///
/// The other fields of `QuoteParams` are the deployment's, not the model's: the
/// freshness limit and the hard skew bound are risk settings the owner sets, and
/// `quote_slot` is the chain's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// The market mid, in [`PRICE_SCALE`].
    pub mid_e9: u128,
    /// Half of the spread, in basis points.
    pub spread_bps: u16,
    /// Mid shift by inventory skew: positive raises both sides.
    pub skew_bps: i16,
    /// Largest order the vault will take, in raw units of the base asset (FR-008).
    pub max_size_base: u64,
}

/// A model configuration that cannot do its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ModelConfigError {
    #[error("the base half-spread of {base_bps} bps is wider than the maximum of {max_bps} bps")]
    BaseSpreadAboveMax { base_bps: u16, max_bps: u16 },
    #[error("a half-spread of {bps} bps is not a spread: at a whole 10 000 bps the bid turns non-positive")]
    SpreadNotBelowWhole { bps: u16 },
    #[error("a mid shift of {bps} bps is out of range: at a whole 10 000 bps the shifted mid turns non-positive")]
    ShiftNotBelowWhole { bps: u16 },
    #[error(
        "the mid shift of {shift_bps} bps does not exceed the widening of {widening_bps} bps: \
         the price of unwinding the inventory would get worse as the inventory got worse, and \
         the skew would never come back"
    )]
    ShiftDoesNotBeatWidening { shift_bps: u16, widening_bps: u16 },
    #[error("the size fraction must be between 1 and 10 000 bps, not {bps}")]
    SizeFractionOutOfRange { bps: u16 },
}

/// Spread and skew as linear functions of the inventory skew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpreadSkewModel {
    base_spread_bps: u16,
    max_spread_bps: u16,
    max_skew_shift_bps: u16,
    size_fraction_bps: u16,
}

impl SpreadSkewModel {
    /// Build a model, refusing a configuration that cannot steer the inventory back.
    ///
    /// - `base_spread_bps` — the half-spread at a balanced inventory;
    /// - `max_spread_bps` — the half-spread once the inventory sits on the hard bound;
    /// - `max_skew_shift_bps` — the mid shift there;
    /// - `size_fraction_bps` — the share of the payable side offered as one order.
    ///
    /// # Errors
    ///
    /// [`ModelConfigError`] — a spread or a shift outside its domain, a size
    /// fraction outside `1..=BPS_DENOM`, or a widening the shift does not beat
    /// (see the module docs).
    pub fn checked(
        base_spread_bps: u16,
        max_spread_bps: u16,
        max_skew_shift_bps: u16,
        size_fraction_bps: u16,
    ) -> Result<Self, ModelConfigError> {
        if max_spread_bps >= BPS_DENOM {
            return Err(ModelConfigError::SpreadNotBelowWhole {
                bps: max_spread_bps,
            });
        }
        if base_spread_bps > max_spread_bps {
            return Err(ModelConfigError::BaseSpreadAboveMax {
                base_bps: base_spread_bps,
                max_bps: max_spread_bps,
            });
        }
        if max_skew_shift_bps >= BPS_DENOM {
            return Err(ModelConfigError::ShiftNotBelowWhole {
                bps: max_skew_shift_bps,
            });
        }
        let widening_bps = max_spread_bps - base_spread_bps;
        if max_skew_shift_bps <= widening_bps {
            return Err(ModelConfigError::ShiftDoesNotBeatWidening {
                shift_bps: max_skew_shift_bps,
                widening_bps,
            });
        }
        if size_fraction_bps == 0 || size_fraction_bps > BPS_DENOM {
            return Err(ModelConfigError::SizeFractionOutOfRange {
                bps: size_fraction_bps,
            });
        }
        Ok(Self {
            base_spread_bps,
            max_spread_bps,
            max_skew_shift_bps,
            size_fraction_bps,
        })
    }

    /// The half-spread quoted at a balanced inventory.
    #[must_use]
    pub const fn base_spread_bps(&self) -> u16 {
        self.base_spread_bps
    }

    /// The half-spread quoted at the hard inventory bound.
    #[must_use]
    pub const fn max_spread_bps(&self) -> u16 {
        self.max_spread_bps
    }

    /// The mid shift at the hard inventory bound.
    #[must_use]
    pub const fn max_skew_shift_bps(&self) -> u16 {
        self.max_skew_shift_bps
    }

    /// The share of the payable side offered as one order.
    #[must_use]
    pub const fn size_fraction_bps(&self) -> u16 {
        self.size_fraction_bps
    }

    /// Price the current state.
    ///
    /// # Errors
    ///
    /// [`QuoteError::QuoteNotSet`] if the feed gave no mid — a zero mid is the
    /// absence of a price, not a price of zero; [`QuoteError::InvalidParams`] if
    /// the vault's hard skew bound is zero, which leaves nothing to measure the
    /// skew against; [`QuoteError::Overflow`] on an intermediate product.
    pub fn quote(&self, state: &MarketState) -> Result<Quote, QuoteError> {
        if state.mid_e9 == 0 {
            return Err(QuoteError::QuoteNotSet);
        }
        if state.max_skew_bps == 0 {
            return Err(QuoteError::InvalidParams);
        }

        let skew_bps = inventory_skew_bps(&state.inventory, state.mid_e9)?;
        // How far the inventory has travelled toward the hard bound, in bps of
        // the way there. An inventory already past the bound — a limit tightened
        // under a position — is the same thing as sitting on it.
        let bound = i32::from(state.max_skew_bps);
        let load = skew_bps
            .checked_abs()
            .ok_or(QuoteError::Overflow)?
            .min(bound)
            .checked_mul(i32::from(BPS_DENOM))
            .ok_or(QuoteError::Overflow)?
            / bound;

        let widening = i32::from(self.max_spread_bps - self.base_spread_bps);
        let spread = i32::from(self.base_spread_bps) + widening * load / i32::from(BPS_DENOM);
        let shift = i32::from(self.max_skew_shift_bps) * load / i32::from(BPS_DENOM);
        // Against the skew: too much base asset is corrected by lowering both
        // sides, which is what makes the vault the cheap place to buy base.
        let shift = if skew_bps > 0 { -shift } else { shift };

        Ok(Quote {
            mid_e9: state.mid_e9,
            // Both fit by construction: the shift is below BPS_DENOM and the
            // spread is between the two configured ones, both u16.
            spread_bps: u16::try_from(spread).map_err(|_| QuoteError::Overflow)?,
            skew_bps: i16::try_from(shift).map_err(|_| QuoteError::Overflow)?,
            max_size_base: self.size(state)?,
        })
    }

    /// The order size: a share of whichever side the vault would run out of first.
    ///
    /// `max_size_base` is one number for both directions (FR-008), so it has to
    /// hold for the direction that pays out base *and* the one that pays out
    /// quote — hence the smaller of the two, with the quote side valued at the
    /// mid. An empty vault gives zero, which is the honest answer: no order
    /// passes the size check, and the quote stands with nothing behind it.
    fn size(&self, state: &MarketState) -> Result<u64, QuoteError> {
        let Inventory {
            base_amount,
            quote_amount,
        } = state.inventory;
        let quote_as_base = u128::from(quote_amount)
            .checked_mul(PRICE_SCALE)
            .ok_or(QuoteError::Overflow)?
            / state.mid_e9;
        let payable = u128::from(base_amount).min(quote_as_base);
        let size = payable
            .checked_mul(u128::from(self.size_fraction_bps))
            .ok_or(QuoteError::Overflow)?
            / u128::from(BPS_DENOM);
        Ok(u64::try_from(size).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use propamm_quote::{side_price_e9, QuoteParams, Side};

    use super::*;

    /// 150 USDC per SOL — the same example `forge quote` and [`crate::feed::mid_e9`] use.
    const MID: u128 = 150_000_000;
    /// The vault's hard bound, as `.env.example` sets it.
    const BOUND: u16 = 2_000;

    /// 10 bps at balance, 40 at the bound, a 150 bps shift there, 5 % of the
    /// payable side per order.
    fn model() -> SpreadSkewModel {
        SpreadSkewModel::checked(10, 40, 150, 500).expect("a workable configuration")
    }

    fn state(base_amount: u64, quote_amount: u64) -> MarketState {
        MarketState {
            mid_e9: MID,
            inventory: Inventory {
                base_amount,
                quote_amount,
            },
            max_skew_bps: BOUND,
        }
    }

    /// 1 000 SOL and 150 000 USDC value the same at the mid: skew zero.
    fn balanced() -> MarketState {
        state(1_000_000_000_000, 150_000_000_000)
    }

    /// 1 000 SOL and 100 000 USDC: skew exactly +2 000 bps, the hard bound.
    fn at_the_bound() -> MarketState {
        state(1_000_000_000_000, 100_000_000_000)
    }

    /// The model's output as the vault would hold it.
    fn params(quote: &Quote) -> QuoteParams {
        QuoteParams {
            mid_e9: quote.mid_e9,
            spread_bps: quote.spread_bps,
            skew_bps: quote.skew_bps,
            max_size_base: quote.max_size_base,
            quote_slot: 1_000,
            max_quote_age_slots: 25,
            max_skew_bps: BOUND,
        }
    }

    #[test]
    fn a_balanced_inventory_gets_the_base_spread_and_no_shift() {
        let quote = model().quote(&balanced()).expect("a priceable state");
        assert_eq!(quote.mid_e9, MID, "the model does not move the feed's mid");
        assert_eq!(quote.spread_bps, 10);
        assert_eq!(quote.skew_bps, 0);
        // Both sides are worth 1 000 SOL at the mid; 5 % of that is 50 SOL.
        assert_eq!(quote.max_size_base, 50_000_000_000);
    }

    #[test]
    fn too_much_base_lowers_both_sides_and_too_much_quote_raises_them() {
        let long_base = model()
            .quote(&state(1_200_000_000_000, 150_000_000_000))
            .expect("a priceable state");
        let long_quote = model()
            .quote(&state(1_000_000_000_000, 180_000_000_000))
            .expect("a priceable state");
        assert!(
            long_base.skew_bps < 0,
            "sitting on base, the vault has to become the cheap place to buy it: {long_base:?}"
        );
        assert!(
            long_quote.skew_bps > 0,
            "and the expensive place to sell it: {long_quote:?}"
        );
        assert!(long_base.spread_bps > 10 && long_quote.spread_bps > 10);
    }

    #[test]
    fn at_the_hard_bound_both_levers_are_at_their_maximum() {
        let quote = model().quote(&at_the_bound()).expect("a priceable state");
        assert_eq!(quote.spread_bps, 40);
        assert_eq!(quote.skew_bps, -150);
        // The quote side is the one that runs out first: 100 000 USDC is
        // 666.66 SOL at the mid, and 5 % of that is 33.33 SOL.
        assert_eq!(quote.max_size_base, 33_333_333_333);
    }

    #[test]
    fn past_the_hard_bound_the_levers_stop_but_the_size_keeps_shrinking() {
        let past = model()
            .quote(&state(1_000_000_000_000, 50_000_000_000))
            .expect("a priceable state");
        let at = model().quote(&at_the_bound()).expect("a priceable state");
        assert_eq!(
            (past.spread_bps, past.skew_bps),
            (at.spread_bps, at.skew_bps),
            "the bound is where the levers end"
        );
        assert!(
            past.max_size_base < at.max_size_base,
            "but there is less left to pay with: {past:?}"
        );
    }

    #[test]
    fn the_unwind_price_improves_as_the_inventory_gets_worse() {
        // This is FR-013 itself: the worse the skew, the more attractive the
        // trade that undoes it. The vault is long base, so the unwinding trade
        // is the trader buying base — the ask — and it has to fall throughout,
        // widening spread and all.
        //
        // The walk stops at 100 000 USDC, which is the hard bound: past it both
        // levers are clamped by design and the ask stops moving, which is what
        // `past_the_hard_bound_the_levers_stop_but_the_size_keeps_shrinking` pins.
        let model = model();
        let mut previous = u128::MAX;
        for quote_amount in [150_000, 130_000, 110_000, 100_000] {
            let quote = model
                .quote(&state(1_000_000_000_000, quote_amount * 1_000_000))
                .expect("a priceable state");
            let ask = side_price_e9(&params(&quote), Side::QuoteToBase).expect("a valid quote");
            assert!(
                ask < previous,
                "the ask did not improve at {quote_amount} USDC: {ask} vs {previous} ({quote:?})"
            );
            previous = ask;
        }
    }

    #[test]
    fn the_size_takes_the_side_that_runs_out_first() {
        // Plenty of base, almost no quote: the vault can hand out 10 SOL worth
        // of USDC, so that is what bounds the order, not the 100 000 SOL it holds.
        let quote = model()
            .quote(&state(100_000_000_000_000, 1_500_000_000))
            .expect("a priceable state");
        assert_eq!(quote.max_size_base, 500_000_000, "5 % of 10 SOL");
    }

    #[test]
    fn an_empty_vault_quotes_a_zero_size_rather_than_failing() {
        let quote = model().quote(&state(0, 0)).expect("a priceable state");
        assert_eq!(quote.max_size_base, 0);
        assert_eq!(quote.skew_bps, 0, "nothing held is nothing skewed");
    }

    #[test]
    fn without_a_mid_or_a_bound_there_is_nothing_to_price() {
        let mut no_mid = balanced();
        no_mid.mid_e9 = 0;
        assert_eq!(model().quote(&no_mid), Err(QuoteError::QuoteNotSet));

        let mut no_bound = balanced();
        no_bound.max_skew_bps = 0;
        assert_eq!(model().quote(&no_bound), Err(QuoteError::InvalidParams));
    }

    #[test]
    fn a_configuration_that_cannot_steer_the_inventory_back_is_refused() {
        // A 30 bps widening against a 30 bps shift: at the bound the two cancel
        // and the unwind price stops improving. That is the model quietly not
        // doing its job, so it is refused rather than tuned.
        assert_eq!(
            SpreadSkewModel::checked(10, 40, 30, 500),
            Err(ModelConfigError::ShiftDoesNotBeatWidening {
                shift_bps: 30,
                widening_bps: 30
            })
        );
        assert_eq!(
            SpreadSkewModel::checked(50, 40, 150, 500),
            Err(ModelConfigError::BaseSpreadAboveMax {
                base_bps: 50,
                max_bps: 40
            })
        );
        assert_eq!(
            SpreadSkewModel::checked(10, 10_000, 150, 500),
            Err(ModelConfigError::SpreadNotBelowWhole { bps: 10_000 })
        );
        assert_eq!(
            SpreadSkewModel::checked(10, 40, 10_000, 500),
            Err(ModelConfigError::ShiftNotBelowWhole { bps: 10_000 })
        );
        assert_eq!(
            SpreadSkewModel::checked(10, 40, 150, 0),
            Err(ModelConfigError::SizeFractionOutOfRange { bps: 0 })
        );
        assert_eq!(
            SpreadSkewModel::checked(10, 40, 150, 10_001),
            Err(ModelConfigError::SizeFractionOutOfRange { bps: 10_001 })
        );
    }

    #[test]
    fn the_output_is_a_quote_the_program_and_the_router_accept() {
        // The model is only useful if its numbers survive the same functions the
        // on-chain swap runs (SC-006): the domain checks in `side_factor` are the
        // ones that would fire on a shift or a spread out of range.
        for inventory in [balanced(), at_the_bound(), state(1_500_000_000_000, 0)] {
            let quote = model().quote(&inventory).expect("a priceable state");
            let params = params(&quote);
            let bid = side_price_e9(&params, Side::BaseToQuote).expect("a valid bid");
            let ask = side_price_e9(&params, Side::QuoteToBase).expect("a valid ask");
            assert!(bid < ask, "the sides crossed: {bid} / {ask} ({quote:?})");
        }
    }
}
