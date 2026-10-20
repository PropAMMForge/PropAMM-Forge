//! The swap instruction for a route (FR-017, T036).
//!
//! # Why not [`Amm::get_swap_and_account_metas`]
//!
//! That method answers with a [`jupiter_amm_interface::Swap`] variant, and there
//! is none for this program until Jupiter lists it (T034 decision). The builder
//! here takes the same [`SwapParams`] the router would hand to that method and
//! returns the instruction itself — so the router twin (T037) and a future listing
//! feed it the same input, and the listed version becomes a thin wrapper.
//!
//! # What it decides and what it does not
//!
//! The account order is the program's own: [`propamm_vault::accounts::Swap`]
//! requires every field, and `to_account_metas` lays them out the way the program
//! reads them. The signer is the **transfer authority**, not the user: inside a
//! route the one who may debit the source account is often the router's PDA.
//!
//! The bound on the result (FR-009) is an argument, not something read from
//! [`SwapParams::out_amount`]. Whoever calls decides it: a router twin that ends
//! the route passes `out × (1 − slippage)`, a Jupiter-shaped wrapper passes 0,
//! because there the aggregator program bounds the whole route. Taking `out_amount`
//! as the bound would fail the hop on any inventory move between quote and
//! execution that the route's own tolerance would accept (T036 decision).
//!
//! The builder does not re-check the quote, its age or the halted flag: the venue
//! was chosen by [`Amm::quote`], and the chain refuses on its own — a second
//! set of checks here would be one more place to diverge from it.
//!
//! [`Amm::get_swap_and_account_metas`]: jupiter_amm_interface::Amm::get_swap_and_account_metas
//! [`Amm::quote`]: jupiter_amm_interface::Amm::quote

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use anyhow::{ensure, Context, Result};
use jupiter_amm_interface::{SwapMode, SwapParams};
use propamm_quote::Side;
use propamm_vault::instructions::swap::{SwapArgs, SwapSide};

use crate::PropAmm;

