//! `initialize_vault` — deployment: the pair is fixed forever, the treasury is
//! created empty, the authorities are split across three keys.
//!
//! # What this instruction decides forever
//!
//! The pair (FR-004) and the vault address. Seeds are `["vault", owner, base_mint, quote_mint]`,
//! so a second vault on the same pair for the same owner cannot be created, and
//! the pair "the other way round" is a different vault with its own capital.
//!
//! # What it does NOT do
//!
//! It takes no capital: the treasury is created empty, funding is `deposit` by
//! the owner (T014, FR-003). It posts no quote: `mid_e9` stays zero, i.e.
//! "no quote posted", and any swap before `update_quote` (T016) is screened
//! out in `propamm_quote` as `QuoteNotSet`.
//!
//! # The honest limit of "an outside deposit is refused" (FR-002)
//!
//! The requirement holds at the level of **instructions**: none lets a third
//! party contribute capital, and none creates shares or a claim on profit. But a
//! token transfer **straight to the vault's ATA** is forbidden by nobody — that is
//! a property of Solana, not our oversight. Such funds become the owner's capital
//! with no rights in return, so the invariant "there are no shares" stays intact;
//! the impossibility of a gift does not follow from it, and cannot be promised.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use propamm_quote::BPS_DENOM;

use crate::errors::VaultError;
use crate::mint_guard::{ensure_transfer_is_faithful, MintRole};
use crate::state::{Vault, VAULT_SEED};

/// Deployment parameters.
///
/// The risk limits are set here rather than by a separate `set_risk_limits` call
/// (T015): SC-001 gives five commands for the whole path from an empty directory
/// to the first swap, and a vault that has no limit yet after deployment is an
/// extra command and a window in which the state exists but the constraint does not.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitializeVaultArgs {
    /// The right to post quotes (FR-010) — the engine key, hot.
    pub pricing_authority: Pubkey,
    /// The right to halt in an emergency (FR-024, FR-023c) — separate from capital.
    pub halt_authority: Pubkey,
    /// Maximum order size in the base asset (FR-008).
    pub max_size_base: u64,
    /// Quote freshness limit in slots (FR-007).
    pub max_quote_age_slots: u32,
    /// Hard bound on inventory skew in basis points (FR-026).
    pub max_skew_bps: u16,
}

