//! Replacing authorities and risk limits on live accounts (T015, FR-010, FR-024).
//!
//! The main thing proven here is not "the field changed" but that **the new key
//! works and the old one does not**. Authorities that were written but unlock
//! nothing look no different from ones written wrongly.

use propamm_vault::errors::VaultError;
use propamm_vault::events::{QuoteClearReason, QuoteCleared};
use propamm_vault::instructions::authority::RiskLimits;
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault_tests::keys::named;
use propamm_vault_tests::world::{MAX_SIZE_BASE, MID_E9, SPREAD_BPS};
use propamm_vault_tests::{fixtures, ix, World};

fn some_quote() -> QuoteUpdate {
    QuoteUpdate {
        mid_e9: MID_E9,
        spread_bps: SPREAD_BPS,
        skew_bps: 0,
        max_size_base: MAX_SIZE_BASE,
    }
}

#[test]
fn replacing_the_pricing_key_hands_over_the_right_and_drops_the_price() {
    let mut world = World::ready();
    let replacement = named(22);
    world.put(&replacement, fixtures::wallet(0));

    let instruction = ix::set_pricing_authority(&world.owner, &world.vault, replacement);
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    assert_eq!(world.vault_state().pricing_authority, replacement);
    assert_eq!(
        world.vault_state().mid_e9,
        0,
        "the previous source's price remained"
    );

    let cleared: QuoteCleared = outcome.event();
    assert_eq!(cleared.reason, QuoteClearReason::PricingAuthorityChanged);

    // The old key no longer quotes…
    let stale_key = ix::update_quote(&world.pricing_authority, &world.vault, some_quote());
    world
        .exec(&stale_key)
        .expect_error(VaultError::PricingAuthorityOnly);

    // …and the new one does.
    let fresh_key = ix::update_quote(&replacement, &world.vault, some_quote());
    world.exec(&fresh_key).expect_ok();
    assert_eq!(world.vault_state().mid_e9, MID_E9);
}

/// A replacement on a vault without a price writes no event: there was nothing to clear.
#[test]
fn replacing_the_pricing_key_without_a_quote_is_silent() {
    let mut world = World::new();
    world.deploy();

    let replacement = named(22);
    let instruction = ix::set_pricing_authority(&world.owner, &world.vault, replacement);
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    assert_eq!(outcome.events::<QuoteCleared>(), 0);
}

#[test]
fn a_key_nobody_can_sign_with_is_not_an_authority() {
    let mut world = World::ready();

    let instruction = ix::set_pricing_authority(
        &world.owner,
        &world.vault,
        anchor_lang::prelude::Pubkey::default(),
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidAuthority);

    let instruction = ix::set_halt_authority(
        &world.owner,
        &world.vault,
        anchor_lang::prelude::Pubkey::default(),
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidAuthority);
}

/// As in `MoveCapital`, the owner is in the seeds, so a foreign signature never
/// reaches `has_one` — the address stops it.
#[test]
fn only_the_owner_replaces_keys() {
    let mut world = World::ready();
    let stranger = named(66);
    world.put(&stranger, fixtures::wallet(0));

    let instruction = ix::set_pricing_authority(&stranger, &world.vault, stranger);
    world
        .exec(&instruction)
        .expect_anchor_error(anchor_lang::error::ErrorCode::ConstraintSeeds);

    assert_eq!(
        world.vault_state().pricing_authority,
        world.pricing_authority
    );
}

/// Replacing the halt key says nothing about the price — and must not touch it.
#[test]
fn replacing_the_halt_key_leaves_the_quote_standing() {
    let mut world = World::ready();
    let before = world.vault_state();
    let replacement = named(33);

    let instruction = ix::set_halt_authority(&world.owner, &world.vault, replacement);
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    let after = world.vault_state();
    assert_eq!(after.halt_authority, replacement);
    assert_eq!(after.mid_e9, before.mid_e9);
    assert_eq!(after.quote_slot, before.quote_slot);
    assert_eq!(outcome.events::<QuoteCleared>(), 0);
}

#[test]
fn risk_limits_change_and_stay_within_their_domain() {
    let mut world = World::ready();

    let instruction = ix::set_risk_limits(
        &world.owner,
        &world.vault,
        RiskLimits {
            max_quote_age_slots: 5,
            max_skew_bps: 1_000,
        },
    );
    world.exec(&instruction).expect_ok();

    let state = world.vault_state();
    assert_eq!(state.max_quote_age_slots, 5);
    assert_eq!(state.max_skew_bps, 1_000);
    // Risk limits do not belong to the quote — it has to survive.
    assert_eq!(state.mid_e9, MID_E9);

    let instruction = ix::set_risk_limits(
        &world.owner,
        &world.vault,
        RiskLimits {
            max_quote_age_slots: 0,
            max_skew_bps: 1_000,
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidRiskLimits);

    let instruction = ix::set_risk_limits(
        &world.owner,
        &world.vault,
        RiskLimits {
            max_quote_age_slots: 5,
            max_skew_bps: propamm_quote::BPS_DENOM + 1,
        },
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidRiskLimits);
}
