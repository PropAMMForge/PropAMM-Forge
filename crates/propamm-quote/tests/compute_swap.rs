//! Behavioural tests of `compute_swap` — written before the implementation (T006, TDD).
//!
//! The tests live in `tests/`, not in `src/`, on purpose: they see exactly the
//! public API the on-chain program (T017) and the router adapter (T034) will
//! see. If a test needs a private detail, a consumer will need it too, and the
//! detail has to become public.
//!
//! The numbers in the tests come from the real SOL/USDC pair, so it is visible
//! that the scales are not invented: the base has 9 decimals, the quote asset 6.

use propamm_quote::{
    compute_swap, inventory_skew_bps, is_fresh, side_price_e9, Inventory, QuoteError, QuoteParams,
    Side, SwapRequest, PRICE_SCALE,
};

/// A price of 150 USDC per SOL in raw units: 150·10⁶ of quote per 10⁹ of base
/// = 0.15 of quote per unit of base, multiplied by 1e9.
const MID_150: u128 = 150_000_000;

/// 1 SOL in raw units.
const ONE_SOL: u64 = 1_000_000_000;

/// A quote for the SOL/USDC pair: a 10 bps spread on each side, no skew.
fn params() -> QuoteParams {
    QuoteParams {
        mid_e9: MID_150,
        spread_bps: 10,
        skew_bps: 0,
        max_size_base: 10 * ONE_SOL,
        quote_slot: 1_000,
        max_quote_age_slots: 25,
        max_skew_bps: 2_000,
    }
}

/// Balanced inventory: 100 SOL and 15 000 USDC — at a price of 150 these are equal halves.
fn balanced() -> Inventory {
    Inventory {
        base_amount: 100 * ONE_SOL,
        quote_amount: 15_000_000_000,
    }
}

fn sell_base(amount_in: u64) -> SwapRequest {
    SwapRequest {
        side: Side::BaseToQuote,
        amount_in,
        min_amount_out: 0,
    }
}

fn buy_base(amount_in: u64) -> SwapRequest {
    SwapRequest {
        side: Side::QuoteToBase,
        amount_in,
        min_amount_out: 0,
    }
}

/// A slot in which the quote is still fresh.
const NOW: u64 = 1_010;

// ── Side price ─────────────────────────────────────────────────────────────

#[test]
fn bid_is_below_mid_and_ask_is_above() {
    let p = params();
    let bid = side_price_e9(&p, Side::BaseToQuote).unwrap();
    let ask = side_price_e9(&p, Side::QuoteToBase).unwrap();

    // 150_000_000 · (10000·9990)/10⁸ and · (10000·10010)/10⁸
    assert_eq!(bid, 149_850_000);
    assert_eq!(ask, 150_150_000);
    assert!(bid < p.mid_e9 && p.mid_e9 < ask);
}

#[test]
fn positive_skew_lifts_both_sides() {
    let p = QuoteParams {
        skew_bps: 100,
        ..params()
    };
    // Skew shifts the mid, not the spread: both sides move the same way.
    assert_eq!(side_price_e9(&p, Side::BaseToQuote).unwrap(), 151_348_500);
    assert_eq!(side_price_e9(&p, Side::QuoteToBase).unwrap(), 151_651_500);
}

#[test]
fn negative_skew_drops_both_sides() {
    let p = QuoteParams {
        skew_bps: -100,
        ..params()
    };
    let bid = side_price_e9(&p, Side::BaseToQuote).unwrap();
    let ask = side_price_e9(&p, Side::QuoteToBase).unwrap();
    assert!(bid < 149_850_000 && ask < 150_150_000);
}

