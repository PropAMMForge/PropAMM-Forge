//! A thousand-swap campaign: SC-004 and half of SC-010.
//!
//! # What exactly is measured here
//!
//! **SC-004** — "0 swaps at a stale quote out of 1 000, of which at least 200
//! are deliberately stale". That is not a property of a single instruction: a
//! stale quote arises between engine ticks, and it can be checked only on a
//! sequence where the age really accumulates.
//!
//! **SC-010 partially** — "no swap takes the inventory past the hard bound".
//! The full requirement (10 000 swaps with hostile sequences) stays with T054;
//! here the invariant itself is checked, step by step, on every successful
//! swap.
//!
//! # Why the run is deterministic
//!
//! The same seed — the same thousand steps on any machine. A campaign that
//! differs every time says nothing on a green run: the next one could fail, and
//! there would be nothing to reproduce it with.
//!
//! # The composition guard
//!
//! A green campaign is not yet evidence by itself: a generator that yields only
//! refusals also gives "0 stale executed". So the composition of the run is
//! counted and checked explicitly — how many attempts were stale, how many swaps
//! really happened, which guards fired. This answers the same trap that led to
//! the success-rate measurement in T007.

use std::collections::BTreeMap;

use propamm_vault::errors::VaultError;
use propamm_vault::instructions::swap::SwapSide;
use propamm_vault_tests::rng::Xorshift;
use propamm_vault_tests::world::{MAX_QUOTE_AGE_SLOTS, MID_E9};
use propamm_vault_tests::World;

const STEPS: usize = 1_000;
/// Every fifth step is deliberately stale — exactly 200 out of 1 000.
const STALE_EVERY: usize = 5;
const SEED: u64 = 0x5EED_2026_0908_0001;

/// Maximum base leg of a step — one base unit.
///
/// The treasury holds a hundred, so no step eats a noticeable share of the
/// inventory: otherwise within a hundred steps the campaign would degenerate
/// into solid `InsufficientLiquidity` and say nothing about freshness.
const MAX_LEG: u64 = 1_000_000_000;

/// The skew bound for the duration of the campaign — 2 %, not the usual 30 %.
///
/// With a wide bound the SC-010 check would be **empty**: a random walk of a
/// thousand steps of one base unit each drifts a few percent from balance and
/// never reaches 30 %, so "no bound violation" would hold by itself. One base
/// asset shifts the skew by roughly 50 bps, so at 2 % the bound is really
/// reached — and starts refusing.
const CAMPAIGN_SKEW_BPS: u16 = 200;

