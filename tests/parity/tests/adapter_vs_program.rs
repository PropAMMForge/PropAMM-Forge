//! SC-006 (T035, FR-016a): the quote a router computes locally is what the chain
//! executes — zero units of divergence, in both directions of the comparison.
//!
//! The comparison itself is [`propamm_parity::compare`]; here are the two ways
//! of feeding it, and they find different things:
//!
//! - **Independent cases** ([`any_state_the_adapter_and_the_program_agree`]):
//!   every case starts from a freshly deployed vault and draws its own quote,
//!   risk limits, treasury holdings, token programs, side, size, quote age and
//!   halt. This covers the parameter domain, and proptest can shrink a failure,
//!   because no case leans on state another case left behind.
//! - **One sequence** ([`a_thousand_swaps_in_a_row_execute_as_quoted`]): one
//!   vault, one listed venue, a thousand executed swaps in a row. The adapter
//!   never sees a fixture here — before every quote it re-reads the bytes the
//!   previous swap left, the way a router's `update` does. That is the part the
//!   independent cases cannot see: a treasury layout or a field the program
//!   rewrites and the adapter reads stale would look correct on a fresh deployment.
//!
//! # The composition guards
//!
//! "No divergence" is empty if the comparisons were mostly refusals — two sides
//! saying "no" agree cheaply. So both tests count what really happened and fail
//! when the executed share falls below what was measured, the lesson of T007.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use propamm_parity::{compare, set_balance, Venue, Verdict};
use propamm_quote::{BPS_DENOM, PRICE_SCALE};
use propamm_vault::instructions::authority::RiskLimits;
use propamm_vault::instructions::swap::SwapSide;
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault_tests::rng::Xorshift;
use propamm_vault_tests::world::{MAX_QUOTE_AGE_SLOTS, MAX_SIZE_BASE, MID_E9, START_SLOT};
use propamm_vault_tests::{ix, World};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

/// SC-006 names the number: a thousand random orders, compared against what
/// executed. A random state executes about half the time (0.487–0.511 over
/// eight measured runs of 2 000), so 2 500 cases give about 1 250 executed, and
/// the guard below demands the thousand with a margin of about ten deviations.
const CASES: u32 = 2_500;
/// SC-006's thousand — the same number both tests must reach in executed swaps.
const EXECUTED_TARGET: usize = 1_000;

/// Every reason the program can refuse a swap for on a valid quote, each of
/// which the independent cases must reach at least once: refusal parity is
/// checked per reason, and a reason never drawn is a reason never compared.
/// `SlippageExceeded` is not here — it is asked of every executed case
/// separately, with `min_amount_out = out + 1`.
const REFUSALS: [&str; 8] = [
    "AmountTooSmall",
    "InsufficientLiquidity",
    "InventoryBound",
    "MathOverflow",
    "QuoteNotSet",
    "QuoteStale",
    "SizeExceeded",
    "VaultHalted",
];

/// Mid ceiling — a million quote units per base unit, as in `propamm-quote`'s
/// property tests: wide enough for any real pair, narrow enough that every
/// product stays inside `u128`.
const MID_MAX: u128 = 1_000_000_000_000;
/// Treasury leg ceiling — a million tokens at nine decimals.
const INV_MAX: u64 = 1_000_000_000_000_000;

/// The token programs of the pair's two sides. Mixed pairs are not decoration:
/// a Token-2022 treasury is a longer account than a classic one, and the adapter
/// reads both.
const PAIRS: [(
    &str,
    anchor_lang::prelude::Pubkey,
    anchor_lang::prelude::Pubkey,
); 4] = [
    ("token/token", anchor_spl::token::ID, anchor_spl::token::ID),
    (
        "token/2022",
        anchor_spl::token::ID,
        anchor_spl::token_2022::ID,
    ),
    (
        "2022/token",
        anchor_spl::token_2022::ID,
        anchor_spl::token::ID,
    ),
    (
        "2022/2022",
        anchor_spl::token_2022::ID,
        anchor_spl::token_2022::ID,
    ),
];

