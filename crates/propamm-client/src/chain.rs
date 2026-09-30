//! The vault program as seen from off chain: instruction builders and account
//! decoders, shared by `forge` and the engine.
//!
//! Both go to the network with the same instructions and read the same
//! accounts. One copy of the discriminators, the account order and the byte
//! offsets is the point: a second copy would be checked only against itself.
//!
//! # Token account layouts are read by hand
//!
//! A mint's `decimals` and a token account's `amount` are two offsets in the
//! standard SPL layout, which Token-2022 keeps unchanged in the first 82 and 165
//! bytes respectively. Pulling in the extension parser for two numbers would add
//! yet another set of `ExtensionType` types to the clients — exactly what T013
//! avoided in the program. The offsets are named constants and tested on recorded data.

use std::path::Path;

use anchor_lang::prelude::{Pubkey, *};
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use anyhow::{bail, Context, Result};
use solana_keypair::Keypair;

use crate::rpc::Account;

/// Wrapped SOL. `fund` speaks about it separately: to put SOL into a vault the
/// owner first needs a wSOL account, and the CLI deliberately does not create it
/// for them — wrapping moves their own lamports outside the vault.
pub const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// Offset of `decimals` in the SPL mint layout: 4 bytes of option + 32 of key + 8 of supply.
const MINT_DECIMALS_OFFSET: usize = 44;

/// The shortest mint: beyond this only Token-2022 extensions can follow.
const MINT_BASE_LEN: usize = 82;

/// Token account layout: mint, owner, amount — in exactly that order.
const TOKEN_MINT_OFFSET: usize = 0;
const TOKEN_OWNER_OFFSET: usize = 32;
const TOKEN_AMOUNT_OFFSET: usize = 64;

/// The shortest token account.
const TOKEN_BASE_LEN: usize = 165;

/// A mint to the extent the commands need it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MintInfo {
    pub address: Pubkey,
    pub decimals: u8,
    /// The token program that owns the mint: classic SPL or Token-2022.
    /// A pair may mix both, so it is asked for each side separately.
    pub token_program: Pubkey,
}

/// Read a Solana keypair from a file.
///
/// # Errors
///
/// If the file is missing or is not a keypair.
pub fn read_keypair(path: &Path) -> Result<Keypair> {
    if !path.exists() {
        bail!("no keypair at {}", path.display());
    }
    solana_keypair::read_keypair_file(path)
        .map_err(|err| anyhow::anyhow!("{} is not a Solana keypair: {err}", path.display()))
}

/// Parse a vault account.
///
/// # Errors
///
/// If the discriminator is wrong — i.e. the account at the address is not ours.
pub fn decode_vault(address: &Pubkey, account: &Account) -> Result<propamm_vault::state::Vault> {
    let mut data = account.data.as_slice();
    propamm_vault::state::Vault::try_deserialize(&mut data)
        .with_context(|| format!("account {address} is not a PropAMM program vault"))
}

/// Decimals and token program of a mint.
///
/// # Errors
///
/// If the account owner is not a token program or the data is shorter than the base layout.
pub fn decode_mint(address: &Pubkey, account: &Account) -> Result<MintInfo> {
    if !is_token_program(&account.owner) {
        bail!(
            "{address} belongs to program {} — not a mint of SPL or Token-2022",
            account.owner
        );
    }
    let decimals = *account.data.get(MINT_DECIMALS_OFFSET).with_context(|| {
        format!(
            "account {address} has only {} bytes, while a mint starts at {MINT_BASE_LEN}",
            account.data.len()
        )
    })?;
    Ok(MintInfo {
        address: *address,
        decimals,
        token_program: account.owner,
    })
}

/// A token account: whose mint, whose authority, how much is on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenAccountInfo {
    pub mint: Pubkey,
    /// Authority over the account. The program requires it to be the vault owner
    /// and checks that itself; the CLI reads the field to report a foreign `--from`
    /// before the transaction, not receive `OwnerOnly` after it.
    pub owner: Pubkey,
    pub amount: u64,
}

