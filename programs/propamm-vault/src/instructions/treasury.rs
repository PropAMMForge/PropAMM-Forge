//! `deposit` / `withdraw` — capital movement, and by the owner only (FR-003).
//!
//! # One pair of accounts for both sides
//!
//! There are no separate "for base" and "for quote" instructions: the side follows
//! from the mint passed in, and the fact that the token account passed in belongs
//! to that side of this vault is checked explicitly. Two almost identical
//! instructions diverge in the details exactly when they are edited separately.
//!
//! # Why `withdraw` clears the quote
//!
//! Client decision of 2026-08-27. Withdrawing capital leaves an on-chain price the
//! asset behind which may be gone: the `InsufficientLiquidity` guard will not let
//! such a swap execute, but the router will keep treating us as the best venue to
//! the last and keep building routes that fail. That hurts SC-005 more than an extra
//! `update_quote` after a withdrawal costs — the engine posts a new price on the next tick.
//!
//! `deposit` does **not** touch the quote: it only adds liquidity, and no posted
//! price becomes unfillable because of it.
//!
//! # An emergency halt does not block withdrawal
//!
//! `halted` (FR-024) forbids swaps, not disposing of one's own capital.
//! A halt you cannot exit after is not an emergency switch, it is a trap.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::errors::VaultError;
use crate::state::{Vault, VAULT_SEED};

/// Side of the treasury, derived from the mint passed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreasurySide {
    Base,
    Quote,
}

/// Which side is being asked for, and whether the accounts passed in really are it.
///
/// Both conditions are checked together: the mint has to belong to the pair, and
/// the token account has to be the very one recorded in state for that side.
/// Separately they let through the pair "base mint + quote treasury", where
/// everything would then fail inside the token program with a mint mismatch
/// message — technically correct and not explanatory at all.
pub fn resolve_side(
    vault: &Vault,
    mint: &Pubkey,
    treasury: &Pubkey,
) -> core::result::Result<TreasurySide, VaultError> {
    let side = if *mint == vault.base_mint {
        TreasurySide::Base
    } else if *mint == vault.quote_mint {
        TreasurySide::Quote
    } else {
        return Err(VaultError::UnknownMint);
    };

    let expected = match side {
        TreasurySide::Base => vault.base_vault,
        TreasurySide::Quote => vault.quote_vault,
    };
    if *treasury != expected {
        return Err(VaultError::TreasuryAccountMismatch);
    }

    Ok(side)
}

#[derive(Accounts)]
pub struct MoveCapital<'info> {
    /// The only one who may move capital (FR-002, FR-003).
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [
            VAULT_SEED,
            owner.key().as_ref(),
            vault.base_mint.as_ref(),
            vault.quote_mint.as_ref(),
        ],
        bump = vault.bump,
        has_one = owner @ VaultError::OwnerOnly,
    )]
    pub vault: Account<'info, Vault>,

    /// Mint of the side being moved; it is what determines the side.
    pub mint: InterfaceAccount<'info, Mint>,

    /// Treasury of that side — checked against the vault state in the handler.
    #[account(mut)]
    pub treasury: InterfaceAccount<'info, TokenAccount>,

    /// The owner's account: source on `deposit`, recipient on `withdraw`.
    #[account(
        mut,
        constraint = owner_token_account.owner == owner.key() @ VaultError::OwnerOnly,
    )]
    pub owner_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

/// Treasury deposit by the owner (FR-003).
pub fn handle_deposit(ctx: Context<MoveCapital>, amount: u64) -> Result<()> {
    require!(amount > 0, VaultError::ZeroAmount);
    resolve_side(
        &ctx.accounts.vault,
        &ctx.accounts.mint.key(),
        &ctx.accounts.treasury.key(),
    )?;

    // `transfer_checked`, not `transfer`: under Token-2022 plain `transfer` is
    // deprecated, and checking the mint and decimals is exactly what makes
    // "received = sent" verified rather than assumed (FR-005).
    transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.owner_token_account.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.treasury.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )
}