#[test]
fn a_thousand_swaps_never_trade_on_a_stale_quote() {
    let mut world = World::ready();
    let tighten = propamm_vault_tests::ix::set_risk_limits(
        &world.owner,
        &world.vault,
        propamm_vault::instructions::authority::RiskLimits {
            max_quote_age_slots: MAX_QUOTE_AGE_SLOTS,
            max_skew_bps: CAMPAIGN_SKEW_BPS,
        },
    );
    world.must(&tighten);

    let mut rng = Xorshift::new(SEED);

    let mut stale_attempts = 0usize;
    let mut stale_executed = 0usize;
    let mut executed = 0usize;
    let mut rejections: BTreeMap<String, usize> = BTreeMap::new();
    let mut bound_violations = 0usize;
    let mut bound_rejections = 0usize;

    for step in 0..STEPS {
        // An engine tick: the price is posted at the current slot…
        world.quote();

        // …and then time passes. On every fifth step — more than the freshness
        // limit allows.
        let stale = step % STALE_EVERY == 0;
        let age = if stale {
            u64::from(MAX_QUOTE_AGE_SLOTS) + 1 + rng.below(40)
        } else {
            rng.below(u64::from(MAX_QUOTE_AGE_SLOTS) + 1)
        };
        world.warp_by(age);

        let side = if rng.below(2) == 0 {
            SwapSide::BaseToQuote
        } else {
            SwapSide::QuoteToBase
        };
        let amount_in = match side {
            SwapSide::BaseToQuote => rng.between(1_000_000, MAX_LEG),
            // Roughly the same base leg, converted at the mid.
            SwapSide::QuoteToBase => rng.between(150_000, MAX_LEG / 6),
        };

        let skew_before = propamm_quote::inventory_skew_bps(&world.inventory(), MID_E9)
            .expect("the skew before the swap does not compute");

        let outcome = world.swap(side, amount_in, 0);

        if stale {
            stale_attempts += 1;
        }

        if outcome.result.program_result.is_ok() {
            executed += 1;
            if stale {
                stale_executed += 1;
            }

            // SC-010: the bound limits swaps, not state. A swap is allowed either when
            // it stays within the bound, or when it reduces the skew.
            let skew_after = propamm_quote::inventory_skew_bps(&world.inventory(), MID_E9)
                .expect("the skew after the swap does not compute");
            let past_bound = skew_after.unsigned_abs() > u32::from(CAMPAIGN_SKEW_BPS);
            let worsened = skew_after.unsigned_abs() > skew_before.unsigned_abs();
            if past_bound && worsened {
                bound_violations += 1;
            }
        } else {
            *rejections
                .entry(format!("{:?}", outcome.result.program_result))
                .or_default() += 1;
            if matches!(
                outcome.result.program_result,
                mollusk_svm::result::ProgramResult::Failure(
                    solana_program_error::ProgramError::Custom(code)
                ) if code == u32::from(VaultError::InventoryBound)
            ) {
                bound_rejections += 1;
            }

            if stale {
                // A stale one has to be screened out by **the freshness guard specifically**.
                // If another guard screened it out, SC-004 is not measured: freshness
                // might as well not have fired.
                outcome.expect_error(VaultError::QuoteStale);
            }
        }
    }

    println!("steps: {STEPS}, executed: {executed}, stale attempts: {stale_attempts}");
    for (reason, count) in &rejections {
        println!("  refusals {count:>4}: {reason}");
    }

    // --- SC-004 ---
    assert_eq!(stale_executed, 0, "a swap at a stale quote");
    assert!(
        stale_attempts >= 200,
        "only {stale_attempts} deliberately stale attempts, at least 200 are needed"
    );

    // --- SC-010, partially ---
    assert_eq!(
        bound_violations, 0,
        "a swap took the inventory past the hard bound, deepening the skew"
    );

    // --- Composition guard of the run ---
    //
    // There are 800 fresh steps. If most of them were screened out, "zero stale
    // executed" would mean nothing: the stale ones would be screened out along
    // with everyone else, and the freshness guard would remain unchecked.
    assert!(
        executed >= 300,
        "the campaign degenerated: only {executed} swaps executed out of 800 fresh steps"
    );
    // And without this line the SC-010 check would be empty: "no bound violation"
    // holds by itself as long as the bound is unreachable.
    assert!(
        bound_rejections >= 50,
        "the hard inventory bound did not fire once during the run ({bound_rejections})"
    );
}

/// The same seed — the same result.
///
/// The run is shorter (a 1 000-step campaign twice is already noticeable time),
/// but sufficient: if the sequence depended on anything but the seed, the
/// divergence would show up within the first few dozen steps.
#[test]
fn the_campaign_is_reproducible() {
    let run = || {
        let mut world = World::ready();
        let mut rng = Xorshift::new(SEED);
        let mut trace = Vec::new();

        for _ in 0..50 {
            world.quote();
            world.warp_by(rng.below(u64::from(MAX_QUOTE_AGE_SLOTS) + 1));
            let side = if rng.below(2) == 0 {
                SwapSide::BaseToQuote
            } else {
                SwapSide::QuoteToBase
            };
            let amount_in = match side {
                SwapSide::BaseToQuote => rng.between(1_000_000, MAX_LEG),
                SwapSide::QuoteToBase => rng.between(150_000, MAX_LEG / 6),
            };
            let outcome = world.swap(side, amount_in, 0);
            trace.push((
                amount_in,
                format!("{:?}", outcome.result.program_result),
                world.inventory().base_amount,
            ));
        }
        trace
    };

    assert_eq!(run(), run());
}
