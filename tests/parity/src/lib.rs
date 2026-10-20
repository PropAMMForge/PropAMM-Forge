//! Harness for SC-006 (T035): the adapter's quote against the program's execution.
//!
//! # What "parity" means here
//!
//! For one swap request on one chain state, the adapter is asked first — the way
//! a router asks — and the program executes second. Then exactly one of two
//! things must be true:
//!
//! - **The adapter quoted `out`.** The program executes with `min_amount_out =
//!   out` and moves exactly `amount_in` in and `out` out, unit for unit, on all
//!   four token accounts; and with `min_amount_out = out + 1` it refuses with
//!   `SlippageExceeded`. The second half is what makes the match exact rather
//!   than "the chain gave at least as much": a quote one unit low would pass the
//!   first half and fail here.
//! - **The adapter refused.** The program refuses too, and with the code the
//!   program itself derives from the adapter's reason (`VaultError::from` the
//!   same `QuoteError`), or with `VaultHalted` when the adapter's own halt guard
//!   fired. "Both said no, for different reasons" is a divergence: a router would
//!   be told a story the chain does not tell.
//!
//! # Where each side comes from
//!
//! The program side is the `.so` in the Mollusk world of `tests/program` — the
//! artifact that goes to the network. The adapter side is built from the bytes
//! that world holds **after** the program wrote them: the vault account and both
//! treasuries cross into the interface's `AccountMap` field by field. Nothing is
//! assembled by hand for the adapter, so a layout the adapter misreads cannot
//! hide behind a fixture that misreads it the same way.
//!
//! The same holds the other way (T036): the program executes the instruction the
//! adapter builds, `PropAmm::swap_instruction`, not one listed by hand in
//! `tests/program`. A router gets the quote and the instruction from the adapter;
//! parity covers both.

#![forbid(unsafe_code)]

use std::sync::atomic::Ordering;

use anchor_lang::prelude::Pubkey as AnchorKey;
use jupiter_amm_interface::{
    AccountMap, Amm, AmmContext, FeeMode, KeyedAccount, SwapMode, SwapParams,
};
use mollusk_svm::result::ProgramResult;
use propamm_adapter::{quote_refusal, PropAmm};
use propamm_vault::errors::VaultError;
use propamm_vault::instructions::swap::SwapSide;
use propamm_vault_tests::{svm, Outcome, World};
use solana_program_error::ProgramError;

/// A fourth-generation account (the Mollusk store) as a third-generation one
/// (the interface). The same fields, the same bytes.
fn to_interface(account: &solana_account::Account) -> solana_account_v3::Account {
    solana_account_v3::Account {
        lamports: account.lamports,
        data: account.data.clone(),
        owner: AnchorKey::new_from_array(account.owner.to_bytes()),
        executable: account.executable,
        rent_epoch: account.rent_epoch,
    }
}

/// The accounts a router would fetch for this venue, as the chain holds them now.
fn account_map(world: &World, keys: &[AnchorKey]) -> AccountMap {
    let mut map = AccountMap::default();
    for key in keys {
        map.insert(*key, to_interface(&world.account(key)));
    }
    map
}

/// The vault listed as a venue, plus the clock a router would share with it.
pub struct Venue {
    context: AmmContext,
    pub amm: PropAmm,
}

impl Venue {
    /// List the vault from its account and read the accounts it asks for.
    ///
    /// # Errors
    ///
    /// If the adapter does not accept the vault or its treasuries — on a state the
    /// program itself wrote, that is a parity failure, not a skip.
    pub fn list(world: &World) -> anyhow::Result<Self> {
        let keyed = KeyedAccount {
            key: world.vault,
            account: to_interface(&world.account(&world.vault)),
            params: None,
        };
        let context = AmmContext::default();
        let amm = PropAmm::from_keyed_account(&keyed, &context)?;
        let mut venue = Self { context, amm };
        venue.sync(world)?;
        Ok(venue)
    }

