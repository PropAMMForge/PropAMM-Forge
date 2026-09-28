//! The tick's model step: ask the model within a budget, and skip the tick —
//! with the reason recorded — when it does not answer in time (FR-015b).
//!
//! # Who measures
//!
//! The tick, not the model. [`PricingModel::price`] takes a deadline, and a
//! model on the other side of a pipe uses it to stop waiting — but a model in
//! this process cannot be interrupted, and a client's own Rust model may ignore
//! the deadline altogether. So the step reads its own clock before and after
//! the call, and an answer that came back past the budget is discarded however
//! it was produced: a price computed for a market that has moved on since is
//! exactly the stale price FR-015b forbids posting.
//!
//! A late **withdrawal** is the one exception, and it is honoured. It is not a
//! price and cannot be stale; it is the model saying it would rather not quote,
//! and taking the quote off the book late is still safer than leaving it.
//!
//! # What a skip does to the quote
//!
//! Nothing. An error says nothing about the market (T029), so the quote on the
//! book stays where it is. If the model keeps missing, nobody refreshes the
//! quote and it expires on chain on its own (FR-007): the AMM drops out of the
//! routes without the engine spending a transaction on it.
//!
//! # What is recorded, and how often
//!
//! Every skip goes to the log. The structured record for `engine_events` —
//! which the free-tier estimate sizes at under a hundred rows a year — is
//! edge-triggered instead: one [`ModelEvent::Skipping`] when the model starts
//! missing, one [`ModelEvent::Recovered`] with the count when it answers in
//! time again. A model that is late on every tick would otherwise write a row
//! per tick, some seventeen thousand a day at a 5 s tick. Storing the event is
//! the caller's business; this step only produces it.

use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{info, warn};

use crate::feed::SLOT_DURATION;
use crate::model::{Decision, MarketState, ModelError, PricingModel, Quote};

/// A monotonic clock. Behind a trait so that "how long the model took" is
/// tested without sleeping.
pub trait Monotonic {
    fn now(&self) -> Instant;
}

/// [`Instant::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemMonotonic;

impl Monotonic for SystemMonotonic {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A budget that cannot do its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BudgetConfigError {
    #[error("the model budget is zero: every tick would be skipped")]
    Zero,
    #[error(
        "the model budget of {budget_ms} ms is not shorter than the on-chain freshness limit \
         of {slots} slots (≈{freshness_ms} ms): a price answered in budget could already be \
         too old to post"
    )]
    NotShorterThanFreshness {
        budget_ms: u128,
        slots: u32,
        freshness_ms: u128,
    },
}

/// How long the model may take to answer (`RISK_MODEL_TIMEOUT_MS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget(Duration);

impl Budget {
    /// A budget with no cross-check. Prefer [`Budget::checked`].
    #[must_use]
    pub const fn new(budget: Duration) -> Self {
        Self(budget)
    }

    /// The same, refusing a budget that cannot protect the quote.
    ///
    /// `max_quote_age_slots` is the vault's freshness limit (FR-007): a model
    /// allowed to think for that long could hand back a price the program
    /// would already refuse as stale. The tighter bound — the budget against
    /// the heartbeat interval — belongs to the update rule (FR-011a, T031).
    ///
    /// # Errors
    ///
    /// [`BudgetConfigError`] — a zero budget, or one not shorter than the
    /// freshness limit.
    pub fn checked(budget: Duration, max_quote_age_slots: u32) -> Result<Self, BudgetConfigError> {
        if budget.is_zero() {
            return Err(BudgetConfigError::Zero);
        }
        let freshness = SLOT_DURATION.saturating_mul(max_quote_age_slots);
        if budget >= freshness {
            return Err(BudgetConfigError::NotShorterThanFreshness {
                budget_ms: budget.as_millis(),
                slots: max_quote_age_slots,
                freshness_ms: freshness.as_millis(),
            });
        }
        Ok(Self(budget))
    }

    /// The budget itself.
    #[must_use]
    pub const fn get(&self) -> Duration {
        self.0
    }
}

/// Why this tick posts nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Skip {
    /// The model answered with a price, but past the budget; the price is discarded.
    #[error("the model answered in {took_ms} ms, budget {budget_ms} ms; the price is discarded")]
    OverBudget { took_ms: u128, budget_ms: u128 },
    /// The model gave no decision — its own timeout included.
    #[error(transparent)]
    Model(ModelError),
}

