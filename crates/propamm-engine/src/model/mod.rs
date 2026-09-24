//! The pricing model — the one replaceable point of the engine (FR-015).
//!
//! Everything else in this crate is transport: read the feed, judge it, decide
//! when to post, sign, send. What to quote is this module's business, and it is
//! the part a client is expected to replace with their own.
//!
//! For now there is one model, [`spread_skew`]. The trait that makes it
//! swappable — and the external-process model behind it (FR-015a) — is T029;
//! until then the model is called directly, and its input and output types are
//! already the ones that trait will use.

pub mod spread_skew;
