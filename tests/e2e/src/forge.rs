//! A `forge` launcher with a stopwatch and a command counter.
//!
//! SC-001 is phrased "by the stopwatch", so the **process** is measured, not a
//! library call: the user pays for argument parsing, config reading, connecting
//! to the node and transaction confirmation, and none of those shares can be
//! written off as "not our code".

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// One `forge` invocation.
pub struct Step {
    /// The command line as it would be typed by hand.
    pub command: String,
    pub elapsed: Duration,
    pub stdout: String,
}

/// The sequence of commands a user goes through.
pub struct Forge {
    binary: PathBuf,
    steps: Vec<Step>,
}

impl Forge {
    /// Find the built binary next to the test.
    ///
    /// The path derives from `current_exe` rather than being assembled from
    /// `CARGO_MANIFEST_DIR`: the test sits in `<target>/<profile>/deps/`, and such a
    /// derivation survives both `CARGO_TARGET_DIR` and a profile change.
    ///
    /// # Errors
    ///
    /// If the binary is missing — it has to be built separately, because `cargo test`
    /// builds only what the test depends on, and it does not depend on the *binary*.
    pub fn new() -> Result<Self> {
        let test = std::env::current_exe().context("unknown where the test was launched from")?;
        let profile = test
            .parent()
            .and_then(Path::parent)
            .context("the test is not where it was expected")?;
        let binary = profile.join(if cfg!(windows) { "forge.exe" } else { "forge" });
        if !binary.exists() {
            bail!(
                "no {} — first: cargo build -p propamm-cli",
                binary.display()
            );
        }
        Ok(Self {
            binary,
            steps: Vec::new(),
        })
    }

    /// Run one command and count it towards SC-001.
    ///
    /// # Errors
    ///
    /// If the process did not start or exited with a non-zero code; the message
    /// carries both streams, because `forge` writes the explanation of a refusal to
    /// stderr and the hint about the next step to stdout.
    pub fn run(&mut self, args: &[&str]) -> Result<&Step> {
        let started = Instant::now();
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .with_context(|| format!("cannot launch {}", self.binary.display()))?;
        let elapsed = started.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let command = format!("forge {}", args.join(" "));
        if !output.status.success() {
            bail!(
                "\"{command}\" exited with {}\n── stdout ──\n{stdout}\n── stderr ──\n{stderr}",
                output.status
            );
        }

        self.steps.push(Step {
            command,
            elapsed,
            stdout,
        });
        Ok(self.steps.last().expect("just pushed"))
    }

    /// How many commands were counted — the same number SC-001 limits.
    #[must_use]
    pub fn commands(&self) -> usize {
        self.steps.len()
    }

    /// The total time of all commands.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.steps.iter().map(|step| step.elapsed).sum()
    }

    /// The measurement table. Printed always, not only on failure: SC-001 is a
    /// number, and it has to stay in the run output, not only in the verdict.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = String::from("\nSC-001 — from an empty directory to the first swap:\n");
        for (index, step) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "  {}. {:<58} {:>7.2} s\n",
                index + 1,
                step.command,
                step.elapsed.as_secs_f64()
            ));
        }
        out.push_str(&format!(
            "  total: {} commands, {:.2} s\n",
            self.commands(),
            self.elapsed().as_secs_f64()
        ));
        out
    }
}
