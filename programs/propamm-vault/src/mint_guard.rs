//! FR-005: screening out assets where the amount received may differ from the amount sent.
//!
//! # Why an allow list, not a deny list
//!
//! A deny list errs towards "let it through": an extension Token-2022 adds
//! after us passes silently — and diverges from the quote on a live asset,
//! i.e. exactly where a mistake costs the most. An allow list errs towards
//! "refuse": a safe extension released tomorrow will require a code change.
//! The second mistake costs one commit, the first costs SC-006.
//!
//! # What exactly is allowed
//!
//! Only metadata and grouping — what takes no part in a transfer at all.
//! Everything else is refused **at deployment** (not at swap time) with the
//! extension name in the log and a separate error code for the four most common causes.
//!
//! Deliberately **not** on the list, even though they do not change the transfer amount:
//! `InterestBearingConfig` and `ScaledUiAmount` change the displayed quantity
//! relative to the raw one, while all accounting and `mid_e9` live in raw units —
//! the console would show a P&L drifting from the market without a single swap;
//! `MintCloseAuthority` allows closing a mint of a pair that is fixed forever.
//! Both can be admitted later, deliberately and with tests.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::spl_token_2022::{
    extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::Mint as SplMint,
};

use crate::errors::VaultError;

/// Side of the pair — needed only so the log shows which mint got in the way.
/// Without it the message "mint carries TransferHook" does not say which of
/// the two to rework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MintRole {
    Base,
    Quote,
}

impl MintRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Quote => "quote",
        }
    }
}

/// Whether the extension stays out of the transfer.
///
/// `Uninitialized` is deliberately absent: it cannot be in an account's
/// extension list, and if it ended up there, that is not a state in which
/// capital should be deployed.
const fn is_transfer_faithful(ext: ExtensionType) -> bool {
    matches!(
        ext,
        ExtensionType::MetadataPointer
            | ExtensionType::TokenMetadata
            | ExtensionType::GroupPointer
            | ExtensionType::TokenGroup
            | ExtensionType::GroupMemberPointer
            | ExtensionType::TokenGroupMember
    )
}

/// The first extension the mint fails on, or `None`.
///
/// Returns the **first** one, not a list: FR-005 asks to explain what got in
/// the way, not to enumerate everything. The order is the one the mint stores them in.
#[must_use]
pub fn first_forbidden(found: &[ExtensionType]) -> Option<ExtensionType> {
    found.iter().copied().find(|e| !is_transfer_faithful(*e))
}

/// Error code for a specific extension.
///
/// The four most common causes have their own codes so that the CLI and the
/// console can explain them without parsing log text; the rest go under a
/// shared code, and then the extension name survives only in the log.
#[must_use]
pub fn extension_error(ext: ExtensionType) -> VaultError {
    match ext {
        ExtensionType::TransferFeeConfig | ExtensionType::TransferFeeAmount => {
            VaultError::MintHasTransferFee
        }
        ExtensionType::TransferHook | ExtensionType::TransferHookAccount => {
            VaultError::MintHasTransferHook
        }
        ExtensionType::PermanentDelegate => VaultError::MintHasPermanentDelegate,
        ExtensionType::NonTransferable | ExtensionType::NonTransferableAccount => {
            VaultError::MintIsNonTransferable
        }
        ExtensionType::DefaultAccountState => VaultError::MintDefaultsToFrozen,
        ExtensionType::Pausable | ExtensionType::PausableAccount => VaultError::MintIsPausable,
        _ => VaultError::MintExtensionNotAllowed,
    }
}

/// Check a mint before fixing it in the pair forever (FR-004).
///
/// Classic SPL passes without reading the data: it has no extensions by
/// construction, and an extra account parse is CU at deployment and code that
/// catches nothing.
pub fn ensure_transfer_is_faithful(
    mint: &AccountInfo<'_>,
    token_program: &Pubkey,
    role: MintRole,
) -> Result<()> {
    if *token_program == anchor_spl::token::ID {
        return Ok(());
    }

    let data = mint.try_borrow_data()?;
    let state = StateWithExtensions::<SplMint>::unpack(&data)?;
    let found = state.get_extension_types()?;

    if let Some(ext) = first_forbidden(&found) {
        msg!(
            "{} mint carries {:?}: rejected at deployment (FR-005)",
            role.as_str(),
            ext
        );
        return Err(extension_error(ext).into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything we are obliged not to let through. The list is deliberately
    /// written by hand rather than derived from `is_transfer_faithful`: otherwise
    /// the test would confirm itself — "forbidden is what is forbidden" — and stay
    /// silent if the allow list one day grew by accident.
    const MUST_BE_REJECTED: &[ExtensionType] = &[
        ExtensionType::TransferFeeConfig,
        ExtensionType::TransferFeeAmount,
        ExtensionType::TransferHook,
        ExtensionType::TransferHookAccount,
        ExtensionType::PermanentDelegate,
        ExtensionType::NonTransferable,
        ExtensionType::NonTransferableAccount,
        ExtensionType::DefaultAccountState,
        ExtensionType::Pausable,
        ExtensionType::PausableAccount,
        ExtensionType::ConfidentialTransferMint,
        ExtensionType::ConfidentialTransferAccount,
        ExtensionType::ConfidentialTransferFeeConfig,
        ExtensionType::ConfidentialMintBurn,
        ExtensionType::InterestBearingConfig,
        ExtensionType::ScaledUiAmount,
        ExtensionType::MintCloseAuthority,
        ExtensionType::Uninitialized,
    ];

    #[test]
    fn a_mint_without_extensions_passes() {
        assert!(first_forbidden(&[]).is_none());
    }

    #[test]
    fn metadata_and_grouping_pass() {
        let ok = [
            ExtensionType::MetadataPointer,
            ExtensionType::TokenMetadata,
            ExtensionType::GroupPointer,
            ExtensionType::TokenGroup,
            ExtensionType::GroupMemberPointer,
            ExtensionType::TokenGroupMember,
        ];
        assert!(first_forbidden(&ok).is_none());
    }

    #[test]
    fn everything_that_can_touch_a_transfer_is_rejected() {
        for ext in MUST_BE_REJECTED {
            assert_eq!(
                first_forbidden(&[*ext]),
                Some(*ext),
                "extension {ext:?} passed the allow list"
            );
        }
    }

    /// An allowed extension in front must not hide a forbidden one behind it.
    #[test]
    fn an_allowed_extension_does_not_shield_the_rest() {
        let mixed = [
            ExtensionType::MetadataPointer,
            ExtensionType::TokenMetadata,
            ExtensionType::TransferHook,
        ];
        assert_eq!(first_forbidden(&mixed), Some(ExtensionType::TransferHook));
    }

    /// The most common causes have their own code: that is what the CLI explains a refusal by.
    #[test]
    fn the_common_causes_map_to_their_own_error_codes() {
        let cases = [
            (ExtensionType::TransferFeeConfig, "MintHasTransferFee"),
            (ExtensionType::TransferHook, "MintHasTransferHook"),
            (ExtensionType::PermanentDelegate, "MintHasPermanentDelegate"),
            (ExtensionType::NonTransferable, "MintIsNonTransferable"),
            (ExtensionType::DefaultAccountState, "MintDefaultsToFrozen"),
            (ExtensionType::Pausable, "MintIsPausable"),
            (
                ExtensionType::InterestBearingConfig,
                "MintExtensionNotAllowed",
            ),
        ];
        for (ext, expected) in cases {
            assert_eq!(format!("{:?}", extension_error(ext)), expected);
        }
    }
}
