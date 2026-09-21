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
//! - [`feed`] — Pyth Hermes over SSE with the timestamp and confidence check (FR-012).

#![forbid(unsafe_code)]

pub mod feed;
