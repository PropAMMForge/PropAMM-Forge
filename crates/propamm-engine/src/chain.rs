//! The engine's view of the network: read the vault, get a blockhash, send a
//! signed transaction — and say what a refusal means for the engine.
//!
//! Behind the [`Chain`] trait, like the feed's `Stream` and the model's
//! process: the sender and the loop are tested on a scripted chain, and
//! [`RpcChain`] is the one implementation that talks to a node. Its part that
//! can be wrong — reading the node's refusals — is tested on recorded answers.
//!
//! # One read for everything the engine needs from the chain
//!
//! The vault, both treasuries and the pricing authority's own account go in one
//! `getMultipleAccounts` at the tip (T032 decision: once per heartbeat, and
//! after every send to see whether it landed). It gives the inventory the model
//! prices, the quote actually on the book, whether the vault is halted, whether
//! this key is still the pricing authority, whether it can still pay fees, and
//! the slot the engine's clock is anchored on.
//!
//! # What a refusal means
//!
//! [`Refusal`] sorts the node's answers by what the engine does next, not by
//! where they came from:
//!
//! - [`Refusal::Transient`], [`Refusal::StaleBlockhash`], [`Refusal::Rejected`]
//!   — this transaction is lost; the next price tries again (after a pause, so
//!   a node answering 429 is not hammered);
//! - [`Refusal::Halted`] — the vault does not quote; wait for a resume;
//! - [`Refusal::Fatal`] — retrying cannot help: a different pricing authority,
//!   no vault at the address, no SOL for fees, no program. The engine stops
//!   with the reason instead of burning calls.
//!
//! A custom error code is trusted only from the instruction at index 0: the
//! engine's transactions carry exactly one instruction, its own, so the code is
//! the vault program's and not some other program's that happens to share a number.

use anchor_lang::prelude::Pubkey;
use anchor_lang::AccountDeserialize;
use propamm_client::chain::{decode_mint, decode_token_account};
use propamm_client::rpc::{Account, NodeError, Rpc};
use propamm_quote::Inventory;
use propamm_vault::errors::VaultError;
use propamm_vault::state::Vault;
use serde_json::Value;
use thiserror::Error;

/// Custom codes below this are the vault program's own; from 2000 up to it
/// they are Anchor's account and constraint checks — the accounts are not what
/// the program expects, which no retry changes.
const ANCHOR_ACCOUNT_CODES: std::ops::Range<u64> = 2000..5000;

/// A refusal no retry can fix. The engine stops with it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Fatal {
    #[error("{signer} is not the pricing authority of the vault: {detail}")]
    NotPricingAuthority { signer: Pubkey, detail: String },
    #[error("there is no vault of this program at {vault}: {detail}")]
    NoVault { vault: Pubkey, detail: String },
    #[error("the pricing authority {payer} cannot pay the fee: {detail}")]
    NoFeeFunds { payer: Pubkey, detail: String },
    #[error("the program is not on this network: {detail}")]
    NoProgram { detail: String },
}

/// Why the chain did not do what was asked — sorted by what the engine does next.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refusal {
    /// The node could not be reached, is rate-limiting or is behind.
    #[error("the node did not take the request: {detail}")]
    Transient { detail: String },
    /// The blockhash the transaction was signed with is unknown to the node.
    #[error("the blockhash is not known to the node")]
    StaleBlockhash,
    /// The program refused this transaction for a reason that is not a state
    /// of the vault — a quote outside the domain, a bug.
    #[error("the program refused the transaction: {detail}")]
    Rejected { detail: String },
    /// The vault is halted and takes no quotes (FR-024).
    #[error("the vault is halted")]
    Halted,
    #[error(transparent)]
    Fatal(Fatal),
}

/// The quote on the book, as the vault holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Book {
    pub mid_e9: u128,
    pub spread_bps: u16,
    pub skew_bps: i16,
    pub max_size_base: u64,
    /// The slot the chain stamped it with when it landed.
    pub quote_slot: u64,
}

/// One read of the vault and everything around it, at the tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaultView {
    /// The slot the node read it at.
    pub slot: u64,
    pub halted: bool,
    pub pricing_authority: Pubkey,
    /// FR-007 — what the update rule, the budget and the silence bound are checked against.
    pub max_quote_age_slots: u32,
    /// FR-026 — the scale the model measures skew against.
    pub max_skew_bps: u16,
    pub inventory: Inventory,
    /// `None` when the vault has no quote (never posted, or cleared).
    pub book: Option<Book>,
    /// What the pricing authority has left for fees.
    pub payer_lamports: u64,
}

