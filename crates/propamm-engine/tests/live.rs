//! Against the real Hermes. Ignored in a plain `cargo test`: the network is not
//! part of the gate, and the stream test needs a key.
//!
//! Run: `PYTH_API_KEY=… cargo test -p propamm-engine --test live -- --ignored --nocapture`

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use propamm_engine::feed::{
    Backoff, FeedError, FeedEvent, Https, Policy, PriceId, Reader, Stream, SystemClock,
    HERMES_DEFAULT_URL,
};

/// SOL/USD on Pythnet.
const SOL_USD: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

fn ids() -> Vec<PriceId> {
    vec![SOL_USD.parse().expect("a valid id")]
}

fn hermes_url() -> String {
    std::env::var("PYTH_HERMES_URL").unwrap_or_else(|_| HERMES_DEFAULT_URL.to_owned())
}

/// Since 2026-08-26 the public Hermes refuses a request without a key. This is
/// the one live check that needs no credentials: it exercises TLS, the headers
/// and the error mapping end to end.
#[test]
#[ignore = "needs the network"]
fn without_a_key_hermes_answers_unauthorized() {
    let url = propamm_engine::feed::stream_url(&hermes_url(), &ids());
    let error = Https::default()
        .open(&url, None)
        .err()
        .expect("Hermes let us in without a key");
    assert!(matches!(error, FeedError::Unauthorized), "{error}");
}

/// With a key: the first accepted price arrives within a few seconds.
#[test]
#[ignore = "needs the network and PYTH_API_KEY"]
fn with_a_key_the_first_price_arrives() {
    let key = std::env::var("PYTH_API_KEY").expect("PYTH_API_KEY is not set");
    let reader = Reader::new(
        &hermes_url(),
        ids(),
        Some(key),
        Policy::new(Duration::from_secs(5), 30),
        Https::default(),
        SystemClock,
    )
    .with_backoff(Backoff {
        min: Duration::from_millis(100),
        max: Duration::from_millis(100),
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || reader.run(&tx));

    let first = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("no event in 15 s");
    assert_eq!(first, FeedEvent::Connected);
    let second = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("no sample in 15 s");
    match second {
        FeedEvent::Price(price) => println!("{price:?}"),
        other => panic!("expected a price, got {other:?}"),
    }
}
