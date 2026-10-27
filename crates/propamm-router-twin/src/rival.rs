//! The rival of SC-005 (T038): a public constant-product pool, `x·y = k`, with a
//! 30 bps fee — the kind of pool the SPEC sets the vault against («форки
//! constant-product … крива `x*y=k`»).
//!
//! # A model, not a program
//!
//! The rival only quotes. SC-005 counts who wins an order, and for that the twin
//! needs quotes; SC-006 and SC-002 measure the vault alone. So there is no rival
//! program: [`Rival::swap_instruction`] lays out a well-formed hop for
//! [`RIVAL_PROGRAM_ID`], which nothing is deployed at, and an order the rival
//! wins is counted, not executed. **The rival's quote is not checked by
//! execution** — it is the textbook formula, and nothing here proves a real pool
//! would pay it (T038 decision).
//!
//! # State
//!
//! The pool's reserves are the balances of its two token accounts, read through
//! [`Amm::update`] the way a real pool's vaults are read. So the rival goes
//! through the same update path of the twin as the vault, and a run moves the
//! rival's price by rewriting the reserves' bytes. The pair, the reserve accounts
//! and the fee are fixed at construction — [`Rival::new`]: there is no pool
//! account to read them from.
//!
//! # The math
//!
//! The fee is taken from the input, and the output is rounded down once:
//!
//! ```text
//! in' = amount_in × (10 000 − fee_bps)
//! out = ⌊ in' × reserve_out / (reserve_in × 10 000 + in') ⌋
//! ```
//!
//! — the classic constant-product formula with the fee kept in the pool. An
//! order whose product does not fit `u128` is refused, not saturated.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anyhow::{bail, ensure, Context, Result};
use jupiter_amm_interface::{
    try_get_account_data_and_owner, AccountMap, Amm, AmmContext, KeyedAccount, Quote, QuoteParams,
    SwapAndAccountMetas, SwapMode, SwapParams,
};

use crate::{Venue, BPS};

/// The fee of the rival in SC-005.
pub const RIVAL_FEE_BPS: u16 = 30;

/// The program a rival hop names. Nothing is deployed at it: the hop is never
/// executed (see the module doc).
pub const RIVAL_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0xC9; 32]);

/// The first byte of the hop's data.
pub const SWAP_TAG: u8 = 1;

/// The accounts of a rival hop: the pool, the transfer authority, the trader's
/// source, both reserves, the trader's destination and the token program of each
/// side. A real pool adds its authority and fee accounts; fewer accounts favour
/// the rival in a tie and in transaction size — the conservative side for SC-005.
pub const RIVAL_SWAP_ACCOUNTS_LEN: usize = 8;

const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022_PROGRAM: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Base layout of a token account: `mint` 0..32, `amount` 64..72, `state` at 108.
const TOKEN_ACCOUNT_LEN: usize = 165;
const TOKEN_ACCOUNT_STATE: usize = 108;
const STATE_INITIALIZED: u8 = 1;

/// What a pool is made of.
#[derive(Clone, Copy, Debug)]
pub struct Pool {
    /// The pool's address — its key in the twin.
    pub key: Pubkey,
    pub mints: [Pubkey; 2],
    /// The token account holding each mint, in the order of `mints`.
    pub reserves: [Pubkey; 2],
    pub fee_bps: u16,
}

/// The balance of each reserve and the token program that owns it, as of the last update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reserves {
    amounts: [u64; 2],
    programs: [Pubkey; 2],
}

/// A public constant-product pool as a twin venue.
#[derive(Clone, Debug)]
pub struct Rival {
    pool: Pool,
    /// `None` until the first [`Amm::update`]: there is nothing to quote before
    /// the reserves are read.
    reserves: Option<Reserves>,
}

impl Rival {
    /// # Errors
    ///
    /// A fee of the whole amount or more, one mint on both sides, or one account
    /// for both reserves.
    pub fn new(pool: Pool) -> Result<Self> {
        ensure!(
            u64::from(pool.fee_bps) < BPS,
            "a fee of {} bps leaves nothing to swap",
            pool.fee_bps
        );
        ensure!(
            pool.mints[0] != pool.mints[1],
            "the pool trades a mint against itself"
        );
        ensure!(
            pool.reserves[0] != pool.reserves[1],
            "both reserves are one account"
        );
        Ok(Self {
            pool,
            reserves: None,
        })
    }

    /// The reserves as of the last update, in the order of the pool's mints.
    pub fn reserves(&self) -> Option<[u64; 2]> {
        self.reserves.map(|reserves| reserves.amounts)
    }

