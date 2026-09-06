//! Mint and token account fixtures for Mollusk.
//!
//! # Why the types come through `anchor_spl` rather than the Mollusk crates
//!
//! `mollusk-svm-programs-token` has builders of its own, but they work with
//! `spl-token-interface` and the fourth-generation `Pubkey`, i.e. a different
//! `Pubkey` than the one the program checks `has_one` and seeds with. Here
//! everything is built with the same types the program itself sees
//! (`anchor_spl::token_2022::spl_token_2022`), and only the finished bytes are
//! moved into `solana_account::Account`. A mint assembled with the wrong type
//! would pass `Pack` and diverge from what `mint_guard` reads.
//!
//! # Classic SPL and Token-2022 in one function
//!
//! The base layout is the same: 82 bytes of mint and 165 bytes of account. What
//! tells them apart is the **account owner** — which is exactly why the owner is
//! a separate argument here, not a constant: a pair may be mixed (T013), and the
//! mixed-pair test has to be built by the same code as the homogeneous one.

use anchor_lang::prelude::Pubkey as AnchorKey;
use anchor_spl::token_2022::spl_token_2022::{
    extension::{
        transfer_fee::TransferFeeConfig, BaseStateWithExtensionsMut, ExtensionType,
        StateWithExtensionsMut,
    },
    state::{Account as TokenAccountState, AccountState, Mint as MintState},
};
use solana_account::Account;
use solana_program_option::COption;
use solana_program_pack::Pack;

use crate::keys::svm;

/// The rent at which Mollusk considers an account exempt from it.
///
/// Taken from the same `Rent::default()` as in the Mollusk sysvar: an
/// underpaid account would give a system program error on `init` rather than
/// the one the test checks.
fn rent_exempt(data: Vec<u8>, owner: &AnchorKey) -> Account {
    Account {
        lamports: rent_for(data.len()),
        data,
        owner: svm(owner),
        executable: false,
        rent_epoch: 0,
    }
}

fn rent_for(space: usize) -> u64 {
    // Solana's rent formula: (128 + bytes) × rate × 2 years. The numbers here are
    // the same as in `Rent::default()`; duplicated because pulling `solana-rent`
    // into the tests for one constant costs more than a line with an explanation.
    const LAMPORTS_PER_BYTE_YEAR: u64 = 3_480;
    const EXEMPTION_YEARS: u64 = 2;
    (128 + space as u64) * LAMPORTS_PER_BYTE_YEAR * EXEMPTION_YEARS
}

/// A mint without extensions — valid for both classic SPL and Token-2022.
#[must_use]
pub fn mint(token_program: &AnchorKey, decimals: u8) -> Account {
    let mut data = vec![0u8; MintState::LEN];
    MintState::pack(
        MintState {
            mint_authority: COption::Some(svm(&crate::keys::named(200))),
            supply: 1_000_000_000_000_000,
            decimals,
            is_initialized: true,
            freeze_authority: COption::None,
        },
        &mut data,
    )
    .expect("the mint does not pack");
    rent_exempt(data, token_program)
}

/// A Token-2022 mint with an extension FR-005 has to refuse.
///
/// This is the one place where the mint check goes through the **real**
/// `StateWithExtensions::unpack`: the `mint_guard` unit tests work with a list
/// of `ExtensionType`, i.e. after the bytes are parsed, and do not cover the parsing itself.
///
/// # Panics
///
/// If the extension does not initialize — then the fixture is not what it calls
/// itself, and a test on it proves nothing.
#[must_use]
pub fn mint_with_transfer_fee(token_program: &AnchorKey, decimals: u8) -> Account {
    let space =
        ExtensionType::try_calculate_account_len::<MintState>(&[ExtensionType::TransferFeeConfig])
            .expect("the length of a mint with an extension does not compute");

    let mut data = vec![0u8; space];
    {
        let mut state = StateWithExtensionsMut::<MintState>::unpack_uninitialized(&mut data)
            .expect("an empty mint does not unpack");
        state
            .init_extension::<TransferFeeConfig>(true)
            .expect("TransferFeeConfig does not initialize");
        state.base = MintState {
            mint_authority: COption::Some(svm(&crate::keys::named(200))),
            supply: 1_000_000_000_000_000,
            decimals,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        state.pack_base();
        state
            .init_account_type()
            .expect("the account type cannot be set");
    }
    rent_exempt(data, token_program)
}

/// A token account with a given balance.
#[must_use]
pub fn token_account(
    token_program: &AnchorKey,
    mint: &AnchorKey,
    owner: &AnchorKey,
    amount: u64,
) -> Account {
    let mut data = vec![0u8; TokenAccountState::LEN];
    TokenAccountState::pack(
        TokenAccountState {
            mint: svm(mint),
            owner: svm(owner),
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        },
        &mut data,
    )
    .expect("the token account does not pack");
    rent_exempt(data, token_program)
}

/// A token account's balance from the account bytes.
///
/// Via `StateWithExtensions`, not `Pack::unpack`: an ATA created by Token-2022
/// carries `ImmutableOwner` and is longer than 165 bytes, so a plain `unpack`
/// would fail on it.
///
/// # Panics
///
/// If the account is not a token account.
#[must_use]
pub fn balance_of(account: &Account) -> u64 {
    use anchor_spl::token_2022::spl_token_2022::extension::StateWithExtensions;

    StateWithExtensions::<TokenAccountState>::unpack(&account.data)
        .expect("the account is not a token account")
        .base
        .amount
}

/// The wallet that pays for the created accounts.
#[must_use]
pub fn wallet(lamports: u64) -> Account {
    Account {
        lamports,
        ..Account::default()
    }
}
