//! `propamm-engine` — the off-chain pricing engine, Rust core (FR-015).
//!
//! The engine is what keeps a quote alive without a human: it reads the price
//! feed, computes the mid, the spread and the size, and posts them on chain with
//! the `pricing_authority` key. It is hosted by the client, next to that hot key.
//!
//! The crate is a library first: every stage is a type that is tested by a plain
//! `cargo test` on recorded input — the network, the clock and the model process
//! are behind traits. The binary, `propamm-engine`, is a thin `main` over
//! [`config`] and [`cycle`]: the configuration from the environment, the feed
//! reader on its own thread, the loop on the main one.
//!
//! What is here so far:
//!
//! - [`feed`] — Pyth Hermes over SSE with the timestamp and confidence check
//!   (FR-012), and the silence rule that withdraws the quote instead of
//!   repeating the last known price (FR-014).
//! - [`model`] — the pricing model, the engine's one replaceable point (FR-015):
//!   the [`model::PricingModel`] trait, the built-in spread-and-skew model
//!   (FR-013), and a model in another process over stdin/stdout (FR-015a).
//! - [`tick`] — the model behind a response budget: a late answer skips the
//!   tick with the reason recorded instead of posting a stale price (FR-015b).
//! - [`policy`] — the hybrid update rule: post when a side moved past the
//!   threshold or the heartbeat is up (FR-011), and refuse a heartbeat whose
//!   replacement would land after the quote it replaces has expired (FR-011a).
//! - [`chain`] — the network as the engine uses it: one read of the vault and
//!   its treasuries at the tip, a blockhash, a send; a refusal sorted by what
//!   the engine does next.
//! - [`sender`] — signing with `pricing_authority`, and reading the book to
//!   see what landed; the slot clock.
//! - [`cycle`] — the tick loop: feed, model, rule and sender together; a halt
//!   pauses it, a refusal no retry can fix stops it.
//! - [`config`] — the environment the binary starts from (`.env.example`).
//! - [`meter`] — the node calls by method, for the free tier's quota.

#![forbid(unsafe_code)]

pub mod chain;
pub mod config;
pub mod cycle;
pub mod feed;
pub mod meter;
pub mod model;
pub mod policy;
pub mod sender;
pub mod tick;
