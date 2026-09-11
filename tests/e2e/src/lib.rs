//! Harness for the end-to-end runs: a real validator, real mints, the real
//! `forge` binary.
//!
//! # Why not Mollusk
//!
//! `tests/program` executes instructions in the SVM without a network — that is
//! where the guards and the CU budget are checked. Here something else is checked:
//! that the **product** takes a person from an empty directory to the first swap
//! (SC-001). That is a stopwatch measurement, and it can be measured only on what
//! really runs: the `forge` process, the validator process, transactions on the network.
//!
//! # What is required of the machine
//!
//! `solana-test-validator` in `PATH`, a free port **8899** and a built
//! `target/deploy/propamm_vault.so`. This port and no other: `forge init
//! --cluster localnet` writes the default node address into `propamm.toml`, and
//! substituting it after `init` would mean measuring SC-001 on a config the user
//! does not get.
//!
//! # The stopwatch
//!
//! **Exactly five `forge` commands** fall under SC-001. The validator and the
//! pair's mints are a precondition, not a user step: on devnet the mints already
//! exist (nobody mints them to deploy an AMM), and so does the network. So the
//! preparation is done before the stopwatch starts and does not count as a command.

pub mod forge;
pub mod mints;
pub mod net;
pub mod trader;
pub mod validator;

/// Port of the local validator.
///
/// Not a parameter: it is baked into [`Cluster::Localnet`](propamm_cli::config::Cluster)
/// and arrives in `propamm.toml` via `forge init`.
pub const RPC_PORT: u16 = 8899;

/// The node address `forge` will see.
pub const RPC_URL: &str = "http://127.0.0.1:8899";
