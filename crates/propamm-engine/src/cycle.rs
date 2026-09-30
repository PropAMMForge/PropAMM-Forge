//! The tick loop: the feed, the model, the update rule and the sender together.
//!
//! # One pass
//!
//! Every usable price goes to the model (FR-015), the model's quote goes to
//! the update rule (FR-011), and what the rule lets through goes to the
//! sender. A withdrawal — the feed's (FR-014) or the model's — goes straight
//! to the sender, once. Between prices the loop wakes on its own deadlines:
//!
//! - the **heartbeat** ([`Rule::due_slot`]): the model is asked again on the
//!   latest live price rather than waiting for the next one (T031);
//! - a **read of the book**: [`LANDING_SLOTS`] after a send, to see whether it
//!   landed, and once per heartbeat otherwise (T032 decision) — the same read
//!   refreshes the inventory the model prices and the slot clock;
//! - the end of a **pause** after a refused withdrawal, to send it again.
//!
//! # What stops the loop
//!
//! A refusal no retry can fix ([`Fatal`](crate::chain::Fatal)): a different
//! pricing authority, no vault, no SOL for fees, no program. A halted vault
//! does **not** stop it (T032 decision): the loop stops posting and resumes
//! once a read shows the vault running again — the operator should not have to
//! restart the engine after a resume. And the feed reader going away, once the
//! quote is off the book: there is nothing left to quote on.
//!
//! # Checked at start against the chain
//!
//! The freshness limit the rule, the model budget and the silence bound must
//! fit in (FR-007, FR-011a, FR-014, FR-015b) is the vault's
//! `max_quote_age_slots` as read from chain, not a number from config — and
//! it is checked again if the owner changes it while the engine runs.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{info, warn};

use crate::chain::{Chain, Fatal, Refusal, VaultView};
use crate::feed::{
    mid_e9, FeedEvent, Price, PriceId, QuoteState, Silence, SilenceConfigError, Wake, Watch,
};
use crate::model::{MarketState, PricingModel, Quote};
use crate::policy::{Rule, RuleConfigError, Update, LANDING_SLOTS};
use crate::sender::{Check, Order, Sender, SlotClock};
use crate::tick::{Budget, BudgetConfigError, ModelStep, Monotonic, Step};

/// The pricing authority stops below one signature fee: it cannot send even a
/// withdrawal.
pub const MIN_FEE_LAMPORTS: u64 = 5_000;

/// Why the loop ended — or never started.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Stop {
    /// At start, any refusal (a transient one can be retried by the caller);
    /// in the loop, only a fatal one.
    #[error(transparent)]
    Chain(Refusal),
    #[error(transparent)]
    Rule(RuleConfigError),
    #[error(transparent)]
    Budget(BudgetConfigError),
    #[error(transparent)]
    Silence(SilenceConfigError),
    #[error("the feed reader is gone and the quote is off the book")]
    FeedGone,
}

impl From<Fatal> for Stop {
    fn from(fatal: Fatal) -> Self {
        Self::Chain(Refusal::Fatal(fatal))
    }
}

/// What the engine is told, not what it reads from chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `QUOTE_DEVIATION_THRESHOLD_BPS`.
    pub threshold_bps: u16,
    /// `QUOTE_HEARTBEAT_SLOTS`.
    pub heartbeat_slots: u32,
    /// `PYTH_PRICE_FEED_ID` — the base asset in USD.
    pub base_feed: PriceId,
    /// `PYTH_QUOTE_PRICE_FEED_ID` — the quote asset in USD; `None` takes it at par.
    pub quote_feed: Option<PriceId>,
}

/// Where the book should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Quote,
    Off,
}

/// The mid, from one feed or the cross of two.
enum Mid {
    Ready(u128),
    /// A leg has not arrived yet, and the engine only just started.
    Waiting,
    Withdraw(String),
}

