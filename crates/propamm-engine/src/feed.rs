//! Price feed: Pyth Hermes over SSE with the timestamp and confidence check (FR-012).
//!
//! # What this module is for
//!
//! The program reads no oracle on chain — that is where the SC-002 budget goes
//! (PLAN). So the engine reads Hermes, Pyth's off-chain service, over a
//! Server-Sent Events stream and turns every update into one of two things: an
//! accepted [`Price`] or a [`Reject`] with the reason. The tick (T030/T031)
//! never sees a raw sample. What leaves this module has already been checked,
//! so a stale or wide price cannot slip through by a forgotten `if` downstream.
//!
//! # Why the checks live here and not in the tick
//!
//! FR-012 names two guards: the publish time and the confidence interval. Both
//! are properties of the sample, not of the market state, so they sit next to
//! parsing — and are tested next to it, on recorded events, without a network.
//! The one stateful guard, "newer than the previous sample of this id", is a
//! defence against a replay after a reconnect and lives in the [`Reader`].
//!
//! # Why blocking I/O rather than an async client
//!
//! `ureq` with rustls is already the workspace's HTTP client (the T023 decision
//! for the CLI). An SSE stream is a plain HTTP body read line by line, and
//! `BufRead::lines` is enough for it. The reader runs on its own thread and
//! hands events over an [`mpsc`] channel, and [`Watch`] on the other end turns
//! silence on that channel into a withdrawal (FR-014). No async HTTP crate — no
//! second TLS stack in the lock.
//!
//! # Silence is a state, not an absence
//!
//! A market maker that repeats its last known price while the feed is quiet is
//! quoting into a market it cannot see. So the quote is live only while a price
//! that passed the policy is younger than a configured bound; past it, or on a
//! sample the policy refused, [`Silence`] says to take the quote off the book
//! (FR-014). The bound is a deployment setting and must be shorter than the
//! vault's own freshness limit — [`Silence::checked`] refuses one that is not.
//!
//! # A hung socket is bounded, not detected
//!
//! `ureq` has no per-read timeout: its body timeout is a budget for the whole
//! body, not restarted on each read. A TCP connection that dies without a reset
//! would therefore block the reader forever. So every session is given a total
//! budget ([`Https::new`]), after which the stream is reopened regardless. That
//! bounds the damage to one session length; the quote itself is withdrawn much
//! sooner by the consumer's silence rule.
//!
//! # Hermes requires a key since 2026-08-26
//!
//! The Pyth Core upgrade put the public `hermes.pyth.network` behind an API key:
//! without one every `/v2/updates/*` route answers `401 unauthorized` (checked
//! 2026-09-21). The key goes in `Authorization: Bearer …`; the upgraded base URL
//! is `https://pyth.dourolabs.app/hermes`. Routes and response shapes did not
//! change, and a free trial key exists (Pyth Terminal).
//!
//! # Scale — the place where the engine can silently diverge from the program
//!
//! Hermes gives `mantissa × 10^expo` in USD per **human** unit of the asset. The
//! program wants `mid_e9`: **raw** units of quote per raw unit of base, × 1e9,
//! and it does not know the mints' decimals (reading them would cost two
//! accounts in the swap). [`mid_e9`] is the one function that does that
//! conversion, in integer arithmetic, and it is tested on the same SOL/USDC
//! example as `forge quote` uses: 150 USDC per SOL is `150_000_000`, not `150e9`.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::io::{BufRead, BufReader};
use std::str::FromStr;
use std::sync::mpsc::{Receiver, RecvError, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use propamm_quote::BPS_DENOM;
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

/// The SSE route of Hermes, relative to the base URL.
pub const HERMES_STREAM_PATH: &str = "/v2/updates/price/stream";

/// The upgraded Hermes base URL — the one that takes the API key.
pub const HERMES_DEFAULT_URL: &str = "https://pyth.dourolabs.app/hermes";

/// How long we wait for the connection and for the response headers. The body
/// is a stream and has its own budget — see [`Https::new`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default total budget of one SSE session, after which the stream is reopened.
///
/// This is not a keep-alive but the bound on a hung socket (see the module
/// docs). Five minutes: long enough that reconnects are not noise in the log,
/// short enough that a dead connection does not outlive a demo window.
pub const DEFAULT_SESSION_LIMIT: Duration = Duration::from_secs(300);

/// How far ahead of the local clock a publish time may be before it is refused.
///
/// The Pyth aggregate is stamped by the publishers' clocks, ours may lag by a
/// second or two. Beyond this the sample is a clock fault, not a fresh price.
pub const DEFAULT_MAX_CLOCK_SKEW: Duration = Duration::from_secs(5);

/// Power of ten in [`propamm_quote::PRICE_SCALE`].
const PRICE_SCALE_POW: u32 = 9;

// If the program's scale ever changes, this fails to compile rather than posting a price 1e9 off.
const _: () = assert!(10u128.pow(PRICE_SCALE_POW) == propamm_quote::PRICE_SCALE);

// ─── Identifiers and samples ────────────────────────────────────────────────

/// A Pyth price feed id — 32 bytes, written as 64 hex digits with or without `0x`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PriceId([u8; 32]);

impl PriceId {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for PriceId {
    type Err = FeedError;

    fn from_str(text: &str) -> Result<Self, FeedError> {
        let hex = text.strip_prefix("0x").unwrap_or(text);
        let bad = || FeedError::BadPriceId(text.to_owned());
        if hex.len() != 64 || !hex.is_ascii() {
            return Err(bad());
        }
        let mut bytes = [0u8; 32];
        for (slot, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for PriceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for PriceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PriceId({self})")
    }
}

/// One price update as Hermes sent it — parsed, not yet judged.
///
/// `price` and `conf` are in the same fixed point: the real value is `x × 10^expo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub id: PriceId,
    /// Signed in the wire format; a non-positive price is refused by [`Policy::check`].
    pub price: i64,
    pub conf: u64,
    pub expo: i32,
    /// Unix seconds, stamped by the Pyth aggregate.
    pub publish_time: i64,
    /// Pythnet slot of the aggregate, when Hermes reports it.
    pub slot: Option<u64>,
}

/// A price that passed [`Policy::check`] — the only form the tick ever sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub id: PriceId,
    /// Positive by construction: the real value is `mantissa × 10^expo`.
    pub mantissa: u64,
    pub expo: i32,
    /// The confidence interval as a share of the price, rounded up.
    pub conf_bps: u32,
    pub publish_time: i64,
    pub slot: Option<u64>,
}

// ─── Errors and rejections ──────────────────────────────────────────────────

/// Why a sample was not turned into a [`Price`].
///
/// A rejection is an event, not an error: the reader keeps going, and the tick
/// learns the feed is alive but not usable — which is a different state from
/// silence (FR-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Reject {
    #[error("price is not positive: {price}")]
    NonPositivePrice { price: i64 },
    #[error("stale: published {age_secs} s ago, limit {limit_secs} s")]
    Stale { age_secs: u64, limit_secs: u64 },
    #[error(
        "from the future: {ahead_secs} s ahead of the local clock, tolerance {tolerance_secs} s"
    )]
    FromFuture {
        ahead_secs: u64,
        tolerance_secs: u64,
    },
    #[error("confidence too wide: {conf_bps} bps, limit {limit_bps} bps")]
    WideConfidence { conf_bps: u32, limit_bps: u32 },
    #[error("not newer than the previous sample: published {publish_time}, previous {previous}")]
    NotNewer { publish_time: i64, previous: i64 },
}

