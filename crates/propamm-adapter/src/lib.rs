//! `propamm-adapter` — the vault as a venue for a Jupiter-style router (FR-016).
//!
//! An implementation of [`jupiter_amm_interface::Amm`] for one deployment: the
//! router asks which accounts to read ([`Amm::get_accounts_to_update`]), hands
//! over their raw bytes ([`Amm::update`]) and asks for a quote ([`Amm::quote`]).
//! The adapter makes no network calls — everything is computed from those bytes.
//!
//! # There is no math here
//!
//! The quote is [`propamm_quote::compute_swap`] — the same function the program
//! executes the swap with — on parameters taken by the program's own
//! [`Vault::quote_params`], with inventory read from the same two token accounts
//! the program reads it from. That is FR-016a: the router and the chain cannot
//! diverge on rounding, because there is one implementation. The property test of
//! that claim is T035.
//!
//! # What the adapter checks on top of the math
//!
//! Exactly what the program checks on top of it and the crate cannot see (see
//! `handle_swap`): the `halted` flag (FR-024) and that the treasuries are the
//! vault's own token accounts. Plus one thing the program gets from the token
//! program for free: a frozen treasury fails the transfer, so it does not quote.
//!
//! # `Swap` variant
//!
//! [`jupiter_amm_interface::Swap`] is a closed enum: a venue gets its variant from
//! Jupiter at listing, and there is none for this program. The adapter does not
//! borrow someone else's variant — a route built that way would go into another
//! program in a real router. The swap instruction comes from the adapter's own
//! [`PropAmm::swap_instruction`] (FR-017, T036), which takes the same
//! [`SwapParams`] a router hands to [`Amm::get_swap_and_account_metas`].

#![forbid(unsafe_code)]

mod swap;

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, bail, ensure, Context, Result};
use jupiter_amm_interface::{
    try_get_account_data_and_owner, AccountMap, Amm, AmmContext, ClockRef, KeyedAccount, Quote,
    QuoteParams, SwapAndAccountMetas, SwapMode, SwapParams,
};
use propamm_quote::{compute_swap, Inventory, QuoteError, Side, SwapRequest};
use propamm_vault::state::Vault;
use std::sync::atomic::Ordering;

/// What the venue is called in a route.
pub const LABEL: &str = "PropAMM Forge";

/// Classic SPL Token.
const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022. The base layout of its token account is the same 165 bytes;
/// extensions go after them, so reading the base fields does not depend on them.
const TOKEN_2022_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Base layout of a token account: `mint` 0..32, `owner` 32..64, `amount` 64..72,
/// …, `state` at 108.
const TOKEN_ACCOUNT_LEN: usize = 165;
const TOKEN_ACCOUNT_STATE: usize = 108;
const STATE_INITIALIZED: u8 = 1;

/// How many accounts the swap instruction carries — what a router charges the
/// venue for in transaction size. Taken from the program's own `Accounts` struct
/// in a test, so a new account in `Swap` cannot leave this number behind.
pub const SWAP_ACCOUNTS_LEN: usize = 10;

/// One vault deployment as a router venue.
#[derive(Clone)]
pub struct PropAmm {
    key: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    params: propamm_quote::QuoteParams,
    halted: bool,
    /// `None` until the first [`Amm::update`]: inventory lives only in the token
    /// accounts, and there is nothing honest to quote before they are read.
    inventory: Option<Inventory>,
    /// The token program of each side, read with the inventory from the owners
    /// of the treasuries — the swap instruction has to name them, and the vault
    /// state does not record them.
    token_programs: Option<TokenPrograms>,
    /// The router's clock, shared — not copied — so that every clone sees the
    /// slot the router last stored. Freshness (FR-007) is measured against it.
    clock: ClockRef,
}

/// The token program of each side; the pair may be mixed.
///
/// Taken from the treasury, not from the mint: a token account is owned by the
/// program that owns its mint, so the answer is the same, and the treasury is an
/// account the router reads anyway — the mint would be one more fetch per venue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenPrograms {
    base: Pubkey,
    quote: Pubkey,
}

/// A decoded vault account. The owner is checked before the bytes are trusted:
/// a foreign program can write an account with our discriminator.
struct VaultView {
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    params: propamm_quote::QuoteParams,
    halted: bool,
}

