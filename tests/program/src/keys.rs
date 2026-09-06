//! Crossing between the two `Pubkey`s that live in one test.
//!
//! Anchor and Mollusk link against different majors of `solana-pubkey` (3.0.0
//! and 4.2.1), and the compiler treats these types as distinct. Their bytes are
//! the same, so the crossing is `to_bytes()` and back; the one thing not to do
//! is keep somewhere a function that takes "just a Pubkey" without saying whose.

use anchor_lang::prelude::Pubkey as AnchorKey;
use solana_pubkey::Pubkey as SvmKey;

/// Anchor key → Mollusk key.
#[must_use]
pub fn svm(key: &AnchorKey) -> SvmKey {
    SvmKey::new_from_array(key.to_bytes())
}

/// Mollusk key → Anchor key.
#[must_use]
pub fn anchor(key: &SvmKey) -> AnchorKey {
    AnchorKey::new_from_array(key.to_bytes())
}

/// A key from one byte — so an account dump shows who is who.
///
/// `Pubkey::new_unique()` would do the same, but every run differs, and a
/// broken test would have to be read with random addresses in the message.
#[must_use]
pub fn named(tag: u8) -> AnchorKey {
    AnchorKey::new_from_array([tag; 32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_round_trip_keeps_every_byte() {
        let key = named(42);
        assert_eq!(anchor(&svm(&key)), key);
        assert_eq!(svm(&key).to_bytes(), key.to_bytes());
    }
}
