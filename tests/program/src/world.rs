//! The world of one test: Mollusk, the account store and convenient steps over them.
//!
//! # State between instructions is kept by us
//!
//! `Mollusk::process_instruction` takes accounts in and returns them out; it has
//! no memory between calls. Hostile sequences — "posted a price, withdrew the
//! capital, tried to swap" — are precisely about state surviving an instruction,
//! so the store lives here.
//!
//! **Written back only after success.** That is not an optimization: a refused
//! transaction leaves no trace on chain, and a test that kept the result of a
//! failed instruction would check a state that never exists on the network.
//!
//! # The clock
//!
//! The slot comes from the sysvar, not from the arguments (T016 decision), so
//! the quote age in tests changes in exactly one way — [`World::warp_by`].
//! The start slot is deliberately non-zero: at zero "quote just posted" and
//! "no quote posted" would give the same number in `quote_slot`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anchor_lang::prelude::Pubkey as AnchorKey;
use anchor_lang::{AccountDeserialize, AccountSerialize, AnchorDeserialize, Discriminator};
use mollusk_svm::program::loader_keys::LOADER_V3;
use mollusk_svm::result::InstructionResult;
use mollusk_svm::Mollusk;
use propamm_vault::errors::VaultError;
use propamm_vault::instructions::initialize_vault::InitializeVaultArgs;
use propamm_vault::instructions::swap::{SwapArgs, SwapSide};
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault::state::Vault;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_program_error::ProgramError;
use solana_pubkey::Pubkey as SvmKey;
use solana_svm_log_collector::LogCollector;

use crate::keys::{named, svm};
use crate::{events, fixtures, ix};

/// The pair's decimals — as in SOL/USDC, so the numbers read by eye.
pub const BASE_DECIMALS: u8 = 9;
pub const QUOTE_DECIMALS: u8 = 6;

/// 150 quote per one base, in raw units: `0.15 · 1e9`.
pub const MID_E9: u128 = 150_000_000;
pub const SPREAD_BPS: u16 = 10;
/// 10 base units — the order size ceiling (FR-008).
pub const MAX_SIZE_BASE: u64 = 10_000_000_000;
pub const MAX_QUOTE_AGE_SLOTS: u32 = 25;
pub const MAX_SKEW_BPS: u16 = 3_000;

/// Default treasury holdings — exactly balanced at [`MID_E9`].
///
/// `100e9 · 0.15e9 = 15_000e6 · 1e9`, i.e. a skew of exactly zero. That is not
/// aesthetics: the hard inventory bound (FR-026) forbids a swap only when it
/// takes the inventory **both** past the bound **and** further than it already
/// was, so the bound test has to start from a known point, not "somewhere near zero".
pub const TREASURY_BASE: u64 = 100_000_000_000;
pub const TREASURY_QUOTE: u64 = 15_000_000_000;

const WALLET_BASE: u64 = 500_000_000_000;
const WALLET_QUOTE: u64 = 75_000_000_000;
/// The slot every world starts at.
///
/// Public for the CU benchmark: `MolluskComputeUnitBencher` takes its own
/// `Mollusk`, and that one has to stand at the same slot, otherwise the quote
/// posted while preparing the case arrives at it already stale — and "measuring
/// the swap" would measure the cost of a refusal.
pub const START_SLOT: u64 = 1_000;
const LAMPORTS: u64 = 1_000_000_000_000;

/// An empty `Mollusk` with our program, the token programs and the clock at
/// [`START_SLOT`].
///
/// A separate function because the same set is needed by two owners: [`World`]
/// keeps its own instance, the CU bencher keeps its own, and they have to be
/// assembled identically. A second one assembled slightly differently would measure a different program.
///
/// # Panics
///
/// If the `.so` is missing — see [`crate::program_elf`].
#[must_use]
pub fn mollusk() -> Mollusk {
    let mut mollusk = Mollusk::default();
    mollusk.add_program_with_loader_and_elf(
        &svm(&propamm_vault::ID),
        &LOADER_V3,
        &crate::program_elf(),
    );
    mollusk_svm_programs_token::token::add_program(&mut mollusk);
    mollusk_svm_programs_token::token2022::add_program(&mut mollusk);
    mollusk_svm_programs_token::associated_token::add_program(&mut mollusk);
    mollusk.warp_to_slot(START_SLOT);
    mollusk
}

