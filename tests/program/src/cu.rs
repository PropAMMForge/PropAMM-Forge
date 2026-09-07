//! Cases for CU measurement and the budget they must not exceed
//! (T021, FR-018, SC-002).
//!
//! # Why the cases live here and not in the benchmark itself
//!
//! They have two consumers, and both have to measure the same thing. `benches/bench.rs`
//! draws a table with a delta to the previous run — a tool for whoever edits the
//! program. `tests/budget.rs` turns the budget into a pass condition — what CI
//! runs. If each built its own cases, the gate would guard one thing while the
//! report showed another, and the divergence would be noticed on the day it
//! already costs money.
//!
//! # Why the budget has to be checked by us
//!
//! `MolluskComputeUnitBencher` knows no budget: `must_pass(true)` panics only
//! when the instruction **refused**. It may cost 200 000 CU and calmly land in
//! the table. The "pass condition" from FR-018 is our
//! assertion, not its.
//!
//! # The trap a separate guard stands against here
//!
//! A swap refused on the very first guard costs a few thousand CU and fits
//! **any** budget. A "CU ≤ 60 000" check on such a case is green and means
//! nothing — the same empty check that led to the success-rate measurement in
//! T007 and to narrowing the skew bound in T020. So [`measure_all`] demands
//! success from every case, and [`gated`] counts how many cases are really
//! under the gate.
//!
//! # What the gate covers
//!
//! The worst of the measured pair variants, not the most convenient: classic
//! SPL on both sides, a mixed pair, Token-2022 on both sides and Token-2022
//! with mints carrying an allowed extension. A transfer fee is deliberately not
//! among the variants — `mint_guard` refuses such a mint at deployment (FR-005),
//! so no pair with it exists, and measuring on it would gate what we do not ship.

use anchor_lang::prelude::Pubkey as AnchorKey;
use propamm_vault::instructions::swap::SwapSide;
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey as SvmKey;

use crate::{fixtures, world, World};

/// The swap instruction budget from SC-002.
///
/// The number is not arbitrary: an aggregator's route transaction carries several
/// venues, and 60 000 is the share of the total limit at which we stay in the route.
/// It may be changed only together with SC-002, and then also in `docs/SPEC.md`.
pub const SWAP_BUDGET_CU: u64 = 60_000;

/// Order size base → quote: one base asset.
///
/// The same as in the `tests/swap.rs` happy path. The cost of a swap does not
/// depend on size — the arithmetic is the same on any numbers — but taking the
/// same size is cheaper than explaining why it differs here.
const ONE_BASE: u64 = 1_000_000_000;
/// Order size quote → base: 150 quote units, i.e. the counter side of the
/// same trade.
const ONE_BASE_WORTH_OF_QUOTE: u64 = 150_000_000;

/// One measured case: the instruction together with the slice of state it
/// executes on.
pub struct Case {
    /// The name in the table. No `|` — the string goes into a markdown cell.
    pub name: &'static str,
    pub instruction: Instruction,
    pub accounts: Vec<(SvmKey, Account)>,
    /// The ceiling the case must not exceed, or `None` — "report only".
    ///
    /// `None` on `update_quote` is not forgetfulness: there is no declared budget
    /// for it in either SPEC or PLAN, and an invented threshold would be a number
    /// off a shelf. A regression there is visible from the delta in the table.
    pub budget: Option<u64>,
}

/// What was measured for one case.
pub struct Measured {
    pub name: &'static str,
    pub consumed: u64,
    pub budget: Option<u64>,
}

impl Measured {
    /// Whether the case exceeded its ceiling.
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.budget.is_some_and(|budget| self.consumed > budget)
    }
}

/// A pair brought to the state a swap is made from.
fn ready_pair(base_program: AnchorKey, quote_program: AnchorKey) -> World {
    let mut world = World::with_token_programs(base_program, quote_program);
    world.deploy().fund().quote();
    world
}

/// A Token-2022 pair whose mints both carry an allowed extension.
fn ready_pair_with_extended_mints() -> World {
    let program = anchor_spl::token_2022::ID;
    let mut world = World::with_token_programs(program, program);
    world.put(
        &world.base_mint.clone(),
        fixtures::mint_with_metadata_pointer(&program, world::BASE_DECIMALS),
    );
    world.put(
        &world.quote_mint.clone(),
        fixtures::mint_with_metadata_pointer(&program, world::QUOTE_DECIMALS),
    );
    world.deploy().fund().quote();
    world
}

