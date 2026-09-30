//! Sending with the `pricing_authority` key, and finding out what landed.
//!
//! # Sent is not landed
//!
//! The update rule compares against the last quote **sent** (T031), so the
//! loop does not wait for a transaction to confirm: it keeps pricing while the
//! post is in flight. The sender then checks the post on the book itself — the
//! vault holds the quote, so one read [`LANDING_SLOTS`] after the send says
//! whether it is there. Not there by [`LOST_AFTER_SLOTS`] means lost: the rule
//! forgets (T031, [`Rule::forget`](crate::policy::Rule::forget)) and the next
//! price goes out whatever it is.
//!
//! Reading the book rather than following signatures covers more for the same
//! call: a post that failed after preflight, a post that was dropped, and a
//! stale post that landed late *over* a newer one all look the same from here —
//! the book is not what was sent — and all need the same answer. The same read
//! refreshes the inventory and anchors the slot clock.
//!
//! # A heartbeat must not repeat a signature
//!
//! A heartbeat re-posts the same quote. Signed with the same blockhash it is
//! the same transaction byte for byte, the network drops it as a duplicate,
//! and the quote's slot is not refreshed — silently. So the blockhash is
//! cached (a fetch per send would double the calls), but never used twice for
//! the same message: a repeat fetches a new one.
//!
//! # After a refusal, a pause
//!
//! A refusal that a retry might fix — the node unreachable or rate-limiting,
//! the program refusing this one quote — costs a call per price if retried at
//! once. The sender pauses instead, doubling from a slot to [`SEND_BACKOFF_MAX`],
//! and a send that the node takes resets it. A withdrawal is not held back by
//! the pause: taking a price off the book is worth the call.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use propamm_client::chain::{clear_quote_ix, update_quote_ix};
use propamm_vault::instructions::update_quote::QuoteUpdate;
use solana_keypair::Keypair;
use solana_signer::Signer as _;
use tracing::{debug, info, warn};

use crate::chain::{Book, Chain, Deployment, Refusal, VaultView};
use crate::feed::SLOT_DURATION;
use crate::model::Quote;
use crate::policy::LANDING_SLOTS;
use crate::tick::Monotonic;

/// A post not on the book this many slots after it was sent is lost — twice
/// the SC-003 target. Calling a post lost too early costs one more post;
/// calling it lost too late leaves the book on the old price that long.
pub const LOST_AFTER_SLOTS: u64 = 2 * LANDING_SLOTS as u64;

/// A cached blockhash is refetched after this many slots. A transaction can
/// land for about 150 blocks after its blockhash; this keeps most of that.
pub const BLOCKHASH_MAX_AGE_SLOTS: u64 = 60;

/// The longest pause after refusals in a row.
pub const SEND_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// What the sender puts on chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// `update_quote`.
    Post(Quote),
    /// `clear_quote` (FR-014).
    Clear,
}

impl Order {
    /// Is this what the book shows?
    fn on(&self, book: Option<&Book>) -> bool {
        match (self, book) {
            (Self::Clear, None) => true,
            (Self::Post(quote), Some(book)) => {
                book.mid_e9 == quote.mid_e9
                    && book.spread_bps == quote.spread_bps
                    && book.skew_bps == quote.skew_bps
                    && book.max_size_base == quote.max_size_base
            }
            _ => false,
        }
    }
}

/// What a read says about the last send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Nothing to say: nothing sent, or it checked out before.
    Quiet,
    /// The order is on the book; for a post, the slot the chain stamped it with.
    Landed { quote_slot: Option<u64> },
    /// Not on the book yet, and not late yet.
    InFlight,
    /// Not on the book by [`LOST_AFTER_SLOTS`].
    Lost,
    /// Something else is on the book than what landed — a stale post that
    /// arrived late, or another engine with the same key.
    Diverged,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    order: Order,
    /// The engine's slot when it was sent.
    slot: u64,
    /// The book as last read before sending: a repeated quote has landed only
    /// once the chain restamps it.
    baseline: Option<Book>,
    /// A read has already found it in flight: the next look is the last one.
    looked: bool,
}

