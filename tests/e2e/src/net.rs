//! What `forge` does not do but the run's preparation does: money from the
//! faucet, rent and sending a transaction with an arbitrary key.
//!
//! The node client here is the same [`propamm_cli::rpc::Rpc`] as in the product.
//! It already has the six methods the commands need; the four needed only by the
//! preparation are deliberately not in it — they go through the open
//! [`Rpc::call`](propamm_cli::rpc::Rpc::call), so as not to widen the CLI surface
//! for the sake of tests.

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use propamm_cli::rpc::{Confirmed, Rpc};
use serde_json::json;
use solana_keypair::Keypair;
use solana_signer::Signer as _;

/// How long to wait for the faucet to reach the balance.
const AIRDROP_ATTEMPTS: usize = 3;

/// Assemble, sign and send a transaction; the first signer pays.
///
/// Repeats `Session::send`, but without a project: before `forge init` there is
/// no config yet, and the pair's mints have to be created before it.
///
/// # Errors
///
/// If there are no signers, the node refused the transaction or it did not confirm.
pub fn send(
    rpc: &Rpc,
    instructions: &[anchor_lang::solana_program::instruction::Instruction],
    signers: &[&Keypair],
) -> Result<Confirmed> {
    let payer = signers
        .first()
        .context("transaction without a single signer")?;
    let blockhash = rpc.get_latest_blockhash()?;
    let message = solana_message::Message::new(instructions, Some(&payer.pubkey()));
    let mut transaction = solana_transaction::Transaction::new_unsigned(message);
    transaction
        .try_sign(signers, blockhash)
        .context("transaction cannot be signed")?;
    let wire = bincode::serialize(&transaction).context("transaction does not serialize")?;
    let signature = rpc.send_transaction(&wire)?;
    rpc.confirm(&signature)
}

/// How many lamports make an account of this size rent-exempt.
///
/// # Errors
///
/// If the node did not respond or responded with something other than a number.
pub fn rent_exempt(rpc: &Rpc, size: usize) -> Result<u64> {
    rpc.call("getMinimumBalanceForRentExemption", json!([size]))?
        .as_u64()
        .context("getMinimumBalanceForRentExemption did not return a number")
}

/// Balance in lamports.
///
/// # Errors
///
/// If the node did not respond or responded with something other than a number.
pub fn balance(rpc: &Rpc, address: &Pubkey) -> Result<u64> {
    rpc.call("getBalance", json!([address.to_string()]))?["value"]
        .as_u64()
        .context("getBalance did not return a number")
}

/// Give a key lamports from the faucet and wait until they show up.
///
/// The local validator's faucet sometimes refuses right after start — the node
/// is already healthy, but the faucet service is not yet. So there are several
/// attempts: otherwise the most common cause of a failed run would be random.
///
/// # Errors
///
/// If the faucet gave nothing in [`AIRDROP_ATTEMPTS`] attempts.
pub fn airdrop(rpc: &Rpc, address: &Pubkey, lamports: u64) -> Result<()> {
    let mut last = String::new();
    for _ in 0..AIRDROP_ATTEMPTS {
        match rpc.call("requestAirdrop", json!([address.to_string(), lamports])) {
            Ok(value) => {
                let signature = value
                    .as_str()
                    .context("requestAirdrop did not return a signature")?
                    .to_string();
                rpc.confirm(&signature)?;
                return Ok(());
            }
            Err(err) => {
                last = format!("{err}");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    bail!("the faucet did not give {lamports} lamports to {address} in {AIRDROP_ATTEMPTS} attempts: {last}")
}

/// Write a keypair to a file in the format `forge --owner` reads.
///
/// # Errors
///
/// If the file cannot be written.
pub fn write_keypair(path: &std::path::Path, keypair: &Keypair) -> Result<()> {
    let bytes: Vec<u8> = keypair.to_bytes().to_vec();
    let json = serde_json::to_string(&bytes).context("the keypair does not serialize")?;
    std::fs::write(path, json).with_context(|| format!("cannot write {}", path.display()))
}