/// The crate's main invariant at the price level: when the division is inexact,
/// bid goes down, ask goes up. Both roundings favour the vault, and there can be no
/// discrepancy here, because it is the same code location for the program and the adapter (SC-006).
#[test]
fn price_rounding_is_asymmetric_in_favor_of_vault() {
    let p = QuoteParams {
        mid_e9: 3,
        spread_bps: 1,
        ..params()
    };
    // 3·99_990_000/10⁸ = 2.9997 → 2 ; 3·100_010_000/10⁸ = 3.0003 → 4
    assert_eq!(side_price_e9(&p, Side::BaseToQuote).unwrap(), 2);
    assert_eq!(side_price_e9(&p, Side::QuoteToBase).unwrap(), 4);
}

// ── A normal swap ──────────────────────────────────────────────────────────

#[test]
fn selling_base_pays_the_bid() {
    let r = compute_swap(&params(), &balanced(), &sell_base(ONE_SOL), NOW).unwrap();

    assert_eq!(r.amount_in, ONE_SOL);
    assert_eq!(r.amount_out, 149_850_000);
    assert_eq!(r.price_e9, 149_850_000);
}

#[test]
fn buying_base_pays_the_ask() {
    let r = compute_swap(&params(), &balanced(), &buy_base(150_150_000), NOW).unwrap();

    assert_eq!(r.amount_out, ONE_SOL);
    assert_eq!(r.price_e9, 150_150_000);
}

#[test]
fn inventory_after_moves_both_legs() {
    let inv = balanced();
    let r = compute_swap(&params(), &inv, &sell_base(ONE_SOL), NOW).unwrap();

    // The trader gave base, the vault gave quote.
    assert_eq!(r.inventory_after.base_amount, inv.base_amount + ONE_SOL);
    assert_eq!(
        r.inventory_after.quote_amount,
        inv.quote_amount - 149_850_000
    );

    let r = compute_swap(&params(), &inv, &buy_base(150_150_000), NOW).unwrap();
    assert_eq!(r.inventory_after.base_amount, inv.base_amount - ONE_SOL);
    assert_eq!(
        r.inventory_after.quote_amount,
        inv.quote_amount + 150_150_000
    );
}

/// What the crate exists separately for: the adapter and the program call one
/// function, so "compute in advance" and "execute" are literally one code path.
#[test]
fn quoting_and_executing_are_the_same_call() {
    let (p, inv) = (params(), balanced());
    let quoted = compute_swap(&p, &inv, &sell_base(3 * ONE_SOL), NOW).unwrap();

    let executed = compute_swap(
        &p,
        &inv,
        &SwapRequest {
            min_amount_out: quoted.amount_out,
            ..sell_base(3 * ONE_SOL)
        },
        NOW,
    )
    .unwrap();

    assert_eq!(quoted.amount_out, executed.amount_out);
}

// ── Payout rounding ────────────────────────────────────────────────────────

#[test]
fn payout_rounds_down_when_selling_base() {
    // 7 · 149_850_000 / 1e9 = 1.049895 → 1
    let r = compute_swap(&params(), &balanced(), &sell_base(7), NOW).unwrap();
    assert_eq!(r.amount_out, 1);
}

#[test]
fn payout_rounds_down_when_buying_base() {
    // 1 · 1e9 / 150_150_000 = 6.666… → 6
    let r = compute_swap(&params(), &balanced(), &buy_base(1), NOW).unwrap();
    assert_eq!(r.amount_out, 6);
}

#[test]
fn dust_that_rounds_to_zero_is_rejected_not_confiscated() {
    // 1 · 149_850_000 / 1e9 = 0.149985 → 0. Executing such a swap would mean
    // taking the input and giving nothing back.
    assert_eq!(
        compute_swap(&params(), &balanced(), &sell_base(1), NOW),
        Err(QuoteError::AmountTooSmall)
    );
}

#[test]
fn zero_amount_in_is_rejected() {
    assert_eq!(
        compute_swap(&params(), &balanced(), &sell_base(0), NOW),
        Err(QuoteError::AmountTooSmall)
    );
}

// ── FR-007: freshness ──────────────────────────────────────────────────────