/// The result of one call together with the log.
pub struct Outcome {
    pub result: InstructionResult,
    pub logs: Vec<String>,
}

impl Outcome {
    /// CU consumed. The SC-002 budget is checked by T021; here the number is merely available.
    #[must_use]
    pub fn cu(&self) -> u64 {
        self.result.compute_units_consumed
    }

    /// # Panics
    ///
    /// If the instruction refused. The log goes into the message in full: without
    /// it "Custom(6013)" has to be translated by hand every time.
    pub fn expect_ok(&self) -> &Self {
        assert!(
            self.result.program_result.is_ok(),
            "expected success, got {:?}\nlog:\n{}",
            self.result.program_result,
            self.logs.join("\n")
        );
        self
    }

    /// # Panics
    ///
    /// If the error code differs or the instruction passed at all.
    pub fn expect_error(&self, expected: VaultError) -> &Self {
        self.expect_custom(u32::from(expected), &format!("{expected:?}"))
    }

    /// An error of the framework itself — `ConstraintSeeds`, `AccountNotSigner` and
    /// the like. Separate from [`Outcome::expect_error`], because these are different
    /// code spaces: ours start at 6000, Anchor's do not.
    ///
    /// # Panics
    ///
    /// If the code differs or the instruction passed.
    pub fn expect_anchor_error(&self, expected: anchor_lang::error::ErrorCode) -> &Self {
        self.expect_custom(u32::from(expected), &format!("{expected:?}"))
    }

    fn expect_custom(&self, code: u32, name: &str) -> &Self {
        let actual = match &self.result.program_result {
            mollusk_svm::result::ProgramResult::Failure(ProgramError::Custom(got)) => Some(*got),
            _ => None,
        };
        assert_eq!(
            actual,
            Some(code),
            "expected {name} ({code}), got {:?}\nlog:\n{}",
            self.result.program_result,
            self.logs.join("\n")
        );
        self
    }

    /// Exactly one event of type `E` from the log.
    ///
    /// # Panics
    ///
    /// If there is not exactly one.
    #[must_use]
    pub fn event<E>(&self) -> E
    where
        E: AnchorDeserialize + Discriminator,
    {
        events::exactly_one::<E>(&self.logs)
    }

    /// How many events of type `E` were written.
    #[must_use]
    pub fn events<E>(&self) -> usize
    where
        E: AnchorDeserialize + Discriminator,
    {
        events::count::<E>(&self.logs)
    }
}

pub struct World {
    pub mollusk: Mollusk,
    pub store: HashMap<SvmKey, Account>,
    pub slot: u64,

    pub owner: AnchorKey,
    pub pricing_authority: AnchorKey,
    pub halt_authority: AnchorKey,
    pub trader: AnchorKey,

    pub base_mint: AnchorKey,
    pub quote_mint: AnchorKey,
    pub base_program: AnchorKey,
    pub quote_program: AnchorKey,

    pub vault: AnchorKey,
    pub bump: u8,
    pub base_treasury: AnchorKey,
    pub quote_treasury: AnchorKey,

    pub owner_base: AnchorKey,
    pub owner_quote: AnchorKey,
    pub trader_base: AnchorKey,
    pub trader_quote: AnchorKey,
}

impl World {
    /// A pair on classic SPL Token: the most common case and the cheapest in CU.
    #[must_use]
    pub fn new() -> Self {
        Self::with_token_programs(anchor_spl::token::ID, anchor_spl::token::ID)
    }