#[derive(Debug)]
struct CachedHash {
    hash: solana_hash::Hash,
    slot: u64,
    /// Signatures already sent with this blockhash.
    signed: HashSet<String>,
}

/// The engine's slot clock: the chain's slot, read at the tip, and between
/// reads the monotonic clock at [`SLOT_DURATION`] per slot.
///
/// Rounded **up**: a chain slower than 400 ms makes the clock run ahead, and a
/// heartbeat that comes early costs a transaction, while one that comes late
/// costs the quote (FR-011a). The next read pulls it back to the chain.
#[derive(Debug, Clone)]
pub struct SlotClock<C> {
    clock: C,
    slot: u64,
    at: Instant,
}

impl<C: Monotonic> SlotClock<C> {
    /// The clock reads `slot` now.
    pub fn anchored(clock: C, slot: u64) -> Self {
        let at = clock.now();
        Self { clock, slot, at }
    }

    /// Put the clock back on the chain's slot.
    pub fn anchor(&mut self, slot: u64) {
        self.slot = slot;
        self.at = self.clock.now();
    }

    /// The slot now.
    #[must_use]
    pub fn now(&self) -> u64 {
        let elapsed = self.clock.now().saturating_duration_since(self.at);
        let slots = elapsed.as_nanos().div_ceil(SLOT_DURATION.as_nanos());
        self.slot
            .saturating_add(u64::try_from(slots).unwrap_or(u64::MAX))
    }

    /// The monotonic instant at which [`SlotClock::now`] first reads `slot`.
    #[must_use]
    pub fn when(&self, slot: u64) -> Instant {
        let ahead = slot.saturating_sub(self.slot);
        if ahead == 0 {
            return self.at;
        }
        // `now` reads `slot + k` once more than `(k − 1)` slot durations passed.
        let whole = u32::try_from(ahead - 1).unwrap_or(u32::MAX);
        self.at + SLOT_DURATION.saturating_mul(whole) + Duration::from_nanos(1)
    }

    /// The monotonic clock behind it.
    pub fn instant(&self) -> Instant {
        self.clock.now()
    }
}

/// Signs with `pricing_authority`, sends, and checks the book.
pub struct Sender<K> {
    chain: K,
    key: Keypair,
    deployment: Deployment,
    hash: Option<CachedHash>,
    pending: Option<Pending>,
    /// The last order the book was seen to hold.
    settled: Option<Order>,
    pause_until: Option<Instant>,
    next_pause: Duration,
}

impl<K: Chain> Sender<K> {
    #[must_use]
    pub fn new(chain: K, key: Keypair, deployment: Deployment) -> Self {
        Self {
            chain,
            key,
            deployment,
            hash: None,
            pending: None,
            settled: None,
            pause_until: None,
            next_pause: SLOT_DURATION,
        }
    }

    /// The signer, which is also the fee payer.
    #[must_use]
    pub fn signer(&self) -> Pubkey {
        self.key.pubkey()
    }

    /// The vault and its pair.
    #[must_use]
    pub const fn deployment(&self) -> Deployment {
        self.deployment
    }

    /// Read the vault at the tip.
    ///
    /// # Errors
    ///
    /// The chain's refusal.
    pub fn read(&mut self) -> Result<VaultView, Refusal> {
        self.chain.read()
    }

    /// The network, e.g. to inspect a scripted one in tests.
    pub fn chain(&self) -> &K {
        &self.chain
    }

    /// Is a post (not a withdrawal) held back by a pause right now?
    #[must_use]
    pub fn is_paused(&self, now: Instant) -> bool {
        self.pause_until.is_some_and(|until| now < until)
    }

    /// When the pause ends, if one is on.
    #[must_use]
    pub fn pause_until(&self) -> Option<Instant> {
        self.pause_until
    }