/// The latest price of each leg, and when it came.
///
/// The silence bound in [`Watch`] is kept by any price, so with two legs one
/// could go quiet behind the other's back. Each leg is held to the same bound
/// here: a cross rate on a stale leg is a stale mid (FR-014).
struct Legs {
    base: PriceId,
    quote: Option<PriceId>,
    base_decimals: u8,
    quote_decimals: u8,
    bound: Duration,
    started: Instant,
    latest: HashMap<PriceId, (Price, Instant)>,
}

impl Legs {
    fn observe(&mut self, price: Price, at: Instant) {
        self.latest.insert(price.id, (price, at));
    }

    fn leg(&self, id: PriceId, now: Instant) -> Result<Option<&Price>, String> {
        match self.latest.get(&id) {
            Some((price, at)) if now.saturating_duration_since(*at) < self.bound => Ok(Some(price)),
            Some(_) => Err(format!("no usable price for {id} within the silence bound")),
            // The legs of one stream arrive one by one: give the other a bound to come.
            None if now.saturating_duration_since(self.started) < self.bound => Ok(None),
            None => Err(format!("no price for {id} since start")),
        }
    }

    fn mid(&self, now: Instant) -> Mid {
        let base = match self.leg(self.base, now) {
            Ok(Some(price)) => price,
            Ok(None) => return Mid::Waiting,
            Err(reason) => return Mid::Withdraw(reason),
        };
        let quote = match self.quote.map(|id| self.leg(id, now)) {
            None => None,
            Some(Ok(Some(price))) => Some(price),
            Some(Ok(None)) => return Mid::Waiting,
            Some(Err(reason)) => return Mid::Withdraw(reason),
        };
        match mid_e9(base, quote, self.base_decimals, self.quote_decimals) {
            Ok(mid) => Mid::Ready(mid),
            Err(error) => Mid::Withdraw(format!("the mid cannot be computed: {error}")),
        }
    }
}

/// The engine, running.
pub struct Cycle<M, K, C> {
    step: ModelStep<M, C>,
    rule: Rule,
    sender: Sender<K>,
    clock: SlotClock<C>,
    legs: Legs,
    silence_bound: Duration,
    view: VaultView,
    next_read: u64,
    /// No read before this slot: the last one failed.
    read_retry: u64,
    want: Want,
    halted: bool,
}

impl<M: PricingModel, K: Chain, C: Monotonic + Clone> Cycle<M, K, C> {
    /// Read the vault and check everything against it before the first price.
    ///
    /// `silence_bound` is the [`Watch`]'s; `clock` is the one `step` measures with.
    ///
    /// # Errors
    ///
    /// [`Stop`] — the chain refused the read, this key is not the vault's
    /// pricing authority or cannot pay fees, or a setting does not fit in the
    /// vault's freshness limit.
    pub fn start(
        step: ModelStep<M, C>,
        mut sender: Sender<K>,
        settings: Settings,
        silence_bound: Duration,
        clock: C,
    ) -> Result<Self, Stop> {
        let view = sender.read().map_err(Stop::Chain)?;
        let signer = sender.signer();
        vet(&view, signer)?;
        let rule = fits(&view, &settings, step.budget(), silence_bound)?;
        let deployment = sender.deployment();
        let now = clock.now();
        info!(
            vault = %deployment.vault,
            slot = view.slot,
            halted = view.halted,
            max_quote_age_slots = view.max_quote_age_slots,
            "engine started"
        );
        Ok(Self {
            step,
            rule,
            clock: SlotClock::anchored(clock, view.slot),
            legs: Legs {
                base: settings.base_feed,
                quote: settings.quote_feed,
                base_decimals: deployment.base_decimals,
                quote_decimals: deployment.quote_decimals,
                bound: silence_bound,
                started: now,
                latest: HashMap::new(),
            },
            silence_bound,
            next_read: view.slot + u64::from(settings.heartbeat_slots),
            read_retry: 0,
            halted: view.halted,
            view,
            want: Want::Quote,
            sender,
        })
    }

