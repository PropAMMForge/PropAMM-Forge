//! T033 — the engine on a local network: SC-003 measured, SC-011 shown by
//! swapping the model, FR-014 seen on chain.
//!
//! # The run
//!
//! One validator, one vault, one stand-in Hermes ([`FakeHermes`]). The same
//! `propamm-engine` binary is started twice, the second time with nothing
//! changed but `MODEL_COMMAND`:
//!
//! 1. the built-in spread-and-skew model;
//! 2. `examples/models/fixed_spread.py` — a model in another language that
//!    shares nothing with the first but the protocol (SC-011).
//!
//! Under each, the price is moved [`MOVES`] times by [`MOVE_BPS`] and the
//! vault is watched until the new mid is on it. At the end the feed goes
//! quiet and the quote has to come off the book (FR-014).
//!
//! # What SC-003 is measured as
//!
//! "≤ 2 slots after the external price moves past 5 bps", p95. The move is the
//! moment the stand-in writes the moved sample to the engine's socket; the
//! slot of the move is the node's `processed` slot read **just before** that
//! write; the slot of the update is the `quote_slot` the program stamps on the
//! vault when the transaction executes. So the number errs against the engine:
//! the slot read before the write can only be earlier than the true one.
//!
//! What it does not include: the feed's own delay. Hermes delivers a price
//! once a second (measured live), so a market move waits up to a second to
//! reach any engine — that is the feed's, not the engine's, and the same for
//! every consumer of it.
//!
//! # The timeline
//!
//! With `E2E_TRACE=<file>` the run is also written out as JSON: every move,
//! the slot it was read at, what landed on the vault and when, and the feed
//! going quiet. It plays no part in the verdict; it is what the landing page
//! draws, so the picture there is this run and not a drawing of one.
//!
//! # Why `#[ignore]`
//!
//! As in `deploy_cycle.rs`: a validator, port 8899, the `.so`, the two
//! binaries and `python3`. Invoke: `scripts/wsl-build.sh e2e-engine`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use anchor_lang::{InstructionData, ToAccountMetas};
use anyhow::{bail, Context, Result};
use propamm_client::chain::decode_vault;
use propamm_client::rpc::Rpc;
use propamm_e2e::engine::Engine;
use propamm_e2e::forge::Forge;
use propamm_e2e::hermes::FakeHermes;
use propamm_e2e::mints::Pair;
use propamm_e2e::net;
use propamm_e2e::trader::{token_balance, Market};
use propamm_e2e::validator::Validator;
use propamm_e2e::RPC_URL;
use propamm_engine::feed::{mid_e9, Price};
use propamm_engine::model::spread_skew::SpreadSkewModel;
use propamm_engine::model::MarketState;
use propamm_quote::Inventory;
use propamm_vault::state::Vault;
use serde_json::json;
use solana_keypair::Keypair;
use solana_signer::Signer as _;

/// SOL/USD on Pythnet — any id would do; this one reads right in the logs.
const SOL_USD: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

/// The SC-003 budget.
const SC_003_SLOTS: u64 = 2;

/// Moves per model. 100 makes p95 the 95th: five late moves are allowed, a sixth is not.
const MOVES: usize = 100;

/// Each move: twice the 5 bps of SC-003, so the rule cannot round it away.
const MOVE_BPS: u64 = 10;

/// 150.25 USD at `expo` −8 — where the price starts and returns to.
const START: u64 = 15_025_000_000;

/// A move that has not landed by now is counted as missed.
const LANDING_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the vault is read while waiting.
const POLL: Duration = Duration::from_millis(25);

/// Until the engine's first quote is on the book. Generous: the Python model's
/// interpreter starts inside it.
const FIRST_QUOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The pair and the capital, as in `deploy_cycle.rs`.
const BASE_DECIMALS: u8 = 9;
const QUOTE_DECIMALS: u8 = 6;
const FUND_BASE: &str = "100";
const FUND_QUOTE: &str = "20000";

/// The built-in model's levers — `.env.example`'s, which are the engine's defaults.
const BUILTIN: [u16; 4] = [10, 40, 150, 500];

/// The second model and its fingerprint on chain.
const FIXED_SPREAD_BPS: u16 = 25;
const FIXED_SIZE_BASE: u64 = 1_000_000_000;

/// `FEED_MAX_SILENCE_MS` the engine runs with — the default.
const SILENCE: Duration = Duration::from_secs(2);

