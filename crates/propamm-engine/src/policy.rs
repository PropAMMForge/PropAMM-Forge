//! The update rule: when a fresh decision is worth a transaction (FR-011), and
//! the check that refuses to start an engine whose heartbeat cannot keep the
//! quote alive (FR-011a).
//!
//! # Two triggers
//!
//! A quote is posted again when either side of the model's new quote has moved
//! away from the side on the book by **more** than the threshold, and in any
//! case once the heartbeat interval has passed since the last post, even if
//! nothing moved: otherwise the on-chain freshness limit (FR-007) takes the
//! quote off the routes by itself.
//!
//! # Why the sides, not the feed mid
//!
//! FR-011 speaks of the external price against the posted mid. A feed move of
//! `d` bps moves both sides by `d` bps, so for a moving market this is the same
//! trigger, and SC-003 is measured the same way. But the sides also move when
//! the market does not: the skew changes after swaps have shifted the
//! inventory, and an external model widens the spread when it sees volatility.
//! Measured on the mid alone, both would wait for the heartbeat, and until then
//! the vault would keep trading at the old skew — exactly the direction it is
//! trying to unwind.
//!
//! The size is not compared. It is not a price: a stale size cannot sell at a
//! wrong price, and the on-chain skew bound (FR-026) caps how far any size can
//! push the inventory.
//!
//! # What the rule compares against
//!
//! The last quote **sent**, not the last one landed: while a post is in flight,
//! the same move would otherwise be posted again on every price until it lands.
//! If the send fails, the caller says so with [`Rule::forget`], and the next
//! quote goes out whatever it is. The heartbeat counts from the slot the post
//! was decided in, which is no later than the `quote_slot` the chain stamps on
//! it — so the heartbeat can only come early, never late.
//!
//! # The whole road must fit (FR-011a)
//!
//! A heartbeat that is merely shorter than the freshness limit is not enough.
//! The old quote has to stay fresh until its replacement **lands**, and the
//! replacement first waits for the model and then for the chain:
//!
//! ```text
//! heartbeat + ⌈model budget in slots⌉ + LANDING_SLOTS ≤ max quote age
//! ```
//!
//! A configuration where that is not so is refused before the engine starts,
//! instead of showing up as the AMM dropping out of routes (FR-011a). The
//! freshness check on chain is inclusive, so the equality still leaves no gap.
//!
//! The sum has no room for waiting on the feed: when [`Rule::due_slot`] comes,
//! the loop asks the model on the latest live price rather than the next one.
//! A model that skips the heartbeat tick (FR-015b) is not covered either, by
//! design — the quote then expires on chain on its own (T030).

use std::fmt;

use propamm_quote::{side_price_e9, QuoteError, QuoteParams, Side, BPS_DENOM};
use thiserror::Error;

use crate::feed::SLOT_DURATION;
use crate::model::Quote;
use crate::tick::Budget;

/// Slots a post may take to land after it is sent — the SC-003 target, p95.
pub const LANDING_SLOTS: u32 = 2;

/// An update rule the engine refuses to start with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RuleConfigError {
    #[error("the deviation threshold is zero: every new price would be a transaction")]
    ZeroThreshold,
    #[error(
        "the deviation threshold of {threshold_bps} bps is not below a whole (10000 bps): \
         a bid cannot move that far"
    )]
    ThresholdTooWide { threshold_bps: u16 },
    #[error("the heartbeat interval is zero: every decision would be a transaction")]
    ZeroHeartbeat,
    #[error(
        "the heartbeat of {heartbeat_slots} slots, plus {budget_slots} slot(s) of model budget \
         and {landing_slots} to land, is {total_slots} slots — past the on-chain freshness \
         limit of {max_age_slots}: the quote would expire before its replacement lands and \
         the AMM would drop out of routes"
    )]
    HeartbeatDoesNotFit {
        heartbeat_slots: u32,
        budget_slots: u32,
        landing_slots: u32,
        total_slots: u64,
        max_age_slots: u32,
    },
}

/// Why a quote goes out this time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The rule does not know what is on the book: nothing sent by this run
    /// yet, or the last send failed.
    Unknown,
    /// The quote was withdrawn and there is a price again.
    Resumed,
    /// A side moved by this many bps of the posted side (rounded down).
    Moved { bps: u32 },
    /// Nothing moved enough, but the heartbeat interval is up.
    Heartbeat { age_slots: u64 },
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("the quote on the book is not known"),
            Self::Resumed => f.write_str("quoting again after a withdrawal"),
            Self::Moved { bps } => write!(f, "a side moved {bps} bps"),
            Self::Heartbeat { age_slots } => {
                write!(f, "heartbeat: {age_slots} slots since the last post")
            }
        }
    }
}