    /// The slot at which the book should be read to check the last send: the
    /// SC-003 target first, then the last chance before it counts as lost.
    #[must_use]
    pub fn check_slot(&self) -> Option<u64> {
        self.pending.map(|pending| {
            let wait = if pending.looked {
                LOST_AFTER_SLOTS
            } else {
                u64::from(LANDING_SLOTS)
            };
            pending.slot + wait
        })
    }

    /// Sign and send `order` at `now_slot`; `baseline` is the book as last read.
    ///
    /// `Ok` is the signature: the node took the transaction. Whether it landed
    /// is [`Sender::check`]'s business.
    ///
    /// # Errors
    ///
    /// The refusal. A transient one starts or extends the pause.
    pub fn send(
        &mut self,
        order: Order,
        now_slot: u64,
        baseline: Option<Book>,
        now: Instant,
    ) -> Result<String, Refusal> {
        let result = match self.try_send(order, now_slot) {
            // Signed with a blockhash the node has already forgotten: once more
            // with a fresh one, before calling it a refusal.
            Err(Refusal::StaleBlockhash) => {
                self.hash = None;
                self.try_send(order, now_slot)
            }
            result => result,
        };
        match &result {
            Ok(signature) => {
                debug!(%signature, ?order, "sent");
                self.pending = Some(Pending {
                    order,
                    slot: now_slot,
                    baseline,
                    looked: false,
                });
                self.pause_until = None;
                self.next_pause = SLOT_DURATION;
            }
            Err(
                refusal @ (Refusal::Transient { .. }
                | Refusal::StaleBlockhash
                | Refusal::Rejected { .. }),
            ) => {
                let pause = self.next_pause;
                warn!(%refusal, pause_ms = pause.as_millis(), "send refused, pausing posts");
                self.pause_until = Some(now + pause);
                self.next_pause = (pause * 2).min(SEND_BACKOFF_MAX);
                self.pending = None;
            }
            Err(Refusal::Halted | Refusal::Fatal(_)) => self.pending = None,
        }
        result
    }

    fn try_send(&mut self, order: Order, now_slot: u64) -> Result<String, Refusal> {
        let (wire, signature) = self.sign(order, now_slot)?;
        self.chain.send(&wire)?;
        if let Some(cached) = &mut self.hash {
            cached.signed.insert(signature.clone());
        }
        Ok(signature)
    }

    /// Sign `order` with a blockhash it has not been sent with before.
    fn sign(&mut self, order: Order, now_slot: u64) -> Result<(Vec<u8>, String), Refusal> {
        let stale = self
            .hash
            .as_ref()
            .is_none_or(|cached| now_slot.saturating_sub(cached.slot) >= BLOCKHASH_MAX_AGE_SLOTS);
        if stale {
            self.refresh_hash(now_slot)?;
        }
        let signed = self.sign_with_cached(order)?;
        let repeated = self
            .hash
            .as_ref()
            .is_some_and(|cached| cached.signed.contains(&signed.1));
        if !repeated {
            return Ok(signed);
        }
        debug!("the same transaction was sent with this blockhash; fetching a new one");
        self.refresh_hash(now_slot)?;
        self.sign_with_cached(order)
    }

    fn refresh_hash(&mut self, now_slot: u64) -> Result<(), Refusal> {
        let hash = self.chain.blockhash()?;
        self.hash = Some(CachedHash {
            hash,
            slot: now_slot,
            signed: HashSet::new(),
        });
        Ok(())
    }

