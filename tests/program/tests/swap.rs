//! `swap` on live accounts (T017, FR-007, FR-008, FR-009, FR-026).
//!
//! T017 was closed with a caveat: "guards on live accounts — T020". Here they
//! are. Every guard is checked separately and on a state where the other guards
//! deliberately pass — otherwise the test would prove only that something refused.
//!
//! # Numbers computed by hand
//!
//! The happy path is checked against a number derived on paper, not via
//! `compute_swap`: the same code on both sides of the equality would prove only
//! that it equals itself. The other cases use `compute_swap` as an oracle on
//! purpose — there it is not the arithmetic that is checked, but that the program
//! moves exactly the amount the arithmetic named.

use anchor_lang::error::ErrorCode;
use propamm_vault::errors::VaultError;
use propamm_vault::events::Swapped;
use propamm_vault::instructions::authority::RiskLimits;
use propamm_vault::instructions::swap::SwapSide;
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault_tests::world::{
    MAX_QUOTE_AGE_SLOTS, MAX_SIZE_BASE, MID_E9, SPREAD_BPS, TREASURY_BASE, TREASURY_QUOTE,
};
use propamm_vault_tests::{ix, World};

/// One base asset at bid = `mid · (1 − 10 bps)`:
/// `floor(150_000_000 · 9_990 · 10_000 / 1e8) = 149_850_000`.
const ONE_BASE: u64 = 1_000_000_000;
const ONE_BASE_PAYS: u64 = 149_850_000;

#[test]
fn a_trader_gives_base_and_receives_quote() {
    let mut world = World::ready();
    let trader_base_before = world.balance(&world.trader_base);
    let trader_quote_before = world.balance(&world.trader_quote);

    let outcome = world.swap(SwapSide::BaseToQuote, ONE_BASE, 0);
    outcome.expect_ok();

    assert_eq!(
        world.balance(&world.trader_base),
        trader_base_before - ONE_BASE
    );
    assert_eq!(
        world.balance(&world.trader_quote),
        trader_quote_before + ONE_BASE_PAYS
    );
    assert_eq!(
        world.balance(&world.base_treasury),
        TREASURY_BASE + ONE_BASE
    );
    assert_eq!(
        world.balance(&world.quote_treasury),
        TREASURY_QUOTE - ONE_BASE_PAYS
    );

    let event: Swapped = outcome.event();
    assert_eq!(event.vault, world.vault);
    assert_eq!(event.side, SwapSide::BaseToQuote);
    assert_eq!(event.amount_in, ONE_BASE);
    assert_eq!(event.amount_out, ONE_BASE_PAYS);
    assert_eq!(event.price_e9, 149_850_000);
    assert_eq!(event.quote_slot, world.vault_state().quote_slot);
    // The treasury holdings in the event are computed, not re-read (T018 decision).
    // Here that is finally checked against what really sits in the token accounts.
    assert_eq!(event.base_amount_after, world.balance(&world.base_treasury));
    assert_eq!(
        event.quote_amount_after,
        world.balance(&world.quote_treasury)
    );
}

/// The reverse side: `ask = ceil(mid · 1.001) = 150_150_000`, and for 150 quote
/// the trader receives `floor(150_000_000 · 1e9 / 150_150_000) = 999_000_999`.
#[test]
fn a_trader_gives_quote_and_receives_base() {
    let mut world = World::ready();
    let trader_base_before = world.balance(&world.trader_base);

    let outcome = world.swap(SwapSide::QuoteToBase, 150_000_000, 0);
    outcome.expect_ok();

    assert_eq!(
        world.balance(&world.trader_base),
        trader_base_before + 999_000_999
    );

    let event: Swapped = outcome.event();
    assert_eq!(event.side, SwapSide::QuoteToBase);
    assert_eq!(event.amount_out, 999_000_999);
    assert_eq!(event.price_e9, 150_150_000);
}

/// A swap changes **no** state field: inventory lives in the token accounts, and
/// the quote stays in force until the next update (the T017 decision because of
/// which `vault` in the swap is not `mut`).
#[test]
fn a_swap_leaves_the_vault_account_untouched() {
    let mut world = World::ready();
    let before = world.vault_state();

    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();

    let after = world.vault_state();
    assert_eq!(after.mid_e9, before.mid_e9);
    assert_eq!(after.quote_slot, before.quote_slot);
    assert_eq!(after.max_size_base, before.max_size_base);
    assert_eq!(after.spread_bps, before.spread_bps);
}

