//! `swap` — executing a swap at the posted quote.
//!
//! # The four guards do not live here
//!
//! Freshness (FR-007), size (FR-008), the result bound (FR-009) and the hard
//! inventory bound (FR-026) are checked by [`propamm_quote::compute_swap`] — the
//! same code the router computes the quote with. Duplicating them in the program
//! would mean a second implementation of the same rules and waiting for them to
//! diverge; that divergence is exactly what SC-006 forbids.
//!
//! The program adds exactly what the crate cannot see: the `halted` flag (FR-024),
//! the trader's signature, and the fact that the token accounts passed in are the
//! ones recorded in the vault state.
//!
//! # Inventory is taken from balances, not from state
//!
//! `Vault` deliberately holds no treasury balances: the only truth about a balance
//! is the token accounts themselves. A copy in state would diverge from them after
//! every transfer made around our program (and nobody can forbid such a transfer,
//! see `initialize_vault`), and the hard inventory bound would be computed from an
//! invented number.
//!
//! # On the CU budget
//!
//! SC-002 gives 60 000 CU for the whole instruction, and there is no `msg!` here on
//! the success path: formatting a string costs noticeably more than the arithmetic
//! itself. On error paths it is harmless — the transaction rolls back anyway. The
//! measurement is T021; this is not estimated by eye.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use propamm_quote::{compute_swap, Inventory, Side, SwapRequest};

use crate::errors::VaultError;
use crate::state::{Vault, VAULT_SEED};

/// Swap direction, named from the **trader's** side.
///
/// Its own type rather than `propamm_quote::Side`: in the crate it knows neither
/// Borsh nor the IDL, and pulling an Anchor dependency into pure math for one enum
/// is exactly the trade that would make it unshareable across three consumers.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapSide {
    /// The trader gives the base asset and receives the quote asset.
    BaseToQuote,
    /// The trader gives the quote asset and receives the base asset.
    QuoteToBase,
}

impl From<SwapSide> for Side {
    fn from(side: SwapSide) -> Self {
        match side {
            SwapSide::BaseToQuote => Self::BaseToQuote,
            SwapSide::QuoteToBase => Self::QuoteToBase,
        }
    }
}

/// A swap order.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapArgs {
    pub side: SwapSide,
    /// Amount in, in raw units of the asset the trader gives.
    pub amount_in: u64,
    /// Bound on the acceptable result (FR-009). Zero means "any", and that is the
    /// trader's deliberate choice, not a missing field.
    pub min_amount_out: u64,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    /// The one who swaps. They need not be the owner of the capital — this is the
    /// external order flow.
    pub trader: Signer<'info>,

    /// Vault state. **Not `mut`:** a swap changes no field — inventory lives in the
    /// token accounts, and the quote stays in force until the next update.
    #[account(
        seeds = [
            VAULT_SEED,
            vault.owner.as_ref(),
            vault.base_mint.as_ref(),
            vault.quote_mint.as_ref(),
        ],
        bump = vault.bump,
    )]
    pub vault: Account<'info, Vault>,

    #[account(
        mut,
        constraint = base_treasury.key() == vault.base_vault @ VaultError::AccountMismatch,
    )]
    pub base_treasury: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = quote_treasury.key() == vault.quote_vault @ VaultError::AccountMismatch,
    )]
    pub quote_treasury: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub trader_base_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub trader_quote_account: InterfaceAccount<'info, TokenAccount>,

    #[account(constraint = base_mint.key() == vault.base_mint @ VaultError::AccountMismatch)]
    pub base_mint: InterfaceAccount<'info, Mint>,

    #[account(constraint = quote_mint.key() == vault.quote_mint @ VaultError::AccountMismatch)]
    pub quote_mint: InterfaceAccount<'info, Mint>,

    /// Token program of the base side.
    pub base_token_program: Interface<'info, TokenInterface>,
    /// Token program of the quote side; the pair may be mixed.
    pub quote_token_program: Interface<'info, TokenInterface>,
}

