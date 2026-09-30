//! `propamm-client` — the vault program as seen from off chain.
//!
//! Shared by `forge` and the engine: the thin JSON-RPC client ([`rpc`]) and the
//! instruction builders and account decoders ([`chain`]). Both go to the
//! network with the same instructions, so there is one copy of them.

pub mod chain;
pub mod rpc;