/// What does not change while the engine runs: read once, at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deployment {
    /// The vault's owner program — taken from the account, not from config.
    pub program_id: Pubkey,
    pub vault: Pubkey,
    pub base_decimals: u8,
    pub quote_decimals: u8,
}

/// The network as the engine uses it.
pub trait Chain {
    /// Read the vault, its treasuries and the payer at the tip.
    ///
    /// # Errors
    ///
    /// [`Refusal::Fatal`] when the vault or a treasury is gone; otherwise
    /// [`Refusal::Transient`].
    fn read(&mut self) -> Result<VaultView, Refusal>;

    /// A recent blockhash to sign with.
    ///
    /// # Errors
    ///
    /// [`Refusal::Transient`].
    fn blockhash(&mut self) -> Result<solana_hash::Hash, Refusal>;

    /// Send a signed transaction. `Ok` means the node took it — not that it landed.
    ///
    /// # Errors
    ///
    /// The node's refusal, sorted.
    fn send(&mut self, wire: &[u8]) -> Result<(), Refusal>;
}

/// [`Chain`] over a JSON-RPC node.
pub struct RpcChain {
    rpc: Rpc,
    vault: Pubkey,
    payer: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
}

impl RpcChain {
    /// Find the vault and its pair.
    ///
    /// # Errors
    ///
    /// [`Refusal::Fatal`] when there is no vault at `vault` or its mints are
    /// not mints; [`Refusal::Transient`] when the node did not answer.
    pub fn connect(rpc: Rpc, vault: Pubkey, payer: Pubkey) -> Result<(Self, Deployment), Refusal> {
        let account = rpc
            .get_account(&vault)
            .map_err(|error| sort_node_error(&error))?
            .ok_or_else(|| no_vault(vault, "the account does not exist"))?;
        let state = decode(&vault, &account)?;
        let mints = rpc
            .get_multiple_accounts(&[state.base_mint, state.quote_mint])
            .map_err(|error| sort_node_error(&error))?;
        let mint = |index: usize, address: &Pubkey| {
            let account = mints[index]
                .as_ref()
                .ok_or_else(|| no_vault(vault, format!("its mint {address} does not exist")))?;
            decode_mint(address, account).map_err(|error| no_vault(vault, format!("{error:#}")))
        };
        let base = mint(0, &state.base_mint)?;
        let quote = mint(1, &state.quote_mint)?;
        let chain = Self {
            rpc,
            vault,
            payer,
            base_vault: state.base_vault,
            quote_vault: state.quote_vault,
        };
        let deployment = Deployment {
            program_id: account.owner,
            vault,
            base_decimals: base.decimals,
            quote_decimals: quote.decimals,
        };
        Ok((chain, deployment))
    }
}

impl Chain for RpcChain {
    fn read(&mut self) -> Result<VaultView, Refusal> {
        let snapshot = self
            .rpc
            .get_multiple_accounts_at_tip(&[
                self.vault,
                self.base_vault,
                self.quote_vault,
                self.payer,
            ])
            .map_err(|error| sort_node_error(&error))?;
        let [vault, base, quote, payer] = snapshot.accounts.as_slice() else {
            return Err(Refusal::Transient {
                detail: "the node returned a different number of accounts".into(),
            });
        };
        let vault = vault
            .as_ref()
            .ok_or_else(|| no_vault(self.vault, "the account is gone"))?;
        let state = decode(&self.vault, vault)?;
        let amount = |account: &Option<Account>, address: &Pubkey| {
            let account = account
                .as_ref()
                .ok_or_else(|| no_vault(self.vault, format!("its treasury {address} is gone")))?;
            decode_token_account(address, account)
                .map(|info| info.amount)
                .map_err(|error| no_vault(self.vault, format!("{error:#}")))
        };
        Ok(VaultView {
            slot: snapshot.slot,
            halted: state.halted,
            pricing_authority: state.pricing_authority,
            max_quote_age_slots: state.max_quote_age_slots,
            max_skew_bps: state.max_skew_bps,
            inventory: Inventory {
                base_amount: amount(base, &self.base_vault)?,
                quote_amount: amount(quote, &self.quote_vault)?,
            },
            book: (state.mid_e9 != 0).then_some(Book {
                mid_e9: state.mid_e9,
                spread_bps: state.spread_bps,
                skew_bps: state.skew_bps,
                max_size_base: state.max_size_base,
                quote_slot: state.quote_slot,
            }),
            payer_lamports: payer.as_ref().map_or(0, |account| account.lamports),
        })
    }

    fn blockhash(&mut self) -> Result<solana_hash::Hash, Refusal> {
        self.rpc
            .get_latest_blockhash()
            .map_err(|error| sort_node_error(&error))
    }