// --- The freshness guard (FR-007) ---

/// The bound is checked right at the bound: exactly `max_quote_age_slots` still
/// passes, the next slot no longer does. An off-by-one here is either lost swaps
/// or trading at a price that is already gone.
#[test]
fn the_freshness_limit_is_checked_at_its_own_edge() {
    let mut world = World::ready();
    world.warp_by(u64::from(MAX_QUOTE_AGE_SLOTS));
    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();

    let mut world = World::ready();
    world.warp_by(u64::from(MAX_QUOTE_AGE_SLOTS) + 1);
    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::QuoteStale);
}

#[test]
fn a_vault_without_a_quote_does_not_trade() {
    let mut world = World::new();
    world.deploy().fund();

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::QuoteNotSet);
}

// --- The size guard (FR-008) ---

/// The bound is declared in the **base** asset, so the base leg is measured, not the input.
#[test]
fn an_order_bigger_than_the_quoted_size_is_refused() {
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, MAX_SIZE_BASE + 1, 0)
        .expect_error(VaultError::SizeExceeded);

    // Exactly the bound passes.
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, MAX_SIZE_BASE, 0)
        .expect_ok();
}

/// The same guard from the quote-asset side: the input is in quote, and what is
/// measured is how much base leaves the vault.
#[test]
fn the_size_limit_measures_the_base_leg_in_both_directions() {
    let mut world = World::ready();
    // 10 base cost ~1 501.5 quote; take noticeably more.
    world
        .swap(SwapSide::QuoteToBase, 2_000_000_000, 0)
        .expect_error(VaultError::SizeExceeded);
}

// --- The result guard (FR-009) ---

#[test]
fn a_result_worse_than_the_declared_limit_is_refused() {
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, ONE_BASE_PAYS + 1)
        .expect_error(VaultError::SlippageExceeded);

    // Equality is not "worse": the bound has to be reachable.
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, ONE_BASE_PAYS)
        .expect_ok();
}

// --- Degenerate input ---

#[test]
fn nothing_in_means_nothing_out() {
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, 0, 0)
        .expect_error(VaultError::AmountTooSmall);
}

/// An input that yields zero output after rounding is "take and give nothing back".
#[test]
fn an_amount_that_rounds_to_zero_is_refused() {
    let mut world = World::ready();
    world
        .swap(SwapSide::BaseToQuote, 1, 0)
        .expect_error(VaultError::AmountTooSmall);
}

// --- Liquidity and the hard inventory bound (FR-026) ---

#[test]
fn a_vault_cannot_pay_out_what_it_does_not_hold() {
    let mut world = World::new();
    world.deploy();
    // Almost no quote asset — plenty of base.
    let deposit_base = ix::deposit(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        TREASURY_BASE,
    );
    world.must(&deposit_base);
    let deposit_quote = ix::deposit(
        &world.owner,
        &world.vault,
        &world.quote_mint,
        &world.quote_treasury,
        &world.owner_quote,
        &world.quote_program,
        1_000,
    );
    world.must(&deposit_quote);
    world.quote();

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::InsufficientLiquidity);
}

/// The hard bound at `max_skew_bps = 0`: the inventory starts exactly balanced,
/// so any swap takes it both past the bound and further than it already was.
#[test]
fn a_swap_that_pushes_inventory_past_the_hard_bound_is_refused() {
    let mut world = World::ready();
    let tighten = ix::set_risk_limits(
        &world.owner,
        &world.vault,
        RiskLimits {
            max_quote_age_slots: MAX_QUOTE_AGE_SLOTS,
            max_skew_bps: 0,
        },
    );
    world.must(&tighten);

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::InventoryBound);
    world
        .swap(SwapSide::QuoteToBase, 150_000_000, 0)
        .expect_error(VaultError::InventoryBound);
}

