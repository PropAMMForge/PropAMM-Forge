//! `update_quote` / `clear_quote` on live accounts (T016, FR-006, FR-007,
//! FR-010, FR-014).
//!
//! The most important thing here is **where the slot comes from**. The T016
//! decision is that `quote_slot` is written by the chain, not the client; without
//! a chain there is nothing to check that decision with, because there simply is
//! no such field in the arguments, and its absence proves nothing.

use propamm_vault::errors::VaultError;
use propamm_vault::events::{QuoteClearReason, QuoteCleared, QuoteUpdated};
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault_tests::keys::named;
use propamm_vault_tests::world::{MAX_SIZE_BASE, MID_E9, SPREAD_BPS};
use propamm_vault_tests::{fixtures, ix, World};

fn quote() -> QuoteUpdate {
    QuoteUpdate {
        mid_e9: MID_E9,
        spread_bps: SPREAD_BPS,
        skew_bps: -25,
        max_size_base: MAX_SIZE_BASE,
    }
}

#[test]
fn a_quote_lands_in_the_account_with_the_chains_own_slot() {
    let mut world = World::new();
    world.deploy();
    world.warp_by(777);

    let instruction = ix::update_quote(&world.pricing_authority, &world.vault, quote());
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    let state = world.vault_state();
    assert_eq!(state.mid_e9, MID_E9);
    assert_eq!(state.spread_bps, SPREAD_BPS);
    assert_eq!(state.skew_bps, -25);
    assert_eq!(state.max_size_base, MAX_SIZE_BASE);
    // Here it is: the slot in state is the chain's slot, and no argument affects
    // it, because there is no such argument.
    assert_eq!(state.quote_slot, world.slot);

    let event: QuoteUpdated = outcome.event();
    assert_eq!(event.vault, world.vault);
    assert_eq!(event.slot, world.slot);
    assert_eq!(event.mid_e9, MID_E9);
    assert_eq!(event.spread_bps, SPREAD_BPS);
    assert_eq!(event.skew_bps, -25);
    assert_eq!(event.max_size_base, MAX_SIZE_BASE);
}

/// Every update moves the age forward. Without this the freshness guard would
/// measure the age from the first quote forever.
#[test]
fn each_update_moves_the_age_forward() {
    let mut world = World::ready();
    let first = world.vault_state().quote_slot;

    world.warp_by(10);
    world.quote();

    assert_eq!(world.vault_state().quote_slot, first + 10);
}

/// The quote signer is not the owner — and vice versa. `Quoting` derives the
/// seeds from the vault **state**, not from the signer, so `has_one` is reached
/// here (unlike `MoveCapital`, where the owner is part of the address).
#[test]
fn nobody_but_the_pricing_authority_quotes() {
    let mut world = World::ready();

    let instruction = ix::update_quote(&world.owner, &world.vault, quote());
    world
        .exec(&instruction)
        .expect_error(VaultError::PricingAuthorityOnly);

    let stranger = named(66);
    world.put(&stranger, fixtures::wallet(0));
    let instruction = ix::update_quote(&stranger, &world.vault, quote());
    world
        .exec(&instruction)
        .expect_error(VaultError::PricingAuthorityOnly);
}

/// A zero in `mid_e9` is "no price", and it must not be accepted as a price:
/// otherwise an argument forgotten by a builder would silently clear the quote (T016 decision).
#[test]
fn a_zero_price_is_not_a_price() {
    let mut world = World::ready();
    let instruction = ix::update_quote(
        &world.pricing_authority,
        &world.vault,
        QuoteUpdate {
            mid_e9: 0,
            ..quote()
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidQuote);

    // The previous quote stays in force meanwhile.
    assert_eq!(world.vault_state().mid_e9, MID_E9);
}

#[test]
fn a_quote_nobody_can_trade_against_is_rejected() {
    let mut world = World::ready();
    let instruction = ix::update_quote(
        &world.pricing_authority,
        &world.vault,
        QuoteUpdate {
            max_size_base: 0,
            ..quote()
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidQuote);
}

/// The domain is checked by the math itself — the program merely refuses what
/// `side_price_e9` fails on. Both sides are computed on purpose: bid and ask
/// cross the boundary in different places.
#[test]
fn parameters_outside_the_maths_domain_do_not_become_a_quote() {
    let mut world = World::ready();

    for bad in [
        QuoteUpdate {
            spread_bps: propamm_quote::BPS_DENOM,
            ..quote()
        },
        QuoteUpdate {
            skew_bps: -10_000,
            ..quote()
        },
    ] {
        let instruction = ix::update_quote(&world.pricing_authority, &world.vault, bad);
        world
            .exec(&instruction)
            .expect_error(VaultError::InvalidQuote);
    }
}

/// A halted vault does not quote: otherwise the halt would clear the price and
/// the engine's next tick would put it back.
#[test]
fn a_halted_vault_does_not_quote() {
    let mut world = World::ready();
    world.halt();

    let instruction = ix::update_quote(&world.pricing_authority, &world.vault, quote());
    world
        .exec(&instruction)
        .expect_error(VaultError::VaultHalted);
}

#[test]
fn clearing_a_quote_leaves_no_field_behind_and_says_why() {
    let mut world = World::ready();

    let instruction = ix::clear_quote(&world.pricing_authority, &world.vault);
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    let state = world.vault_state();
    assert_eq!(state.mid_e9, 0);
    assert_eq!(state.spread_bps, 0);
    assert_eq!(state.skew_bps, 0);
    assert_eq!(state.max_size_base, 0);
    assert_eq!(state.quote_slot, 0);
    // Risk limits are untouched by clearing.
    assert_eq!(state.max_skew_bps, propamm_vault_tests::world::MAX_SKEW_BPS);

    let event: QuoteCleared = outcome.event();
    assert_eq!(event.reason, QuoteClearReason::Explicit);
    assert_eq!(event.slot, world.slot);
}

/// A second clear in a row — success without an event. Otherwise the history
/// would gain a clearing of a price that never existed.
#[test]
fn clearing_twice_writes_one_event() {
    let mut world = World::ready();
    let instruction = ix::clear_quote(&world.pricing_authority, &world.vault);

    let first = world.exec(&instruction);
    first.expect_ok();
    assert_eq!(first.events::<QuoteCleared>(), 1);

    let second = world.exec(&instruction);
    second.expect_ok();
    assert_eq!(second.events::<QuoteCleared>(), 0);
}

/// Removing a price is always a safe action, and a halt does not stand in its way (T016).
#[test]
fn a_halted_vault_can_still_drop_its_quote() {
    let mut world = World::ready();
    world.halt();

    let instruction = ix::clear_quote(&world.pricing_authority, &world.vault);
    world.exec(&instruction).expect_ok();
    assert_eq!(world.vault_state().mid_e9, 0);
}
