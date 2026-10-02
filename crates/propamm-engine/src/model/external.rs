//! A model in another process, over a line protocol on stdin/stdout (FR-015a).
//!
//! The model is any program that reads questions from its stdin and writes
//! answers to its stdout — first of all Python, where the quant tooling lives.
//! A reference one ships with the repository: `examples/models/spread_skew.py`,
//! the built-in model ported line by line, and tested against it.
//!
//! # The protocol, version 1
//!
//! One JSON object per line, `\n`-terminated. The core writes a question to the
//! model's stdin; the model writes one answer to its stdout.
//!
//! ```text
//! → {"v":1,"id":7,"mid_e9":"150000000","inventory":{"base_amount":"1000000000","quote_amount":"150000000"},"max_skew_bps":2000}
//! ← {"id":7,"quote":{"mid_e9":"150000000","spread_bps":10,"skew_bps":0,"max_size_base":"50000000"}}
//! ← {"id":7,"withdraw":"volatility above the model's comfort"}
//! ← {"id":7,"error":"ZeroDivisionError: division by zero"}
//! ```
//!
//! - The answer carries the `id` of the question and **exactly one** of
//!   `quote`, `withdraw`, `error`. A quote is posted, a withdrawal takes the
//!   quote down (see [`Decision::Withdraw`]), an error skips the tick.
//! - Amounts and prices — everything that is a `u64` or a `u128` — travel as
//!   **decimal strings**: a JSON number past 2^53 is silently rounded by any
//!   parser that reads numbers as doubles, and a mid that lost its low digits is
//!   still a plausible mid. Basis points are plain numbers.
//! - Unknown fields in an answer are ignored, so a model may add its own.
//! - `v` is the protocol version; a model that does not speak it answers with
//!   an `error`.
//! - **stdout is the protocol and nothing else.** A stray `print` is a broken
//!   answer. The model logs to stderr, which the core copies into its own log
//!   line by line.
//!
//! # One question at a time
//!
//! If the answer does not come by the deadline, the call returns
//! [`ModelError::Timeout`] and the question stays open. The next call first
//! reads that late answer and throws it away — a price computed for an old
//! state is not an answer to the new one — and only then asks. So there is at
//! most one question in the pipe, and a slow model cannot build a queue of
//! stale work behind it. A model that keeps a question open for longer than
//! the hang limit is not slow but stuck, and is restarted.
//!
//! # A process that dies
//!
//! Is started again, with a pause that doubles from [`Backoff::min`] up to
//! [`Backoff::max`] and resets once the model answers. Until then every call
//! returns [`ModelError::Down`] at once: the tick is skipped, and the quote on
//! chain ages out by itself — which is the right outcome for a market maker
//! that has lost its pricing.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::{Decision, MarketState, ModelError, PricingModel, Quote};
use crate::feed::Backoff;

/// The protocol version this core speaks.
pub const PROTOCOL_VERSION: u8 = 1;

/// How long a question may stay open before the model is taken for stuck.
///
/// Long, because the first answer after a start includes the model's own
/// start-up: a Python model importing its numerical stack takes seconds. A model
/// that is merely slow never gets here — it is skipped tick by tick well before.
pub const DEFAULT_HANG_LIMIT: Duration = Duration::from_secs(10);

/// How long a dying process is given to report its exit status before it is killed.
const EXIT_GRACE: Duration = Duration::from_millis(100);

/// The program to run and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl ModelCommand {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

impl fmt::Display for ModelCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program.to_string_lossy())?;
        for arg in &self.args {
            write!(f, " {}", arg.to_string_lossy())?;
        }
        Ok(())
    }
}

// ─── The wire ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Request {
    v: u8,
    id: u64,
    mid_e9: String,
    inventory: WireInventory,
    max_skew_bps: u16,
}

#[derive(Serialize)]
struct WireInventory {
    base_amount: String,
    quote_amount: String,
}

impl Request {
    fn line(id: u64, state: &MarketState) -> String {
        let request = Self {
            v: PROTOCOL_VERSION,
            id,
            mid_e9: state.mid_e9.to_string(),
            inventory: WireInventory {
                base_amount: state.inventory.base_amount.to_string(),
                quote_amount: state.inventory.quote_amount.to_string(),
            },
            max_skew_bps: state.max_skew_bps,
        };
        // Strings and integers only: serialization has nothing to fail on.
        let mut line = serde_json::to_string(&request).unwrap_or_default();
        line.push('\n');
        line
    }
}