#[test]
#[ignore = "needs solana-test-validator, a free port 8899, the engine binary and python3; invoke: scripts/wsl-build.sh e2e-engine"]
fn the_quote_follows_the_price_and_a_second_model_drops_in() -> Result<()> {
    let artifact = workspace_root().join("target/deploy/propamm_vault.so");
    let validator = Validator::start(&propamm_vault::ID, &artifact)?;
    let rpc = &validator.rpc;

    // ── A deployed, funded vault — the US1 path, by `forge`.
    let workspace = tempfile::tempdir()?;
    let owner = Keypair::new();
    let owner_path = workspace.path().join("owner.json");
    net::write_keypair(&owner_path, &owner)?;
    net::airdrop(rpc, &owner.pubkey(), 100_000_000_000)?;
    let pair = Pair::create(rpc, &owner, BASE_DECIMALS, QUOTE_DECIMALS)?;
    pair.open_and_fill(
        rpc,
        &owner,
        &owner.pubkey(),
        1_000_000_000_000,
        100_000_000_000,
    )?;
    let project = workspace.path().join("amm");
    let project_arg = project.to_string_lossy().into_owned();
    let owner_arg = owner_path.to_string_lossy().into_owned();
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
    for (side, amount) in [("base", FUND_BASE), ("quote", FUND_QUOTE)] {
        forge.run(&[
            "fund",
            "--path",
            &project_arg,
            "--side",
            side,
            "--amount",
            amount,
        ])?;
    }
    let market = Market::derive(&owner.pubkey(), &pair);

    // ── The engine gets its own key (FR-010): the owner hands over the right
    //    to quote and nothing else.
    let pricing = Keypair::new();
    let pricing_path = workspace.path().join("pricing.json");
    net::write_keypair(&pricing_path, &pricing)?;
    net::airdrop(rpc, &pricing.pubkey(), 2_000_000_000)?;
    set_pricing_authority(rpc, &owner, &market.vault, &pricing.pubkey())?;

    let hermes = FakeHermes::start(SOL_USD, START)?;
    let env = |model: Option<String>| {
        let mut env = BTreeMap::from([
            ("SOLANA_RPC_URL", RPC_URL.to_owned()),
            ("VAULT_ADDRESS", market.vault.to_string()),
            (
                "PRICING_AUTHORITY_KEYPAIR",
                pricing_path.to_string_lossy().into_owned(),
            ),
            ("PYTH_HERMES_URL", hermes.url().to_owned()),
            ("PYTH_PRICE_FEED_ID", SOL_USD.to_owned()),
            ("RUST_LOG", "info".to_owned()),
        ]);
        if let Some(command) = model {
            env.insert("MODEL_COMMAND", command);
        }
        env
    };
    let watch = Watch {
        rpc,
        vault: market.vault,
        treasuries: [market.treasuries.base, market.treasuries.quote],
    };

    // ── 1. The built-in model.
    let builtin = SpreadSkewModel::checked(BUILTIN[0], BUILTIN[1], BUILTIN[2], BUILTIN[3])
        .expect("the defaults are a valid model");
    let mut trace = Trace::new();
    let mut engine = Engine::start(&env(None), &workspace.path().join("engine-builtin.log"))?;
    let first = measure(
        &watch,
        &hermes,
        &mut engine,
        &mut trace,
        "built-in",
        |book, state| {
            let quote = builtin.quote(state).expect("the model prices this state");
            book.spread_bps == quote.spread_bps
                && book.skew_bps == quote.skew_bps
                && book.max_size_base == quote.max_size_base
        },
    )?;
    engine.stop();

    // ── 2. A model in another process. The same binary; only `MODEL_COMMAND`.
    let script = workspace_root().join("examples/models/fixed_spread.py");
    let command = format!(
        "python3 {} --spread-bps {FIXED_SPREAD_BPS} --size-base {FIXED_SIZE_BASE}",
        script.display()
    );
    let mut engine = Engine::start(
        &env(Some(command)),
        &workspace.path().join("engine-fixed.log"),
    )?;
    let second = measure(
        &watch,
        &hermes,
        &mut engine,
        &mut trace,
        "fixed_spread.py",
        |book, _| {
            book.spread_bps == FIXED_SPREAD_BPS
                && book.skew_bps == 0
                && book.max_size_base == FIXED_SIZE_BASE
        },
    )?;

    // ── 3. The feed goes quiet: the quote comes off the book (FR-014).
    hermes.set_quiet(true)?;
    let quiet_at = Instant::now();
    let cleared = loop {
        engine.alive()?;
        let (slot, vault) = watch.read()?;
        if vault.mid_e9 == 0 {
            trace.quiet(quiet_at, Instant::now(), slot);
            break quiet_at.elapsed();
        }
        if quiet_at.elapsed() > SILENCE + LANDING_TIMEOUT {
            bail!(
                "the feed is quiet and the quote is still on the book after {:.1} s\n── engine log tail ──\n{}",
                quiet_at.elapsed().as_secs_f64(),
                engine.log_tail(30)
            );
        }
        std::thread::sleep(POLL);
    };
    engine.stop();

    // ── The report — printed always: these are the numbers M2 is closed with.
    let binary = Engine::binary()?;
    println!("\nSC-003 — slots from the price move to the quote on chain, p95 ≤ {SC_003_SLOTS}:");
    for run in [&first, &second] {
        println!("  {}", run.slots_line());
    }
    println!(
        "SC-011 — one binary ({}, {} bytes), the model swapped by MODEL_COMMAND only:",
        binary.display(),
        std::fs::metadata(&binary)?.len()
    );
    for run in [&first, &second] {
        println!("  {}", run.fingerprint_line());
    }
    println!(
        "FR-014 — the feed went quiet: the quote was off the book in {:.2} s (silence bound {} s)",
        cleared.as_secs_f64(),
        SILENCE.as_secs()
    );
    println!("RPC — the engine's own count, per model run:");
    for run in [&first, &second] {
        println!("  {}", run.rpc_line());
    }
    println!();
    if let Some(path) = std::env::var_os("E2E_TRACE") {
        trace.write(&PathBuf::from(&path))?;
        println!("timeline written to {}", PathBuf::from(path).display());
    }

    // ── The verdict.
    for run in [&first, &second] {
        assert_eq!(
            run.fingerprinted,
            run.landed(),
            "SC-011: {} — {} of {} quotes on chain are not this model's",
            run.model,
            run.landed() - run.fingerprinted,
            run.landed()
        );
        let p95 = run.p95();
        assert!(
            p95 <= SC_003_SLOTS,
            "SC-003 not met with the {} model: p95 = {} slots against {SC_003_SLOTS}",
            run.model,
            fmt_slots(p95)
        );
    }
    Ok(())
}