impl PropAmm {
    /// The `swap` instruction for one hop of a route.
    ///
    /// The side follows from `source_mint` → `destination_mint`; `in_amount` is
    /// the amount in; `token_transfer_authority` signs; the trader's token
    /// accounts are `source_token_account` and `destination_token_account`.
    ///
    /// # Errors
    ///
    /// Exact-out, a pair the vault does not trade, or no [`Amm::update`] yet — the
    /// token programs of the treasuries are not known before it.
    ///
    /// [`Amm::update`]: jupiter_amm_interface::Amm::update
    pub fn swap_instruction(
        &self,
        params: &SwapParams,
        min_amount_out: u64,
    ) -> Result<Instruction> {
        ensure!(
            params.swap_mode == SwapMode::ExactIn,
            "the vault swaps exact-in only"
        );
        let side = self.side(&params.source_mint, &params.destination_mint)?;
        let programs = self.token_programs.with_context(|| {
            format!(
                "vault {} has not been updated yet: the token programs of its treasuries \
                 are not known",
                self.key
            )
        })?;

        let (side, trader_base_account, trader_quote_account) = match side {
            Side::BaseToQuote => (
                SwapSide::BaseToQuote,
                params.source_token_account,
                params.destination_token_account,
            ),
            Side::QuoteToBase => (
                SwapSide::QuoteToBase,
                params.destination_token_account,
                params.source_token_account,
            ),
        };
        let accounts = propamm_vault::accounts::Swap {
            trader: params.token_transfer_authority,
            vault: self.key,
            base_treasury: self.base_vault,
            quote_treasury: self.quote_vault,
            trader_base_account,
            trader_quote_account,
            base_mint: self.base_mint,
            quote_mint: self.quote_mint,
            base_token_program: programs.base,
            quote_token_program: programs.quote,
        };
        let args = SwapArgs {
            side,
            amount_in: params.in_amount,
            min_amount_out,
        };
        Ok(Instruction {
            program_id: propamm_vault::ID,
            accounts: accounts.to_account_metas(None),
            data: propamm_vault::instruction::Swap { args }.data(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        context, keyed, updated, vault, BASE_MINT, BASE_VAULT, KEY, QUOTE_MINT, QUOTE_SLOT,
        QUOTE_VAULT,
    };
    use crate::{TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
    use anchor_lang::prelude::{AccountMeta, Pubkey};
    use jupiter_amm_interface::Amm;

    const AUTHORITY: Pubkey = Pubkey::new_from_array([22; 32]);
    const USER: Pubkey = Pubkey::new_from_array([23; 32]);
    const SOURCE: Pubkey = Pubkey::new_from_array([20; 32]);
    const DESTINATION: Pubkey = Pubkey::new_from_array([21; 32]);
    const JUPITER: Pubkey = Pubkey::new_from_array([77; 32]);
    /// What the router quoted for the hop — deliberately unlike any bound below,
    /// so a builder that took it as the bound is caught.
    const QUOTED_OUT: u64 = 149_000_000;

    fn hop(source_mint: Pubkey, destination_mint: Pubkey) -> SwapParams<'static, 'static> {
        SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1_000_000_000,
            out_amount: QUOTED_OUT,
            source_mint,
            destination_mint,
            source_token_account: SOURCE,
            destination_token_account: DESTINATION,
            token_transfer_authority: AUTHORITY,
            // A different key from the authority: the instruction must name the
            // one who may debit the source account, not the end user of the route.
            user: USER,
            payer: USER,
            quote_mint_to_referrer: None,
            jupiter_program_id: &JUPITER,
            missing_dynamic_accounts_as_default: false,
        }
    }

    /// The accounts the program expects, written out field by field from the
    /// fixture rather than from the builder's own mapping.
    fn expected(trader_base: Pubkey, trader_quote: Pubkey) -> Vec<AccountMeta> {
        propamm_vault::accounts::Swap {
            trader: AUTHORITY,
            vault: KEY,
            base_treasury: BASE_VAULT,
            quote_treasury: QUOTE_VAULT,
            trader_base_account: trader_base,
            trader_quote_account: trader_quote,
            base_mint: BASE_MINT,
            quote_mint: QUOTE_MINT,
            // The fixture's base treasury is classic SPL Token, the quote one Token-2022.
            base_token_program: TOKEN_PROGRAM,
            quote_token_program: TOKEN_2022_PROGRAM,
        }
        .to_account_metas(None)
    }

    fn data(side: SwapSide, amount_in: u64, min_amount_out: u64) -> Vec<u8> {
        propamm_vault::instruction::Swap {
            args: SwapArgs {
                side,
                amount_in,
                min_amount_out,
            },
        }
        .data()
    }

    #[test]
    fn base_to_quote_debits_the_source_as_base() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let ix = amm
            .swap_instruction(&hop(BASE_MINT, QUOTE_MINT), 7)
            .unwrap();
        assert_eq!(ix.program_id, propamm_vault::ID);
        assert_eq!(ix.accounts, expected(SOURCE, DESTINATION));
        assert_eq!(ix.data, data(SwapSide::BaseToQuote, 1_000_000_000, 7));
    }

    #[test]
    fn quote_to_base_debits_the_source_as_quote() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let ix = amm
            .swap_instruction(&hop(QUOTE_MINT, BASE_MINT), 7)
            .unwrap();
        assert_eq!(ix.accounts, expected(DESTINATION, SOURCE));
        assert_eq!(ix.data, data(SwapSide::QuoteToBase, 1_000_000_000, 7));
    }

    #[test]
    fn only_the_transfer_authority_signs() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let ix = amm
            .swap_instruction(&hop(BASE_MINT, QUOTE_MINT), 0)
            .unwrap();
        let signers: Vec<Pubkey> = ix
            .accounts
            .iter()
            .filter(|meta| meta.is_signer)
            .map(|meta| meta.pubkey)
            .collect();
        assert_eq!(signers, vec![AUTHORITY]);
        assert!(ix.accounts.iter().all(|meta| meta.pubkey != USER));
    }

    #[test]
    fn the_bound_is_the_callers_not_the_quote() {
        let amm = updated(&vault(), QUOTE_SLOT);
        for bound in [0, 1, QUOTED_OUT - 1, u64::MAX] {
            let ix = amm
                .swap_instruction(&hop(BASE_MINT, QUOTE_MINT), bound)
                .unwrap();
            assert_eq!(
                ix.data,
                data(SwapSide::BaseToQuote, 1_000_000_000, bound),
                "{bound}"
            );
        }
    }

    #[test]
    fn instruction_carries_as_many_accounts_as_the_router_is_told() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let ix = amm
            .swap_instruction(&hop(BASE_MINT, QUOTE_MINT), 0)
            .unwrap();
        assert_eq!(ix.accounts.len(), amm.get_accounts_len());
    }

    #[test]
    fn refuses_what_it_cannot_build() {
        let fresh = PropAmm::from_keyed_account(&keyed(&vault()), &context(QUOTE_SLOT)).unwrap();
        let why = fresh
            .swap_instruction(&hop(BASE_MINT, QUOTE_MINT), 0)
            .unwrap_err();
        assert!(why.to_string().contains("not been updated"), "{why}");

        let amm = updated(&vault(), QUOTE_SLOT);
        let mut exact_out = hop(BASE_MINT, QUOTE_MINT);
        exact_out.swap_mode = SwapMode::ExactOut;
        assert!(amm.swap_instruction(&exact_out, 0).is_err());

        let stranger = Pubkey::new_from_array([99; 32]);
        assert!(amm.swap_instruction(&hop(stranger, QUOTE_MINT), 0).is_err());
        assert!(amm.swap_instruction(&hop(BASE_MINT, BASE_MINT), 0).is_err());
    }
}
