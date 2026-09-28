//! `propamm-engine` — the off-chain pricing engine, Rust core (FR-015).
//!
//! The engine is what keeps a quote alive without a human: it reads the price
//! feed, computes the mid, the spread and the size, and posts them on chain with
//! the `pricing_authority` key. It is hosted by the client, next to that hot key.
//!
//! The crate is a library first: every stage is a type that is tested by a plain
//! `cargo test` on recorded input — the network, the clock and the model process
//! are behind traits. The binary comes later, once there is a tick to run.
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

#![forbid(unsafe_code)]

pub mod feed;
pub mod model;
pub mod tick;
