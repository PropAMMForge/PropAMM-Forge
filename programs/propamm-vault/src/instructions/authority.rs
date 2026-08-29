//! Replacing authorities and risk limits — all by the owner only (FR-010).
//!
//! # Why the authorities are separated at all
//!
//! The quote key is hot: it lives in the engine, signs dozens of transactions an
//! hour and for that very reason is the most likely to leak. The capital key is
//! cold and rarely needed. Separating them makes it possible to replace the first
//! without moving the second — and this instruction is what makes FR-010 enforceable.
//!
//! # Why replacing the quote key clears the quote
//!
//! Replacement means the previous quoter is no longer trusted — otherwise it would
//! not be done. Leaving on chain a price that this very quoter posted would mean
//! trading at a quote from a source that is no longer trusted. It is the same
//! logic by which `withdraw` clears the quote (T014), and the cost is the same:
//! the engine posts a new one on the next tick.
//!
//! Replacing the halt key does not touch the quote: it says nothing about the price.

use anchor_lang::prelude::*;

use crate::errors::VaultError;
use crate::state::{Vault, VAULT_SEED};

/// The owner's risk limits — what the engine cannot change.
///
/// `max_size_base` is **not** here: under FR-006 and FR-008 the maximum size is
/// carried by the quote, so it is written by `pricing_authority` in `update_quote` (T016).
/// A size ceiling from the owner would save nothing: a key that can post any
/// price is not restrained by a size limit — it is restrained by `max_skew_bps`,
/// and that is right here.
///
/// This is the only place the limits are validated.
///
/// The same type is used by `initialize_vault`: if the checks lived separately,
/// `set_risk_limits` would let through what deployment refused, and a risk
/// limit would depend on which road it was set by.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiskLimits {
    /// Quote freshness limit in slots (FR-007).
    pub max_quote_age_slots: u32,
    /// Hard bound on inventory skew in basis points (FR-026).
    pub max_skew_bps: u16,
}

impl RiskLimits {
    pub fn validate(&self) -> Result<()> {
        require!(self.max_quote_age_slots > 0, VaultError::InvalidRiskLimits);
        // Skew never exceeds 100% by construction of `inventory_skew_bps`,
        // so a bound above 10 000 bps limits nothing and merely looks like a bound.
        require!(
            self.max_skew_bps <= propamm_quote::BPS_DENOM,
            VaultError::InvalidRiskLimits
        );
        Ok(())
    }
}

/// The shared account shape for all three instructions: the owner's signature and
/// their vault. Nothing but state moves here — no token accounts needed.
#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [
            VAULT_SEED,
            owner.key().as_ref(),
            vault.base_mint.as_ref(),
            vault.quote_mint.as_ref(),
        ],
        bump = vault.bump,
        has_one = owner @ VaultError::OwnerOnly,
    )]
    pub vault: Account<'info, Vault>,
}

/// Replace the quote signer (FR-010). Clears the current quote.
pub fn handle_set_pricing_authority(ctx: Context<AdminOnly>, new_authority: Pubkey) -> Result<()> {
    require_keys_neq!(
        new_authority,
        Pubkey::default(),
        VaultError::InvalidAuthority
    );

    let vault = &mut ctx.accounts.vault;
    vault.pricing_authority = new_authority;
    vault.clear_quote();

    Ok(())
}

/// Replace the emergency-halt signer (FR-024, FR-023c).
///
/// Formally FR-010 speaks only of the quote key, but without this instruction a
/// lost or compromised halt key can never be replaced — and the emergency switch
/// stays a promise with nothing behind it.
pub fn handle_set_halt_authority(ctx: Context<AdminOnly>, new_authority: Pubkey) -> Result<()> {
    require_keys_neq!(
        new_authority,
        Pubkey::default(),
        VaultError::InvalidAuthority
    );
    ctx.accounts.vault.halt_authority = new_authority;
    Ok(())
}

/// Change the risk limits (FR-007, FR-026).
///
/// Narrowing the skew bound below the current state is **allowed** and does not
/// lock the vault: the bound limits swaps, not state (Phase 1 decision), so a
/// rebalancing swap stays possible even when the current skew is already past the new bound.
pub fn handle_set_risk_limits(ctx: Context<AdminOnly>, limits: RiskLimits) -> Result<()> {
    limits.validate()?;

    let vault = &mut ctx.accounts.vault;
    vault.max_quote_age_slots = limits.max_quote_age_slots;
    vault.max_skew_bps = limits.max_skew_bps;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sane() -> RiskLimits {
        RiskLimits {
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
        }
    }

    #[test]
    fn sane_limits_pass() {
        assert!(sane().validate().is_ok());
    }

    #[test]
    fn limits_that_do_not_limit_are_rejected() {
        let mut l = sane();
        l.max_quote_age_slots = 0;
        assert!(l.validate().is_err());

        let mut l = sane();
        l.max_skew_bps = propamm_quote::BPS_DENOM + 1;
        assert!(l.validate().is_err());
    }

    #[test]
    fn a_hundred_percent_skew_is_still_a_valid_bound() {
        let mut l = sane();
        l.max_skew_bps = propamm_quote::BPS_DENOM;
        assert!(l.validate().is_ok());
    }

    #[test]
    fn limits_survive_a_round_trip() {
        let l = sane();
        let mut bytes = Vec::new();
        l.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 4 + 2);
        assert_eq!(RiskLimits::deserialize(&mut bytes.as_slice()).unwrap(), l);
    }
}