/// Parse a token account.
///
/// # Errors
///
/// If the account belongs to a non-token program or is shorter than the base layout.
pub fn decode_token_account(address: &Pubkey, account: &Account) -> Result<TokenAccountInfo> {
    if !is_token_program(&account.owner) {
        bail!(
            "{address} is not a token account: it is owned by {}",
            account.owner
        );
    }
    let data = account.data.get(..TOKEN_BASE_LEN).with_context(|| {
        format!(
            "account {address} has only {} bytes, while a token account starts at {TOKEN_BASE_LEN}",
            account.data.len()
        )
    })?;
    let key_at = |offset: usize| {
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&data[offset..offset + 32]);
        Pubkey::new_from_array(bytes)
    };
    let mut amount = [0u8; 8];
    amount.copy_from_slice(&data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8]);
    Ok(TokenAccountInfo {
        mint: key_at(TOKEN_MINT_OFFSET),
        owner: key_at(TOKEN_OWNER_OFFSET),
        amount: u64::from_le_bytes(amount),
    })
}

/// Whether this is one of the two token programs.
#[must_use]
pub fn is_token_program(program: &Pubkey) -> bool {
    *program == anchor_spl::token::ID || *program == anchor_spl::token_2022::ID
}

/// ATA — the same address the `associated_token::` constraint in the program derives.
#[must_use]
pub fn associated_token_address(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address_with_program_id(
        wallet,
        mint,
        token_program,
    )
}

/// The `initialize_vault` instruction.
///
/// # Why the accounts are listed as a named struct
///
/// `accounts::InitializeVault` requires every field, so forgetting an account is
/// impossible — but swapping two `Pubkey`s is possible, and the compiler stays
/// silent. The order and composition of what comes out of here are pinned by the test
/// [`tests::initialize_vault_puts_every_account_where_the_program_expects_it`].
#[must_use]
pub fn initialize_vault_ix(
    program_id: &Pubkey,
    owner: &Pubkey,
    vault: &Pubkey,
    base: &MintInfo,
    quote: &MintInfo,
    args: propamm_vault::instructions::initialize_vault::InitializeVaultArgs,
) -> anchor_lang::solana_program::instruction::Instruction {
    let accounts = propamm_vault::accounts::InitializeVault {
        owner: *owner,
        base_mint: base.address,
        quote_mint: quote.address,
        vault: *vault,
        base_vault: associated_token_address(vault, &base.address, &base.token_program),
        quote_vault: associated_token_address(vault, &quote.address, &quote.token_program),
        base_token_program: base.token_program,
        quote_token_program: quote.token_program,
        associated_token_program: anchor_spl::associated_token::ID,
        system_program: anchor_lang::system_program::ID,
    };
    anchor_lang::solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts: accounts.to_account_metas(None),
        data: propamm_vault::instruction::InitializeVault { args }.data(),
    }
}

/// The `deposit` instruction.
#[must_use]
pub fn deposit_ix(
    program_id: &Pubkey,
    owner: &Pubkey,
    vault: &Pubkey,
    mint: &MintInfo,
    treasury: &Pubkey,
    owner_token_account: &Pubkey,
    amount: u64,
) -> anchor_lang::solana_program::instruction::Instruction {
    let accounts = propamm_vault::accounts::MoveCapital {
        owner: *owner,
        vault: *vault,
        mint: mint.address,
        treasury: *treasury,
        owner_token_account: *owner_token_account,
        token_program: mint.token_program,
    };
    anchor_lang::solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts: accounts.to_account_metas(None),
        data: propamm_vault::instruction::Deposit { amount }.data(),
    }
}

/// The `update_quote` instruction.
#[must_use]
pub fn update_quote_ix(
    program_id: &Pubkey,
    pricing_authority: &Pubkey,
    vault: &Pubkey,
    quote: propamm_vault::instructions::update_quote::QuoteUpdate,
) -> anchor_lang::solana_program::instruction::Instruction {
    let accounts = propamm_vault::accounts::Quoting {
        pricing_authority: *pricing_authority,
        vault: *vault,
    };
    anchor_lang::solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts: accounts.to_account_metas(None),
        data: propamm_vault::instruction::UpdateQuote { quote }.data(),
    }
}