/// What one model's run measured.
struct Run {
    model: &'static str,
    /// Slots from each move to its quote on chain; `None` — it never landed.
    slots: Vec<Option<u64>>,
    /// The same in wall time, as observed by polling (an upper bound).
    wall: Vec<Duration>,
    /// Quotes on chain that carry this model's fingerprint.
    fingerprinted: usize,
    rpc: Option<(Duration, BTreeMap<String, u64>)>,
}

impl Run {
    fn landed(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Missed moves sort last: they count against the percentile.
    fn sorted(&self) -> Vec<u64> {
        let mut slots: Vec<u64> = self.slots.iter().map(|s| s.unwrap_or(u64::MAX)).collect();
        slots.sort_unstable();
        slots
    }

    fn percentile(sorted: &[u64], p: usize) -> u64 {
        let rank = (sorted.len() * p).div_ceil(100).max(1);
        sorted[rank - 1]
    }

    fn p95(&self) -> u64 {
        Self::percentile(&self.sorted(), 95)
    }

    fn slots_line(&self) -> String {
        let sorted = self.sorted();
        let mut histogram = BTreeMap::<String, usize>::new();
        for slots in &sorted {
            *histogram.entry(fmt_slots(*slots)).or_default() += 1;
        }
        let histogram: Vec<String> = histogram
            .iter()
            .map(|(slots, count)| format!("{slots}:{count}"))
            .collect();
        let mut wall: Vec<Duration> = self.wall.clone();
        wall.sort_unstable();
        let ms = |p: usize| {
            wall.get((wall.len() * p).div_ceil(100).max(1) - 1)
                .map_or(0, Duration::as_millis)
        };
        format!(
            "{:<16} n={} p50={} p95={} max={} missed={}  [slots:count {}]  wall p50={} ms p95={} ms",
            self.model,
            sorted.len(),
            fmt_slots(Self::percentile(&sorted, 50)),
            fmt_slots(Self::percentile(&sorted, 95)),
            fmt_slots(*sorted.last().unwrap_or(&0)),
            sorted.len() - self.landed(),
            histogram.join(" "),
            ms(50),
            ms(95),
        )
    }

    fn fingerprint_line(&self) -> String {
        format!(
            "{:<16} {}/{} quotes on chain are this model's",
            self.model,
            self.fingerprinted,
            self.landed()
        )
    }

    fn rpc_line(&self) -> String {
        let Some((elapsed, calls)) = &self.rpc else {
            return format!("{:<16} no totals logged yet", self.model);
        };
        let total: u64 = calls.values().sum();
        let sends = calls.get("sendTransaction").copied().unwrap_or(0);
        let minutes = elapsed.as_secs_f64() / 60.0;
        let calls: Vec<String> = calls.iter().map(|(m, c)| format!("{m}={c}")).collect();
        format!(
            "{:<16} {:.1} min: {} calls ({:.1}/min), sendTransaction {} ({:.2} calls per send)  [{}]",
            self.model,
            minutes,
            total,
            total as f64 / minutes.max(f64::EPSILON),
            sends,
            total as f64 / (sends.max(1) as f64),
            calls.join(" ")
        )
    }
}

/// The run as a timeline — see "The timeline" above. Times are milliseconds
/// from the first move; integers past 2^53 are strings, as in the model protocol.
struct Trace {
    origin: Option<Instant>,
    moves: Vec<serde_json::Value>,
    quiet: Option<serde_json::Value>,
}

impl Trace {
    fn new() -> Self {
        Self {
            origin: None,
            moves: Vec::new(),
            quiet: None,
        }
    }

