//! A thin Solana JSON-RPC client — exactly the methods the commands use.
//!
//! # Why not `solana-client`
//!
//! T023 decision. `solana-client` pulls in almost the whole agave stack — by the
//! resolver's count +378 crates versus +80 here — while of its forty methods
//! `forge` needs six. The instructions would have to be assembled by hand via
//! [`anchor_lang::InstructionData`] anyway, so the gain would come down to ready
//! wrappers over HTTP. The main point is not the weight, though: `jupiter-amm-interface`
//! asks for solana crates as `">=2"`, and every new point of contact with that
//! stack is one more open version range through which a third party's minor release stops our build.
//!
//! Checked with the resolver separately: **none** of the options moves
//! `solana-pubkey` 3.0.0 from under `anchor-lang` — the question was purely weight.
//!
//! # Why the transport is behind a trait
//!
//! [`Transport`] exists precisely so that response parsing and error message
//! formation are tested by a plain `cargo test` on recorded responses. The
//! on-chain error the CLI shows a human is half of its usefulness, and it must
//! not be tested only against a live network.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

/// How long we wait for each individual request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long we wait for a transaction to confirm before giving up.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(90);

/// Pause between status polls.
const CONFIRM_POLL: Duration = Duration::from_millis(500);

/// How `forge` talks to the node. The default implementation is [`Http`].
pub trait Transport {
    /// Send the body and return the response as text.
    ///
    /// # Errors
    ///
    /// If the request did not get through or the response cannot be read.
    fn post_json(&self, url: &str, body: &str) -> Result<String>;
}

/// HTTP transport on `ureq` with rustls.
///
/// rustls, not system TLS: the build has to pass the same way in WSL and on
/// Windows, and `openssl-sys` is the most common reason it does not pass everywhere.
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for Http {
    fn post_json(&self, url: &str, body: &str) -> Result<String> {
        let mut response = self
            .agent
            .post(url)
            .header("content-type", "application/json")
            .send(body)
            .with_context(|| format!("node {url} did not respond"))?;
        response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("the response of node {url} cannot be read"))
    }
}

/// An account in the form the commands need it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Account {
    pub lamports: u64,
    pub owner: Pubkey,
    pub executable: bool,
    pub data: Vec<u8>,
}

/// A confirmed transaction.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Confirmed {
    pub signature: String,
    pub slot: u64,
}

/// The node client.
pub struct Rpc {
    url: String,
    transport: Box<dyn Transport>,
    /// Request counter for the `id` field. `RefCell` because every method takes
    /// `&self`: the command holds the client together with the rest of the session,
    /// and `&mut` here would demand a mutable borrow for the whole call.
    next_id: RefCell<u64>,
}

