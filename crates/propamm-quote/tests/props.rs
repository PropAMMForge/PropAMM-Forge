//! Property tests of the `propamm-quote` invariants (T007).
//!
//! The tests in `compute_swap.rs` pin behaviour on specific numbers — they read
//! like a specification, but their coverage is narrow: the mutation check in
//! T006 showed that the ask rounding direction is held by exactly one example,
//! because on SOL/USDC numbers the division comes out exact. Here it is the
//! opposite: no chosen numbers, only statements that must hold across the whole range.
//!
//! # On the generators — the main thing in this file
//!
//! Statements about an executed swap take the form "if `Ok`, then …", and such a
//! test checks nothing if the generator almost never yields `Ok`. That is not a
//! hypothetical danger: the first edition of this file yielded **4 successful
//! swaps out of 4000** generated (57% screened out on freshness, 21% on the result
//! bound, 20% on overflow), i.e. six properties out of ten saw not a single
//! executed swap in a typical run and were green for no reason.
//!
//! So there are three generators here, with different roles:
//!
//! - [`feasible`] builds a **consistent** scenario: amounts bounded by what the
//!   vault can really pay out, a fresh slot, bounds lifted. 100% executes here, and
//!   that is what the statements about the swap arithmetic itself need.
//! - [`feasible_with_guards`] brings back live size, inventory and result
//!   bounds — 80% success, the rest are genuine guard refusals. The statement
//!   "success means every guard passed" stands on it; with the bounds lifted it
//!   would be checking an empty space.
//! - [`wide_params`] takes the whole domain — needed where the statement does
//!   not depend on the swap succeeding (side price, skew sign, absence of
//!   panics).
//!
//! Both success rates are pinned by the test
//! [`feasible_scenarios_mostly_succeed`], so the properties do not go empty
//! silently during the next guard edit.
//!
//! # Two kinds of statements
//!
//! - **Independent** — phrased in terms of the consequence, not the formula. The
//!   main one, [`vault_never_loses_value_at_mid`], catches swapped sides, the
//!   sign of the skew and any rounding towards the trader without knowing how
//!   the price is actually computed.
//! - **Those that restate the formula** — about the direction and tightness of
//!   rounding. They check not the formula (which they restate) but exactly which
//!   way an inexact division is truncated. There is no broader way to say "bid
//!   down, ask up" without restating the formula, and that is a deliberate limit.

use propamm_quote::{
    compute_swap, inventory_skew_bps, is_fresh, side_price_e9, Inventory, QuoteError, QuoteParams,
    Side, SwapRequest, SwapResult, BPS_DENOM, PRICE_SCALE,
};
use proptest::prelude::*;

/// Denominator of the skew × spread product — the same as in the crate.
const FACTOR_DENOM: i128 = (BPS_DENOM as i128) * (BPS_DENOM as i128);

/// Ceiling of the mid: 1e12 at scale 1e9 — a million quote units per one base
/// unit. Wider is not needed, and narrower than `u128` is needed so the
/// products in the statements themselves stay exact.
const MID_MAX: u128 = 1_000_000_000_000;

/// Ceiling of an inventory leg: 1e15 raw units — a million tokens at nine
/// decimals. Together with [`MID_MAX`] it keeps all intermediate products in
/// `u128` and the outputs in `u64`.
const INV_MAX: u64 = 1_000_000_000_000_000;

type Scenario = (QuoteParams, Inventory, SwapRequest, u64);

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::BaseToQuote), Just(Side::QuoteToBase)]
}

/// The whole parameter domain: spread up to 99.99%, skew up to ±99.99%.
///
/// Nobody posts such quotes, and that is exactly why they are here: rounding
/// has to be correct on them too.
fn wide_params() -> impl Strategy<Value = QuoteParams> {
    (
        1u128..=MID_MAX,
        0u16..BPS_DENOM,
        -9_999i16..=9_999i16,
        1u64..=INV_MAX,
        0u64..=1_000_000u64,
        0u32..=1_000u32,
        0u16..=BPS_DENOM,
    )
        .prop_map(
            |(
                mid_e9,
                spread_bps,
                skew_bps,
                max_size_base,
                quote_slot,
                max_quote_age_slots,
                max_skew_bps,
            )| QuoteParams {
                mid_e9,
                spread_bps,
                skew_bps,
                max_size_base,
                quote_slot,
                max_quote_age_slots,
                max_skew_bps,
            },
        )
}

