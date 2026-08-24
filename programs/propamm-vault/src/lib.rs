//! PropAMM on-chain program: a vault that serves swaps between exactly two assets
//! declared at deployment (FR-004).
//!
//! # What is here now
//!
//! The skeleton and the state layout. There are no instructions yet: `initialize_vault` (T013),
//! `deposit`/`withdraw` (T014), `set_pricing_authority`/`set_risk_limits` (T015),
//! `update_quote` (T016), `swap` (T017), `halt`/`resume` (T052). The empty
//! `#[program]` is not a throwaway stub: it pins the program ID and gives the build
//! an artifact the instruction tests build on.
//!
//! # Where the math lives
//!
//! Not here. The amount paid out is computed by [`propamm_quote::compute_swap`] —
//! the same code the router adapter uses to compute it. The program adds what the
//! crate cannot see: the `pricing_authority` signature, the `halted` flag, token
//! account ownership and Token-2022 extensions. `Ok` from the crate does not mean
//! the swap is allowed (see `state::Vault::quote_params`).

use anchor_lang::prelude::*;

pub mod state;

pub use state::*;

declare_id!("77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq");

#[program]
pub mod propamm_vault {}