    /// The index of the input side, or why the pool does not trade the pair.
    fn side(&self, input_mint: &Pubkey, output_mint: &Pubkey) -> Result<usize> {
        let [a, b] = self.pool.mints;
        match (*input_mint, *output_mint) {
            (i, o) if (i, o) == (a, b) => Ok(0),
            (i, o) if (i, o) == (b, a) => Ok(1),
            _ => bail!(
                "pool {} trades {a} / {b}, not {input_mint} → {output_mint}",
                self.pool.key
            ),
        }
    }

    fn updated(&self) -> Result<Reserves> {
        self.reserves
            .with_context(|| format!("pool {} has not been updated yet", self.pool.key))
    }
}

/// `out` for `amount_in` against `reserve_in` / `reserve_out` (see the module doc).
fn constant_product_out(
    amount_in: u64,
    reserve_in: u64,
    reserve_out: u64,
    fee_bps: u16,
) -> Result<u64> {
    ensure!(
        reserve_in > 0 && reserve_out > 0,
        "the pool is empty on one side"
    );
    let bps = u128::from(BPS);
    let in_after_fee = u128::from(amount_in) * (bps - u128::from(fee_bps));
    let numerator = in_after_fee
        .checked_mul(u128::from(reserve_out))
        .context("the order is too large for the pool math")?;
    // reserve_in × 10 000 + in' < 2^64 × 2^14 + 2^78: no overflow.
    let denominator = u128::from(reserve_in) * bps + in_after_fee;
    // < reserve_out, because the denominator exceeds in' while reserve_in > 0.
    Ok((numerator / denominator) as u64)
}

/// The balance of a reserve and its token program, or why it cannot be read.
fn read_reserve(
    account_map: &AccountMap,
    address: &Pubkey,
    mint: &Pubkey,
) -> Result<(u64, Pubkey)> {
    let (data, owner) = try_get_account_data_and_owner(account_map, address)?;
    ensure!(
        *owner == TOKEN_PROGRAM || *owner == TOKEN_2022_PROGRAM,
        "reserve {address} is owned by {owner}, not by a token program"
    );
    ensure!(
        data.len() >= TOKEN_ACCOUNT_LEN,
        "reserve {address} is {} bytes, shorter than a token account",
        data.len()
    );
    ensure!(
        data[..32] == mint.to_bytes(),
        "reserve {address} holds a different mint than the pool trades"
    );
    ensure!(
        data[TOKEN_ACCOUNT_STATE] == STATE_INITIALIZED,
        "reserve {address} is not in the initialized state (frozen or empty)"
    );
    let amount: [u8; 8] = data[64..72].try_into().context("amount field")?;
    Ok((u64::from_le_bytes(amount), *owner))
}

impl Amm for Rival {
    fn from_keyed_account(keyed_account: &KeyedAccount, _: &AmmContext) -> Result<Self> {
        bail!(
            "the rival has no pool account to decode {}: build it with `Rival::new`",
            keyed_account.key
        )
    }

    fn label(&self) -> String {
        format!("Public CPMM {} bps", self.pool.fee_bps)
    }

