//! `propamm-engine` — the engine as a process: the configuration from the
//! environment, the feed reader on its own thread, the tick loop on this one.
//!
//! ```text
//! propamm-engine [--env-file <path>]
//! ```
//!
//! The keys are listed in `.env.example`. `--env-file` fills the environment
//! from a file first; a variable already set in the environment wins over the
//! file. Without the flag no file is read — see [`propamm_engine::config`].
//!
//! The process runs until the loop stops ([`Stop`]): a refusal no retry can
//! fix, or the feed reader gone. It exits with 1 then, with 2 on a
//! configuration it will not start with. Logs go to stderr; `RUST_LOG`
//! narrows or widens them (default `info`).

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use propamm_client::chain::read_keypair;
use propamm_client::rpc::{Http, Rpc};
use propamm_engine::chain::RpcChain;
use propamm_engine::config::{Config, ModelChoice};
use propamm_engine::cycle::{Cycle, Settings, Stop};
use propamm_engine::feed::{Https, Reader, Silence, SystemClock, Watch};
use propamm_engine::meter::{Meter, Metered};
use propamm_engine::model::external::ExternalProcessModel;
use propamm_engine::model::PricingModel;
use propamm_engine::sender::Sender;
use propamm_engine::tick::{Budget, ModelStep, SystemMonotonic};
use solana_signer::Signer as _;
use tracing::{error, info};

/// The bound on one node request.
///
/// A hung request holds the whole loop, withdrawals included, so it has to be
/// well inside the time a quote stays fresh (25 slots ≈ 10 s by default) —
/// and well above what a send with preflight takes on a public node.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // Colour codes only on a terminal: a log file is read by people and by
        // the end-to-end run, which parses the `rpc calls` line.
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match load() {
        Ok(config) => config,
        Err(error) => {
            error!("{error:#}");
            return ExitCode::from(2);
        }
    };
    match run(config) {
        Ok(stop) => error!(%stop, "the engine stopped"),
        Err(error) => error!("{error:#}"),
    }
    ExitCode::FAILURE
}

/// The arguments, then the configuration.
fn load() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--env-file" => {
                let path = PathBuf::from(args.next().context("--env-file needs a path")?);
                dotenvy::from_path(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
            }
            "-h" | "--help" => {
                println!(
                    "propamm-engine [--env-file <path>] — the keys are listed in .env.example"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}; usage: propamm-engine [--env-file <path>]"),
        }
    }
    Ok(Config::from_env()?)
}

/// Start everything and run the loop until it stops.
fn run(config: Config) -> Result<Stop> {
    let key = read_keypair(&config.pricing_keypair)?;
    let meter = Meter::new(Instant::now());
    let rpc = Rpc::with_transport(
        config.rpc_url.clone(),
        Box::new(Metered::new(Http::with_timeout(RPC_TIMEOUT), meter.clone())),
    );
    let (chain, deployment) = RpcChain::connect(rpc, config.vault, key.pubkey())
        .map_err(|refusal| anyhow::anyhow!("cannot find the vault: {refusal}"))?;
    let sender = Sender::new(chain, key, deployment);

    let model: Box<dyn PricingModel> = match &config.model {
        ModelChoice::Builtin(model) => {
            info!(?model, "pricing model: built in");
            Box::new(*model)
        }
        ModelChoice::External(command) => {
            info!(%command, "pricing model: another process");
            Box::new(
                ExternalProcessModel::start(command.clone(), Default::default())
                    .with_context(|| format!("cannot start the model {command}"))?,
            )
        }
    };
    let step = ModelStep::new(model, Budget::new(config.model_budget));
    let settings = Settings {
        threshold_bps: config.threshold_bps,
        heartbeat_slots: config.heartbeat_slots,
        base_feed: config.base_feed,
        quote_feed: config.quote_feed,
    };
    info!(
        rpc = %config.rpc_url,
        hermes = %config.hermes_url,
        feed_key = config.api_key.is_some(),
        threshold_bps = settings.threshold_bps,
        heartbeat_slots = settings.heartbeat_slots,
        silence_ms = config.silence.as_millis(),
        model_budget_ms = config.model_budget.as_millis(),
        "configuration"
    );
    // Checks the settings against the vault's freshness limit on chain; the
    // feed is not opened for an engine that will not run.
    let cycle = match Cycle::start(step, sender, settings, config.silence, SystemMonotonic) {
        Ok(cycle) => cycle,
        Err(stop) => return Ok(stop),
    };

    let mut ids = vec![config.base_feed];
    ids.extend(config.quote_feed);
    let reader = Reader::new(
        &config.hermes_url,
        ids,
        config.api_key.map(|key| key.expose().to_owned()),
        config.feed_policy,
        Https::default(),
        SystemClock,
    );
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("feed".to_owned())
        .spawn(move || reader.run(&tx))
        .context("cannot start the feed reader")?;

    let mut watch = Watch::new(Silence::new(config.silence));
    let stop = cycle.run(&mut watch, &rx);
    meter.snapshot(Instant::now()).log();
    Ok(stop)
}