/// How the quote stands when the swap is asked for.
#[derive(Debug, Clone, Copy)]
enum Posting {
    /// Posted by `update_quote`.
    Posted,
    /// Posted, then removed by `clear_quote` (FR-014).
    Cleared,
    /// Never posted since deployment.
    Never,
}

#[derive(Debug, Clone)]
struct Case {
    pair: usize,
    posting: Posting,
    quote: QuoteUpdate,
    limits: RiskLimits,
    base_treasury: u64,
    quote_treasury: u64,
    side: SwapSide,
    amount_in: u64,
    /// The trader's balance on the receiving side before the swap.
    trader_out: u64,
    /// Slots between the quote and the swap.
    age: u64,
    halted: bool,
}

/// A rough size of what the vault could pay out, valued at the mid.
///
/// Deliberately not the side price from the code under test: the input is scaled
/// against this as a percentage, past 100% on purpose, so `InsufficientLiquidity`
/// is reached honestly instead of the generator avoiding it.
fn payable(side: SwapSide, mid_e9: u128, base: u64, quote: u64) -> u128 {
    match side {
        SwapSide::BaseToQuote => u128::from(quote) * PRICE_SCALE / mid_e9,
        SwapSide::QuoteToBase => u128::from(base) * mid_e9 / PRICE_SCALE,
    }
}

fn case() -> impl Strategy<Value = Case> {
    let quote = (
        prop_oneof![1 => Just(MID_E9), 3 => 1u128..=MID_MAX],
        prop_oneof![1 => Just(0u16), 6 => 1u16..=500, 1 => 501u16..BPS_DENOM],
        prop_oneof![6 => -2_000i16..=2_000, 1 => -9_999i16..=9_999],
        prop_oneof![3 => Just(u64::MAX), 1 => 1u64..=INV_MAX],
    );
    let limits = (
        1u32..=1_000,
        prop_oneof![3 => Just(BPS_DENOM), 1 => 0u16..=BPS_DENOM],
    );
    let state = (
        0..PAIRS.len(),
        prop_oneof![18 => Just(Posting::Posted), 1 => Just(Posting::Cleared), 1 => Just(Posting::Never)],
        0u64..=INV_MAX,
        0u64..=INV_MAX,
        prop_oneof![39 => Just(false), 1 => Just(true)],
    );
    let request = (
        prop_oneof![Just(SwapSide::BaseToQuote), Just(SwapSide::QuoteToBase)],
        // Percent of `payable`; a zero input and a size from the whole range as
        // their own branches.
        prop_oneof![1 => Just(None), 18 => (1u128..=120).prop_map(Some), 1 => Just(Some(0))],
        0u64..=1_000_000_000_000,
        // Stale on purpose one time in ten; the age seed picks the slot within.
        prop_oneof![9 => Just(false), 1 => Just(true)],
        any::<u32>(),
        any::<u64>(),
    );

    (quote, limits, state, request).prop_map(
        |(
            (mid_e9, spread_bps, skew_bps, max_size_base),
            (max_quote_age_slots, max_skew_bps),
            (pair, posting, base_treasury, quote_treasury, halted),
            (side, percent, trader_out, stale, age_seed, raw_amount),
        )| {
            let amount_in = match percent {
                // The whole range, including amounts no treasury can absorb.
                None => raw_amount,
                Some(0) => 0,
                Some(percent) => {
                    let scaled =
                        payable(side, mid_e9, base_treasury, quote_treasury) * percent / 100;
                    u64::try_from(scaled).unwrap_or(u64::MAX).max(1)
                }
            };
            let max_age = u64::from(max_quote_age_slots);
            let age = if stale {
                max_age + 1 + u64::from(age_seed % 50)
            } else {
                u64::from(age_seed) % (max_age + 1)
            };
            Case {
                pair,
                posting,
                quote: QuoteUpdate {
                    mid_e9,
                    spread_bps,
                    skew_bps,
                    max_size_base,
                },
                limits: RiskLimits {
                    max_quote_age_slots,
                    max_skew_bps,
                },
                base_treasury,
                quote_treasury,
                side,
                amount_in,
                trader_out,
                age,
                halted,
            }
        },
    )
}

/// A deployed vault on one pair of token programs, and its store right after
/// deployment — what every independent case starts from.
struct Deployment {
    world: World,
    pristine: HashMap<solana_pubkey::Pubkey, solana_account::Account>,
}

