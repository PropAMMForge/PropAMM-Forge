//! PropAMM quote math.
//!
//! The crate deliberately depends neither on Solana nor on std: it is built the
//! same way by the BPF compiler inside the on-chain program, by the host compiler
//! inside the router adapter and by the test run. One implementation for three
//! consumers is what makes quote/execution parity (SC-006) a property of the
//! construction rather than of developer discipline.
//!
//! # Scales and units
//!
//! All amounts are **raw token units**, as the token program sees them; the crate
//! knows nothing about decimals. Accordingly `mid_e9` is the price in raw units of
//! the quote asset per one raw unit of the base asset, multiplied by [`PRICE_SCALE`].
//! For the pair SOL(9)/USDC(6) at 150 USDC that is `0.15 · 1e9 = 150_000_000`.
//!
//! Converting human prices into this scale is the engine's job (FR-013), and
//! deliberately not this crate's: otherwise every consumer would have to know the
//! mints' decimals, and reading them on chain would cost two extra accounts in the SC-002 budget.
//!
//! # The rounding invariant
//!
//! **Every rounding is in the vault's favour.** There are exactly two places where
//! it arises at all, and both are here:
//!
//! 1. the side price ([`side_price_e9`]) — bid down, ask up;
//! 2. the amount paid out ([`compute_swap`]) — always down.
//!
//! There can be no discrepancy between the router and the chain, because this is
//! literally the same code. The invariant is pinned by a property test (T007).
//!
//! # What the crate does NOT check
//!
//! Only what can be derived from the quote parameters and the inventory. Checks
//! that need chain or account state stay outside the crate: the emergency-halt
//! flag `halted` (FR-024), the `pricing_authority` signature (FR-010), token
//! account ownership and Token-2022 extensions (FR-005).
//! A [`compute_swap`] call that returned `Ok` **does not mean** the swap is allowed.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]

use core::fmt;

/// Basis-point denominator.
pub const BPS_DENOM: u16 = 10_000;

/// Fixed-point scale for the price.
pub const PRICE_SCALE: u128 = 1_000_000_000;

/// Denominator of the product of two bps factors — skew and spread.
const FACTOR_DENOM: u128 = (BPS_DENOM as u128) * (BPS_DENOM as u128);

/// Parameters of the current quote — the same thing that sits in the `Vault` account.
///
/// There is no "per side" price in the state (FR-006): both sides are derived from
/// here deterministically, so the router reproduces them from the same account
/// bytes without looking anywhere else (FR-006a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteParams {
    /// Market mid in fixed point [`PRICE_SCALE`]. Zero means the quote is not
    /// posted — not a price of zero.
    pub mid_e9: u128,
    /// Half of the spread in basis points: bid and ask each move away from the mid
    /// by this amount.
    pub spread_bps: u16,
    /// Mid shift by inventory skew (FR-013). Positive raises both sides — that is
    /// how the vault accumulates the base asset; negative does the opposite.
    pub skew_bps: i16,
    /// Maximum order size in the base asset (FR-008).
    pub max_size_base: u64,
    /// Slot in which the quote was posted — the source of its age (FR-007).
    pub quote_slot: u64,
    /// Freshness limit in slots (FR-007).
    pub max_quote_age_slots: u32,
    /// Hard bound on inventory skew in basis points (FR-026).
    pub max_skew_bps: u16,
}

/// Vault holdings in raw units of both assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inventory {
    pub base_amount: u64,
    pub quote_amount: u64,
}

/// Swap direction, named from the **trader's** side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The trader gives the base asset and receives the quote asset; the vault buys base at bid.
    BaseToQuote,
    /// The trader gives the quote asset and receives the base asset; the vault sells base at ask.
    QuoteToBase,
}

/// A swap order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapRequest {
    pub side: Side,
    /// Amount in, in raw units of the asset the trader gives.
    pub amount_in: u64,
    /// The initiator's bound on the acceptable result (FR-009). For a quote with no
    /// intent to execute — zero.
    pub min_amount_out: u64,
}

/// Result of the computation. There is no partial fill (FR-008): either the whole
/// order, or an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapResult {
    pub amount_in: u64,
    pub amount_out: u64,
    /// The side price the swap was computed at, in the scale of [`PRICE_SCALE`].
    pub price_e9: u128,
    pub inventory_after: Inventory,
    /// Inventory skew after the swap — for the engine (FR-013), for the console (FR-023).
    pub skew_bps_after: i32,
}