/// What can go wrong between the socket and a [`Sample`].
#[derive(Debug, Error)]
pub enum FeedError {
    #[error("\"{0}\" is not a price id: expected 32 bytes as 64 hex digits, with or without 0x")]
    BadPriceId(String),
    #[error(
        "Hermes answered 401 unauthorized: since 2026-08-26 the public Hermes needs an API key (PYTH_API_KEY)"
    )]
    Unauthorized,
    #[error("transport: {0}")]
    Transport(String),
    #[error("reading the stream: {0}")]
    Io(#[from] std::io::Error),
    #[error("the server closed the stream")]
    StreamEnded,
    #[error("event is not a Hermes price update: {0}")]
    BadEvent(#[from] serde_json::Error),
    #[error("event has no `parsed` block — was the stream opened with parsed=true?")]
    NoParsed,
    #[error("{field} \"{value}\" in the event is not a number")]
    BadNumber { field: &'static str, value: String },
}

/// Why a [`Price`] could not be expressed as `mid_e9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScaleError {
    #[error("the intermediate product does not fit in u128")]
    Overflow,
    #[error("the mid turns into zero on this pair: one raw unit of base costs less than 1e-9 of a raw unit of quote")]
    Zero,
}

// ─── SSE framing ────────────────────────────────────────────────────────────

/// Assembles SSE events from lines.
///
/// Only the `data:` field matters to us: Hermes sends one JSON document per
/// event, occasionally split over several `data:` lines. Comment lines (`:`),
/// `event:`, `id:` and `retry:` are skipped. A blank line dispatches the event.
#[derive(Debug, Default)]
pub struct SseParser {
    data: String,
    has_data: bool,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one line (without its line ending; a trailing `\r` is tolerated).
    /// Returns the event's data once a blank line closes it.
    pub fn feed_line(&mut self, line: &str) -> Option<String> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if !self.has_data {
                return None;
            }
            self.has_data = false;
            return Some(std::mem::take(&mut self.data));
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field != "data" {
            return None;
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(value);
        self.has_data = true;
        None
    }
}

// ─── Hermes wire format ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WireUpdate {
    #[serde(default)]
    parsed: Option<Vec<WireParsed>>,
}

#[derive(Deserialize)]
struct WireParsed {
    id: String,
    price: WirePrice,
    #[serde(default)]
    metadata: Option<WireMetadata>,
}

/// Hermes writes `price` and `conf` as strings "to avoid precision loss" in JS.
#[derive(Deserialize)]
struct WirePrice {
    price: String,
    conf: String,
    expo: i32,
    publish_time: i64,
}

#[derive(Deserialize)]
struct WireMetadata {
    #[serde(default)]
    slot: Option<u64>,
}

/// Parse the `data` of one SSE event into the samples it carries.
///
/// # Errors
///
/// If the JSON is not a Hermes price update, has no `parsed` block, or a number
/// field does not parse. The binary (VAA) part is ignored: the engine does not
/// post Pyth updates on chain.
pub fn parse_event(data: &str) -> Result<Vec<Sample>, FeedError> {
    let update: WireUpdate = serde_json::from_str(data)?;
    let parsed = update.parsed.ok_or(FeedError::NoParsed)?;
    parsed
        .into_iter()
        .map(|entry| {
            Ok(Sample {
                id: entry.id.parse()?,
                price: number("price", &entry.price.price)?,
                conf: number("conf", &entry.price.conf)?,
                expo: entry.price.expo,
                publish_time: entry.price.publish_time,
                slot: entry.metadata.and_then(|m| m.slot),
            })
        })
        .collect()
}

fn number<T: FromStr>(field: &'static str, value: &str) -> Result<T, FeedError> {
    value.parse().map_err(|_| FeedError::BadNumber {
        field,
        value: value.to_owned(),
    })
}

// ─── The FR-012 policy ──────────────────────────────────────────────────────

/// The two FR-012 thresholds plus the clock tolerance — deployment settings, not code constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Oldest publish time we still accept, relative to the local clock.
    pub max_age: Duration,
    /// Widest confidence interval we still accept, as a share of the price.
    pub max_conf_bps: u32,
    /// How far ahead of the local clock a publish time may be.
    pub max_clock_skew: Duration,
}

impl Policy {
    #[must_use]
    pub const fn new(max_age: Duration, max_conf_bps: u32) -> Self {
        Self {
            max_age,
            max_conf_bps,
            max_clock_skew: DEFAULT_MAX_CLOCK_SKEW,
        }
    }

    /// Judge one sample against the local clock (`now_unix` in seconds).
    ///
    /// # Errors
    ///
    /// The reason the sample is unusable — see [`Reject`]. Checks run in the
    /// order sign → clock → confidence, so the reported reason is the first one
    /// hit, not the last one.
    pub fn check(&self, sample: &Sample, now_unix: i64) -> Result<Price, Reject> {
        let mantissa = u64::try_from(sample.price)
            .ok()
            .filter(|price| *price > 0)
            .ok_or(Reject::NonPositivePrice {
                price: sample.price,
            })?;

        let age = now_unix.saturating_sub(sample.publish_time);
        if age < 0 {
            let ahead_secs = age.unsigned_abs();
            let tolerance_secs = self.max_clock_skew.as_secs();
            if ahead_secs > tolerance_secs {
                return Err(Reject::FromFuture {
                    ahead_secs,
                    tolerance_secs,
                });
            }
        } else {
            let age_secs = age.unsigned_abs();
            let limit_secs = self.max_age.as_secs();
            if age_secs > limit_secs {
                return Err(Reject::Stale {
                    age_secs,
                    limit_secs,
                });
            }
        }

        let conf_bps = conf_bps(sample.conf, mantissa);
        if conf_bps > self.max_conf_bps {
            return Err(Reject::WideConfidence {
                conf_bps,
                limit_bps: self.max_conf_bps,
            });
        }

        Ok(Price {
            id: sample.id,
            mantissa,
            expo: sample.expo,
            conf_bps,
            publish_time: sample.publish_time,
            slot: sample.slot,
        })
    }
}

/// The confidence interval as basis points of the price, rounded **up**: an
/// interval that is 30.01 bps wide is wider than a 30 bps limit.
fn conf_bps(conf: u64, price: u64) -> u32 {
    let bps = (u128::from(conf) * u128::from(BPS_DENOM)).div_ceil(u128::from(price));
    u32::try_from(bps).unwrap_or(u32::MAX)
}

// ─── Scale ──────────────────────────────────────────────────────────────────