    fn sign_with_cached(&self, order: Order) -> Result<(Vec<u8>, String), Refusal> {
        let Some(cached) = &self.hash else {
            return Err(Refusal::StaleBlockhash);
        };
        let signer = self.key.pubkey();
        let Deployment {
            program_id, vault, ..
        } = self.deployment;
        let instruction = match order {
            Order::Post(quote) => update_quote_ix(
                &program_id,
                &signer,
                &vault,
                QuoteUpdate {
                    mid_e9: quote.mid_e9,
                    spread_bps: quote.spread_bps,
                    skew_bps: quote.skew_bps,
                    max_size_base: quote.max_size_base,
                },
            ),
            Order::Clear => clear_quote_ix(&program_id, &signer, &vault),
        };
        let message = solana_message::Message::new(&[instruction], Some(&signer));
        let mut transaction = solana_transaction::Transaction::new_unsigned(message);
        transaction
            .try_sign(&[&self.key], cached.hash)
            .map_err(|error| Refusal::Rejected {
                detail: format!("the transaction cannot be signed: {error}"),
            })?;
        let signature = transaction.signatures[0].to_string();
        let wire = bincode::serialize(&transaction).map_err(|error| Refusal::Rejected {
            detail: format!("the transaction does not serialize: {error}"),
        })?;
        Ok((wire, signature))
    }

    /// What `view` — read at `view.slot` — says about the last send.
    pub fn check(&mut self, view: &VaultView) -> Check {
        let book = view.book.as_ref();
        if let Some(pending) = self.pending {
            if landed(&pending, book) {
                self.pending = None;
                self.settled = Some(pending.order);
                info!(order = ?pending.order, slot = view.slot, "on the book");
                return Check::Landed {
                    quote_slot: book.map(|book| book.quote_slot),
                };
            }
            if view.slot < pending.slot + LOST_AFTER_SLOTS {
                if let Some(pending) = &mut self.pending {
                    pending.looked = true;
                }
                return Check::InFlight;
            }
            self.pending = None;
            self.settled = None;
            warn!(order = ?pending.order, sent_slot = pending.slot, slot = view.slot, "not on the book: lost");
            return Check::Lost;
        }
        match self.settled {
            Some(order) if order.on(book) => Check::Landed {
                quote_slot: book.map(|book| book.quote_slot),
            },
            Some(order) => {
                self.settled = None;
                warn!(expected = ?order, ?book, "the book is not what this engine put there");
                Check::Diverged
            }
            None => Check::Quiet,
        }
    }
}

/// Is the pending order on `book`?
fn landed(pending: &Pending, book: Option<&Book>) -> bool {
    if !pending.order.on(book) {
        return false;
    }
    let (Order::Post(_), Some(book)) = (pending.order, book) else {
        // A withdrawal: no quote is no quote, whoever cleared it.
        return true;
    };
    match &pending.baseline {
        // The same quote as before the send: only a newer stamp says it is ours.
        Some(before) if Order::Post(as_quote(before)).on(Some(book)) => {
            book.quote_slot > before.quote_slot
        }
        _ => true,
    }
}

