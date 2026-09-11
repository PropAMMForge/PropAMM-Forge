//! The external order flow.
//!
//! A swap is not part of `forge` and must not be: the CLI has six commands
//! (`init`/`deploy`/`fund`/`quote`/`status`/`halt`), and none of them swaps —
//! swapping is done by a **third party**, most often the router (US3). So the
//! trader here is our own: it assembles the instruction by exactly the same path
//! the adapter will — `anchor_lang::InstructionData` over the program's types,
//! with no second copy of the layout.

use anchor_lang::prelude::Pubkey;
use anchor_lang::{InstructionData, ToAccountMetas};
use anyhow::{Context, Result};
use propamm_cli::chain::{associated_token_address, decode_token_account, MintInfo};
use propamm_cli::rpc::{Confirmed, Rpc};
use propamm_vault::instructions::swap::{SwapArgs, SwapSide};
use propamm_vault::state::Vault;
use solana_keypair::Keypair;
use solana_signer::Signer as _;

use crate::mints::{Holdings, Pair};
use crate::net;

/// Everything one needs to know about the venue to assemble an order.
///
/// A separate struct rather than seven arguments: the treasury addresses derive
/// from the vault address, which derives from the owner and the pair, and
/// whoever assembles this set by hand has a chance of taking a treasury from
/// another vault. Here it is derived once and in full.
pub struct Market {
    pub program_id: Pubkey,
    pub vault: Pubkey,
    pub treasuries: Holdings,
    pub base: MintInfo,
    pub quote: MintInfo,
}

impl Market {
    /// Derive the venue from what a third party knows: the program, the owner,
    /// the pair. That is exactly the set a router holds after listing.
    #[must_use]
    pub fn derive(owner: &Pubkey, pair: &Pair) -> Self {
        let vault = Vault::pda(owner, &pair.base.address, &pair.quote.address).0;
        Self {
            program_id: propamm_vault::ID,
            vault,
            treasuries: Holdings {
                base: associated_token_address(
                    &vault,
                    &pair.base.address,
                    &pair.base.token_program,
                ),
                quote: associated_token_address(
                    &vault,
                    &pair.quote.address,
                    &pair.quote.token_program,
                ),
            },
            base: pair.base,
            quote: pair.quote,
        }
    }
}

/// A participant who brings an order from outside.
pub struct Trader {
    pub keypair: Keypair,
    pub holdings: Holdings,
}

impl Trader {
    #[must_use]
    pub const fn new(keypair: Keypair, holdings: Holdings) -> Self {
        Self { keypair, holdings }
    }

    #[must_use]
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Execute a swap.
    ///
    /// # Errors
    ///
    /// If the transaction failed — including a program refusal, in which case the
    /// message carries its code.
    pub fn swap(&self, rpc: &Rpc, market: &Market, args: SwapArgs) -> Result<Confirmed> {
        let instruction = swap_ix(market, &self.pubkey(), self.holdings, args);
        net::send(rpc, &[instruction], &[&self.keypair]).context("the swap failed")
    }
}

/// The `swap` instruction.
///
/// The account order is not listed by hand: `accounts::Swap` requires every
/// field, and `to_account_metas` lays them out the same way the program reads them.
#[must_use]
pub fn swap_ix(
    market: &Market,
    trader: &Pubkey,
    holdings: Holdings,
    args: SwapArgs,
) -> anchor_lang::solana_program::instruction::Instruction {
    let accounts = propamm_vault::accounts::Swap {
        trader: *trader,
        vault: market.vault,
        base_treasury: market.treasuries.base,
        quote_treasury: market.treasuries.quote,
        trader_base_account: holdings.base,
        trader_quote_account: holdings.quote,
        base_mint: market.base.address,
        quote_mint: market.quote.address,
        base_token_program: market.base.token_program,
        quote_token_program: market.quote.token_program,
    };
    anchor_lang::solana_program::instruction::Instruction {
        program_id: market.program_id,
        accounts: accounts.to_account_metas(None),
        data: propamm_vault::instruction::Swap { args }.data(),
    }
}

/// Raw balance of a token account.
///
/// # Errors
///
/// If the account is missing or does not parse.
pub fn token_balance(rpc: &Rpc, address: &Pubkey) -> Result<u64> {
    let account = rpc
        .get_account(address)?
        .with_context(|| format!("no token account {address}"))?;
    Ok(decode_token_account(address, &account)?.amount)
}

/// An order in one line — so the test writes the direction once.
#[must_use]
pub const fn request(side: SwapSide, amount_in: u64, min_amount_out: u64) -> SwapArgs {
    SwapArgs {
        side,
        amount_in,
        min_amount_out,
    }
}
