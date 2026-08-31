//! Program error codes.
//!
//! The messages are deliberately English: they are read not only by the owner
//! but by the router, the explorer and third-party clients — the same as
//! `Display` in `propamm_quote::QuoteError`. Comments follow the same rule.
//!
//! The mapping of `QuoteError` onto these codes is done in T017 with the swap guards.

use anchor_lang::prelude::*;

// `PartialEq` here is not cosmetic: without it, pure checks such as
// `resolve_side` would have to be compared via `matches!`, i.e. with no
// difference between "the wrong error code" and "the right one".
#[error_code]
#[derive(PartialEq, Eq)]
pub enum VaultError {
    #[msg("base and quote mints must differ")]
    IdenticalMints,

    #[msg("mint is not owned by the token program passed for its side")]
    MintProgramMismatch,

    #[msg("authority cannot be the default pubkey")]
    InvalidAuthority,

    #[msg("risk limits are outside their domain")]
    InvalidRiskLimits,

    #[msg("only the vault owner may move capital")]
    OwnerOnly,

    #[msg("only the pricing authority may quote")]
    PricingAuthorityOnly,

    #[msg("quote parameters are outside the domain the math accepts")]
    InvalidQuote,

    #[msg("vault is halted")]
    VaultHalted,

    #[msg("mint is not part of this vault's pair")]
    UnknownMint,

    #[msg("treasury account does not belong to this vault side")]
    TreasuryAccountMismatch,

    #[msg("amount must be greater than zero")]
    ZeroAmount,

    #[msg("vault does not hold that much of the asset")]
    InsufficientVaultBalance,

    // --- Swap guards: a mirror of `propamm_quote::QuoteError` ---
    //
    // Every crate variant has its own code here so that the CLI, the console and
    // the adapter can tell "the venue is not quoting" from "the order is too large"
    // without parsing text. The mapping is `From<QuoteError>` below — a single
    // `match` the compiler forces to be updated as soon as the crate gains a variant.
    #[msg("quote is not set")]
    QuoteNotSet,

    #[msg("quote is older than the freshness limit")]
    QuoteStale,

    #[msg("base leg exceeds the maximum quoted size")]
    SizeExceeded,

    #[msg("result is worse than the declared limit")]
    SlippageExceeded,

    #[msg("swap would push inventory past the hard bound")]
    InventoryBound,

    #[msg("vault cannot pay out that amount")]
    InsufficientLiquidity,

    #[msg("amount rounds to zero")]
    AmountTooSmall,

    #[msg("intermediate value overflowed")]
    MathOverflow,

    #[msg("account does not match the one recorded in the vault")]
    AccountMismatch,

    // --- FR-005: extensions able to make received ≠ sent ---
    //
    // Separate codes rather than one shared code, so that the CLI and the console
    // can explain the cause without parsing log text. The name of the specific
    // extension still goes to the log before the refusal: the allow list has an
    // "unknown extension" branch, and without the name it would be useless for diagnosis.
    #[msg("mint has a transfer fee: received amount would differ from sent")]
    MintHasTransferFee,

    #[msg("mint has a transfer hook: transfer behaviour is not ours to predict")]
    MintHasTransferHook,

    #[msg("mint has a permanent delegate: vault funds could be moved without us")]
    MintHasPermanentDelegate,

    #[msg("mint is non-transferable")]
    MintIsNonTransferable,

    #[msg("mint freezes new accounts by default")]
    MintDefaultsToFrozen,

    #[msg("mint is pausable: transfers can be stopped by a third party")]
    MintIsPausable,

    #[msg("mint carries an extension that is not on the allow list")]
    MintExtensionNotAllowed,
}

/// Mapping of math errors onto program codes.
///
/// Written as a `match` without a `_` arm: when `propamm_quote` gains a new
/// variant, the compiler stops right here — instead of silently handing the
/// swap "some error".
impl From<propamm_quote::QuoteError> for VaultError {
    fn from(err: propamm_quote::QuoteError) -> Self {
        use propamm_quote::QuoteError as Q;
        match err {
            Q::QuoteNotSet => Self::QuoteNotSet,
            Q::InvalidParams => Self::InvalidQuote,
            Q::QuoteStale => Self::QuoteStale,
            Q::SizeExceeded => Self::SizeExceeded,
            Q::SlippageExceeded => Self::SlippageExceeded,
            Q::InventoryBound => Self::InventoryBound,
            Q::InsufficientLiquidity => Self::InsufficientLiquidity,
            Q::AmountTooSmall => Self::AmountTooSmall,
            Q::Overflow => Self::MathOverflow,
        }
    }
}