fn inventory() -> impl Strategy<Value = Inventory> {
    (0u64..=INV_MAX, 0u64..=INV_MAX).prop_map(|(base_amount, quote_amount)| Inventory {
        base_amount,
        quote_amount,
    })
}

/// A consistent scenario in which the swap has every chance of happening.
///
/// The input is bounded by a fraction of what the vault can pay out at the worst
/// price estimate for itself; the slot is fresh; the result bound is zero. The price
/// estimate is taken **not** from the code under test but from the bounds that follow
/// from the spread and skew: at `spread ≤ 5%` and `|skew| ≤ 20%` the price factor lies
/// between 0.76 and 1.27. Otherwise the generator would adapt to a bug in `side_price_e9`.
fn feasible() -> impl Strategy<Value = Scenario> {
    (
        (
            1u128..=MID_MAX,
            // A zero spread as a separate branch, and that is not a whim. At a spread
            // of one basis point or more the spread margin covers a one-unit error,
            // and `vault_never_loses_value_at_mid` stops seeing a flipped payout
            // rounding — verified by mutation. At a zero spread there is no margin,
            // and any rounding up immediately makes the swap loss-making — that is
            // exactly what the check has to see.
            prop_oneof![1 => Just(0u16), 4 => 1u16..=500u16],
            -2_000i16..=2_000i16,
            0u64..=1_000_000u64,
            1u32..=1_000u32,
        ),
        (
            1u64..=INV_MAX,
            1u64..=INV_MAX,
            side(),
            1u64..=100u64,
            0u32..=1_000u32,
        ),
    )
        .prop_map(
            |(
                (mid_e9, spread_bps, skew_bps, quote_slot, max_quote_age_slots),
                (base_amount, quote_amount, side, percent, age),
            )| {
                let price_low = (mid_e9.saturating_mul(76) / 100).max(1);
                let price_high = mid_e9.saturating_mul(127) / 100 + 1;

                // How much can be fed in so that the vault definitely has enough to
                // settle even at the worst price for itself.
                let payable = match side {
                    Side::BaseToQuote => u128::from(quote_amount) * PRICE_SCALE / price_high,
                    Side::QuoteToBase => u128::from(base_amount) * price_low / PRICE_SCALE,
                };
                let payable = u64::try_from(payable.min(u128::from(INV_MAX))).unwrap_or(INV_MAX);
                let amount_in = (payable / 100 * percent).max(1);

                let params = QuoteParams {
                    mid_e9,
                    spread_bps,
                    skew_bps,
                    // The size and inventory bounds deliberately do not interfere here: they
                    // have their own statements below, built on an already executed swap,
                    // and there they are not decorative.
                    max_size_base: u64::MAX,
                    quote_slot,
                    max_quote_age_slots,
                    max_skew_bps: BPS_DENOM,
                };
                let inventory = Inventory {
                    base_amount,
                    quote_amount,
                };
                let request = SwapRequest {
                    side,
                    amount_in,
                    min_amount_out: 0,
                };
                let slot = quote_slot + u64::from(age.min(max_quote_age_slots));

                (params, inventory, request, slot)
            },
        )
}

