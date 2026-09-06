//! Hostile sequences: what breaks between instructions, not inside them.
//!
//! Every guard is checked separately in its own file. Here something else is
//! checked — that the state one instruction left does not open a hole for the
//! next. It is on this seam that the bugs invisible from any single instruction
//! live: a quote that survived a capital withdrawal; a key still valid after a
//! replacement; an age not counted from the update it was thought to be.

use propamm_vault::errors::VaultError;
use propamm_vault::events::Swapped;
use propamm_vault::instructions::swap::SwapSide;
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault_tests::keys::named;
use propamm_vault_tests::world::{MAX_QUOTE_AGE_SLOTS, MAX_SIZE_BASE, MID_E9, SPREAD_BPS};
use propamm_vault_tests::{fixtures, ix, World};

const ONE_BASE: u64 = 1_000_000_000;

/// A capital withdrawal clears the price — and that is exactly why the next swap fails.
///
/// Without this the router would keep treating us as the best venue to the last
/// and keep building routes that fail (rationale in the header of `treasury.rs`).
#[test]
fn a_quote_does_not_survive_the_capital_it_was_quoted_on() {
    let mut world = World::ready();
    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();

    let withdraw = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.quote_mint,
        &world.quote_treasury,
        &world.owner_quote,
        &world.quote_program,
        1_000_000,
    );
    world.must(&withdraw);

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::QuoteNotSet);
}

/// Replacing the quote signer also clears the price: the previous source is no
/// longer trusted, and trading at the quote it posted is not allowed.
#[test]
fn a_quote_does_not_survive_the_key_that_set_it() {
    let mut world = World::ready();
    let replacement = named(22);
    world.put(&replacement, fixtures::wallet(0));

    let handover = ix::set_pricing_authority(&world.owner, &world.vault, replacement);
    world.must(&handover);

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::QuoteNotSet);

    // The new key brings the venue back to trading with one update.
    let requote = ix::update_quote(
        &replacement,
        &world.vault,
        QuoteUpdate {
            mid_e9: MID_E9,
            spread_bps: SPREAD_BPS,
            skew_bps: 0,
            max_size_base: MAX_SIZE_BASE,
        },
    );
    world.must(&requote);
    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();
}

/// A stale quote comes back to life by an **update**, not by someone trying to
/// use it. The age counts from the last `update_quote`, and the engine's next
/// tick has to reset it.
#[test]
fn a_stale_quote_comes_back_only_when_it_is_re_quoted() {
    let mut world = World::ready();
    world.warp_by(u64::from(MAX_QUOTE_AGE_SLOTS) + 5);

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::QuoteStale);

    world.quote();
    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();
}

/// A cleared quote does not "come back to life" from the next swap and leaves
/// no trace in state: `QuoteNotSet` has to hold until a price is posted again.
#[test]
fn a_cleared_quote_stays_cleared() {
    let mut world = World::ready();
    let clear = ix::clear_quote(&world.pricing_authority, &world.vault);
    world.must(&clear);

    for _ in 0..3 {
        world.warp_by(1);
        world
            .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
            .expect_error(VaultError::QuoteNotSet);
    }
    assert_eq!(world.vault_state().mid_e9, 0);
}

/// A halt closes trading and price updates — and does not close the exit for the owner.
/// Three different answers to one flag; they are easy to confuse, and each is
/// checked here in one sequence.
#[test]
fn a_halt_closes_trading_and_quoting_but_not_the_door() {
    let mut world = World::ready();
    world.halt();

    world
        .swap(SwapSide::BaseToQuote, ONE_BASE, 0)
        .expect_error(VaultError::VaultHalted);

    let requote = ix::update_quote(
        &world.pricing_authority,
        &world.vault,
        QuoteUpdate {
            mid_e9: MID_E9,
            spread_bps: SPREAD_BPS,
            skew_bps: 0,
            max_size_base: MAX_SIZE_BASE,
        },
    );
    world.exec(&requote).expect_error(VaultError::VaultHalted);

    let withdraw = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        1_000,
    );
    world.exec(&withdraw).expect_ok();
}

/// Several swaps in a row at one quote: the price does not change, the inventory
/// accumulates. This catches the assumption "a swap somehow updates the price itself".
#[test]
fn several_swaps_share_one_quote_and_move_only_the_inventory() {
    let mut world = World::ready();
    let quote_slot = world.vault_state().quote_slot;
    let mut previous = world.inventory().base_amount;

    for step in 0..5u64 {
        world.warp_by(1);
        let outcome = world.swap(SwapSide::BaseToQuote, ONE_BASE, 0);
        outcome.expect_ok();

        let event: Swapped = outcome.event();
        assert_eq!(event.price_e9, 149_850_000, "price drifted at step {step}");
        assert_eq!(event.quote_slot, quote_slot);

        let now = world.inventory().base_amount;
        assert_eq!(now, previous + ONE_BASE);
        previous = now;
    }

    assert_eq!(world.vault_state().quote_slot, quote_slot);
}

/// The classic attempt: slip the treasury itself in place of the trader's
/// account so the vault pays itself. Anchor stops this as a repeated mutable
/// account — i.e. before our arithmetic.
#[test]
fn a_trader_cannot_point_his_own_leg_at_the_treasury() {
    let mut world = World::ready();
    let instruction = ix::swap(
        &world.trader,
        &world.vault,
        &world.base_treasury,
        &world.quote_treasury,
        &world.base_treasury, // instead of trader_base_account
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
    let outcome = world.exec(&instruction);
    assert!(
        outcome.result.program_result.is_err(),
        "the vault paid itself\nlog:\n{}",
        outcome.logs.join("\n")
    );
}

/// Deploy → fund → quote → swap → withdraw: the full US1 cycle in one run. It
/// does not replace the e2e on a local network (T024), but catches divergence
/// between the steps earlier and cheaper.
#[test]
fn the_whole_cycle_runs_end_to_end() {
    let mut world = World::new();
    world.deploy().fund().quote();

    world.warp_by(3);
    world.swap(SwapSide::BaseToQuote, ONE_BASE, 0).expect_ok();
    world.warp_by(3);
    world
        .swap(SwapSide::QuoteToBase, 150_000_000, 0)
        .expect_ok();

    let base = world.inventory().base_amount;
    let withdraw = ix::withdraw(
        &world.owner,
        &world.vault,
        &world.base_mint,
        &world.base_treasury,
        &world.owner_base,
        &world.base_program,
        base,
    );
    world.must(&withdraw);

    assert_eq!(world.inventory().base_amount, 0);
    assert_eq!(world.vault_state().mid_e9, 0, "withdraw kept the quote");
}