/// The mid in the program's form: raw units of quote per raw unit of base, × 1e9.
///
/// `base` is the feed of the base asset in USD; `quote` is the feed of the quote
/// asset in USD, or `None` to take the quote asset as exactly one USD (a
/// stablecoin the deployment chooses to trust at par). Both must have passed
/// [`Policy::check`] — this function does not look at the clock.
///
/// The result is rounded **down**; that is the mid, not a side price, and the
/// two places where rounding has a direction are in `propamm-quote`.
///
/// # Errors
///
/// If an intermediate product does not fit in `u128`, or the mid is zero — the
/// program reads a zero mid as "no quote", so it must never be produced by accident.
pub fn mid_e9(
    base: &Price,
    quote: Option<&Price>,
    base_decimals: u8,
    quote_decimals: u8,
) -> Result<u128, ScaleError> {
    let (quote_mantissa, quote_expo) = quote.map_or((1, 0), |q| (q.mantissa, q.expo));

    // base.mantissa × 10^base.expo        USD per human base
    // ───────────────────────────── ×  10^(quote_decimals − base_decimals) × 10^9
    // quote_mantissa × 10^quote_expo      USD per human quote
    let power = i64::from(base.expo) - i64::from(quote_expo) + i64::from(quote_decimals)
        - i64::from(base_decimals)
        + i64::from(PRICE_SCALE_POW);

    let mut numerator = u128::from(base.mantissa);
    let mut denominator = u128::from(quote_mantissa);
    if power >= 0 {
        numerator = numerator
            .checked_mul(pow10(power.unsigned_abs())?)
            .ok_or(ScaleError::Overflow)?;
    } else {
        denominator = denominator
            .checked_mul(pow10(power.unsigned_abs())?)
            .ok_or(ScaleError::Overflow)?;
    }

    let mid = numerator / denominator;
    if mid == 0 {
        return Err(ScaleError::Zero);
    }
    Ok(mid)
}

fn pow10(power: u64) -> Result<u128, ScaleError> {
    u32::try_from(power)
        .ok()
        .and_then(|power| 10u128.checked_pow(power))
        .ok_or(ScaleError::Overflow)
}

// ─── Transport ──────────────────────────────────────────────────────────────

/// How the reader opens the stream. The default implementation is [`Https`].
///
/// Behind a trait so that the reconnect loop, the replay guard and the event
/// order are tested by a plain `cargo test` on recorded bodies.
pub trait Stream {
    /// Open the SSE stream at `url` and return its body as lines.
    ///
    /// # Errors
    ///
    /// If the connection did not open or the server refused the request.
    fn open(&self, url: &str, api_key: Option<&str>) -> Result<Box<dyn BufRead>, FeedError>;
}

/// HTTPS transport on `ureq` with rustls.
pub struct Https {
    agent: ureq::Agent,
}

impl Https {
    /// `session_limit` is the total budget of one stream body — see the module
    /// docs on hung sockets. [`DEFAULT_SESSION_LIMIT`] is the usual choice.
    #[must_use]
    pub fn new(session_limit: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(CONNECT_TIMEOUT))
            .timeout_recv_body(Some(session_limit))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for Https {
    fn default() -> Self {
        Self::new(DEFAULT_SESSION_LIMIT)
    }
}

impl Stream for Https {
    fn open(&self, url: &str, api_key: Option<&str>) -> Result<Box<dyn BufRead>, FeedError> {
        let mut request = self
            .agent
            .get(url)
            .header("Accept", "text/event-stream")
            // No gzip on a stream: a compressor flushes in blocks, and an event
            // would sit in its buffer until the next ones push it out.
            .header("Accept-Encoding", "identity");
        if let Some(key) = api_key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        let response = request.call().map_err(|error| match error {
            ureq::Error::StatusCode(401) => FeedError::Unauthorized,
            other => FeedError::Transport(other.to_string()),
        })?;
        let body = response.into_body().into_reader();
        Ok(Box::new(BufReader::new(body)))
    }
}

/// The wall clock in unix seconds. Behind a trait so that the age check is
/// tested at fixed instants.
pub trait Clock {
    fn unix_now(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_now(&self) -> i64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX)
    }
}

// ─── The reader ─────────────────────────────────────────────────────────────

/// What the reader sends to the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    /// The stream is open. Sent on every (re)connect.
    Connected,
    /// A sample that passed the policy.
    Price(Price),
    /// A sample that did not. The feed is alive; the price is not usable.
    Rejected { id: PriceId, reason: Reject },
    /// The stream is gone; the reader is about to retry with backoff.
    Disconnected { reason: String },
}

/// Pause between reconnect attempts: doubles from `min` up to `max`, resets
/// to `min` after a session that delivered at least one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            min: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// Build the stream URL for a set of ids.
///
/// `ids[]` is sent percent-encoded (`ids%5B%5D`): brackets are not URL
/// characters, and Hermes decodes the query before reading the brackets.
#[must_use]
pub fn stream_url(base: &str, ids: &[PriceId]) -> String {
    let mut url = format!(
        "{}{HERMES_STREAM_PATH}?parsed=true&encoding=hex",
        base.trim_end_matches('/')
    );
    for id in ids {
        // Writing to a String cannot fail.
        let _ = write!(url, "&ids%5B%5D={id}");
    }
    url
}

/// Reads the stream, judges every sample, and keeps reconnecting until the
/// receiving side goes away.
pub struct Reader<S, C> {
    url: String,
    ids: Vec<PriceId>,
    api_key: Option<String>,
    policy: Policy,
    backoff: Backoff,
    stream: S,
    clock: C,
    /// Latest publish time seen per id — the replay guard across reconnects.
    last_publish: HashMap<PriceId, i64>,
    /// Events delivered in the current session; resets the backoff when non-zero.
    delivered: u64,
}