/// The `clear_quote` instruction.
#[must_use]
pub fn clear_quote_ix(
    program_id: &Pubkey,
    pricing_authority: &Pubkey,
    vault: &Pubkey,
) -> anchor_lang::solana_program::instruction::Instruction {
    let accounts = propamm_vault::accounts::Quoting {
        pricing_authority: *pricing_authority,
        vault: *vault,
    };
    anchor_lang::solana_program::instruction::Instruction {
        program_id: *program_id,
        accounts: accounts.to_account_metas(None),
        data: propamm_vault::instruction::ClearQuote {}.data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AnchorDeserialize;
    use propamm_vault::instructions::initialize_vault::InitializeVaultArgs;
    use propamm_vault::instructions::update_quote::QuoteUpdate;

    fn key(tag: u8) -> Pubkey {
        Pubkey::new_from_array([tag; 32])
    }

    fn mint(tag: u8, decimals: u8, token_program: Pubkey) -> MintInfo {
        MintInfo {
            address: key(tag),
            decimals,
            token_program,
        }
    }

    fn mint_account(owner: Pubkey, decimals: u8) -> Account {
        let mut data = vec![0u8; MINT_BASE_LEN];
        data[MINT_DECIMALS_OFFSET] = decimals;
        Account {
            lamports: 1_461_600,
            owner,
            executable: false,
            data,
        }
    }

    fn token_account(program: Pubkey, mint: Pubkey, authority: Pubkey, amount: u64) -> Account {
        let mut data = vec![0u8; TOKEN_BASE_LEN];
        data[TOKEN_MINT_OFFSET..TOKEN_MINT_OFFSET + 32].copy_from_slice(&mint.to_bytes());
        data[TOKEN_OWNER_OFFSET..TOKEN_OWNER_OFFSET + 32].copy_from_slice(&authority.to_bytes());
        data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8].copy_from_slice(&amount.to_le_bytes());
        Account {
            lamports: 2_039_280,
            owner: program,
            executable: false,
            data,
        }
    }

    #[test]
    fn a_mint_yields_its_decimals_and_program() {
        for program in [anchor_spl::token::ID, anchor_spl::token_2022::ID] {
            let info = decode_mint(&key(1), &mint_account(program, 6)).unwrap();
            assert_eq!(info.decimals, 6);
            assert_eq!(info.token_program, program);
        }
    }

    /// A Token-2022 mint with extensions is longer than the base layout, and the
    /// `decimals` offset in it is the same — otherwise a pair with such an asset
    /// would be read from the wrong byte.
    #[test]
    fn a_token_2022_mint_with_extensions_reads_the_same_offset() {
        let mut account = mint_account(anchor_spl::token_2022::ID, 9);
        account.data.extend_from_slice(&[7u8; 120]);
        assert_eq!(decode_mint(&key(1), &account).unwrap().decimals, 9);
    }

    #[test]
    fn an_account_owned_by_someone_else_is_not_a_mint() {
        let stranger = key(200);
        let err = decode_mint(&key(1), &mint_account(stranger, 6)).unwrap_err();
        assert!(format!("{err}").contains("not a mint"), "{err}");
    }

    #[test]
    fn a_truncated_mint_is_refused_instead_of_read_short() {
        let mut account = mint_account(anchor_spl::token::ID, 6);
        account.data.truncate(MINT_DECIMALS_OFFSET);
        assert!(decode_mint(&key(1), &account).is_err());
    }

    /// Three fields from one layout, and all three distinct: swapped offsets would
    /// give a "balance" that is really the tail of someone else's key.
    #[test]
    fn a_token_account_yields_its_mint_owner_and_amount() {
        let account = token_account(anchor_spl::token::ID, key(3), key(1), 5_000_000_000);
        let info = decode_token_account(&key(2), &account).unwrap();
        assert_eq!(info.mint, key(3));
        assert_eq!(info.owner, key(1));
        assert_eq!(info.amount, 5_000_000_000);
    }

    #[test]
    fn a_truncated_token_account_is_refused() {
        let mut account = token_account(anchor_spl::token::ID, key(3), key(1), 1);
        account.data.truncate(TOKEN_AMOUNT_OFFSET + 4);
        assert!(decode_token_account(&key(2), &account).is_err());
    }

    /// The Anchor coder writes a missing field as zero and does not complain, so
    /// the only way to prove the arguments arrived is to decode them back.
    #[test]
    fn initialize_vault_args_survive_the_round_trip() {
        let args = InitializeVaultArgs {
            pricing_authority: key(9),
            halt_authority: key(10),
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
        };
        let ix = initialize_vault_ix(
            &key(100),
            &key(1),
            &key(2),
            &mint(3, 9, anchor_spl::token::ID),
            &mint(4, 6, anchor_spl::token_2022::ID),
            args,
        );
        let discriminator = propamm_vault::instruction::InitializeVault::DISCRIMINATOR;
        assert_eq!(&ix.data[..discriminator.len()], discriminator);
        let decoded =
            InitializeVaultArgs::deserialize(&mut &ix.data[discriminator.len()..]).unwrap();
        assert_eq!(decoded, args);
    }

    #[test]
    fn a_quote_update_survives_the_round_trip() {
        let quote = QuoteUpdate {
            mid_e9: 150_250_000,
            spread_bps: 20,
            skew_bps: -35,
            max_size_base: 10_000_000_000,
        };
        let ix = update_quote_ix(&key(100), &key(9), &key(2), quote);
        let discriminator = propamm_vault::instruction::UpdateQuote::DISCRIMINATOR;
        assert_eq!(&ix.data[..discriminator.len()], discriminator);
        let decoded = QuoteUpdate::deserialize(&mut &ix.data[discriminator.len()..]).unwrap();
        assert_eq!(decoded, quote);
    }

    #[test]
    fn a_deposit_amount_survives_the_round_trip() {
        let ix = deposit_ix(
            &key(100),
            &key(1),
            &key(2),
            &mint(3, 6, anchor_spl::token::ID),
            &key(5),
            &key(6),
            5_000_000_000,
        );
        let discriminator = propamm_vault::instruction::Deposit::DISCRIMINATOR;
        assert_eq!(&ix.data[..discriminator.len()], discriminator);
        let amount = u64::deserialize(&mut &ix.data[discriminator.len()..]).unwrap();
        assert_eq!(amount, 5_000_000_000);
    }

    /// Two consecutive `Pubkey`s in a named struct are indistinguishable to the
    /// compiler, so swapped treasuries would pass the build and fail on chain with
    /// a mint mismatch message. The order is pinned here.
    #[test]
    fn initialize_vault_puts_every_account_where_the_program_expects_it() {
        let (program, owner, vault) = (key(100), key(1), key(2));
        let base = mint(3, 9, anchor_spl::token::ID);
        let quote = mint(4, 6, anchor_spl::token_2022::ID);
        let ix = initialize_vault_ix(
            &program,
            &owner,
            &vault,
            &base,
            &quote,
            InitializeVaultArgs {
                pricing_authority: key(9),
                halt_authority: key(10),
                max_quote_age_slots: 25,
                max_skew_bps: 3_000,
            },
        );
        let expected = [
            ("owner", owner),
            ("base_mint", base.address),
            ("quote_mint", quote.address),
            ("vault", vault),
            (
                "base_vault",
                associated_token_address(&vault, &base.address, &base.token_program),
            ),
            (
                "quote_vault",
                associated_token_address(&vault, &quote.address, &quote.token_program),
            ),
            ("base_token_program", base.token_program),
            ("quote_token_program", quote.token_program),
            ("associated_token_program", anchor_spl::associated_token::ID),
            ("system_program", anchor_lang::system_program::ID),
        ];
        assert_eq!(
            ix.accounts.len(),
            expected.len(),
            "the number of accounts changed"
        );
        for (index, (name, key)) in expected.iter().enumerate() {
            assert_eq!(ix.accounts[index].pubkey, *key, "account #{index} — {name}");
        }
        assert!(ix.accounts[0].is_signer, "owner must sign");
        assert!(
            ix.accounts[3].is_writable,
            "the vault is created, hence written"
        );
    }

    /// The treasuries of both sides must derive from **different** token programs
    /// when the pair mixes them: one program for both would give an address that does not exist.
    #[test]
    fn a_mixed_pair_derives_each_treasury_from_its_own_token_program() {
        let vault = key(2);
        let base = mint(3, 9, anchor_spl::token::ID);
        let quote = mint(3, 6, anchor_spl::token_2022::ID);
        assert_ne!(
            associated_token_address(&vault, &base.address, &base.token_program),
            associated_token_address(&vault, &quote.address, &quote.token_program),
            "the same mint under different token programs has different ATAs"
        );
    }

    #[test]
    fn clearing_a_quote_touches_the_same_two_accounts_as_setting_one() {
        let set = update_quote_ix(
            &key(100),
            &key(9),
            &key(2),
            QuoteUpdate {
                mid_e9: 1,
                spread_bps: 0,
                skew_bps: 0,
                max_size_base: 1,
            },
        );
        let cleared = clear_quote_ix(&key(100), &key(9), &key(2));
        assert_eq!(
            set.accounts
                .iter()
                .map(|meta| meta.pubkey)
                .collect::<Vec<_>>(),
            cleared
                .accounts
                .iter()
                .map(|meta| meta.pubkey)
                .collect::<Vec<_>>()
        );
        assert_ne!(
            set.data[..8],
            cleared.data[..8],
            "instructions have distinct discriminators"
        );
    }

    #[test]
    fn the_native_mint_is_the_address_everyone_knows() {
        assert_eq!(
            NATIVE_MINT.to_string(),
            "So11111111111111111111111111111111111111112"
        );
    }
}
