//! `propamm-router-twin` — a local twin of an aggregator router (FR-019, T037).
//!
//! It does what a router does with a venue, through the same
//! [`jupiter_amm_interface::Amm`] trait a router would use: asks each venue which
//! accounts to read, hands over their bytes, asks for quotes, picks the best one
//! and assembles the transaction. It exists to check US3 without a real listing,
//! and it **does not prove** SC-005 under real conditions: a real router has a
//! different set of venues, its own filters and its own queues (SPEC, «Поза скоупом»).
//!
//! # One venue per order
//!
//! FR-019 asks for the best venue, not for the best split, so a route is one hop.
//! Splitting would make "who won the order" — what SC-005 counts — a fraction.
//!
//! # Ties go against the venue under test
//!
//! When two venues give the same `out`, the venue under test loses; among the
//! rest, fewer accounts (a smaller transaction) wins, then the earlier
//! registration. So the 70% of SC-005 is not inflated by ties, and the result does
//! not depend on the order in which a test registered the venues (T037 decision).
//!
//! # Why [`Venue`] and not `get_swap_and_account_metas`
//!
//! The interface answers with a closed `Swap` enum, and the vault program has no
//! variant until Jupiter lists it (T034). Every venue of the twin builds its own
//! instruction through [`Venue::swap_instruction`] — for the vault that is
//! [`PropAmm::swap_instruction`] (T036), for the rival ([`rival::Rival`], T038)
//! its own builder. After a listing the trait becomes a thin wrapper over the
//! interface method.
//!
//! # The transaction
//!
//! [`Route::transaction`] lays the hop into an unsigned legacy transaction and
//! refuses one that does not fit a packet: SC-002 asks for the swap "at a route
//! transaction size no larger than the network limit", and this is where that
//! size is known. Compute-budget instructions and the CPI into a router program
//! are T039.

#![forbid(unsafe_code)]

pub mod rival;

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use anyhow::{ensure, Context, Result};
use jupiter_amm_interface::{
    AccountMap, Amm, AmmContext, FeeMode, Quote, QuoteParams, SwapMode, SwapParams,
};
use propamm_adapter::PropAmm;
use solana_message::Message;
use solana_transaction::Transaction;
use std::cmp::Reverse;
use std::sync::atomic::Ordering;

/// The largest transaction the network accepts, serialized with its signatures
/// (`PACKET_DATA_SIZE` of the validator: 1280 IPv6 MTU − 48 bytes of headers).
pub const PACKET_DATA_SIZE: usize = 1232;

/// Basis points in one.
const BPS: u64 = 10_000;

/// The router program a hop is built for. The twin has none — the hop is a
/// top-level instruction of the transaction — and no builder here reads it; it
/// only fills the field of [`SwapParams`] that a real router fills with its own id.
pub const TWIN_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0; 32]);

/// A venue the twin can route through: an [`Amm`] that can also build its own
/// swap instruction.
pub trait Venue: Amm + Send + Sync {
    /// The instruction for one hop. `min_amount_out` is the route's bound (FR-009),
    /// not the quoted `out` — see [`PropAmm::swap_instruction`].
    fn swap_instruction(&self, params: &SwapParams, min_amount_out: u64) -> Result<Instruction>;
}

impl Venue for PropAmm {
    fn swap_instruction(&self, params: &SwapParams, min_amount_out: u64) -> Result<Instruction> {
        PropAmm::swap_instruction(self, params, min_amount_out)
    }
}

/// An exact-in order.
#[derive(Clone, Copy, Debug)]
pub struct Order {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub amount: u64,
    /// The tolerance of the route: the hop may deliver this much less than quoted.
    pub slippage_bps: u16,
}

/// Who trades and with which token accounts.
#[derive(Clone, Copy, Debug)]
pub struct Trader {
    /// Signs and pays; may debit `source_token_account`.
    pub authority: Pubkey,
    pub source_token_account: Pubkey,
    pub destination_token_account: Pubkey,
}

/// What one venue answered to an order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub venue: Pubkey,
    pub label: String,
    /// `out` of the quote, or why the venue is out of this order.
    pub outcome: Result<u64, String>,
}