    /// Run until a [`Stop`]. Meant for the engine's main thread; the feed
    /// reader runs on its own and hands prices over `rx`.
    pub fn run(mut self, watch: &mut Watch, rx: &Receiver<FeedEvent>) -> Stop {
        loop {
            let wake_at = self.wake_at();
            let pass = match watch.recv_until(rx, Some(wake_at)) {
                None => return Stop::FeedGone,
                Some(Wake::Feed(QuoteState::Live(price))) => self.on_price(price),
                Some(Wake::Feed(QuoteState::Withdrawn(reason))) => {
                    self.on_withdrawal(&reason.to_string())
                }
                Some(Wake::Timer) => Ok(()),
            };
            if let Err(stop) = pass.and_then(|()| self.housekeeping()) {
                return stop;
            }
        }
    }

    /// A usable price arrived.
    ///
    /// # Errors
    ///
    /// A fatal refusal of the send.
    pub fn on_price(&mut self, price: Price) -> Result<(), Stop> {
        self.legs.observe(price, self.clock.instant());
        self.want = Want::Quote;
        self.price_and_post()
    }

    /// The feed says the quote has to come off the book.
    ///
    /// # Errors
    ///
    /// A fatal refusal of the send.
    pub fn on_withdrawal(&mut self, reason: &str) -> Result<(), Stop> {
        self.withdraw(reason)
    }

    /// Everything that is due by the clock: a read of the book, a withdrawal
    /// to send again, the heartbeat.
    ///
    /// # Errors
    ///
    /// A fatal refusal, or a freshness limit changed on chain to one the
    /// settings no longer fit in.
    pub fn housekeeping(&mut self) -> Result<(), Stop> {
        let now_slot = self.clock.now();
        if now_slot >= self.read_due() {
            self.refresh(now_slot)?;
        }
        let paused = self.sender.is_paused(self.clock.instant());
        if self.want == Want::Off && !paused {
            // A no-op unless a withdrawal was lost: the rule's edge sees to that.
            self.withdraw("sending the withdrawal again")?;
        }
        let due = self
            .rule
            .due_slot()
            .is_some_and(|due| self.clock.now() >= due);
        if self.want == Want::Quote && due && !self.halted {
            self.price_and_post()?;
        }
        Ok(())
    }

    /// The rule, e.g. to see what it last posted.
    #[must_use]
    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// The sender, e.g. to inspect a scripted chain.
    #[must_use]
    pub fn sender(&self) -> &Sender<K> {
        &self.sender
    }

    /// Is the vault halted, as last seen?
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    fn read_due(&self) -> u64 {
        let due = self
            .sender
            .check_slot()
            .map_or(self.next_read, |check| check.min(self.next_read));
        due.max(self.read_retry)
    }

    fn wake_at(&self) -> Instant {
        let mut at = self.clock.when(self.read_due());
        if let Some(due) = self.rule.due_slot() {
            if self.want == Want::Quote && !self.halted {
                at = at.min(self.clock.when(due));
            }
        }
        if let Some(until) = self.sender.pause_until() {
            if self.want == Want::Off {
                at = at.min(until);
            }
        }
        at
    }

    fn price_and_post(&mut self) -> Result<(), Stop> {
        let mid = match self.legs.mid(self.clock.instant()) {
            Mid::Ready(mid) => mid,
            Mid::Waiting => return Ok(()),
            Mid::Withdraw(reason) => return self.withdraw(&reason),
        };
        let state = MarketState {
            mid_e9: mid,
            inventory: self.view.inventory,
            max_skew_bps: self.view.max_skew_bps,
        };
        match self.step.run(&state).step {
            Step::Post(quote) => self.post(quote),
            Step::Withdraw { reason } => self.withdraw(&reason),
            // Logged by the step; the quote on the book stays (T030).
            Step::Skip(_) => Ok(()),
        }
    }