impl Deployment {
    fn new(
        base_program: anchor_lang::prelude::Pubkey,
        quote_program: anchor_lang::prelude::Pubkey,
    ) -> Self {
        let mut world = World::with_token_programs(base_program, quote_program);
        world.deploy();
        let pristine = world.store.clone();
        Self { world, pristine }
    }

    /// Back to the moment after deployment: the store and the clock.
    fn reset(&mut self) -> &mut World {
        self.world.store.clone_from(&self.pristine);
        self.world.slot = START_SLOT;
        self.world.mollusk.warp_to_slot(START_SLOT);
        &mut self.world
    }
}

/// Bring the world to the case's state through the program's own instructions
/// wherever one exists, then compare.
fn play(world: &mut World, case: &Case) -> Result<Verdict, String> {
    let step = |world: &mut World, ixn: &solana_instruction::Instruction, what: &str| {
        let outcome = world.exec(ixn);
        if outcome.result.program_result.is_ok() {
            Ok(())
        } else {
            Err(format!(
                "setting up the case, {what} refused: {:?} — the generator left the domain",
                outcome.result.program_result
            ))
        }
    };

    let limits = ix::set_risk_limits(&world.owner, &world.vault, case.limits);
    step(world, &limits, "set_risk_limits")?;
    match case.posting {
        Posting::Never => {}
        Posting::Posted | Posting::Cleared => {
            let post = ix::update_quote(&world.pricing_authority, &world.vault, case.quote);
            step(world, &post, "update_quote")?;
            if matches!(case.posting, Posting::Cleared) {
                let clear = ix::clear_quote(&world.pricing_authority, &world.vault);
                step(world, &clear, "clear_quote")?;
            }
        }
    }

    // Holdings are written straight into the program-made accounts: depositing a
    // random million tokens would test `deposit`, not the swap.
    let (base_treasury, quote_treasury) = (world.base_treasury, world.quote_treasury);
    set_balance(world, &base_treasury, case.base_treasury);
    set_balance(world, &quote_treasury, case.quote_treasury);
    let (trader_in, trader_out) = match case.side {
        SwapSide::BaseToQuote => (world.trader_base, world.trader_quote),
        SwapSide::QuoteToBase => (world.trader_quote, world.trader_base),
    };
    // Exactly the input: a trader who cannot pay is a token-program refusal the
    // adapter has no way to know about, and not what SC-006 is about.
    set_balance(world, &trader_in, case.amount_in);
    set_balance(world, &trader_out, case.trader_out);

    if case.halted {
        // There is no `halt` instruction until T052; the harness writes the flag.
        world.halt();
    }
    world.warp_by(case.age);

    let venue = Venue::list(world).map_err(|err| format!("the adapter does not list: {err:#}"))?;
    compare(world, &venue, case.side, case.amount_in)
}

/// What a run consisted of.
#[derive(Default, Debug)]
struct Tally {
    executed: usize,
    refused: BTreeMap<String, usize>,
}

impl Tally {
    fn record(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Executed { .. } => self.executed += 1,
            Verdict::Refused(reason) => {
                *self.refused.entry(format!("{reason:?}")).or_default() += 1
            }
        }
    }

    fn total(&self) -> usize {
        self.executed + self.refused.values().sum::<usize>()
    }
}