/// What to do with the model's quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Send it.
    Post(Why),
    /// The quote on the book is still good enough.
    Hold,
}

/// What the rule last sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    Unknown,
    Posted {
        bid_e9: u128,
        ask_e9: u128,
        slot: u64,
    },
    Withdrawn,
}

/// The hybrid update rule (FR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    threshold_bps: u16,
    heartbeat_slots: u32,
    last: Last,
}

impl Rule {
    /// A rule with no cross-check. Prefer [`Rule::checked`].
    #[must_use]
    pub const fn new(threshold_bps: u16, heartbeat_slots: u32) -> Self {
        Self {
            threshold_bps,
            heartbeat_slots,
            last: Last::Unknown,
        }
    }

    /// The same, refusing a rule the engine must not start with (FR-011a).
    ///
    /// `max_quote_age_slots` is the vault's freshness limit (FR-007) and
    /// `budget` the model's (FR-015b); see the module docs for the sum they
    /// have to fit in.
    ///
    /// # Errors
    ///
    /// [`RuleConfigError`] — a zero threshold or heartbeat, a threshold of a
    /// whole or more, or a heartbeat whose replacement would land after the
    /// quote it replaces has expired.
    pub fn checked(
        threshold_bps: u16,
        heartbeat_slots: u32,
        max_quote_age_slots: u32,
        budget: Budget,
    ) -> Result<Self, RuleConfigError> {
        if threshold_bps == 0 {
            return Err(RuleConfigError::ZeroThreshold);
        }
        if threshold_bps >= BPS_DENOM {
            return Err(RuleConfigError::ThresholdTooWide { threshold_bps });
        }
        if heartbeat_slots == 0 {
            return Err(RuleConfigError::ZeroHeartbeat);
        }
        let budget_slots =
            u32::try_from(budget.get().as_nanos().div_ceil(SLOT_DURATION.as_nanos()))
                .unwrap_or(u32::MAX);
        let total_slots =
            u64::from(heartbeat_slots) + u64::from(budget_slots) + u64::from(LANDING_SLOTS);
        if total_slots > u64::from(max_quote_age_slots) {
            return Err(RuleConfigError::HeartbeatDoesNotFit {
                heartbeat_slots,
                budget_slots,
                landing_slots: LANDING_SLOTS,
                total_slots,
                max_age_slots: max_quote_age_slots,
            });
        }
        Ok(Self::new(threshold_bps, heartbeat_slots))
    }

    /// `QUOTE_DEVIATION_THRESHOLD_BPS`.
    #[must_use]
    pub const fn threshold_bps(&self) -> u16 {
        self.threshold_bps
    }

    /// `QUOTE_HEARTBEAT_SLOTS`.
    #[must_use]
    pub const fn heartbeat_slots(&self) -> u32 {
        self.heartbeat_slots
    }

    /// The slot by which the quote on the book has to be posted again, even if
    /// nothing moved.
    ///
    /// `None` when there is no deadline: nothing has been posted yet (the next
    /// quote goes out at once), or the quote is withdrawn.
    #[must_use]
    pub fn due_slot(&self) -> Option<u64> {
        match self.last {
            Last::Posted { slot, .. } => Some(slot.saturating_add(u64::from(self.heartbeat_slots))),
            Last::Unknown | Last::Withdrawn => None,
        }
    }

    /// The model quoted `quote` at `now_slot`: send it, or hold the one on the book?
    ///
    /// [`Update::Post`] is taken as sent — the next quote is compared against
    /// this one. If the send fails, call [`Rule::forget`].
    ///
    /// # Errors
    ///
    /// [`QuoteError`] when `quote` has no side prices at all (the tick's
    /// [`Quote::check`] catches that first); nothing is recorded.
    pub fn on_quote(&mut self, quote: &Quote, now_slot: u64) -> Result<Update, QuoteError> {
        let (bid_e9, ask_e9) = sides(quote)?;
        let why = match self.last {
            Last::Unknown => Why::Unknown,
            Last::Withdrawn => Why::Resumed,
            Last::Posted {
                bid_e9: posted_bid,
                ask_e9: posted_ask,
                slot,
            } => {
                let moved = [(posted_bid, bid_e9), (posted_ask, ask_e9)]
                    .into_iter()
                    .filter_map(|(old, new)| moved_bps(old, new, self.threshold_bps))
                    .max();
                let age_slots = now_slot.saturating_sub(slot);
                if let Some(bps) = moved {
                    Why::Moved { bps }
                } else if age_slots >= u64::from(self.heartbeat_slots) {
                    Why::Heartbeat { age_slots }
                } else {
                    return Ok(Update::Hold);
                }
            }
        };
        self.last = Last::Posted {
            bid_e9,
            ask_e9,
            slot: now_slot,
        };
        Ok(Update::Post(why))
    }