/// The same consistent scenario, but with **live** size, inventory and result
/// bounds instead of lifted ones.
///
/// [`feasible`] lifts these three bounds on purpose so that the computation
/// itself executes; here they come back, and `Ok` stops being automatic. The
/// difference is not cosmetic: the one test that asserts "success means ALL
/// guards passed" would be checking an empty space with the bounds lifted.
/// The weights in `prop_oneof` were chosen by measurement, not by eye: uniform
/// bounds gave 13% success, because each cut off its own quarter. Freshness is
/// not touched here at all — it has a two-sided statement
/// [`staleness_is_decided_by_the_age_limit_alone`], and adding it here would
/// only eat the coverage of the remaining guards.
fn feasible_with_guards() -> impl Strategy<Value = Scenario> {
    (
        feasible(),
        prop_oneof![1 => Just(u64::MAX), 1 => 1u64..=INV_MAX],
        prop_oneof![3 => Just(BPS_DENOM), 1 => 0u16..=BPS_DENOM],
        prop_oneof![9 => Just(0u64), 1 => 1u64..=INV_MAX],
    )
        .prop_map(
            |((p, inv, req, slot), max_size_base, max_skew_bps, min_amount_out)| {
                (
                    QuoteParams {
                        max_size_base,
                        max_skew_bps,
                        ..p
                    },
                    inv,
                    SwapRequest {
                        min_amount_out,
                        ..req
                    },
                    slot,
                )
            },
        )
}

/// Inventory value at the mid, in the scale of [`PRICE_SCALE`].
fn value_at_mid(inventory: &Inventory, mid_e9: u128) -> u128 {
    let base = u128::from(inventory.base_amount)
        .checked_mul(mid_e9)
        .expect("the generator keeps the legs within u128");
    let quote = u128::from(inventory.quote_amount)
        .checked_mul(PRICE_SCALE)
        .expect("the generator keeps the legs within u128");
    base.checked_add(quote).expect("legs sum within u128")
}

/// Thresholds of the successful-swap share guarded by
/// [`feasible_scenarios_mostly_succeed`]. Both come from an actual measurement
/// with a downward margin: `feasible` gives 1.000 (bounds lifted, pure arithmetic
/// remains), `feasible_with_guards` 0.798, and the rest there are live guard
/// refusals (`InventoryBound` 434, `SlippageExceeded` 367, `SizeExceeded` 8 out of 4 000).
const MIN_SUCCESS_RATE: f64 = 0.95;
const MIN_GUARDED_SUCCESS_RATE: f64 = 0.60;