impl<S: Stream, C: Clock> Reader<S, C> {
    /// `hermes_url` is the base (see [`HERMES_DEFAULT_URL`]); `api_key` is the
    /// bearer token Hermes has required since 2026-08-26.
    #[must_use]
    pub fn new(
        hermes_url: &str,
        ids: Vec<PriceId>,
        api_key: Option<String>,
        policy: Policy,
        stream: S,
        clock: C,
    ) -> Self {
        Self {
            url: stream_url(hermes_url, &ids),
            ids,
            api_key,
            policy,
            backoff: Backoff::default(),
            stream,
            clock,
            last_publish: HashMap::new(),
            delivered: 0,
        }
    }

    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// The URL the reader opens.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Run until the receiver is dropped. Meant for a dedicated thread.
    ///
    /// Every session ends in one of two ways: the receiver is gone (return) or
    /// the stream broke (a [`FeedEvent::Disconnected`], a pause, a retry).
    pub fn run(mut self, tx: &Sender<FeedEvent>) {
        let mut pause = self.backoff.min;
        loop {
            self.delivered = 0;
            match self.session(tx) {
                Ok(()) => return,
                Err(error) => {
                    warn!(%error, "feed session ended");
                    let event = FeedEvent::Disconnected {
                        reason: error.to_string(),
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
            if self.delivered > 0 {
                pause = self.backoff.min;
            }
            thread::sleep(pause);
            pause = pause.saturating_mul(2).min(self.backoff.max);
        }
    }

    /// One connection. `Ok` means the receiver is gone; `Err` means the stream broke.
    fn session(&mut self, tx: &Sender<FeedEvent>) -> Result<(), FeedError> {
        let reader = self.stream.open(&self.url, self.api_key.as_deref())?;
        info!(url = %self.url, "feed connected");
        if tx.send(FeedEvent::Connected).is_err() {
            return Ok(());
        }

        let mut parser = SseParser::new();
        for line in reader.lines() {
            let line = line?;
            let Some(data) = parser.feed_line(&line) else {
                continue;
            };
            // One malformed event is a wire drift to log, not a reason to drop a
            // live connection: the next event may well be fine.
            let samples = match parse_event(&data) {
                Ok(samples) => samples,
                Err(error) => {
                    warn!(%error, "skipping an event");
                    continue;
                }
            };
            for sample in samples {
                if !self.ids.contains(&sample.id) {
                    warn!(id = %sample.id, "skipping a sample for an id we did not ask for");
                    continue;
                }
                if tx.send(self.judge(sample)).is_err() {
                    return Ok(());
                }
                self.delivered += 1;
            }
        }
        Err(FeedError::StreamEnded)
    }

    /// The replay guard, then the policy.
    fn judge(&mut self, sample: Sample) -> FeedEvent {
        if let Some(&previous) = self.last_publish.get(&sample.id) {
            if sample.publish_time <= previous {
                return FeedEvent::Rejected {
                    id: sample.id,
                    reason: Reject::NotNewer {
                        publish_time: sample.publish_time,
                        previous,
                    },
                };
            }
        }
        self.last_publish.insert(sample.id, sample.publish_time);
        match self.policy.check(&sample, self.clock.unix_now()) {
            Ok(price) => FeedEvent::Price(price),
            Err(reason) => FeedEvent::Rejected {
                id: sample.id,
                reason,
            },
        }
    }
}

// ─── Silence: withdrawing the quote (FR-014) ────────────────────────────────

/// Nominal length of a Solana slot.
///
/// Used in one place only: refusing a silence bound that is not shorter than
/// the on-chain freshness limit (see [`Silence::checked`]). Nothing at runtime
/// converts slots into seconds — the chain's clock is the slot itself, and a
/// real slot drifts around this figure.
pub const SLOT_DURATION: Duration = Duration::from_millis(400);

/// A silence bound that cannot do its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SilenceConfigError {
    #[error("the silence bound is zero: the quote would be withdrawn before the first price")]
    Zero,
    #[error(
        "the silence bound of {silence_ms} ms is not shorter than the on-chain freshness limit \
         of {slots} slots (≈{freshness_ms} ms): the quote would go stale on chain before the \
         engine noticed the feed had stopped"
    )]
    NotShorterThanFreshness {
        silence_ms: u128,
        slots: u32,
        freshness_ms: u128,
    },
}

/// Why the quote has to come off the book.
///
/// Every variant means the same thing to the tick: post a withdrawal, do not
/// repeat the last known price (FR-014). They differ only in what goes in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Withdrawal {
    #[error("no usable price for {silent_ms} ms, bound {bound_ms} ms")]
    Silent { silent_ms: u128, bound_ms: u128 },
    #[error("the feed is alive but the price is not usable: {reason}")]
    Unusable { id: PriceId, reason: Reject },
    #[error("the feed reader is gone")]
    ReaderGone,
}

/// What the feed says about the quote right now.
///
/// Edge-triggered: a withdrawal is reported once, and the next report comes
/// only when a usable price returns. Prices, on the other hand, are reported
/// every time — whether a fresh price is worth a transaction is the update
/// rule's decision (FR-011, T031), not the feed's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteState {
    /// A usable price: the quote may stand, refreshed to this one.
    Live(Price),
    /// Take the quote off the book.
    Withdrawn(Withdrawal),
}

/// The state behind FR-014, as a fold over [`FeedEvent`]s and the passage of time.
///
/// # What counts as the feed being alive
///
/// Only an accepted [`Price`]. A connect, a disconnect or a rejected sample
/// carry no price, so none of them restarts the silence clock: the engine's
/// question is not "is the socket open" but "do I have a price I may quote on".
///
/// # Why a rejection withdraws at once and a disconnect does not
///
/// A rejected sample is the feed telling us this price is unusable *now* —
/// stale, wider than the confidence bound, or non-positive. Those are exactly
/// the moments a market maker gets picked off for quoting the last known mid,
/// so the quote goes off the book immediately.
///
/// A disconnect says nothing about the price: a reconnect takes a backoff
/// pause, and withdrawing on every blip would mean two transactions per flap.
/// It is covered by the silence bound, which the disconnect never reset.
///
/// [`Reject::NotNewer`] is the one rejection that is not a fault: it is the
/// same sample again after a reconnect, and we already judged it. It neither
/// refreshes the clock nor withdraws.
///
/// # The clock
///
/// Time comes in as a monotonic [`Duration`] since some origin the caller
/// keeps — [`Watch`] uses an [`Instant`]. That keeps this type pure and its
/// tests free of sleeps.
#[derive(Debug, Clone, Copy)]
pub struct Silence {
    bound: Duration,
    /// Monotonic instant from which there has been no usable price.
    since: Duration,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No price yet, and nothing has been withdrawn — a quote from an earlier
    /// run of the engine may still be on the book.
    Pending,
    /// A price has passed; the quote may stand.
    Live,
    /// The withdrawal has been reported; there is nothing more to do until a
    /// usable price returns.
    Withdrawn,
}

impl Silence {
    /// A bound with no cross-check. Prefer [`Silence::checked`].
    #[must_use]
    pub const fn new(bound: Duration) -> Self {
        Self {
            bound,
            since: Duration::ZERO,
            state: State::Pending,
        }
    }

    /// The same, refusing a bound that cannot protect the quote.
    ///
    /// `max_quote_age_slots` is the vault's freshness limit (FR-007). If the
    /// engine were allowed to stay quiet for that long, the quote would expire
    /// on chain — the AMM would drop out of routes on its own — before the
    /// engine ever decided the feed had stopped. That is the same failure
    /// FR-011a refuses for the heartbeat, one level up.
    ///
    /// # Errors
    ///
    /// [`SilenceConfigError`] — a zero bound, or one not shorter than the
    /// freshness limit.
    pub fn checked(bound: Duration, max_quote_age_slots: u32) -> Result<Self, SilenceConfigError> {
        if bound.is_zero() {
            return Err(SilenceConfigError::Zero);
        }
        let freshness = SLOT_DURATION.saturating_mul(max_quote_age_slots);
        if bound >= freshness {
            return Err(SilenceConfigError::NotShorterThanFreshness {
                silence_ms: bound.as_millis(),
                slots: max_quote_age_slots,
                freshness_ms: freshness.as_millis(),
            });
        }
        Ok(Self::new(bound))
    }