fn as_quote(book: &Book) -> Quote {
    Quote {
        mid_e9: book.mid_e9,
        spread_bps: book.spread_bps,
        skew_bps: book.skew_bps,
        max_size_base: book.max_size_base,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use propamm_quote::Inventory;

    use super::*;
    use crate::chain::Fatal;

    /// A monotonic clock the test moves.
    #[derive(Clone)]
    pub(crate) struct Hands(pub Rc<Cell<Instant>>);

    impl Hands {
        pub(crate) fn new() -> Self {
            Self(Rc::new(Cell::new(Instant::now())))
        }

        pub(crate) fn advance(&self, by: Duration) {
            self.0.set(self.0.get() + by);
        }
    }

    impl Monotonic for Hands {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    /// A chain that answers from a script and keeps what it was sent.
    #[derive(Default)]
    pub(crate) struct Scripted {
        pub reads: VecDeque<Result<VaultView, Refusal>>,
        pub sends: VecDeque<Result<(), Refusal>>,
        pub hashes: u8,
        pub sent: Vec<Vec<u8>>,
        pub reads_done: usize,
    }

    impl Chain for Scripted {
        fn read(&mut self) -> Result<VaultView, Refusal> {
            self.reads_done += 1;
            self.reads.pop_front().expect("a scripted read")
        }

        fn blockhash(&mut self) -> Result<solana_hash::Hash, Refusal> {
            self.hashes += 1;
            Ok(solana_hash::Hash::new_from_array([self.hashes; 32]))
        }

        fn send(&mut self, wire: &[u8]) -> Result<(), Refusal> {
            self.sent.push(wire.to_vec());
            self.sends.pop_front().unwrap_or(Ok(()))
        }
    }

    pub(crate) fn deployment() -> Deployment {
        Deployment {
            program_id: propamm_vault::ID,
            vault: Pubkey::new_from_array([1; 32]),
            base_decimals: 9,
            quote_decimals: 6,
        }
    }

    pub(crate) fn quote(mid_e9: u128) -> Quote {
        Quote {
            mid_e9,
            spread_bps: 10,
            skew_bps: 0,
            max_size_base: 1_000,
        }
    }

    pub(crate) fn book(quote: Quote, quote_slot: u64) -> Book {
        Book {
            mid_e9: quote.mid_e9,
            spread_bps: quote.spread_bps,
            skew_bps: quote.skew_bps,
            max_size_base: quote.max_size_base,
            quote_slot,
        }
    }

    pub(crate) fn view(slot: u64, book: Option<Book>) -> VaultView {
        VaultView {
            slot,
            halted: false,
            pricing_authority: Pubkey::default(),
            max_quote_age_slots: 25,
            max_skew_bps: 2_000,
            inventory: Inventory {
                base_amount: 10_000_000_000,
                quote_amount: 1_500_000_000,
            },
            book,
            payer_lamports: 1_000_000_000,
        }
    }

    fn sender(chain: Scripted) -> Sender<Scripted> {
        Sender::new(chain, Keypair::new(), deployment())
    }

    fn signature_of(wire: &[u8]) -> String {
        let transaction: solana_transaction::Transaction = bincode::deserialize(wire).unwrap();
        transaction.signatures[0].to_string()
    }

    #[test]
    fn a_post_is_signed_by_the_pricing_authority_for_the_vault() {
        let mut sender = sender(Scripted::default());
        let now = Instant::now();
        let signature = sender.send(Order::Post(quote(150)), 10, None, now).unwrap();
        let wire = &sender.chain().sent[0];
        let transaction: solana_transaction::Transaction = bincode::deserialize(wire).unwrap();
        assert_eq!(transaction.signatures[0].to_string(), signature);
        assert!(transaction.verify().is_ok(), "the signature must verify");
        let message = &transaction.message;
        assert_eq!(message.account_keys[0], sender.signer(), "the payer signs");
        assert_eq!(message.instructions.len(), 1, "one instruction: ours");
        assert!(message.account_keys.contains(&deployment().vault));
        assert!(message.account_keys.contains(&propamm_vault::ID));
    }

    /// The silent failure this module exists to prevent: a heartbeat of the
    /// same quote, with the same blockhash, is the same transaction.
    #[test]
    fn a_repeated_quote_never_goes_out_with_the_same_signature() {
        let mut sender = sender(Scripted::default());
        let now = Instant::now();
        let first = sender.send(Order::Post(quote(150)), 10, None, now).unwrap();
        let second = sender.send(Order::Post(quote(150)), 22, None, now).unwrap();
        assert_ne!(first, second);
        assert_eq!(sender.chain().hashes, 2, "a new blockhash for the repeat");
    }

    /// And the cache does its job otherwise: different quotes share a blockhash.
    #[test]
    fn different_quotes_share_the_cached_blockhash() {
        let mut sender = sender(Scripted::default());
        let now = Instant::now();
        sender.send(Order::Post(quote(150)), 10, None, now).unwrap();
        sender.send(Order::Post(quote(151)), 11, None, now).unwrap();
        sender.send(Order::Clear, 12, None, now).unwrap();
        assert_eq!(sender.chain().hashes, 1);
    }

    #[test]
    fn an_old_blockhash_is_refetched() {
        let mut sender = sender(Scripted::default());
        let now = Instant::now();
        sender.send(Order::Post(quote(150)), 10, None, now).unwrap();
        sender
            .send(
                Order::Post(quote(151)),
                10 + BLOCKHASH_MAX_AGE_SLOTS,
                None,
                now,
            )
            .unwrap();
        assert_eq!(sender.chain().hashes, 2);
    }

    #[test]
    fn a_blockhash_the_node_forgot_is_retried_once_with_a_new_one() {
        let chain = Scripted {
            sends: VecDeque::from([Err(Refusal::StaleBlockhash), Ok(())]),
            ..Scripted::default()
        };
        let mut sender = sender(chain);
        let now = Instant::now();
        let signature = sender.send(Order::Post(quote(150)), 10, None, now).unwrap();
        let sent = &sender.chain().sent;
        assert_eq!(sent.len(), 2);
        assert_eq!(signature, signature_of(&sent[1]));
        assert!(!sender.is_paused(now));
    }

    #[test]
    fn refusals_in_a_row_pause_posts_longer_each_time_and_a_success_resets() {
        let busy = || {
            Err(Refusal::Transient {
                detail: "429".into(),
            })
        };
        let chain = Scripted {
            sends: VecDeque::from([busy(), busy(), Ok(()), busy()]),
            ..Scripted::default()
        };
        let mut sender = sender(chain);
        let now = Instant::now();
        let order = Order::Post(quote(150));
        assert!(sender.send(order, 10, None, now).is_err());
        assert_eq!(sender.pause_until(), Some(now + SLOT_DURATION));
        assert!(sender.is_paused(now));
        assert!(sender.send(order, 11, None, now).is_err());
        assert_eq!(sender.pause_until(), Some(now + SLOT_DURATION * 2));
        sender.send(order, 12, None, now).unwrap();
        assert!(!sender.is_paused(now));
        assert!(sender.send(order, 13, None, now).is_err());
        assert_eq!(
            sender.pause_until(),
            Some(now + SLOT_DURATION),
            "reset by the success"
        );
    }

    #[test]
    fn the_pause_stops_growing_at_its_ceiling() {
        let chain = Scripted {
            sends: (0..20)
                .map(|_| {
                    Err(Refusal::Transient {
                        detail: "down".into(),
                    })
                })
                .collect(),
            ..Scripted::default()
        };
        let mut sender = sender(chain);
        let now = Instant::now();
        for slot in 0..20 {
            let _ = sender.send(Order::Clear, slot, None, now);
        }
        assert_eq!(sender.pause_until(), Some(now + SEND_BACKOFF_MAX));
    }

    #[test]
    fn a_halt_or_a_fatal_refusal_does_not_pause() {
        let chain = Scripted {
            sends: VecDeque::from([
                Err(Refusal::Halted),
                Err(Refusal::Fatal(Fatal::NoProgram { detail: "x".into() })),
            ]),
            ..Scripted::default()
        };
        let mut sender = sender(chain);
        let now = Instant::now();
        assert_eq!(
            sender.send(Order::Post(quote(1)), 1, None, now),
            Err(Refusal::Halted)
        );
        assert!(sender.send(Order::Post(quote(2)), 2, None, now).is_err());
        assert!(!sender.is_paused(now));
    }

    // ── What the book says ───────────────────────────────────────────────────

    #[test]
    fn a_post_on_the_book_has_landed() {
        let mut sender = sender(Scripted::default());
        sender
            .send(Order::Post(quote(150)), 10, None, Instant::now())
            .unwrap();
        assert_eq!(sender.check_slot(), Some(10 + u64::from(LANDING_SLOTS)));
        assert_eq!(
            sender.check(&view(12, Some(book(quote(150), 11)))),
            Check::Landed {
                quote_slot: Some(11)
            }
        );
        assert_eq!(sender.check_slot(), None);
    }

    #[test]
    fn a_post_not_on_the_book_is_in_flight_then_lost() {
        let mut sender = sender(Scripted::default());
        let old = Some(book(quote(100), 5));
        sender
            .send(Order::Post(quote(150)), 10, old, Instant::now())
            .unwrap();
        assert_eq!(sender.check(&view(12, old)), Check::InFlight);
        assert_eq!(
            sender.check_slot(),
            Some(10 + LOST_AFTER_SLOTS),
            "one more look, not one per slot"
        );
        assert_eq!(
            sender.check(&view(10 + LOST_AFTER_SLOTS - 1, old)),
            Check::InFlight
        );
        assert_eq!(sender.check(&view(10 + LOST_AFTER_SLOTS, old)), Check::Lost);
        assert_eq!(sender.check(&view(20, old)), Check::Quiet, "forgotten");
    }

    /// A heartbeat repeats the quote: until the chain restamps it, what is on
    /// the book is the previous post, not this one.
    #[test]
    fn a_repeated_quote_lands_only_when_restamped() {
        let mut sender = sender(Scripted::default());
        let before = book(quote(150), 5);
        sender
            .send(Order::Post(quote(150)), 17, Some(before), Instant::now())
            .unwrap();
        assert_eq!(sender.check(&view(19, Some(before))), Check::InFlight);
        assert_eq!(
            sender.check(&view(20, Some(book(quote(150), 18)))),
            Check::Landed {
                quote_slot: Some(18)
            }
        );
    }

    #[test]
    fn a_withdrawal_has_landed_when_there_is_no_quote() {
        let mut sender = sender(Scripted::default());
        let old = Some(book(quote(150), 5));
        sender.send(Order::Clear, 10, old, Instant::now()).unwrap();
        assert_eq!(sender.check(&view(12, old)), Check::InFlight);
        assert_eq!(
            sender.check(&view(13, None)),
            Check::Landed { quote_slot: None }
        );
    }

    /// A stale post that lands late over a newer one — or a second engine
    /// with the same key — shows up on the next read.
    #[test]
    fn a_book_that_changed_behind_the_engine_is_diverged() {
        let mut sender = sender(Scripted::default());
        sender
            .send(Order::Post(quote(150)), 10, None, Instant::now())
            .unwrap();
        sender.check(&view(12, Some(book(quote(150), 11))));
        assert_eq!(
            sender.check(&view(24, Some(book(quote(150), 11)))),
            Check::Landed {
                quote_slot: Some(11)
            },
            "still ours"
        );
        assert_eq!(
            sender.check(&view(30, Some(book(quote(140), 27)))),
            Check::Diverged
        );
        assert_eq!(
            sender.check(&view(40, Some(book(quote(140), 27)))),
            Check::Quiet
        );
    }

    #[test]
    fn a_refused_send_leaves_nothing_pending() {
        let chain = Scripted {
            sends: VecDeque::from([Err(Refusal::Rejected { detail: "x".into() })]),
            ..Scripted::default()
        };
        let mut sender = sender(chain);
        let _ = sender.send(Order::Post(quote(150)), 10, None, Instant::now());
        assert_eq!(sender.check_slot(), None);
        assert_eq!(sender.check(&view(20, None)), Check::Quiet);
    }

    // ── The slot clock ───────────────────────────────────────────────────────

    #[test]
    fn the_slot_clock_counts_started_slots_from_its_anchor() {
        let hands = Hands::new();
        let mut clock = SlotClock::anchored(hands.clone(), 100);
        assert_eq!(clock.now(), 100);
        hands.advance(Duration::from_millis(1));
        assert_eq!(clock.now(), 101, "rounded up: early, never late");
        hands.advance(SLOT_DURATION - Duration::from_millis(1));
        assert_eq!(clock.now(), 101);
        hands.advance(Duration::from_millis(1));
        assert_eq!(clock.now(), 102);
        clock.anchor(101);
        assert_eq!(clock.now(), 101, "the chain has the last word");
    }

    #[test]
    fn the_slot_clock_says_when_it_will_read_a_slot() {
        let hands = Hands::new();
        let clock = SlotClock::anchored(hands.clone(), 100);
        for target in [100, 101, 112] {
            let at = clock.when(target);
            hands.0.set(at);
            assert!(clock.now() >= target, "{target}");
            hands.0.set(at - Duration::from_nanos(1));
            assert!(target == 100 || clock.now() < target, "{target}");
        }
    }
}