/// Why the swap cannot be computed or is not allowed.
///
/// The on-chain program maps these variants onto its own error codes (T017);
/// the router adapter treats any of them as "the venue is not quoting".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// The mid is zero — the quote is not posted or was cleared.
    QuoteNotSet,
    /// The spread or the skew takes the side price out of the meaningful range.
    InvalidParams,
    /// The quote is older than `max_quote_age_slots` (FR-007).
    QuoteStale,
    /// The order's base leg exceeds `max_size_base` (FR-008).
    SizeExceeded,
    /// The result is worse than the declared bound (FR-009).
    SlippageExceeded,
    /// The swap would take the inventory past the hard bound (FR-026).
    InventoryBound,
    /// The vault does not hold as much of the asset as it has to pay out.
    InsufficientLiquidity,
    /// The input is zero or so small that the output rounds to zero.
    AmountTooSmall,
    /// An intermediate value does not fit its type.
    Overflow,
}

impl fmt::Display for QuoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::QuoteNotSet => "quote is not set",
            Self::InvalidParams => "quote parameters are out of domain",
            Self::QuoteStale => "quote is older than the freshness limit",
            Self::SizeExceeded => "base leg exceeds the maximum quoted size",
            Self::SlippageExceeded => "result is worse than the declared limit",
            Self::InventoryBound => "swap would push inventory past the hard bound",
            Self::InsufficientLiquidity => "vault cannot pay out that amount",
            Self::AmountTooSmall => "amount rounds to zero",
            Self::Overflow => "intermediate value overflowed",
        };
        f.write_str(text)
    }
}

/// Whether the quote is still fresh as of `current_slot` (FR-007).
///
/// A slot smaller than `quote_slot` gives an age of zero, not an overflow: the
/// chain clock is not obliged to be monotonic relative to the data we read.
#[must_use]
pub fn is_fresh(params: &QuoteParams, current_slot: u64) -> bool {
    current_slot.saturating_sub(params.quote_slot) <= u64::from(params.max_quote_age_slots)
}

/// Side price in the scale of [`PRICE_SCALE`]: bid for [`Side::BaseToQuote`],
/// ask for [`Side::QuoteToBase`].
///
/// The rounding is asymmetric and reliable for that very reason: bid down, ask
/// up — both in the vault's favour.
pub fn side_price_e9(params: &QuoteParams, side: Side) -> Result<u128, QuoteError> {
    if params.mid_e9 == 0 {
        return Err(QuoteError::QuoteNotSet);
    }
    let factor = side_factor(params, side)?;
    match side {
        Side::BaseToQuote => mul_div_floor(params.mid_e9, factor, FACTOR_DENOM),
        Side::QuoteToBase => mul_div_ceil(params.mid_e9, factor, FACTOR_DENOM),
    }
}

/// Inventory skew in basis points: positive — too much base asset, negative —
/// too much quote asset, zero — balance at the current mid.
///
/// Both legs are valued at `mid_e9`, not at the quote sides: otherwise the same
/// position would give a different skew depending on the direction it is asked
/// about.
pub fn inventory_skew_bps(inventory: &Inventory, mid_e9: u128) -> Result<i32, QuoteError> {
    let base_value = u128::from(inventory.base_amount)
        .checked_mul(mid_e9)
        .ok_or(QuoteError::Overflow)?;
    let quote_value = u128::from(inventory.quote_amount)
        .checked_mul(PRICE_SCALE)
        .ok_or(QuoteError::Overflow)?;
    let total = base_value
        .checked_add(quote_value)
        .ok_or(QuoteError::Overflow)?;
    if total == 0 {
        return Ok(0);
    }

    let base_value = i128::try_from(base_value).map_err(|_| QuoteError::Overflow)?;
    let quote_value = i128::try_from(quote_value).map_err(|_| QuoteError::Overflow)?;
    let total = i128::try_from(total).map_err(|_| QuoteError::Overflow)?;

    let skew = base_value
        .checked_sub(quote_value)
        .and_then(|diff| diff.checked_mul(i128::from(BPS_DENOM)))
        .and_then(|scaled| scaled.checked_div(total))
        .ok_or(QuoteError::Overflow)?;
    i32::try_from(skew).map_err(|_| QuoteError::Overflow)
}