#[derive(Debug, Deserialize)]
struct Reply {
    id: u64,
    quote: Option<WireQuote>,
    withdraw: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireQuote {
    mid_e9: String,
    spread_bps: u16,
    skew_bps: i16,
    max_size_base: String,
}

impl Reply {
    fn parse(line: &str) -> Result<Self, ModelError> {
        serde_json::from_str(line).map_err(|error| ModelError::Protocol {
            detail: format!("{error} in {:?}", clip(line)),
        })
    }

    fn into_decision(self) -> Result<Decision, ModelError> {
        match (self.quote, self.withdraw, self.error) {
            (Some(quote), None, None) => {
                let quote = Quote {
                    mid_e9: decimal("mid_e9", &quote.mid_e9)?,
                    spread_bps: quote.spread_bps,
                    skew_bps: quote.skew_bps,
                    max_size_base: decimal("max_size_base", &quote.max_size_base)?,
                };
                quote.check().map_err(|error| ModelError::Protocol {
                    detail: format!("the quote is not a price: {error}"),
                })?;
                Ok(Decision::Quote(quote))
            }
            (None, Some(reason), None) => Ok(Decision::Withdraw { reason }),
            (None, None, Some(message)) => Err(ModelError::Failed { message }),
            _ => Err(ModelError::Protocol {
                detail: "an answer carries exactly one of `quote`, `withdraw`, `error`".into(),
            }),
        }
    }
}

/// A decimal string, digits only: no sign, no spaces, no exponent.
fn decimal<T: std::str::FromStr>(field: &str, text: &str) -> Result<T, ModelError> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    digits
        .then(|| text.parse().ok())
        .flatten()
        .ok_or_else(|| ModelError::Protocol {
            detail: format!(
                "`{field}` is not a decimal string that fits: {:?}",
                clip(text)
            ),
        })
}

