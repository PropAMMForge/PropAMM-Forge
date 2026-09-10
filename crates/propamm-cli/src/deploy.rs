//! `forge deploy` — create a vault on the network (FR-002).
//!
//! # "Deployment" here is not about bytecode
//!
//! There is one program on the network for everyone (FR-002a), and the command
//! creates an **account** in it: a PDA of the owner and the pair plus two treasury
//! ATAs owned by that PDA. So a deployment costs rent for three small accounts
//! rather than several SOL for bytecode — and only because of that is SC-001 (15 minutes) reachable at all.
//!
//! # What is checked before the transaction
//!
//! Three things, each of which would otherwise arrive as an error about something else:
//!
//! - **the program is in place** — otherwise simulation says "Attempt to load a
//!   program that does not exist", which on a local network is true but no hint;
//! - **the vault does not exist yet** — otherwise Anchor refuses on `init` with
//!   an already-in-use account code, which most often means "you already deployed, see
//!   `forge status`»;
//! - **both mints exist and belong to token programs** — otherwise `deploy`
//!   fails inside the CPI while deserializing a foreign account.
//!
//! Asset screening per FR-005 is deliberately **not** duplicated: `mint_guard`
//! lives in the program, and a copy of its rules in the CLI would diverge from it.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Result};

use crate::chain::{associated_token_address, initialize_vault_ix, MintInfo, Session};
use crate::rpc::Confirmed;

/// Command arguments after parsing.
#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    /// Pair selector; may be omitted if the project has exactly one vault.
    pub vault: Option<String>,
}

/// What appeared on the network.
#[derive(Clone, Debug)]
pub struct Deployed {
    pub name: String,
    pub address: Pubkey,
    pub base: MintInfo,
    pub quote: MintInfo,
    pub base_treasury: Pubkey,
    pub quote_treasury: Pubkey,
    pub confirmed: Confirmed,
}

/// Run `forge deploy`.
///
/// # Errors
///
/// If there is no project, the program is not on the network, the vault is
/// already deployed, the mints cannot be read or the transaction failed.
pub fn run(options: &Options) -> Result<Deployed> {
    let session = Session::open(&options.path)?;
    let entry = session.config.select(options.vault.as_deref())?.clone();

    session.ensure_program_deployed()?;

    let (address, _bump) = session.vault_pda(&entry);
    if session.read_vault(&address)?.is_some() {
        bail!(
            "vault \"{}\" is already deployed at {address} — see: forge status --vault {}",
            entry.name,
            entry.name
        );
    }

    let (base, quote) = session.read_pair(&entry)?;

    let args = propamm_vault::instructions::initialize_vault::InitializeVaultArgs {
        pricing_authority: entry.pricing_authority.0,
        halt_authority: entry.halt_authority.0,
        max_quote_age_slots: entry.max_quote_age_slots,
        max_skew_bps: entry.max_skew_bps,
    };
    let instruction = initialize_vault_ix(
        &session.program_id(),
        &session.owner(),
        &address,
        &base,
        &quote,
        args,
    );
    let confirmed = session.send(&[instruction], &[session.owner_key()])?;

    Ok(Deployed {
        name: entry.name,
        address,
        base,
        quote,
        base_treasury: associated_token_address(&address, &base.address, &base.token_program),
        quote_treasury: associated_token_address(&address, &quote.address, &quote.token_program),
        confirmed,
    })
}
