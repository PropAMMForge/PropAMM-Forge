//! Instruction assembly for Mollusk.
//!
//! # Why the arguments are spelled out in full rather than taken from the world
//!
//! Half of this task is **hostile** calls: someone else's treasury instead of
//! ours, an unsigned trader, a mint not from this pair. A builder that takes
//! accounts from the world state cannot assemble such a call — and the test
//! would have to be written by patching a ready `Instruction` by index, i.e.
//! silently binding itself to the field order in `#[derive(Accounts)]`.
//!
//! The account order here is the declaration order itself. Anchor checks it
//! positionally, so two accounts of the same type swapped around are a mistake
//! the compiler does not see; the happy path of each instruction catches it.
//!
//! # Instruction data comes from the program's types
//!
//! The discriminator and borsh go through `anchor_lang::InstructionData`, not by
//! hand. A second copy of the encoding here would prove that the test and the
//! program read the bytes the same way only until they started being edited apart.

use anchor_lang::{prelude::Pubkey as AnchorKey, InstructionData};
use propamm_vault::instructions::{
    authority::RiskLimits, initialize_vault::InitializeVaultArgs, swap::SwapArgs,
    update_quote::QuoteUpdate,
};
use solana_instruction::{AccountMeta, Instruction};

use crate::keys::svm;

fn signer(key: &AnchorKey) -> AccountMeta {
    AccountMeta::new_readonly(svm(key), true)
}

fn signer_mut(key: &AnchorKey) -> AccountMeta {
    AccountMeta::new(svm(key), true)
}

fn readonly(key: &AnchorKey) -> AccountMeta {
    AccountMeta::new_readonly(svm(key), false)
}

fn writable(key: &AnchorKey) -> AccountMeta {
    AccountMeta::new(svm(key), false)
}

fn build(accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    Instruction {
        program_id: svm(&propamm_vault::ID),
        accounts,
        data,
    }
}

/// `initialize_vault` — accounts in `InitializeVault` order.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn initialize_vault(
    owner: &AnchorKey,
    base_mint: &AnchorKey,
    quote_mint: &AnchorKey,
    vault: &AnchorKey,
    base_treasury: &AnchorKey,
    quote_treasury: &AnchorKey,
    base_token_program: &AnchorKey,
    quote_token_program: &AnchorKey,
    args: InitializeVaultArgs,
) -> Instruction {
    build(
        vec![
            signer_mut(owner),
            readonly(base_mint),
            readonly(quote_mint),
            writable(vault),
            writable(base_treasury),
            writable(quote_treasury),
            readonly(base_token_program),
            readonly(quote_token_program),
            readonly(&anchor_spl::associated_token::ID),
            readonly(&anchor_lang::system_program::ID),
        ],
        propamm_vault::instruction::InitializeVault { args }.data(),
    )
}

/// `set_pricing_authority` — accounts in `AdminOnly` order.
#[must_use]
pub fn set_pricing_authority(
    owner: &AnchorKey,
    vault: &AnchorKey,
    new_authority: AnchorKey,
) -> Instruction {
    build(
        vec![signer(owner), writable(vault)],
        propamm_vault::instruction::SetPricingAuthority { new_authority }.data(),
    )
}

/// `set_halt_authority` — accounts in `AdminOnly` order.
#[must_use]
pub fn set_halt_authority(
    owner: &AnchorKey,
    vault: &AnchorKey,
    new_authority: AnchorKey,
) -> Instruction {
    build(
        vec![signer(owner), writable(vault)],
        propamm_vault::instruction::SetHaltAuthority { new_authority }.data(),
    )
}

/// `set_risk_limits` — accounts in `AdminOnly` order.
#[must_use]
pub fn set_risk_limits(owner: &AnchorKey, vault: &AnchorKey, limits: RiskLimits) -> Instruction {
    build(
        vec![signer(owner), writable(vault)],
        propamm_vault::instruction::SetRiskLimits { limits }.data(),
    )
}

/// `update_quote` — accounts in `Quoting` order.
#[must_use]
pub fn update_quote(
    pricing_authority: &AnchorKey,
    vault: &AnchorKey,
    quote: QuoteUpdate,
) -> Instruction {
    build(
        vec![signer(pricing_authority), writable(vault)],
        propamm_vault::instruction::UpdateQuote { quote }.data(),
    )
}

/// `clear_quote` — accounts in `Quoting` order.
#[must_use]
pub fn clear_quote(pricing_authority: &AnchorKey, vault: &AnchorKey) -> Instruction {
    build(
        vec![signer(pricing_authority), writable(vault)],
        propamm_vault::instruction::ClearQuote {}.data(),
    )
}

/// `deposit` — accounts in `MoveCapital` order.
#[must_use]
pub fn deposit(
    owner: &AnchorKey,
    vault: &AnchorKey,
    mint: &AnchorKey,
    treasury: &AnchorKey,
    owner_token_account: &AnchorKey,
    token_program: &AnchorKey,
    amount: u64,
) -> Instruction {
    build(
        vec![
            signer(owner),
            writable(vault),
            readonly(mint),
            writable(treasury),
            writable(owner_token_account),
            readonly(token_program),
        ],
        propamm_vault::instruction::Deposit { amount }.data(),
    )
}

/// `withdraw` — accounts in `MoveCapital` order.
#[must_use]
pub fn withdraw(
    owner: &AnchorKey,
    vault: &AnchorKey,
    mint: &AnchorKey,
    treasury: &AnchorKey,
    owner_token_account: &AnchorKey,
    token_program: &AnchorKey,
    amount: u64,
) -> Instruction {
    build(
        vec![
            signer(owner),
            writable(vault),
            readonly(mint),
            writable(treasury),
            writable(owner_token_account),
            readonly(token_program),
        ],
        propamm_vault::instruction::Withdraw { amount }.data(),
    )
}

/// `swap` — accounts in `Swap` order.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn swap(
    trader: &AnchorKey,
    vault: &AnchorKey,
    base_treasury: &AnchorKey,
    quote_treasury: &AnchorKey,
    trader_base_account: &AnchorKey,
    trader_quote_account: &AnchorKey,
    base_mint: &AnchorKey,
    quote_mint: &AnchorKey,
    base_token_program: &AnchorKey,
    quote_token_program: &AnchorKey,
    args: SwapArgs,
) -> Instruction {
    build(
        vec![
            signer(trader),
            readonly(vault),
            writable(base_treasury),
            writable(quote_treasury),
            writable(trader_base_account),
            writable(trader_quote_account),
            readonly(base_mint),
            readonly(quote_mint),
            readonly(base_token_program),
            readonly(quote_token_program),
        ],
        propamm_vault::instruction::Swap { args }.data(),
    )
}

/// Remove the signature from the account at position `index`.
///
/// A separate function, because "the trader did not sign" is not a different
/// builder but the same call with one flag removed; writing a second builder
/// for it would mean two places that have to stay identical.
///
/// # Panics
///
/// If the position does not exist.
#[must_use]
pub fn without_signature(mut instruction: Instruction, index: usize) -> Instruction {
    instruction
        .accounts
        .get_mut(index)
        .expect("no such account")
        .is_signer = false;
    instruction
}