    fn send(&mut self, wire: &[u8]) -> Result<(), Refusal> {
        match self.rpc.send_transaction(wire) {
            Ok(_) => Ok(()),
            Err(error) => match sort_send_error(&error, self.payer, self.vault) {
                // The same transaction went out before and was processed: the
                // node has it, which is all `send` promises.
                None => Ok(()),
                Some(refusal) => Err(refusal),
            },
        }
    }
}

fn decode(address: &Pubkey, account: &Account) -> Result<Vault, Refusal> {
    let mut data = account.data.as_slice();
    Vault::try_deserialize(&mut data).map_err(|error| no_vault(*address, error.to_string()))
}

fn no_vault(vault: Pubkey, detail: impl Into<String>) -> Refusal {
    Refusal::Fatal(Fatal::NoVault {
        vault,
        detail: detail.into(),
    })
}

/// A failed read: the node is unreachable or behind — nothing fatal about it.
fn sort_node_error(error: &anyhow::Error) -> Refusal {
    Refusal::Transient {
        detail: format!("{error:#}"),
    }
}

/// A failed `sendTransaction`. `None` — the transaction was already processed.
fn sort_send_error(error: &anyhow::Error, payer: Pubkey, vault: Pubkey) -> Option<Refusal> {
    let Some(node) = error.downcast_ref::<NodeError>() else {
        // The request never got an answer: transport, timeout, HTTP 429.
        return Some(Refusal::Transient {
            detail: format!("{error:#}"),
        });
    };
    match &node.err {
        Some(err) => sort_transaction_error(err, payer, vault, &node.to_string()),
        // A JSON-RPC error with no transaction behind it: the node is unhealthy,
        // behind, or did not like the request.
        None => Some(Refusal::Transient {
            detail: node.to_string(),
        }),
    }
}