/// The chosen venue and the instruction to execute on it.
#[derive(Clone, Debug)]
pub struct Route {
    pub venue: Pubkey,
    pub label: String,
    pub quote: Quote,
    /// `out × (1 − slippage)`, rounded down.
    pub min_amount_out: u64,
    pub instructions: Vec<Instruction>,
    /// Every venue in registration order, the winner included — the distribution
    /// of a run is counted from these.
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("bad order: {0}")]
    BadOrder(String),
    #[error("no venue quotes the order")]
    NoRoute(Vec<Candidate>),
    #[error("venue {venue} won but did not build its instruction: {reason}")]
    Build { venue: Pubkey, reason: String },
}

struct Entry {
    venue: Box<dyn Venue>,
    /// The outcome of the last [`Router::update`]. A venue whose accounts did not
    /// read is out until they read again: its state is the previous one, and
    /// routing over it would be routing over balances that are no longer there.
    synced: Result<(), String>,
}

/// The twin.
pub struct Router {
    context: AmmContext,
    under_test: Pubkey,
    entries: Vec<Entry>,
}

impl Router {
    /// `under_test` is the key of the venue whose share is being measured — it
    /// loses ties (see the module doc).
    pub fn new(under_test: Pubkey) -> Self {
        Self {
            context: AmmContext::default(),
            under_test,
            entries: Vec::new(),
        }
    }

    /// The context to build venues with: its clock is the router's, shared.
    pub fn context(&self) -> &AmmContext {
        &self.context
    }

    /// The slot freshness is measured against (FR-007).
    pub fn set_slot(&self, slot: u64) {
        self.context.clock_ref.slot.store(slot, Ordering::Relaxed);
    }

    /// Adds a venue. It takes part in routing after its first successful [`Router::update`].
    pub fn register(&mut self, venue: Box<dyn Venue>) -> Result<()> {
        let key = venue.key();
        ensure!(
            self.entries.iter().all(|entry| entry.venue.key() != key),
            "venue {key} is already registered"
        );
        self.entries.push(Entry {
            venue,
            synced: Err("not updated yet".to_string()),
        });
        Ok(())
    }