/// Compute a swap at the current quote.
///
/// One function both for "how much would I get" (the router, `min_amount_out = 0`)
/// and for "execute" (the program) — which is exactly why the quote cannot diverge
/// from execution on rounding (SC-006).
///
/// Checked, in this order: the parameter domain, freshness (FR-007), non-zero
/// input, maximum size (FR-008), non-zero output, availability of the asset to
/// pay out, the result bound (FR-009), the hard inventory bound (FR-026). Checks
/// that need state beyond these inputs stay with the caller — see the crate
/// documentation.
pub fn compute_swap(
    params: &QuoteParams,
    inventory: &Inventory,
    request: &SwapRequest,
    current_slot: u64,
) -> Result<SwapResult, QuoteError> {
    let price_e9 = side_price_e9(params, request.side)?;

    if !is_fresh(params, current_slot) {
        return Err(QuoteError::QuoteStale);
    }
    if request.amount_in == 0 {
        return Err(QuoteError::AmountTooSmall);
    }

    let amount_in = u128::from(request.amount_in);
    let amount_out = match request.side {
        Side::BaseToQuote => mul_div_floor(amount_in, price_e9, PRICE_SCALE)?,
        // `side_price_e9` rounds the ask up, so with a non-zero mid it is at least
        // one and the division is defined.
        Side::QuoteToBase => mul_div_floor(amount_in, PRICE_SCALE, price_e9)?,
    };
    let amount_out = u64::try_from(amount_out).map_err(|_| QuoteError::Overflow)?;

    // FR-008: the bound is declared in the base asset, so the base leg is measured,
    // not the input. Otherwise the same bound would mean different sizes per direction.
    let base_leg = match request.side {
        Side::BaseToQuote => request.amount_in,
        Side::QuoteToBase => amount_out,
    };
    if base_leg > params.max_size_base {
        return Err(QuoteError::SizeExceeded);
    }

    // An output that rounded to zero is "take the input and give nothing back".
    if amount_out == 0 {
        return Err(QuoteError::AmountTooSmall);
    }

    let inventory_after = match request.side {
        Side::BaseToQuote => Inventory {
            base_amount: inventory
                .base_amount
                .checked_add(request.amount_in)
                .ok_or(QuoteError::Overflow)?,
            quote_amount: inventory
                .quote_amount
                .checked_sub(amount_out)
                .ok_or(QuoteError::InsufficientLiquidity)?,
        },
        Side::QuoteToBase => Inventory {
            base_amount: inventory
                .base_amount
                .checked_sub(amount_out)
                .ok_or(QuoteError::InsufficientLiquidity)?,
            quote_amount: inventory
                .quote_amount
                .checked_add(request.amount_in)
                .ok_or(QuoteError::Overflow)?,
        },
    };

    if amount_out < request.min_amount_out {
        return Err(QuoteError::SlippageExceeded);
    }

    let skew_before = inventory_skew_bps(inventory, params.mid_e9)?;
    let skew_bps_after = inventory_skew_bps(&inventory_after, params.mid_e9)?;
    if exceeds_inventory_bound(skew_before, skew_bps_after, params.max_skew_bps) {
        return Err(QuoteError::InventoryBound);
    }

    Ok(SwapResult {
        amount_in: request.amount_in,
        amount_out,
        price_e9,
        inventory_after,
        skew_bps_after,
    })
}

/// The FR-026 bound limits **swaps**, not state.
///
/// Inventory can end up past the bound without a single swap — a move of the mid
/// is enough. If the rule read "after a swap the skew is always within bounds",
/// a vault in that state would not accept even the swap that rebalances it, and
/// would be stuck until manual intervention — i.e. the risk bound would itself create risk.
/// So a swap is forbidden only when it takes the inventory further **both** past
/// the bound **and** past where it already was.
fn exceeds_inventory_bound(before: i32, after: i32, max_skew_bps: u16) -> bool {
    let after = after.unsigned_abs();
    after > u32::from(max_skew_bps) && after > before.unsigned_abs()
}

fn side_factor(params: &QuoteParams, side: Side) -> Result<u128, QuoteError> {
    let denom = i32::from(BPS_DENOM);

    let skewed = denom
        .checked_add(i32::from(params.skew_bps))
        .ok_or(QuoteError::Overflow)?;
    if skewed <= 0 {
        return Err(QuoteError::InvalidParams);
    }

    let spread = i32::from(params.spread_bps);
    if spread >= denom {
        return Err(QuoteError::InvalidParams);
    }
    let sided = match side {
        Side::BaseToQuote => denom.checked_sub(spread),
        Side::QuoteToBase => denom.checked_add(spread),
    }
    .ok_or(QuoteError::Overflow)?;

    let skewed = u128::try_from(skewed).map_err(|_| QuoteError::Overflow)?;
    let sided = u128::try_from(sided).map_err(|_| QuoteError::Overflow)?;
    skewed.checked_mul(sided).ok_or(QuoteError::Overflow)
}

fn mul_div_floor(a: u128, b: u128, denominator: u128) -> Result<u128, QuoteError> {
    a.checked_mul(b)
        .and_then(|product| product.checked_div(denominator))
        .ok_or(QuoteError::Overflow)
}

fn mul_div_ceil(a: u128, b: u128, denominator: u128) -> Result<u128, QuoteError> {
    let product = a.checked_mul(b).ok_or(QuoteError::Overflow)?;
    denominator
        .checked_sub(1)
        .and_then(|bump| product.checked_add(bump))
        .and_then(|raised| raised.checked_div(denominator))
        .ok_or(QuoteError::Overflow)
}
