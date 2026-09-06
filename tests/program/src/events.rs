//! Reading the T018 events from the program log.
//!
//! # Why this is separate from the golden vectors
//!
//! The golden vectors (T019) prove that the event **layout** is the same in Rust
//! and in TS. They say nothing about whether the event reaches the log at all:
//! `emit!` could sit on a branch that never executes, write the wrong type or
//! vanish on rollback. Here the event is taken from where the collector (T042)
//! will take it — the `Program data:` line in the transaction log.
//!
//! # Why attribution is by discriminator, not by address
//!
//! The `Program data:` line carries no program address. Eight bytes of
//! discriminator are not a signature: anyone can write the same bytes to the
//! log. For a test with one program of ours in the frame that is enough; the
//! collector on a live chain will need the `invoke`/`success` stack — which is
//! exactly why this is written down here rather than forgotten until T042.

use anchor_lang::{AnchorDeserialize, Discriminator};
use base64::Engine as _;

const DATA_PREFIX: &str = "Program data: ";

/// All events of type `E` from the log, in order of appearance.
///
/// Lines that are not `Program data:` and events of other types are skipped
/// silently — several of them in one transaction are legitimate (`withdraw` writes `CapitalMoved` and
/// `QuoteCleared`).
///
/// # Panics
///
/// If a line has our discriminator but does not deserialize: that means the
/// event layout and the type layout diverged, and staying silent is not an option.
#[must_use]
pub fn decode_all<E>(logs: &[String]) -> Vec<E>
where
    E: AnchorDeserialize + Discriminator,
{
    logs.iter()
        .filter_map(|line| line.strip_prefix(DATA_PREFIX))
        .filter_map(|payload| {
            base64::engine::general_purpose::STANDARD
                .decode(payload.trim())
                .ok()
        })
        .filter_map(|bytes| {
            let (tag, body) = bytes.split_at_checked(8)?;
            if tag != E::DISCRIMINATOR {
                return None;
            }
            Some(E::try_from_slice(body).unwrap_or_else(|err| {
                panic!("an event with our discriminator does not parse: {err}")
            }))
        })
        .collect()
}

/// Exactly one event of type `E`.
///
/// # Panics
///
/// If there are zero or more than one. Both cases are an error: "none" means
/// `emit!` did not fire, "two" means the event is written twice and the
/// history counts one fact as two.
#[must_use]
pub fn exactly_one<E>(logs: &[String]) -> E
where
    E: AnchorDeserialize + Discriminator,
{
    let mut found = decode_all::<E>(logs);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one event, found {}\nlog:\n{}",
        found.len(),
        logs.join("\n")
    );
    found.remove(0)
}

/// How many events of type `E` are in the log.
#[must_use]
pub fn count<E>(logs: &[String]) -> usize
where
    E: AnchorDeserialize + Discriminator,
{
    decode_all::<E>(logs).len()
}