    /// A pair whose sides are served by different token programs (FR-005, T013).
    ///
    /// # Panics
    ///
    /// If the `.so` is missing — see [`crate::program_elf`].
    #[must_use]
    pub fn with_token_programs(base_program: AnchorKey, quote_program: AnchorKey) -> Self {
        let mollusk = mollusk();

        let owner = named(1);
        let pricing_authority = named(2);
        let halt_authority = named(3);
        let trader = named(9);
        let base_mint = named(4);
        let quote_mint = named(5);

        let (vault, bump) = Vault::pda(&owner, &base_mint, &quote_mint);
        let ata = |wallet: &AnchorKey, mint: &AnchorKey, program: &AnchorKey| {
            anchor_spl::associated_token::get_associated_token_address_with_program_id(
                wallet, mint, program,
            )
        };

        let base_treasury = ata(&vault, &base_mint, &base_program);
        let quote_treasury = ata(&vault, &quote_mint, &quote_program);
        let owner_base = ata(&owner, &base_mint, &base_program);
        let owner_quote = ata(&owner, &quote_mint, &quote_program);
        let trader_base = ata(&trader, &base_mint, &base_program);
        let trader_quote = ata(&trader, &quote_mint, &quote_program);

        let mut store: HashMap<SvmKey, Account> = HashMap::new();
        for (key, account) in [
            mollusk_svm_programs_token::token::keyed_account(),
            mollusk_svm_programs_token::token2022::keyed_account(),
            mollusk_svm_programs_token::associated_token::keyed_account(),
            mollusk_svm::program::keyed_account_for_system_program(),
        ] {
            store.insert(key, account);
        }

        let mut world = Self {
            mollusk,
            store,
            slot: START_SLOT,
            owner,
            pricing_authority,
            halt_authority,
            trader,
            base_mint,
            quote_mint,
            base_program,
            quote_program,
            vault,
            bump,
            base_treasury,
            quote_treasury,
            owner_base,
            owner_quote,
            trader_base,
            trader_quote,
        };

        world.put(&owner, fixtures::wallet(LAMPORTS));
        world.put(&pricing_authority, fixtures::wallet(0));
        world.put(&halt_authority, fixtures::wallet(0));
        world.put(&trader, fixtures::wallet(0));
        world.put(&base_mint, fixtures::mint(&base_program, BASE_DECIMALS));
        world.put(&quote_mint, fixtures::mint(&quote_program, QUOTE_DECIMALS));
        world.put(
            &owner_base,
            fixtures::token_account(&base_program, &base_mint, &owner, WALLET_BASE),
        );
        world.put(
            &owner_quote,
            fixtures::token_account(&quote_program, &quote_mint, &owner, WALLET_QUOTE),
        );
        world.put(
            &trader_base,
            fixtures::token_account(&base_program, &base_mint, &trader, WALLET_BASE),
        );
        world.put(
            &trader_quote,
            fixtures::token_account(&quote_program, &quote_mint, &trader, WALLET_QUOTE),
        );

        world
    }

    /// Recompute everything derived from the pair and the token programs.
    ///
    /// Needed by tests that substitute a mint or a program **before** deployment:
    /// the vault address derives from the pair, and the treasury addresses also
    /// from the token program, so without recomputation the instruction would fail
    /// on the seed check instead of the check it was assembled for.
    pub fn retarget(&mut self) -> &mut Self {
        let (vault, bump) = Vault::pda(&self.owner, &self.base_mint, &self.quote_mint);
        self.vault = vault;
        self.bump = bump;

        let ata = |wallet: &AnchorKey, mint: &AnchorKey, program: &AnchorKey| {
            anchor_spl::associated_token::get_associated_token_address_with_program_id(
                wallet, mint, program,
            )
        };
        self.base_treasury = ata(&vault, &self.base_mint, &self.base_program);
        self.quote_treasury = ata(&vault, &self.quote_mint, &self.quote_program);
        self.owner_base = ata(&self.owner, &self.base_mint, &self.base_program);
        self.owner_quote = ata(&self.owner, &self.quote_mint, &self.quote_program);
        self.trader_base = ata(&self.trader, &self.base_mint, &self.base_program);
        self.trader_quote = ata(&self.trader, &self.quote_mint, &self.quote_program);
        self
    }

