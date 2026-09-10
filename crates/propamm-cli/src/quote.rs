//! `forge quote` — post or clear a quote (FR-006, FR-007, FR-014).
//!
//! # The price is entered in human form and `mid_e9` is computed here
//!
//! T023 decision. The program does not read the mints' decimals — that would cost
//! two extra accounts in the SC-002 budget — so the conversion of "150.25 SOL/USDC"
//! into raw units is done by the off-chain side. The chain has no guard beneath
//! this conversion: a three-orders-of-magnitude mistake posts a price at which the
//! vault hands out its inventory entirely legitimately. So the command **always
//! prints** the resulting `mid_e9` together with bid and ask, and `--mid-e9` stays
//! the direct path for those who compute it themselves (and for the US2 engine, which will do exactly that).
//!
//! # The domain is checked before the transaction — by the same code
//!
//! `update_quote` refuses parameters on which [`propamm_quote::side_price_e9`]
//! fails. The same function is called here, so an infeasible price costs a
//! message, not a transaction. This is not a copy of the rules: one rule, one crate.
//!
//! # `pricing_authority` signs, not the owner
//!
//! That is the whole point of separating the authorities (FR-010). While the right
//! to set the price is with the same key as the capital, the command takes the key
//! from the config; once it is handed to the engine, `--keypair` is needed, and the
//! CLI checks it against what is recorded in the vault account, not against `propamm.toml`.

use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anyhow::{bail, Context, Result};
use propamm_quote::{side_price_e9, QuoteParams};
use solana_keypair::Keypair;
use solana_signer::Signer as _;

use crate::amount::Decimal;
use crate::chain::{clear_quote_ix, read_keypair, update_quote_ix, MintInfo, Session};
use crate::rpc::Confirmed;

/// How the market mid is given.
#[derive(Clone, Copy, Debug)]
pub enum Mid {
    /// A human price: how much quote per one base.
    Human(Decimal),
    /// A ready `mid_e9` — no conversion.
    Raw(u128),
}

/// How the maximum order size is given.
#[derive(Clone, Copy, Debug)]
pub enum Size {
    Human(Decimal),
    Raw(u64),
}

/// Quote parameters before conversion.
#[derive(Clone, Copy, Debug)]
pub struct SetQuote {
    pub mid: Mid,
    pub spread_bps: u16,
    pub skew_bps: i16,
    pub size: Size,
}

/// What we do with the quote.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Set(SetQuote),
    Clear,
}

/// Command arguments after parsing.
#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    pub vault: Option<String>,
    pub action: Action,
    /// The `pricing_authority` keypair, if it is no longer the owner.
    pub keypair: Option<PathBuf>,
}

/// The computed price in both forms.
#[derive(Clone, Copy, Debug)]
pub struct QuoteSummary {
    pub base: MintInfo,
    pub quote: MintInfo,
    pub mid_e9: u128,
    /// Whether the human price converted exactly. `false` means a slightly different
    /// price went on chain than was typed — and a human has to see that.
    pub exact: bool,
    pub spread_bps: u16,
    pub skew_bps: i16,
    pub max_size_base: u64,
    pub bid_e9: u128,
    pub ask_e9: u128,
}

/// What happened.
#[derive(Clone, Debug)]
pub struct Quoted {
    pub name: String,
    pub vault: Pubkey,
    pub authority: Pubkey,
    /// `None` for `--clear`.
    pub summary: Option<QuoteSummary>,
    pub confirmed: Confirmed,
}

/// Run `forge quote`.
///
/// # Errors
///
/// If the vault is not deployed or is halted, the signer key is missing or wrong,
/// the price does not convert or falls outside the domain, or the transaction
/// failed.
pub fn run(options: &Options) -> Result<Quoted> {
    let session = Session::open(&options.path)?;
    let entry = session.config.select(options.vault.as_deref())?.clone();

    let (vault_address, _bump) = session.vault_pda(&entry);
    let vault = session.read_vault(&vault_address)?.with_context(|| {
        format!(
            "vault \"{}\" is not deployed yet ({vault_address}) — first: forge deploy --vault {}",
            entry.name, entry.name
        )
    })?;

    // A halted vault does not quote (FR-024): otherwise the halt would clear the
    // price and the engine's next tick would put it back.
    if vault.halted {
        bail!(
            "vault \"{}\" is halted — it will not accept a quote",
            entry.name
        );
    }

    let signer = pricing_signer(&session, &vault, options.keypair.as_deref())?;
    let authority = signer.pubkey();

    let (instruction, summary) = match options.action {
        Action::Clear => (
            clear_quote_ix(&session.program_id(), &authority, &vault_address),
            None,
        ),
        Action::Set(set) => {
            let (base, quote) = session.read_pair(&entry)?;
            let summary = summarize(&set, &base, &quote, &vault)?;
            (
                update_quote_ix(
                    &session.program_id(),
                    &authority,
                    &vault_address,
                    propamm_vault::instructions::update_quote::QuoteUpdate {
                        mid_e9: summary.mid_e9,
                        spread_bps: summary.spread_bps,
                        skew_bps: summary.skew_bps,
                        max_size_base: summary.max_size_base,
                    },
                ),
                Some(summary),
            )
        }
    };

    // The quote signer also pays: in US2 that is the engine's hot key with its own
    // small balance, and it must not pay from the capital key.
    let confirmed = session.send(&[instruction], &[&signer])?;

    Ok(Quoted {
        name: entry.name,
        vault: vault_address,
        authority,
        summary,
        confirmed,
    })
}

