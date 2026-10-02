//! A stand-in for Pyth Hermes: an SSE stream on localhost whose price the run moves.
//!
//! # Why not the real Hermes
//!
//! SC-003 is "≤ 2 slots after the price moves past 5 bps", and the real feed
//! moves when the market does: how many such moves an hour gives, and when, is
//! not something a run can choose (T026 decision). Here the run decides the
//! moment of every move and knows it to the instant.
//!
//! # What it imitates, and why that much
//!
//! The engine opens it through its own `Https` transport and `Reader` — only
//! `PYTH_HERMES_URL` points elsewhere — so the wire format is Hermes' own:
//! `data: {"parsed":[…]}` events, prices as decimal strings with `expo` −8.
//! The cadence is Hermes' too, measured live on 2026-10-02: **one event a
//! second, `publish_time` one more each time**. That matters: the engine's
//! replay guard drops a sample whose `publish_time` is not newer than the last,
//! so a stand-in that sent faster than one a second would have its moves
//! dropped and measure the guard, not the engine.
//!
//! A move is sent at once, out of the one-second rhythm, with the next
//! `publish_time`; the regular event then waits for the wall clock to catch up,
//! so `publish_time` never runs more than a second ahead of it (the engine
//! refuses one more than 5 s ahead).
//!
//! One client at a time: the engine of one model is stopped before the engine
//! of the next starts; a client that went away frees the stand-in for the next.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

/// The rhythm of regular events.
const EVERY: Duration = Duration::from_secs(1);

/// How often the stand-in looks for a client while it has none.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Confidence of every sample: 2 bps, what SOL/USD showed live.
const CONF_BPS: u64 = 2;

/// A sample went out.
#[derive(Debug, Clone, Copy)]
pub struct Emitted {
    /// The moment the bytes were written to the socket.
    pub at: Instant,
    pub publish_time: i64,
}

enum Command {
    Move {
        mantissa: u64,
        reply: Sender<Result<Emitted, String>>,
    },
    Quiet(bool),
}

/// The stand-in, running on its own thread until dropped.
pub struct FakeHermes {
    url: String,
    commands: Option<Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeHermes {
    /// Serve one feed (`id`, 64 hex digits) starting at `mantissa` × 10⁻⁸ USD.
    ///
    /// # Errors
    ///
    /// If no local port can be taken.
    pub fn start(id: &str, mantissa: u64) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .context("the stand-in Hermes cannot take a port")?;
        listener
            .set_nonblocking(true)
            .context("the listener cannot be made non-blocking")?;
        let url = format!("http://{}", listener.local_addr()?);
        let (commands, rx) = mpsc::channel();
        let server = Server {
            listener,
            id: id.trim_start_matches("0x").to_owned(),
            mantissa,
            last_publish: 0,
            quiet: false,
            client: None,
        };
        let thread = thread::Builder::new()
            .name("fake-hermes".to_owned())
            .spawn(move || server.run(&rx))
            .context("the stand-in Hermes cannot start")?;
        Ok(Self {
            url,
            commands: Some(commands),
            thread: Some(thread),
        })
    }

    /// The base URL, for `PYTH_HERMES_URL`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Move the price and send it now.
    ///
    /// # Errors
    ///
    /// If no client is connected — there is nobody for the move to reach.
    pub fn move_to(&self, mantissa: u64) -> Result<Emitted> {
        let (reply, answer) = mpsc::channel();
        self.send(Command::Move { mantissa, reply })?;
        answer
            .recv()
            .context("the stand-in Hermes is gone")?
            .map_err(|reason| anyhow!(reason))
    }

    /// Stop (or resume) the regular events — a feed that went silent.
    ///
    /// # Errors
    ///
    /// If the stand-in is gone.
    pub fn set_quiet(&self, quiet: bool) -> Result<()> {
        self.send(Command::Quiet(quiet))
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .as_ref()
            .context("the stand-in Hermes is stopped")?
            .send(command)
            .map_err(|_| anyhow!("the stand-in Hermes is gone"))
    }
}