    /// What a router does before quoting: move its clock to the chain's slot and
    /// hand the venue the accounts it asked for, read fresh.
    ///
    /// # Errors
    ///
    /// If `update` refuses the bytes.
    pub fn sync(&mut self, world: &World) -> anyhow::Result<()> {
        self.context
            .clock_ref
            .slot
            .store(world.slot, Ordering::Relaxed);
        let map = account_map(world, &self.amm.get_accounts_to_update());
        self.amm.update(&map)
    }
}

/// Overwrite a token account's balance in place.
///
/// In place, not by replacing the account with a fixture: a Token-2022 treasury
/// created by the program carries `ImmutableOwner` and is longer than 165 bytes,
/// and the adapter has to read the account the program made, not a tidier one.
/// The balance is bytes 64..72 in both token programs.
///
/// # Panics
///
/// If the account is not in the store or is shorter than a token account.
pub fn set_balance(world: &mut World, key: &AnchorKey, amount: u64) {
    let account = world
        .store
        .get_mut(&svm(key))
        .expect("no such account in the store");
    account.data[64..72].copy_from_slice(&amount.to_le_bytes());
}

/// The adapter's view of a side: which mint goes in, which comes out.
fn mints(world: &World, side: SwapSide) -> (AnchorKey, AnchorKey) {
    match side {
        SwapSide::BaseToQuote => (world.base_mint, world.quote_mint),
        SwapSide::QuoteToBase => (world.quote_mint, world.base_mint),
    }
}

/// The four token accounts a swap moves, in the order (trader in, trader out,
/// treasury in, treasury out).
fn legs(world: &World, side: SwapSide) -> [AnchorKey; 4] {
    match side {
        SwapSide::BaseToQuote => [
            world.trader_base,
            world.trader_quote,
            world.base_treasury,
            world.quote_treasury,
        ],
        SwapSide::QuoteToBase => [
            world.trader_quote,
            world.trader_base,
            world.quote_treasury,
            world.base_treasury,
        ],
    }
}

/// Execute a swap with the instruction the adapter builds (FR-017, T036).
///
/// Not `World::swap`: that builder lists the accounts by hand in `tests/program`,
/// and parity through it would say nothing about the instruction a router gets.
/// Here every execution — the honoured quote, the refused `out + 1`, the refusal
/// — goes through `PropAmm::swap_instruction`, so a wrong account, side or signer
/// in the builder is a divergence like any other.
fn execute(
    world: &mut World,
    venue: &Venue,
    side: SwapSide,
    amount_in: u64,
    min_amount_out: u64,
) -> Result<Outcome, String> {
    let (source_mint, destination_mint) = mints(world, side);
    let [source, destination, _, _] = legs(world, side);
    let jupiter = AnchorKey::new_unique();
    let hop = SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: amount_in,
        // Not the bound: the builder takes that as an argument.
        out_amount: 0,
        source_mint,
        destination_mint,
        source_token_account: source,
        destination_token_account: destination,
        token_transfer_authority: world.trader,
        user: world.trader,
        payer: world.trader,
        quote_mint_to_referrer: None,
        jupiter_program_id: &jupiter,
        missing_dynamic_accounts_as_default: false,
    };
    let instruction = venue
        .amm
        .swap_instruction(&hop, min_amount_out)
        .map_err(|err| {
            format!("{side:?} {amount_in}: the adapter built no instruction: {err:#}")
        })?;
    Ok(world.exec(&instruction))
}

/// What one comparison found, when the two sides agreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Quoted and executed, to the unit.
    Executed { amount_in: u64, amount_out: u64 },
    /// Both refused, for this reason.
    Refused(VaultError),
}

fn custom_code(outcome: &Outcome) -> Option<u32> {
    match &outcome.result.program_result {
        ProgramResult::Failure(ProgramError::Custom(code)) => Some(*code),
        _ => None,
    }
}

