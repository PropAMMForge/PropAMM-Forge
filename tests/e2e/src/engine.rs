//! A `propamm-engine` launcher: the binary as a process, with its log in a file.
//!
//! The engine runs as the operator runs it — the same binary, configured by
//! environment only — because that is what SC-011 is about: a second model is
//! swapped in by configuration, with nothing rebuilt.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// A running engine; stopped when dropped.
pub struct Engine {
    child: Child,
    log: PathBuf,
}

impl Engine {
    /// The built binary next to the test — see [`Forge::new`](crate::forge::Forge::new).
    ///
    /// # Errors
    ///
    /// If it is missing: `cargo test` builds what the test links, not the binaries it launches.
    pub fn binary() -> Result<PathBuf> {
        let test = std::env::current_exe().context("unknown where the test was launched from")?;
        let profile = test
            .parent()
            .and_then(Path::parent)
            .context("the test is not where it was expected")?;
        let binary = profile.join(if cfg!(windows) {
            "propamm-engine.exe"
        } else {
            "propamm-engine"
        });
        if !binary.exists() {
            bail!(
                "no {} — first: cargo build -p propamm-engine",
                binary.display()
            );
        }
        Ok(binary)
    }

    /// Start the engine with exactly `env` — nothing inherited but `PATH`
    /// and `HOME`, so no key or node of the machine's own leaks into the run.
    ///
    /// # Errors
    ///
    /// If the binary is missing or does not start.
    pub fn start(env: &BTreeMap<&str, String>, log: &Path) -> Result<Self> {
        let binary = Self::binary()?;
        let out = File::create(log).with_context(|| format!("cannot create {}", log.display()))?;
        let mut command = Command::new(&binary);
        command
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(out));
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let child = command
            .spawn()
            .with_context(|| format!("cannot launch {}", binary.display()))?;
        Ok(Self {
            child,
            log: log.to_path_buf(),
        })
    }

    /// Is it still running? If not, why — with the tail of its log.
    ///
    /// # Errors
    ///
    /// If the engine exited.
    pub fn alive(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            bail!(
                "the engine exited with {status}\n── log tail ──\n{}",
                self.log_tail(30)
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    #[must_use]
    pub fn log_tail(&self, lines: usize) -> String {
        let text = self.log();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    /// The latest `rpc calls` totals the engine logged: method → count, and
    /// the seconds they cover.
    #[must_use]
    pub fn rpc_calls(&self) -> Option<(Duration, BTreeMap<String, u64>)> {
        parse_rpc_calls(&self.log())
    }

    /// Stop the process and wait for it.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The last `rpc calls` line of a log: `elapsed_s=… total=… calls=getSlot=1 sendTransaction=3`.
///
/// The `calls` field is the last on the line by construction (`meter::Snapshot::log`).
fn parse_rpc_calls(log: &str) -> Option<(Duration, BTreeMap<String, u64>)> {
    let line = log.lines().rev().find(|line| line.contains("rpc calls"))?;
    let elapsed = line
        .split_whitespace()
        .find_map(|word| word.strip_prefix("elapsed_s="))?
        .parse()
        .ok()?;
    let calls = line.split_once(" calls=")?.1;
    let by_method = calls
        .split_whitespace()
        .filter_map(|pair| {
            let (method, count) = pair.split_once('=')?;
            Some((method.to_owned(), count.parse().ok()?))
        })
        .collect();
    Some((Duration::from_secs(elapsed), by_method))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_rpc_calls_line_is_read() {
        let log = "\
2026-10-02T10:00:00Z  INFO propamm_engine::meter: rpc calls elapsed_s=60 total=4 calls=getSlot=1 sendTransaction=3
2026-10-02T10:00:30Z  INFO propamm_engine::cycle: posting why=moved
2026-10-02T10:01:00Z  INFO propamm_engine::meter: rpc calls elapsed_s=120 total=9 calls=getLatestBlockhash=2 getMultipleAccounts=2 sendTransaction=5
";
        let (elapsed, calls) = parse_rpc_calls(log).unwrap();
        assert_eq!(elapsed, Duration::from_secs(120));
        assert_eq!(calls["sendTransaction"], 5);
        assert_eq!(calls["getMultipleAccounts"], 2);
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn a_log_without_totals_has_none() {
        assert_eq!(parse_rpc_calls("engine started\n"), None);
    }
}
