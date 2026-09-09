//! The `forge` entry point.
//!
//! Only argument parsing and result printing live here; the work itself is in
//! [`propamm_cli::init`], so it can be tested by calling rather than by spawning a
//! process.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use propamm_cli::config::{Cluster, DEFAULT_MAX_QUOTE_AGE_SLOTS, DEFAULT_MAX_SKEW_BPS};
use propamm_cli::init;

#[derive(Parser)]
#[command(
    name = "forge",
    about = "Prop AMM builder on Solana",
    version,
    // The hint at the bottom of `--help`: the T023 commands do not exist yet, and a
    // silent absence would look like a broken build rather than the order of work.
    after_help = "deploy / fund / quote / status arrive with the next task (T023)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project: config, README, .gitignore
    Init(InitArgs),
}

#[derive(clap::Args)]
struct InitArgs {
    /// Project directory
    #[arg(default_value = ".")]
    path: PathBuf,

    /// The pair as BASE/QUOTE — two mint addresses
    #[arg(long)]
    pair: String,

    /// Project name (defaults to the directory name)
    #[arg(long)]
    name: Option<String>,

    /// Vault selector for the other commands (defaults to one derived from the pair)
    #[arg(long)]
    vault_name: Option<String>,

    /// Owner keypair (defaults to ~/.config/solana/id.json)
    #[arg(long)]
    owner: Option<PathBuf>,

    /// Cluster
    #[arg(long, default_value = "devnet")]
    cluster: Cluster,

    /// Quote freshness limit, in slots
    #[arg(long, default_value_t = DEFAULT_MAX_QUOTE_AGE_SLOTS)]
    max_quote_age_slots: u32,

    /// Hard bound on inventory skew, in basis points
    #[arg(long, default_value_t = DEFAULT_MAX_SKEW_BPS)]
    max_skew_bps: u16,

    /// Overwrite an existing propamm.toml
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init(args) => run_init(args),
    }
}

fn run_init(args: InitArgs) -> Result<()> {
    let (base_mint, quote_mint) = init::parse_pair(&args.pair)?;
    let created = init::run(&init::Options {
        path: args.path,
        name: args.name,
        base_mint,
        quote_mint,
        owner_keypair: args.owner,
        cluster: args.cluster,
        vault_name: args.vault_name,
        max_quote_age_slots: args.max_quote_age_slots,
        max_skew_bps: args.max_skew_bps,
        force: args.force,
    })?;

    let vault = created
        .config
        .vaults
        .first()
        .expect("init always creates one vault");
    println!(
        "project {} — {}",
        created.config.project.name,
        created.dir.display()
    );
    for file in &created.files {
        println!("  created {}", file.display());
    }
    println!();
    println!("  cluster  {}", created.config.network.cluster);
    println!("  owner    {}", created.config.owner.pubkey);
    println!(
        "  vault    {} — {} / {}",
        vault.name, vault.base_mint, vault.quote_mint
    );
    println!();
    println!("Nothing on chain yet. Next: forge deploy");
    Ok(())
}
