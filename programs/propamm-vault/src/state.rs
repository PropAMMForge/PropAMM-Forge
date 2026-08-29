//! Vault state — one PDA per deployment.
//!
//! # Why the quote lives here and not in a separate account
//!
//! Every extra account in a swap is bytes in the transaction and CU spent on
//! deserialization, and SC-002 gives 60 000 CU for the whole instruction. Splitting
//! buys nothing: both accounts would be written by the same signer.
//!
//! # Why the layout is compact
//!
//! `try_accounts` in Anchor has a 4 KiB frame, and a bloated account shows up as
//! the warning "overwrites values in the frame" **on a formally successful
//! build** — i.e. silently. The size is pinned here by the test
//! [`tests::layout_stays_within_its_budget`]: if a field is added deliberately, the
//! number in the test is changed with it, and if not, the test fails before the
//! build starts to stay silent.
//!
//! There is deliberately no room for future fields. A reserve would give a false
//! freedom: a field appended after deployment still needs an account migration,
//! because old records carry no bytes for it. Cheaper to know that at compile time.

use anchor_lang::prelude::*;
use propamm_quote::QuoteParams;

/// Seed prefix of the vault PDA.
///
/// The address derives from `(owner, base_mint, quote_mint)` — see [`Vault::pda`].
pub const VAULT_SEED: &[u8] = b"vault";

/// Vault: the owner's capital, authorities, current quote and risk limits.
///
/// Layout (without the 8-byte discriminator):
///
/// | Field | Bytes | Offset |
/// |---|---|---|
/// | `owner`, `pricing_authority`, `halt_authority` | 3 × 32 | 0 |
/// | `base_mint`, `quote_mint` | 2 × 32 | 96 |
/// | `base_vault`, `quote_vault` | 2 × 32 | 160 |
/// | `mid_e9` | 16 | 224 |
/// | `max_size_base`, `quote_slot` | 2 × 8 | 240 |
/// | `max_quote_age_slots` | 4 | 256 |
/// | `spread_bps`, `skew_bps`, `max_skew_bps` | 3 × 2 | 260 |
/// | `halted`, `bump` | 2 × 1 | 266 |
///
/// Fields go from wide to narrow. Borsh writes them packed and knows nothing about
/// alignment, so the order here is not about memory — it is about readable offsets
/// in an account dump when a swap does not behave as expected.
#[account]
#[derive(InitSpace)]
pub struct Vault {
    /// The sole holder of the capital (FR-002, FR-003). There are no shares; an
    /// outside deposit is refused.
    pub owner: Pubkey,
    /// The right to post quotes (FR-010). Changed by the owner without moving
    /// funds — the engine key is hot, the capital key is not.
    pub pricing_authority: Pubkey,
    /// The right to halt in an emergency (FR-024, FR-023c). Separate on purpose:
    /// halting may be done by someone not trusted with the capital.
    pub halt_authority: Pubkey,
    /// Base asset of the pair; fixed at deployment (FR-004).
    pub base_mint: Pubkey,
    /// Quote asset of the pair; fixed at deployment (FR-004).
    pub quote_mint: Pubkey,
    /// Token account of the base asset owned by this PDA.
    pub base_vault: Pubkey,
    /// Token account of the quote asset owned by this PDA.
    pub quote_vault: Pubkey,
    /// Market mid in fixed point [`propamm_quote::PRICE_SCALE`], in **raw units**
    /// of quote per raw unit of base. The program does not read the mints'
    /// decimals — that would cost two extra accounts in the SC-002 budget.
    /// Zero means "no quote posted", not a price of zero.
    pub mid_e9: u128,
    /// Maximum order size in the base asset (FR-008).
    pub max_size_base: u64,
    /// Slot in which the quote was posted — the source of its age (FR-007).
    pub quote_slot: u64,
    /// Quote freshness limit in slots (FR-007).
    pub max_quote_age_slots: u32,
    /// Half of the spread in basis points.
    pub spread_bps: u16,
    /// Mid shift by inventory skew (FR-013); computed by the engine.
    pub skew_bps: i16,
    /// Hard bound on inventory skew in basis points (FR-026).
    pub max_skew_bps: u16,
    /// Emergency halt (FR-024). The one flag the program and the adapter each
    /// read on their own — and therefore the main place they could diverge.
    pub halted: bool,
    /// Bump PDA.
    pub bump: u8,
}

impl Vault {
    /// Account size including the discriminator — what goes into `space` on `init`.
    pub const SPACE: usize = 8 + Self::INIT_SPACE;

    /// Remove the quote — the "no price" state, the same as after deployment.
    ///
    /// There are already three places the quote disappears from: capital withdrawal
    /// (T014), replacement of the quote signer (T015) and explicit clearing by the
    /// engine (T016). Three copies of this assignment would diverge on the first new
    /// quote field — that is exactly how `max_size_base` almost outlived the price.
    pub fn clear_quote(&mut self) {
        self.mid_e9 = 0;
        self.spread_bps = 0;
        self.skew_bps = 0;
        self.max_size_base = 0;
        self.quote_slot = 0;
    }