    fn ms(&mut self, at: Instant) -> u128 {
        let origin = *self.origin.get_or_insert(at);
        at.saturating_duration_since(origin).as_millis()
    }

    fn moved(
        &mut self,
        model: &str,
        price: u64,
        slot_before: u64,
        emitted: Instant,
        landed: Option<(&Vault, Instant)>,
    ) {
        let t_ms = self.ms(emitted);
        let landed = landed.map(|(vault, seen)| {
            json!({
                "seen_ms": self.ms(seen),
                "quote_slot": vault.quote_slot,
                "mid_e9": vault.mid_e9.to_string(),
                "spread_bps": vault.spread_bps,
                "skew_bps": vault.skew_bps,
                "max_size_base": vault.max_size_base.to_string(),
            })
        });
        self.moves.push(json!({
            "model": model,
            "t_ms": t_ms,
            "price_mantissa": price.to_string(),
            "expo": -8,
            "slot_before": slot_before,
            "landed": landed,
        }));
    }

    fn quiet(&mut self, quiet_at: Instant, cleared_at: Instant, slot: u64) {
        self.quiet = Some(json!({
            "t_ms": self.ms(quiet_at),
            "cleared_ms": self.ms(cleared_at),
            "cleared_seen_slot": slot,
        }));
    }

    fn write(&self, path: &std::path::Path) -> Result<()> {
        let body = json!({
            "run": "tests/e2e/tests/engine.rs — local validator, stand-in Hermes",
            "move_bps": MOVE_BPS,
            "silence_ms": SILENCE.as_millis(),
            "moves": self.moves,
            "quiet": self.quiet,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&body)?)
            .with_context(|| format!("writing the timeline to {}", path.display()))
    }
}

fn fmt_slots(slots: u64) -> String {
    if slots == u64::MAX {
        "missed".to_owned()
    } else {
        slots.to_string()
    }
}

/// Reads of the vault, as a router would see it: at the tip.
struct Watch<'a> {
    rpc: &'a Rpc,
    vault: Pubkey,
    treasuries: [Pubkey; 2],
}

impl Watch<'_> {
    /// The node's slot and the vault.
    fn read(&self) -> Result<(u64, Vault)> {
        let snapshot = self.rpc.get_multiple_accounts_at_tip(&[self.vault])?;
        let account = snapshot.accounts[0].as_ref().context("the vault is gone")?;
        Ok((snapshot.slot, decode_vault(&self.vault, account)?))
    }

    fn slot(&self) -> Result<u64> {
        self.rpc
            .call("getSlot", json!([{"commitment": "processed"}]))?
            .as_u64()
            .context("getSlot did not return a number")
    }

    /// What the model is handed for this mid — to check its quote against.
    fn state(&self, mid_e9: u128, vault: &Vault) -> Result<MarketState> {
        Ok(MarketState {
            mid_e9,
            inventory: Inventory {
                base_amount: token_balance(self.rpc, &self.treasuries[0])?,
                quote_amount: token_balance(self.rpc, &self.treasuries[1])?,
            },
            max_skew_bps: vault.max_skew_bps,
        })
    }

    /// Wait until the vault carries `mid`; the vault then, and when it was seen.
    fn until_mid(
        &self,
        mid: u128,
        engine: &mut Engine,
        timeout: Duration,
    ) -> Result<Option<(Vault, Instant)>> {
        let started = Instant::now();
        loop {
            let (_, vault) = self.read()?;
            if vault.mid_e9 == mid {
                return Ok(Some((vault, Instant::now())));
            }
            if started.elapsed() > timeout {
                engine.alive()?;
                return Ok(None);
            }
            std::thread::sleep(POLL);
        }
    }
}