fn decode_vault(address: &Pubkey, owner: &Pubkey, mut data: &[u8]) -> Result<VaultView> {
    ensure!(
        *owner == propamm_vault::ID,
        "{address} is owned by {owner}, not by the vault program"
    );
    let vault = Vault::try_deserialize(&mut data)
        .map_err(|err| anyhow!("{address} is not a vault account: {err}"))?;
    Ok(VaultView {
        base_mint: vault.base_mint,
        quote_mint: vault.quote_mint,
        base_vault: vault.base_vault,
        quote_vault: vault.quote_vault,
        params: vault.quote_params(),
        halted: vault.halted,
    })
}

/// The balance of a treasury and its token program, or why it cannot be paid out of.
fn read_treasury(
    account_map: &AccountMap,
    address: &Pubkey,
    mint: &Pubkey,
) -> Result<(u64, Pubkey)> {
    let (data, owner) = try_get_account_data_and_owner(account_map, address)?;
    ensure!(
        *owner == TOKEN_PROGRAM || *owner == TOKEN_2022_PROGRAM,
        "treasury {address} is owned by {owner}, not by a token program"
    );
    ensure!(
        data.len() >= TOKEN_ACCOUNT_LEN,
        "treasury {address} is {} bytes, shorter than a token account",
        data.len()
    );
    ensure!(
        data[..32] == mint.to_bytes(),
        "treasury {address} holds a different mint than the vault records"
    );
    // A frozen treasury is a transfer the token program will refuse: quoting it
    // would be quoting a swap that cannot execute.
    ensure!(
        data[TOKEN_ACCOUNT_STATE] == STATE_INITIALIZED,
        "treasury {address} is not in the initialized state (frozen or empty)"
    );
    let amount: [u8; 8] = data[64..72].try_into().context("amount field")?;
    Ok((u64::from_le_bytes(amount), *owner))
}

/// A refusal of the math itself, kept typed inside the `anyhow::Error` that
/// [`Amm::quote`] returns.
///
/// A wrapper here rather than `impl Error for QuoteError` in `propamm-quote`:
/// that crate is `no_std` and is compiled into the program, and the adapter's
/// needs are no reason to touch the on-chain artifact. The text is the same as
/// before — the type is what is new.
#[derive(Debug)]
struct MathRefusal {
    vault: Pubkey,
    reason: QuoteError,
}

impl std::fmt::Display for MathRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vault {} does not quote: {}", self.vault, self.reason)
    }
}

impl std::error::Error for MathRefusal {}

/// The math's reason, if [`Amm::quote`] refused because of it.
///
/// `None` means the refusal came from the adapter's own guards — the halted
/// flag, an unknown pair, exact-out, no `update` yet. The program maps the same
/// [`QuoteError`] onto its error code (`VaultError::from`), so a router and a
/// test can tell "the chain would refuse with this code" from "not this venue".
pub fn quote_refusal(err: &anyhow::Error) -> Option<QuoteError> {
    err.downcast_ref::<MathRefusal>()
        .map(|refusal| refusal.reason)
}

impl PropAmm {
    /// The current inventory, if the treasuries have been read.
    pub fn inventory(&self) -> Option<Inventory> {
        self.inventory
    }

    /// The quote parameters as of the last read of the vault.
    pub fn quote_params(&self) -> propamm_quote::QuoteParams {
        self.params
    }

    fn side(&self, input_mint: &Pubkey, output_mint: &Pubkey) -> Result<Side> {
        if (*input_mint, *output_mint) == (self.base_mint, self.quote_mint) {
            Ok(Side::BaseToQuote)
        } else if (*input_mint, *output_mint) == (self.quote_mint, self.base_mint) {
            Ok(Side::QuoteToBase)
        } else {
            bail!(
                "vault {} trades {} / {}, not {input_mint} → {output_mint}",
                self.key,
                self.base_mint,
                self.quote_mint
            )
        }
    }
}

