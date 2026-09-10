//! `forge fund` — fund a vault treasury with the owner's own capital (FR-003).
//!
//! # Why the command creates nothing
//!
//! In `deposit` the owner's account is the **source** of the transfer. A freshly
//! created empty ATA lets nothing be put into the vault: the very same transaction
//! would fail on a zero balance as the next step. So the command creates no
//! accounts and refuses, naming what is missing — and names the wSOL case
//! separately, where "no account" means "SOL is not wrapped yet", and wrapping
//! moves the owner's own lamports outside the vault and must not happen by itself.
//!
//! `--from` exists for those who hold the asset somewhere other than the ATA: the
//! program requires of the account only that the vault owner controls it.
//!
//! # The side comes from state, not from the config
//!
//! The command reads the mint and the treasury from the `Vault` account, even
//! though the same addresses are in `propamm.toml`. The config is intent, the
//! state is what is really deployed; when they diverge (a config edit after
//! `deploy`), the transfer must go where the program looks, or not go at all.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};

use crate::amount::{format_raw, Decimal};
use crate::chain::{
    associated_token_address, decode_token_account, deposit_ix, MintInfo, Session, NATIVE_MINT,
};
use crate::rpc::Confirmed;

/// The side of the pair being funded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum Side {
    Base,
    Quote,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Base => "base",
            Self::Quote => "quote",
        })
    }
}

/// Command arguments after parsing.
#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    pub vault: Option<String>,
    pub side: Side,
    /// Amount in human units of that side's asset.
    pub amount: Decimal,
    /// Source account; defaults to the owner's ATA.
    pub from: Option<Pubkey>,
}

/// What was transferred.
#[derive(Clone, Debug)]
pub struct Funded {
    pub name: String,
    pub vault: Pubkey,
    pub side: Side,
    pub mint: MintInfo,
    pub from: Pubkey,
    pub treasury: Pubkey,
    /// The raw amount that went into the transaction.
    pub amount: u64,
    /// Treasury balance before the transfer — so it is visible what exactly changed.
    pub treasury_before: u64,
    pub confirmed: Confirmed,
}

/// Run `forge fund`.
///
/// # Errors
///
/// If the vault is not deployed, the amount is finer than the mint's smallest
/// unit, the source account is missing, foreign, of the wrong side or holds too little.
pub fn run(options: &Options) -> Result<Funded> {
    let session = Session::open(&options.path)?;
    let entry = session.config.select(options.vault.as_deref())?.clone();

    let (vault_address, _bump) = session.vault_pda(&entry);
    let vault = session.read_vault(&vault_address)?.with_context(|| {
        format!(
            "vault \"{}\" is not deployed yet ({vault_address}) — first: forge deploy --vault {}",
            entry.name, entry.name
        )
    })?;

    let (mint_address, treasury) = match options.side {
        Side::Base => (vault.base_mint, vault.base_vault),
        Side::Quote => (vault.quote_mint, vault.quote_vault),
    };
    let mint = session.read_mint(&mint_address)?;
    let amount = options.amount.to_raw(mint.decimals)?;

    let from = options.from.unwrap_or_else(|| {
        associated_token_address(&session.owner(), &mint_address, &mint.token_program)
    });

    let source = session.rpc.get_account(&from)?;
    let Some(source) = source else {
        if mint_address == NATIVE_MINT {
            bail!(
                "the owner has no wSOL account ({from}).\n\
                 to put SOL into a vault it has to be wrapped first — that moves your lamports, and forge does not do it for you:\n\
                 \x20   spl-token wrap {}\n\
                 then repeat the command",
                options.amount
            );
        }
        bail!(
            "account {from} does not exist — it should hold the asset {mint_address} you are putting into the vault.\n\
             create it: spl-token create-account {mint_address}\n\
             or name your own account: forge fund --from <address>"
        );
    };

    let source_info = decode_token_account(&from, &source)?;
    if source_info.mint != mint_address {
        bail!(
            "account {from} holds asset {}, while the {} side of the pair is {mint_address}",
            source_info.mint,
            options.side
        );
    }
    if source_info.owner != session.owner() {
        bail!(
            "account {from} is controlled by {}, not by the vault owner {} — the program would refuse this transfer",
            source_info.owner,
            session.owner()
        );
    }
    if source_info.amount < amount {
        bail!(
            "account {from} holds only {}, while {} has to be transferred",
            format_raw(source_info.amount, mint.decimals),
            format_raw(amount, mint.decimals)
        );
    }

    let treasury_before = session
        .rpc
        .get_account(&treasury)?
        .map(|account| decode_token_account(&treasury, &account))
        .transpose()?
        .map_or(0, |info| info.amount);

    let instruction = deposit_ix(
        &session.program_id(),
        &session.owner(),
        &vault_address,
        &mint,
        &treasury,
        &from,
        amount,
    );
    let confirmed = session.send(&[instruction], &[session.owner_key()])?;

    Ok(Funded {
        name: entry.name,
        vault: vault_address,
        side: options.side,
        mint,
        from,
        treasury,
        amount,
        treasury_before,
        confirmed,
    })
}