#[test]
fn quote_at_the_age_limit_is_still_fresh() {
    let p = params();
    let at_limit = p.quote_slot + u64::from(p.max_quote_age_slots);
    assert!(is_fresh(&p, at_limit));
    assert!(compute_swap(&p, &balanced(), &sell_base(ONE_SOL), at_limit).is_ok());
}

#[test]
fn quote_past_the_age_limit_is_stale() {
    let p = params();
    let past = p.quote_slot + u64::from(p.max_quote_age_slots) + 1;
    assert!(!is_fresh(&p, past));
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(ONE_SOL), past),
        Err(QuoteError::QuoteStale)
    );
}

#[test]
fn slot_before_the_quote_is_not_treated_as_infinitely_old() {
    // The chain clock is not obliged to be monotonic relative to our data;
    // a negative age is an age of zero, not an overflow.
    let p = params();
    assert!(is_fresh(&p, p.quote_slot - 5));
}

// ── FR-008: maximum size, no partial fill ──────────────────────────────────

#[test]
fn size_limit_applies_to_the_base_leg_when_selling_base() {
    let p = params();
    assert!(compute_swap(&p, &balanced(), &sell_base(p.max_size_base), NOW).is_ok());
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(p.max_size_base + 1), NOW),
        Err(QuoteError::SizeExceeded)
    );
}

#[test]
fn size_limit_applies_to_the_base_leg_when_buying_base() {
    // The bound is declared in the base asset, so for the reverse direction the output
    // is measured, not the input — otherwise the bound would mean different sizes per direction.
    let p = QuoteParams {
        max_size_base: ONE_SOL,
        ..params()
    };
    assert!(compute_swap(&p, &balanced(), &buy_base(150_150_000), NOW).is_ok());
    assert_eq!(
        compute_swap(&p, &balanced(), &buy_base(150_150_000 * 2), NOW),
        Err(QuoteError::SizeExceeded)
    );
}

// ── FR-009: the acceptable-result bound ────────────────────────────────────

#[test]
fn result_at_the_declared_limit_is_accepted() {
    let r = compute_swap(
        &params(),
        &balanced(),
        &SwapRequest {
            min_amount_out: 149_850_000,
            ..sell_base(ONE_SOL)
        },
        NOW,
    );
    assert!(r.is_ok());
}

#[test]
fn result_worse_than_the_declared_limit_is_rejected() {
    assert_eq!(
        compute_swap(
            &params(),
            &balanced(),
            &SwapRequest {
                min_amount_out: 149_850_001,
                ..sell_base(ONE_SOL)
            },
            NOW,
        ),
        Err(QuoteError::SlippageExceeded)
    );
}

// ── FR-026: the hard inventory bound ───────────────────────────────────────

#[test]
fn balanced_inventory_has_zero_skew() {
    assert_eq!(inventory_skew_bps(&balanced(), MID_150).unwrap(), 0);
}

#[test]
fn skew_sign_says_which_asset_is_in_excess() {
    let base_heavy = Inventory {
        base_amount: 100 * ONE_SOL,
        quote_amount: 0,
    };
    let quote_heavy = Inventory {
        base_amount: 0,
        quote_amount: 15_000_000_000,
    };
    assert_eq!(inventory_skew_bps(&base_heavy, MID_150).unwrap(), 10_000);
    assert_eq!(inventory_skew_bps(&quote_heavy, MID_150).unwrap(), -10_000);
}

#[test]
fn empty_inventory_has_no_skew() {
    let empty = Inventory {
        base_amount: 0,
        quote_amount: 0,
    };
    assert_eq!(inventory_skew_bps(&empty, MID_150).unwrap(), 0);
}

#[test]
fn swap_that_pushes_inventory_past_the_hard_bound_is_rejected() {
    let p = QuoteParams {
        max_skew_bps: 2_000,
        max_size_base: 1_000 * ONE_SOL,
        ..params()
    };
    // 100 SOL against 15 000 USDC is still balance; selling 90 SOL into it means
    // scooping out almost all of the quote asset.
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(90 * ONE_SOL), NOW),
        Err(QuoteError::InventoryBound)
    );
}