/// Sort a `TransactionError` as the node spells it in JSON. `None` — it is
/// `AlreadyProcessed`, which is not a failure.
///
/// `detail` goes into the refusal: for a program error it carries the logs.
pub fn sort_transaction_error(
    err: &Value,
    payer: Pubkey,
    vault: Pubkey,
    detail: &str,
) -> Option<Refusal> {
    let detail = detail.to_string();
    if let Some(name) = err.as_str() {
        return match name {
            "AlreadyProcessed" => None,
            "BlockhashNotFound" => Some(Refusal::StaleBlockhash),
            "InsufficientFundsForFee" | "AccountNotFound" => {
                Some(Refusal::Fatal(Fatal::NoFeeFunds { payer, detail }))
            }
            "ProgramAccountNotFound" | "InvalidProgramForExecution" => {
                Some(Refusal::Fatal(Fatal::NoProgram { detail }))
            }
            // Congestion and account locks pass; anything else we do not know
            // is retried rather than fatal — it costs one call per price.
            _ => Some(Refusal::Transient { detail }),
        };
    }
    if err.get("InsufficientFundsForRent").is_some() {
        return Some(Refusal::Fatal(Fatal::NoFeeFunds { payer, detail }));
    }
    let Some([index, inner]) = err
        .get("InstructionError")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return Some(Refusal::Transient { detail });
    };
    // One instruction per transaction: anything but index 0 is not ours.
    if index.as_u64() != Some(0) {
        return Some(Refusal::Rejected { detail });
    }
    let Some(code) = inner.get("Custom").and_then(Value::as_u64) else {
        return Some(Refusal::Rejected { detail });
    };
    let is = |error: VaultError| code == u64::from(u32::from(error));
    Some(if is(VaultError::VaultHalted) {
        Refusal::Halted
    } else if is(VaultError::PricingAuthorityOnly) {
        Refusal::Fatal(Fatal::NotPricingAuthority {
            signer: payer,
            detail,
        })
    } else if ANCHOR_ACCOUNT_CODES.contains(&code) {
        Refusal::Fatal(Fatal::NoVault { vault, detail })
    } else {
        Refusal::Rejected { detail }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anchor_lang::{AccountSerialize, Discriminator};
    use anyhow::anyhow;
    use base64::Engine as _;
    use propamm_client::rpc::Transport;
    use serde_json::json;

    use super::*;

    fn payer() -> Pubkey {
        Pubkey::new_from_array([7; 32])
    }

    fn sort(err: Value) -> Option<Refusal> {
        sort_transaction_error(&err, payer(), Pubkey::new_from_array([1; 32]), "logs")
    }

    #[test]
    fn a_halted_vault_is_told_apart_from_other_program_errors() {
        let code = u32::from(VaultError::VaultHalted);
        assert_eq!(
            sort(json!({"InstructionError": [0, {"Custom": code}]})),
            Some(Refusal::Halted)
        );
    }

    /// The codes come from the program's own enum, and they are the numbers the
    /// node reports: a reordering of `VaultError` shows up here, not on chain.
    #[test]
    fn the_codes_are_the_programs_own() {
        assert_eq!(u32::from(VaultError::PricingAuthorityOnly), 6005);
        assert_eq!(u32::from(VaultError::InvalidQuote), 6006);
        assert_eq!(u32::from(VaultError::VaultHalted), 6007);
    }

    #[test]
    fn another_key_is_fatal() {
        let code = u32::from(VaultError::PricingAuthorityOnly);
        assert!(matches!(
            sort(json!({"InstructionError": [0, {"Custom": code}]})),
            Some(Refusal::Fatal(Fatal::NotPricingAuthority { .. }))
        ));
    }

    #[test]
    fn an_invalid_quote_is_rejected_not_fatal() {
        let code = u32::from(VaultError::InvalidQuote);
        assert!(matches!(
            sort(json!({"InstructionError": [0, {"Custom": code}]})),
            Some(Refusal::Rejected { .. })
        ));
    }

    /// `AccountNotInitialized` (3012), `ConstraintSeeds` (2006): the vault at
    /// the address is not a vault — no retry makes it one.
    #[test]
    fn anchor_account_checks_are_fatal() {
        for code in [2006, 3012] {
            assert!(
                matches!(
                    sort(json!({"InstructionError": [0, {"Custom": code}]})),
                    Some(Refusal::Fatal(Fatal::NoVault { .. }))
                ),
                "{code}"
            );
        }
    }

    /// The memory of an earlier project: a custom code is the failing
    /// program's, whichever it is. Ours is only ever at index 0.
    #[test]
    fn a_custom_code_from_another_instruction_is_not_read_as_ours() {
        let code = u32::from(VaultError::VaultHalted);
        assert!(matches!(
            sort(json!({"InstructionError": [1, {"Custom": code}]})),
            Some(Refusal::Rejected { .. })
        ));
    }

    #[test]
    fn a_payer_without_sol_is_fatal() {
        for err in [
            json!("InsufficientFundsForFee"),
            json!("AccountNotFound"),
            json!({"InsufficientFundsForRent": {"account_index": 0}}),
        ] {
            assert!(
                matches!(
                    sort(err.clone()),
                    Some(Refusal::Fatal(Fatal::NoFeeFunds { .. }))
                ),
                "{err}"
            );
        }
    }

    #[test]
    fn a_stale_blockhash_and_congestion_are_retried() {
        assert_eq!(
            sort(json!("BlockhashNotFound")),
            Some(Refusal::StaleBlockhash)
        );
        assert!(matches!(
            sort(json!("WouldExceedMaxBlockCostLimit")),
            Some(Refusal::Transient { .. })
        ));
    }

    #[test]
    fn already_processed_is_not_a_failure() {
        assert_eq!(sort(json!("AlreadyProcessed")), None);
    }

    // ── RpcChain on recorded answers ────────────────────────────────────────

    struct Canned(RefCell<Vec<String>>);

    impl Transport for Canned {
        fn post_json(&self, _url: &str, body: &str) -> anyhow::Result<String> {
            self.0
                .borrow_mut()
                .pop()
                .ok_or_else(|| anyhow!("no answer prepared for {body}"))
        }
    }

    fn chain(replies: Vec<String>) -> RpcChain {
        let transport = Canned(RefCell::new(replies.into_iter().rev().collect()));
        RpcChain {
            rpc: Rpc::with_transport("http://test", Box::new(transport)),
            vault: Pubkey::new_from_array([1; 32]),
            payer: payer(),
            base_vault: Pubkey::new_from_array([2; 32]),
            quote_vault: Pubkey::new_from_array([3; 32]),
        }
    }

    fn encoded(data: &[u8], owner: &Pubkey, lamports: u64) -> Value {
        json!({
            "lamports": lamports,
            "owner": owner.to_string(),
            "executable": false,
            "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
            "rentEpoch": 0
        })
    }

    fn vault_bytes(vault: &Vault) -> Vec<u8> {
        let mut data = Vec::new();
        vault.try_serialize(&mut data).unwrap();
        assert_eq!(&data[..8], Vault::DISCRIMINATOR);
        data
    }

    fn token_bytes(amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data
    }

    fn sample_vault() -> Vault {
        Vault {
            owner: Pubkey::new_from_array([9; 32]),
            pricing_authority: payer(),
            halt_authority: Pubkey::new_from_array([8; 32]),
            base_mint: Pubkey::new_from_array([4; 32]),
            quote_mint: Pubkey::new_from_array([5; 32]),
            base_vault: Pubkey::new_from_array([2; 32]),
            quote_vault: Pubkey::new_from_array([3; 32]),
            mid_e9: 150_000_000,
            max_size_base: 1_000,
            quote_slot: 40,
            max_quote_age_slots: 25,
            spread_bps: 10,
            skew_bps: -3,
            max_skew_bps: 2_000,
            halted: false,
            bump: 254,
        }
    }

    fn read_reply(vault: &Vault, base: u64, quote: u64, payer_lamports: u64) -> String {
        let token = anchor_spl::token::ID;
        json!({"jsonrpc": "2.0", "id": 1, "result": {"context": {"slot": 1234}, "value": [
            encoded(&vault_bytes(vault), &propamm_vault::ID, 3_000_000),
            encoded(&token_bytes(base), &token, 2_039_280),
            encoded(&token_bytes(quote), &token, 2_039_280),
            {"lamports": payer_lamports, "owner": "11111111111111111111111111111111",
             "executable": false, "data": ["", "base64"], "rentEpoch": 0},
        ]}})
        .to_string()
    }

    #[test]
    fn one_read_gives_the_inventory_the_book_and_the_slot() {
        let view = chain(vec![read_reply(&sample_vault(), 10_000, 2_500, 900_000)])
            .read()
            .unwrap();
        assert_eq!(view.slot, 1234);
        assert_eq!(
            view.inventory,
            Inventory {
                base_amount: 10_000,
                quote_amount: 2_500
            }
        );
        assert_eq!(view.max_quote_age_slots, 25);
        assert_eq!(view.payer_lamports, 900_000);
        assert_eq!(
            view.book,
            Some(Book {
                mid_e9: 150_000_000,
                spread_bps: 10,
                skew_bps: -3,
                max_size_base: 1_000,
                quote_slot: 40
            })
        );
    }

    /// A zero mid is how the program stores "no quote" — cleared or never set.
    #[test]
    fn a_cleared_vault_has_no_book() {
        let mut vault = sample_vault();
        vault.mid_e9 = 0;
        let view = chain(vec![read_reply(&vault, 1, 1, 1)]).read().unwrap();
        assert_eq!(view.book, None);
    }

    #[test]
    fn a_vault_that_is_gone_is_fatal() {
        let reply = json!({"jsonrpc": "2.0", "id": 1, "result": {"context": {"slot": 1},
            "value": [null, null, null, null]}})
        .to_string();
        assert!(matches!(
            chain(vec![reply]).read(),
            Err(Refusal::Fatal(Fatal::NoVault { .. }))
        ));
    }

    #[test]
    fn a_read_the_node_did_not_answer_is_transient() {
        assert!(matches!(
            chain(vec![]).read(),
            Err(Refusal::Transient { .. })
        ));
    }

    /// Recorded from a preflight failure: the refusal comes back sorted, with
    /// the program log in its detail.
    #[test]
    fn a_preflight_refusal_is_sorted_with_its_logs() {
        let reply = json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32002,
            "message": "Transaction simulation failed: Error processing Instruction 0: custom program error: 0x1777",
            "data": {
                "err": {"InstructionError": [0, {"Custom": 6007}]},
                "logs": [
                    "Program log: Instruction: UpdateQuote",
                    "Program log: AnchorError occurred. Error Code: VaultHalted. Error Number: 6007. Error Message: vault is halted."
                ]
            }
        }})
        .to_string();
        assert_eq!(chain(vec![reply]).send(&[1]), Err(Refusal::Halted));
    }

    #[test]
    fn an_already_processed_send_is_success() {
        let reply = json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32002,
            "message": "Transaction simulation failed: This transaction has already been processed",
            "data": {"err": "AlreadyProcessed", "logs": []}
        }})
        .to_string();
        assert_eq!(chain(vec![reply]).send(&[1]), Ok(()));
    }

    #[test]
    fn a_send_that_got_no_answer_is_transient() {
        assert!(matches!(
            chain(vec![]).send(&[1]),
            Err(Refusal::Transient { .. })
        ));
    }

    /// Node unhealthy: a JSON-RPC error with no transaction behind it.
    #[test]
    fn a_node_that_is_behind_is_transient() {
        let reply = json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32005, "message": "Node is behind by 42 slots"
        }})
        .to_string();
        assert!(matches!(
            chain(vec![reply]).send(&[1]),
            Err(Refusal::Transient { .. })
        ));
    }
}
