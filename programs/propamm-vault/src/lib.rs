//! PropAMM on-chain program: a vault that serves swaps between exactly two assets
//! declared at deployment (FR-004).
//!
//! # What is here now
//!
//! State layout ([`state::Vault`]), asset screening per FR-005
//! ([`mint_guard`]), deployment (`initialize_vault`), capital movement
//! (`deposit` / `withdraw`), administration (`set_*`) and quoting
//! (`update_quote` / `clear_quote`), the swap itself (`swap`) and the accounting events
//! ([`events`]).
//!
//! Next: `halt` / `resume` (T052).
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
pub mod events;
pub mod instructions;
pub mod mint_guard;
pub mod state;

// The glob is mandatory here, not a convenience: `#[program]` looks for the
// generated `__client_accounts_*` modules at the crate root, and without the
// re-export the macro fails with "unresolved import `crate`", which says nothing
// about the real cause. This is also why handlers are named `handle_*`: otherwise
// they would collide with the entry points this same macro creates from their names.
pub use errors::*;
pub use events::*;
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
        instructions::initialize_vault::handle_initialize_vault(ctx, args)
    }

    /// Replace the quote signer (FR-010). Clears the current quote:
    /// replacement means the previous price source is no longer trusted.
    pub fn set_pricing_authority(ctx: Context<AdminOnly>, new_authority: Pubkey) -> Result<()> {
        instructions::authority::handle_set_pricing_authority(ctx, new_authority)
    }

    /// Replace the emergency-halt signer (FR-024, FR-023c).
    pub fn set_halt_authority(ctx: Context<AdminOnly>, new_authority: Pubkey) -> Result<()> {
        instructions::authority::handle_set_halt_authority(ctx, new_authority)
    }

    /// Change the risk limits (FR-007, FR-008, FR-026).
    pub fn set_risk_limits(ctx: Context<AdminOnly>, limits: RiskLimits) -> Result<()> {
        instructions::authority::handle_set_risk_limits(ctx, limits)
    }

    /// Post a quote (FR-006, FR-007). Signed by `pricing_authority`;
    /// `quote_slot` is taken from the chain, not from the arguments.
    pub fn update_quote(ctx: Context<Quoting>, quote: QuoteUpdate) -> Result<()> {
        instructions::update_quote::handle_update_quote(ctx, quote)
    }

    /// Clear the quote (FR-014) — the feed went silent, and repeating the last
    /// known price is worse than not quoting at all.
    pub fn clear_quote(ctx: Context<Quoting>) -> Result<()> {
        instructions::update_quote::handle_clear_quote(ctx)
    }

    /// Swap at the posted quote (FR-007, FR-008, FR-009, FR-026).
    /// Four guards are computed by `propamm_quote`; the program adds `halted`
    /// and the check of the accounts against the state.
    pub fn swap(ctx: Context<Swap>, args: SwapArgs) -> Result<()> {
        instructions::swap::handle_swap(ctx, args)
    }

    /// Treasury deposit by the owner (FR-003).
    pub fn deposit(ctx: Context<MoveCapital>, amount: u64) -> Result<()> {
        instructions::treasury::handle_deposit(ctx, amount)
    }

    /// Capital withdrawal by the owner (FR-003). Clears the current quote —
    /// client decision of 2026-08-27, rationale in the header of `treasury.rs`.
    pub fn withdraw(ctx: Context<MoveCapital>, amount: u64) -> Result<()> {
        instructions::treasury::handle_withdraw(ctx, amount)
    }
}