/// What the tick does with the quote this time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Post this quote — whether it is worth a transaction is the update rule's
    /// decision (FR-011, T031).
    Post(Quote),
    /// Take the quote off the book (the model's own [`Decision::Withdraw`]).
    Withdraw { reason: String },
    /// Leave the quote as it is.
    Skip(Skip),
}

/// A change in whether the model keeps its budget — the row for `engine_events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    /// The first skip after the model was keeping its budget (or since start).
    Skipping { reason: Skip },
    /// The model answered in time again after `skipped` skipped ticks.
    Recovered { skipped: u64, for_ms: u128 },
}

impl ModelEvent {
    /// `engine_events.kind`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Skipping { .. } => "model_skipping",
            Self::Recovered { .. } => "model_recovered",
        }
    }

    /// `engine_events.reason`.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Skipping { reason } => reason.to_string(),
            Self::Recovered { skipped, for_ms } => {
                format!("{skipped} tick(s) skipped over {for_ms} ms")
            }
        }
    }
}

/// One pass of the model step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub step: Step,
    /// How long the model took, as the step measured it.
    pub took: Duration,
    /// Set only when the model starts or stops missing its budget.
    pub event: Option<ModelEvent>,
}

/// A run of consecutive skips.
#[derive(Debug, Clone, Copy)]
struct Streak {
    since: Instant,
    skipped: u64,
}

/// The model behind a budget (FR-015b).
#[derive(Debug)]
pub struct ModelStep<M, C = SystemMonotonic> {
    model: M,
    clock: C,
    budget: Budget,
    streak: Option<Streak>,
}

impl<M: PricingModel> ModelStep<M> {
    /// The model on the system clock.
    #[must_use]
    pub fn new(model: M, budget: Budget) -> Self {
        Self::with_clock(model, budget, SystemMonotonic)
    }
}

impl<M: PricingModel, C: Monotonic> ModelStep<M, C> {
    #[must_use]
    pub fn with_clock(model: M, budget: Budget, clock: C) -> Self {
        Self {
            model,
            clock,
            budget,
            streak: None,
        }
    }

    /// The model, e.g. to read its process id.
    #[must_use]
    pub fn model(&self) -> &M {
        &self.model
    }

    /// Is the model in a run of skips right now?
    #[must_use]
    pub fn is_skipping(&self) -> bool {
        self.streak.is_some()
    }

    /// Ask the model about `state` and decide what the tick does with the answer.
    pub fn run(&mut self, state: &MarketState) -> Outcome {
        let budget = self.budget.get();
        let started = self.clock.now();
        let answer = self.model.price(state, started + budget);
        let took = self.clock.now().saturating_duration_since(started);

        let step = match answer {
            // The one late answer that is honoured — see the module docs.
            Ok(Decision::Withdraw { reason }) => Step::Withdraw { reason },
            Ok(Decision::Quote(_)) if took > budget => Step::Skip(Skip::OverBudget {
                took_ms: took.as_millis(),
                budget_ms: budget.as_millis(),
            }),
            // Every model passes here, a client's own Rust one included: a quote
            // that is not a price is caught before a transaction is spent on it.
            Ok(Decision::Quote(quote)) => match quote.check() {
                Ok(()) => Step::Post(quote),
                Err(error) => Step::Skip(Skip::Model(error.into())),
            },
            Err(error) => Step::Skip(Skip::Model(error)),
        };

        let event = self.record(&step, started, took);
        Outcome { step, took, event }
    }

