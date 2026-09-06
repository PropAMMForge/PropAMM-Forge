//! Harness for the instruction tests on Mollusk (T020) and the CU benchmark (T021).
//!
//! # What is proven here that was not proven before
//!
//! Before this task the on-chain part was checked by everything that can be
//! checked **without a chain**: account layout, argument domains, error mapping,
//! instruction bytes. Here the `.so` itself runs for the first time: Anchor
//! assembles `try_accounts`, the token program makes the transfer, `Clock` comes
//! from the sysvar, and `emit!` writes to the log. Everything these layers could
//! break silently is caught only from here.
//!
//! # Two generations of solana crates in one test
//!
//! `anchor-lang` 1.2.0 links against `solana-pubkey` 3.0.0, Mollusk 0.15.1 against
//! 4.2.1. Both are in `Cargo.lock`, and the two `Pubkey`s are **not compatible**
//! with each other: the compiler sees different types with the same name. So all
//! code crosses between them explicitly, via [`keys::svm`] and [`keys::anchor`] —
//! the bytes are the same in both, there is nothing to diverge. `solana-instruction`
//! is meanwhile single (3.4.1) and links against pubkey 4.2.1 itself, so
//! `Instruction` and `AccountMeta` are types from the Mollusk side.
//!
//! The on-chain build is unaffected: fourth-generation crates are test
//! dependencies, `programs/propamm-vault` has none of them.
//!
//! # Where the program comes from
//!
//! From `target/deploy/propamm_vault.so`, i.e. the very artifact that would go to
//! the network — not from the host build of the crate. The difference is not
//! theoretical: `overflow-checks`, stack size and the behaviour of `msg!` under
//! BPF are their own. If the file is missing, the test says `wsl-build.sh build`
//! is needed first rather than failing with an obscure `No such file`.

pub mod events;
pub mod fixtures;
pub mod ix;
pub mod keys;
pub mod rng;
pub mod world;

pub use keys::{anchor, svm};
pub use world::{Outcome, World};

/// Path to the BPF artifact relative to this crate.
///
/// `CARGO_MANIFEST_DIR` expands at compile time, so no absolute path remains
/// in the source text — a condition of the trace sweep (T025).
pub const PROGRAM_ELF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/deploy/propamm_vault.so"
);

/// Read the `.so` or explain what is missing.
///
/// # Panics
///
/// If the artifact is missing — there is no way to build it from Windows, and
/// the message has to name the very command that does it.
#[must_use]
pub fn program_elf() -> Vec<u8> {
    std::fs::read(PROGRAM_ELF).unwrap_or_else(|err| {
        panic!(
            "cannot read {PROGRAM_ELF}: {err}\n\
             build the program first: wsl-build.sh build"
        )
    })
}