/// A line quoted in an error — enough to recognize it, not a whole dump.
fn clip(text: &str) -> &str {
    const MAX: usize = 200;
    match text.char_indices().nth(MAX) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

// ─── Restarts ───────────────────────────────────────────────────────────────

/// When the next start may be attempted — a rule over instants, tested without
/// sleeping.
#[derive(Debug, Clone, Copy)]
struct Restart {
    backoff: Backoff,
    pause: Duration,
    not_before: Option<Instant>,
}

impl Restart {
    fn new(backoff: Backoff) -> Self {
        Self {
            backoff,
            pause: backoff.min,
            not_before: None,
        }
    }

    /// The process died at `now`: the next start waits the current pause, and
    /// the pause after that doubles. Returns the wait.
    fn died(&mut self, now: Instant) -> Duration {
        let wait = self.pause;
        self.not_before = Some(now + wait);
        self.pause = self.pause.saturating_mul(2).min(self.backoff.max);
        wait
    }

    /// How long until a start is allowed; zero means now.
    fn wait(&self, now: Instant) -> Duration {
        self.not_before
            .map_or(Duration::ZERO, |at| at.saturating_duration_since(now))
    }

    /// The model answered: it works, and the next death starts from the
    /// shortest pause again.
    fn answered(&mut self) {
        self.pause = self.backoff.min;
    }
}

// ─── The process ────────────────────────────────────────────────────────────

struct Running {
    child: Child,
    stdin: ChildStdin,
    /// Lines of the model's stdout, read on a thread of their own so that the
    /// wait for them can have a deadline. Disconnected means the stream ended.
    lines: Receiver<io::Result<String>>,
}

impl Running {
    fn spawn(command: &ModelCommand) -> io::Result<Self> {
        let mut child = command.command().spawn()?;
        let pid = child.id();
        let piped = |what: &str| io::Error::other(format!("the model's {what} is not piped"));
        let stdin = child.stdin.take().ok_or_else(|| piped("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| piped("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| piped("stderr"))?;

        let (tx, lines) = mpsc::channel();
        let readers = thread::Builder::new()
            .name("model-stdout".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let broken = line.is_err();
                    if tx.send(line).is_err() || broken {
                        return;
                    }
                }
            })
            .and_then(|_| {
                thread::Builder::new()
                    .name("model-stderr".into())
                    .spawn(move || {
                        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                            info!(target: "propamm_engine::model::stderr", pid, "{line}");
                        }
                    })
            });
        if let Err(error) = readers {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        info!(pid, %command, "model process started");
        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    /// Stop the process and say how it ended: its own exit status if it has
    /// one within the grace period, "killed" otherwise.
    fn stop(mut self) -> String {
        let given_up = Instant::now() + EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return format!("it ended with {status}"),
                Ok(None) if Instant::now() < given_up => thread::sleep(Duration::from_millis(5)),
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        "it was killed".into()
    }
}

// ─── The model ──────────────────────────────────────────────────────────────

/// A [`PricingModel`] that is another program (FR-015a). See the module docs
/// for the protocol.
pub struct ExternalProcessModel {
    command: ModelCommand,
    running: Option<Running>,
    restart: Restart,
    hang_limit: Duration,
    /// Why the process last went down — repeated while waiting to restart it.
    last_failure: String,
    next_id: u64,
    /// The open question and when it was asked.
    open: Option<(u64, Instant)>,
}

impl fmt::Debug for ExternalProcessModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalProcessModel")
            .field("command", &self.command)
            .field("pid", &self.pid())
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

impl ExternalProcessModel {
    /// Start the process.
    ///
    /// # Errors
    ///
    /// The program cannot be started at all — a wrong path is a configuration
    /// error, and it is reported here, at the engine's start, rather than as a
    /// stream of skipped ticks. A process that starts and then dies is
    /// restarted instead (see the module docs).
    pub fn start(command: ModelCommand, backoff: Backoff) -> io::Result<Self> {
        let running = Running::spawn(&command)?;
        Ok(Self {
            command,
            running: Some(running),
            restart: Restart::new(backoff),
            hang_limit: DEFAULT_HANG_LIMIT,
            last_failure: String::new(),
            next_id: 1,
            open: None,
        })
    }

    /// Replace [`DEFAULT_HANG_LIMIT`].
    #[must_use]
    pub fn with_hang_limit(mut self, hang_limit: Duration) -> Self {
        self.hang_limit = hang_limit;
        self
    }

    #[must_use]
    pub fn command(&self) -> &ModelCommand {
        &self.command
    }

    /// The process id, while the process is running.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.running.as_ref().map(|running| running.child.id())
    }

    /// Make sure there is a process to ask, starting one if the pause is over.
    fn ensure_running(&mut self, now: Instant) -> Result<(), ModelError> {
        if self.running.is_some() {
            return Ok(());
        }
        let retry_in = self.restart.wait(now);
        if !retry_in.is_zero() {
            return Err(ModelError::Down {
                reason: self.last_failure.clone(),
                retry_in,
            });
        }
        match Running::spawn(&self.command) {
            Ok(running) => {
                self.running = Some(running);
                Ok(())
            }
            Err(error) => Err(self.down(format!("cannot start: {error}"))),
        }
    }

    /// Take the process down and schedule the next start.
    fn down(&mut self, what: String) -> ModelError {
        let reason = match self.running.take() {
            Some(running) => format!("{what}; {}", running.stop()),
            None => what,
        };
        self.open = None;
        let retry_in = self.restart.died(Instant::now());
        warn!(command = %self.command, %reason, ?retry_in, "model process down");
        self.last_failure.clone_from(&reason);
        ModelError::Down { reason, retry_in }
    }

    fn send(&mut self, line: &str) -> Result<(), ModelError> {
        let Some(running) = self.running.as_mut() else {
            return Err(self.down("not running".into()));
        };
        let written = running
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| running.stdin.flush());
        written.map_err(|error| self.down(format!("writing to its stdin failed: {error}")))
    }

    /// Read lines until the answer to question `id`, discarding answers to
    /// older ones.
    fn await_reply(
        &mut self,
        id: u64,
        started: Instant,
        deadline: Instant,
    ) -> Result<Reply, ModelError> {
        loop {
            let Some(running) = self.running.as_ref() else {
                return Err(self.down("not running".into()));
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match running.lines.recv_timeout(left) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => {
                    return Err(self.down(format!("reading its stdout failed: {error}")))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.down("it closed its stdout".into()))
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ModelError::Timeout {
                        waited: started.elapsed(),
                    })
                }
            };
            let reply = match Reply::parse(&line) {
                Ok(reply) => reply,
                Err(error) => {
                    // An answer, just not one in the protocol, and there is no
                    // telling which question it was for. The open question is
                    // taken as answered by it: otherwise a model that printed
                    // one bad line would be waited on forever. If the real
                    // answer follows, it is older than the next question and
                    // is discarded as late.
                    self.open = None;
                    return Err(error);
                }
            };
            if reply.id < id {
                debug!(id = reply.id, "late model answer discarded");
                continue;
            }
            self.open = None;
            if reply.id > id {
                return Err(ModelError::Protocol {
                    detail: format!(
                        "an answer to question {}, which was never asked; the open one is {id}",
                        reply.id
                    ),
                });
            }
            self.restart.answered();
            return Ok(reply);
        }
    }
}