    /// Vault PDA seeds: `(owner, base_mint, quote_mint)`.
    ///
    /// The pair is part of the address, so one owner deploys several vaults from
    /// one project (FR-004a), and no shared pool arises between pairs (FR-004).
    /// The mint order is part of the address: the reversed pair gives a **different**
    /// vault, not the same one with a mirrored quote.
    pub fn seeds<'a>(
        owner: &'a Pubkey,
        base_mint: &'a Pubkey,
        quote_mint: &'a Pubkey,
    ) -> [&'a [u8]; 4] {
        [
            VAULT_SEED,
            owner.as_ref(),
            base_mint.as_ref(),
            quote_mint.as_ref(),
        ]
    }

    /// Vault address and its bump.
    pub fn pda(owner: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&Self::seeds(owner, base_mint, quote_mint), &crate::ID)
    }

    /// The current quote in the form [`propamm_quote`] understands.
    ///
    /// This is the **only** bridge between on-chain state and the math, and at the
    /// same time the one place where swapped fields would slip past every math test:
    /// `spread_bps` and `max_skew_bps` share a type, so the compiler stays silent.
    /// That is why the field correspondence is pinned by a separate test
    /// [`tests::quote_params_maps_every_field_to_its_own_place`] on sentinels where
    /// every number differs.
    ///
    /// `Ok` from [`propamm_quote::compute_swap`] on these parameters **does not mean**
    /// the swap is allowed: `halted`, the `pricing_authority` signature, token
    /// account ownership and Token-2022 extensions are invisible to the crate (T017).
    pub fn quote_params(&self) -> QuoteParams {
        QuoteParams {
            mid_e9: self.mid_e9,
            spread_bps: self.spread_bps,
            skew_bps: self.skew_bps,
            max_size_base: self.max_size_base,
            quote_slot: self.quote_slot,
            max_quote_age_slots: self.max_quote_age_slots,
            max_skew_bps: self.max_skew_bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::Discriminator;

    /// Layout from `PLAN.md` → "Data model", counted byte by byte:
    /// 7 keys × 32 + 16 + 2 × 8 + 4 + 3 × 2 + 2 × 1.
    const EXPECTED_BODY: usize = 224 + 16 + 16 + 4 + 6 + 2;

    fn sentinel() -> Vault {
        Vault {
            owner: Pubkey::new_from_array([1; 32]),
            pricing_authority: Pubkey::new_from_array([2; 32]),
            halt_authority: Pubkey::new_from_array([3; 32]),
            base_mint: Pubkey::new_from_array([4; 32]),
            quote_mint: Pubkey::new_from_array([5; 32]),
            base_vault: Pubkey::new_from_array([6; 32]),
            quote_vault: Pubkey::new_from_array([7; 32]),
            mid_e9: 150_000_000,
            max_size_base: 11,
            quote_slot: 12,
            max_quote_age_slots: 13,
            spread_bps: 14,
            skew_bps: -15,
            max_skew_bps: 16,
            halted: true,
            bump: 254,
        }
    }

    #[test]
    fn layout_stays_within_its_budget() {
        assert_eq!(Vault::INIT_SPACE, EXPECTED_BODY, "layout changed");
        assert_eq!(EXPECTED_BODY, 268);
        assert_eq!(Vault::DISCRIMINATOR.len(), 8);
        assert_eq!(Vault::SPACE, 276);
    }

    /// `INIT_SPACE` is a promise of the macro, not a measurement. Here it is checked
    /// against what Borsh really writes: a mismatch would mean an account the state
    /// does not fit into, and it would surface right at deployment.
    #[test]
    fn serialized_account_is_exactly_the_reserved_space() {
        let mut buf = Vec::new();
        sentinel().try_serialize(&mut buf).unwrap();
        assert_eq!(buf.len(), Vault::SPACE);

        let back = Vault::try_deserialize(&mut buf.as_slice()).unwrap();
        assert_eq!(back.owner, sentinel().owner);
        assert_eq!(back.mid_e9, sentinel().mid_e9);
        assert_eq!(back.skew_bps, sentinel().skew_bps);
        assert!(back.halted);
        assert_eq!(back.bump, 254);
    }

    /// Clearing must remove **all** quote fields, not the ones someone remembered.
    #[test]
    fn clearing_a_quote_leaves_no_quote_field_behind() {
        let mut v = sentinel();
        v.clear_quote();

        assert_eq!(v.mid_e9, 0);
        assert_eq!(v.spread_bps, 0);
        assert_eq!(v.skew_bps, 0);
        assert_eq!(v.max_size_base, 0);
        assert_eq!(v.quote_slot, 0);

        // Risk limits and authorities are untouched by clearing.
        assert_eq!(v.max_quote_age_slots, 13);
        assert_eq!(v.max_skew_bps, 16);
        assert_eq!(v.pricing_authority, sentinel().pricing_authority);
    }

    #[test]
    fn quote_params_maps_every_field_to_its_own_place() {
        let v = sentinel();
        let p = v.quote_params();

        assert_eq!(p.mid_e9, 150_000_000);
        assert_eq!(p.spread_bps, 14);
        assert_eq!(p.skew_bps, -15);
        assert_eq!(p.max_size_base, 11);
        assert_eq!(p.quote_slot, 12);
        assert_eq!(p.max_quote_age_slots, 13);
        assert_eq!(p.max_skew_bps, 16);
    }

    /// The pair is part of the address, and so is its direction. If `base_mint` and
    /// `quote_mint` entered the seed symmetrically, a second deployment of "the same
    /// pair reversed" would silently target someone else's capital.
    #[test]
    fn pair_direction_is_part_of_the_address() {
        let owner = Pubkey::new_from_array([1; 32]);
        let base = Pubkey::new_from_array([4; 32]);
        let quote = Pubkey::new_from_array([5; 32]);

        let (direct, bump) = Vault::pda(&owner, &base, &quote);
        let (reversed, _) = Vault::pda(&owner, &quote, &base);
        let (other_owner, _) = Vault::pda(&Pubkey::new_from_array([9; 32]), &base, &quote);

        assert_ne!(direct, reversed);
        assert_ne!(direct, other_owner);
        assert_eq!(Vault::pda(&owner, &base, &quote), (direct, bump));
    }
}
