//! The pair's mints and the participants' accounts — a precondition of the run, not its step.
//!
//! The pair is deliberately **mixed**: the base asset on classic SPL, the quote
//! asset on Token-2022, and with different `decimals`. That is the cheapest way
//! to keep the run from passing on a symmetric case: both token programs land in
//! one swap transaction, and different `decimals` catch swapped sides in the price
//! conversion — with equal decimals such a mistake yields the same numbers.

use anchor_lang::prelude::Pubkey;
use anyhow::{Context, Result};
use propamm_cli::chain::{associated_token_address, MintInfo};
use propamm_cli::rpc::Rpc;
use solana_keypair::Keypair;
use solana_signer::Signer as _;

use crate::net;

/// A mint without extensions. Token-2022 keeps the same length while there are no extensions.
const MINT_LEN: usize = 82;

/// A pair ready for `forge init`.
pub struct Pair {
    pub base: MintInfo,
    pub quote: MintInfo,
    /// The right to mint both sides. Stays in the harness: the trader needs topping
    /// up, and doing that through a separate key would mean keeping two.
    authority: Keypair,
}

impl Pair {
    /// Create the pair: SPL on the left, Token-2022 on the right.
    ///
    /// # Errors
    ///
    /// If the mint creation transaction failed.
    pub fn create(
        rpc: &Rpc,
        payer: &Keypair,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> Result<Self> {
        let authority = payer.insecure_clone();
        let base = create_mint(rpc, payer, anchor_spl::token::ID, base_decimals)
            .context("cannot create the base mint")?;
        let quote = create_mint(rpc, payer, anchor_spl::token_2022::ID, quote_decimals)
            .context("cannot create the quote mint")?;
        Ok(Self {
            base,
            quote,
            authority,
        })
    }

    /// The pair as `BASE/QUOTE` — exactly the way `forge init --pair` takes it.
    #[must_use]
    pub fn as_argument(&self) -> String {
        format!("{}/{}", self.base.address, self.quote.address)
    }

    /// Open both ATAs for a wallet and put the starting amounts on them.
    ///
    /// The amounts are in raw units: the harness has no second copy of the
    /// conversion through `decimals`, because that very conversion is what is checked in `forge`.
    ///
    /// # Errors
    ///
    /// If the transaction failed.
    pub fn open_and_fill(
        &self,
        rpc: &Rpc,
        payer: &Keypair,
        wallet: &Pubkey,
        base_raw: u64,
        quote_raw: u64,
    ) -> Result<Holdings> {
        let holdings = Holdings {
            base: associated_token_address(wallet, &self.base.address, &self.base.token_program),
            quote: associated_token_address(wallet, &self.quote.address, &self.quote.token_program),
        };

        // One transaction for both sides: the ATA creation and the minting fit, and
        // every extra transaction is one more blockhash and one more wait for
        // confirmation in the preparation, which is already the longest part of the
        // run.
        let mut instructions = vec![
            create_ata(payer, wallet, &self.base),
            create_ata(payer, wallet, &self.quote),
        ];
        if base_raw > 0 {
            instructions.push(mint_to(
                &self.base,
                &holdings.base,
                &self.authority.pubkey(),
                base_raw,
            ));
        }
        if quote_raw > 0 {
            instructions.push(mint_to(
                &self.quote,
                &holdings.quote,
                &self.authority.pubkey(),
                quote_raw,
            ));
        }
        net::send(rpc, &instructions, &[payer, &self.authority])
            .with_context(|| format!("cannot open accounts for {wallet}"))?;
        Ok(holdings)
    }
}

/// Two token accounts of one wallet.
#[derive(Clone, Copy, Debug)]
pub struct Holdings {
    pub base: Pubkey,
    pub quote: Pubkey,
}

/// Create a mint in the given token program.
fn create_mint(
    rpc: &Rpc,
    payer: &Keypair,
    token_program: Pubkey,
    decimals: u8,
) -> Result<MintInfo> {
    let mint = Keypair::new();
    let lamports = net::rent_exempt(rpc, MINT_LEN)?;

    let create = anchor_lang::solana_program::system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        lamports,
        MINT_LEN as u64,
        &token_program,
    );
    // The builder from `spl-token-2022-interface` takes `token_program` as a parameter
    // and thereby works for both programs: the `InitializeMint2` layout is shared.
    let initialize = anchor_spl::token_2022::spl_token_2022::instruction::initialize_mint2(
        &token_program,
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        decimals,
    )
    .context("initialize_mint2 does not assemble")?;

    net::send(rpc, &[create, initialize], &[payer, &mint])?;
    Ok(MintInfo {
        address: mint.pubkey(),
        decimals,
        token_program,
    })
}

fn create_ata(
    payer: &Keypair,
    wallet: &Pubkey,
    mint: &MintInfo,
) -> anchor_lang::solana_program::instruction::Instruction {
    anchor_spl::associated_token::spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        wallet,
        &mint.address,
        &mint.token_program,
    )
}

fn mint_to(
    mint: &MintInfo,
    account: &Pubkey,
    authority: &Pubkey,
    amount: u64,
) -> anchor_lang::solana_program::instruction::Instruction {
    anchor_spl::token_2022::spl_token_2022::instruction::mint_to(
        &mint.token_program,
        &mint.address,
        account,
        authority,
        &[],
        amount,
    )
    .expect("mint_to assembles from constant arguments")
}