/// Two swap cases — one per direction — from a ready pair.
fn swap_cases(
    world: &World,
    base_to_quote: &'static str,
    quote_to_base: &'static str,
) -> [Case; 2] {
    let case = |name: &'static str, side, amount_in| {
        let instruction = world.swap_ix(side, amount_in, 0);
        Case {
            name,
            accounts: world.accounts_for(&instruction),
            instruction,
            budget: Some(SWAP_BUDGET_CU),
        }
    };
    [
        case(base_to_quote, SwapSide::BaseToQuote, ONE_BASE),
        case(
            quote_to_base,
            SwapSide::QuoteToBase,
            ONE_BASE_WORTH_OF_QUOTE,
        ),
    ]
}

/// All cases for the benchmark and for the gate.
///
/// The order in the list is the row order in the table; keeping it stable is
/// worth it for the delta the bencher looks up by name.
#[must_use]
pub fn cases() -> Vec<Case> {
    let spl = anchor_spl::token::ID;
    let t22 = anchor_spl::token_2022::ID;

    let spl_pair = ready_pair(spl, spl);
    let mut out = Vec::with_capacity(9);
    out.extend(swap_cases(
        &spl_pair,
        "swap base to quote (spl)",
        "swap quote to base (spl)",
    ));
    out.extend(swap_cases(
        &ready_pair(spl, t22),
        "swap base to quote (spl+t22)",
        "swap quote to base (spl+t22)",
    ));
    out.extend(swap_cases(
        &ready_pair(t22, t22),
        "swap base to quote (t22)",
        "swap quote to base (t22)",
    ));
    out.extend(swap_cases(
        &ready_pair_with_extended_mints(),
        "swap base to quote (t22+ext)",
        "swap quote to base (t22+ext)",
    ));

    // `update_quote` — report only. The engine sends it every tick, so a regression
    // here costs money, but it has no declared ceiling (see `Case::budget`).
    let instruction = crate::ix::update_quote(
        &spl_pair.pricing_authority,
        &spl_pair.vault,
        propamm_vault::instructions::update_quote::QuoteUpdate {
            mid_e9: world::MID_E9,
            spread_bps: world::SPREAD_BPS,
            skew_bps: 0,
            max_size_base: world::MAX_SIZE_BASE,
        },
    );
    out.push(Case {
        name: "update_quote",
        accounts: spl_pair.accounts_for(&instruction),
        instruction,
        budget: None,
    });

    out
}

/// How many cases are really under the gate.
///
/// Needed by the budget test: a list that shrank to zero gated cases would give
/// a green gate that guards nothing.
#[must_use]
pub fn gated(cases: &[Case]) -> usize {
    cases.iter().filter(|case| case.budget.is_some()).count()
}

/// Execute every case and return the CU consumed.
///
/// # Panics
///
/// If any case **refused**. This is not strictness for its own sake: a refused
/// swap costs a few thousand CU and would fit the budget, i.e. the gate would
/// stay green exactly when the most important thing broke.
#[must_use]
pub fn measure_all(cases: &[Case]) -> Vec<Measured> {
    let mollusk = world::mollusk();
    cases
        .iter()
        .map(|case| {
            let result = mollusk.process_instruction(&case.instruction, &case.accounts);
            assert!(
                result.program_result.is_ok(),
                "case \"{}\" refused: {:?} — the number measured on it means nothing",
                case.name,
                result.program_result
            );
            Measured {
                name: case.name,
                consumed: result.compute_units_consumed,
                budget: case.budget,
            }
        })
        .collect()
}

/// A table for the eyes: the same thing the bencher puts into markdown, but
/// straight to stdout and with a verdict against the budget.
#[must_use]
pub fn table(measured: &[Measured]) -> String {
    let width = measured
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or_default();
    let mut out = String::new();
    for m in measured {
        let verdict = match m.budget {
            Some(budget) if m.consumed > budget => format!("OVER THE BUDGET {budget}"),
            Some(budget) => format!("within {budget}"),
            None => "no budget".to_string(),
        };
        out.push_str(&format!(
            "  {:width$}  {:>7} CU  {verdict}\n",
            m.name, m.consumed
        ));
    }
    out
}

/// The cases that exceeded their ceiling.
#[must_use]
pub fn overruns(measured: &[Measured]) -> Vec<&Measured> {
    measured.iter().filter(|m| m.over_budget()).collect()
}

/// The most expensive of the gated cases — the number SC-002 is reported with.
#[must_use]
pub fn worst_gated(measured: &[Measured]) -> Option<&Measured> {
    measured
        .iter()
        .filter(|m| m.budget.is_some())
        .max_by_key(|m| m.consumed)
}

/// Where the bencher puts `compute_units.md`.
///
/// In `target/`, not next to the crate: `*.md` is in `.gitignore`, so the file
/// would not land in the repo anyway, and the delta between runs is needed only
/// locally — in CI there is no previous run by definition.
#[must_use]
pub fn report_dir() -> String {
    format!("{}/../../target/benches", env!("CARGO_MANIFEST_DIR"))
}