    /// Create the treasuries in advance — the way a stranger could.
    ///
    /// An ATA for someone else's owner can be created by anyone, and that is exactly
    /// why deployment stands on `init_if_needed` (T013). Here that case is reproduced on purpose.
    pub fn pre_create_treasuries(&mut self) -> &mut Self {
        let base = fixtures::token_account(&self.base_program, &self.base_mint, &self.vault, 0);
        let quote = fixtures::token_account(&self.quote_program, &self.quote_mint, &self.vault, 0);
        self.put(&self.base_treasury.clone(), base);
        self.put(&self.quote_treasury.clone(), quote);
        self
    }

    /// Put an account into the store.
    pub fn put(&mut self, key: &AnchorKey, account: Account) {
        self.store.insert(svm(key), account);
    }

    /// An account from the store, or an empty one — the same way the runtime would see it.
    #[must_use]
    pub fn account(&self, key: &AnchorKey) -> Account {
        self.store.get(&svm(key)).cloned().unwrap_or_default()
    }

    /// Accounts for an instruction: in the order they are listed, without repeats.
    ///
    /// The order is kept, repeats are removed: both sides of the pair may point at
    /// the same token program, and an account listed twice is not the same thing
    /// to Mollusk as one listed once.
    ///
    /// Public for the CU benchmark: it executes the instruction with its own
    /// `Mollusk`, and the slice of state has to be the same one [`World::exec`] would get.
    #[must_use]
    pub fn accounts_for(&self, instruction: &Instruction) -> Vec<(SvmKey, Account)> {
        let mut seen: Vec<SvmKey> = Vec::with_capacity(instruction.accounts.len());
        let mut accounts: Vec<(SvmKey, Account)> = Vec::with_capacity(instruction.accounts.len());
        for meta in &instruction.accounts {
            if seen.contains(&meta.pubkey) {
                continue;
            }
            seen.push(meta.pubkey);
            let account = self.store.get(&meta.pubkey).cloned().unwrap_or_default();
            accounts.push((meta.pubkey, account));
        }
        accounts
    }

    /// Execute an instruction. State is written back **only on success**.
    pub fn exec(&mut self, instruction: &Instruction) -> Outcome {
        let logger = LogCollector::new_ref();
        self.mollusk.logger = Some(Rc::clone(&logger));

        let accounts = self.accounts_for(instruction);
        let result = self.mollusk.process_instruction(instruction, &accounts);
        let logs = RefCell::borrow(&logger).get_recorded_content().to_vec();

        if result.program_result.is_ok() {
            for (key, account) in &result.resulting_accounts {
                self.store.insert(*key, account.clone());
            }
        }

        Outcome { result, logs }
    }

    /// Execute and demand success.
    ///
    /// # Panics
    ///
    /// If the instruction refused — in world preparation that is a test bug, not
    /// a check.
    pub fn must(&mut self, instruction: &Instruction) -> Outcome {
        let outcome = self.exec(instruction);
        outcome.expect_ok();
        outcome
    }

    /// Advance the chain clock.
    pub fn warp_by(&mut self, slots: u64) {
        self.slot += slots;
        self.mollusk.warp_to_slot(self.slot);
    }

    /// Default deployment arguments.
    #[must_use]
    pub fn init_args(&self) -> InitializeVaultArgs {
        InitializeVaultArgs {
            pricing_authority: self.pricing_authority,
            halt_authority: self.halt_authority,
            max_quote_age_slots: MAX_QUOTE_AGE_SLOTS,
            max_skew_bps: MAX_SKEW_BPS,
        }
    }

    /// The deployment instruction with the world's standard accounts.
    #[must_use]
    pub fn initialize_ix(&self, args: InitializeVaultArgs) -> Instruction {
        ix::initialize_vault(
            &self.owner,
            &self.base_mint,
            &self.quote_mint,
            &self.vault,
            &self.base_treasury,
            &self.quote_treasury,
            &self.base_program,
            &self.quote_program,
            args,
        )
    }

    /// Deploy the vault. The treasuries exist and are empty afterwards.
    pub fn deploy(&mut self) -> &mut Self {
        let args = self.init_args();
        let instruction = self.initialize_ix(args);
        self.must(&instruction);
        self
    }

