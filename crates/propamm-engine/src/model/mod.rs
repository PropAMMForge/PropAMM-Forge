//! The pricing model — the one replaceable point of the engine (FR-015).
//!
//! Everything else in this crate is transport: read the feed, judge it, decide
//! when to post, sign, send. What to quote is this module's business, and it is
//! the part a client is expected to replace with their own.
//!
//! A model is anything that implements [`PricingModel`]: given the market and
//! the inventory ([`MarketState`]), it answers with a [`Decision`] — a
//! [`Quote`] to post, or a withdrawal. There are two ways to plug one in
//! (FR-015a):
//!
//! - [`spread_skew`] — the built-in model, in Rust, in-process (FR-013);
//! - [`external`] — any program on the other end of a line protocol on
//!   stdin/stdout, first of all Python, where the quant tooling lives.
//!
//! # The deadline
//!
//! [`PricingModel::price`] takes the instant by which the answer is needed. A
//! model that cannot promise an answer in time — a process on the other side of
//! a pipe — uses it to stop waiting; the built-in one returns long before it
//! matters. What to do about a late answer is not the model's decision: the
//! tick measures, skips and records the reason (FR-015b, T030).

use std::time::{Duration, Instant};

use propamm_quote::{Inventory, QuoteError, BPS_DENOM};
use thiserror::Error;

pub mod external;
pub mod spread_skew;

/// What the core hands the model (FR-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketState {
    /// The mid from the feed, in [`PRICE_SCALE`](propamm_quote::PRICE_SCALE):
    /// raw quote per raw base × 1e9.
    pub mid_e9: u128,
    /// What the vault holds right now.
    pub inventory: Inventory,
    /// The vault's hard inventory bound (FR-026) — the scale skew is measured against.
    pub max_skew_bps: u16,
}

/// What the model gives back: the four fields of the quote the engine may move.
///
/// The other fields of `QuoteParams` are the deployment's, not the model's: the
/// freshness limit and the hard skew bound are risk settings the owner sets, and
/// `quote_slot` is the chain's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// The market mid, in [`PRICE_SCALE`](propamm_quote::PRICE_SCALE).
    pub mid_e9: u128,
    /// Half of the spread, in basis points.
    pub spread_bps: u16,
    /// Mid shift by inventory skew: positive raises both sides.
    pub skew_bps: i16,
    /// Largest order the vault will take, in raw units of the base asset (FR-008).
    pub max_size_base: u64,
}

impl Quote {
    /// Whether both sides of this quote are prices at all.
    ///
    /// The same domain `side_price_e9` enforces: a non-zero mid, a half-spread
    /// below a whole, a shift that leaves the mid positive. A quote outside it
    /// would be refused by the program anyway — this only finds out before a
    /// transaction is spent on it.
    ///
    /// # Errors
    ///
    /// [`QuoteError::QuoteNotSet`] for a zero mid, [`QuoteError::InvalidParams`]
    /// for a spread or a shift out of range.
    pub fn check(&self) -> Result<(), QuoteError> {
        if self.mid_e9 == 0 {
            return Err(QuoteError::QuoteNotSet);
        }
        let whole = i32::from(BPS_DENOM);
        if i32::from(self.spread_bps) >= whole || i32::from(self.skew_bps) <= -whole {
            return Err(QuoteError::InvalidParams);
        }
        Ok(())
    }
}

/// What a model decided about the current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Post this quote.
    Quote(Quote),
    /// Do not quote now. The core takes this exactly as it takes a silent feed
    /// (FR-014): the quote is withdrawn, not left to age on chain. A model may
    /// know something about the market the core does not — news, volatility, a
    /// venue it watches — and "I would rather not quote" has to mean that.
    Withdraw { reason: String },
}

/// Why a model gave no decision this time. Every variant is a skipped tick,
/// not a withdrawal: an error says nothing about the market.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelError {
    /// The model could not price this state (the built-in one's own refusal).
    #[error("the model cannot price this state: {0}")]
    Quote(QuoteError),
    /// The model said it failed — an exception in an external model.
    #[error("the model reported a failure: {message}")]
    Failed { message: String },
    /// No answer by the deadline. The question stays open: the late answer is
    /// read and discarded before the next one is asked.
    #[error("the model did not answer within {waited:?}")]
    Timeout { waited: Duration },
    /// The answer was not in the protocol, or its quote was not a price.
    #[error("the model broke the protocol: {detail}")]
    Protocol { detail: String },
    /// The model process is not running; the next start is attempted after
    /// `retry_in`.
    #[error("the model process is down ({reason}), next start in {retry_in:?}")]
    Down { reason: String, retry_in: Duration },
}

