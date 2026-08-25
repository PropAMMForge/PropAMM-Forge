//! PropAMM on-chain program: a vault that serves swaps between exactly two assets
//! declared at deployment (FR-004).
//!
//! # What is here now
//!
//! State layout ([`state::Vault`]), asset screening per FR-005
//! ([`mint_guard`]) and the first instruction — `initialize_vault`.
//!
//! Next: `deposit`/`withdraw` (T014), `set_pricing_authority`/`set_risk_limits`
//! (T015), `update_quote` (T016), `swap` (T017), `halt`/`resume` (T052).
//!
//! # Where the math lives
//!
//! Not here. The amount paid out is computed by [`propamm_quote::compute_swap`] —
//! the same code the router adapter uses to compute it. The program adds what the
//! crate cannot see: the `pricing_authority` signature, the `halted` flag, token
//! account ownership and Token-2022 extensions. `Ok` from the crate does not mean
//! the swap is allowed (see `state::Vault::quote_params`).

use anchor_lang::prelude::*;

pub mod errors;
pub mod instructions;
pub mod mint_guard;
pub mod state;

pub use errors::*;
pub use instructions::*;
pub use state::*;

declare_id!("77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq");

#[program]
pub mod propamm_vault {
    use super::*;

    /// Deployment: the pair is fixed forever, the treasury is created empty,
    /// there is no quote yet (FR-001, FR-002, FR-004, FR-005).
    pub fn initialize_vault(
        ctx: Context<InitializeVault>,
        args: InitializeVaultArgs,
    ) -> Result<()> {
        instructions::initialize_vault::handler(ctx, args)
    }
}