#[test]
fn any_state_the_adapter_and_the_program_agree() {
    let deployments = RefCell::new(
        PAIRS
            .iter()
            .map(|(_, base, quote)| Deployment::new(*base, *quote))
            .collect::<Vec<_>>(),
    );
    let tally = RefCell::new(Tally::default());

    let mut runner = TestRunner::new(Config {
        cases: CASES,
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&case(), |case| {
        let mut deployments = deployments.borrow_mut();
        let world = deployments[case.pair].reset();
        let verdict = play(world, &case)
            .map_err(|why| TestCaseError::fail(format!("pair {}: {why}", PAIRS[case.pair].0)))?;
        tally.borrow_mut().record(verdict);
        Ok(())
    });
    if let Err(err) = result {
        panic!("{err}");
    }

    let tally = tally.into_inner();
    let share = tally.executed as f64 / tally.total() as f64;
    eprintln!(
        "independent cases: {} compared, {} executed ({share:.3}), refused {:?}",
        tally.total(),
        tally.executed,
        tally.refused
    );
    assert!(
        tally.executed >= EXECUTED_TARGET,
        "only {} of {} cases executed ({share:.3}) — SC-006 needs {EXECUTED_TARGET} \
         compared against an execution, not against a refusal",
        tally.executed,
        tally.total()
    );
    let missing: Vec<_> = REFUSALS
        .iter()
        .filter(|reason| !tally.refused.contains_key(**reason))
        .collect();
    assert!(
        missing.is_empty(),
        "never refused for {missing:?} — the parity of those reasons went unchecked"
    );
    let unexpected: Vec<_> = tally
        .refused
        .keys()
        .filter(|reason| !REFUSALS.contains(&reason.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "refused for {unexpected:?}, which this test does not expect to draw"
    );
}

/// The sequence's seed. Fixed: a thousand swaps in a row are reproduced from the
/// step number, not from a regression file — the same reasoning as SC-004's campaign.
const SEED: u64 = 0x5EED_2026_1017_0035;
/// Attempts allowed to reach the target: the stale and bound refusals in between
/// are part of the run, an endless one is a broken generator.
const MAX_ATTEMPTS: usize = 3_000;

#[test]
fn a_thousand_swaps_in_a_row_execute_as_quoted() {
    // A mixed pair: both treasury layouts in one run.
    let mut world = World::with_token_programs(anchor_spl::token::ID, anchor_spl::token_2022::ID);
    world.deploy().fund().quote();
    let mut venue = Venue::list(&world).expect("the adapter lists the vault");
    let mut rng = Xorshift::new(SEED);
    let mut mid = MID_E9;
    let mut tally = Tally::default();

    for attempt in 0..MAX_ATTEMPTS {
        if tally.executed == EXECUTED_TARGET {
            break;
        }

        // The engine's side of the world: a re-quote now and then (a random walk
        // of the mid, within a percent), and the clock moving — sometimes far
        // enough to make the quote stale.
        match rng.below(10) {
            0..=2 => {
                let step = i128::from(rng.between(0, 200)) - 100;
                mid = u128::try_from(i128::try_from(mid).unwrap() * (10_000 + step) / 10_000)
                    .unwrap()
                    .max(1);
                let quote = QuoteUpdate {
                    mid_e9: mid,
                    spread_bps: u16::try_from(rng.between(0, 50)).unwrap(),
                    skew_bps: i16::try_from(i64::try_from(rng.between(0, 600)).unwrap() - 300)
                        .unwrap(),
                    max_size_base: MAX_SIZE_BASE,
                };
                world.quote_with(quote);
            }
            3 => world.warp_by(u64::from(MAX_QUOTE_AGE_SLOTS) + 1 + rng.below(10)),
            _ => world.warp_by(rng.below(3)),
        }

        let side = if rng.below(2) == 0 {
            SwapSide::BaseToQuote
        } else {
            SwapSide::QuoteToBase
        };
        // Up to about one base unit either way — the treasury holds a hundred, so
        // the run does not drain into `InsufficientLiquidity`.
        let amount_in = match side {
            SwapSide::BaseToQuote => rng.between(1, 1_000_000_000),
            SwapSide::QuoteToBase => rng.between(1, 150_000_000),
        };

        venue
            .sync(&world)
            .unwrap_or_else(|err| panic!("attempt {attempt}: update refused: {err:#}"));
        let verdict = compare(&mut world, &venue, side, amount_in)
            .unwrap_or_else(|why| panic!("attempt {attempt}: {why}"));
        tally.record(verdict);
    }

    eprintln!(
        "sequence: {} attempts, {} executed, refused {:?}",
        tally.total(),
        tally.executed,
        tally.refused
    );
    assert_eq!(
        tally.executed, EXECUTED_TARGET,
        "the run stopped short of {EXECUTED_TARGET} executed swaps"
    );
    assert!(
        tally.refused.contains_key("QuoteStale"),
        "no stale refusal in the run — the freshness half of the comparison went unchecked"
    );
}
