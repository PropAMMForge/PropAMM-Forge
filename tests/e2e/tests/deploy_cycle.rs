//! T024 — the full US1 cycle on a local network and the SC-001 measurement.
//!
//! The run answers one question: does the product take a person from an empty
//! directory to an AMM that executed its first swap in five commands and a
//! quarter of an hour. Everything else here is scenery.
//!
//! # Why `#[ignore]`
//!
//! The test needs `solana-test-validator` in `PATH`, a free port 8899 and a built
//! `.so`. In `cargo test --workspace` it shows as ignored — on purpose: the gate
//! has to stay runnable on a machine without Solana, and a test visible in every
//! run is harder to forget than one hidden behind a feature.
//! Invoke: `scripts/wsl-build.sh e2e`.

use std::path::PathBuf;
use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use propamm_e2e::forge::Forge;
use propamm_e2e::mints::{Holdings, Pair};
use propamm_e2e::net;
use propamm_e2e::trader::{request, token_balance, Market, Trader};
use propamm_e2e::validator::Validator;
use propamm_vault::instructions::swap::SwapSide;
use solana_keypair::Keypair;
use solana_signer::Signer as _;

/// The SC-001 limit.
const SC_001_LIMIT: Duration = Duration::from_secs(15 * 60);
const SC_001_COMMANDS: usize = 5;

/// How many decimals each side has. Different on purpose — see `mints`.
const BASE_DECIMALS: u8 = 9;
const QUOTE_DECIMALS: u8 = 6;

/// The quote the run posts.
const MID: &str = "150.25";
const SPREAD_BPS: &str = "20";
const MAX_SIZE_BASE: &str = "10";

/// How much capital the owner puts in, in human units.
const FUND_BASE: &str = "100";
const FUND_QUOTE: &str = "20000";

/// The trader's order: exactly one base asset.
const SWAP_IN: u64 = 1_000_000_000;

/// Expected output at bid = 150.25 × (1 − 20/10000) = 149.9495.
///
/// A band, not an equality: the exact number is the subject of SC-006 (T034/T054),
/// and repeating its computation here would mean a second implementation of the
/// rounding rule. What is checked here is different — that the price is the one
/// that was posted, not the mid and not the opposite side.
const EXPECTED_OUT: u64 = 149_949_500;
const EXPECTED_TOLERANCE: u64 = 10;