fn expect_refusal(outcome: &Outcome, expected: VaultError, what: &str) -> Result<(), String> {
    if custom_code(outcome) == Some(u32::from(expected)) {
        return Ok(());
    }
    Err(format!(
        "{what}: expected {expected:?} ({}), the program gave {:?}\nlog:\n{}",
        u32::from(expected),
        outcome.result.program_result,
        outcome.logs.join("\n")
    ))
}

/// Ask the adapter, then execute on the program, and compare.
///
/// The venue must be synced to the world's current state — that is the caller's
/// step, because a sequence keeps one venue across many swaps the way a router does.
///
/// # Errors
///
/// A description of the divergence — the adapter and the program disagreed.
pub fn compare(
    world: &mut World,
    venue: &Venue,
    side: SwapSide,
    amount_in: u64,
) -> Result<Verdict, String> {
    if venue.amm.inventory() != Some(world.inventory()) {
        return Err(format!(
            "the adapter reads inventory {:?}, the treasuries hold {:?}",
            venue.amm.inventory(),
            world.inventory()
        ));
    }

    let (input_mint, output_mint) = mints(world, side);
    let ask = jupiter_amm_interface::QuoteParams {
        amount: amount_in,
        input_mint,
        output_mint,
        swap_mode: SwapMode::ExactIn,
        fee_mode: FeeMode::Normal,
    };

    match venue.amm.quote(&ask) {
        Ok(quote) => {
            if quote.in_amount != amount_in {
                return Err(format!(
                    "asked to quote {amount_in} in, the adapter quoted {} in",
                    quote.in_amount
                ));
            }
            let out = quote.out_amount;

            // One unit more than the quote must not be honoured. Refusals leave no
            // trace in the world, so this does not disturb the execution below.
            if let Some(more) = out.checked_add(1) {
                let outcome = execute(world, venue, side, amount_in, more)?;
                expect_refusal(
                    &outcome,
                    VaultError::SlippageExceeded,
                    &format!("{side:?} {amount_in}: the adapter quoted {out}, asked for {more}"),
                )?;
            }

            let accounts = legs(world, side);
            let before = accounts.map(|key| world.balance(&key));
            let outcome = execute(world, venue, side, amount_in, out)?;
            if !outcome.result.program_result.is_ok() {
                return Err(format!(
                    "{side:?} {amount_in}: the adapter quoted {out}, the program refused with \
                     {:?}\nlog:\n{}",
                    outcome.result.program_result,
                    outcome.logs.join("\n")
                ));
            }
            let after = accounts.map(|key| world.balance(&key));

            // Deltas in i128: one leg goes down, and a u64 underflow would be a
            // panic in the comparison rather than a report of the divergence.
            let delta = |i: usize| i128::from(after[i]) - i128::from(before[i]);
            let expected = [
                -i128::from(amount_in),
                i128::from(out),
                i128::from(amount_in),
                -i128::from(out),
            ];
            let moved = [delta(0), delta(1), delta(2), delta(3)];
            if moved != expected {
                return Err(format!(
                    "{side:?} {amount_in}: the adapter quoted {out}; balances moved {moved:?} \
                     (trader in, trader out, treasury in, treasury out), expected {expected:?}"
                ));
            }
            Ok(Verdict::Executed {
                amount_in,
                amount_out: out,
            })
        }
        Err(err) => {
            let expected = if venue.amm.is_active() {
                match quote_refusal(&err) {
                    Some(reason) => VaultError::from(reason),
                    None => {
                        return Err(format!(
                            "{side:?} {amount_in}: the adapter refused for a reason of its own, \
                             with nothing the program would refuse for: {err:#}"
                        ))
                    }
                }
            } else {
                // The adapter's own guard: the halted flag, the same one `handle_swap`
                // checks before the math.
                VaultError::VaultHalted
            };
            let outcome = execute(world, venue, side, amount_in, 0)?;
            expect_refusal(
                &outcome,
                expected,
                &format!("{side:?} {amount_in}: the adapter refused with «{err:#}»"),
            )?;
            Ok(Verdict::Refused(expected))
        }
    }
}