/// Capital withdrawal by the owner (FR-003). Clears the current quote.
pub fn handle_withdraw(ctx: Context<MoveCapital>, amount: u64) -> Result<()> {
    require!(amount > 0, VaultError::ZeroAmount);
    resolve_side(
        &ctx.accounts.vault,
        &ctx.accounts.mint.key(),
        &ctx.accounts.treasury.key(),
    )?;
    require!(
        ctx.accounts.treasury.amount >= amount,
        VaultError::InsufficientVaultBalance
    );

    // Seeds come from state, not from the accounts passed in: `owner` is already
    // checked via `has_one`, and the pair's mints are immutable since deployment (FR-004).
    let owner = ctx.accounts.vault.owner;
    let base_mint = ctx.accounts.vault.base_mint;
    let quote_mint = ctx.accounts.vault.quote_mint;
    let bump = [ctx.accounts.vault.bump];
    let seeds: [&[u8]; 5] = [
        VAULT_SEED,
        owner.as_ref(),
        base_mint.as_ref(),
        quote_mint.as_ref(),
        &bump,
    ];
    let signer: [&[&[u8]]; 1] = [&seeds];

    transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.treasury.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.owner_token_account.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            &signer,
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;

    // The quote is cleared entirely, back to "as after deployment": a partial clear
    // (say, only `mid_e9`) would leave a spread and a skew from a price that is gone,
    // and the next reader of the state could not tell them from current ones.
    ctx.accounts.vault.clear_quote();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> Vault {
        Vault {
            owner: Pubkey::new_from_array([1; 32]),
            pricing_authority: Pubkey::new_from_array([2; 32]),
            halt_authority: Pubkey::new_from_array([3; 32]),
            base_mint: Pubkey::new_from_array([4; 32]),
            quote_mint: Pubkey::new_from_array([5; 32]),
            base_vault: Pubkey::new_from_array([6; 32]),
            quote_vault: Pubkey::new_from_array([7; 32]),
            mid_e9: 150_000_000,
            max_size_base: 1_000,
            quote_slot: 42,
            max_quote_age_slots: 25,
            spread_bps: 10,
            skew_bps: 0,
            max_skew_bps: 3_000,
            halted: false,
            bump: 254,
        }
    }

    #[test]
    fn each_side_resolves_from_its_own_mint() {
        let v = vault();
        assert_eq!(
            resolve_side(&v, &v.base_mint, &v.base_vault),
            Ok(TreasurySide::Base)
        );
        assert_eq!(
            resolve_side(&v, &v.quote_mint, &v.quote_vault),
            Ok(TreasurySide::Quote)
        );
    }

    #[test]
    fn a_mint_outside_the_pair_is_rejected() {
        let v = vault();
        let stranger = Pubkey::new_from_array([9; 32]);
        assert_eq!(
            resolve_side(&v, &stranger, &v.base_vault),
            Err(VaultError::UnknownMint)
        );
    }

    /// The right mint with the wrong treasury — exactly the pair the token program
    /// would reject with a message about something else.
    #[test]
    fn the_right_mint_with_the_wrong_treasury_is_rejected() {
        let v = vault();
        assert_eq!(
            resolve_side(&v, &v.base_mint, &v.quote_vault),
            Err(VaultError::TreasuryAccountMismatch)
        );
        assert_eq!(
            resolve_side(&v, &v.quote_mint, &v.base_vault),
            Err(VaultError::TreasuryAccountMismatch)
        );
    }

    #[test]
    fn a_treasury_from_another_vault_is_rejected() {
        let v = vault();
        let foreign = Pubkey::new_from_array([8; 32]);
        assert_eq!(
            resolve_side(&v, &v.base_mint, &foreign),
            Err(VaultError::TreasuryAccountMismatch)
        );
    }
}
