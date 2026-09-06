//! `deposit` / `withdraw` on live accounts (T014, FR-003).
//!
//! Here the transfer itself runs for the first time: before this, only the fact
//! that `resolve_side` tells the sides apart on pure data was checked. The
//! `treasury_amount_after` arithmetic is computed, not re-read (T018 decision),
//! so every balance check here is also a check of that arithmetic.

use propamm_vault::errors::VaultError;
use propamm_vault::events::{CapitalFlow, CapitalMoved, QuoteClearReason, QuoteCleared};
use propamm_vault::instructions::treasury::TreasurySide;
use propamm_vault_tests::keys::named;
use propamm_vault_tests::world::{TREASURY_BASE, TREASURY_QUOTE};
use propamm_vault_tests::{fixtures, ix, World};

#[test]
fn a_deposit_moves_capital_and_says_so() {
    let mut world = World::new();
    world.deploy();

    let before = world.balance(&world.owner_base);
    let instruction = ix::deposit(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        TREASURY_BASE,
    );
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    assert_eq!(world.balance(&world.base_treasury), TREASURY_BASE);
    assert_eq!(world.balance(&world.owner_base), before - TREASURY_BASE);

    let event: CapitalMoved = outcome.event();
    assert_eq!(event.vault, world.vault);
    assert_eq!(event.flow, CapitalFlow::Deposit);
    assert_eq!(event.side, TreasurySide::Base);
    assert_eq!(event.amount, TREASURY_BASE);
    // The computed balance and the real one have to match — otherwise the P&L
    // (FR-021) would be computed from a number that is not on chain.
    assert_eq!(event.treasury_amount_after, TREASURY_BASE);
    assert_eq!(event.slot, world.slot);
}

#[test]
fn a_second_deposit_adds_to_the_first() {
    let mut world = World::new();
    world.deploy();

    for _ in 0..2 {
        let instruction = ix::deposit(
            &world.owner,
            &world.vault,
            &world.quote_mint,
            &world.quote_treasury,
            &world.owner_quote,
            &world.quote_program,
            TREASURY_QUOTE,
        );
        let outcome = world.exec(&instruction);
        outcome.expect_ok();
    }
    assert_eq!(world.balance(&world.quote_treasury), TREASURY_QUOTE * 2);
}

/// **Found by this test:** `has_one = owner` in `MoveCapital` never fires. The
/// owner is part of the vault **seeds**, so a foreign signature gives a different
/// PDA address, and Anchor stops at `ConstraintSeeds` earlier. The guard is no
/// weaker for it — on the contrary, the address is stronger than the field — but
/// the error code the CLI has to expect is this one.
#[test]
fn capital_does_not_move_for_anyone_but_the_owner() {
    let mut world = World::new();
    world.deploy().fund();

    let stranger = named(66);
    world.put(&stranger, fixtures::wallet(1_000_000_000));
    let stranger_account =
        fixtures::token_account(&world.base_program, &world.base_mint, &stranger, 1_000);
    let stranger_token = anchor_spl::associated_token::get_associated_token_address_with_program_id(
        &stranger,
        &world.base_mint,
        &world.base_program,
    );
    world.put(&stranger_token, stranger_account);

    let instruction = ix::withdraw(
        &stranger,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &stranger_token,
        &world.base_program,
        1,
    );
    world
        .exec(&instruction)
        .expect_anchor_error(anchor_lang::error::ErrorCode::ConstraintSeeds);

    assert_eq!(world.balance(&world.base_treasury), TREASURY_BASE);
}

/// The recipient account has to belong to the owner. This check, unlike
/// `has_one`, does fire: it mentions only a field declared above.
#[test]
fn capital_cannot_be_withdrawn_into_a_stranger_account() {
    let mut world = World::new();
    world.deploy().fund();

    let instruction = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.trader_base,
        &world.base_program,
        1,
    );
    world.exec(&instruction).expect_error(VaultError::OwnerOnly);
}

#[test]
fn a_move_of_nothing_is_not_a_move() {
    let mut world = World::new();
    world.deploy();

    let instruction = ix::deposit(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        0,
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::ZeroAmount);
}

