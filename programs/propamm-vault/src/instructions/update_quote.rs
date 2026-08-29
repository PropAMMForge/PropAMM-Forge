//! `update_quote` / `clear_quote` — the quote is posted and cleared by
//! `pricing_authority`, not the owner (FR-006, FR-007, FR-010, FR-014).
//!
//! # The slot comes from the chain, not from the arguments
//!
//! `quote_slot` is the source of the quote's age, i.e. what FR-007 rests on.
//! A slot sent by the client would make the age a value claimed by the party
//! being checked: a stale quote would be posted as "fresh" after the fact,
//! while the freshness guard stayed in place and caught nothing.
//!
//! # The domain is checked by the math itself
//!
//! We do not restate here the rules "spread below 100%" and "skew does not drive
//! the price to zero": instead both sides are computed via
//! [`propamm_quote::side_price_e9`] and the parameters it refuses are rejected.
//! A copy of the rules in the program would diverge from the crate — and they
//! must not diverge, because the same crate answers the router (SC-006).
//!
//! Both sides are computed on purpose: bid subtracts the spread, ask adds it,
//! and they cross the domain boundary in different places.
//!
//! # Why clearing is a separate instruction rather than `mid_e9 = 0`
//!
//! The Anchor coder writes a missing field as zero and does not complain. If a
//! zero in `mid_e9` meant "clear the quote", an argument forgotten by a builder
//! would silently remove the price instead of failing. So `update_quote` **refuses**
//! zero, and clearing is a separate instruction with nothing to forget.

use anchor_lang::prelude::*;
use propamm_quote::{side_price_e9, QuoteParams, Side};

use crate::errors::VaultError;
use crate::state::{Vault, VAULT_SEED};

/// Quote parameters (FR-006). There is no "per side" price here — both are
/// derived from these numbers deterministically (FR-006a).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteUpdate {
    /// Market mid in fixed point `1e9`, in **raw units** of quote per raw unit
    /// of base. Converting a human price is the engine's job; the program does
    /// not read the mints' decimals.
    pub mid_e9: u128,
    /// Half of the spread in basis points.
    pub spread_bps: u16,
    /// Mid shift by inventory skew (FR-013).
    pub skew_bps: i16,
    /// Maximum order size in the base asset (FR-008). It belongs to the quote,
    /// not to the risk limits: the size is declared by whoever declares the price.
    pub max_size_base: u64,
}

#[derive(Accounts)]
pub struct Quoting<'info> {
    /// The quote signer (FR-010). The owner of the capital is not needed here — and
    /// that is the whole point of separating the authorities.
    pub pricing_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [
            VAULT_SEED,
            vault.owner.as_ref(),
            vault.base_mint.as_ref(),
            vault.quote_mint.as_ref(),
        ],
        bump = vault.bump,
        has_one = pricing_authority @ VaultError::PricingAuthorityOnly,
    )]
    pub vault: Account<'info, Vault>,
}

/// Post a quote (FR-006, FR-007).
pub fn handle_update_quote(ctx: Context<Quoting>, quote: QuoteUpdate) -> Result<()> {
    // A halted vault does not quote: otherwise the halt would clear the price and
    // the engine's next tick would put it back (FR-024).
    require!(!ctx.accounts.vault.halted, VaultError::VaultHalted);

    require!(quote.mid_e9 > 0, VaultError::InvalidQuote);
    // A zero size is a quote nothing can be swapped at; it differs from "no price"
    // in nothing but the price being visible.
    require!(quote.max_size_base > 0, VaultError::InvalidQuote);

    let slot = Clock::get()?.slot;

    let candidate = QuoteParams {
        mid_e9: quote.mid_e9,
        spread_bps: quote.spread_bps,
        skew_bps: quote.skew_bps,
        max_size_base: quote.max_size_base,
        quote_slot: slot,
        max_quote_age_slots: ctx.accounts.vault.max_quote_age_slots,
        max_skew_bps: ctx.accounts.vault.max_skew_bps,
    };
    for side in [Side::BaseToQuote, Side::QuoteToBase] {
        if let Err(err) = side_price_e9(&candidate, side) {
            msg!("quote rejected: {}", err);
            return Err(VaultError::InvalidQuote.into());
        }
    }

    let vault = &mut ctx.accounts.vault;
    vault.mid_e9 = quote.mid_e9;
    vault.spread_bps = quote.spread_bps;
    vault.skew_bps = quote.skew_bps;
    vault.max_size_base = quote.max_size_base;
    vault.quote_slot = slot;

    Ok(())
}

/// Clear the quote (FR-014): the feed went silent, and repeating the last known
/// price is worse than not quoting at all.
///
/// Allowed on a halted vault too: removing a price is always a safe action.
pub fn handle_clear_quote(ctx: Context<Quoting>) -> Result<()> {
    ctx.accounts.vault.clear_quote();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use propamm_quote::BPS_DENOM;

    fn params(spread_bps: u16, skew_bps: i16) -> QuoteParams {
        QuoteParams {
            mid_e9: 150_000_000,
            spread_bps,
            skew_bps,
            max_size_base: 1_000,
            quote_slot: 10,
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
        }
    }

    /// What the program accepts must match what the math accepts. The test checks
    /// the boundary from both sides: inside, both sides compute; outside, at least
    /// one fails.
    fn both_sides_ok(p: &QuoteParams) -> bool {
        side_price_e9(p, Side::BaseToQuote).is_ok() && side_price_e9(p, Side::QuoteToBase).is_ok()
    }

    #[test]
    fn a_normal_quote_is_inside_the_domain() {
        assert!(both_sides_ok(&params(25, 0)));
        assert!(both_sides_ok(&params(0, 0)));
        assert!(both_sides_ok(&params(BPS_DENOM - 1, 0)));
    }

    #[test]
    fn a_spread_of_a_hundred_percent_is_outside() {
        assert!(!both_sides_ok(&params(BPS_DENOM, 0)));
    }

    /// A skew that drives the mid to zero is outside the domain too: past that the
    /// price is not "very low", it simply does not exist.
    #[test]
    fn a_skew_that_cancels_the_mid_is_outside() {
        assert!(!both_sides_ok(&params(25, -(BPS_DENOM as i16))));
        assert!(both_sides_ok(&params(25, -(BPS_DENOM as i16) + 1)));
    }

    #[test]
    fn quote_survives_a_round_trip() {
        let q = QuoteUpdate {
            mid_e9: 150_000_000,
            spread_bps: 25,
            skew_bps: -30,
            max_size_base: 1_000_000,
        };
        let mut bytes = Vec::new();
        q.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 16 + 2 + 2 + 8);
        assert_eq!(QuoteUpdate::deserialize(&mut bytes.as_slice()).unwrap(), q);
    }
}