/// The key the quote is signed with.
///
/// Checked against `pricing_authority` **from the account**, not from the config:
/// the right may have been handed to the engine via `set_pricing_authority`, and
/// `propamm.toml` knows nothing about it. A mismatch here is a transaction the
/// program would refuse with `PricingAuthorityOnly`, and it is better to say so before it.
fn pricing_signer(
    session: &Session,
    vault: &propamm_vault::state::Vault,
    keypair: Option<&std::path::Path>,
) -> Result<Keypair> {
    if let Some(path) = keypair {
        let signer = read_keypair(path)?;
        if signer.pubkey() != vault.pricing_authority {
            bail!(
                "key {} signs as {}, while quotes for this vault are posted by {}",
                path.display(),
                signer.pubkey(),
                vault.pricing_authority
            );
        }
        return Ok(signer);
    }
    if vault.pricing_authority != session.owner() {
        bail!(
            "quotes for this vault are posted by {}, not by the owner {} — name the key: forge quote --keypair <path>",
            vault.pricing_authority,
            session.owner()
        );
    }
    // The owner key has already been read by the session; its bytes are copied
    // because `Keypair` is deliberately not `Clone`.
    read_keypair(std::path::Path::new(&session.config.owner.keypair))
}

/// Convert the input into what goes on chain and check the domain.
fn summarize(
    set: &SetQuote,
    base: &MintInfo,
    quote: &MintInfo,
    vault: &propamm_vault::state::Vault,
) -> Result<QuoteSummary> {
    let (mid_e9, exact) = match set.mid {
        Mid::Raw(value) => (value, true),
        Mid::Human(price) => price.to_mid_e9(base.decimals, quote.decimals)?,
    };
    let max_size_base = match set.size {
        Size::Raw(value) => value,
        Size::Human(size) => size.to_raw(base.decimals)?,
    };
    if mid_e9 == 0 {
        bail!("the market mid is zero — that is not a price but \"quote cleared\"; to clear: forge quote --clear");
    }
    if max_size_base == 0 {
        bail!("zero order size — nothing can be swapped at such a quote");
    }

    // The same parameters the program assembles: it takes `quote_slot` from the
    // chain, but that does not affect the side price, and the bounds come from state
    // so the check matches the on-chain one down to the last field.
    let params = QuoteParams {
        mid_e9,
        spread_bps: set.spread_bps,
        skew_bps: set.skew_bps,
        max_size_base,
        quote_slot: vault.quote_slot,
        max_quote_age_slots: vault.max_quote_age_slots,
        max_skew_bps: vault.max_skew_bps,
    };
    let bid_e9 = side_price_e9(&params, propamm_quote::Side::BaseToQuote)
        .map_err(|err| anyhow::anyhow!("bid does not compute with these parameters: {err}"))?;
    let ask_e9 = side_price_e9(&params, propamm_quote::Side::QuoteToBase)
        .map_err(|err| anyhow::anyhow!("ask does not compute with these parameters: {err}"))?;

    Ok(QuoteSummary {
        base: *base,
        quote: *quote,
        mid_e9,
        exact,
        spread_bps: set.spread_bps,
        skew_bps: set.skew_bps,
        max_size_base,
        bid_e9,
        ask_e9,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use propamm_vault::state::Vault;

    fn key(tag: u8) -> Pubkey {
        Pubkey::new_from_array([tag; 32])
    }

    fn mint(decimals: u8) -> MintInfo {
        MintInfo {
            address: key(decimals),
            decimals,
            token_program: anchor_spl::token::ID,
        }
    }

    fn vault() -> Vault {
        Vault {
            owner: key(1),
            pricing_authority: key(1),
            halt_authority: key(1),
            base_mint: key(9),
            quote_mint: key(6),
            base_vault: key(11),
            quote_vault: key(12),
            mid_e9: 0,
            max_size_base: 0,
            quote_slot: 0,
            max_quote_age_slots: 25,
            spread_bps: 0,
            skew_bps: 0,
            max_skew_bps: 3_000,
            halted: false,
            bump: 254,
        }
    }

    fn set(mid: &str, spread_bps: u16, skew_bps: i16, size: &str) -> SetQuote {
        SetQuote {
            mid: Mid::Human(mid.parse().unwrap()),
            spread_bps,
            skew_bps,
            size: Size::Human(size.parse().unwrap()),
        }
    }

    /// The canonical case: SOL/USDC, a ±20 bps spread.
    #[test]
    fn a_human_price_becomes_mid_e9_and_two_sides() {
        let summary = summarize(&set("150.25", 20, 0, "10"), &mint(9), &mint(6), &vault()).unwrap();
        assert_eq!(summary.mid_e9, 150_250_000);
        assert!(summary.exact);
        assert_eq!(summary.max_size_base, 10_000_000_000);
        assert!(
            summary.bid_e9 < summary.mid_e9 && summary.mid_e9 < summary.ask_e9,
            "bid {} mid {} ask {}",
            summary.bid_e9,
            summary.mid_e9,
            summary.ask_e9
        );
    }

    /// Zero spread — both sides coincide with the mid; the rounding asymmetry
    /// has nothing to shift here.
    #[test]
    fn a_zero_spread_leaves_both_sides_at_the_middle() {
        let summary = summarize(&set("150.25", 0, 0, "10"), &mint(9), &mint(6), &vault()).unwrap();
        assert_eq!(summary.bid_e9, summary.mid_e9);
        assert_eq!(summary.ask_e9, summary.mid_e9);
    }

    /// A positive skew raises both sides — that is how the vault accumulates the base asset.
    #[test]
    fn skew_moves_both_sides_together() {
        let flat = summarize(&set("150.25", 20, 0, "10"), &mint(9), &mint(6), &vault()).unwrap();
        let skewed =
            summarize(&set("150.25", 20, 100, "10"), &mint(9), &mint(6), &vault()).unwrap();
        assert!(skewed.bid_e9 > flat.bid_e9);
        assert!(skewed.ask_e9 > flat.ask_e9);
    }

    /// The main reason the check stands before the transaction: a spread of 100 %
    /// or more drives the bid to zero, and the program would refuse such a quote.
    #[test]
    fn a_spread_outside_the_domain_is_refused_before_the_transaction() {
        let err = summarize(
            &set("150.25", 10_000, 0, "10"),
            &mint(9),
            &mint(6),
            &vault(),
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("bid") || text.contains("ask"), "{text}");
    }

    /// `--mid-e9` bypasses the conversion and is always "exact": the human computed it.
    #[test]
    fn a_raw_mid_goes_through_untouched() {
        let summary = summarize(
            &SetQuote {
                mid: Mid::Raw(150_250_000),
                spread_bps: 20,
                skew_bps: 0,
                size: Size::Raw(10_000_000_000),
            },
            &mint(9),
            &mint(6),
            &vault(),
        )
        .unwrap();
        assert_eq!(summary.mid_e9, 150_250_000);
        assert_eq!(summary.max_size_base, 10_000_000_000);
        assert!(summary.exact);
    }

    /// A zero `mid_e9` is "no price", and posting a price must not silently turn
    /// into it: the program has a separate instruction for that.
    #[test]
    fn a_zero_raw_mid_points_at_the_clear_command() {
        let err = summarize(
            &SetQuote {
                mid: Mid::Raw(0),
                spread_bps: 0,
                skew_bps: 0,
                size: Size::Raw(1),
            },
            &mint(9),
            &mint(6),
            &vault(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("--clear"), "{err}");
    }

    #[test]
    fn a_zero_size_is_refused() {
        let err = summarize(
            &SetQuote {
                mid: Mid::Raw(1),
                spread_bps: 0,
                skew_bps: 0,
                size: Size::Raw(0),
            },
            &mint(9),
            &mint(6),
            &vault(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("size"), "{err}");
    }

    /// An inexact conversion is not an error, but it has to be noticed: not quite
    /// the price that was typed goes on chain.
    #[test]
    fn an_inexact_conversion_is_flagged_rather_than_refused() {
        let summary = summarize(
            &set("150.2500001", 20, 0, "10"),
            &mint(9),
            &mint(6),
            &vault(),
        )
        .unwrap();
        assert!(!summary.exact);
    }
}
