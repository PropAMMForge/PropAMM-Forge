//! `initialize_vault` on live accounts (T013, FR-001, FR-002, FR-004, FR-005).
//!
//! Before this file only the fact that the arguments pass `validate()` was
//! checked. Here the instruction itself runs for the first time: the treasury
//! ATAs are created by the token program, the seeds are checked by Anchor, and
//! `mint_guard` reads the mint's **real bytes**, not a list of `ExtensionType`.

use anchor_lang::error::ErrorCode;
use anchor_spl::token_2022::spl_token_2022::extension::{
    BaseStateWithExtensions, ExtensionType, StateWithExtensions,
};
use anchor_spl::token_2022::spl_token_2022::state::Mint as MintState;
use propamm_vault::errors::VaultError;
use propamm_vault_tests::keys::named;
use propamm_vault_tests::world::{MAX_QUOTE_AGE_SLOTS, MAX_SKEW_BPS};
use propamm_vault_tests::{fixtures, World};

#[test]
fn a_deployment_fixes_the_pair_and_opens_two_empty_treasuries() {
    let mut world = World::new();
    world.deploy();

    let state = world.vault_state();
    assert_eq!(state.owner, world.owner);
    assert_eq!(state.pricing_authority, world.pricing_authority);
    assert_eq!(state.halt_authority, world.halt_authority);
    assert_eq!(state.base_mint, world.base_mint);
    assert_eq!(state.quote_mint, world.quote_mint);
    assert_eq!(state.base_vault, world.base_treasury);
    assert_eq!(state.quote_vault, world.quote_treasury);
    assert_eq!(state.max_quote_age_slots, MAX_QUOTE_AGE_SLOTS);
    assert_eq!(state.max_skew_bps, MAX_SKEW_BPS);
    assert!(!state.halted);
    assert_eq!(state.bump, world.bump);

    // FR-002: no outside capital is accepted — the treasuries are created empty.
    assert_eq!(world.balance(&world.base_treasury), 0);
    assert_eq!(world.balance(&world.quote_treasury), 0);

    // There is no quote, and that is not "a price of zero": a swap before
    // `update_quote` is screened out as `QuoteNotSet`.
    assert_eq!(state.mid_e9, 0);
    assert_eq!(state.max_size_base, 0);
    assert_eq!(state.quote_slot, 0);
}

/// A pair may mix classic SPL and Token-2022 (T013). The treasuries are then
/// created by different programs, and each address derives from its own.
#[test]
fn a_mixed_pair_deploys_under_two_token_programs() {
    let mut world = World::with_token_programs(anchor_spl::token::ID, anchor_spl::token_2022::ID);
    world.deploy();

    let state = world.vault_state();
    assert_eq!(state.base_vault, world.base_treasury);
    assert_eq!(state.quote_vault, world.quote_treasury);
    assert_eq!(
        world.account(&world.base_treasury).owner,
        propamm_vault_tests::svm(&anchor_spl::token::ID)
    );
    assert_eq!(
        world.account(&world.quote_treasury).owner,
        propamm_vault_tests::svm(&anchor_spl::token_2022::ID)
    );
}

/// FR-005 on real bytes.
///
/// The `mint_guard` unit tests start from a list of `ExtensionType`, i.e. after
/// the account is parsed. The path `StateWithExtensions::unpack` →
/// `get_extension_types` never executed before this test — and it is exactly
/// what decides whether a mint with a transfer fee becomes half of a pair forever.
#[test]
fn a_mint_with_a_transfer_fee_never_becomes_half_of_a_pair() {
    let mut world = World::with_token_programs(anchor_spl::token::ID, anchor_spl::token_2022::ID);

    let mint = fixtures::mint_with_transfer_fee(&anchor_spl::token_2022::ID, 6);
    // The fixture has to be what it calls itself: otherwise the test would go
    // green because the extension is simply absent.
    let data = mint.data.clone();
    let found = StateWithExtensions::<MintState>::unpack(&data)
        .expect("the mint does not parse")
        .get_extension_types()
        .expect("the extensions cannot be read");
    assert!(found.contains(&ExtensionType::TransferFeeConfig));

    world.put(&world.quote_mint.clone(), mint);

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_error(VaultError::MintHasTransferFee);
}

/// The degenerate pair "a mint with itself" does not deploy.
///
/// **Found by this test:** the refusal comes not from our `IdenticalMints` but
/// from Anchor's own `ConstraintDuplicateMutableAccount`. The cause is the same
/// as with `MintProgramMismatch`: a `constraint` that mentions a field declared
/// below is moved to the end of `try_accounts`, and on identical mints both
/// treasuries are literally one and the same ATA, so the framework stops on it earlier.
///
/// On the same token program there is no way around this: the ATA address
/// derives from `(vault, mint, program)`, so with `base_mint == quote_mint` the
/// two treasuries always coincide. That is, `IdenticalMints` (6000) is
/// unreachable in this configuration. The vault is not created all the same —
/// the FR-004 requirement holds, only the error code is less precise.
#[test]
fn the_same_mint_on_both_sides_is_not_a_pair() {
    let mut world = World::new();
    // The pair is part of the vault address, so the addresses have to be recomputed:
    // otherwise the instruction would fail on the seed check, and the test would speak of something else.
    world.quote_mint = world.base_mint;
    world.retarget();

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_anchor_error(ErrorCode::ConstraintDuplicateMutableAccount);

    assert_eq!(
        world.account(&world.vault).data.len(),
        0,
        "a vault on the degenerate pair got created after all"
    );
}