#[test]
#[ignore = "needs solana-test-validator and a free port 8899; invoke: scripts/wsl-build.sh e2e"]
fn empty_folder_to_first_swap() -> Result<()> {
    let artifact = workspace_root().join("target/deploy/propamm_vault.so");
    let validator = Validator::start(&propamm_vault::ID, &artifact)?;
    let rpc = &validator.rpc;

    // ── Precondition. The stopwatch is not running yet: on devnet the pair's mints
    //    already exist, and so does the network — minting USDC is not part of the path of whoever deploys an AMM.
    let workspace = tempfile::tempdir()?;
    let owner = Keypair::new();
    let owner_key_path = workspace.path().join("owner.json");
    net::write_keypair(&owner_key_path, &owner)?;
    net::airdrop(rpc, &owner.pubkey(), 100_000_000_000)?;

    let pair = Pair::create(rpc, &owner, BASE_DECIMALS, QUOTE_DECIMALS)?;
    // The owner's account addresses are recorded nowhere: `forge fund` takes the
    // ATA itself, and that is exactly what is checked in it.
    pair.open_and_fill(
        rpc,
        &owner,
        &owner.pubkey(),
        1_000_000_000_000,
        100_000_000_000,
    )?;

    let trader_key = Keypair::new();
    net::airdrop(rpc, &trader_key.pubkey(), 2_000_000_000)?;
    let trader_holdings =
        pair.open_and_fill(rpc, &trader_key, &trader_key.pubkey(), 10_000_000_000, 0)?;
    let trader = Trader::new(trader_key, trader_holdings);

    // The project directory is empty — the very one SC-001 starts from.
    let project = workspace.path().join("amm");
    let project_arg = project.to_string_lossy().into_owned();
    let owner_arg = owner_key_path.to_string_lossy().into_owned();

    // ── The stopwatch. Five commands, not one more.
    let mut forge = Forge::new()?;
    forge.run(&[
        "init",
        &project_arg,
        "--pair",
        &pair.as_argument(),
        "--owner",
        &owner_arg,
        "--cluster",
        "localnet",
    ])?;
    forge.run(&["deploy", "--path", &project_arg])?;
    forge.run(&[
        "fund",
        "--path",
        &project_arg,
        "--side",
        "base",
        "--amount",
        FUND_BASE,
    ])?;
    forge.run(&[
        "fund",
        "--path",
        &project_arg,
        "--side",
        "quote",
        "--amount",
        FUND_QUOTE,
    ])?;

    // The balances are taken BEFORE the quote is posted, even though it does not
    // move them: the quote lives 25 slots (≈10 s), and four extra requests between
    // it and the swap on a cold machine are the kind of fragility that later reads
    // as a random `QuoteStale`.
    let market = Market::derive(&owner.pubkey(), &pair);
    let before = Balances::read(rpc, &trader, market.treasuries)?;

    forge.run(&[
        "quote",
        "--path",
        &project_arg,
        "--mid",
        MID,
        "--spread-bps",
        SPREAD_BPS,
        "--size",
        MAX_SIZE_BASE,
    ])?;

    // ── The swap. This is already an external participant, and it does not count
    //    as a command — but it does count on the stopwatch: SC-001 speaks of an AMM that *executed* a swap.
    let confirmed = trader.swap(rpc, &market, request(SwapSide::BaseToQuote, SWAP_IN, 0))?;
    let after = Balances::read(rpc, &trader, market.treasuries)?;

    print!("{}", forge.report());
    println!("  swap: confirmed in slot {}", confirmed.slot);

    // ── The verdict.
    let spent = before.trader_base - after.trader_base;
    let received = after.trader_quote - before.trader_quote;
    println!(
        "  swapped: {spent} raw base → {received} raw quote (expected ≈{EXPECTED_OUT} at the posted bid)\n"
    );

    assert_eq!(spent, SWAP_IN, "the trader gave the wrong amount");
    assert_eq!(
        after.base_treasury - before.base_treasury,
        SWAP_IN,
        "the vault received something other than what the trader gave"
    );
    assert_eq!(
        before.quote_treasury - after.quote_treasury,
        received,
        "the vault gave something other than what the trader received"
    );
    assert!(
        received.abs_diff(EXPECTED_OUT) <= EXPECTED_TOLERANCE,
        "the execution price {received} does not match the posted bid side ({EXPECTED_OUT})"
    );

    assert_eq!(
        forge.commands(),
        SC_001_COMMANDS,
        "SC-001 allows {SC_001_COMMANDS} commands"
    );
    assert!(
        forge.elapsed() <= SC_001_LIMIT,
        "SC-001 not met: {:.1} s against the limit of {} s",
        forge.elapsed().as_secs_f64(),
        SC_001_LIMIT.as_secs()
    );
    Ok(())
}

/// The four balances a swap has to move, taken in one go.
struct Balances {
    trader_base: u64,
    trader_quote: u64,
    base_treasury: u64,
    quote_treasury: u64,
}

impl Balances {
    fn read(rpc: &propamm_cli::rpc::Rpc, trader: &Trader, treasuries: Holdings) -> Result<Self> {
        Ok(Self {
            trader_base: token_balance(rpc, &trader.holdings.base)?,
            trader_quote: token_balance(rpc, &trader.holdings.quote)?,
            base_treasury: token_balance(rpc, &treasuries.base)?,
            quote_treasury: token_balance(rpc, &treasuries.quote)?,
        })
    }
}

/// The workspace root — `target/deploy` is visible from here.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root exists")
}

/// The program key in the test and in genesis has to be one. The check is here,
/// not in the harness: if `declare_id!` diverged from the one the artifact was
/// built with, the run would fail on `forge deploy` with a message about a missing program.
#[test]
fn program_id_is_the_one_the_artifact_was_built_with() {
    let declared: Pubkey = propamm_vault::ID;
    assert_eq!(
        declared.to_string(),
        "77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq"
    );
}