    /// The quote has to come off the book (the feed or the model says so):
    /// `true` if a withdrawal has to be sent.
    ///
    /// Edge-triggered, like the feed's own withdrawal: the model says
    /// "withdraw" on every tick, and only the first one is a transaction. A
    /// rule that knows nothing withdraws too — a quote from an earlier run of
    /// the engine may still be on the book.
    pub fn on_withdraw(&mut self) -> bool {
        if self.last == Last::Withdrawn {
            return false;
        }
        self.last = Last::Withdrawn;
        true
    }

    /// The last send failed: the rule no longer knows what is on the book, and
    /// the next quote or withdrawal goes out whatever it is.
    pub fn forget(&mut self) {
        self.last = Last::Unknown;
    }
}

/// Bid and ask of `quote`, exactly as the program would derive them.
fn sides(quote: &Quote) -> Result<(u128, u128), QuoteError> {
    // Only the price fields matter to `side_price_e9`; the rest are the vault's.
    let params = QuoteParams {
        mid_e9: quote.mid_e9,
        spread_bps: quote.spread_bps,
        skew_bps: quote.skew_bps,
        max_size_base: quote.max_size_base,
        quote_slot: 0,
        max_quote_age_slots: 0,
        max_skew_bps: 0,
    };
    Ok((
        side_price_e9(&params, Side::BaseToQuote)?,
        side_price_e9(&params, Side::QuoteToBase)?,
    ))
}