/// The mint belongs to one token program, and another was passed.
///
/// **Found by this test:** Anchor moves a `constraint` that mentions a field
/// declared below to the end of `try_accounts` — and `base_token_program` is
/// sixth, while `base_mint` itself is second. That is, the check runs **after**
/// `init_if_needed` has already gone into the ATA program. On a fresh
/// deployment that CPI fails first, and our error code never gets to appear
/// (see `a_fresh_deployment_fails_inside_the_ata_cpi_before_our_own_check`).
///
/// Here the treasuries are created in advance — the case `init_if_needed` was
/// chosen for (T013): an ATA for someone else's owner can be created by anyone.
/// No CPI happens, and the guard finally gets its say.
#[test]
fn a_mint_from_another_token_program_is_rejected_by_name() {
    let mut world = World::new();
    world.pre_create_treasuries();
    // The mint stays at its address but now belongs to Token-2022, while classic
    // SPL goes into the instruction as `base_token_program`.
    world.put(
        &world.base_mint.clone(),
        fixtures::mint(&anchor_spl::token_2022::ID, 9),
    );

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_error(VaultError::MintProgramMismatch);
}

/// The same input, but the treasuries do not exist yet.
///
/// The test pins the **actual** behaviour, not the desired one: the deployment
/// refuses, but with the token program's code from inside the CPI, not ours.
/// In other words, on a fresh deployment `MintProgramMismatch` is unreachable —
/// and the wording "refused with the cause named" does not hold here. It puts
/// no capital at risk (the deployment does not happen anyway), so it needs no
/// rework; but it has to be known, and it is recorded here.
#[test]
fn a_fresh_deployment_fails_inside_the_ata_cpi_before_our_own_check() {
    let mut world = World::new();
    world.put(
        &world.base_mint.clone(),
        fixtures::mint(&anchor_spl::token_2022::ID, 9),
    );

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    let outcome = world.exec(&instruction);

    assert!(
        outcome.result.program_result.is_err(),
        "a deployment with the wrong token program passed"
    );
    // Specifically not our code: our space starts at 6000.
    if let mollusk_svm::result::ProgramResult::Failure(
        solana_program_error::ProgramError::Custom(code),
    ) = outcome.result.program_result
    {
        assert!(
            code < 6000,
            "this time the refusal came from our guard — time to rewrite the test note"
        );
    }
}

#[test]
fn an_authority_nobody_can_sign_with_is_rejected() {
    let mut world = World::new();
    let mut args = world.init_args();
    args.pricing_authority = anchor_lang::prelude::Pubkey::default();

    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidAuthority);

    let mut args = world.init_args();
    args.halt_authority = anchor_lang::prelude::Pubkey::default();
    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidAuthority);
}

#[test]
fn limits_that_do_not_limit_stop_the_deployment() {
    let mut world = World::new();
    let mut args = world.init_args();
    args.max_quote_age_slots = 0;

    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_error(VaultError::InvalidRiskLimits);
}

/// A second vault on the same triple `(owner, base, quote)` is not created: the
/// address is the same, and `init` (not `init_if_needed`) refuses on an occupied account.
/// The main thing here is that the state of the first one stays untouched.
#[test]
fn the_same_pair_cannot_be_deployed_twice() {
    let mut world = World::new();
    world.deploy().fund().quote();

    let before = world.vault_state();
    let base_before = world.balance(&world.base_treasury);
    let quote_before = world.balance(&world.quote_treasury);

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    let outcome = world.exec(&instruction);
    assert!(
        outcome.result.program_result.is_err(),
        "a repeated deployment passed\nlog:\n{}",
        outcome.logs.join("\n")
    );

    // The worst possible outcome would be not "an error" but "the vault zeroed
    // together with the quote and the treasury".
    let after = world.vault_state();
    assert_eq!(after.mid_e9, before.mid_e9);
    assert_eq!(after.max_size_base, before.max_size_base);
    assert_eq!(world.balance(&world.base_treasury), base_before);
    assert_eq!(world.balance(&world.quote_treasury), quote_before);
}

/// The owner is payer and signer at once. Without the signature Anchor stops
/// on its own code, before any check of ours.
#[test]
fn a_deployment_without_the_owners_signature_stops_at_the_framework() {
    let mut world = World::new();
    let args = world.init_args();
    let instruction = propamm_vault_tests::ix::without_signature(world.initialize_ix(args), 0);

    world
        .exec(&instruction)
        .expect_anchor_error(ErrorCode::AccountNotSigner);
}

/// Other owners' vaults are other addresses. An attempt to deploy on a PDA
/// derived from someone else's owner fails the seed check.
#[test]
fn a_vault_address_derived_from_someone_else_is_not_ours() {
    let mut world = World::new();
    let (stranger_vault, _) = propamm_vault::state::Vault::pda(
        &named(77),
        &world.base_mint.clone(),
        &world.quote_mint.clone(),
    );
    world.vault = stranger_vault;

    let args = world.init_args();
    let instruction = world.initialize_ix(args);
    world
        .exec(&instruction)
        .expect_anchor_error(ErrorCode::ConstraintSeeds);
}