    fn program_id(&self) -> Pubkey {
        RIVAL_PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.pool.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        self.pool.mints.to_vec()
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        self.pool.reserves.to_vec()
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        // Both are read before either is stored: a failed update keeps the
        // previous pair of balances, not a new one next to an old one.
        let (amount_0, program_0) =
            read_reserve(account_map, &self.pool.reserves[0], &self.pool.mints[0])?;
        let (amount_1, program_1) =
            read_reserve(account_map, &self.pool.reserves[1], &self.pool.mints[1])?;
        self.reserves = Some(Reserves {
            amounts: [amount_0, amount_1],
            programs: [program_0, program_1],
        });
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        ensure!(
            quote_params.swap_mode == SwapMode::ExactIn,
            "the rival quotes exact-in only"
        );
        let input = self.side(&quote_params.input_mint, &quote_params.output_mint)?;
        let amounts = self.updated()?.amounts;
        let out = constant_product_out(
            quote_params.amount,
            amounts[input],
            amounts[1 - input],
            self.pool.fee_bps,
        )?;
        Ok(Quote {
            in_amount: quote_params.amount,
            out_amount: out,
            // Informational: the formula keeps the fee inside one rounding.
            fee_amount: (u128::from(quote_params.amount) * u128::from(self.pool.fee_bps)
                / u128::from(BPS)) as u64,
            fee_mint: quote_params.input_mint,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, _swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        bail!("the rival has no program and no `Swap` variant: its hop comes from `Venue::swap_instruction`")
    }

    fn get_accounts_len(&self) -> usize {
        RIVAL_SWAP_ACCOUNTS_LEN
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Venue for Rival {
    /// `[SWAP_TAG] ++ in_amount ++ min_amount_out`, little-endian, over the
    /// accounts of [`RIVAL_SWAP_ACCOUNTS_LEN`].
    fn swap_instruction(&self, params: &SwapParams, min_amount_out: u64) -> Result<Instruction> {
        ensure!(
            params.swap_mode == SwapMode::ExactIn,
            "the rival swaps exact-in only"
        );
        let input = self.side(&params.source_mint, &params.destination_mint)?;
        let output = 1 - input;
        let programs = self.updated()?.programs;

        let mut data = vec![SWAP_TAG];
        data.extend(params.in_amount.to_le_bytes());
        data.extend(min_amount_out.to_le_bytes());
        Ok(Instruction {
            program_id: RIVAL_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(self.pool.key, false),
                AccountMeta::new_readonly(params.token_transfer_authority, true),
                AccountMeta::new(params.source_token_account, false),
                AccountMeta::new(self.pool.reserves[input], false),
                AccountMeta::new(self.pool.reserves[output], false),
                AccountMeta::new(params.destination_token_account, false),
                AccountMeta::new_readonly(programs[input], false),
                AccountMeta::new_readonly(programs[output], false),
            ],
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Order, Router, Trader, TWIN_PROGRAM_ID};
    use jupiter_amm_interface::FeeMode;
    use solana_account::Account;

    const SOL: Pubkey = Pubkey::new_from_array([4; 32]);
    const USDC: Pubkey = Pubkey::new_from_array([5; 32]);
    const POOL: Pubkey = Pubkey::new_from_array([30; 32]);
    const SOL_RESERVE: Pubkey = Pubkey::new_from_array([31; 32]);
    const USDC_RESERVE: Pubkey = Pubkey::new_from_array([32; 32]);
    const ONE_SOL: u64 = 1_000_000_000;

    fn pool() -> Pool {
        Pool {
            key: POOL,
            mints: [SOL, USDC],
            reserves: [SOL_RESERVE, USDC_RESERVE],
            fee_bps: RIVAL_FEE_BPS,
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

    /// 1 000 SOL against 150 000 USDC: the mid is 150.
    fn reserves(sol: u64, usdc: u64) -> AccountMap {
        let mut map = AccountMap::default();
        map.insert(SOL_RESERVE, token_account(&SOL, sol, TOKEN_PROGRAM));
        map.insert(USDC_RESERVE, token_account(&USDC, usdc, TOKEN_2022_PROGRAM));
        map
    }

    fn rival(sol: u64, usdc: u64) -> Rival {
        let mut rival = Rival::new(pool()).unwrap();
        rival.update(&reserves(sol, usdc)).unwrap();
        rival
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

    /// The formula in floating point, far from rounding and overflow — a second
    /// way of writing it, not the same code.
    fn reference(amount_in: u64, reserve_in: u64, reserve_out: u64, fee_bps: u16) -> f64 {
        let a = amount_in as f64 * (1.0 - f64::from(fee_bps) / 10_000.0);
        a * reserve_out as f64 / (reserve_in as f64 + a)
    }

    #[test]
    fn quotes_the_constant_product_in_both_directions() {
        let (sol, usdc) = (1_000 * ONE_SOL, 150_000_000_000);
        let rival = rival(sol, usdc);

        let out = rival.quote(&ask(ONE_SOL, SOL, USDC)).unwrap();
        assert_eq!(out.in_amount, ONE_SOL);
        assert!((out.out_amount as f64 - reference(ONE_SOL, sol, usdc, 30)).abs() < 1.0);
        // ≈ 150 × 0.997 × 1000/1001: the fee and the impact, both below the mid.
        assert_eq!(out.out_amount, 149_401_047);
        assert_eq!((out.fee_amount, out.fee_mint), (3_000_000, SOL));

        let back = rival.quote(&ask(150_000_000, USDC, SOL)).unwrap();
        assert!((back.out_amount as f64 - reference(150_000_000, usdc, sol, 30)).abs() < 1.0);
        assert!(back.out_amount < ONE_SOL);
    }

    #[test]
    fn rounds_down_and_keeps_k_from_shrinking() {
        let amm = rival(1_000, 3_000);
        for amount in [1, 2, 7, 333, 1_000, 99_999] {
            let out = amm.quote(&ask(amount, SOL, USDC)).unwrap().out_amount;
            assert!(out as f64 <= reference(amount, 1_000, 3_000, 30));
            // After the swap the product does not fall below the one before it.
            assert!(u128::from(1_000 + amount) * u128::from(3_000 - out) >= 3_000_000);
        }
        // At a price of 1, one unit in is less than one out after the fee.
        let even = rival(1_000, 1_000);
        assert_eq!(even.quote(&ask(1, SOL, USDC)).unwrap().out_amount, 0);
    }

    #[test]
    fn the_fee_is_what_separates_it_from_a_free_pool() {
        let mut free = pool();
        free.fee_bps = 0;
        let mut free = Rival::new(free).unwrap();
        free.update(&reserves(1_000_000, 1_000_000)).unwrap();
        // 1 000 into 1 000 000 / 1 000 000 without a fee: ⌊10^9 / 1 001 000⌋.
        assert_eq!(free.quote(&ask(1_000, SOL, USDC)).unwrap().out_amount, 999);
        // With 30 bps: ⌊997 × 10^6 / 1 000 997⌋.
        let paid = rival(1_000_000, 1_000_000);
        assert_eq!(paid.quote(&ask(1_000, SOL, USDC)).unwrap().out_amount, 996);
    }

    #[test]
    fn refuses_what_it_cannot_quote() {
        let amm = rival(1_000, 1_000);
        let why = |params: QuoteParams| format!("{:#}", amm.quote(&params).unwrap_err());
        assert!(why(ask(1, SOL, Pubkey::new_from_array([9; 32]))).contains("trades"));
        let mut exact_out = ask(1, SOL, USDC);
        exact_out.swap_mode = SwapMode::ExactOut;
        assert!(why(exact_out).contains("exact-in"));

        let empty = rival(0, 1_000);
        assert!(format!("{:#}", empty.quote(&ask(1, SOL, USDC)).unwrap_err()).contains("empty"));
        // Nothing to pay out is a refusal with its reason, not a quote of zero.
        assert!(format!("{:#}", empty.quote(&ask(1, USDC, SOL)).unwrap_err()).contains("empty"));
        let huge = rival(1, u64::MAX);
        assert!(
            format!("{:#}", huge.quote(&ask(u64::MAX, SOL, USDC)).unwrap_err())
                .contains("too large")
        );

        let fresh = Rival::new(pool()).unwrap();
        assert!(
            format!("{:#}", fresh.quote(&ask(1, SOL, USDC)).unwrap_err())
                .contains("not been updated")
        );
    }

    #[test]
    fn refuses_a_pool_that_cannot_exist() {
        let mut whole_fee = pool();
        whole_fee.fee_bps = 10_000;
        assert!(Rival::new(whole_fee).is_err());
        let mut one_mint = pool();
        one_mint.mints = [SOL, SOL];
        assert!(Rival::new(one_mint).is_err());
        let mut one_reserve = pool();
        one_reserve.reserves = [SOL_RESERVE, SOL_RESERVE];
        assert!(Rival::new(one_reserve).is_err());
    }

    #[test]
    fn a_failed_update_keeps_the_previous_reserves() {
        let mut rival = rival(1_000, 2_000);
        let mut bad = reserves(5, 5);

        let mut wrong_mint = bad.clone();
        wrong_mint.insert(USDC_RESERVE, token_account(&SOL, 5, TOKEN_PROGRAM));
        let mut foreign = bad.clone();
        foreign.get_mut(&SOL_RESERVE).unwrap().owner = Pubkey::new_from_array([77; 32]);
        let mut short = bad.clone();
        short
            .get_mut(&USDC_RESERVE)
            .unwrap()
            .data
            .truncate(TOKEN_ACCOUNT_LEN - 1);
        let mut frozen = bad.clone();
        frozen.get_mut(&USDC_RESERVE).unwrap().data[TOKEN_ACCOUNT_STATE] = 2;
        bad.remove(&USDC_RESERVE);

        for (map, reason) in [
            (bad, "USDC reserve missing"),
            (wrong_mint, "wrong mint"),
            (foreign, "foreign owner"),
            (short, "short"),
            (frozen, "frozen"),
        ] {
            assert!(rival.update(&map).is_err(), "{reason}");
            assert_eq!(rival.reserves(), Some([1_000, 2_000]), "{reason}");
        }
        rival.update(&reserves(7, 9)).unwrap();
        assert_eq!(rival.reserves(), Some([7, 9]));
    }

    #[test]
    fn asks_for_both_reserves_and_nothing_else() {
        let rival = Rival::new(pool()).unwrap();
        assert_eq!(
            rival.get_accounts_to_update(),
            vec![SOL_RESERVE, USDC_RESERVE]
        );
        assert_eq!(rival.get_reserve_mints(), vec![SOL, USDC]);
        assert_eq!(rival.label(), "Public CPMM 30 bps");
    }

    fn hop(source_mint: Pubkey, destination_mint: Pubkey) -> SwapParams<'static, 'static> {
        SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1_234,
            out_amount: 999,
            source_mint,
            destination_mint,
            source_token_account: Pubkey::new_from_array([20; 32]),
            destination_token_account: Pubkey::new_from_array([21; 32]),
            token_transfer_authority: Pubkey::new_from_array([22; 32]),
            user: Pubkey::new_from_array([23; 32]),
            payer: Pubkey::new_from_array([24; 32]),
            quote_mint_to_referrer: None,
            jupiter_program_id: &TWIN_PROGRAM_ID,
            missing_dynamic_accounts_as_default: false,
        }
    }

    #[test]
    fn the_hop_names_the_reserves_of_its_direction_and_carries_the_bound() {
        let rival = rival(1_000, 1_000);
        let sell = rival.swap_instruction(&hop(SOL, USDC), 900).unwrap();
        assert_eq!(sell.program_id, RIVAL_PROGRAM_ID);
        assert_eq!(sell.accounts.len(), rival.get_accounts_len());
        let mut data = vec![SWAP_TAG];
        data.extend(1_234u64.to_le_bytes());
        data.extend(900u64.to_le_bytes());
        assert_eq!(sell.data, data);

        let keys: Vec<_> = sell.accounts.iter().map(|meta| meta.pubkey).collect();
        let signers: Vec<_> = sell.accounts.iter().filter(|meta| meta.is_signer).collect();
        assert_eq!(signers.len(), 1);
        assert_eq!(signers[0].pubkey, Pubkey::new_from_array([22; 32]));
        assert_eq!(
            keys[2..6],
            [
                Pubkey::new_from_array([20; 32]),
                SOL_RESERVE,
                USDC_RESERVE,
                Pubkey::new_from_array([21; 32])
            ]
        );
        assert_eq!(keys[6..], [TOKEN_PROGRAM, TOKEN_2022_PROGRAM]);

        let buy = rival.swap_instruction(&hop(USDC, SOL), 900).unwrap();
        let keys: Vec<_> = buy.accounts.iter().map(|meta| meta.pubkey).collect();
        assert_eq!(keys[3..5], [USDC_RESERVE, SOL_RESERVE]);
        assert_eq!(keys[6..], [TOKEN_2022_PROGRAM, TOKEN_PROGRAM]);

        assert!(rival.swap_instruction(&hop(SOL, SOL), 0).is_err());
        let mut exact_out = hop(SOL, USDC);
        exact_out.swap_mode = SwapMode::ExactOut;
        assert!(rival.swap_instruction(&exact_out, 0).is_err());
        assert!(Rival::new(pool())
            .unwrap()
            .swap_instruction(&hop(SOL, USDC), 0)
            .is_err());
    }

    #[test]
    fn the_twin_routes_through_it_over_its_update_path() {
        let mut twin = Router::new(Pubkey::new_from_array([1; 32]));
        twin.register(Box::new(Rival::new(pool()).unwrap()))
            .unwrap();
        assert_eq!(twin.accounts_to_update(), vec![SOL_RESERVE, USDC_RESERVE]);
        assert!(twin
            .update(&reserves(1_000 * ONE_SOL, 150_000_000_000))
            .is_empty());

        let order = Order {
            input_mint: SOL,
            output_mint: USDC,
            amount: ONE_SOL,
            slippage_bps: 50,
        };
        let trader = Trader {
            authority: Pubkey::new_from_array([22; 32]),
            source_token_account: Pubkey::new_from_array([20; 32]),
            destination_token_account: Pubkey::new_from_array([21; 32]),
        };
        let route = twin.route(&order, &trader).unwrap();
        assert_eq!(route.venue, POOL);
        assert_eq!(route.quote.out_amount, 149_401_047);
        assert!(route.transaction(&trader.authority).is_ok());

        // A missing reserve takes it out of routing, as for any venue.
        let mut partial = reserves(1, 1);
        partial.remove(&SOL_RESERVE);
        assert_eq!(twin.update(&partial).len(), 1);
        assert!(twin.route(&order, &trader).is_err());
    }
}