proptest! {
    // ── Independent statements ─────────────────────────────────────────────

    /// The project's main invariant in its most honest form: no swap makes the
    /// vault poorer if both legs are valued at the same mid.
    ///
    /// The statement knows nothing about how the price is computed — so it catches
    /// swapped sides, an extra sign in the skew and any rounding towards the
    /// trader. The skew is zeroed here: with it the vault deliberately quotes worse
    /// than the mid to accumulate the asset it needs, and the value at the mid may
    /// fall — that is not a bug, it is what skew is for.
    #[test]
    fn vault_never_loses_value_at_mid(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;
        let p = QuoteParams { skew_bps: 0, ..p };

        if let Ok(r) = compute_swap(&p, &inv, &req, slot) {
            prop_assert!(
                value_at_mid(&r.inventory_after, p.mid_e9) >= value_at_mid(&inv, p.mid_e9),
                "the swap made the vault poorer: {:?} → {:?} at mid {}",
                inv, r.inventory_after, p.mid_e9,
            );
        }
    }

    /// Inventory changes by exactly the two legs of the order and loses nothing on the way.
    #[test]
    fn inventory_moves_by_exactly_the_two_legs(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(r) = compute_swap(&p, &inv, &req, slot) {
            let after = r.inventory_after;
            match req.side {
                Side::BaseToQuote => {
                    prop_assert_eq!(after.base_amount, inv.base_amount + r.amount_in);
                    prop_assert_eq!(after.quote_amount, inv.quote_amount - r.amount_out);
                }
                Side::QuoteToBase => {
                    prop_assert_eq!(after.quote_amount, inv.quote_amount + r.amount_in);
                    prop_assert_eq!(after.base_amount, inv.base_amount - r.amount_out);
                }
            }
            prop_assert_eq!(r.amount_in, req.amount_in, "there is no partial fill");
        }
    }

    /// Success means **all** guards passed, not the one the check reached first.
    /// The order of checks is an implementation detail; this statement is about
    /// the outcome.
    #[test]
    fn success_implies_every_guard_held(scenario in feasible_with_guards()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(r) = compute_swap(&p, &inv, &req, slot) {
            prop_assert!(is_fresh(&p, slot), "FR-007");
            prop_assert!(r.amount_out >= req.min_amount_out, "FR-009");
            prop_assert!(r.amount_out > 0, "an output truncated to zero confiscates the input");

            let base_leg = match req.side {
                Side::BaseToQuote => r.amount_in,
                Side::QuoteToBase => r.amount_out,
            };
            prop_assert!(base_leg <= p.max_size_base, "FR-008 is measured by the base leg");

            // FR-026 in the wording chosen in T006: the bound limits swaps, not state,
            // so a swap that REDUCES an already excessive skew is allowed.
            let before = inventory_skew_bps(&inv, p.mid_e9).unwrap();
            prop_assert!(
                r.skew_bps_after.unsigned_abs() <= u32::from(p.max_skew_bps)
                    || r.skew_bps_after.unsigned_abs() <= before.unsigned_abs(),
                "skew {} → {} past the bound {}",
                before, r.skew_bps_after, p.max_skew_bps,
            );
        }
    }

    /// More in is never less out. Catches any break of monotonicity a router
    /// would exploit against the vault.
    #[test]
    fn output_is_monotone_in_input(scenario in feasible(), divisor in 2u64..=64u64) {
        let (p, inv, req, slot) = scenario;
        let smaller = (req.amount_in / divisor).max(1);

        let quote_for = |amount_in| compute_swap(
            &p, &inv,
            &SwapRequest { amount_in, ..req },
            slot,
        );

        if let (Ok(a), Ok(b)) = (quote_for(smaller), quote_for(req.amount_in)) {
            prop_assert!(a.amount_out <= b.amount_out, "{} → {:?}, {} → {:?}",
                smaller, a.amount_out, req.amount_in, b.amount_out);
        }
    }

    /// The declared result bound affects only whether the swap happens — not its
    /// numbers. This is SC-006 in miniature: "compute" and "execute" are one call,
    /// and the second has no right to compute differently.
    #[test]
    fn declared_limit_does_not_change_the_numbers(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(q) = compute_swap(&p, &inv, &req, slot) {
            let executed = compute_swap(
                &p, &inv,
                &SwapRequest { min_amount_out: q.amount_out, ..req },
                slot,
            );
            prop_assert_eq!(executed, Ok(q));
        }
    }

    /// A bound above the computed amount always refuses the swap — by one unit too (FR-009).
    #[test]
    fn a_limit_above_the_quote_always_rejects(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(q) = compute_swap(&p, &inv, &req, slot) {
            let too_greedy = SwapRequest { min_amount_out: q.amount_out + 1, ..req };
            prop_assert_eq!(
                compute_swap(&p, &inv, &too_greedy, slot),
                Err(QuoteError::SlippageExceeded)
            );
        }
    }

    /// A size bound cut below the base leg refuses the swap entirely — there is
    /// no partial fill (FR-008).
    #[test]
    fn a_size_limit_below_the_base_leg_always_rejects(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(q) = compute_swap(&p, &inv, &req, slot) {
            let base_leg = match req.side {
                Side::BaseToQuote => q.amount_in,
                Side::QuoteToBase => q.amount_out,
            };
            prop_assume!(base_leg > 0);

            let tightened = QuoteParams { max_size_base: base_leg - 1, ..p };
            prop_assert_eq!(
                compute_swap(&tightened, &inv, &req, slot),
                Err(QuoteError::SizeExceeded)
            );
        }
    }

    /// An inventory bound cut below the skew the swap leads to refuses it — but
    /// only when the swap makes the skew worse (FR-026).
    #[test]
    fn a_bound_below_a_worsening_skew_always_rejects(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(q) = compute_swap(&p, &inv, &req, slot) {
            let before = inventory_skew_bps(&inv, p.mid_e9).unwrap().unsigned_abs();
            let after = q.skew_bps_after.unsigned_abs();
            prop_assume!(after > before && after > 0);

            let tightened = QuoteParams {
                max_skew_bps: u16::try_from(after - 1).unwrap_or(0),
                ..p
            };
            prop_assert_eq!(
                compute_swap(&tightened, &inv, &req, slot),
                Err(QuoteError::InventoryBound)
            );
        }
    }

    /// Freshness is exactly `age <= max_quote_age_slots`, with no off-by-one and no
    /// overflow on a slot smaller than `quote_slot`.
    ///
    /// The statement is two-sided, so it does not go empty regardless of which slots
    /// the generator yields: parameters from [`wide_params`] are always within the
    /// domain, so the side price computes and `QuoteStale` is not displaced by another error.
    #[test]
    fn staleness_is_decided_by_the_age_limit_alone(
        p in wide_params(),
        inv in inventory(),
        s in side(),
        amount_in in 0u64..=INV_MAX,
        slot in any::<u64>(),
    ) {
        let req = SwapRequest { side: s, amount_in, min_amount_out: 0 };
        let stale = matches!(compute_swap(&p, &inv, &req, slot), Err(QuoteError::QuoteStale));
        prop_assert_eq!(stale, !is_fresh(&p, slot));
    }

    /// The sign of the skew reads unambiguously, empty inventory has no skew, and
    /// the magnitude never exceeds 100%.
    #[test]
    fn skew_sign_follows_the_heavier_leg(inv in inventory(), mid_e9 in 1u128..=MID_MAX) {
        let skew = inventory_skew_bps(&inv, mid_e9).unwrap();
        let base_value = u128::from(inv.base_amount) * mid_e9;
        let quote_value = u128::from(inv.quote_amount) * PRICE_SCALE;

        prop_assert!(skew.unsigned_abs() <= u32::from(BPS_DENOM));
        match base_value.cmp(&quote_value) {
            core::cmp::Ordering::Greater => prop_assert!(skew > 0),
            core::cmp::Ordering::Less => prop_assert!(skew < 0),
            core::cmp::Ordering::Equal => prop_assert_eq!(skew, 0),
        }
    }

    // ── Statements that restate the formula ────────────────────────────────

    /// The side price is truncated in the vault's favour and by no more than one
    /// unit: bid down, ask up. The formula is restated here deliberately, because
    /// what is checked is the direction of truncation, not the way the number is obtained.
    #[test]
    fn side_price_rounds_toward_the_vault_and_stays_tight(p in wide_params()) {
        let skewed = i128::from(BPS_DENOM) + i128::from(p.skew_bps);
        let spread = i128::from(p.spread_bps);
        let mid = i128::try_from(p.mid_e9).unwrap();

        let bid = i128::try_from(side_price_e9(&p, Side::BaseToQuote).unwrap()).unwrap();
        let exact_bid = mid * skewed * (i128::from(BPS_DENOM) - spread);
        prop_assert!(bid * FACTOR_DENOM <= exact_bid, "bid truncated upwards");
        prop_assert!((bid + 1) * FACTOR_DENOM > exact_bid, "bid truncated by more than one unit");

        let ask = i128::try_from(side_price_e9(&p, Side::QuoteToBase).unwrap()).unwrap();
        let exact_ask = mid * skewed * (i128::from(BPS_DENOM) + spread);
        prop_assert!(ask * FACTOR_DENOM >= exact_ask, "ask truncated downwards");
        prop_assert!((ask - 1) * FACTOR_DENOM < exact_ask, "ask raised by more than one unit");
    }

    /// The payout is truncated downwards in both directions — and also by no more
    /// than one unit, otherwise "in the vault's favour" would turn into "anything goes".
    #[test]
    fn payout_rounds_down_and_stays_tight(scenario in feasible()) {
        let (p, inv, req, slot) = scenario;

        if let Ok(r) = compute_swap(&p, &inv, &req, slot) {
            let amount_in = u128::from(r.amount_in);
            let amount_out = u128::from(r.amount_out);
            let (numerator, denominator) = match req.side {
                Side::BaseToQuote => (amount_in * r.price_e9, PRICE_SCALE),
                Side::QuoteToBase => (amount_in * PRICE_SCALE, r.price_e9),
            };
            prop_assert!(amount_out * denominator <= numerator, "payout rounded up");
            prop_assert!((amount_out + 1) * denominator > numerator, "payout truncated too deep");
        }
    }

    // ── Behaviour across the whole type range ──────────────────────────────

    /// No input crashes the computation or yields a silently wrapped number: anything
    /// on the input is either `Ok` or a named error.
    ///
    /// The generator here is unbounded, including `mid_e9` near the `u128` ceiling,
    /// so `Ok` practically never happens — and that is not a flaw of the test but
    /// its point: an overflow must be an error, not a result. Meaningful statements
    /// about an executed swap live above, on [`feasible`].
    #[test]
    fn arbitrary_input_never_panics_and_never_wraps(
        mid_e9 in any::<u128>(),
        spread_bps in any::<u16>(),
        skew_bps in any::<i16>(),
        max_size_base in any::<u64>(),
        quote_slot in any::<u64>(),
        max_quote_age_slots in any::<u32>(),
        max_skew_bps in any::<u16>(),
        base_amount in any::<u64>(),
        quote_amount in any::<u64>(),
        s in side(),
        amount_in in any::<u64>(),
        min_amount_out in any::<u64>(),
        slot in any::<u64>(),
    ) {
        let p = QuoteParams {
            mid_e9, spread_bps, skew_bps, max_size_base,
            quote_slot, max_quote_age_slots, max_skew_bps,
        };
        let inv = Inventory { base_amount, quote_amount };
        let req = SwapRequest { side: s, amount_in, min_amount_out };

        if let Ok(r) = compute_swap(&p, &inv, &req, slot) {
            prop_assert!(r.amount_out > 0);
            prop_assert!(r.amount_out >= req.min_amount_out);
            prop_assert!(is_fresh(&p, slot));
            prop_assert_eq!(r.amount_in, req.amount_in);
        }
    }
}

