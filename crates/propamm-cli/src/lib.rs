//! `forge` — the Prop AMM builder.
//!
//! The crate is built as a library, not only as a binary: commands are tested by
//! calling them (`init::run`), not by spawning a process and parsing stdout. A
//! test that reads output catches a change of wording and misses a change of behaviour.
//!
//! # The model everything stands on
//!
//! There is **one** `propamm_vault` program on the network. A vault is a PDA of
//! the owner and the pair, so "deploy your own AMM" means creating your own
//! account in the shared program, not uploading your own bytecode. Because of
//! this the user needs neither Rust, nor Anchor, nor several SOL for a program —
//! and that is exactly why SC-001 (15 minutes, ≤ 5 commands) is reachable.

pub mod amount;
pub mod chain;
pub mod config;
pub mod deploy;
pub mod fund;
pub mod init;
pub mod quote;
pub mod rpc;
pub mod status;