pub fn handle_swap(ctx: Context<Swap>, args: SwapArgs) -> Result<()> {
    let vault = &ctx.accounts.vault;

    // The one guard that is not in the math: the crate cannot see chain state.
    require!(!vault.halted, VaultError::VaultHalted);

    let inventory = Inventory {
        base_amount: ctx.accounts.base_treasury.amount,
        quote_amount: ctx.accounts.quote_treasury.amount,
    };
    let request = SwapRequest {
        side: args.side.into(),
        amount_in: args.amount_in,
        min_amount_out: args.min_amount_out,
    };

    let result = compute_swap(
        &vault.quote_params(),
        &inventory,
        &request,
        Clock::get()?.slot,
    )
    .map_err(|err| {
        msg!("swap rejected: {}", err);
        VaultError::from(err)
    })?;

    // Input first, then output. The order is not cosmetic: if the trader's transfer
    // fails, the vault has not yet given anything away, and the rollback does not
    // depend on how carefully the token program behaves mid-transaction.
    let (
        pay_in_from,
        pay_in_to,
        pay_in_mint,
        pay_in_program,
        pay_out_from,
        pay_out_to,
        pay_out_mint,
        pay_out_program,
    ) = match args.side {
        SwapSide::BaseToQuote => (
            ctx.accounts.trader_base_account.to_account_info(),
            ctx.accounts.base_treasury.to_account_info(),
            &ctx.accounts.base_mint,
            ctx.accounts.base_token_program.to_account_info(),
            ctx.accounts.quote_treasury.to_account_info(),
            ctx.accounts.trader_quote_account.to_account_info(),
            &ctx.accounts.quote_mint,
            ctx.accounts.quote_token_program.to_account_info(),
        ),
        SwapSide::QuoteToBase => (
            ctx.accounts.trader_quote_account.to_account_info(),
            ctx.accounts.quote_treasury.to_account_info(),
            &ctx.accounts.quote_mint,
            ctx.accounts.quote_token_program.to_account_info(),
            ctx.accounts.base_treasury.to_account_info(),
            ctx.accounts.trader_base_account.to_account_info(),
            &ctx.accounts.base_mint,
            ctx.accounts.base_token_program.to_account_info(),
        ),
    };

    transfer_checked(
        CpiContext::new(
            pay_in_program.key(),
            TransferChecked {
                from: pay_in_from,
                mint: pay_in_mint.to_account_info(),
                to: pay_in_to,
                authority: ctx.accounts.trader.to_account_info(),
            },
        ),
        result.amount_in,
        pay_in_mint.decimals,
    )?;

    let owner = vault.owner;
    let base_mint_key = vault.base_mint;
    let quote_mint_key = vault.quote_mint;
    let bump = [vault.bump];
    let seeds: [&[u8]; 5] = [
        VAULT_SEED,
        owner.as_ref(),
        base_mint_key.as_ref(),
        quote_mint_key.as_ref(),
        &bump,
    ];
    let signer: [&[&[u8]]; 1] = [&seeds];

    transfer_checked(
        CpiContext::new_with_signer(
            pay_out_program.key(),
            TransferChecked {
                from: pay_out_from,
                mint: pay_out_mint.to_account_info(),
                to: pay_out_to,
                authority: vault.to_account_info(),
            },
            &signer,
        ),
        result.amount_out,
        pay_out_mint.decimals,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use propamm_quote::QuoteError;

    #[test]
    fn sides_map_to_the_math_without_a_twist() {
        assert_eq!(Side::from(SwapSide::BaseToQuote), Side::BaseToQuote);
        assert_eq!(Side::from(SwapSide::QuoteToBase), Side::QuoteToBase);
    }

    /// Every math refusal has its own program code. A sentinel test, because
    /// swapped `match` arms would pass the compiler.
    #[test]
    fn every_math_error_maps_to_its_own_code() {
        let cases = [
            (QuoteError::QuoteNotSet, "QuoteNotSet"),
            (QuoteError::InvalidParams, "InvalidQuote"),
            (QuoteError::QuoteStale, "QuoteStale"),
            (QuoteError::SizeExceeded, "SizeExceeded"),
            (QuoteError::SlippageExceeded, "SlippageExceeded"),
            (QuoteError::InventoryBound, "InventoryBound"),
            (QuoteError::InsufficientLiquidity, "InsufficientLiquidity"),
            (QuoteError::AmountTooSmall, "AmountTooSmall"),
            (QuoteError::Overflow, "MathOverflow"),
        ];
        for (err, expected) in cases {
            assert_eq!(format!("{:?}", VaultError::from(err)), expected);
        }
    }

    #[test]
    fn args_survive_a_round_trip() {
        let a = SwapArgs {
            side: SwapSide::QuoteToBase,
            amount_in: 1_000_000,
            min_amount_out: 999,
        };
        let mut bytes = Vec::new();
        a.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 1 + 8 + 8);
        assert_eq!(SwapArgs::deserialize(&mut bytes.as_slice()).unwrap(), a);
    }
}
