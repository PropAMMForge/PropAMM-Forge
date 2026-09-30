//! A session with the network: project config, key and node in one place.
//!
//! # What is not done here
//!
//! No bytecode is uploaded. There is one `propamm_vault` program on the network
//! (FR-002a), and `forge deploy` creates an **account** in it, not a program. So
//! the session can only check that the program at the address from the config
//! really exists and is executable: on a local network that is the most common
//! cause of refusal, and without a separate check it would arrive as "Attempt
//! to load a program that does not exist" from simulation — correct, but not explanatory.
//!
//! The instruction builders and the account decoders are not here either: the
//! engine needs the same ones, so they live in [`propamm_client::chain`].

use std::path::{Path, PathBuf};

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use propamm_client::chain::{decode_mint, decode_vault, read_keypair, MintInfo};
use propamm_client::rpc::{Confirmed, Rpc};
use solana_keypair::Keypair;
use solana_signer::Signer as _;

use crate::config::{ProjectConfig, VaultEntry};

/// Config, key and node — everything the commands that go to the network need.
pub struct Session {
    pub dir: PathBuf,
    pub config: ProjectConfig,
    pub rpc: Rpc,
    owner: Keypair,
}

impl Session {
    /// Open the project in a directory.
    ///
    /// # Errors
    ///
    /// If there is no config, the owner key cannot be read or **does not match**
    /// the address in the config.
    pub fn open(dir: &Path) -> Result<Self> {
        let config = ProjectConfig::read_from(dir)?;
        let owner = read_keypair(Path::new(&config.owner.keypair))?;

        // The promise from `config::Owner`: a mismatch between the address and the key
        // means the config describes one vault while the command signs another. The PDA
        // derives from the owner's address, so without this check `deploy` would create
        // a vault the present key has no access to, and it would look like success.
        let signing = owner.pubkey();
        if signing != config.owner.pubkey.0 {
            bail!(
                "key {} signs as {signing}, while the config names {} as the owner",
                config.owner.keypair,
                config.owner.pubkey
            );
        }

        let rpc = Rpc::new(config.network.rpc_url.clone());
        Ok(Self {
            dir: dir.to_path_buf(),
            config,
            rpc,
            owner,
        })
    }

    #[must_use]
    pub fn owner_key(&self) -> &Keypair {
        &self.owner
    }

    #[must_use]
    pub fn owner(&self) -> Pubkey {
        self.owner.pubkey()
    }

    #[must_use]
    pub fn program_id(&self) -> Pubkey {
        self.config.network.program_id.0
    }

    /// The vault address for writing the config.
    ///
    /// The seeds come from the same function as in the program
    /// ([`propamm_vault::state::Vault::pda`]), but `program_id` comes from the config
    /// rather than `declare_id!`: on a local network the program lives at its own
    /// address, and a PDA from the baked-in ID would point elsewhere.
    #[must_use]
    pub fn vault_pda(&self, entry: &VaultEntry) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &propamm_vault::state::Vault::seeds(
                &self.config.owner.pubkey.0,
                &entry.base_mint.0,
                &entry.quote_mint.0,
            ),
            &self.program_id(),
        )
    }

    /// Make sure the program at the address from the config is really there and executable.
    ///
    /// # Errors
    ///
    /// If the account is missing or is not a program.
    pub fn ensure_program_deployed(&self) -> Result<()> {
        let program_id = self.program_id();
        let Some(account) = self.rpc.get_account(&program_id)? else {
            bail!(
                "program {program_id} is not on the {} network ({})\n\
                 on {} the product owner deploys it once for everyone; on a local network — solana program deploy",
                self.config.network.cluster,
                self.rpc.url(),
                self.config.network.cluster,
            );
        };
        if !account.executable {
            bail!("account {program_id} exists but is not a program — check program_id in propamm.toml");
        }
        Ok(())
    }

    /// The vault state, if it is deployed.
    ///
    /// # Errors
    ///
    /// If the account exists but does not parse as a `Vault` — in particular when
    /// `program_id` in the config points at something else.
    pub fn read_vault(&self, address: &Pubkey) -> Result<Option<propamm_vault::state::Vault>> {
        let Some(account) = self.rpc.get_account(address)? else {
            return Ok(None);
        };
        decode_vault(address, &account).map(Some)
    }

    /// A mint: how many decimals it has and whose program it is.
    ///
    /// # Errors
    ///
    /// If the mint is not on the network or belongs to a non-token program.
    pub fn read_mint(&self, address: &Pubkey) -> Result<MintInfo> {
        let account = self
            .rpc
            .get_account(address)?
            .with_context(|| format!("mint {address} is not on the network {}", self.rpc.url()))?;
        decode_mint(address, &account)
    }

    /// Both sides of the pair in one request.
    ///
    /// # Errors
    ///
    /// If either side is missing or is not a mint; the message names the side —
    /// the same way `init::parse_pair` does.
    pub fn read_pair(&self, entry: &VaultEntry) -> Result<(MintInfo, MintInfo)> {
        let addresses = [entry.base_mint.0, entry.quote_mint.0];
        let accounts = self.rpc.get_multiple_accounts(&addresses)?;
        let base = accounts[0]
            .as_ref()
            .with_context(|| format!("base mint {} is not on the network", entry.base_mint))?;
        let quote = accounts[1]
            .as_ref()
            .with_context(|| format!("quote mint {} is not on the network", entry.quote_mint))?;
        Ok((
            decode_mint(&addresses[0], base).context("base side of the pair")?,
            decode_mint(&addresses[1], quote).context("quote side of the pair")?,
        ))
    }

    /// Assemble, sign and send a transaction; the first signer pays.
    ///
    /// # Errors
    ///
    /// If the node refused the transaction or it did not confirm.
    pub fn send(
        &self,
        instructions: &[anchor_lang::solana_program::instruction::Instruction],
        signers: &[&Keypair],
    ) -> Result<Confirmed> {
        let payer = signers
            .first()
            .context("transaction without a single signer")?;
        let blockhash = self.rpc.get_latest_blockhash()?;
        let message = solana_message::Message::new(instructions, Some(&payer.pubkey()));
        let mut transaction = solana_transaction::Transaction::new_unsigned(message);
        transaction
            .try_sign(signers, blockhash)
            .context("transaction cannot be signed")?;
        let wire = bincode::serialize(&transaction).context("transaction does not serialize")?;
        let signature = self.rpc.send_transaction(&wire)?;
        self.rpc.confirm(&signature)
    }
}
