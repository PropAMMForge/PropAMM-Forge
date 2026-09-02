//! On-chain events (FR-020) — the only source the collector (T042) rebuilds the
//! vault history from.
//!
//! # A field that is not here does not exist for the console
//!
//! The collector does not read account state after the fact: `Vault` keeps only
//! the **current** quote, and the previous one vanishes forever after `update_quote`.
//! So everything the console shows over time — P&L (FR-021), CU (FR-022), inventory
//! and quote age (FR-023) — has to be in the event at the moment it is written.
//! A field can be added later, but the stretch of history before the addition
//! stays without it forever.
//!
//! # What is deliberately **not** in the events
//!
//! The transaction signature, `fee_lamports` and `cu_consumed` are the transaction
//! envelope, not our data; the collector fetches `getTransaction` anyway for the
//! fee and the CU (PLAN → "Data model", tables `quote_updates` and `swaps`). The
//! signer key is in the envelope too. Duplicating them in the logs would mean
//! paying `sol_log_data` bytes for what has already arrived.
//!
//! `slot` is the exception, and a deliberate one: it **is** in the envelope, but it
//! stays in every event so that a history row can be written from the event alone,
//! without a mandatory join to the transaction. Eight bytes here are cheaper than
//! a collector whose record depends on two sources.
//!
//! # Why inventory is in the event rather than a separate read
//!
//! `Swapped` carries the treasury holdings **after** the swap, and that fills
//! `inventory_snapshots` straight from the swap stream. The alternative — two
//! `getTokenAccountBalance` per swap — costs more than RPC quota (PLAN →
//! "RPC quota"): a read always lags, and under an active stream it returns the
//! holdings of a **different** slot than the swap that changed them.
//!
//! The numbers come from arithmetic, not from a re-read balance: [`crate::mint_guard`]
//! refuses `TransferFeeConfig`, so "sent" equals "received" exactly, and
//! `before ± amount` is not an approximation. If a transfer fee ever became allowed,
//! these fields would be the first to lie.
//!
//! # On the CU budget
//!
//! `emit!` is `sol_log_data`: a syscall plus the event bytes. Against SC-002
//! (60 000 CU per swap) that is on the order of hundreds, but the measurement is
//! T021, not an estimate here.

use anchor_lang::prelude::*;

use crate::instructions::swap::SwapSide;
use crate::instructions::treasury::TreasurySide;

/// A quote was posted (FR-006, FR-020).
///
/// `slot` here is the same slot that was written to `Vault::quote_slot`, i.e.
/// the start of the quote's age (FR-007).
///
/// The two side prices are not in the event: bid and ask derive deterministically
/// from `mid_e9`, `spread_bps` and `skew_bps` via `propamm_quote::side_price_e9` —
/// the same crate computes them for the program, the adapter and the console.
/// Writing them into the event would create a fourth source of the same numbers,
/// and divergence of sources is exactly what SC-006 forbids.
#[event]
pub struct QuoteUpdated {
    pub vault: Pubkey,
    pub slot: u64,
    pub mid_e9: u128,
    pub spread_bps: u16,
    pub skew_bps: i16,
    pub max_size_base: u64,
}

/// Why the quote disappeared.
///
/// It is cleared from three places, and they are not equal for the console: an
/// explicit clear is normal engine operation when the feed is silent (FR-014), while
/// the other two follow an owner action after which the engine will not restore the
/// price on its own until it sees the new state. Without this field all three would look alike.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteClearReason {
    /// `clear_quote` from `pricing_authority` — the feed went silent (FR-014).
    Explicit,
    /// Side effect of `withdraw`: the asset behind the price may be gone.
    CapitalWithdrawn,
    /// Side effect of `set_pricing_authority`: the price source is no longer trusted.
    PricingAuthorityChanged,
}

/// The quote was cleared (FR-014, FR-020).
///
/// Written **only** when there really was a quote: all three clearing sites call
/// [`crate::state::Vault::clear_quote`] unconditionally, and without this condition
/// the history would gain clearings of a price that never existed.
#[event]
pub struct QuoteCleared {
    pub vault: Pubkey,
    pub slot: u64,
    pub reason: QuoteClearReason,
}

/// A swap was executed (FR-020).
///
/// `slot` is when the swap happened, `quote_slot` is when the quote it was priced
/// by was posted. The difference between them is the quote's age at execution
/// time: what FR-007 is measured by after the fact, not by intent.
#[event]
pub struct Swapped {
    pub vault: Pubkey,
    pub slot: u64,
    /// Direction as named from the trader's side — the same type as in the `swap`
    /// arguments, not a copy of it: two copies would diverge in variant order, and
    /// Borsh writes a variant as its index, so a flipped direction in the accounting
    /// would not look like an error.
    pub side: SwapSide,
    pub amount_in: u64,
    pub amount_out: u64,
    /// The side price the swap was computed at, in the scale of
    /// [`propamm_quote::PRICE_SCALE`].
    pub price_e9: u128,
    /// Slot of the quote the swap was executed against.
    pub quote_slot: u64,
    pub base_amount_after: u64,
    pub quote_amount_after: u64,
}

/// Direction of the owner's capital movement.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapitalFlow {
    Deposit,
    Withdraw,
}