/// The mid the engine computes from this price — with the engine's own function.
fn mid_of(mantissa: u64) -> u128 {
    let price = Price {
        id: SOL_USD.parse().expect("a valid id"),
        mantissa,
        expo: -8,
        conf_bps: 2,
        publish_time: 0,
        slot: None,
    };
    mid_e9(&price, None, BASE_DECIMALS, QUOTE_DECIMALS).expect("the mid fits")
}

/// One model's run: the first quote, then [`MOVES`] moves, each waited for.
fn measure(
    watch: &Watch<'_>,
    hermes: &FakeHermes,
    engine: &mut Engine,
    trace: &mut Trace,
    model: &'static str,
    is_ours: impl Fn(&Vault, &MarketState) -> bool,
) -> Result<Run> {
    // The price stands where the previous run left it; the new engine posts on its first price.
    let mut price = START;
    hermes_settle(hermes, engine, price)?;
    let Some((vault, _)) = watch.until_mid(mid_of(price), engine, FIRST_QUOTE_TIMEOUT)? else {
        bail!(
            "the {model} engine posted nothing in {} s\n── engine log tail ──\n{}",
            FIRST_QUOTE_TIMEOUT.as_secs(),
            engine.log_tail(30)
        );
    };
    // The previous engine's quote has the same mid: wait for this one's fingerprint.
    let deadline = Instant::now() + FIRST_QUOTE_TIMEOUT;
    let mut vault = vault;
    while !is_ours(&vault, &watch.state(vault.mid_e9, &vault)?) {
        if Instant::now() > deadline {
            bail!(
                "the {model} engine's quote never reached the book\n── engine log tail ──\n{}",
                engine.log_tail(30)
            );
        }
        std::thread::sleep(POLL);
        engine.alive()?;
        vault = watch.read()?.1;
    }

    let mut run = Run {
        model,
        slots: Vec::with_capacity(MOVES),
        wall: Vec::with_capacity(MOVES),
        fingerprinted: 0,
        rpc: None,
    };
    for index in 0..MOVES {
        // Up by MOVE_BPS, then back: the price stays near the start, and
        // every move is past the threshold from the quote before it.
        price = if index % 2 == 0 {
            START + START * MOVE_BPS / 10_000
        } else {
            START
        };
        let before = watch.slot()?;
        let emitted = hermes.move_to(price)?;
        match watch.until_mid(mid_of(price), engine, LANDING_TIMEOUT)? {
            Some((vault, seen)) => {
                run.slots
                    .push(Some(vault.quote_slot.saturating_sub(before)));
                run.wall.push(seen.saturating_duration_since(emitted.at));
                if is_ours(&vault, &watch.state(vault.mid_e9, &vault)?) {
                    run.fingerprinted += 1;
                }
                trace.moved(model, price, before, emitted.at, Some((&vault, seen)));
            }
            None => {
                run.slots.push(None);
                trace.moved(model, price, before, emitted.at, None);
            }
        }
        engine.alive()?;
        // 1.0–2.0 s apart, spread over the feed's one-second rhythm and the
        // slot boundary rather than locked to either.
        let pause = 1_000 + (index as u64 * 373) % 1_000;
        std::thread::sleep(Duration::from_millis(pause));
    }
    run.rpc = engine.rpc_calls();
    Ok(run)
}

/// The engine must be on the stream before the first move.
fn hermes_settle(hermes: &FakeHermes, engine: &mut Engine, price: u64) -> Result<()> {
    let deadline = Instant::now() + FIRST_QUOTE_TIMEOUT;
    loop {
        engine.alive()?;
        if hermes.move_to(price).is_ok() {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "the engine never connected to the stand-in Hermes\n── engine log tail ──\n{}",
                engine.log_tail(30)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The owner hands `pricing_authority` to the engine's key (FR-010).
fn set_pricing_authority(
    rpc: &Rpc,
    owner: &Keypair,
    vault: &Pubkey,
    engine: &Pubkey,
) -> Result<()> {
    let instruction = anchor_lang::solana_program::instruction::Instruction {
        program_id: propamm_vault::ID,
        accounts: propamm_vault::accounts::AdminOnly {
            owner: owner.pubkey(),
            vault: *vault,
        }
        .to_account_metas(None),
        data: propamm_vault::instruction::SetPricingAuthority {
            new_authority: *engine,
        }
        .data(),
    };
    net::send(rpc, &[instruction], &[owner])?;
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root exists")
}
