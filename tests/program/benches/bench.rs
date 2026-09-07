//! CU benchmark of the `swap` and `update_quote` instructions (T021, FR-018, SC-002).
//!
//! Invoked as `cargo bench -p propamm-vault-tests`, and from Windows as
//! `wsl-build.sh bench-cu`. It executes the real `target/deploy/propamm_vault.so`,
//! so it does not start without a build; the message about that comes from [`program_elf`].
//!
//! # What the bencher does here and what we do ourselves
//!
//! `MolluskComputeUnitBencher` can do one thing, but a useful one: it remembers
//! the previous run and shows the **delta** — exactly what is needed when you
//! edit the program and want to know the price of the edit. What it cannot do
//! is a budget: `must_pass(true)` panics only on an instruction refusal, and a
//! 200 000 CU swap would land in the table without a sound.
//!
//! So the stdout table, the budget verdict and the failure itself are ours, and
//! they are built on the same cases that go to the bencher ([`cu::cases`]). The
//! budget as a pass condition also lives in `tests/budget.rs`: `cargo bench` does
//! not run in CI, `cargo test` does.
//!
//! # Where the markdown goes
//!
//! To `target/benches/compute_units.md`. Not next to the crate: `*.md` is in
//! `.gitignore`, so the file would not land in the repo anyway, and the delta is
//! needed locally — in CI there is no previous run.
//!
//! `harness = false` in `Cargo.toml` is mandatory: with the stock harness
//! `cargo bench` would look for `#[bench]`, which is still a nightly API.

use mollusk_svm_bencher::MolluskComputeUnitBencher;
use propamm_vault_tests::{cu, world};

fn main() {
    quieten_the_runtime_log();

    let cases = cu::cases();
    let measured = cu::measure_all(&cases);

    let report_dir = cu::report_dir();
    let mut bencher = MolluskComputeUnitBencher::new(world::mollusk())
        .must_pass(true)
        .out_dir(&report_dir);
    for case in &cases {
        bencher = bencher.bench((case.name, &case.instruction, &case.accounts));
    }
    bencher.execute();

    // The table is printed AFTER the bencher, not before: otherwise the output of
    // the run itself washes it away, and whoever looks at the tail of the log sees
    // the runtime log instead of the numbers all this was invoked for.
    println!(
        "\nInstruction CU (swap budget — {} CU):",
        cu::SWAP_BUDGET_CU
    );
    print!("{}", cu::table(&measured));
    println!("\ntable with delta: {report_dir}/compute_units.md");

    // Failure at the end, not on the first violation: when several pair variants
    // went over the bound, all of them need to be known — otherwise the next run
    // shows the next one, and so on one at a time.
    let overruns = cu::overruns(&measured);
    assert!(
        overruns.is_empty(),
        "over the SC-002 budget: {}",
        overruns
            .iter()
            .map(|m| format!("{} — {} CU", m.name, m.consumed))
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Remove the runtime's DEBUG log from the output.
///
/// `Mollusk` brings up `solana_logger` with its own default level, and every
/// call leaves a dozen `stable_log` lines. Nine cases give several hundred
/// lines in which the table drowns without a trace.
///
/// Someone else's `RUST_LOG` is left alone: `solana_logger` takes the environment
/// variable in preference to its default, so whoever needs the log — and it is
/// needed exactly when a case suddenly refused — enables it the usual
/// way.
fn quieten_the_runtime_log() {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "off");
    }
}