/// The owner's capital moved (FR-003).
///
/// Formally FR-020 speaks of swaps and quotes, but without this event the P&L
/// (FR-021) is computed wrongly and **silently**: the collector sees inventory
/// shrink and has nothing to tell a swap loss from a withdrawal. The figure on the
/// console looks plausible all the while — which is exactly why the event is here
/// and not in a later task.
///
/// The other treasury is not mentioned in the event because `deposit` / `withdraw`
/// move exactly one side; `side` says which.
#[event]
pub struct CapitalMoved {
    pub vault: Pubkey,
    pub slot: u64,
    pub flow: CapitalFlow,
    pub side: TreasurySide,
    pub amount: u64,
    /// Balance of **this** treasury after the transfer.
    pub treasury_amount_after: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::Discriminator;

    fn key(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    /// Event bytes on the wire. This borsh version has no `try_to_vec`, and the rest
    /// of the crate already serializes via `serialize` — one form for the whole crate.
    fn wire<T: AnchorSerialize>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.serialize(&mut bytes).unwrap();
        bytes
    }

    /// The sizes are pinned on purpose: an event is bytes in `sol_log_data`, i.e. CU
    /// in the swap, where the SC-002 budget is tightest. A field added without thought
    /// must fail here, not show up in T021 as a budget drop without a cause.
    #[test]
    fn events_stay_the_size_they_were_designed_to_be() {
        let quote_updated = QuoteUpdated {
            vault: key(1),
            slot: 100,
            mid_e9: 150_000_000,
            spread_bps: 25,
            skew_bps: -30,
            max_size_base: 1_000_000,
        };
        assert_eq!(wire(&quote_updated).len(), 32 + 8 + 16 + 2 + 2 + 8);

        let quote_cleared = QuoteCleared {
            vault: key(1),
            slot: 100,
            reason: QuoteClearReason::CapitalWithdrawn,
        };
        assert_eq!(wire(&quote_cleared).len(), 32 + 8 + 1);

        let swapped = Swapped {
            vault: key(1),
            slot: 100,
            side: SwapSide::QuoteToBase,
            amount_in: 5,
            amount_out: 6,
            price_e9: 150_000_000,
            quote_slot: 98,
            base_amount_after: 7,
            quote_amount_after: 8,
        };
        assert_eq!(wire(&swapped).len(), 32 + 8 + 1 + 8 + 8 + 16 + 8 + 8 + 8);

        let capital_moved = CapitalMoved {
            vault: key(1),
            slot: 100,
            flow: CapitalFlow::Withdraw,
            side: TreasurySide::Quote,
            amount: 9,
            treasury_amount_after: 10,
        };
        assert_eq!(wire(&capital_moved).len(), 32 + 8 + 1 + 1 + 8 + 8);
    }

    /// The collector parses the log by discriminator. A collision of two would mean a
    /// swap written into the quotes table, and no type on that path would object.
    #[test]
    fn every_event_has_its_own_discriminator() {
        let discriminators = [
            QuoteUpdated::DISCRIMINATOR,
            QuoteCleared::DISCRIMINATOR,
            Swapped::DISCRIMINATOR,
            CapitalMoved::DISCRIMINATOR,
        ];
        for (i, a) in discriminators.iter().enumerate() {
            assert_eq!(a.len(), 8);
            for b in discriminators.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// A round-trip, not just serialization: the event is read by **foreign** code —
    /// the TypeScript collector (T042) and the SDK (T019). Borsh field order is
    /// positional, so two adjacent `u64`s swapped around would pass the compiler, and
    /// the console would show `amount_in` as `amount_out`.
    #[test]
    fn a_swap_event_survives_a_round_trip_field_by_field() {
        let before = Swapped {
            vault: key(3),
            slot: 12_345,
            side: SwapSide::BaseToQuote,
            amount_in: 1_000_000,
            amount_out: 2_000_000,
            price_e9: 2_000_000_000,
            quote_slot: 12_340,
            base_amount_after: 3_000_000,
            quote_amount_after: 4_000_000,
        };
        let bytes = wire(&before);
        let after = Swapped::deserialize(&mut bytes.as_slice()).unwrap();

        assert_eq!(after.vault, key(3));
        assert_eq!(after.slot, 12_345);
        assert_eq!(after.side, SwapSide::BaseToQuote);
        assert_eq!(after.amount_in, 1_000_000);
        assert_eq!(after.amount_out, 2_000_000);
        assert_eq!(after.price_e9, 2_000_000_000);
        assert_eq!(after.quote_slot, 12_340);
        assert_eq!(after.base_amount_after, 3_000_000);
        assert_eq!(after.quote_amount_after, 4_000_000);
    }

    /// Every clear reason has its own number on the wire. Borsh writes variants by
    /// index, so a reordering in the declaration would silently rename the whole
    /// accumulated history.
    #[test]
    fn clear_reasons_keep_their_wire_numbers() {
        for (reason, tag) in [
            (QuoteClearReason::Explicit, 0u8),
            (QuoteClearReason::CapitalWithdrawn, 1),
            (QuoteClearReason::PricingAuthorityChanged, 2),
        ] {
            assert_eq!(wire(&reason), vec![tag]);
        }
        assert_eq!(wire(&CapitalFlow::Deposit), vec![0u8]);
        assert_eq!(wire(&CapitalFlow::Withdraw), vec![1u8]);
        assert_eq!(wire(&TreasurySide::Base), vec![0u8]);
        assert_eq!(wire(&TreasurySide::Quote), vec![1u8]);
    }
}