/// How far `new` is from `old` in bps of `old`, rounded down — if that is
/// strictly more than `threshold_bps`. The comparison itself is exact.
fn moved_bps(old: u128, new: u128, threshold_bps: u16) -> Option<u32> {
    let diff = old.abs_diff(new);
    let Some(scaled) = diff.checked_mul(u128::from(BPS_DENOM)) else {
        return Some(u32::MAX);
    };
    // Saturating is safe: if `old × threshold` overflows, it is above `scaled` anyway.
    if scaled <= old.saturating_mul(u128::from(threshold_bps)) {
        return None;
    }
    let bps = scaled.checked_div(old).unwrap_or(u128::MAX);
    Some(u32::try_from(bps).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const THRESHOLD: u16 = 5;
    const HEARTBEAT: u32 = 12;
    const MAX_AGE: u32 = 25;
    const BUDGET: Budget = Budget::new(Duration::from_millis(50));

    /// 0.15 in `PRICE_SCALE`, with a 10 bps half-spread: bid 149 850 000, ask 150 150 000.
    fn quote(mid_e9: u128) -> Quote {
        Quote {
            mid_e9,
            spread_bps: 10,
            skew_bps: 0,
            max_size_base: 1_000,
        }
    }

    const MID: u128 = 150_000_000;

    fn rule() -> Rule {
        Rule::checked(THRESHOLD, HEARTBEAT, MAX_AGE, BUDGET).expect("the .env.example defaults")
    }

    /// A rule with `quote(MID)` on the book, posted at slot 100.
    fn posted() -> Rule {
        let mut rule = rule();
        assert_eq!(
            rule.on_quote(&quote(MID), 100),
            Ok(Update::Post(Why::Unknown))
        );
        rule
    }

    fn fits(heartbeat: u32, budget_ms: u64) -> Result<Rule, RuleConfigError> {
        Rule::checked(
            THRESHOLD,
            heartbeat,
            MAX_AGE,
            Budget::new(Duration::from_millis(budget_ms)),
        )
    }

    #[test]
    fn the_env_example_defaults_are_accepted() {
        let rule = rule();
        assert_eq!(rule.threshold_bps(), THRESHOLD);
        assert_eq!(rule.heartbeat_slots(), HEARTBEAT);
    }

    #[test]
    fn a_threshold_or_heartbeat_that_cannot_work_is_refused() {
        assert_eq!(
            Rule::checked(0, HEARTBEAT, MAX_AGE, BUDGET),
            Err(RuleConfigError::ZeroThreshold)
        );
        assert_eq!(
            Rule::checked(BPS_DENOM, HEARTBEAT, MAX_AGE, BUDGET),
            Err(RuleConfigError::ThresholdTooWide {
                threshold_bps: BPS_DENOM
            })
        );
        assert!(Rule::checked(BPS_DENOM - 1, HEARTBEAT, MAX_AGE, BUDGET).is_ok());
        assert_eq!(
            Rule::checked(THRESHOLD, 0, MAX_AGE, BUDGET),
            Err(RuleConfigError::ZeroHeartbeat)
        );
    }

    #[test]
    fn a_heartbeat_not_shorter_than_the_freshness_limit_is_refused() {
        // The letter of FR-011a.
        assert!(matches!(
            fits(MAX_AGE, 50),
            Err(RuleConfigError::HeartbeatDoesNotFit { .. })
        ));
    }

    #[test]
    fn the_replacement_has_to_land_before_the_quote_expires() {
        // 24 < 25 passes the letter of FR-011a, but 24 + 1 + 2 lands at 27.
        assert_eq!(
            fits(24, 50),
            Err(RuleConfigError::HeartbeatDoesNotFit {
                heartbeat_slots: 24,
                budget_slots: 1,
                landing_slots: LANDING_SLOTS,
                total_slots: 27,
                max_age_slots: MAX_AGE,
            })
        );
        // 22 + 1 + 2 = 25: lands in the last slot the old quote is fresh in.
        assert!(fits(22, 50).is_ok());
        assert!(fits(23, 50).is_err());
    }

    #[test]
    fn the_budget_counts_in_whole_slots_rounded_up() {
        // Exactly one slot of budget is one slot; a millisecond more is two.
        assert!(fits(22, 400).is_ok());
        assert!(matches!(
            fits(22, 401),
            Err(RuleConfigError::HeartbeatDoesNotFit {
                budget_slots: 2,
                ..
            })
        ));
        assert!(fits(21, 401).is_ok());
    }

    #[test]
    fn a_budget_past_u32_slots_saturates_instead_of_wrapping() {
        let budget = Budget::new(Duration::from_secs(u64::MAX));
        assert!(matches!(
            Rule::checked(THRESHOLD, 1, u32::MAX, budget),
            Err(RuleConfigError::HeartbeatDoesNotFit {
                budget_slots: u32::MAX,
                ..
            })
        ));
    }

    #[test]
    fn the_first_quote_goes_out_and_the_same_quote_is_held() {
        let mut rule = posted();
        assert_eq!(rule.on_quote(&quote(MID), 101), Ok(Update::Hold));
        assert_eq!(rule.due_slot(), Some(100 + u64::from(HEARTBEAT)));
    }

    #[test]
    fn a_move_of_exactly_the_threshold_is_held_and_one_unit_more_is_posted() {
        // +5 bps on the mid moves both sides by exactly 5 bps:
        // bid 149 850 000 → 149 924 925, ask 150 150 000 → 150 225 075.
        let mut rule = posted();
        assert_eq!(rule.on_quote(&quote(150_075_000), 101), Ok(Update::Hold));

        let mut rule = posted();
        assert_eq!(
            rule.on_quote(&quote(150_075_001), 101),
            Ok(Update::Post(Why::Moved { bps: 5 }))
        );
    }

    #[test]
    fn a_fall_counts_like_a_rise() {
        let mut rule = posted();
        assert_eq!(
            rule.on_quote(&quote(MID - MID / 1_000), 101),
            Ok(Update::Post(Why::Moved { bps: 10 }))
        );
    }

    #[test]
    fn the_move_is_measured_against_the_quote_sent_not_the_last_one_seen() {
        let mut rule = posted();
        // +3 bps, then +3 bps more: each step is under the threshold, the sum is not.
        assert_eq!(rule.on_quote(&quote(150_045_000), 101), Ok(Update::Hold));
        assert_eq!(
            rule.on_quote(&quote(150_090_000), 102),
            Ok(Update::Post(Why::Moved { bps: 6 }))
        );
        // And now the reference is the new one.
        assert_eq!(rule.on_quote(&quote(150_090_000), 103), Ok(Update::Hold));
        assert_eq!(rule.due_slot(), Some(102 + u64::from(HEARTBEAT)));
    }

    #[test]
    fn a_new_skew_is_posted_while_the_mid_stands_still() {
        // Swaps shifted the inventory; the feed did not move. This is why the
        // rule watches the sides and not the mid.
        let mut rule = posted();
        let skewed = Quote {
            skew_bps: -10,
            ..quote(MID)
        };
        assert!(matches!(
            rule.on_quote(&skewed, 101),
            Ok(Update::Post(Why::Moved { .. }))
        ));
    }

    #[test]
    fn a_wider_spread_is_posted_once_a_side_moves_past_the_threshold() {
        let mut rule = posted();
        let wider = |spread_bps| Quote {
            spread_bps,
            ..quote(MID)
        };
        // Each side moves by the widening in bps of the mid, which is a little
        // more in bps of the bid and a little less in bps of the ask.
        // 10 → 14: the bid moves 60 000, 4.004 bps of 149 850 000.
        assert_eq!(rule.on_quote(&wider(14), 101), Ok(Update::Hold));
        // 10 → 15: the bid moves 75 000, 5.005 bps — over; the ask's 4.995 is not.
        assert_eq!(
            rule.on_quote(&wider(15), 101),
            Ok(Update::Post(Why::Moved { bps: 5 }))
        );
    }

    #[test]
    fn a_new_size_alone_is_held() {
        let mut rule = posted();
        let bigger = Quote {
            max_size_base: 1_000_000,
            ..quote(MID)
        };
        assert_eq!(rule.on_quote(&bigger, 101), Ok(Update::Hold));
    }

    #[test]
    fn the_heartbeat_posts_a_quote_that_did_not_move() {
        let mut rule = posted();
        let due = 100 + u64::from(HEARTBEAT);
        assert_eq!(rule.on_quote(&quote(MID), due - 1), Ok(Update::Hold));
        assert_eq!(
            rule.on_quote(&quote(MID), due),
            Ok(Update::Post(Why::Heartbeat {
                age_slots: u64::from(HEARTBEAT)
            }))
        );
        assert_eq!(rule.on_quote(&quote(MID), due + 1), Ok(Update::Hold));
        assert_eq!(rule.due_slot(), Some(due + u64::from(HEARTBEAT)));
    }

    #[test]
    fn a_withdrawal_is_sent_once_and_the_next_quote_resumes() {
        let mut rule = posted();
        assert!(rule.on_withdraw());
        assert!(!rule.on_withdraw());
        assert_eq!(rule.due_slot(), None);
        assert_eq!(
            rule.on_quote(&quote(MID), 101),
            Ok(Update::Post(Why::Resumed))
        );
    }

    #[test]
    fn a_rule_that_knows_nothing_withdraws() {
        // A quote from an earlier run of the engine may still be on the book.
        let mut rule = rule();
        assert_eq!(rule.due_slot(), None);
        assert!(rule.on_withdraw());
    }

    #[test]
    fn after_a_failed_send_the_next_quote_goes_out_whatever_it_is() {
        let mut rule = posted();
        rule.forget();
        assert_eq!(rule.due_slot(), None);
        assert_eq!(
            rule.on_quote(&quote(MID), 101),
            Ok(Update::Post(Why::Unknown))
        );

        let mut rule = posted();
        assert!(rule.on_withdraw());
        rule.forget();
        assert!(rule.on_withdraw(), "a failed withdrawal is sent again");
    }

    #[test]
    fn a_quote_without_side_prices_is_an_error_and_is_not_recorded() {
        let mut rule = posted();
        assert_eq!(rule.on_quote(&quote(0), 101), Err(QuoteError::QuoteNotSet));
        let broken = Quote {
            spread_bps: BPS_DENOM,
            ..quote(MID)
        };
        assert_eq!(rule.on_quote(&broken, 101), Err(QuoteError::InvalidParams));
        assert_eq!(rule.on_quote(&quote(MID), 101), Ok(Update::Hold));
    }

    #[test]
    fn moved_bps_survives_a_zero_side_and_an_overflow() {
        assert_eq!(moved_bps(0, 0, THRESHOLD), None);
        assert_eq!(moved_bps(0, 1, THRESHOLD), Some(u32::MAX));
        assert_eq!(moved_bps(1, u128::MAX, THRESHOLD), Some(u32::MAX));
        assert_eq!(moved_bps(u128::MAX, u128::MAX, THRESHOLD), None);
    }

    #[test]
    fn why_reads_as_a_log_line() {
        assert_eq!(Why::Moved { bps: 7 }.to_string(), "a side moved 7 bps");
        assert_eq!(
            Why::Heartbeat { age_slots: 12 }.to_string(),
            "heartbeat: 12 slots since the last post"
        );
    }
}