impl InitializeVaultArgs {
    /// Domain of the parameters.
    ///
    /// A zero key is refused not out of pedantry: nothing can sign with it, so a
    /// vault with such a `pricing_authority` never quotes, and one with such a
    /// `halt_authority` never halts. Both states look operational right up to the
    /// moment they are needed.
    pub fn validate(&self) -> Result<()> {
        require_keys_neq!(
            self.pricing_authority,
            Pubkey::default(),
            VaultError::InvalidAuthority
        );
        require_keys_neq!(
            self.halt_authority,
            Pubkey::default(),
            VaultError::InvalidAuthority
        );
        require!(self.max_size_base > 0, VaultError::InvalidRiskLimits);
        require!(self.max_quote_age_slots > 0, VaultError::InvalidRiskLimits);
        // Skew never exceeds 100% by construction of `inventory_skew_bps`,
        // so a bound above 10 000 bps limits nothing and merely looks like a bound.
        require!(
            self.max_skew_bps <= BPS_DENOM,
            VaultError::InvalidRiskLimits
        );
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeVault<'info> {
    /// Owner of the capital and payer for the created accounts (FR-002).
    #[account(mut)]
    pub owner: Signer<'info>,

    /// Base asset of the pair.
    ///
    /// The mint account's `owner` is checked against the token program passed in
    /// explicitly: the pair may mix classic SPL and Token-2022, and without this check
    /// a side would go to the wrong program with an obscure error inside the CPI.
    #[account(
        constraint = base_mint.key() != quote_mint.key() @ VaultError::IdenticalMints,
        constraint = base_mint.to_account_info().owner == base_token_program.key @ VaultError::MintProgramMismatch,
    )]
    pub base_mint: InterfaceAccount<'info, Mint>,

    /// Quote asset of the pair.
    #[account(
        constraint = quote_mint.to_account_info().owner == quote_token_program.key @ VaultError::MintProgramMismatch,
    )]
    pub quote_mint: InterfaceAccount<'info, Mint>,

    #[account(
        init,
        payer = owner,
        space = Vault::SPACE,
        seeds = [
            VAULT_SEED,
            owner.key().as_ref(),
            base_mint.key().as_ref(),
            quote_mint.key().as_ref(),
        ],
        bump,
    )]
    pub vault: Account<'info, Vault>,

    /// Treasury of the base asset — an ATA owned by the PDA.
    ///
    /// `init_if_needed`, not `init`, and that is not a relaxation: an ATA for someone
    /// else's owner can be created by anyone, so with `init` a stranger could block
    /// the deployment forever by creating the account in advance. The ATA address
    /// derives from `(vault, mint, token_program)`, so an "already created" account
    /// at that address can have neither a different mint nor a different owner —
    /// there is nothing to substitute here.
    #[account(
        init_if_needed,
        payer = owner,
        associated_token::mint = base_mint,
        associated_token::authority = vault,
        associated_token::token_program = base_token_program,
    )]
    pub base_vault: InterfaceAccount<'info, TokenAccount>,

    /// Treasury of the quote asset — an ATA owned by the PDA.
    #[account(
        init_if_needed,
        payer = owner,
        associated_token::mint = quote_mint,
        associated_token::authority = vault,
        associated_token::token_program = quote_token_program,
    )]
    pub quote_vault: InterfaceAccount<'info, TokenAccount>,

    /// Token program of the base side: classic SPL or Token-2022.
    pub base_token_program: Interface<'info, TokenInterface>,
    /// Token program of the quote side; may differ from the base one.
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_vault(
    ctx: Context<InitializeVault>,
    args: InitializeVaultArgs,
) -> Result<()> {
    args.validate()?;

    // FR-005 — before writing state: the pair is fixed forever, and an asset whose
    // received amount may differ from the sent one cannot be removed afterwards.
    ensure_transfer_is_faithful(
        &ctx.accounts.base_mint.to_account_info(),
        ctx.accounts.base_token_program.key,
        MintRole::Base,
    )?;
    ensure_transfer_is_faithful(
        &ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.quote_token_program.key,
        MintRole::Quote,
    )?;

    let vault = &mut ctx.accounts.vault;

    vault.owner = ctx.accounts.owner.key();
    vault.pricing_authority = args.pricing_authority;
    vault.halt_authority = args.halt_authority;
    vault.base_mint = ctx.accounts.base_mint.key();
    vault.quote_mint = ctx.accounts.quote_mint.key();
    vault.base_vault = ctx.accounts.base_vault.key();
    vault.quote_vault = ctx.accounts.quote_vault.key();

    // There is no quote yet — and that is not "a price of zero" but `QuoteNotSet` in the math.
    // Swaps are impossible before `update_quote` even with a full treasury.
    vault.mid_e9 = 0;
    vault.spread_bps = 0;
    vault.skew_bps = 0;
    vault.quote_slot = 0;

    vault.max_size_base = args.max_size_base;
    vault.max_quote_age_slots = args.max_quote_age_slots;
    vault.max_skew_bps = args.max_skew_bps;

    vault.halted = false;
    vault.bump = ctx.bumps.vault;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sane() -> InitializeVaultArgs {
        InitializeVaultArgs {
            pricing_authority: Pubkey::new_from_array([2; 32]),
            halt_authority: Pubkey::new_from_array([3; 32]),
            max_size_base: 1_000_000,
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
        }
    }

    #[test]
    fn sane_arguments_pass() {
        assert!(sane().validate().is_ok());
    }

    #[test]
    fn a_zero_key_is_not_an_authority() {
        let mut a = sane();
        a.pricing_authority = Pubkey::default();
        assert!(a.validate().is_err());

        let mut a = sane();
        a.halt_authority = Pubkey::default();
        assert!(a.validate().is_err());
    }

    #[test]
    fn limits_that_do_not_limit_are_rejected() {
        let mut a = sane();
        a.max_size_base = 0;
        assert!(a.validate().is_err(), "zero order size");

        let mut a = sane();
        a.max_quote_age_slots = 0;
        assert!(a.validate().is_err(), "zero freshness limit");

        let mut a = sane();
        a.max_skew_bps = BPS_DENOM + 1;
        assert!(a.validate().is_err(), "skew above 100%");
    }

    /// Exactly 100% is "effectively no bound", but it is reachable and does not
    /// contradict `inventory_skew_bps`, so there is no reason to refuse it.
    #[test]
    fn a_hundred_percent_skew_is_still_a_valid_bound() {
        let mut a = sane();
        a.max_skew_bps = BPS_DENOM;
        assert!(a.validate().is_ok());
    }

    /// Argument encoding round-trip. The Anchor coder writes a missing field as zero
    /// and does not complain, so the instruction builder in the SDK (T019) could
    /// silently send `max_size_base = 0` — a state this same check deems invalid.
    /// A layout drift is caught here while it is cheap.
    #[test]
    fn arguments_survive_a_round_trip() {
        let a = sane();
        let mut bytes = Vec::new();
        a.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 32 + 32 + 8 + 4 + 2);

        let back = InitializeVaultArgs::deserialize(&mut bytes.as_slice()).unwrap();
        assert_eq!(back, a);
    }
}
