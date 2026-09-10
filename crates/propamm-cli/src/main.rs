//! The `forge` entry point.
//!
//! Only argument parsing and result printing live here; the work itself is in the
//! [`propamm_cli`] modules, so it can be tested by calling rather than by spawning a process.
//!
//! Every command that goes to the network ends with a hint about the next step:
//! SC-001 gives five commands for the path from an empty directory to the first
//! swap, and the path has to be visible from what is already printed, not from the docs.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use propamm_cli::amount::{format_mid_e9, format_raw, Decimal};
use propamm_cli::config::{Cluster, DEFAULT_MAX_QUOTE_AGE_SLOTS, DEFAULT_MAX_SKEW_BPS};
use propamm_cli::rpc::Confirmed;
use propamm_cli::{deploy, fund, init, quote, status};

#[derive(Parser)]
#[command(
    name = "forge",
    about = "Prop AMM builder on Solana",
    version,
    after_help = "The whole path: init → deploy → fund → quote → status."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project: config, README, .gitignore
    Init(InitArgs),
    /// Create a vault on the network
    Deploy(DeployArgs),
    /// Fund a vault treasury
    Fund(FundArgs),
    /// Post or clear a quote
    Quote(QuoteArgs),
    /// Show the state of every vault in the project
    Status(StatusArgs),
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

/// Shared by every command that works with an already created project.
#[derive(clap::Args)]
struct ProjectArgs {
    /// Project directory
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Pair selector; may be omitted if the project has a single vault
    #[arg(long)]
    vault: Option<String>,
}

#[derive(clap::Args)]
struct DeployArgs {
    #[command(flatten)]
    project: ProjectArgs,
}

#[derive(clap::Args)]
struct FundArgs {
    #[command(flatten)]
    project: ProjectArgs,

    /// Which side of the pair to fund
    #[arg(long)]
    side: fund::Side,

    /// Amount in human units of that asset
    #[arg(long, value_parser = parse_decimal)]
    amount: Decimal,

    /// Source account (defaults to the owner's ATA)
    #[arg(long, value_parser = parse_pubkey)]
    from: Option<anchor_lang::prelude::Pubkey>,
}

#[derive(clap::Args)]
struct QuoteArgs {
    #[command(flatten)]
    project: ProjectArgs,

    /// Clear the quote instead of posting one (FR-014)
    #[arg(long)]
    clear: bool,

    /// Market mid: how much quote per one base
    #[arg(long, value_parser = parse_decimal, conflicts_with = "mid_e9")]
    mid: Option<Decimal>,

    /// The mid directly in raw form, without conversion through decimals
    #[arg(long)]
    mid_e9: Option<u128>,

    /// Half of the spread in basis points
    #[arg(long)]
    spread_bps: Option<u16>,

    /// Mid shift by inventory skew, in basis points
    #[arg(long)]
    skew_bps: Option<i16>,

    /// Maximum order size in human units of the base asset
    #[arg(long, value_parser = parse_decimal, conflicts_with = "max_size_base")]
    size: Option<Decimal>,

    /// Maximum order size in raw units of the base asset
    #[arg(long)]
    max_size_base: Option<u64>,

    /// pricing_authority keypair, if it is no longer the owner
    #[arg(long)]
    keypair: Option<PathBuf>,
}

#[derive(clap::Args)]
struct StatusArgs {
    #[command(flatten)]
    project: ProjectArgs,
}

fn parse_decimal(text: &str) -> Result<Decimal, String> {
    text.parse::<Decimal>().map_err(|err| format!("{err}"))
}