#[test]
fn a_mint_outside_the_pair_has_no_side() {
    let mut world = World::new();
    world.deploy().fund();

    let stranger_mint = named(55);
    world.put(
        &stranger_mint,
        fixtures::mint(&world.base_program.clone(), 9),
    );

    let instruction = ix::deposit(
        &world.owner,
        &world.vault,
        &stranger_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        1_000,
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::UnknownMint);
}

/// The mint of one side, the treasury of the other. Separately both accounts are
/// valid — which is exactly why the side and the treasury are checked together (T014).
#[test]
fn the_base_mint_does_not_open_the_quote_treasury() {
    let mut world = World::new();
    world.deploy().fund();

    let instruction = ix::deposit(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.quote_treasury,
        &world.owner_base,
        &world.base_program,
        1_000,
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::TreasuryAccountMismatch);
}

#[test]
fn a_withdrawal_bigger_than_the_treasury_is_refused() {
    let mut world = World::new();
    world.deploy().fund();

    let instruction = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        TREASURY_BASE + 1,
    );
    world
        .exec(&instruction)
        .expect_error(VaultError::InsufficientVaultBalance);

    assert_eq!(world.balance(&world.base_treasury), TREASURY_BASE);
}

/// A capital withdrawal clears the quote (T014 decision) and writes a separate
/// event about it with its own reason — otherwise the history could not tell it from an explicit clear.
#[test]
fn a_withdrawal_takes_the_quote_down_with_it() {
    let mut world = World::ready();

    assert_ne!(world.vault_state().mid_e9, 0);

    let instruction = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.quote_mint,
        &world.quote_treasury,
        &world.owner_quote,
        &world.quote_program,
        1_000_000,
    );
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    let state = world.vault_state();
    assert_eq!(state.mid_e9, 0);
    assert_eq!(state.spread_bps, 0);
    assert_eq!(state.max_size_base, 0);
    assert_eq!(state.quote_slot, 0);

    let moved: CapitalMoved = outcome.event();
    assert_eq!(moved.flow, CapitalFlow::Withdraw);
    assert_eq!(moved.side, TreasurySide::Quote);
    assert_eq!(moved.treasury_amount_after, TREASURY_QUOTE - 1_000_000);

    let cleared: QuoteCleared = outcome.event();
    assert_eq!(cleared.reason, QuoteClearReason::CapitalWithdrawn);
    // Both events come from one transaction — and have to speak of one slot.
    assert_eq!(cleared.slot, moved.slot);
}

/// Clearing what was not there is not an event. Otherwise every withdrawal from
/// a vault without a price would write an invented quote clearing into the history.
#[test]
fn a_withdrawal_without_a_quote_reports_only_itself() {
    let mut world = World::new();
    world.deploy().fund();

    let instruction = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        1,
    );
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    assert_eq!(outcome.events::<CapitalMoved>(), 1);
    assert_eq!(outcome.events::<QuoteCleared>(), 0);
}

/// A deposit does not touch the price: it only adds liquidity, and no posted
/// quote becomes unfillable because of it.
#[test]
fn a_deposit_leaves_the_quote_alone() {
    let mut world = World::ready();
    let before = world.vault_state();

    let instruction = ix::deposit(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        1_000_000,
    );
    let outcome = world.exec(&instruction);
    outcome.expect_ok();

    let after = world.vault_state();
    assert_eq!(after.mid_e9, before.mid_e9);
    assert_eq!(after.quote_slot, before.quote_slot);
    assert_eq!(outcome.events::<QuoteCleared>(), 0);
}

/// A halt forbids swaps, not disposing of one's own capital (T014): a halt you
/// cannot exit after is a trap, not a switch.
#[test]
fn a_halted_vault_still_lets_its_owner_out() {
    let mut world = World::ready();
    world.halt();

    let instruction = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        1_000,
    );
    world.exec(&instruction).expect_ok();
    assert_eq!(world.balance(&world.base_treasury), TREASURY_BASE - 1_000);
}