    /// Start the silence clock at `now` instead of at zero.
    #[must_use]
    pub const fn started_at(mut self, now: Duration) -> Self {
        self.since = now;
        self
    }

    /// The bound itself.
    #[must_use]
    pub const fn bound(&self) -> Duration {
        self.bound
    }

    /// Is there a usable price behind the quote right now?
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.state == State::Live
    }

    /// Fold one feed event in at monotonic instant `now`.
    ///
    /// `None` means the event changes nothing the tick has to act on.
    pub fn observe(&mut self, now: Duration, event: &FeedEvent) -> Option<QuoteState> {
        match event {
            FeedEvent::Price(price) => {
                self.since = now;
                self.state = State::Live;
                Some(QuoteState::Live(*price))
            }
            // The same sample twice after a reconnect: no news, no fault.
            FeedEvent::Rejected {
                reason: Reject::NotNewer { .. },
                ..
            } => None,
            FeedEvent::Rejected { id, reason } => self.withdraw(Withdrawal::Unusable {
                id: *id,
                reason: *reason,
            }),
            FeedEvent::Connected | FeedEvent::Disconnected { .. } => None,
        }
    }

    /// Nothing has arrived by `now`.
    ///
    /// The quote stands while a usable price is *younger* than the bound, so a
    /// silence of exactly the bound withdraws. (The FR-012 limits in [`Policy`]
    /// are inclusive; here the timer wakes exactly on the bound, and an
    /// inclusive bound would only buy a spin through the loop.)
    pub fn tick(&mut self, now: Duration) -> Option<QuoteState> {
        let silent = now.saturating_sub(self.since);
        if silent < self.bound {
            return None;
        }
        self.withdraw(Withdrawal::Silent {
            silent_ms: silent.as_millis(),
            bound_ms: self.bound.as_millis(),
        })
    }

    /// The reader thread ended: there will be no further events at all.
    pub fn reader_gone(&mut self) -> Option<QuoteState> {
        self.withdraw(Withdrawal::ReaderGone)
    }

    /// How long the caller may block before [`Silence::tick`] would withdraw.
    ///
    /// `None` once the quote is already off the book: there is no deadline to
    /// keep, only a price to wait for.
    #[must_use]
    pub fn deadline(&self, now: Duration) -> Option<Duration> {
        if self.state == State::Withdrawn {
            return None;
        }
        Some(self.bound.saturating_sub(now.saturating_sub(self.since)))
    }

    /// Report a withdrawal once; a second reason while the quote is already off
    /// the book is not news.
    fn withdraw(&mut self, reason: Withdrawal) -> Option<QuoteState> {
        if self.state == State::Withdrawn {
            return None;
        }
        self.state = State::Withdrawn;
        warn!(%reason, "withdrawing the quote");
        Some(QuoteState::Withdrawn(reason))
    }
}

/// [`Silence`] driven by a real clock and the reader's channel.
///
/// This is the shell around the state machine, and the tick's whole view of the
/// feed: block in [`Watch::recv`] until there is something to do with the quote.
#[derive(Debug)]
pub struct Watch {
    silence: Silence,
    origin: Instant,
}

impl Watch {
    /// The silence clock starts now — before the first price, not after it.
    #[must_use]
    pub fn new(silence: Silence) -> Self {
        Self {
            silence: silence.started_at(Duration::ZERO),
            origin: Instant::now(),
        }
    }

    /// The bound this watch keeps.
    #[must_use]
    pub const fn bound(&self) -> Duration {
        self.silence.bound()
    }