    fn post(&mut self, quote: Quote) -> Result<(), Stop> {
        if self.halted || self.sender.is_paused(self.clock.instant()) {
            return Ok(());
        }
        let now_slot = self.clock.now();
        match self.rule.on_quote(&quote, now_slot) {
            Ok(Update::Post(why)) => {
                info!(%why, mid_e9 = quote.mid_e9, spread_bps = quote.spread_bps, skew_bps = quote.skew_bps, "posting");
                self.send(Order::Post(quote), now_slot)
            }
            Ok(Update::Hold) => Ok(()),
            // `Quote::check` in the step stops these first.
            Err(error) => {
                warn!(%error, "the rule cannot compare this quote");
                Ok(())
            }
        }
    }

    fn withdraw(&mut self, reason: &str) -> Result<(), Stop> {
        self.want = Want::Off;
        if !self.rule.on_withdraw() {
            return Ok(());
        }
        info!(reason, "withdrawing the quote");
        let now_slot = self.clock.now();
        self.send(Order::Clear, now_slot)
    }

    fn send(&mut self, order: Order, now_slot: u64) -> Result<(), Stop> {
        let now = self.clock.instant();
        match self.sender.send(order, now_slot, self.view.book, now) {
            Ok(_) => Ok(()),
            Err(Refusal::Fatal(fatal)) => Err(fatal.into()),
            Err(refusal) => {
                // Whatever it was, the book is not known now; look at it.
                self.rule.forget();
                if refusal == Refusal::Halted {
                    warn!("the vault is halted: not posting until it resumes");
                    self.halted = true;
                }
                self.next_read = now_slot;
                Ok(())
            }
        }
    }

    fn refresh(&mut self, now_slot: u64) -> Result<(), Stop> {
        let view = match self.sender.read() {
            Ok(view) => view,
            Err(Refusal::Fatal(fatal)) => return Err(fatal.into()),
            Err(refusal) => {
                warn!(%refusal, "cannot read the vault");
                self.read_retry = now_slot + u64::from(LANDING_SLOTS);
                return Ok(());
            }
        };
        self.clock.anchor(view.slot);
        vet(&view, self.sender.signer())?;
        if view.max_quote_age_slots != self.view.max_quote_age_slots {
            warn!(
                from = self.view.max_quote_age_slots,
                to = view.max_quote_age_slots,
                "the vault's freshness limit changed"
            );
            let settings = Settings {
                threshold_bps: self.rule.threshold_bps(),
                heartbeat_slots: self.rule.heartbeat_slots(),
                base_feed: self.legs.base,
                quote_feed: self.legs.quote,
            };
            fits(&view, &settings, self.step.budget(), self.silence_bound)?;
        }
        if self.halted && !view.halted {
            info!("the vault resumed");
            self.rule.forget();
        }
        self.halted = view.halted;
        match self.sender.check(&view) {
            Check::Landed {
                quote_slot: Some(quote_slot),
            } => self.rule.landed_at(quote_slot),
            Check::Lost | Check::Diverged => self.rule.forget(),
            Check::Landed { quote_slot: None } | Check::InFlight | Check::Quiet => {}
        }
        self.next_read = view.slot + u64::from(self.rule.heartbeat_slots());
        self.view = view;
        Ok(())
    }
}

/// Stop if this key cannot quote for this vault.
fn vet(view: &VaultView, signer: anchor_lang::prelude::Pubkey) -> Result<(), Stop> {
    if view.pricing_authority != signer {
        return Err(Fatal::NotPricingAuthority {
            signer,
            detail: format!("the vault names {}", view.pricing_authority),
        }
        .into());
    }
    if view.payer_lamports < MIN_FEE_LAMPORTS {
        return Err(Fatal::NoFeeFunds {
            payer: signer,
            detail: format!("{} lamports left", view.payer_lamports),
        }
        .into());
    }
    Ok(())
}