/// The bound limits **swaps**, not state. Inventory can end up past the bound
/// without a single swap — a move of the mid is enough. If the rule read "after
/// a swap the skew is always within bounds", a vault in that state could not
/// accept even the swap that rebalances it, and would be stuck until manual intervention.
#[test]
fn swap_that_reduces_an_already_excessive_skew_is_allowed() {
    let p = QuoteParams {
        max_skew_bps: 2_000,
        ..params()
    };
    let base_heavy = Inventory {
        base_amount: 10 * ONE_SOL,
        quote_amount: 0,
    };
    assert_eq!(inventory_skew_bps(&base_heavy, p.mid_e9).unwrap(), 10_000);

    let r = compute_swap(&p, &base_heavy, &buy_base(150_150_000), NOW).unwrap();
    assert_eq!(r.amount_out, ONE_SOL);
    assert!(r.skew_bps_after < 10_000);
    assert!(r.skew_bps_after > 2_000, "still past the bound — fine");
}

#[test]
fn swap_that_worsens_an_already_excessive_skew_is_still_rejected() {
    let p = QuoteParams {
        max_skew_bps: 2_000,
        ..params()
    };
    let base_heavy = Inventory {
        base_amount: 100 * ONE_SOL,
        quote_amount: 1_000_000_000,
    };
    // Skew is already +8750; selling base into a vault already flooded with base
    // pushes it further — exactly what the bound forbids.
    assert!(inventory_skew_bps(&base_heavy, p.mid_e9).unwrap() > 2_000);
    assert_eq!(
        compute_swap(&p, &base_heavy, &sell_base(5 * ONE_SOL), NOW),
        Err(QuoteError::InventoryBound)
    );
}

// ── Liquidity and parameter validity ───────────────────────────────────────

#[test]
fn vault_cannot_pay_more_than_it_holds() {
    let thin = Inventory {
        base_amount: 100 * ONE_SOL,
        quote_amount: 1_000,
    };
    let p = QuoteParams {
        max_skew_bps: 10_000,
        ..params()
    };
    assert_eq!(
        compute_swap(&p, &thin, &sell_base(ONE_SOL), NOW),
        Err(QuoteError::InsufficientLiquidity)
    );
}

#[test]
fn unset_quote_does_not_trade_at_zero() {
    let p = QuoteParams {
        mid_e9: 0,
        ..params()
    };
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(ONE_SOL), NOW),
        Err(QuoteError::QuoteNotSet)
    );
    assert_eq!(
        side_price_e9(&p, Side::BaseToQuote),
        Err(QuoteError::QuoteNotSet)
    );
}

#[test]
fn spread_wide_enough_to_invert_the_bid_is_invalid() {
    let p = QuoteParams {
        spread_bps: 10_000,
        ..params()
    };
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(ONE_SOL), NOW),
        Err(QuoteError::InvalidParams)
    );
}

#[test]
fn skew_deep_enough_to_invert_the_mid_is_invalid() {
    let p = QuoteParams {
        skew_bps: -10_000,
        ..params()
    };
    assert_eq!(
        compute_swap(&p, &balanced(), &sell_base(ONE_SOL), NOW),
        Err(QuoteError::InvalidParams)
    );
}

#[test]
fn extreme_price_and_size_overflow_instead_of_wrapping() {
    let p = QuoteParams {
        mid_e9: u128::MAX / 2,
        spread_bps: 0,
        skew_bps: 0,
        max_size_base: u64::MAX,
        max_skew_bps: 10_000,
        ..params()
    };
    let inv = Inventory {
        base_amount: u64::MAX,
        quote_amount: u64::MAX,
    };
    assert_eq!(
        compute_swap(&p, &inv, &sell_base(u64::MAX), NOW),
        Err(QuoteError::Overflow)
    );
}

#[test]
fn price_scale_is_one_e9() {
    assert_eq!(PRICE_SCALE, 1_000_000_000);
}