    /// Is there a usable price behind the quote right now?
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.silence.is_live()
    }

    /// Block until the quote has to change, draining the feed's own events.
    ///
    /// `None` means the reader is gone *and* the quote is already off the book —
    /// the tick has nothing left to wait for.
    pub fn recv(&mut self, rx: &Receiver<FeedEvent>) -> Option<QuoteState> {
        loop {
            if let Some(state) = self.silence.tick(self.origin.elapsed()) {
                return Some(state);
            }
            let event = match self.silence.deadline(self.origin.elapsed()) {
                Some(budget) => match rx.recv_timeout(budget) {
                    Ok(event) => event,
                    // The bound has run out; the next tick turns it into a withdrawal.
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return self.silence.reader_gone(),
                },
                // Already withdrawn: no deadline to keep, only a price to wait for.
                None => match rx.recv() {
                    Ok(event) => event,
                    Err(RecvError) => return self.silence.reader_gone(),
                },
            };
            if let Some(state) = self.silence.observe(self.origin.elapsed(), &event) {
                return Some(state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// SOL/USD on Pythnet.
    const SOL_USD: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
    /// USDC/USD on Pythnet.
    const USDC_USD: &str = "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";

    const NOW: i64 = 1_758_400_000;

    fn id(hex: &str) -> PriceId {
        hex.parse().expect("a valid price id")
    }

    fn policy() -> Policy {
        Policy::new(Duration::from_secs(5), 30)
    }

    /// A Hermes event as the stream sends it: `price` and `conf` are strings,
    /// the binary part is present but irrelevant.
    fn event(id: &str, price: &str, conf: &str, publish_time: i64) -> String {
        format!(
            r#"{{"binary":{{"encoding":"hex","data":["504e4155"]}},"parsed":[{{"id":"{id}","price":{{"price":"{price}","conf":"{conf}","expo":-8,"publish_time":{publish_time}}},"ema_price":{{"price":"{price}","conf":"{conf}","expo":-8,"publish_time":{publish_time}}},"metadata":{{"slot":123456,"proof_available_time":{publish_time},"prev_publish_time":{}}}}}]}}"#,
            publish_time - 1
        )
    }

    fn sample(price: i64, conf: u64, publish_time: i64) -> Sample {
        Sample {
            id: id(SOL_USD),
            price,
            conf,
            expo: -8,
            publish_time,
            slot: Some(1),
        }
    }

    fn accepted(mantissa: u64, expo: i32) -> Price {
        Price {
            id: id(SOL_USD),
            mantissa,
            expo,
            conf_bps: 1,
            publish_time: NOW,
            slot: None,
        }
    }

    // ─── PriceId ───

    #[test]
    fn a_price_id_round_trips_with_and_without_prefix() {
        let plain = id(SOL_USD);
        let prefixed = id(&format!("0x{SOL_USD}"));
        assert_eq!(plain, prefixed);
        assert_eq!(plain.to_string(), SOL_USD);
        assert_eq!(plain.as_bytes()[0], 0xef);
        assert_eq!(plain.as_bytes()[31], 0x6d);
    }

    #[test]
    fn a_short_or_non_hex_id_is_refused() {
        assert!(matches!(
            "ef0d".parse::<PriceId>(),
            Err(FeedError::BadPriceId(_))
        ));
        let non_hex = format!("zz{}", &SOL_USD[2..]);
        assert!(matches!(
            non_hex.parse::<PriceId>(),
            Err(FeedError::BadPriceId(_))
        ));
        // 64 bytes of multi-byte text: the length check in chars would let it through.
        let wide = "é".repeat(32);
        assert!(matches!(
            wide.parse::<PriceId>(),
            Err(FeedError::BadPriceId(_))
        ));
    }

    // ─── SSE ───

    #[test]
    fn sse_dispatches_on_a_blank_line_and_ignores_the_rest() {
        let mut parser = SseParser::new();
        assert_eq!(parser.feed_line(": keep-alive"), None);
        assert_eq!(parser.feed_line("event: price"), None);
        assert_eq!(parser.feed_line("id: 7"), None);
        assert_eq!(parser.feed_line("data: {\"a\":1}"), None);
        assert_eq!(parser.feed_line(""), Some("{\"a\":1}".to_owned()));
        // A blank line with nothing pending is not an empty event.
        assert_eq!(parser.feed_line(""), None);
    }

    #[test]
    fn sse_joins_multi_line_data_and_tolerates_crlf() {
        let mut parser = SseParser::new();
        assert_eq!(parser.feed_line("data:first\r"), None);
        assert_eq!(parser.feed_line("data: second\r"), None);
        assert_eq!(parser.feed_line("\r"), Some("first\nsecond".to_owned()));
    }

    // ─── Hermes JSON ───

    #[test]
    fn a_hermes_event_parses_into_a_sample() {
        let samples = parse_event(&event(SOL_USD, "15000000000", "5000000", NOW)).unwrap();
        assert_eq!(
            samples,
            vec![Sample {
                id: id(SOL_USD),
                price: 15_000_000_000,
                conf: 5_000_000,
                expo: -8,
                publish_time: NOW,
                slot: Some(123_456),
            }]
        );
    }

    #[test]
    fn an_event_without_parsed_block_is_an_error() {
        let error =
            parse_event(r#"{"binary":{"encoding":"hex","data":[]},"parsed":null}"#).unwrap_err();
        assert!(matches!(error, FeedError::NoParsed));
        let error = parse_event(r#"{"binary":{"encoding":"hex","data":[]}}"#).unwrap_err();
        assert!(matches!(error, FeedError::NoParsed));
    }

    #[test]
    fn a_non_numeric_price_names_the_field() {
        let error = parse_event(&event(SOL_USD, "1.5e10", "1", NOW)).unwrap_err();
        assert!(
            matches!(error, FeedError::BadNumber { field: "price", .. }),
            "{error}"
        );
        let error = parse_event(&event(SOL_USD, "1", "-1", NOW)).unwrap_err();
        assert!(
            matches!(error, FeedError::BadNumber { field: "conf", .. }),
            "{error}"
        );
    }

    #[test]
    fn metadata_is_optional_on_the_wire() {
        let data = format!(
            r#"{{"binary":{{"encoding":"hex","data":[]}},"parsed":[{{"id":"{SOL_USD}","price":{{"price":"1","conf":"0","expo":0,"publish_time":{NOW}}},"ema_price":{{"price":"1","conf":"0","expo":0,"publish_time":{NOW}}}}}]}}"#
        );
        assert_eq!(parse_event(&data).unwrap()[0].slot, None);
    }

    // ─── Policy ───

    #[test]
    fn a_fresh_tight_sample_is_accepted() {
        let price = policy()
            .check(&sample(15_000_000_000, 5_000_000, NOW - 1), NOW)
            .unwrap();
        assert_eq!(price.mantissa, 15_000_000_000);
        assert_eq!(price.expo, -8);
        // 5_000_000 / 15_000_000_000 = 3.33 bps → 4 after rounding up.
        assert_eq!(price.conf_bps, 4);
        assert_eq!(price.publish_time, NOW - 1);
    }

    #[test]
    fn the_age_limit_is_inclusive() {
        let policy = policy();
        assert!(policy.check(&sample(1, 0, NOW - 5), NOW).is_ok());
        assert_eq!(
            policy.check(&sample(1, 0, NOW - 6), NOW).unwrap_err(),
            Reject::Stale {
                age_secs: 6,
                limit_secs: 5
            }
        );
    }

    #[test]
    fn a_sample_from_the_future_is_tolerated_within_the_skew_only() {
        let policy = policy();
        assert!(policy.check(&sample(1, 0, NOW + 5), NOW).is_ok());
        assert_eq!(
            policy.check(&sample(1, 0, NOW + 6), NOW).unwrap_err(),
            Reject::FromFuture {
                ahead_secs: 6,
                tolerance_secs: 5
            }
        );
    }

    #[test]
    fn the_confidence_limit_is_inclusive_and_rounds_up() {
        let policy = policy();
        // Exactly 30 bps.
        assert!(policy.check(&sample(10_000, 30, NOW), NOW).is_ok());
        // 30.01 bps: rounded up to 31, over the limit.
        let error = policy
            .check(&sample(1_000_000, 3_001, NOW), NOW)
            .unwrap_err();
        assert_eq!(
            error,
            Reject::WideConfidence {
                conf_bps: 31,
                limit_bps: 30
            }
        );
    }

    #[test]
    fn a_zero_or_negative_price_is_refused_before_anything_else() {
        let policy = policy();
        assert_eq!(
            policy.check(&sample(0, 0, NOW - 100), NOW).unwrap_err(),
            Reject::NonPositivePrice { price: 0 }
        );
        assert_eq!(
            policy.check(&sample(-5, 0, NOW), NOW).unwrap_err(),
            Reject::NonPositivePrice { price: -5 }
        );
    }

    #[test]
    fn an_absurd_confidence_saturates_rather_than_overflows() {
        let error = policy().check(&sample(1, u64::MAX, NOW), NOW).unwrap_err();
        assert_eq!(
            error,
            Reject::WideConfidence {
                conf_bps: u32::MAX,
                limit_bps: 30
            }
        );
    }

    // ─── Scale ───

    /// The same example `forge quote` is tested on: 150 USDC per SOL is 150_000_000.
    #[test]
    fn sol_at_150_usd_against_usdc_at_par() {
        let sol = accepted(15_000_000_000, -8);
        assert_eq!(mid_e9(&sol, None, 9, 6).unwrap(), 150_000_000);
    }

    #[test]
    fn a_quote_feed_below_par_raises_the_mid() {
        let sol = accepted(15_000_000_000, -8);
        let usdc = Price {
            id: id(USDC_USD),
            ..accepted(99_990_000, -8)
        };
        // 150 / 0.9999 = 150.015001…
        assert_eq!(mid_e9(&sol, Some(&usdc), 9, 6).unwrap(), 150_015_001);
    }

    #[test]
    fn a_positive_power_multiplies_the_numerator() {
        // Whole-unit base (0 decimals) against a 6-decimal quote: 150 × 10^6 × 10^9.
        let sol = accepted(15_000_000_000, -8);
        assert_eq!(mid_e9(&sol, None, 0, 6).unwrap(), 150_000_000_000_000_000);
    }

    #[test]
    fn a_mid_that_vanishes_is_an_error_not_a_zero() {
        // 1e-8 USD per whole token with 18 decimals against a 0-decimal quote.
        let dust = accepted(1, -8);
        assert_eq!(mid_e9(&dust, None, 18, 0).unwrap_err(), ScaleError::Zero);
    }

    #[test]
    fn an_overflowing_product_is_an_error() {
        let huge = accepted(u64::MAX, 30);
        assert_eq!(mid_e9(&huge, None, 0, 9).unwrap_err(), ScaleError::Overflow);
    }

    // ─── URL ───

    #[test]
    fn the_stream_url_encodes_brackets_and_asks_for_parsed() {
        let url = stream_url(
            "https://pyth.dourolabs.app/hermes/",
            &[id(SOL_USD), id(USDC_USD)],
        );
        assert_eq!(
            url,
            format!(
                "https://pyth.dourolabs.app/hermes/v2/updates/price/stream?parsed=true&encoding=hex&ids%5B%5D={SOL_USD}&ids%5B%5D={USDC_USD}"
            )
        );
    }

    // ─── Reader ───

    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn unix_now(&self) -> i64 {
            self.0
        }
    }

    /// Serves scripted bodies one per `open`; once they run out, every `open` fails.
    struct Scripted {
        bodies: Mutex<VecDeque<String>>,
        /// The key the last `open` was given — shared with the test, since the reader owns the stream.
        seen_key: Arc<Mutex<Option<String>>>,
    }

    impl Scripted {
        fn new(bodies: &[&str]) -> Self {
            Self {
                bodies: Mutex::new(bodies.iter().map(|b| (*b).to_owned()).collect()),
                seen_key: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl Stream for Scripted {
        fn open(&self, _url: &str, api_key: Option<&str>) -> Result<Box<dyn BufRead>, FeedError> {
            *self.seen_key.lock().unwrap() = api_key.map(str::to_owned);
            let body = self.bodies.lock().unwrap().pop_front();
            body.map_or_else(
                || Err(FeedError::Transport("script exhausted".to_owned())),
                |body| Ok(Box::new(Cursor::new(body)) as Box<dyn BufRead>),
            )
        }
    }

    fn sse(events: &[String]) -> String {
        let mut body = String::from(": hello\n\n");
        for event in events {
            body.push_str("data: ");
            body.push_str(event);
            body.push_str("\n\n");
        }
        body
    }

    /// Runs the reader on a thread and collects events up to and including the
    /// first "script exhausted" disconnect; then drops the receiver so the reader returns.
    fn collect(reader: Reader<Scripted, FixedClock>) -> Vec<FeedEvent> {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || reader.run(&tx));
        let mut events = Vec::new();
        for event in rx.iter() {
            let done = matches!(&event, FeedEvent::Disconnected { reason } if reason.contains("script exhausted"));
            events.push(event);
            if done {
                break;
            }
        }
        drop(rx);
        handle.join().expect("the reader thread panicked");
        events
    }

    fn reader(bodies: &[&str]) -> Reader<Scripted, FixedClock> {
        Reader::new(
            HERMES_DEFAULT_URL,
            vec![id(SOL_USD)],
            Some("secret".to_owned()),
            policy(),
            Scripted::new(bodies),
            FixedClock(NOW),
        )
        .with_backoff(Backoff {
            min: Duration::ZERO,
            max: Duration::ZERO,
        })
    }

    #[test]
    fn the_reader_judges_reconnects_and_guards_against_replay() {
        let first = sse(&[
            event(SOL_USD, "15000000000", "5000000", NOW - 1),
            event(SOL_USD, "15000000000", "5000000", NOW - 60),
        ]);
        let second = sse(&[
            // The same publish time as the first session's last sample: a replay.
            event(SOL_USD, "15000000000", "5000000", NOW - 1),
            event(SOL_USD, "15100000000", "5000000", NOW),
        ]);
        let events = collect(reader(&[&first, &second]));

        let sol = id(SOL_USD);
        assert_eq!(events.len(), 9, "{events:#?}");
        assert_eq!(events[0], FeedEvent::Connected);
        assert!(matches!(
            events[1],
            FeedEvent::Price(Price {
                mantissa: 15_000_000_000,
                ..
            })
        ));
        // The second sample is OLDER than the first and the replay guard sees that
        // before the clock does: it is "not newer", not "stale".
        assert_eq!(
            events[2],
            FeedEvent::Rejected {
                id: sol,
                reason: Reject::NotNewer {
                    publish_time: NOW - 60,
                    previous: NOW - 1
                }
            }
        );
        assert!(
            matches!(&events[3], FeedEvent::Disconnected { reason } if reason.contains("closed"))
        );
        assert_eq!(events[4], FeedEvent::Connected);
        assert_eq!(
            events[5],
            FeedEvent::Rejected {
                id: sol,
                reason: Reject::NotNewer {
                    publish_time: NOW - 1,
                    previous: NOW - 1
                }
            }
        );
        assert!(matches!(
            events[6],
            FeedEvent::Price(Price {
                mantissa: 15_100_000_000,
                ..
            })
        ));
        // The scripted body ends (EOF) before the script runs out: two different reasons.
        assert!(
            matches!(&events[7], FeedEvent::Disconnected { reason } if reason.contains("closed"))
        );
        assert!(
            matches!(&events[8], FeedEvent::Disconnected { reason } if reason.contains("script exhausted"))
        );
    }

    #[test]
    fn a_stale_sample_is_rejected_by_the_clock() {
        let body = sse(&[event(SOL_USD, "15000000000", "5000000", NOW - 60)]);
        let events = collect(reader(&[&body]));
        assert_eq!(
            events[1],
            FeedEvent::Rejected {
                id: id(SOL_USD),
                reason: Reject::Stale {
                    age_secs: 60,
                    limit_secs: 5
                }
            }
        );
    }

    #[test]
    fn a_malformed_event_and_a_foreign_id_are_skipped_without_dropping_the_session() {
        let body = format!(
            "data: not json\n\n{}",
            sse(&[
                event(USDC_USD, "100000000", "1", NOW),
                event(SOL_USD, "15000000000", "5000000", NOW),
            ])
        );
        let events = collect(reader(&[&body]));
        // Connected, the one good price, EOF, then the exhausted script.
        assert_eq!(events.len(), 4, "{events:#?}");
        assert!(matches!(
            events[1],
            FeedEvent::Price(Price {
                mantissa: 15_000_000_000,
                ..
            })
        ));
    }

    #[test]
    fn the_key_reaches_the_transport_and_the_url_is_the_stream_route() {
        let stream = Scripted::new(&[]);
        let seen_key = Arc::clone(&stream.seen_key);
        let reader = Reader::new(
            HERMES_DEFAULT_URL,
            vec![id(SOL_USD)],
            Some("secret".to_owned()),
            policy(),
            stream,
            FixedClock(NOW),
        )
        .with_backoff(Backoff {
            min: Duration::ZERO,
            max: Duration::ZERO,
        });
        assert!(reader
            .url()
            .starts_with("https://pyth.dourolabs.app/hermes/v2/updates/price/stream?"));
        let events = collect(reader);
        assert_eq!(events.len(), 1, "{events:#?}");
        assert_eq!(seen_key.lock().unwrap().as_deref(), Some("secret"));
    }

    // ─── Silence (FR-014) ───

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Two seconds of patience, the clock starting at zero.
    fn silence() -> Silence {
        Silence::new(Duration::from_secs(2))
    }

    fn price_event(publish_time: i64) -> FeedEvent {
        FeedEvent::Price(Price {
            publish_time,
            ..accepted(15_000_000_000, -8)
        })
    }

    fn rejected(reason: Reject) -> FeedEvent {
        FeedEvent::Rejected {
            id: id(SOL_USD),
            reason,
        }
    }

    #[test]
    fn a_price_puts_the_quote_on_the_book_and_restarts_the_silence_clock() {
        let mut silence = silence();
        assert!(!silence.is_live());
        assert_eq!(
            silence.observe(ms(1_500), &price_event(NOW)),
            Some(QuoteState::Live(Price {
                publish_time: NOW,
                ..accepted(15_000_000_000, -8)
            }))
        );
        assert!(silence.is_live());
        // The clock now runs from 1 500 ms, so 3 000 ms absolute is 1 500 ms of silence.
        assert_eq!(silence.tick(ms(3_000)), None);
        assert_eq!(silence.deadline(ms(3_000)), Some(ms(500)));
    }

    #[test]
    fn silence_past_the_bound_withdraws_the_quote_exactly_once() {
        let mut silence = silence();
        silence.observe(ms(0), &price_event(NOW));
        assert_eq!(silence.tick(ms(1_999)), None, "still within the bound");
        assert_eq!(
            silence.tick(ms(2_000)),
            Some(QuoteState::Withdrawn(Withdrawal::Silent {
                silent_ms: 2_000,
                bound_ms: 2_000
            })),
            "the bound itself is silence, not freshness"
        );
        assert!(!silence.is_live());
        assert_eq!(
            silence.tick(ms(9_000)),
            None,
            "withdrawn once, not per tick"
        );
    }

    #[test]
    fn silence_before_the_first_price_withdraws_too() {
        // A quote left on the book by an earlier run of the engine is exactly
        // the one nobody is watching; it has to come off like any other.
        let mut silence = silence();
        assert!(matches!(
            silence.tick(ms(2_000)),
            Some(QuoteState::Withdrawn(Withdrawal::Silent { .. }))
        ));
    }

    #[test]
    fn a_rejected_sample_withdraws_the_quote_at_once() {
        let mut silence = silence();
        silence.observe(ms(0), &price_event(NOW));
        let reason = Reject::WideConfidence {
            conf_bps: 120,
            limit_bps: 30,
        };
        assert_eq!(
            silence.observe(ms(10), &rejected(reason)),
            Some(QuoteState::Withdrawn(Withdrawal::Unusable {
                id: id(SOL_USD),
                reason
            })),
            "a live feed with an unusable price is not a reason to keep quoting"
        );
        assert_eq!(
            silence.observe(ms(20), &rejected(reason)),
            None,
            "the second rejection is not news"
        );
    }

    #[test]
    fn a_replayed_sample_after_a_reconnect_neither_refreshes_nor_withdraws() {
        let mut silence = silence();
        silence.observe(ms(0), &price_event(NOW));
        let replay = rejected(Reject::NotNewer {
            publish_time: NOW,
            previous: NOW,
        });
        assert_eq!(silence.observe(ms(1_000), &replay), None);
        assert!(silence.is_live(), "a replay is not a fault");
        assert_eq!(
            silence.deadline(ms(1_000)),
            Some(ms(1_000)),
            "and it does not buy another two seconds either"
        );
    }

    #[test]
    fn a_reconnect_does_not_withdraw_and_does_not_stop_the_clock() {
        let mut silence = silence();
        silence.observe(ms(0), &price_event(NOW));
        let blip = FeedEvent::Disconnected {
            reason: "the server closed the stream".to_owned(),
        };
        assert_eq!(silence.observe(ms(500), &blip), None);
        assert_eq!(silence.observe(ms(600), &FeedEvent::Connected), None);
        assert!(
            silence.is_live(),
            "a blip shorter than the bound is not silence"
        );
        assert!(
            matches!(
                silence.tick(ms(2_000)),
                Some(QuoteState::Withdrawn(Withdrawal::Silent { .. }))
            ),
            "but the reconnect did not reset the clock either"
        );
    }

    #[test]
    fn a_price_after_a_withdrawal_brings_the_quote_back() {
        let mut silence = silence();
        silence.tick(ms(2_000));
        assert_eq!(
            silence.deadline(ms(2_000)),
            None,
            "nothing left to wait out"
        );
        assert!(matches!(
            silence.observe(ms(2_500), &price_event(NOW + 1)),
            Some(QuoteState::Live(_))
        ));
        assert_eq!(silence.deadline(ms(2_500)), Some(ms(2_000)));
    }

    #[test]
    fn a_silence_bound_that_cannot_protect_the_quote_is_refused() {
        // 25 slots ≈ 10 000 ms of on-chain freshness.
        assert!(Silence::checked(Duration::from_secs(2), 25).is_ok());
        assert!(matches!(
            Silence::checked(Duration::ZERO, 25),
            Err(SilenceConfigError::Zero)
        ));
        assert!(
            matches!(
                Silence::checked(Duration::from_secs(10), 25),
                Err(SilenceConfigError::NotShorterThanFreshness {
                    silence_ms: 10_000,
                    slots: 25,
                    freshness_ms: 10_000,
                })
            ),
            "equal is not shorter"
        );
    }

    // ─── Watch: the state machine on a real clock and channel ───

    #[test]
    fn the_watch_hands_over_a_price_and_then_withdraws_on_silence() {
        let (tx, rx) = mpsc::channel();
        let mut watch = Watch::new(Silence::new(ms(80)));
        tx.send(FeedEvent::Connected).expect("the watch is alive");
        tx.send(price_event(NOW)).expect("the watch is alive");

        assert!(matches!(watch.recv(&rx), Some(QuoteState::Live(_))));
        let started = Instant::now();
        let withdrawal = watch.recv(&rx);
        assert!(
            matches!(
                withdrawal,
                Some(QuoteState::Withdrawn(Withdrawal::Silent { .. }))
            ),
            "{withdrawal:?}"
        );
        assert!(started.elapsed() >= ms(80), "it withdrew before the bound");
        // The sender is still alive: the watch waits for a price, not for a deadline.
        assert!(!watch.is_live());
    }

    #[test]
    fn the_watch_reports_the_reader_going_away_and_then_has_nothing_left() {
        let (tx, rx) = mpsc::channel();
        let mut watch = Watch::new(Silence::new(Duration::from_secs(30)));
        tx.send(price_event(NOW)).expect("the watch is alive");
        assert!(matches!(watch.recv(&rx), Some(QuoteState::Live(_))));
        drop(tx);
        assert_eq!(
            watch.recv(&rx),
            Some(QuoteState::Withdrawn(Withdrawal::ReaderGone))
        );
        assert_eq!(watch.recv(&rx), None, "nothing left to wait for");
    }
}
