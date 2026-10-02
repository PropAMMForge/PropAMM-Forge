//! Counting the node calls the engine makes, by method.
//!
//! The free RPC tiers bill per call (PLAN, "RPC credits"), and a provider's
//! dashboard answers a day late and for every client on the key at once. So
//! the engine keeps its own count: [`Metered`] wraps the transport, sorts every
//! request by its JSON-RPC `method`, and logs the running totals once a
//! [`LOG_EVERY`]. The binary logs them once more when it stops.
//!
//! A call is counted when it is attempted, not when it succeeds: a provider
//! bills a refused `sendTransaction` the same as one that landed.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use propamm_client::rpc::Transport;
use tracing::info;

/// How often the running totals go to the log.
pub const LOG_EVERY: Duration = Duration::from_secs(60);

/// The counts, shared between the transport and whoever reports them.
#[derive(Clone)]
pub struct Meter {
    inner: Rc<RefCell<Counts>>,
}

struct Counts {
    started: Instant,
    logged: Instant,
    by_method: BTreeMap<String, u64>,
}

impl Meter {
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Counts {
                started: now,
                logged: now,
                by_method: BTreeMap::new(),
            })),
        }
    }

    /// Count one call.
    pub fn count(&self, method: &str) {
        let mut counts = self.inner.borrow_mut();
        *counts.by_method.entry(method.to_owned()).or_default() += 1;
    }

    /// The totals so far.
    #[must_use]
    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let counts = self.inner.borrow();
        Snapshot {
            elapsed: now.saturating_duration_since(counts.started),
            by_method: counts.by_method.clone(),
        }
    }

    /// The totals, if [`LOG_EVERY`] has passed since they were last due.
    fn due(&self, now: Instant) -> Option<Snapshot> {
        {
            let mut counts = self.inner.borrow_mut();
            if now.saturating_duration_since(counts.logged) < LOG_EVERY {
                return None;
            }
            counts.logged = now;
        }
        Some(self.snapshot(now))
    }
}

/// The totals at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub elapsed: Duration,
    pub by_method: BTreeMap<String, u64>,
}

impl Snapshot {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_method.values().sum()
    }

    /// Write the totals to the log, in the one form a reader of the log parses.
    pub fn log(&self) {
        info!(
            elapsed_s = self.elapsed.as_secs(),
            total = self.total(),
            calls = %self,
            "rpc calls"
        );
    }
}

/// `method=count` pairs, by method name: `getLatestBlockhash=3 sendTransaction=40`.
impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (method, count) in &self.by_method {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{method}={count}")?;
        }
        Ok(())
    }
}

/// A transport that counts what passes through it.
pub struct Metered<T> {
    inner: T,
    meter: Meter,
}

impl<T: Transport> Metered<T> {
    #[must_use]
    pub fn new(inner: T, meter: Meter) -> Self {
        Self { inner, meter }
    }
}

impl<T: Transport> Transport for Metered<T> {
    fn post_json(&self, url: &str, body: &str) -> anyhow::Result<String> {
        self.meter.count(&method_of(body));
        let reply = self.inner.post_json(url, body);
        if let Some(totals) = self.meter.due(Instant::now()) {
            totals.log();
        }
        reply
    }
}

/// The `method` of a JSON-RPC request; a body without one still costs a call.
fn method_of(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|request| request["method"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use propamm_client::rpc::Rpc;
    use serde_json::json;

    use super::*;

    /// Answers every request with an empty result.
    struct Echo;

    impl Transport for Echo {
        fn post_json(&self, _: &str, _: &str) -> anyhow::Result<String> {
            Ok(r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_owned())
        }
    }

    /// Refuses every request.
    struct Down;

    impl Transport for Down {
        fn post_json(&self, _: &str, _: &str) -> anyhow::Result<String> {
            anyhow::bail!("connection refused")
        }
    }

    #[test]
    fn calls_are_counted_by_method() {
        let start = Instant::now();
        let meter = Meter::new(start);
        let rpc = Rpc::with_transport("http://node", Box::new(Metered::new(Echo, meter.clone())));
        for _ in 0..3 {
            rpc.call("sendTransaction", json!([])).unwrap();
        }
        rpc.call("getLatestBlockhash", json!([])).unwrap();

        let totals = meter.snapshot(start);
        assert_eq!(totals.total(), 4);
        assert_eq!(totals.to_string(), "getLatestBlockhash=1 sendTransaction=3");
    }

    #[test]
    fn a_failed_call_still_costs_one() {
        let start = Instant::now();
        let meter = Meter::new(start);
        let rpc = Rpc::with_transport("http://node", Box::new(Metered::new(Down, meter.clone())));
        assert!(rpc.call("sendTransaction", json!([])).is_err());
        assert_eq!(meter.snapshot(start).to_string(), "sendTransaction=1");
    }

    #[test]
    fn a_body_without_a_method_is_still_counted() {
        let meter = Meter::new(Instant::now());
        Metered::new(Echo, meter.clone())
            .post_json("http://node", "not json")
            .unwrap();
        assert_eq!(meter.snapshot(Instant::now()).to_string(), "unknown=1");
    }

    #[test]
    fn the_totals_fall_due_once_per_interval() {
        let start = Instant::now();
        let meter = Meter::new(start);
        meter.count("getSlot");
        assert_eq!(meter.due(start + LOG_EVERY / 2), None);
        let due = meter
            .due(start + LOG_EVERY)
            .expect("an interval has passed");
        assert_eq!(due.elapsed, LOG_EVERY);
        assert_eq!(due.total(), 1);
        // Not again until another full interval from the last one.
        assert_eq!(meter.due(start + LOG_EVERY + LOG_EVERY / 2), None);
        assert!(meter.due(start + LOG_EVERY * 2).is_some());
    }
}