    /// The accounts every venue asks for, each once, in registration order.
    pub fn accounts_to_update(&self) -> Vec<Pubkey> {
        let mut keys = Vec::new();
        for entry in &self.entries {
            for key in entry.venue.get_accounts_to_update() {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// Hands the accounts to every venue. Returns the venues that failed to read
    /// them — they are out of routing until the next successful update.
    #[must_use]
    pub fn update(&mut self, account_map: &AccountMap) -> Vec<(Pubkey, String)> {
        let mut failed = Vec::new();
        for entry in &mut self.entries {
            entry.synced = entry
                .venue
                .update(account_map)
                .map_err(|err| format!("{err:#}"));
            if let Err(reason) = &entry.synced {
                failed.push((entry.venue.key(), reason.clone()));
            }
        }
        failed
    }

    /// The best venue for `order` and the instruction for it.
    pub fn route(&self, order: &Order, trader: &Trader) -> Result<Route, RouteError> {
        if u64::from(order.slippage_bps) > BPS {
            return Err(RouteError::BadOrder(format!(
                "slippage {} bps is more than the whole amount",
                order.slippage_bps
            )));
        }
        if order.input_mint == order.output_mint {
            return Err(RouteError::BadOrder(
                "input and output mint are the same".into(),
            ));
        }

        let params = QuoteParams {
            amount: order.amount,
            input_mint: order.input_mint,
            output_mint: order.output_mint,
            swap_mode: SwapMode::ExactIn,
            fee_mode: FeeMode::Normal,
        };
        let answers: Vec<(Candidate, Option<Quote>)> = self
            .entries
            .iter()
            .map(|entry| {
                let venue = &entry.venue;
                let quote = Self::ask(entry, &params);
                let candidate = Candidate {
                    venue: venue.key(),
                    label: venue.label(),
                    outcome: quote.as_ref().map(|q| q.out_amount).map_err(Clone::clone),
                };
                (candidate, quote.ok())
            })
            .collect();

        let best = answers
            .iter()
            .enumerate()
            .filter_map(|(index, (_, quote))| quote.map(|quote| (index, quote)))
            .max_by_key(|(index, quote)| {
                let venue = &self.entries[*index].venue;
                (
                    quote.out_amount,
                    venue.key() != self.under_test,
                    Reverse(venue.get_accounts_len()),
                    Reverse(*index),
                )
            });
        let candidates: Vec<Candidate> = answers.into_iter().map(|(c, _)| c).collect();
        let Some((index, quote)) = best else {
            return Err(RouteError::NoRoute(candidates));
        };

        let venue = &self.entries[index].venue;
        let min_amount_out = bound(quote.out_amount, order.slippage_bps);
        let hop = SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: order.amount,
            out_amount: quote.out_amount,
            source_mint: order.input_mint,
            destination_mint: order.output_mint,
            source_token_account: trader.source_token_account,
            destination_token_account: trader.destination_token_account,
            token_transfer_authority: trader.authority,
            user: trader.authority,
            payer: trader.authority,
            quote_mint_to_referrer: None,
            jupiter_program_id: &TWIN_PROGRAM_ID,
            missing_dynamic_accounts_as_default: false,
        };
        let instruction = venue
            .swap_instruction(&hop, min_amount_out)
            .map_err(|err| RouteError::Build {
                venue: venue.key(),
                reason: format!("{err:#}"),
            })?;

        Ok(Route {
            venue: venue.key(),
            label: venue.label(),
            quote,
            min_amount_out,
            instructions: vec![instruction],
            candidates,
        })
    }

    /// One venue's quote, or why it is out of this order.
    fn ask(entry: &Entry, params: &QuoteParams) -> Result<Quote, String> {
        entry.synced.clone()?;
        if !entry.venue.is_active() {
            return Err("inactive".to_string());
        }
        let quote = entry
            .venue
            .quote(params)
            .map_err(|err| format!("{err:#}"))?;
        // The hop is built for the whole amount; a venue that takes less would
        // leave the rest of the order unrouted.
        if quote.in_amount != params.amount {
            return Err(format!("fills {} of {}", quote.in_amount, params.amount));
        }
        if quote.out_amount == 0 {
            return Err("quotes nothing out".to_string());
        }
        Ok(quote)
    }
}

/// `out × (1 − slippage)`, rounded down — the bound never exceeds what the route tolerates.
fn bound(out: u64, slippage_bps: u16) -> u64 {
    let kept = u128::from(BPS - u64::from(slippage_bps));
    // ≤ out, so the narrowing cannot fail.
    (u128::from(out) * kept / u128::from(BPS)) as u64
}

impl Route {
    /// The route as an unsigned legacy transaction paid by `payer`.
    ///
    /// # Errors
    ///
    /// The transaction with its signatures does not fit [`PACKET_DATA_SIZE`].
    pub fn transaction(&self, payer: &Pubkey) -> Result<Transaction> {
        let transaction = Transaction::new_unsigned(Message::new(&self.instructions, Some(payer)));
        let size = transaction_size(&transaction)?;
        ensure!(
            size <= PACKET_DATA_SIZE,
            "the route transaction is {size} bytes, more than the {PACKET_DATA_SIZE} the network accepts"
        );
        Ok(transaction)
    }
}

/// Bytes on the wire, signatures included (an unsigned transaction carries
/// zeroed ones of the same size).
pub fn transaction_size(transaction: &Transaction) -> Result<usize> {
    Ok(bincode::serialize(transaction)
        .context("transaction does not serialize")?
        .len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::prelude::AccountMeta;
    use jupiter_amm_interface::{KeyedAccount, SwapAndAccountMetas};
    use propamm_vault::state::Vault;
    use solana_account::Account;

    const BASE_MINT: Pubkey = Pubkey::new_from_array([4; 32]);
    const QUOTE_MINT: Pubkey = Pubkey::new_from_array([5; 32]);
    const TRADER: Trader = Trader {
        authority: Pubkey::new_from_array([22; 32]),
        source_token_account: Pubkey::new_from_array([20; 32]),
        destination_token_account: Pubkey::new_from_array([21; 32]),
    };
    const FAKE_PROGRAM: Pubkey = Pubkey::new_from_array([88; 32]);

    fn sell(amount: u64, slippage_bps: u16) -> Order {
        Order {
            input_mint: BASE_MINT,
            output_mint: QUOTE_MINT,
            amount,
            slippage_bps,
        }
    }

    /// A venue that quotes a fixed `out` for the pair. It reads its key from the
    /// account map, so leaving the key out fails its update.
    #[derive(Clone)]
    struct Fixed {
        key: Pubkey,
        out: Result<u64, &'static str>,
        accounts_len: usize,
        /// The most it takes in; above it the quote is a partial fill.
        cap: u64,
        active: bool,
    }

    fn fixed(seed: u8, out: u64) -> Fixed {
        Fixed {
            key: Pubkey::new_from_array([seed; 32]),
            out: Ok(out),
            accounts_len: 8,
            cap: u64::MAX,
            active: true,
        }
    }

    impl Amm for Fixed {
        fn from_keyed_account(_: &KeyedAccount, _: &AmmContext) -> Result<Self> {
            unreachable!("built directly")
        }
        fn label(&self) -> String {
            format!("fixed {}", self.key.to_bytes()[0])
        }
        fn program_id(&self) -> Pubkey {
            FAKE_PROGRAM
        }
        fn key(&self) -> Pubkey {
            self.key
        }
        fn get_reserve_mints(&self) -> Vec<Pubkey> {
            vec![BASE_MINT, QUOTE_MINT]
        }
        fn get_accounts_to_update(&self) -> Vec<Pubkey> {
            vec![self.key, BASE_MINT]
        }
        fn update(&mut self, account_map: &AccountMap) -> Result<()> {
            account_map.get(&self.key).context("missing")?;
            Ok(())
        }
        fn quote(&self, params: &QuoteParams) -> Result<Quote> {
            let out = self.out.map_err(anyhow::Error::msg)?;
            Ok(Quote {
                in_amount: params.amount.min(self.cap),
                out_amount: out,
                ..Quote::default()
            })
        }
        fn get_swap_and_account_metas(&self, _: &SwapParams) -> Result<SwapAndAccountMetas> {
            unreachable!("the twin never asks")
        }
        fn get_accounts_len(&self) -> usize {
            self.accounts_len
        }
        fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
            Box::new(self.clone())
        }
        fn is_active(&self) -> bool {
            self.active
        }
    }

    impl Venue for Fixed {
        fn swap_instruction(
            &self,
            params: &SwapParams,
            min_amount_out: u64,
        ) -> Result<Instruction> {
            let mut data = params.in_amount.to_le_bytes().to_vec();
            data.extend(min_amount_out.to_le_bytes());
            let mut accounts = vec![AccountMeta::new_readonly(
                params.token_transfer_authority,
                true,
            )];
            accounts.resize(self.accounts_len, AccountMeta::new(self.key, false));
            Ok(Instruction {
                program_id: FAKE_PROGRAM,
                accounts,
                data,
            })
        }
    }

    fn present(keys: &[Pubkey]) -> AccountMap {
        let mut map = AccountMap::default();
        for key in keys {
            map.insert(*key, Account::default());
        }
        map
    }

    /// A router over `venues`, all updated. The first venue is the one under test.
    fn router(venues: Vec<Fixed>) -> Router {
        let mut router = Router::new(venues[0].key);
        for venue in venues {
            router.register(Box::new(venue)).unwrap();
        }
        let failed = router.update(&present(&router.accounts_to_update()));
        assert!(failed.is_empty(), "{failed:?}");
        router
    }

    #[test]
    fn the_better_quote_wins() {
        let twin = router(vec![fixed(1, 100), fixed(2, 101), fixed(3, 99)]);
        let route = twin.route(&sell(1_000, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([2; 32]));
        assert_eq!(route.quote.out_amount, 101);
        let outs: Vec<_> = route.candidates.iter().map(|c| c.outcome.clone()).collect();
        assert_eq!(outs, vec![Ok(100), Ok(101), Ok(99)]);
    }

    #[test]
    fn a_tie_goes_against_the_venue_under_test() {
        // Under test is registered first and also has the fewest accounts: both
        // neutral rules would hand it the tie.
        let mut ours = fixed(1, 100);
        ours.accounts_len = 3;
        let twin = router(vec![ours, fixed(2, 100)]);
        let route = twin.route(&sell(1_000, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([2; 32]));
    }

    #[test]
    fn among_the_rest_fewer_accounts_then_earlier_registration() {
        let mut lean = fixed(3, 100);
        lean.accounts_len = 4;
        let twin = router(vec![fixed(1, 90), fixed(2, 100), lean.clone()]);
        assert_eq!(twin.route(&sell(1, 0), &TRADER).unwrap().venue, lean.key);

        let twin = router(vec![fixed(1, 90), fixed(2, 100), fixed(3, 100)]);
        assert_eq!(
            twin.route(&sell(1, 0), &TRADER).unwrap().venue,
            Pubkey::new_from_array([2; 32])
        );
    }

    #[test]
    fn venues_that_cannot_take_the_order_are_out_with_a_reason() {
        let mut refusing = fixed(2, 0);
        refusing.out = Err("quote stale");
        let mut inactive = fixed(3, 1_000);
        inactive.active = false;
        let empty = fixed(4, 0);
        let twin = router(vec![fixed(1, 10), refusing, inactive, empty]);

        let route = twin.route(&sell(1_000, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([1; 32]));
        let outs: Vec<_> = route.candidates.iter().map(|c| c.outcome.clone()).collect();
        assert_eq!(
            outs,
            vec![
                Ok(10),
                Err("quote stale".into()),
                Err("inactive".into()),
                Err("quotes nothing out".into()),
            ]
        );
    }

    #[test]
    fn a_venue_that_failed_to_update_is_out_until_it_reads_again() {
        let mut twin = router(vec![fixed(1, 10), fixed(2, 20)]);
        let failed = twin.update(&present(&[Pubkey::new_from_array([1; 32])]));
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, Pubkey::new_from_array([2; 32]));
        let route = twin.route(&sell(1, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([1; 32]));
        assert!(route.candidates[1].outcome.is_err());

        let failed = twin.update(&present(&twin.accounts_to_update()));
        assert!(failed.is_empty());
        let route = twin.route(&sell(1, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([2; 32]));
    }

    #[test]
    fn a_venue_is_out_before_its_first_update() {
        let mut twin = Router::new(Pubkey::new_from_array([1; 32]));
        twin.register(Box::new(fixed(1, 10))).unwrap();
        match twin.route(&sell(1, 0), &TRADER) {
            Err(RouteError::NoRoute(candidates)) => {
                assert_eq!(candidates[0].outcome, Err("not updated yet".into()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_venue_means_no_route_with_every_reason() {
        let mut stale = fixed(2, 0);
        stale.out = Err("quote stale");
        let twin = router(vec![fixed(1, 0), stale]);
        match twin.route(&sell(1, 0), &TRADER) {
            Err(RouteError::NoRoute(candidates)) => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_partial_fill_does_not_win_the_order() {
        let mut partial = fixed(2, 1_000);
        partial.cap = 999;
        let twin = router(vec![fixed(1, 10), partial]);
        let route = twin.route(&sell(1_000, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([1; 32]));
        assert_eq!(route.candidates[1].outcome, Err("fills 999 of 1000".into()));
    }

    #[test]
    fn the_bound_is_out_less_slippage_rounded_down() {
        assert_eq!(bound(10_000, 0), 10_000);
        assert_eq!(bound(10_000, 30), 9_970);
        assert_eq!(bound(9_999, 1), 9_998); // 9 998.0001 → down
        assert_eq!(bound(u64::MAX, 0), u64::MAX);
        assert_eq!(bound(u64::MAX, 10_000), 0);

        let twin = router(vec![fixed(1, 10_000)]);
        let route = twin.route(&sell(1_000, 50), &TRADER).unwrap();
        assert_eq!(route.min_amount_out, 9_950);
        // The bound reached the instruction, not the quote.
        assert_eq!(route.instructions[0].data[8..], 9_950u64.to_le_bytes());
    }

    #[test]
    fn bad_orders_are_refused() {
        let twin = router(vec![fixed(1, 10)]);
        assert!(matches!(
            twin.route(&sell(1, 10_001), &TRADER),
            Err(RouteError::BadOrder(_))
        ));
        let mut same = sell(1, 0);
        same.output_mint = BASE_MINT;
        assert!(matches!(
            twin.route(&same, &TRADER),
            Err(RouteError::BadOrder(_))
        ));
    }

    #[test]
    fn registry_asks_each_account_once_and_refuses_a_second_copy() {
        let mut twin = router(vec![fixed(1, 10), fixed(2, 10)]);
        assert_eq!(
            twin.accounts_to_update(),
            vec![
                Pubkey::new_from_array([1; 32]),
                BASE_MINT,
                Pubkey::new_from_array([2; 32])
            ]
        );
        assert!(twin.register(Box::new(fixed(2, 99))).is_err());
    }

    #[test]
    fn a_transaction_too_big_for_a_packet_is_refused() {
        let twin = router(vec![fixed(1, 10)]);
        let mut route = twin.route(&sell(1, 0), &TRADER).unwrap();
        assert!(route.transaction(&TRADER.authority).is_ok());
        route.instructions[0].data = vec![0; PACKET_DATA_SIZE];
        let why = route.transaction(&TRADER.authority).unwrap_err();
        assert!(why.to_string().contains("more than"), "{why}");
    }

    // A real vault next to a fake rival: the twin quotes it through the adapter
    // and the hop is exactly what the adapter's builder makes.

    const VAULT_KEY: Pubkey = Pubkey::new_from_array([10; 32]);
    const BASE_VAULT: Pubkey = Pubkey::new_from_array([6; 32]);
    const QUOTE_VAULT: Pubkey = Pubkey::new_from_array([7; 32]);
    const QUOTE_SLOT: u64 = 1_000;
    const TOKEN_PROGRAM: Pubkey =
        Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

    /// SOL(9)/USDC(6) at 150 USDC, 5 bps spread — the adapter's fixture.
    fn vault() -> Vault {
        Vault {
            owner: Pubkey::new_from_array([1; 32]),
            pricing_authority: Pubkey::new_from_array([2; 32]),
            halt_authority: Pubkey::new_from_array([3; 32]),
            base_mint: BASE_MINT,
            quote_mint: QUOTE_MINT,
            base_vault: BASE_VAULT,
            quote_vault: QUOTE_VAULT,
            mid_e9: 150_000_000,
            max_size_base: 50_000_000_000,
            quote_slot: QUOTE_SLOT,
            max_quote_age_slots: 10,
            spread_bps: 5,
            skew_bps: 0,
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

    /// The base layout of a token account: mint, owner, amount, …, state = initialized.
    fn token_account(mint: &Pubkey, amount: u64) -> Account {
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(&mint.to_bytes());
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1;
        Account {
            lamports: 1,
            data,
            owner: TOKEN_PROGRAM,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn vault_accounts() -> AccountMap {
        let mut map = AccountMap::default();
        map.insert(VAULT_KEY, vault_account(&vault()));
        map.insert(BASE_VAULT, token_account(&BASE_MINT, 100_000_000_000));
        map.insert(QUOTE_VAULT, token_account(&QUOTE_MINT, 15_000_000_000));
        map
    }

    /// The adapter on its own, outside the twin, on the twin's clock — the oracle.
    fn adapter(context: &AmmContext) -> PropAmm {
        let keyed = KeyedAccount {
            key: VAULT_KEY,
            account: vault_account(&vault()),
            params: None,
        };
        let mut amm = PropAmm::from_keyed_account(&keyed, context).unwrap();
        amm.update(&vault_accounts()).unwrap();
        amm
    }

    const ONE_SOL: u64 = 1_000_000_000;

    /// The vault's quote for selling 1 SOL, asked of the adapter directly.
    fn vault_out() -> u64 {
        let context = AmmContext::default();
        context.clock_ref.slot.store(QUOTE_SLOT, Ordering::Relaxed);
        adapter(&context)
            .quote(&QuoteParams {
                amount: ONE_SOL,
                input_mint: BASE_MINT,
                output_mint: QUOTE_MINT,
                swap_mode: SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .unwrap()
            .out_amount
    }

    /// The vault under test and a rival quoting `rival_out`, both updated.
    fn with_vault(rival_out: u64) -> Router {
        let mut twin = Router::new(VAULT_KEY);
        twin.set_slot(QUOTE_SLOT);
        let keyed = KeyedAccount {
            key: VAULT_KEY,
            account: vault_account(&vault()),
            params: None,
        };
        let amm = PropAmm::from_keyed_account(&keyed, twin.context()).unwrap();
        twin.register(Box::new(amm)).unwrap();
        twin.register(Box::new(fixed(2, rival_out))).unwrap();

        let mut map = present(&twin.accounts_to_update());
        map.extend(vault_accounts());
        let failed = twin.update(&map);
        assert!(failed.is_empty(), "{failed:?}");
        twin
    }

    #[test]
    fn the_vault_wins_over_a_worse_rival_with_its_own_instruction() {
        let out = vault_out();
        let twin = with_vault(out - 1);

        let route = twin.route(&sell(ONE_SOL, 30), &TRADER).unwrap();
        assert_eq!(route.venue, VAULT_KEY);
        assert_eq!(route.label, propamm_adapter::LABEL);
        assert_eq!(route.quote.out_amount, out);
        assert_eq!(route.min_amount_out, bound(out, 30));

        // The hop is the adapter's builder output, byte for byte.
        let expected = adapter(twin.context())
            .swap_instruction(
                &SwapParams {
                    swap_mode: SwapMode::ExactIn,
                    in_amount: ONE_SOL,
                    out_amount: out,
                    source_mint: BASE_MINT,
                    destination_mint: QUOTE_MINT,
                    source_token_account: TRADER.source_token_account,
                    destination_token_account: TRADER.destination_token_account,
                    token_transfer_authority: TRADER.authority,
                    user: TRADER.authority,
                    payer: TRADER.authority,
                    quote_mint_to_referrer: None,
                    jupiter_program_id: &TWIN_PROGRAM_ID,
                    missing_dynamic_accounts_as_default: false,
                },
                bound(out, 30),
            )
            .unwrap();
        assert_eq!(route.instructions, vec![expected]);

        // It fits a packet, and the trader is the only signer and the payer.
        let tx = route.transaction(&TRADER.authority).unwrap();
        assert!(transaction_size(&tx).unwrap() <= PACKET_DATA_SIZE);
        assert_eq!(tx.message.header.num_required_signatures, 1);
        assert_eq!(tx.message.account_keys[0], TRADER.authority);
    }

    #[test]
    fn an_equal_rival_takes_the_tie_from_the_vault() {
        let out = vault_out();
        let twin = with_vault(out);
        let route = twin.route(&sell(ONE_SOL, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([2; 32]));
        assert_eq!(route.candidates[0].outcome, Ok(out));
    }

    /// The vault under test and the public pool of SC-005 holding `sol` / `usdc`.
    fn against_the_pool(sol: u64, usdc: u64) -> Router {
        use crate::rival::{Pool, Rival, RIVAL_FEE_BPS};
        let mut twin = Router::new(VAULT_KEY);
        twin.set_slot(QUOTE_SLOT);
        let keyed = KeyedAccount {
            key: VAULT_KEY,
            account: vault_account(&vault()),
            params: None,
        };
        let amm = PropAmm::from_keyed_account(&keyed, twin.context()).unwrap();
        twin.register(Box::new(amm)).unwrap();
        let (sol_reserve, usdc_reserve) = (
            Pubkey::new_from_array([31; 32]),
            Pubkey::new_from_array([32; 32]),
        );
        let pool = Rival::new(Pool {
            key: Pubkey::new_from_array([30; 32]),
            mints: [BASE_MINT, QUOTE_MINT],
            reserves: [sol_reserve, usdc_reserve],
            fee_bps: RIVAL_FEE_BPS,
        })
        .unwrap();
        twin.register(Box::new(pool)).unwrap();

        let mut map = vault_accounts();
        map.insert(sol_reserve, token_account(&BASE_MINT, sol));
        map.insert(usdc_reserve, token_account(&QUOTE_MINT, usdc));
        let failed = twin.update(&map);
        assert!(failed.is_empty(), "{failed:?}");
        twin
    }

    #[test]
    fn the_vault_beats_the_public_pool_at_the_same_mid_and_loses_to_a_better_one() {
        let out = vault_out();
        // 1 000 SOL / 150 000 USDC: the vault's mid, 30 bps fee against its 5 bps.
        let same_mid = against_the_pool(1_000 * ONE_SOL, 150_000_000_000);
        let route = same_mid.route(&sell(ONE_SOL, 30), &TRADER).unwrap();
        assert_eq!(route.venue, VAULT_KEY);
        let rival_out = route.candidates[1].outcome.clone().unwrap();
        assert!(rival_out < out, "{rival_out} vs {out}");

        // The pool's mid 1% above: its fee and impact no longer cover the gap.
        let richer = against_the_pool(1_000 * ONE_SOL, 151_500_000_000);
        let route = richer.route(&sell(ONE_SOL, 30), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([30; 32]));
        assert_eq!(
            route.instructions[0].program_id,
            crate::rival::RIVAL_PROGRAM_ID
        );
        assert_eq!(route.candidates[0].outcome, Ok(out));
    }

    #[test]
    fn a_stale_vault_is_out_and_the_rival_routes() {
        let twin = with_vault(1);
        twin.set_slot(QUOTE_SLOT + 11);
        let route = twin.route(&sell(ONE_SOL, 0), &TRADER).unwrap();
        assert_eq!(route.venue, Pubkey::new_from_array([2; 32]));
        let why = route.candidates[0].outcome.clone().unwrap_err();
        assert!(why.contains("does not quote"), "{why}");
    }
}