/// A guard against empty tests.
///
/// Half of the statements above take the form "if `Ok`, then …" and silently
/// go green when the generator stops yielding executed swaps. That is exactly
/// what happened to the first edition of this file: 4 successes out of 4000.
/// The test measures the success rate of [`feasible`] and fails as soon as it
/// drops — so that the next guard tightening breaks a test rather than silently removing coverage.
#[test]
fn feasible_scenarios_mostly_succeed() {
    let plain = success_rate("feasible", feasible());
    let guarded = success_rate("feasible_with_guards", feasible_with_guards());

    assert!(
        plain.0 >= MIN_SUCCESS_RATE && guarded.0 >= MIN_GUARDED_SUCCESS_RATE,
        "consistent scenarios stopped executing — statements of the form \"if Ok\" \
         have become empty. Fix the generator or the guard, not the threshold.\n\
         feasible:             {:.3} (need ≥ {MIN_SUCCESS_RATE}), refusals {:?}\n\
         feasible_with_guards: {:.3} (need ≥ {MIN_GUARDED_SUCCESS_RATE}), refusals {:?}",
        plain.0,
        plain.1,
        guarded.0,
        guarded.1,
    );
}

type Refusals = std::collections::BTreeMap<&'static str, usize>;

fn success_rate(_label: &str, strategy: impl Strategy<Value = Scenario>) -> (f64, Refusals) {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    const SAMPLES: usize = 4_000;
    let mut runner = TestRunner::deterministic();

    let mut succeeded = 0usize;
    let mut refusals = Refusals::new();
    for _ in 0..SAMPLES {
        let (p, inv, req, slot) = strategy.new_tree(&mut runner).unwrap().current();
        match compute_swap(&p, &inv, &req, slot) {
            Ok(SwapResult { .. }) => succeeded += 1,
            Err(e) => *refusals.entry(name_of(e)).or_default() += 1,
        }
    }

    (succeeded as f64 / SAMPLES as f64, refusals)
}

fn name_of(error: QuoteError) -> &'static str {
    match error {
        QuoteError::QuoteNotSet => "QuoteNotSet",
        QuoteError::InvalidParams => "InvalidParams",
        QuoteError::QuoteStale => "QuoteStale",
        QuoteError::SizeExceeded => "SizeExceeded",
        QuoteError::SlippageExceeded => "SlippageExceeded",
        QuoteError::InventoryBound => "InventoryBound",
        QuoteError::InsufficientLiquidity => "InsufficientLiquidity",
        QuoteError::AmountTooSmall => "AmountTooSmall",
        QuoteError::Overflow => "Overflow",
    }
}