impl Drop for FakeHermes {
    fn drop(&mut self) {
        // Dropping the sender ends the server loop.
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Server {
    listener: TcpListener,
    id: String,
    mantissa: u64,
    last_publish: i64,
    quiet: bool,
    client: Option<TcpStream>,
}

impl Server {
    fn run(mut self, commands: &Receiver<Command>) {
        let mut next = Instant::now();
        loop {
            if self.client.is_none() {
                self.client = self.accept();
                next = Instant::now();
            }
            let wait = if self.client.is_some() {
                next.saturating_duration_since(Instant::now())
            } else {
                ACCEPT_POLL
            };
            match commands.recv_timeout(wait) {
                Ok(Command::Move { mantissa, reply }) => {
                    self.mantissa = mantissa;
                    let _ =
                        reply.send(self.emit(true).ok_or_else(|| {
                            "no engine is connected to the stand-in Hermes".to_owned()
                        }));
                }
                Ok(Command::Quiet(quiet)) => self.quiet = quiet,
                Err(RecvTimeoutError::Timeout) => {
                    if self.client.is_some() && !self.quiet {
                        self.emit(false);
                    }
                    next = Instant::now() + EVERY;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Take a client if one is waiting, read its request, answer with the stream head.
    fn accept(&self) -> Option<TcpStream> {
        let (stream, _) = self.listener.accept().ok()?;
        stream.set_nonblocking(false).ok()?;
        stream.set_nodelay(true).ok()?;
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut line = String::new();
        // The request head; the path and headers are not checked — the engine
        // asks for the one feed this stand-in has.
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) if line == "\r\n" || line == "\n" => break,
                Ok(_) => {}
            }
        }
        let mut stream = stream;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Cache-Control: no-cache\r\n\
                  Connection: close\r\n\r\n",
            )
            .ok()?;
        Some(stream)
    }

    /// Send the current price. A regular event waits for the wall clock; a
    /// move does not. `None` if there is no client or it went away.
    fn emit(&mut self, moved: bool) -> Option<Emitted> {
        let now = unix_now();
        if !moved && now <= self.last_publish {
            return None;
        }
        let publish_time = now.max(self.last_publish + 1);
        let event = event(&self.id, self.mantissa, publish_time);
        let client = self.client.as_mut()?;
        if client
            .write_all(event.as_bytes())
            .and_then(|()| client.flush())
            .is_err()
        {
            // The engine went away; wait for the next one.
            self.client = None;
            return None;
        }
        let at = Instant::now();
        self.last_publish = publish_time;
        Some(Emitted { at, publish_time })
    }
}

/// One SSE event in Hermes' shape.
fn event(id: &str, mantissa: u64, publish_time: i64) -> String {
    let conf = mantissa * CONF_BPS / 10_000;
    let price = format!(
        r#"{{"price":"{mantissa}","conf":"{conf}","expo":-8,"publish_time":{publish_time}}}"#
    );
    format!(
        "data: {{\"binary\":{{\"encoding\":\"hex\",\"data\":[]}},\"parsed\":[{{\"id\":\"{id}\",\
         \"price\":{price},\"ema_price\":{price},\"metadata\":{{\"prev_publish_time\":{}}}}}]}}\n\n",
        publish_time - 1
    )
}

fn unix_now() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    const ID: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";

    /// Connect as the engine would and read events off the stream.
    struct Client {
        reader: BufReader<TcpStream>,
    }

    impl Client {
        fn connect(url: &str) -> Self {
            let address = url.trim_start_matches("http://");
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(b"GET /v2/updates/price/stream HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    return Self { reader };
                }
            }
        }

        /// The next event's `(price, publish_time)`.
        fn next(&mut self) -> (u64, i64) {
            let mut line = String::new();
            loop {
                line.clear();
                assert!(
                    self.reader.read_line(&mut line).unwrap() > 0,
                    "stream ended"
                );
                if let Some(data) = line.strip_prefix("data: ") {
                    let event: serde_json::Value = serde_json::from_str(data).unwrap();
                    let price = &event["parsed"][0]["price"];
                    return (
                        price["price"].as_str().unwrap().parse().unwrap(),
                        price["publish_time"].as_i64().unwrap(),
                    );
                }
            }
        }
    }

    #[test]
    fn the_stream_carries_the_price_and_a_move_goes_out_at_once() {
        let hermes = FakeHermes::start(ID, 15_025_000_000).unwrap();
        let mut client = Client::connect(hermes.url());
        let (price, first) = client.next();
        assert_eq!(price, 15_025_000_000);

        let before = Instant::now();
        let emitted = hermes.move_to(15_040_000_000).unwrap();
        let (price, publish_time) = client.next();
        assert_eq!(price, 15_040_000_000);
        assert!(emitted.at.duration_since(before) < Duration::from_millis(200));
        assert!(publish_time > first, "the replay guard would drop it");
        assert_eq!(publish_time, emitted.publish_time);

        // The regular rhythm resumes, and never repeats a publish time.
        let (_, after) = client.next();
        assert!(after > publish_time);
    }

    #[test]
    fn the_event_is_what_the_engine_parses() {
        let line = event(ID, 15_025_000_000, 1_790_000_000);
        let data = line.strip_prefix("data: ").unwrap().trim_end();
        let samples = propamm_engine::feed::parse_event(data).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].id, ID.parse().unwrap());
        assert_eq!(samples[0].price, 15_025_000_000);
        assert_eq!(samples[0].publish_time, 1_790_000_000);
    }

    #[test]
    fn a_move_with_nobody_connected_is_an_error() {
        let hermes = FakeHermes::start(ID, 1).unwrap();
        assert!(hermes.move_to(2).is_err());
    }

    #[test]
    fn a_client_that_left_frees_the_stand_in_for_the_next() {
        let hermes = FakeHermes::start(ID, 15_025_000_000).unwrap();
        let mut first = Client::connect(hermes.url());
        first.next();
        let mut stream = first.reader.into_inner();
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        let _ = stream.read(&mut [0; 1]);
        drop(stream);

        // The server notices on its next write; the second client then gets the stream.
        let mut second = Client::connect(hermes.url());
        let (price, _) = second.next();
        assert_eq!(price, 15_025_000_000);
    }
}