impl Rpc {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_transport(url, Box::new(Http::new()))
    }

    #[must_use]
    pub fn with_transport(url: impl Into<String>, transport: Box<dyn Transport>) -> Self {
        Self {
            url: url.into(),
            transport,
            next_id: RefCell::new(1),
        }
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. Returns the contents of the `result` field.
    ///
    /// # Errors
    ///
    /// If the transport did not deliver the request, the response is not JSON-RPC
    /// or the node answered with an error.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut next = self.next_id.borrow_mut();
            let id = *next;
            *next = next.saturating_add(1);
            id
        };
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let text = self.transport.post_json(&self.url, &request.to_string())?;
        parse_response(method, &text)
    }

    /// The account, or `None` if it is not on the network.
    ///
    /// # Errors
    ///
    /// If the call failed or the data arrived in an encoding other than the one requested.
    pub fn get_account(&self, address: &Pubkey) -> Result<Option<Account>> {
        let result = self.call(
            "getAccountInfo",
            json!([address.to_string(), {"encoding": "base64", "commitment": "confirmed"}]),
        )?;
        parse_account(&result["value"])
    }

    /// Several accounts in one request.
    ///
    /// A batch, not a loop: `status` shows all the project's vaults together, and a
    /// separate request per account would spend the free RPC quota exactly where
    /// it counts (see the risks section in PLAN).
    ///
    /// # Errors
    ///
    /// If the call failed or the node returned a different number of accounts.
    pub fn get_multiple_accounts(&self, addresses: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<String> = addresses.iter().map(ToString::to_string).collect();
        let result = self.call(
            "getMultipleAccounts",
            json!([keys, {"encoding": "base64", "commitment": "confirmed"}]),
        )?;
        let values = result["value"]
            .as_array()
            .context("getMultipleAccounts did not return an array")?;
        if values.len() != addresses.len() {
            bail!(
                "requested {} accounts, got {}",
                addresses.len(),
                values.len()
            );
        }
        values.iter().map(parse_account).collect()
    }

    /// The current slot.
    ///
    /// # Errors
    ///
    /// If the call failed.
    pub fn get_slot(&self) -> Result<u64> {
        self.call("getSlot", json!([{"commitment": "confirmed"}]))?
            .as_u64()
            .context("getSlot did not return a number")
    }

    /// A fresh blockhash for signing.
    ///
    /// # Errors
    ///
    /// If the call failed or the hash does not parse.
    pub fn get_latest_blockhash(&self) -> Result<solana_hash::Hash> {
        let result = self.call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))?;
        let text = result["value"]["blockhash"]
            .as_str()
            .context("getLatestBlockhash did not return a blockhash")?;
        text.parse()
            .with_context(|| format!("\"{text}\" is not a blockhash"))
    }

    /// Send a signed transaction.
    ///
    /// Preflight is **not** disabled: the simulation costs nothing, and its logs are
    /// the only place the Anchor error code is visible before the transaction is
    /// paid for.
    ///
    /// # Errors
    ///
    /// If the node refused the transaction; the message carries the program logs.
    pub fn send_transaction(&self, wire: &[u8]) -> Result<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(wire);
        let result = self.call(
            "sendTransaction",
            json!([encoded, {"encoding": "base64", "preflightCommitment": "confirmed"}]),
        )?;
        result
            .as_str()
            .map(ToString::to_string)
            .context("sendTransaction did not return a signature")
    }

    /// Wait for confirmation and return the slot the transaction executed in.
    ///
    /// # Errors
    ///
    /// If the transaction ended with an error or did not confirm within the allotted time.
    pub fn confirm(&self, signature: &str) -> Result<Confirmed> {
        let deadline = Instant::now() + CONFIRM_TIMEOUT;
        loop {
            let result = self.call(
                "getSignatureStatuses",
                json!([[signature], {"searchTransactionHistory": true}]),
            )?;
            let status = &result["value"][0];
            if !status.is_null() {
                if !status["err"].is_null() {
                    bail!(
                        "transaction {signature} executed with an error: {}",
                        status["err"]
                    );
                }
                let level = status["confirmationStatus"].as_str().unwrap_or("processed");
                if matches!(level, "confirmed" | "finalized") {
                    return Ok(Confirmed {
                        signature: signature.to_string(),
                        // The slot comes from the status rather than being computed: the chain
                        // clock is the slot, and there is no second source for it.
                        slot: status["slot"].as_u64().unwrap_or_default(),
                    });
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "transaction {signature} did not confirm within {} s — it may still have executed; check: forge status",
                    CONFIRM_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(CONFIRM_POLL);
        }
    }
}

/// Parse the JSON-RPC envelope.
fn parse_response(method: &str, text: &str) -> Result<Value> {
    let mut value: Value = serde_json::from_str(text)
        .with_context(|| format!("{method}: the response is not JSON: {}", head(text)))?;

    if let Some(error) = value.get("error") {
        bail!("{}", describe_rpc_error(method, error));
    }
    match value.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => bail!(
            "{method}: the response has neither result nor error: {}",
            head(text)
        ),
    }
}

/// How many trailing log lines we show.
///
/// The tail, not the head: the error is at the end, and the lines before it
/// name the CPI in which it happened.
const LOG_TAIL: usize = 25;

/// A node error in readable form.
///
/// The program logs are pulled up on purpose: the `message` field holds
/// "Transaction simulation failed: Error processing Instruction 0…", i.e.
/// exactly zero information about the cause, while `AnchorError … Error Code: QuoteStale`
/// sits in the logs.
///
/// # Why the logs are not filtered to "interesting" lines
///
/// The first version kept only lines with the word `Error` — and on a real
/// refusal that removed exactly what was needed: the line "Error processing
/// Instruction" passed the filter, while `invoke [2]` before it, which names the failed CPI, did not.
/// A filter that keeps the line with the error's name and drops the line with
/// its location is worse than no filter. So the tail is shown whole, and the
/// error code is not translated into a name: a lookup table in the CLI would
/// diverge from the program on the first new code, while the log comes from the program itself.
fn describe_rpc_error(method: &str, error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("unknown error");
    let mut text = format!("{method}: {message}");
    if let Some(logs) = error["data"]["logs"].as_array() {
        let lines: Vec<&str> = logs.iter().filter_map(Value::as_str).collect();
        let skipped = lines.len().saturating_sub(LOG_TAIL);
        if skipped > 0 {
            let _ = write!(text, "\n    … {skipped} more log lines above");
        }
        for line in &lines[skipped..] {
            text.push_str("\n    ");
            text.push_str(line);
        }
    }
    text
}