fn parse_pubkey(text: &str) -> Result<anchor_lang::prelude::Pubkey, String> {
    text.parse()
        .map_err(|err| format!("\"{text}\" is not an address: {err}"))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init(args) => run_init(args),
        Command::Deploy(args) => run_deploy(&args),
        Command::Fund(args) => run_fund(&args),
        Command::Quote(args) => run_quote(args),
        Command::Status(args) => run_status(&args),
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

fn run_deploy(args: &DeployArgs) -> Result<()> {
    let deployed = deploy::run(&deploy::Options {
        path: args.project.path.clone(),
        vault: args.project.vault.clone(),
    })?;

    println!("vault {} deployed", deployed.name);
    println!("  address    {}", deployed.address);
    println!(
        "  pair       {} ({} decimals) / {} ({} decimals)",
        deployed.base.address,
        deployed.base.decimals,
        deployed.quote.address,
        deployed.quote.decimals
    );
    println!("  treasuries {}", deployed.base_treasury);
    println!("             {}", deployed.quote_treasury);
    print_confirmed(&deployed.confirmed);
    println!();
    println!(
        "Treasuries are empty, there is no price. Next: forge fund --vault {} --side quote --amount <amount>",
        deployed.name
    );
    Ok(())
}

fn run_fund(args: &FundArgs) -> Result<()> {
    let funded = fund::run(&fund::Options {
        path: args.project.path.clone(),
        vault: args.project.vault.clone(),
        side: args.side,
        amount: args.amount,
        from: args.from,
    })?;

    let decimals = funded.mint.decimals;
    println!(
        "vault {} — {} side funded with {}",
        funded.name,
        funded.side,
        format_raw(funded.amount, decimals)
    );
    println!("  asset      {}", funded.mint.address);
    println!("  from       {}", funded.from);
    println!(
        "  treasury   {}  {} → {}",
        funded.treasury,
        format_raw(funded.treasury_before, decimals),
        format_raw(
            funded.treasury_before.saturating_add(funded.amount),
            decimals
        )
    );
    print_confirmed(&funded.confirmed);
    println!();
    println!(
        "Next: fund the other side or post a price — forge quote --vault {} --mid <price> --spread-bps 20 --size <size>",
        funded.name
    );
    Ok(())
}

fn run_quote(args: QuoteArgs) -> Result<()> {
    let action = if args.clear {
        quote::Action::Clear
    } else {
        let mid = match (args.mid, args.mid_e9) {
            (Some(price), None) => quote::Mid::Human(price),
            (None, Some(raw)) => quote::Mid::Raw(raw),
            _ => anyhow::bail!(
                "a market mid is required: --mid <human price> or --mid-e9 <raw value>"
            ),
        };
        let size = match (args.size, args.max_size_base) {
            (Some(size), None) => quote::Size::Human(size),
            (None, Some(raw)) => quote::Size::Raw(raw),
            _ => anyhow::bail!(
                "a maximum order size is required: --size <size> or --max-size-base <raw value>"
            ),
        };
        let spread_bps = args.spread_bps.ok_or_else(|| {
            anyhow::anyhow!("--spread-bps is required: half of the spread in basis points")
        })?;
        quote::Action::Set(quote::SetQuote {
            mid,
            spread_bps,
            skew_bps: args.skew_bps.unwrap_or(0),
            size,
        })
    };

    let quoted = quote::run(&quote::Options {
        path: args.project.path.clone(),
        vault: args.project.vault.clone(),
        action,
        keypair: args.keypair,
    })?;

    match &quoted.summary {
        None => println!("vault {} — quote cleared", quoted.name),
        Some(summary) => {
            let human =
                |value: u128| format_mid_e9(value, summary.base.decimals, summary.quote.decimals);
            println!("vault {} — quote posted", quoted.name);
            println!(
                "  mid        {}  →  mid_e9 = {}{}",
                human(summary.mid_e9),
                summary.mid_e9,
                if summary.exact {
                    ""
                } else {
                    "  (the typed number does not convert exactly — this is what went on chain)"
                }
            );
            println!(
                "  sides      bid {}  ask {}  (±{} bps, skew {} bps)",
                human(summary.bid_e9),
                human(summary.ask_e9),
                summary.spread_bps,
                summary.skew_bps
            );
            println!(
                "  size       up to {} base per order",
                format_raw(summary.max_size_base, summary.base.decimals)
            );
            println!("  signed by  {}", quoted.authority);
        }
    }
    print_confirmed(&quoted.confirmed);
    println!();
    println!("Next: forge status --vault {}", quoted.name);
    Ok(())
}

fn run_status(args: &StatusArgs) -> Result<()> {
    let report = status::run(&status::Options {
        path: args.project.path.clone(),
        vault: args.project.vault.clone(),
    })?;
    print!("{}", status::render(&report));
    Ok(())
}

/// One confirmation format for every command: signature and slot.
///
/// The slot, not the time: the chain clock is the slot, and it is what later
/// stands in `quote_slot` and in the freshness limit.
fn print_confirmed(confirmed: &Confirmed) {
    println!("  ✅ {} in slot {}", confirmed.signature, confirmed.slot);
}
