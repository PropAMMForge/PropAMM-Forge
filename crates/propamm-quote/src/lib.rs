//! PropAMM quote math.
//!
//! The crate deliberately depends neither on Solana nor on std: it is built the
//! same way by the BPF compiler inside the on-chain program, by the host compiler
//! inside the router adapter and by the test run. One implementation for three
//! consumers is what makes quote/execution parity (SC-006) a property of the
//! construction rather than of developer discipline.
//!
//! The contents come in T006.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(clippy::arithmetic_side_effects)]