/// Parse an account from the `value` field.
fn parse_account(value: &Value) -> Result<Option<Account>> {
    if value.is_null() {
        return Ok(None);
    }
    let owner = value["owner"]
        .as_str()
        .context("the account has no owner")?
        .parse::<Pubkey>()
        .context("the account owner is not an address")?;
    let encoded = value["data"][0]
        .as_str()
        .context("account data did not arrive as a [data, encoding] pair")?;
    let encoding = value["data"][1].as_str().unwrap_or("base64");
    if encoding != "base64" {
        bail!("the node returned data in encoding \"{encoding}\", not base64");
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("account data is not base64")?;
    Ok(Some(Account {
        lamports: value["lamports"].as_u64().unwrap_or_default(),
        owner,
        executable: value["executable"].as_bool().unwrap_or(false),
        data,
    }))
}

/// The beginning of the text for an error message.
fn head(text: &str) -> String {
    let cut: String = text.chars().take(200).collect();
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::anyhow;
    use std::rc::Rc;

    /// A transport that hands out pre-recorded responses in turn and remembers
    /// what was asked of it.
    struct Canned {
        replies: RefCell<Vec<String>>,
        seen: RefCell<Vec<String>>,
    }

    impl Canned {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: RefCell::new(replies.iter().rev().map(ToString::to_string).collect()),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Rc<Canned> {
        fn post_json(&self, _url: &str, body: &str) -> Result<String> {
            self.seen.borrow_mut().push(body.to_string());
            self.replies
                .borrow_mut()
                .pop()
                .ok_or_else(|| anyhow!("the test prepared no response for {body}"))
        }
    }

    fn spy(replies: &[&str]) -> (Rpc, Rc<Canned>) {
        let transport = Rc::new(Canned::new(replies));
        (
            Rpc::with_transport("http://test", Box::new(Rc::clone(&transport))),
            transport,
        )
    }

    fn rpc(replies: &[&str]) -> Rpc {
        spy(replies).0
    }

    #[test]
    fn a_result_comes_back_unwrapped() {
        let client = rpc(&[r#"{"jsonrpc":"2.0","id":1,"result":1284}"#]);
        assert_eq!(client.get_slot().unwrap(), 1284);
    }

    #[test]
    fn a_missing_account_is_none_and_not_an_error() {
        let client =
            rpc(&[r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":null}}"#]);
        assert!(client.get_account(&Pubkey::new_unique()).unwrap().is_none());
    }

    #[test]
    fn an_account_decodes_from_base64() {
        let client = rpc(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":{
            "lamports":2039280,
            "owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "executable":false,
            "data":["AQID","base64"],
            "rentEpoch":0
        }}}"#,
        ]);
        let account = client
            .get_account(&Pubkey::new_unique())
            .unwrap()
            .expect("the account should have been found");
        assert_eq!(account.data, vec![1, 2, 3]);
        assert_eq!(account.lamports, 2_039_280);
        assert!(!account.executable);
    }

    /// The module's most important test: the cause of an on-chain refusal is in
    /// the logs, not in the `message` field, and that is what a human has to see.
    #[test]
    fn a_program_error_surfaces_the_anchor_log_line() {
        let client = rpc(&[r#"{"jsonrpc":"2.0","id":1,"error":{
            "code":-32002,
            "message":"Transaction simulation failed: Error processing Instruction 0: custom program error: 0x1773",
            "data":{"logs":[
                "Program 77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq invoke [1]",
                "Program log: Instruction: UpdateQuote",
                "Program log: AnchorError occurred. Error Code: VaultHalted. Error Number: 6003. Error Message: vault is halted.",
                "Program 77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq consumed 4210 of 200000 compute units"
            ]}
        }}"#]);
        let err = client.send_transaction(&[1, 2, 3]).unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("VaultHalted"),
            "the error code got lost: {text}"
        );
        assert!(
            text.contains("Instruction: UpdateQuote"),
            "the line saying WHICH instruction failed got lost: {text}"
        );
    }

    /// A long log is cut from the head, not the tail, and says so directly: the
    /// cause of the refusal is at the end.
    #[test]
    fn a_long_log_keeps_its_tail_and_says_how_much_was_cut() {
        let lines: Vec<String> = (0..40).map(|index| format!("\"line {index}\"")).collect();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32002,"message":"failure","data":{{"logs":[{}]}}}}}}"#,
            lines.join(",")
        );
        let client = rpc(&[&body]);
        let text = format!("{}", client.send_transaction(&[]).unwrap_err());
        assert!(text.contains("line 39"), "the tail got lost: {text}");
        assert!(
            !text.contains("line 14"),
            "the head should have been cut: {text}"
        );
        assert!(text.contains("15 more log lines"), "cut not named: {text}");
    }

    /// An error without logs must stay readable, not disappear.
    #[test]
    fn an_error_without_logs_still_reports_its_message() {
        let client = rpc(&[
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid param: WrongSize"}}"#,
        ]);
        let err = client.get_slot().unwrap_err();
        assert!(format!("{err}").contains("WrongSize"), "{err}");
    }

    #[test]
    fn a_reply_that_is_not_json_rpc_is_named_as_such() {
        let client = rpc(&["<html>502 Bad Gateway</html>"]);
        let err = client.get_slot().unwrap_err();
        assert!(format!("{err}").contains("is not JSON"), "{err}");
    }

    #[test]
    fn a_short_count_from_get_multiple_accounts_is_an_error() {
        let client =
            rpc(&[r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[null]}}"#]);
        let err = client
            .get_multiple_accounts(&[Pubkey::new_unique(), Pubkey::new_unique()])
            .unwrap_err();
        assert!(format!("{err}").contains("got 1"), "{err}");
    }

    /// An empty request must not go to the network at all — otherwise `status` of
    /// a project without vaults would spend a call on nothing.
    #[test]
    fn asking_for_no_accounts_does_not_call_the_node() {
        let client = rpc(&[]);
        assert!(client.get_multiple_accounts(&[]).unwrap().is_empty());
    }

    #[test]
    fn a_failed_transaction_is_reported_from_its_status() {
        let client = rpc(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[
            {"slot":42,"confirmations":0,"err":{"InstructionError":[0,{"Custom":6003}]},"confirmationStatus":"confirmed"}
        ]}}"#,
        ]);
        let err = client.confirm("5xK2").unwrap_err();
        assert!(format!("{err}").contains("6003"), "{err}");
    }

    #[test]
    fn a_confirmed_transaction_reports_its_slot() {
        let client = rpc(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[
            {"slot":1284,"confirmations":null,"err":null,"confirmationStatus":"finalized"}
        ]}}"#,
        ]);
        let confirmed = client.confirm("5xK2").unwrap();
        assert_eq!(confirmed.slot, 1284);
        assert_eq!(confirmed.signature, "5xK2");
    }

    /// `id` must grow: the node may answer out of order, and equal `id`s would
    /// make the responses indistinguishable.
    #[test]
    fn request_ids_advance() {
        let (client, transport) = spy(&[
            r#"{"jsonrpc":"2.0","id":1,"result":1}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":2}"#,
        ]);
        client.get_slot().unwrap();
        client.get_slot().unwrap();
        let seen = transport.seen.borrow();
        assert!(seen[0].contains(r#""id":1"#), "{}", seen[0]);
        assert!(seen[1].contains(r#""id":2"#), "{}", seen[1]);
    }

    /// Addresses in a batch must travel in the same order they were requested in:
    /// matching the response to the request rests on that alone.
    #[test]
    fn a_batch_keeps_the_order_it_was_given() {
        let (client, transport) = spy(&[
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":1},"value":[null,null]}}"#,
        ]);
        let first = Pubkey::new_from_array([1; 32]);
        let second = Pubkey::new_from_array([2; 32]);
        client.get_multiple_accounts(&[first, second]).unwrap();
        let seen = transport.seen.borrow();
        let body = &seen[0];
        assert!(
            body.find(&first.to_string()) < body.find(&second.to_string()),
            "{body}"
        );
    }
}
