//! SC-002 as a pass condition, not as a number in a report (T021, FR-018).
//!
//! # Why this is a test and not only a benchmark
//!
//! The budget has to catch a regression where people look — in CI. CI runs
//! `cargo test --workspace`; it does not run `cargo bench`, and adding a separate
//! step would mean compiling one more target for a single `assert`. The cases
//! are meanwhile shared with the benchmark ([`cu::cases`]), so the gate and the
//! report cannot diverge.
//!
//! # Why three tests rather than one
//!
//! A single assert "CU ≤ 60 000" goes green from anything: from an empty list
//! of cases, from a swap refused on the first guard, from a list where only the
//! budget-less `update_quote` remains. Each of the three tests removes one of
//! the ways to get green for no reason.

use propamm_vault_tests::cu;

/// How many cases must stand under the gate: four pair variants × two directions.
///
/// The number here is deliberately strict. If merely "non-zero" were checked, a
/// case lost while editing `cu::cases` would take a whole pair variant with it,
/// and the gate would stay green precisely because it stopped looking that way.
const GATED_CASES: usize = 8;

/// The main claim of T021.
#[test]
fn every_swap_fits_the_declared_budget() {
    let cases = cu::cases();
    let measured = cu::measure_all(&cases);
    let overruns = cu::overruns(&measured);

    assert!(
        overruns.is_empty(),
        "the swap went over the SC-002 budget ({} CU):\n{}\nfull table:\n{}",
        cu::SWAP_BUDGET_CU,
        overruns
            .iter()
            .map(|m| format!("  {} — {} CU", m.name, m.consumed))
            .collect::<Vec<_>>()
            .join("\n"),
        cu::table(&measured),
    );

    // The number SC-002 is reported with is printed always: otherwise "green" does
    // not say by what margin. `cargo test -- --nocapture`.
    let worst = cu::worst_gated(&measured).expect("not a single gated case");
    println!(
        "most expensive swap: {} — {} CU, margin {} CU\n{}",
        worst.name,
        worst.consumed,
        cu::SWAP_BUDGET_CU - worst.consumed,
        cu::table(&measured),
    );
}

/// The gate must not shrink unnoticed.
#[test]
fn the_gate_still_covers_every_pair_variant() {
    let cases = cu::cases();
    assert_eq!(
        cu::gated(&cases),
        GATED_CASES,
        "the number of cases under the budget changed — either a pair variant was added \
         (then raise GATED_CASES) or one was lost (then the gate stopped looking \
         that way)"
    );
}

/// What is measured has to be measured on success.
///
/// `measure_all` demands success itself and panics on a refusal; this test checks
/// that the demand really works — on a case that refuses on purpose. Without it
/// the guard would remain a line of code nobody ever executed.
#[test]
#[should_panic(expected = "means nothing")]
fn a_failing_case_is_never_reported_as_cheap() {
    use propamm_vault::instructions::swap::SwapSide;
    use propamm_vault_tests::{cu::Case, World};

    // The vault is deployed and funded, but no quote is posted — the swap refuses
    // on `QuoteStale` and costs single-digit thousands of CU, i.e. calmly "fits"
    // into 60 000.
    let mut world = World::new();
    world.deploy().fund();
    let instruction = world.swap_ix(SwapSide::BaseToQuote, 1_000_000_000, 0);

    let cheap_failure = Case {
        name: "swap without a quote",
        accounts: world.accounts_for(&instruction),
        instruction,
        budget: Some(cu::SWAP_BUDGET_CU),
    };
    // The value goes nowhere: the test expects a panic, not a number.
    let _ = cu::measure_all(&[cheap_failure]);
}