/// The bound limits **swaps**, not state (Phase 1 decision): a vault already past
/// the bound must accept the swap that rebalances it. Otherwise the risk bound
/// would itself create risk — a stuck vault that cannot be turned around without intervention.
#[test]
fn a_rebalancing_swap_is_allowed_even_from_beyond_the_bound() {
    let mut world = World::ready();

    // First skew the inventory under the current 30% bound.
    world
        .swap(SwapSide::BaseToQuote, MAX_SIZE_BASE, 0)
        .expect_ok();
    let skewed = propamm_quote::inventory_skew_bps(&world.inventory(), MID_E9).unwrap();
    assert!(skewed > 0, "the inventory did not skew");

    // Now the bound becomes zero: the state is already past it.
    let tighten = ix::set_risk_limits(
        &world.owner,
        &world.vault,
        RiskLimits {
            max_quote_age_slots: MAX_QUOTE_AGE_SLOTS,
            max_skew_bps: 0,
        },
    );
    world.must(&tighten);

    // A swap towards balance passes…
    world
        .swap(SwapSide::QuoteToBase, 150_000_000, 0)
        .expect_ok();
    let after = propamm_quote::inventory_skew_bps(&world.inventory(), MID_E9).unwrap();
    assert!(after.abs() < skewed.abs(), "the swap did not rebalance");

    // …while one that deepens the skew does not.
    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::InventoryBound);
}

// --- What the math cannot see ---

#[test]
fn a_halted_vault_does_not_trade() {
    let mut world = World::ready();
    world.halt();

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::VaultHalted);
}

#[test]
fn a_trader_who_did_not_sign_does_not_trade() {
    let mut world = World::ready();
    let instruction = ix::without_signature(world.swap_ix(SwapSide::BaseToQuote, ONE_BASE, 0), 0);

    world
        .exec(&instruction)
        .expect_anchor_error(ErrorCode::AccountNotSigner);
}

/// The treasury is not the one recorded in state. Without this check a trader
/// could substitute their own account for the vault's treasury.
#[test]
fn a_treasury_that_is_not_ours_is_not_a_treasury() {
    let mut world = World::ready();
    let instruction = ix::swap(
        &world.trader,
        &world.vault,
        &world.owner_base, // instead of base_treasury
        &world.quote_treasury,
        &world.trader_base,
        &world.trader_quote,
        &world.base_mint,
        &world.quote_mint,
        &world.base_program,
        &world.quote_program,
        propamm_vault::instructions::swap::SwapArgs {
            side: SwapSide::BaseToQuote,
            amount_in: ONE_BASE,
            min_amount_out: 0,
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::AccountMismatch);
}

#[test]
fn a_mint_from_outside_the_pair_is_not_accepted() {
    let mut world = World::ready();
    let instruction = ix::swap(
        &world.trader,
        &world.vault,
        &world.base_treasury,
        &world.quote_treasury,
        &world.trader_base,
        &world.trader_quote,
        &world.quote_mint, // instead of base_mint
        &world.quote_mint,
        &world.base_program,
        &world.quote_program,
        propamm_vault::instructions::swap::SwapArgs {
            side: SwapSide::BaseToQuote,
            amount_in: ONE_BASE,
            min_amount_out: 0,
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::AccountMismatch);
}

/// The spread in the quote really takes from both sides: a round trip leaves
/// the trader with less than they started with. This is the invariant the
/// rounding in the math is asymmetric for.
#[test]
fn a_round_trip_leaves_the_trader_with_less_than_he_started() {
    let mut world = World::ready();
    let base_before = world.balance(&world.trader_base);

    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();
    world
        .swap(SwapSide::QuoteToBase, ONE_BASE_PAYS, 0)
        .expect_ok();

    assert!(
        world.balance(&world.trader_base) < base_before,
        "a round trip cost the trader nothing"
    );
}

/// A quote with skew: a positive `skew_bps` raises both sides, so the vault
/// pays more for the base asset. Checked here rather than in the math, because
/// it passes through state for the first time.
#[test]
fn a_skewed_quote_moves_both_sides_of_the_market() {
    let mut world = World::ready();
    world.quote_with(QuoteUpdate {
        mid_e9: MID_E9,
        spread_bps: SPREAD_BPS,
        skew_bps: 100,
        max_size_base: MAX_SIZE_BASE,
    });

    let outcome = world.swap(SwapSide::BaseToQuote, ONE_BASE, 0);
    outcome.expect_ok();
    let event: Swapped = outcome.event();
    assert!(
        event.price_e9 > u128::from(ONE_BASE_PAYS),
        "the skew did not raise the bid"
    );
}