// Not `#[from]`: `QuoteError` is `no_std`-friendly and has no `std::error::Error`
// to be a source with.
impl From<QuoteError> for ModelError {
    fn from(error: QuoteError) -> Self {
        Self::Quote(error)
    }
}

/// The engine's one replaceable point (FR-015).
pub trait PricingModel {
    /// Price `state`, answering by `deadline` if the model can bound its wait.
    ///
    /// # Errors
    ///
    /// [`ModelError`] — no decision this tick; see the variants for why.
    fn price(&mut self, state: &MarketState, deadline: Instant) -> Result<Decision, ModelError>;
}

impl<M: PricingModel + ?Sized> PricingModel for Box<M> {
    fn price(&mut self, state: &MarketState, deadline: Instant) -> Result<Decision, ModelError> {
        (**self).price(state, deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote() -> Quote {
        Quote {
            mid_e9: 150_000_000,
            spread_bps: 10,
            skew_bps: -30,
            max_size_base: 1_000,
        }
    }

    #[test]
    fn a_quote_inside_the_domain_passes() {
        assert_eq!(quote().check(), Ok(()));
        let edge = Quote {
            spread_bps: BPS_DENOM - 1,
            skew_bps: 1 - i16::try_from(BPS_DENOM).unwrap(),
            ..quote()
        };
        assert_eq!(edge.check(), Ok(()));
    }

    #[test]
    fn a_quote_that_is_not_a_price_is_caught_before_it_is_sent() {
        let zero_mid = Quote {
            mid_e9: 0,
            ..quote()
        };
        assert_eq!(zero_mid.check(), Err(QuoteError::QuoteNotSet));
        let whole_spread = Quote {
            spread_bps: BPS_DENOM,
            ..quote()
        };
        assert_eq!(whole_spread.check(), Err(QuoteError::InvalidParams));
        let whole_shift = Quote {
            skew_bps: -i16::try_from(BPS_DENOM).unwrap(),
            ..quote()
        };
        assert_eq!(whole_shift.check(), Err(QuoteError::InvalidParams));
    }

    /// The domain `check` claims is the one the shared arithmetic enforces:
    /// on both sides of each edge, `side_price_e9` agrees with it.
    #[test]
    fn the_check_matches_what_the_program_computes() {
        use propamm_quote::{side_price_e9, QuoteParams, Side};

        let whole = i16::try_from(BPS_DENOM).unwrap();
        for (spread_bps, skew_bps) in [
            (0, 0),
            (BPS_DENOM - 1, 0),
            (BPS_DENOM, 0),
            (0, 1 - whole),
            (0, -whole),
            (0, i16::MAX),
        ] {
            let q = Quote {
                spread_bps,
                skew_bps,
                ..quote()
            };
            let params = QuoteParams {
                mid_e9: q.mid_e9,
                spread_bps,
                skew_bps,
                max_size_base: q.max_size_base,
                quote_slot: 0,
                max_quote_age_slots: 1,
                max_skew_bps: 2_000,
            };
            let priced = [Side::BaseToQuote, Side::QuoteToBase]
                .iter()
                .all(|&side| side_price_e9(&params, side).is_ok());
            assert_eq!(
                q.check().is_ok(),
                priced,
                "spread {spread_bps}, skew {skew_bps}"
            );
        }
    }

    #[test]
    fn a_boxed_model_is_a_model() {
        let mut model: Box<dyn PricingModel> = Box::new(
            spread_skew::SpreadSkewModel::checked(10, 40, 150, 500)
                .expect("a workable configuration"),
        );
        let state = MarketState {
            mid_e9: 150_000_000,
            inventory: Inventory {
                base_amount: 1_000_000_000,
                quote_amount: 150_000_000,
            },
            max_skew_bps: 2_000,
        };
        let decision = model
            .price(&state, Instant::now())
            .expect("a balanced state prices");
        assert!(matches!(decision, Decision::Quote(q) if q.spread_bps == 10 && q.skew_bps == 0));
    }
}