impl PricingModel for ExternalProcessModel {
    fn price(&mut self, state: &MarketState, deadline: Instant) -> Result<Decision, ModelError> {
        let started = Instant::now();
        self.ensure_running(started)?;

        if let Some((stale, asked)) = self.open {
            let open_for = started.saturating_duration_since(asked);
            if open_for >= self.hang_limit {
                return Err(self.down(format!("question {stale} has been open for {open_for:?}")));
            }
            // Whatever the late answer says, it is about an old state; a broken
            // one closes the question just the same.
            match self.await_reply(stale, started, deadline) {
                Ok(_) => debug!(id = stale, "late model answer discarded"),
                Err(ModelError::Protocol { detail }) => {
                    debug!(id = stale, %detail, "late model answer was broken")
                }
                Err(error) => return Err(error),
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        self.send(&Request::line(id, state))?;
        self.open = Some((id, Instant::now()));
        self.await_reply(id, started, deadline)?.into_decision()
    }
}

impl Drop for ExternalProcessModel {
    fn drop(&mut self) {
        if let Some(mut running) = self.running.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use propamm_quote::{Inventory, QuoteError};

    use super::*;
    use crate::model::spread_skew::SpreadSkewModel;

    /// The reference model that ships with the repository.
    const REFERENCE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/models/spread_skew.py"
    );

    /// No pause between restarts: the restart itself is what is under test.
    const AT_ONCE: Backoff = Backoff {
        min: Duration::ZERO,
        max: Duration::ZERO,
    };

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
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

    /// A model in a few lines of Python. `body` runs once per question, with
    /// `q` the question, `i` its id and `n` the count of questions this process
    /// has seen — `n` starts over in a restarted process, `i` does not;
    /// `quote(i)` is a valid answer whose spread is the question id, so a test
    /// can tell which question an answer belongs to.
    fn fake(body: &str) -> ModelCommand {
        let script = format!(
            "import json, sys, time\n\
             def answer(o):\n    sys.stdout.write(json.dumps(o) + '\\n'); sys.stdout.flush()\n\
             def quote(i):\n    return {{'id': i, 'quote': {{'mid_e9': '150000000', 'spread_bps': i, 'skew_bps': 0, 'max_size_base': '1000'}}}}\n\
             n = 0\n\
             for line in sys.stdin:\n    q = json.loads(line); i = q['id']; n += 1\n{body}\n"
        );
        ModelCommand::new("python3").arg("-c").arg(script)
    }

    fn start(command: ModelCommand) -> ExternalProcessModel {
        ExternalProcessModel::start(command, AT_ONCE).expect("python3 starts")
    }

    fn spread_of(decision: Result<Decision, ModelError>) -> u16 {
        match decision {
            Ok(Decision::Quote(quote)) => quote.spread_bps,
            other => panic!("expected a quote, got {other:?}"),
        }
    }

    fn reference(config: [u16; 4]) -> ExternalProcessModel {
        let [base, max, shift, size] = config.map(|bps| bps.to_string());
        start(ModelCommand::new("python3").arg(REFERENCE).args([
            "--base-spread-bps",
            &base,
            "--max-spread-bps",
            &max,
            "--max-skew-shift-bps",
            &shift,
            "--size-fraction-bps",
            &size,
        ]))
    }

    /// The protocol carries everything a model needs: the Python port, asked
    /// over the pipe, answers exactly what the built-in model answers in-process.
    ///
    /// The second configuration is there for the rounding: with its wide
    /// levers, a skew rounded toward negative infinity instead of toward zero
    /// changes the answer, which the first one's narrow levers would hide.
    #[test]
    fn the_python_reference_answers_what_the_builtin_model_answers() {
        const HUGE_MID: u128 = 100_000_000_000_000_000_000; // past 2^53 and past u64
        let mut states = Vec::new();
        for mid_e9 in [150_000_000, 1, 3_333_333_333, HUGE_MID] {
            let big = if mid_e9 == HUGE_MID {
                1_000_000_000_000
            } else {
                u64::MAX
            };
            for (base_amount, quote_amount) in [
                (0, 0),
                (1, 0),
                (0, 1),
                (1_000_000_000, 150_000_000),
                (1_000_000_000, 200_000_000),
                (1_000_000_000, 120_000_000),
                (3_000_000_000, 150_000_000),
                (1_000_000_000, 900_000_000),
                (big, 7),
                (7, big),
                (big, big),
            ] {
                for max_skew_bps in [1, 2_000, 10_000] {
                    states.push(MarketState {
                        mid_e9,
                        inventory: Inventory {
                            base_amount,
                            quote_amount,
                        },
                        max_skew_bps,
                    });
                }
            }
        }

        for config in [[10, 40, 150, 500], [0, 5_000, 9_000, 10_000]] {
            let [base, max, shift, size] = config;
            let mut builtin =
                SpreadSkewModel::checked(base, max, shift, size).expect("a workable configuration");
            let mut python = reference(config);
            for state in &states {
                let expected = builtin.price(state, soon());
                assert!(
                    expected.is_ok(),
                    "{config:?} {state:?}: the grid is meant to be priceable, got {expected:?}"
                );
                assert_eq!(
                    python.price(state, soon()),
                    expected,
                    "{config:?} {state:?}"
                );
            }
        }
    }

    #[test]
    fn where_the_builtin_model_refuses_the_reference_reports_a_failure() {
        let mut python = reference([10, 40, 150, 500]);
        let no_mid = MarketState {
            mid_e9: 0,
            ..state()
        };
        assert_eq!(
            SpreadSkewModel::checked(10, 40, 150, 500)
                .unwrap()
                .price(&no_mid, soon()),
            Err(ModelError::Quote(QuoteError::QuoteNotSet))
        );
        match python.price(&no_mid, soon()) {
            Err(ModelError::Failed { message }) => {
                assert!(message.contains("quote is not set"), "{message}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        // A failure is an answer: the process is alive and the next question works.
        assert_eq!(spread_of(python.price(&state(), soon())), 10);
    }

    #[test]
    fn a_reference_model_that_cannot_steer_back_refuses_to_start() {
        let mut python = reference([10, 200, 150, 500]);
        match python.price(&state(), soon()) {
            Err(ModelError::Down { reason, .. }) => {
                assert!(reason.contains("exit status: 2"), "{reason}")
            }
            other => panic!("expected the model down, got {other:?}"),
        }
    }

    /// The second model of the SC-011 run: whatever the inventory, the same
    /// half-spread, no shift and the same size — the fingerprint the
    /// end-to-end run recognises it by on chain. A missing mid is a withdrawal.
    #[test]
    fn the_fixed_spread_model_ignores_the_inventory() {
        const FIXED: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/models/fixed_spread.py"
        );
        let mut python = start(ModelCommand::new("python3").arg(FIXED).args([
            "--spread-bps",
            "25",
            "--size-base",
            "1000000000",
        ]));
        let lopsided = MarketState {
            inventory: Inventory {
                base_amount: 9_000_000_000,
                quote_amount: 1,
            },
            ..state()
        };
        for state in [state(), lopsided] {
            assert_eq!(
                python.price(&state, soon()),
                Ok(Decision::Quote(Quote {
                    mid_e9: state.mid_e9,
                    spread_bps: 25,
                    skew_bps: 0,
                    max_size_base: 1_000_000_000,
                }))
            );
        }
        let no_mid = MarketState {
            mid_e9: 0,
            ..state()
        };
        assert!(matches!(
            python.price(&no_mid, soon()),
            Ok(Decision::Withdraw { .. })
        ));
    }

    /// The line on the wire is the one the module docs show — the contract
    /// models are written against.
    #[test]
    fn the_question_is_the_documented_line() {
        assert_eq!(
            Request::line(7, &state()),
            "{\"v\":1,\"id\":7,\"mid_e9\":\"150000000\",\
             \"inventory\":{\"base_amount\":\"1000000000\",\"quote_amount\":\"150000000\"},\
             \"max_skew_bps\":2000}\n"
        );
    }

    #[test]
    fn the_answers_are_parsed_as_documented() {
        let decide = |line: &str| Reply::parse(line).and_then(Reply::into_decision);
        assert_eq!(
            decide(
                r#"{"id":7,"quote":{"mid_e9":"150000000","spread_bps":10,"skew_bps":-30,"max_size_base":"50000000"},"debug":[1,2]}"#
            ),
            Ok(Decision::Quote(Quote {
                mid_e9: 150_000_000,
                spread_bps: 10,
                skew_bps: -30,
                max_size_base: 50_000_000,
            }))
        );
        assert_eq!(
            decide(r#"{"id":7,"withdraw":"volatility"}"#),
            Ok(Decision::Withdraw {
                reason: "volatility".into()
            })
        );
        assert_eq!(
            decide(r#"{"id":7,"error":"boom"}"#),
            Err(ModelError::Failed {
                message: "boom".into()
            })
        );

        for broken in [
            // not JSON — a stray print
            "hello",
            // none of the three, and two of them
            r#"{"id":7}"#,
            r#"{"id":7,"withdraw":"x","error":"y"}"#,
            // an amount as a number: the rounding the strings are there to prevent
            r#"{"id":7,"quote":{"mid_e9":150000000,"spread_bps":10,"skew_bps":0,"max_size_base":"1"}}"#,
            // not digits, a sign, too big for the field
            r#"{"id":7,"quote":{"mid_e9":"1.5e8","spread_bps":10,"skew_bps":0,"max_size_base":"1"}}"#,
            r#"{"id":7,"quote":{"mid_e9":"+150000000","spread_bps":10,"skew_bps":0,"max_size_base":"1"}}"#,
            r#"{"id":7,"quote":{"mid_e9":"1","spread_bps":10,"skew_bps":0,"max_size_base":"18446744073709551616"}}"#,
            // bps out of their type
            r#"{"id":7,"quote":{"mid_e9":"1","spread_bps":70000,"skew_bps":0,"max_size_base":"1"}}"#,
            // in the type, but not a price
            r#"{"id":7,"quote":{"mid_e9":"0","spread_bps":10,"skew_bps":0,"max_size_base":"1"}}"#,
            r#"{"id":7,"quote":{"mid_e9":"1","spread_bps":10000,"skew_bps":0,"max_size_base":"1"}}"#,
            r#"{"id":7,"quote":{"mid_e9":"1","spread_bps":10,"skew_bps":-10000,"max_size_base":"1"}}"#,
        ] {
            assert!(
                matches!(decide(broken), Err(ModelError::Protocol { .. })),
                "{broken}: {:?}",
                decide(broken)
            );
        }
    }

    #[test]
    fn a_model_may_decline_to_quote() {
        let mut model = start(fake(
            "    answer({'id': i, 'withdraw': 'news in ten seconds'})",
        ));
        assert_eq!(
            model.price(&state(), soon()),
            Ok(Decision::Withdraw {
                reason: "news in ten seconds".into()
            })
        );
    }

    #[test]
    fn stderr_is_the_model_s_log_not_its_answer() {
        let mut model = start(fake(
            "    print('thinking', file=sys.stderr, flush=True)\n    answer(quote(i))",
        ));
        assert_eq!(spread_of(model.price(&state(), soon())), 1);
        assert_eq!(spread_of(model.price(&state(), soon())), 2);
    }

    /// A stray `print` breaks one answer, not the model: the real answer that
    /// follows it is late by then and is discarded, and the next question is
    /// answered normally.
    #[test]
    fn a_broken_line_costs_one_answer() {
        let mut model = start(fake(
            "    if n == 1: print('debug: got it', flush=True)\n    answer(quote(i))",
        ));
        assert!(matches!(
            model.price(&state(), soon()),
            Err(ModelError::Protocol { .. })
        ));
        assert_eq!(spread_of(model.price(&state(), soon())), 2);
    }

    #[test]
    fn an_answer_to_a_question_never_asked_is_a_protocol_error() {
        let mut model = start(fake("    answer(quote(i + 5))"));
        match model.price(&state(), soon()) {
            Err(ModelError::Protocol { detail }) => {
                assert!(detail.contains("never asked"), "{detail}")
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    /// The late answer is not taken for the next one: the second call gets the
    /// answer to the second question, and there is never more than one
    /// question in the pipe.
    #[test]
    fn a_late_answer_is_discarded_not_used() {
        let mut model = start(fake("    if n == 1: time.sleep(0.3)\n    answer(quote(i))"));
        let late = model.price(&state(), Instant::now() + Duration::from_millis(50));
        assert!(matches!(late, Err(ModelError::Timeout { .. })), "{late:?}");
        assert_eq!(spread_of(model.price(&state(), soon())), 2);
        assert_eq!(model.open, None);
    }

    #[test]
    fn a_model_stuck_past_the_hang_limit_is_restarted() {
        let mut model = start(fake(
            "    if i == 1: time.sleep(3600)\n    answer(quote(i))",
        ))
        .with_hang_limit(Duration::from_millis(200));
        let first_pid = model.pid();
        let stuck = model.price(&state(), Instant::now() + Duration::from_millis(50));
        assert!(
            matches!(stuck, Err(ModelError::Timeout { .. })),
            "{stuck:?}"
        );
        thread::sleep(Duration::from_millis(250));
        match model.price(&state(), soon()) {
            Err(ModelError::Down { reason, .. }) => {
                assert!(reason.contains("has been open"), "{reason}")
            }
            other => panic!("expected the model down, got {other:?}"),
        }
        // A fresh process, and it answers the next question.
        assert_eq!(spread_of(model.price(&state(), soon())), 2);
        assert_ne!(model.pid(), first_pid);
    }

    #[test]
    fn a_model_that_dies_is_started_again() {
        let mut model = start(fake("    answer(quote(i))\n    sys.exit(3)"));
        let first_pid = model.pid();
        assert_eq!(spread_of(model.price(&state(), soon())), 1);
        match model.price(&state(), soon()) {
            Err(ModelError::Down { reason, retry_in }) => {
                assert!(reason.contains("exit status: 3"), "{reason}");
                assert_eq!(retry_in, Duration::ZERO);
            }
            other => panic!("expected the model down, got {other:?}"),
        }
        assert_eq!(spread_of(model.price(&state(), soon())), 3);
        assert_ne!(model.pid(), first_pid);
    }

    #[test]
    fn while_the_pause_lasts_nothing_is_started() {
        let pause = Backoff {
            min: Duration::from_secs(3600),
            max: Duration::from_secs(3600),
        };
        let mut model =
            ExternalProcessModel::start(fake("    sys.exit(3)"), pause).expect("python3 starts");
        let first = model.price(&state(), soon());
        let Err(ModelError::Down { reason, .. }) = first else {
            panic!("expected the model down, got {first:?}")
        };
        match model.price(&state(), soon()) {
            Err(ModelError::Down {
                reason: again,
                retry_in,
            }) => {
                assert_eq!(again, reason);
                assert!(retry_in > Duration::from_secs(3500), "{retry_in:?}");
            }
            other => panic!("expected the model down, got {other:?}"),
        }
        assert_eq!(model.pid(), None);
    }

    #[test]
    fn a_program_that_does_not_exist_is_refused_at_start() {
        let error = ExternalProcessModel::start(ModelCommand::new("/nonexistent/model"), AT_ONCE)
            .expect_err("nothing to start");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_pause_doubles_up_to_the_cap_and_resets_on_an_answer() {
        let backoff = Backoff {
            min: Duration::from_secs(1),
            max: Duration::from_secs(5),
        };
        let mut restart = Restart::new(backoff);
        let t0 = Instant::now();
        assert_eq!(restart.wait(t0), Duration::ZERO);

        let waits: Vec<_> = (0..5).map(|_| restart.died(t0).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 5, 5]);
        assert_eq!(restart.wait(t0), Duration::from_secs(5));
        assert_eq!(
            restart.wait(t0 + Duration::from_secs(2)),
            Duration::from_secs(3)
        );
        assert_eq!(restart.wait(t0 + Duration::from_secs(9)), Duration::ZERO);

        restart.answered();
        assert_eq!(restart.died(t0).as_secs(), 1);
    }
}
