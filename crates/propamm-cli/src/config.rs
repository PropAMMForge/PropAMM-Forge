//! `propamm.toml` — the project config, which holds a **list** of vaults (FR-004a).
//!
//! # Why the config is written by serialization, not from a template
//!
//! A file assembled from strings and a file the CLI can read are two different
//! things exactly until someone adds a field to one of them. Here one structure
//! is written and read, and a test does the round-trip: `init` → read →
//! compare. It is the same trap that makes every instruction builder need a
//! reverse decode.
//!
//! Only `README.md` remains a template — nobody reads it programmatically.
//!
//! # Keys in the config are base58 strings, not bytes
//!
//! `Pubkey` serializes as an array of 32 numbers, and in TOML that would look
//! like a line of garbage a human cannot check by eye against what the wallet shows.
//! [`ConfigKey`] writes base58 and **parses it on read**, so a mistake in an
//! address stops the command at config load, not at transaction assembly.
//!
//! # `deny_unknown_fields` on purpose
//!
//! The config is edited by hand. A silently ignored field with a typo in its name
//! is a risk limit the owner believes is set while the program does not see it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Config file name in the project root.
pub const CONFIG_FILE: &str = "propamm.toml";

/// Config schema version.
///
/// Written as the first field, so that a command meeting a config from a newer
/// CLI version says so directly rather than failing on an unknown field.
pub const SCHEMA_VERSION: u32 = 1;

/// Default quote freshness limit, in slots.
///
/// **The number is in neither SPEC nor PLAN** — it comes from the program tests
/// (`tests/program/src/world.rs`), where 25 slots ≈ 10 seconds. It is a
/// deliberate default, not a requirement: the owner changes it with an `init`
/// flag or a config field.
pub const DEFAULT_MAX_QUOTE_AGE_SLOTS: u32 = 25;

/// Default hard bound on inventory skew, in basis points.
///
/// Likewise from the program tests: 3 000 bps = 30 %. See the caveat on
/// [`DEFAULT_MAX_QUOTE_AGE_SLOTS`].
pub const DEFAULT_MAX_SKEW_BPS: u16 = 3_000;

/// A Solana address in the config: base58 on paper, `Pubkey` in memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConfigKey(pub Pubkey);

impl From<Pubkey> for ConfigKey {
    fn from(key: Pubkey) -> Self {
        Self(key)
    }
}

impl fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for ConfigKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ConfigKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Pubkey::from_str(&text)
            .map(Self)
            .map_err(|err| serde::de::Error::custom(format!("\"{text}\" is not an address: {err}")))
    }
}

/// The cluster the project lives on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Cluster {
    Localnet,
    Devnet,
    Mainnet,
}

impl Cluster {
    /// Default RPC address.
    ///
    /// Written into the config explicitly rather than substituted silently on every
    /// run: a command that goes to the network has to name it in the config,
    /// otherwise "I deployed to devnet" and "the CLI went to devnet" are two different claims.
    #[must_use]
    pub const fn default_rpc_url(self) -> &'static str {
        match self {
            Self::Localnet => "http://127.0.0.1:8899",
            Self::Devnet => "https://api.devnet.solana.com",
            Self::Mainnet => "https://api.mainnet-beta.solana.com",
        }
    }
}

impl fmt::Display for Cluster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Localnet => "localnet",
            Self::Devnet => "devnet",
            Self::Mainnet => "mainnet",
        };
        f.write_str(name)
    }
}

/// The whole `propamm.toml`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub version: u32,
    pub project: Project,
    pub network: Network,
    pub owner: Owner,
    /// The list of vaults (FR-004a): one pair per vault, no shared pool (FR-004),
    /// but the project shows them together.
    #[serde(default, rename = "vault")]
    pub vaults: Vec<VaultEntry>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub cluster: Cluster,
    pub rpc_url: String,
    /// Address of the already deployed `propamm_vault` program.
    ///
    /// One program serves everyone: a vault is a PDA of the owner and the pair, so
    /// there is one bytecode on the network, and "deploy your own AMM" means
    /// creating your own PDA. The field is still in the config, because on a local network the address is different.
    pub program_id: ConfigKey,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    /// The owner's address — what goes into the vault seeds.
    pub pubkey: ConfigKey,
    /// Path to the keypair the commands sign with.
    ///
    /// Kept next to the address on purpose: a mismatch between them means the
    /// config describes one vault while the command signs another, and that has to
    /// surface before the transaction, not after.
    pub keypair: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultEntry {
    /// Selector for the T023 commands: `forge fund --vault <name>`.
    pub name: String,
    pub base_mint: ConfigKey,
    pub quote_mint: ConfigKey,
    /// The right to post quotes (FR-010). Defaults to the owner; the engine from
    /// US2 gets a separate key right here.
    pub pricing_authority: ConfigKey,
    /// The right to halt in an emergency (FR-023c), separate from the rest.
    pub halt_authority: ConfigKey,
    pub max_quote_age_slots: u32,
    pub max_skew_bps: u16,
}

impl ProjectConfig {
    /// Write the config into the project directory.
    ///
    /// # Errors
    ///
    /// If the config does not serialize or the file cannot be written.
    pub fn write_to(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join(CONFIG_FILE);
        let text = toml::to_string_pretty(self).context("the config does not serialize")?;
        std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(path)
    }

    /// Read the config from the project directory.
    ///
    /// # Errors
    ///
    /// If the file is missing, does not parse or carries an unknown schema version.
    pub fn read_from(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read {} — is this a PropAMM project directory? create one: forge init",
                path.display()
            )
        })?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))?;
        if config.version != SCHEMA_VERSION {
            bail!(
                "{} is written to schema v{}, and this version of forge knows v{SCHEMA_VERSION}",
                path.display(),
                config.version
            );
        }
        Ok(config)
    }

    /// The vault by selector.
    ///
    /// # Errors
    ///
    /// If there is none — with the list of those that exist: a typo in the selector
    /// would otherwise look like "the vault is not deployed".
    pub fn vault(&self, name: &str) -> Result<&VaultEntry> {
        self.vaults
            .iter()
            .find(|vault| vault.name == name)
            .with_context(|| {
                let known: Vec<&str> = self.vaults.iter().map(|v| v.name.as_str()).collect();
                if known.is_empty() {
                    format!("vault \"{name}\" is not declared, and the config has none at all")
                } else {
                    format!(
                        "vault \"{name}\" is not declared; the config has: {}",
                        known.join(", ")
                    )
                }
            })
    }
}