/// Check the settings against the freshness limit on chain.
fn fits(
    view: &VaultView,
    settings: &Settings,
    budget: Budget,
    silence: Duration,
) -> Result<Rule, Stop> {
    let max_age = view.max_quote_age_slots;
    Budget::checked(budget.get(), max_age).map_err(Stop::Budget)?;
    Silence::checked(silence, max_age).map_err(Stop::Silence)?;
    Rule::checked(
        settings.threshold_bps,
        settings.heartbeat_slots,
        max_age,
        budget,
    )
    .map_err(Stop::Rule)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use solana_keypair::Keypair;
    use solana_signer::Signer as _;

    use super::*;
    use crate::chain::Book;
    use crate::feed::SLOT_DURATION;
    use crate::model::{Decision, ModelError};
    use crate::sender::tests::{book, deployment, view, Hands, Scripted};
    use crate::sender::LOST_AFTER_SLOTS;

    const BASE: PriceId = PriceId::from_bytes([0xB; 32]);
    const QUOTE: PriceId = PriceId::from_bytes([0xC; 32]);
    const HEARTBEAT: u32 = 12;
    const SILENCE: Duration = Duration::from_secs(2);

    /// Quotes the mid with a 10 bps half-spread, as if nothing else mattered.
    struct Flat;

    impl PricingModel for Flat {
        fn price(&mut self, state: &MarketState, _: Instant) -> Result<Decision, ModelError> {
            Ok(Decision::Quote(Quote {
                mid_e9: state.mid_e9,
                spread_bps: 10,
                skew_bps: 0,
                max_size_base: 1_000,
            }))
        }
    }

    /// SOL/USD at `usd`, as Pyth sends it (expo −8).
    fn price(id: PriceId, usd_e8: u64) -> Price {
        Price {
            id,
            mantissa: usd_e8,
            expo: -8,
            conf_bps: 2,
            publish_time: 1_790_000_000,
            slot: None,
        }
    }

    fn sol(usd_e8: u64) -> Price {
        price(BASE, usd_e8)
    }

    /// The quote a price turns into under `Flat`, with the deployment's decimals.
    fn quote_at(base: &Price) -> Quote {
        let d = deployment();
        Quote {
            mid_e9: mid_e9(base, None, d.base_decimals, d.quote_decimals).unwrap(),
            spread_bps: 10,
            skew_bps: 0,
            max_size_base: 1_000,
        }
    }

    fn settings(quote_feed: Option<PriceId>) -> Settings {
        Settings {
            threshold_bps: 5,
            heartbeat_slots: HEARTBEAT,
            base_feed: BASE,
            quote_feed,
        }
    }

    struct Rig {
        hands: Hands,
        key: anchor_lang::prelude::Pubkey,
    }

    impl Rig {
        fn view(&self, slot: u64, book: Option<Book>) -> VaultView {
            VaultView {
                pricing_authority: self.key,
                ..view(slot, book)
            }
        }

        /// Move time forward by whole slots.
        fn slots(&self, n: u32) {
            self.hands.advance(SLOT_DURATION * n);
        }
    }

    /// A cycle over a scripted chain whose first read is `first`; the rest of
    /// the reads are `reads(&rig)`.
    fn cycle_with(
        quote_feed: Option<PriceId>,
        first: impl FnOnce(&Rig) -> VaultView,
        reads: impl FnOnce(&Rig) -> Vec<VaultView>,
        sends: Vec<Result<(), Refusal>>,
    ) -> (Result<Cycle<Flat, Scripted, Hands>, Stop>, Rig) {
        let keypair = Keypair::new();
        let rig = Rig {
            hands: Hands::new(),
            key: keypair.pubkey(),
        };
        let mut script = VecDeque::from([Ok(first(&rig))]);
        script.extend(reads(&rig).into_iter().map(Ok));
        let chain = Scripted {
            reads: script,
            sends: sends.into(),
            ..Scripted::default()
        };
        let sender = Sender::new(chain, keypair, deployment());
        let step = ModelStep::with_clock(
            Flat,
            Budget::new(Duration::from_millis(50)),
            rig.hands.clone(),
        );
        let cycle = Cycle::start(
            step,
            sender,
            settings(quote_feed),
            SILENCE,
            rig.hands.clone(),
        );
        (cycle, rig)
    }

    fn cycle(
        reads: impl FnOnce(&Rig) -> Vec<VaultView>,
        sends: Vec<Result<(), Refusal>>,
    ) -> (Cycle<Flat, Scripted, Hands>, Rig) {
        let (cycle, rig) = cycle_with(None, |rig| rig.view(100, None), reads, sends);
        (cycle.expect("the defaults start"), rig)
    }

    fn sent(cycle: &Cycle<Flat, Scripted, Hands>) -> usize {
        cycle.sender().chain().sent.len()
    }

    fn reads(cycle: &Cycle<Flat, Scripted, Hands>) -> usize {
        cycle.sender().chain().reads_done
    }

    // ── Start ────────────────────────────────────────────────────────────────

    #[test]
    fn another_key_does_not_start() {
        let (cycle, _) = cycle_with(None, |_| view(100, None), |_| vec![], vec![]);
        assert!(matches!(
            cycle.err(),
            Some(Stop::Chain(Refusal::Fatal(
                Fatal::NotPricingAuthority { .. }
            )))
        ));
    }

    #[test]
    fn a_key_without_sol_does_not_start() {
        let (cycle, _) = cycle_with(
            None,
            |rig| VaultView {
                payer_lamports: MIN_FEE_LAMPORTS - 1,
                ..rig.view(100, None)
            },
            |_| vec![],
            vec![],
        );
        assert!(matches!(
            cycle.err(),
            Some(Stop::Chain(Refusal::Fatal(Fatal::NoFeeFunds { .. })))
        ));
    }

    /// FR-011a against the vault's own limit: 12 + 1 + 2 does not fit in 14.
    #[test]
    fn a_heartbeat_that_does_not_fit_the_vaults_limit_does_not_start() {
        let (cycle, _) = cycle_with(
            None,
            |rig| VaultView {
                max_quote_age_slots: 14,
                ..rig.view(100, None)
            },
            |_| vec![],
            vec![],
        );
        assert!(matches!(
            cycle.err(),
            Some(Stop::Rule(RuleConfigError::HeartbeatDoesNotFit { .. }))
        ));
    }

    #[test]
    fn a_silence_bound_past_the_vaults_limit_does_not_start() {
        let (cycle, _) = cycle_with(
            None,
            // 5 slots are 2 s: a silence bound of 2 s is not shorter than that.
            |rig| VaultView {
                max_quote_age_slots: 5,
                ..rig.view(100, None)
            },
            |_| vec![],
            vec![],
        );
        assert!(matches!(cycle.err(), Some(Stop::Silence(_))));
    }

    // ── Posting ──────────────────────────────────────────────────────────────

    #[test]
    fn the_first_price_is_posted_and_a_small_move_is_held() {
        let (mut cycle, _) = cycle(|_| vec![], vec![]);
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1);
        // 150.00 → 150.05 is 3.3 bps: under the 5 bps threshold.
        cycle.on_price(sol(150_05000000)).unwrap();
        assert_eq!(sent(&cycle), 1);
        // → 150.15 is 10 bps from the posted sides.
        cycle.on_price(sol(150_15000000)).unwrap();
        assert_eq!(sent(&cycle), 2);
    }

    #[test]
    fn the_book_is_read_after_a_send_and_a_landed_post_stands() {
        let posted = quote_at(&sol(150_00000000));
        let (mut cycle, rig) = cycle(|rig| vec![rig.view(102, Some(book(posted, 101)))], vec![]);
        cycle.on_price(sol(150_00000000)).unwrap();
        cycle.housekeeping().unwrap();
        assert_eq!(reads(&cycle), 1, "no read before the landing target");
        rig.slots(LANDING_SLOTS);
        cycle.housekeeping().unwrap();
        assert_eq!(reads(&cycle), 2);
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1, "the same price is held");
    }

    /// The loop keeps going while a post is in flight, and a post that never
    /// shows up is sent again rather than trusted.
    #[test]
    fn a_lost_post_is_sent_again_on_the_next_price() {
        let (mut cycle, rig) = cycle(|rig| vec![rig.view(102, None), rig.view(104, None)], vec![]);
        cycle.on_price(sol(150_00000000)).unwrap();
        rig.slots(LANDING_SLOTS);
        cycle.housekeeping().unwrap();
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1, "in flight: held");
        rig.slots(LOST_AFTER_SLOTS as u32 - LANDING_SLOTS);
        cycle.housekeeping().unwrap();
        assert_eq!(reads(&cycle), 3, "one read at the target, one at the limit");
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 2, "lost: the same quote goes out again");
    }

    /// The heartbeat does not wait for a new price (T031): the prices keep
    /// coming unchanged, and on the due slot the timer posts on the last one.
    #[test]
    fn the_heartbeat_posts_on_the_last_live_price() {
        let posted = quote_at(&sol(150_00000000));
        let (mut cycle, rig) = cycle(|rig| vec![rig.view(102, Some(book(posted, 101)))], vec![]);
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(cycle.rule().due_slot(), Some(100 + u64::from(HEARTBEAT)));
        for _ in 1..HEARTBEAT {
            rig.slots(1);
            cycle.on_price(sol(150_00000000)).unwrap();
            cycle.housekeeping().unwrap();
        }
        assert_eq!(sent(&cycle), 1, "unchanged prices are held");
        rig.slots(1);
        cycle.housekeeping().unwrap();
        assert_eq!(sent(&cycle), 2, "the heartbeat, with no new price");
    }

    /// The dangerous straggler: a post that lands after the withdrawal that
    /// replaced it puts a price back on a book that should be empty. The
    /// periodic read sees it and the withdrawal goes out again.
    #[test]
    fn a_post_landing_after_its_withdrawal_is_withdrawn_again() {
        let stale = quote_at(&sol(150_00000000));
        let (mut cycle, rig) = cycle(
            |rig| {
                vec![
                    rig.view(102, None),
                    rig.view(100 + u64::from(HEARTBEAT) + 2, Some(book(stale, 108))),
                ]
            },
            vec![],
        );
        cycle.on_price(sol(150_00000000)).unwrap();
        cycle.on_withdrawal("silent").unwrap();
        rig.slots(LANDING_SLOTS);
        cycle.housekeeping().unwrap();
        assert_eq!(sent(&cycle), 2, "a post and its withdrawal");
        rig.slots(HEARTBEAT);
        cycle.housekeeping().unwrap();
        assert_eq!(reads(&cycle), 3);
        assert_eq!(
            sent(&cycle),
            3,
            "the book is not what was sent: withdrawn again"
        );
    }

    // ── Withdrawing ──────────────────────────────────────────────────────────

    #[test]
    fn a_withdrawal_is_sent_once_and_the_next_price_quotes_again() {
        let (mut cycle, _) = cycle(|_| vec![], vec![]);
        cycle.on_price(sol(150_00000000)).unwrap();
        cycle.on_withdrawal("silent").unwrap();
        cycle.on_withdrawal("still silent").unwrap();
        cycle.housekeeping().unwrap();
        assert_eq!(sent(&cycle), 2, "one post, one withdrawal");
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 3, "resumed");
    }

    #[test]
    fn a_refused_withdrawal_is_sent_again_after_the_pause_not_before() {
        let busy = || {
            Err(Refusal::Transient {
                detail: "429".into(),
            })
        };
        let (mut cycle, rig) = cycle(|rig| vec![rig.view(100, None)], vec![busy(), Ok(())]);
        cycle.on_withdrawal("silent").unwrap();
        assert_eq!(sent(&cycle), 1);
        rig.hands.advance(Duration::from_millis(1));
        cycle.housekeeping().unwrap();
        assert_eq!(sent(&cycle), 1, "paused");
        rig.hands.advance(SLOT_DURATION);
        cycle.housekeeping().unwrap();
        assert_eq!(sent(&cycle), 2, "again once the pause is over");
    }

    #[test]
    fn a_silent_quote_leg_withdraws_the_cross() {
        let (cycle, rig) = cycle_with(Some(QUOTE), |rig| rig.view(100, None), |_| vec![], vec![]);
        let mut cycle = cycle.unwrap();
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 0, "the quote leg has a bound to arrive");
        cycle.on_price(price(QUOTE, 1_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1);
        rig.hands.advance(SILENCE);
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 2, "the USDC leg went quiet: withdrawn");
        assert_eq!(cycle.rule().due_slot(), None);
    }

    // ── Refusals ─────────────────────────────────────────────────────────────

    /// A halt stops posts, not the engine; a read that shows the vault
    /// running again resumes them (T032 decision).
    #[test]
    fn a_halted_vault_pauses_posting_until_it_resumes() {
        let (mut cycle, rig) = cycle(
            |rig| {
                vec![
                    VaultView {
                        halted: true,
                        ..rig.view(100, None)
                    },
                    rig.view(112, None),
                ]
            },
            vec![Err(Refusal::Halted)],
        );
        cycle.on_price(sol(150_00000000)).unwrap();
        assert!(cycle.is_halted());
        cycle.housekeeping().unwrap();
        cycle.on_price(sol(151_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1, "no posts while halted");
        rig.slots(HEARTBEAT);
        cycle.housekeeping().unwrap();
        assert!(!cycle.is_halted());
        cycle.on_price(sol(151_00000000)).unwrap();
        assert_eq!(sent(&cycle), 2);
    }

    #[test]
    fn a_fatal_refusal_stops_the_loop() {
        let (mut cycle, _) = cycle(
            |_| vec![],
            vec![Err(Refusal::Fatal(Fatal::NoProgram {
                detail: "gone".into(),
            }))],
        );
        assert!(matches!(
            cycle.on_price(sol(150_00000000)),
            Err(Stop::Chain(Refusal::Fatal(Fatal::NoProgram { .. })))
        ));
    }

    /// The owner handed the pricing authority to another key while the engine ran.
    #[test]
    fn a_rotated_pricing_authority_stops_the_loop_on_the_next_read() {
        let (mut cycle, rig) = cycle(|_| vec![view(112, None)], vec![]);
        rig.slots(HEARTBEAT);
        assert!(matches!(
            cycle.housekeeping(),
            Err(Stop::Chain(Refusal::Fatal(
                Fatal::NotPricingAuthority { .. }
            )))
        ));
    }

    #[test]
    fn a_post_the_program_refuses_is_forgotten_and_paused() {
        let (mut cycle, rig) = cycle(
            |rig| vec![rig.view(100, None)],
            vec![Err(Refusal::Rejected {
                detail: "InvalidQuote".into(),
            })],
        );
        cycle.on_price(sol(150_00000000)).unwrap();
        cycle.housekeeping().unwrap();
        assert_eq!(reads(&cycle), 2, "the book is read right after a refusal");
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 1, "paused");
        rig.hands.advance(SLOT_DURATION);
        cycle.on_price(sol(150_00000000)).unwrap();
        assert_eq!(sent(&cycle), 2, "forgotten, so the same quote goes again");
        assert!(cycle.rule().due_slot().is_some());
    }
}