    /// Fill both treasuries up to [`TREASURY_BASE`] / [`TREASURY_QUOTE`].
    pub fn fund(&mut self) -> &mut Self {
        let base = ix::deposit(
            &self.owner,
            &self.vault,
            &self.base_mint,
            &self.base_treasury,
            &self.owner_base,
            &self.base_program,
            TREASURY_BASE,
        );
        self.must(&base);
        let quote = ix::deposit(
            &self.owner,
            &self.vault,
            &self.quote_mint,
            &self.quote_treasury,
            &self.owner_quote,
            &self.quote_program,
            TREASURY_QUOTE,
        );
        self.must(&quote);
        self
    }

    /// Post the standard quote.
    pub fn quote(&mut self) -> &mut Self {
        self.quote_with(QuoteUpdate {
            mid_e9: MID_E9,
            spread_bps: SPREAD_BPS,
            skew_bps: 0,
            max_size_base: MAX_SIZE_BASE,
        })
    }

    /// Post a given quote.
    pub fn quote_with(&mut self, quote: QuoteUpdate) -> &mut Self {
        let instruction = ix::update_quote(&self.pricing_authority, &self.vault, quote);
        self.must(&instruction);
        self
    }

    /// Deployed, funded and quoted — the state most swap checks start from.
    /// Kept as one step because every swap test needs it.
    #[must_use]
    pub fn ready() -> Self {
        let mut world = Self::new();
        world.deploy().fund().quote();
        world
    }

    /// The swap instruction with the world's standard accounts.
    #[must_use]
    pub fn swap_ix(&self, side: SwapSide, amount_in: u64, min_amount_out: u64) -> Instruction {
        ix::swap(
            &self.trader,
            &self.vault,
            &self.base_treasury,
            &self.quote_treasury,
            &self.trader_base,
            &self.trader_quote,
            &self.base_mint,
            &self.quote_mint,
            &self.base_program,
            &self.quote_program,
            SwapArgs {
                side,
                amount_in,
                min_amount_out,
            },
        )
    }

    /// Execute a swap.
    pub fn swap(&mut self, side: SwapSide, amount_in: u64, min_amount_out: u64) -> Outcome {
        let instruction = self.swap_ix(side, amount_in, min_amount_out);
        self.exec(&instruction)
    }

    /// The vault state as the program reads it.
    ///
    /// # Panics
    ///
    /// If the account does not parse — i.e. the vault is not deployed yet.
    #[must_use]
    pub fn vault_state(&self) -> Vault {
        let account = self.account(&self.vault);
        Vault::try_deserialize(&mut account.data.as_slice()).expect("the vault does not parse")
    }

    /// Write the vault state directly.
    ///
    /// Needed for exactly one thing: `halted`. There are no `halt` / `resume`
    /// instructions yet — they are in T052 — while the `VaultHalted` guard already
    /// exists and without this would stay unchecked until Phase 7.
    ///
    /// # Panics
    ///
    /// If the state does not serialize.
    pub fn write_vault_state(&mut self, state: &Vault) {
        let mut data = Vec::new();
        state.try_serialize(&mut data).expect("vault does not pack");
        let mut account = self.account(&self.vault);
        account.data = data;
        self.put(&self.vault.clone(), account);
    }

    /// Halt the vault, bypassing the instruction that does not exist yet.
    pub fn halt(&mut self) -> &mut Self {
        let mut state = self.vault_state();
        state.halted = true;
        self.write_vault_state(&state);
        self
    }

    /// A token account's balance.
    ///
    /// # Panics
    ///
    /// If the account is not a token account.
    #[must_use]
    pub fn balance(&self, key: &AnchorKey) -> u64 {
        fixtures::balance_of(&self.account(key))
    }

    /// Treasury holdings — what the program computes the inventory skew from.
    #[must_use]
    pub fn inventory(&self) -> propamm_quote::Inventory {
        propamm_quote::Inventory {
            base_amount: self.balance(&self.base_treasury),
            quote_amount: self.balance(&self.quote_treasury),
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}
