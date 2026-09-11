//! A local validator for the duration of the run.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use propamm_cli::rpc::Rpc;

use crate::{RPC_PORT, RPC_URL};

/// How long to wait for the node to answer `getHealth`.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const READY_POLL: Duration = Duration::from_millis(250);

/// A validator that lives exactly as long as this struct.
pub struct Validator {
    child: Child,
    ledger: PathBuf,
    log: PathBuf,
    pub rpc: Rpc,
}

impl Validator {
    /// Bring up a validator with the program in genesis.
    ///
    /// The program is put **into genesis** rather than uploaded by transaction: an
    /// upload would cost several SOL and minutes for 300 KB of bytecode, and the
    /// product does not upload bytecode either (FR-002a) — `forge deploy` creates
    /// an account in an already present program.
    ///
    /// # Errors
    ///
    /// If the port is taken, the artifact is missing, the binary is not in `PATH`
    /// or the node did not come up within [`READY_TIMEOUT`].
    pub fn start(program_id: &Pubkey, artifact: &Path) -> Result<Self> {
        if !artifact.exists() {
            bail!(
                "no {} — first: scripts/wsl-build.sh build-sbf",
                artifact.display()
            );
        }
        ensure_port_free()?;

        // The ledger goes into a temporary directory, not the repository. In WSL that
        // the native file system: on a mounted Windows drive the validator writes hundreds of
        // megabytes through 9p and comes up many times slower, while SC-001 measures
        // slower, while SC-001 measures minutes.
        let base = std::env::temp_dir().join(format!("propamm-e2e-{}", std::process::id()));
        let ledger = base.join("ledger");
        let log = base.join("validator.log");
        std::fs::create_dir_all(&base)
            .with_context(|| format!("cannot create {}", base.display()))?;

        let log_file = std::fs::File::create(&log)
            .with_context(|| format!("cannot create {}", log.display()))?;
        let log_err = log_file
            .try_clone()
            .context("the log descriptor cannot be duplicated")?;

        let child = Command::new("solana-test-validator")
            .arg("--ledger")
            .arg(&ledger)
            .arg("--bpf-program")
            .arg(program_id.to_string())
            .arg(artifact)
            .arg("--rpc-port")
            .arg(RPC_PORT.to_string())
            // `--reset` is mandatory: without it the validator picks up the ledger from
            // the previous run together with the old vault state, and the test
            // "deployed for the first time" would pass on an already deployed one.
            .arg("--reset")
            .arg("--quiet")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()
            .context("cannot launch solana-test-validator — is it in PATH?")?;

        let mut validator = Self {
            child,
            ledger,
            log,
            rpc: Rpc::new(RPC_URL),
        };
        validator.wait_until_ready()?;
        validator.ensure_program_is_executable(program_id)?;
        Ok(validator)
    }

    /// The tail of the validator log — the only place the cause is visible if it did not come up.
    #[must_use]
    pub fn log_tail(&self, lines: usize) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            // The process is checked first: if the validator died (most often — a taken
            // port or a broken artifact), `getHealth` can be polled right up to the
            // deadline and yield "did not come up in 90 s" instead of the cause.
            if let Some(status) = self
                .child
                .try_wait()
                .context("the process status cannot be read")?
            {
                bail!(
                    "solana-test-validator exited with status {status}\n── log tail ──\n{}",
                    self.log_tail(30)
                );
            }
            if self.rpc.call("getHealth", serde_json::json!([])).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "the node did not answer getHealth in {} s\n── log tail ──\n{}",
                    READY_TIMEOUT.as_secs(),
                    self.log_tail(30)
                );
            }
            std::thread::sleep(READY_POLL);
        }
    }

    /// The program from genesis has to be in place and executable.
    ///
    /// The check is not redundant: an SBPFv3 artifact lands in genesis without a
    /// single complaint — the account looks right — and the very first call gives
    /// "Program is not deployed". Without this check the run would fail on
    /// `forge deploy` with a message about something else.
    fn ensure_program_is_executable(&self, program_id: &Pubkey) -> Result<()> {
        let account = self
            .rpc
            .get_account(program_id)?
            .with_context(|| format!("program {program_id} is not on the network"))?;
        if !account.executable {
            bail!("account {program_id} exists but is not executable");
        }
        Ok(())
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // The ledger is hundreds of megabytes per run. The log stays: if the test
        // failed, its tail is already printed, but the full file is sometimes needed.
        let _ = std::fs::remove_dir_all(&self.ledger);
    }
}

/// Refuse loudly if someone is already on the port.
///
/// Silently continuing here is the most expensive mistake possible: a foreign
/// (or our own forgotten) validator would answer everything, the run would pass,
/// and a green test would mean "something on the network works", not "what we just built works".
fn ensure_port_free() -> Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, RPC_PORT));
    match TcpStream::connect_timeout(&address, Duration::from_millis(300)) {
        Ok(_) => bail!(
            "port {RPC_PORT} is already taken — stop the other node: pkill -f 'solana-test-valid[a]tor'"
        ),
        Err(err) if matches!(err.kind(), ErrorKind::ConnectionRefused | ErrorKind::TimedOut) => {
            Ok(())
        }
        Err(err) => bail!("port {RPC_PORT} cannot be checked: {err}"),
    }
}