    fn record(&mut self, step: &Step, now: Instant, took: Duration) -> Option<ModelEvent> {
        if let Step::Skip(reason) = step {
            warn!(%reason, took_ms = took.as_millis(), "skipping the tick");
            if let Some(streak) = &mut self.streak {
                streak.skipped += 1;
                return None;
            }
            self.streak = Some(Streak {
                since: now,
                skipped: 1,
            });
            return Some(ModelEvent::Skipping {
                reason: reason.clone(),
            });
        }
        let streak = self.streak.take()?;
        let for_ms = now.saturating_duration_since(streak.since).as_millis();
        info!(
            skipped = streak.skipped,
            for_ms, "the model is back in budget"
        );
        Some(ModelEvent::Recovered {
            skipped: streak.skipped,
            for_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use propamm_quote::{Inventory, QuoteError};

    use super::*;
    use crate::feed::Backoff;
    use crate::model::external::{ExternalProcessModel, ModelCommand};

    const BUDGET: Duration = Duration::from_millis(50);

    /// A clock the fake model moves: the model "takes" exactly what it is told to.
    #[derive(Clone)]
    struct FakeClock(Rc<Cell<Instant>>);

    impl Monotonic for FakeClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    /// Answers from a script, each after a given time, and remembers the deadlines.
    struct Scripted {
        clock: FakeClock,
        answers: VecDeque<(Duration, Result<Decision, ModelError>)>,
        deadlines: Vec<Instant>,
    }

    impl PricingModel for Scripted {
        fn price(&mut self, _: &MarketState, deadline: Instant) -> Result<Decision, ModelError> {
            self.deadlines.push(deadline);
            let (takes, answer) = self.answers.pop_front().expect("a scripted answer");
            let clock = &self.clock.0;
            clock.set(clock.get() + takes);
            answer
        }
    }

    fn step(
        answers: impl IntoIterator<Item = (u64, Result<Decision, ModelError>)>,
    ) -> ModelStep<Scripted, FakeClock> {
        let clock = FakeClock(Rc::new(Cell::new(Instant::now())));
        let model = Scripted {
            clock: clock.clone(),
            answers: answers
                .into_iter()
                .map(|(ms, answer)| (Duration::from_millis(ms), answer))
                .collect(),
            deadlines: Vec::new(),
        };
        ModelStep::with_clock(model, Budget::new(BUDGET), clock)
    }

    fn quote() -> Quote {
        Quote {
            mid_e9: 150_000_000,
            spread_bps: 10,
            skew_bps: 0,
            max_size_base: 1_000,
        }
    }

    fn post() -> Result<Decision, ModelError> {
        Ok(Decision::Quote(quote()))
    }

    fn state() -> MarketState {
        MarketState {
            mid_e9: 150_000_000,
            inventory: Inventory {
                base_amount: 1_000_000_000,
                quote_amount: 150_000_000,
            },
            max_skew_bps: 2_000,
        }
    }

    #[test]
    fn an_answer_in_budget_is_posted_and_the_model_is_given_the_budget() {
        let mut step = step([(10, post())]);
        let started = step.clock.now();
        let out = step.run(&state());
        assert_eq!(out.step, Step::Post(quote()));
        assert_eq!(out.took, Duration::from_millis(10));
        assert_eq!(out.event, None);
        assert_eq!(step.model().deadlines, [started + BUDGET]);
    }

    #[test]
    fn an_answer_exactly_at_the_budget_is_in_budget() {
        let mut step = step([(50, post())]);
        assert_eq!(step.run(&state()).step, Step::Post(quote()));
    }

    #[test]
    fn a_price_past_the_budget_is_discarded_even_when_the_model_did_answer() {
        // The in-process case: nothing interrupted the model, it simply took too long.
        let mut step = step([(51, post())]);
        let out = step.run(&state());
        assert_eq!(
            out.step,
            Step::Skip(Skip::OverBudget {
                took_ms: 51,
                budget_ms: 50
            })
        );
    }

    #[test]
    fn a_late_withdrawal_is_still_honoured() {
        let mut step = step([(
            500,
            Ok(Decision::Withdraw {
                reason: "news".into(),
            }),
        )]);
        let out = step.run(&state());
        assert_eq!(
            out.step,
            Step::Withdraw {
                reason: "news".into()
            }
        );
        assert_eq!(out.event, None, "a withdrawal is an answer, not a skip");
    }

    #[test]
    fn a_model_error_skips_the_tick() {
        let timeout = ModelError::Timeout { waited: BUDGET };
        let mut step = step([(50, Err(timeout.clone()))]);
        assert_eq!(step.run(&state()).step, Step::Skip(Skip::Model(timeout)));
    }

    #[test]
    fn a_quote_that_is_not_a_price_is_skipped_whichever_model_sent_it() {
        let zero_mid = Quote {
            mid_e9: 0,
            ..quote()
        };
        let mut step = step([(1, Ok(Decision::Quote(zero_mid)))]);
        assert_eq!(
            step.run(&state()).step,
            Step::Skip(Skip::Model(ModelError::Quote(QuoteError::QuoteNotSet)))
        );
    }

    #[test]
    fn the_event_is_written_on_the_edges_only() {
        let down = ModelError::Down {
            reason: "exit 1".into(),
            retry_in: Duration::from_secs(1),
        };
        let mut step = step([
            (10, post()),
            (80, post()),
            (0, Err(down.clone())),
            (0, Err(down)),
            (10, post()),
            (10, post()),
        ]);
        let events: Vec<_> = (0..6).map(|_| step.run(&state()).event).collect();

        let first = Skip::OverBudget {
            took_ms: 80,
            budget_ms: 50,
        };
        assert_eq!(
            events,
            [
                None,
                Some(ModelEvent::Skipping { reason: first }),
                None,
                None,
                // Measured from the start of the first skipped call: 80 + 0 + 0 ms.
                Some(ModelEvent::Recovered {
                    skipped: 3,
                    for_ms: 80
                }),
                None,
            ]
        );
        assert!(!step.is_skipping());
    }

    #[test]
    fn a_withdrawal_ends_a_run_of_skips() {
        let mut step = step([
            (
                0,
                Err(ModelError::Failed {
                    message: "boom".into(),
                }),
            ),
            (
                5,
                Ok(Decision::Withdraw {
                    reason: "vol".into(),
                }),
            ),
        ]);
        step.run(&state());
        assert!(step.is_skipping());
        let out = step.run(&state());
        assert_eq!(
            out.event,
            Some(ModelEvent::Recovered {
                skipped: 1,
                for_ms: 0
            })
        );
    }

    #[test]
    fn the_event_maps_onto_engine_events() {
        let skipping = ModelEvent::Skipping {
            reason: Skip::OverBudget {
                took_ms: 80,
                budget_ms: 50,
            },
        };
        assert_eq!(skipping.kind(), "model_skipping");
        assert_eq!(
            skipping.reason(),
            "the model answered in 80 ms, budget 50 ms; the price is discarded"
        );
        let recovered = ModelEvent::Recovered {
            skipped: 3,
            for_ms: 15_000,
        };
        assert_eq!(recovered.kind(), "model_recovered");
        assert_eq!(recovered.reason(), "3 tick(s) skipped over 15000 ms");
    }

    #[test]
    fn a_budget_that_cannot_protect_the_quote_is_refused() {
        assert_eq!(
            Budget::checked(Duration::ZERO, 10),
            Err(BudgetConfigError::Zero)
        );
        // 10 slots ≈ 4 000 ms.
        assert_eq!(
            Budget::checked(Duration::from_millis(4_000), 10),
            Err(BudgetConfigError::NotShorterThanFreshness {
                budget_ms: 4_000,
                slots: 10,
                freshness_ms: 4_000
            })
        );
        assert_eq!(
            Budget::checked(Duration::from_millis(50), 10),
            Ok(Budget::new(Duration::from_millis(50)))
        );
    }

    /// The whole path on a real clock and a real process: a model that takes
    /// longer than the budget is cut off by the deadline and the tick skipped;
    /// the same model within a budget it can keep is posted.
    #[test]
    fn a_slow_process_model_is_skipped_and_a_fast_one_posted() {
        let reference = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/models/spread_skew.py"
        );
        let command = ModelCommand::new("python3").arg(reference).args([
            "--base-spread-bps",
            "10",
            "--max-spread-bps",
            "40",
            "--max-skew-shift-bps",
            "150",
            "--size-fraction-bps",
            "500",
        ]);
        let backoff = Backoff {
            min: Duration::ZERO,
            max: Duration::ZERO,
        };
        let model = ExternalProcessModel::start(command, backoff).expect("python3 starts");

        // No interpreter answers its first question within a millisecond.
        let mut tight = ModelStep::new(model, Budget::new(Duration::from_millis(1)));
        let out = tight.run(&state());
        assert!(
            matches!(
                out.step,
                Step::Skip(Skip::Model(ModelError::Timeout { .. }))
            ),
            "{out:?}"
        );
        assert!(matches!(out.event, Some(ModelEvent::Skipping { .. })));

        let ModelStep { model, .. } = tight;
        let mut roomy = ModelStep::new(model, Budget::new(Duration::from_secs(10)));
        let out = roomy.run(&state());
        assert!(matches!(out.step, Step::Post(_)), "{out:?}");
    }
}