impl Amm for PropAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self> {
        let account = &keyed_account.account;
        let view = decode_vault(&keyed_account.key, &account.owner, &account.data)?;
        Ok(Self {
            key: keyed_account.key,
            base_mint: view.base_mint,
            quote_mint: view.quote_mint,
            base_vault: view.base_vault,
            quote_vault: view.quote_vault,
            params: view.params,
            halted: view.halted,
            inventory: None,
            token_programs: None,
            clock: amm_context.clock_ref.clone(),
        })
    }

    fn label(&self) -> String {
        LABEL.to_string()
    }

    fn program_id(&self) -> Pubkey {
        propamm_vault::ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.base_mint, self.quote_mint]
    }

    /// The vault — for the quote, which the engine replaces every few seconds —
    /// and both treasuries — for the inventory, which is their balances (see
    /// `handle_swap`: the state holds no copy of them).
    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.key, self.base_vault, self.quote_vault]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let (data, owner) = try_get_account_data_and_owner(account_map, &self.key)?;
        let view = decode_vault(&self.key, owner, data)?;
        // The pair and the treasuries are fixed at deployment. If they ever read
        // differently, this is not the vault the adapter was built for.
        ensure!(
            (
                view.base_mint,
                view.quote_mint,
                view.base_vault,
                view.quote_vault
            ) == (
                self.base_mint,
                self.quote_mint,
                self.base_vault,
                self.quote_vault
            ),
            "vault {} changed its pair or treasuries",
            self.key
        );

        // Everything is read before anything is stored: a failed update leaves the
        // previous consistent state, not a new quote over old balances.
        let (base_amount, base_program) =
            read_treasury(account_map, &self.base_vault, &self.base_mint)?;
        let (quote_amount, quote_program) =
            read_treasury(account_map, &self.quote_vault, &self.quote_mint)?;
        self.params = view.params;
        self.halted = view.halted;
        self.inventory = Some(Inventory {
            base_amount,
            quote_amount,
        });
        self.token_programs = Some(TokenPrograms {
            base: base_program,
            quote: quote_program,
        });
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        ensure!(
            quote_params.swap_mode == SwapMode::ExactIn,
            "the vault quotes exact-in only"
        );
        let side = self.side(&quote_params.input_mint, &quote_params.output_mint)?;
        // The one guard the program adds to the math (FR-024).
        ensure!(!self.halted, "vault {} is halted", self.key);
        let inventory = self
            .inventory
            .with_context(|| format!("vault {} has not been updated yet", self.key))?;

        let request = SwapRequest {
            side,
            amount_in: quote_params.amount,
            // A quote with no intent to execute: the bound belongs to the route.
            min_amount_out: 0,
        };
        let slot = self.clock.slot.load(Ordering::Relaxed);
        let result = compute_swap(&self.params, &inventory, &request, slot).map_err(|reason| {
            MathRefusal {
                vault: self.key,
                reason,
            }
        })?;

        // No fee: the program charges none, the spread is in the side price, and a
        // derived "fee" would diverge from the fee line of the P&L (FR-021). T034 decision.
        Ok(Quote {
            in_amount: result.amount_in,
            out_amount: result.amount_out,
            fee_mint: quote_params.input_mint,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, _swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        bail!(
            "jupiter-amm-interface has no `Swap` variant for the vault program — Jupiter \
             assigns one at listing; until then the swap instruction comes from the \
             adapter's own builder, `PropAmm::swap_instruction`"
        )
    }

    fn get_accounts_len(&self) -> usize {
        SWAP_ACCOUNTS_LEN
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }

    /// A halted vault is out until its owner resumes it. A vault without a quote
    /// is not: the engine clears the price on a silent feed (FR-014) and posts it
    /// again seconds later — dropping the venue for that would be wrong.
    fn is_active(&self) -> bool {
        !self.halted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::ToAccountMetas;
    use jupiter_amm_interface::FeeMode;
    use propamm_quote::QuoteError;
    use solana_account::Account;

    pub(crate) const KEY: Pubkey = Pubkey::new_from_array([10; 32]);
    pub(crate) const BASE_MINT: Pubkey = Pubkey::new_from_array([4; 32]);
    pub(crate) const QUOTE_MINT: Pubkey = Pubkey::new_from_array([5; 32]);
    pub(crate) const BASE_VAULT: Pubkey = Pubkey::new_from_array([6; 32]);
    pub(crate) const QUOTE_VAULT: Pubkey = Pubkey::new_from_array([7; 32]);

    /// SOL(9)/USDC(6) at 150 USDC — see the scales in `propamm-quote`.
    const MID_E9: u128 = 150_000_000;
    pub(crate) const QUOTE_SLOT: u64 = 1_000;

    pub(crate) fn vault() -> Vault {
        Vault {
            owner: Pubkey::new_from_array([1; 32]),
            pricing_authority: Pubkey::new_from_array([2; 32]),
            halt_authority: Pubkey::new_from_array([3; 32]),
            base_mint: BASE_MINT,
            quote_mint: QUOTE_MINT,
            base_vault: BASE_VAULT,
            quote_vault: QUOTE_VAULT,
            mid_e9: MID_E9,
            max_size_base: 50_000_000_000,
            quote_slot: QUOTE_SLOT,
            max_quote_age_slots: 10,
            spread_bps: 5,
            skew_bps: -3,
            max_skew_bps: 4_000,
            halted: false,
            bump: 255,
        }
    }

    fn vault_account(vault: &Vault) -> Account {
        let mut data = Vec::new();
        anchor_lang::AccountSerialize::try_serialize(vault, &mut data).unwrap();
        Account {
            lamports: 1,
            data,
            owner: propamm_vault::ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn token_account(mint: &Pubkey, amount: u64, program: Pubkey) -> Account {
        let mut data = vec![0; TOKEN_ACCOUNT_LEN];
        data[..32].copy_from_slice(&mint.to_bytes());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[TOKEN_ACCOUNT_STATE] = STATE_INITIALIZED;
        Account {
            lamports: 1,
            data,
            owner: program,
            executable: false,
            rent_epoch: 0,
        }
    }

    const BASE_HELD: u64 = 100_000_000_000; // 100 SOL
    const QUOTE_HELD: u64 = 15_000_000_000; // 15 000 USDC

    fn accounts(vault: &Vault) -> AccountMap {
        let mut map = AccountMap::default();
        map.insert(KEY, vault_account(vault));
        map.insert(
            BASE_VAULT,
            token_account(&BASE_MINT, BASE_HELD, TOKEN_PROGRAM),
        );
        map.insert(
            QUOTE_VAULT,
            token_account(&QUOTE_MINT, QUOTE_HELD, TOKEN_2022_PROGRAM),
        );
        map
    }

    pub(crate) fn context(slot: u64) -> AmmContext {
        let ctx = AmmContext::default();
        ctx.clock_ref.slot.store(slot, Ordering::Relaxed);
        ctx
    }

    pub(crate) fn keyed(vault: &Vault) -> KeyedAccount {
        KeyedAccount {
            key: KEY,
            account: vault_account(vault),
            params: None,
        }
    }

    pub(crate) fn updated(vault: &Vault, slot: u64) -> PropAmm {
        let mut amm = PropAmm::from_keyed_account(&keyed(vault), &context(slot)).unwrap();
        amm.update(&accounts(vault)).unwrap();
        amm
    }

    fn ask(amount: u64, input_mint: Pubkey, output_mint: Pubkey) -> QuoteParams {
        QuoteParams {
            amount,
            input_mint,
            output_mint,
            swap_mode: SwapMode::ExactIn,
            fee_mode: FeeMode::Normal,
        }
    }

    fn reason(amm: &PropAmm, params: &QuoteParams) -> String {
        amm.quote(params).unwrap_err().to_string()
    }

    #[test]
    fn reads_exactly_the_accounts_the_swap_reads_inventory_from() {
        let amm = PropAmm::from_keyed_account(&keyed(&vault()), &context(QUOTE_SLOT)).unwrap();
        assert_eq!(
            amm.get_accounts_to_update(),
            vec![KEY, BASE_VAULT, QUOTE_VAULT]
        );
        assert_eq!(amm.get_reserve_mints(), vec![BASE_MINT, QUOTE_MINT]);
        assert_eq!(amm.key(), KEY);
        assert_eq!(amm.program_id(), propamm_vault::ID);
        assert!(!amm.has_dynamic_accounts());
        assert!(!amm.supports_exact_out());
    }

    /// The core of FR-016a at the level of single cases: the adapter's answer is
    /// `compute_swap` on the program's own mapping of the account. The property
    /// over random inputs is T035.
    #[test]
    fn quote_is_the_program_math_in_both_directions() {
        let v = vault();
        let amm = updated(&v, QUOTE_SLOT + 3);
        let inventory = Inventory {
            base_amount: BASE_HELD,
            quote_amount: QUOTE_HELD,
        };
        assert_eq!(amm.inventory(), Some(inventory));

        let cases = [
            (Side::BaseToQuote, 1_000_000_000, BASE_MINT, QUOTE_MINT),
            (Side::QuoteToBase, 150_000_000, QUOTE_MINT, BASE_MINT),
        ];
        for (side, amount_in, input, output) in cases {
            let expected = compute_swap(
                &v.quote_params(),
                &inventory,
                &SwapRequest {
                    side,
                    amount_in,
                    min_amount_out: 0,
                },
                QUOTE_SLOT + 3,
            )
            .unwrap();
            let quote = amm.quote(&ask(amount_in, input, output)).unwrap();
            assert_eq!(quote.in_amount, amount_in);
            assert_eq!(quote.out_amount, expected.amount_out, "{side:?}");
            assert_eq!(quote.fee_amount, 0);
            assert!(quote.fee_pct.is_zero());
        }
    }

    #[test]
    fn nothing_is_quoted_before_the_treasuries_are_read() {
        let amm = PropAmm::from_keyed_account(&keyed(&vault()), &context(QUOTE_SLOT)).unwrap();
        let why = reason(&amm, &ask(1_000_000_000, BASE_MINT, QUOTE_MINT));
        assert!(why.contains("not been updated"), "{why}");
    }

    /// The router's clock moves after `update`; the quote must age with it, not
    /// with the slot the adapter was created at.
    #[test]
    fn quote_ages_with_the_routers_clock() {
        let ctx = context(QUOTE_SLOT);
        let mut amm = PropAmm::from_keyed_account(&keyed(&vault()), &ctx).unwrap();
        amm.update(&accounts(&vault())).unwrap();
        let clone = amm.clone_amm();
        let params = ask(1_000_000_000, BASE_MINT, QUOTE_MINT);
        assert!(amm.quote(&params).is_ok());

        ctx.clock_ref.slot.store(QUOTE_SLOT + 10, Ordering::Relaxed);
        assert!(
            amm.quote(&params).is_ok(),
            "the limit itself is still fresh"
        );

        ctx.clock_ref.slot.store(QUOTE_SLOT + 11, Ordering::Relaxed);
        let stale = QuoteError::QuoteStale.to_string();
        assert!(reason(&amm, &params).contains(&stale));
        assert_eq!(
            quote_refusal(&amm.quote(&params).unwrap_err()),
            Some(QuoteError::QuoteStale),
            "the reason must stay typed — T035 compares it with the program's code"
        );
        assert!(clone
            .quote(&params)
            .unwrap_err()
            .to_string()
            .contains(&stale));
    }

    #[test]
    fn halted_vault_does_not_quote_and_is_inactive() {
        let v = Vault {
            halted: true,
            ..vault()
        };
        let amm = updated(&v, QUOTE_SLOT);
        assert!(!amm.is_active());
        let why = reason(&amm, &ask(1_000_000_000, BASE_MINT, QUOTE_MINT));
        assert!(why.contains("halted"), "{why}");
        // The halt is the adapter's own guard, not the math's.
        let err = amm
            .quote(&ask(1_000_000_000, BASE_MINT, QUOTE_MINT))
            .unwrap_err();
        assert_eq!(quote_refusal(&err), None);
    }

    /// A cleared quote is "not quoting now", not "gone": the venue stays active.
    #[test]
    fn cleared_quote_refuses_but_keeps_the_venue() {
        let mut v = vault();
        v.clear_quote();
        let amm = updated(&v, QUOTE_SLOT);
        assert!(amm.is_active());
        let why = reason(&amm, &ask(1_000_000_000, BASE_MINT, QUOTE_MINT));
        assert!(why.contains(&QuoteError::QuoteNotSet.to_string()), "{why}");
        let err = amm
            .quote(&ask(1_000_000_000, BASE_MINT, QUOTE_MINT))
            .unwrap_err();
        assert_eq!(quote_refusal(&err), Some(QuoteError::QuoteNotSet));
    }

    #[test]
    fn update_takes_the_new_quote() {
        let mut amm = updated(&vault(), QUOTE_SLOT);
        let params = ask(1_000_000_000, BASE_MINT, QUOTE_MINT);
        let before = amm.quote(&params).unwrap().out_amount;

        let moved = Vault {
            mid_e9: MID_E9 * 2,
            ..vault()
        };
        amm.update(&accounts(&moved)).unwrap();
        assert_eq!(amm.quote_params(), moved.quote_params());
        assert!(amm.quote(&params).unwrap().out_amount > before);
    }

    #[test]
    fn refuses_what_the_vault_does_not_trade() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let stranger = Pubkey::new_from_array([99; 32]);
        assert!(amm.quote(&ask(1, stranger, QUOTE_MINT)).is_err());
        assert!(amm.quote(&ask(1, BASE_MINT, BASE_MINT)).is_err());

        let exact_out = QuoteParams {
            swap_mode: SwapMode::ExactOut,
            ..ask(1_000_000, QUOTE_MINT, BASE_MINT)
        };
        assert!(reason(&amm, &exact_out).contains("exact-in"));
    }

    #[test]
    fn foreign_account_is_not_a_vault() {
        let mut account = vault_account(&vault());
        account.owner = Pubkey::new_from_array([42; 32]);
        let keyed = KeyedAccount {
            key: KEY,
            account,
            params: None,
        };
        assert!(PropAmm::from_keyed_account(&keyed, &context(0)).is_err());

        let mut garbage = vault_account(&vault());
        garbage.data[0] ^= 0xff;
        let keyed = KeyedAccount {
            key: KEY,
            account: garbage,
            params: None,
        };
        assert!(PropAmm::from_keyed_account(&keyed, &context(0)).is_err());
    }

    /// Each broken treasury fails the update and leaves the previous state intact.
    #[test]
    fn broken_treasury_fails_the_update_without_half_applying_it() {
        let mut amm = updated(&vault(), QUOTE_SLOT);
        let good = amm
            .quote(&ask(1_000_000_000, BASE_MINT, QUOTE_MINT))
            .unwrap();

        let moved = Vault {
            mid_e9: MID_E9 * 2,
            ..vault()
        };
        type Breakage = (&'static str, fn(&mut Account));
        let breakages: [Breakage; 4] = [
            ("foreign owner", |a| {
                a.owner = Pubkey::new_from_array([42; 32])
            }),
            ("wrong mint", |a| a.data[..32].copy_from_slice(&[9; 32])),
            ("frozen", |a| a.data[TOKEN_ACCOUNT_STATE] = 2),
            ("truncated", |a| a.data.truncate(100)),
        ];
        for (name, breakage) in breakages {
            let mut map = accounts(&moved);
            breakage(map.get_mut(&QUOTE_VAULT).unwrap());
            assert!(amm.update(&map).is_err(), "{name}");
            assert_eq!(amm.quote_params(), vault().quote_params(), "{name}");
            let again = amm
                .quote(&ask(1_000_000_000, BASE_MINT, QUOTE_MINT))
                .unwrap();
            assert_eq!(again.out_amount, good.out_amount, "{name}");
        }

        let mut map = accounts(&moved);
        map.remove(&BASE_VAULT);
        assert!(amm.update(&map).is_err(), "missing treasury");
    }

    #[test]
    fn accounts_len_is_the_programs_swap_accounts() {
        let key = Pubkey::new_from_array([1; 32]);
        let swap = propamm_vault::accounts::Swap {
            trader: key,
            vault: key,
            base_treasury: key,
            quote_treasury: key,
            trader_base_account: key,
            trader_quote_account: key,
            base_mint: key,
            quote_mint: key,
            base_token_program: key,
            quote_token_program: key,
        };
        assert_eq!(swap.to_account_metas(None).len(), SWAP_ACCOUNTS_LEN);
        assert_eq!(
            updated(&vault(), QUOTE_SLOT).get_accounts_len(),
            SWAP_ACCOUNTS_LEN
        );
    }

    #[test]
    fn swap_metas_refuse_rather_than_borrow_a_variant() {
        let amm = updated(&vault(), QUOTE_SLOT);
        let jupiter = Pubkey::new_from_array([77; 32]);
        let params = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1,
            out_amount: 1,
            source_mint: BASE_MINT,
            destination_mint: QUOTE_MINT,
            source_token_account: Pubkey::new_from_array([20; 32]),
            destination_token_account: Pubkey::new_from_array([21; 32]),
            token_transfer_authority: Pubkey::new_from_array([22; 32]),
            user: Pubkey::new_from_array([22; 32]),
            payer: Pubkey::new_from_array([22; 32]),
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter,
            missing_dynamic_accounts_as_default: false,
        };
        let why = amm.get_swap_and_account_metas(&params).err().unwrap();
        assert!(why.to_string().contains("no `Swap` variant"));
    }
}
