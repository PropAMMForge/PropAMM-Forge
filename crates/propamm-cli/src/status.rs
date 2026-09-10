//! `forge status` — the state of all the project's vaults together (FR-004a, FR-023).
//!
//! # Three requests regardless of the number of vaults
//!
//! The slot, then a batch of all the PDAs, then a batch of the mints and
//! treasuries of those that are deployed. A loop of "one account per request"
//! would spend the free RPC quota exactly where it counts (a risk from PLAN),
//! and `status` is the command run more often than any other.
//!
//! # Quote age is counted in slots, not by the clock
//!
//! `max_quote_age_slots` is slots, and comparing a difference in seconds against
//! it would mean inventing time nobody measured. The slot comes from the network
//! in the same request as the accounts, so "fresh" here means exactly what it
//! will mean for the FR-007 guard in the next swap.
//!
//! # Separation of computation and printing
//!
//! [`collect`] goes to the network and returns a [`Report`]; [`render`] knows
//! nothing about the network or the config. Because of this the output format —
//! what a human reads at the moment something went wrong — is tested by ordinary tests.

use std::fmt::Write as _;
use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use propamm_quote::{inventory_skew_bps, is_fresh, side_price_e9, Inventory, QuoteParams, Side};

use crate::amount::{format_mid_e9, format_raw};
use crate::chain::{decode_token_account, MintInfo, Session};
use crate::config::Cluster;

/// Command arguments after parsing.
#[derive(Clone, Debug)]
pub struct Options {
    pub path: PathBuf,
    /// Without a selector all the project's vaults are shown.
    pub vault: Option<String>,
}

/// The state of the whole project.
#[derive(Clone, Debug)]
pub struct Report {
    pub project: String,
    pub cluster: Cluster,
    pub rpc_url: String,
    pub program_id: Pubkey,
    pub program_deployed: bool,
    pub owner: Pubkey,
    pub slot: u64,
    pub vaults: Vec<VaultStatus>,
}

/// One vault.
#[derive(Clone, Debug)]
pub struct VaultStatus {
    pub name: String,
    pub address: Pubkey,
    pub live: Option<Live>,
}

/// A deployed vault.
#[derive(Clone, Debug)]
pub struct Live {
    pub halted: bool,
    pub base: MintInfo,
    pub quote: MintInfo,
    pub base_amount: u64,
    pub quote_amount: u64,
    pub pricing_authority: Pubkey,
    pub halt_authority: Pubkey,
    pub max_quote_age_slots: u32,
    pub max_skew_bps: u16,
    /// `None` until a quote is posted.
    pub quote_params: Option<QuoteParams>,
    /// Inventory skew at the current mid; without a quote there is none, because
    /// there is nothing to value the base leg with.
    pub skew_bps: Option<i32>,
}

/// Run `forge status`.
///
/// # Errors
///
/// If there is no project or the node does not respond.
pub fn run(options: &Options) -> Result<Report> {
    let session = Session::open(&options.path)?;
    collect(&session, options.vault.as_deref())
}

/// Collect the report.
///
/// # Errors
///
/// If the selector is not found or the node does not respond.
pub fn collect(session: &Session, selector: Option<&str>) -> Result<Report> {
    let entries = match selector {
        Some(name) => vec![session.config.select(Some(name))?.clone()],
        None => session.config.vaults.clone(),
    };

    let slot = session.rpc.get_slot()?;
    let program = session.rpc.get_account(&session.program_id())?;

    let addresses: Vec<Pubkey> = entries
        .iter()
        .map(|entry| session.vault_pda(entry).0)
        .collect();
    let accounts = session.rpc.get_multiple_accounts(&addresses)?;

    // The second batch: the mints and treasuries of the vaults that really exist.
    // The order here is a contract with the parsing below, so the four addresses
    // per vault are laid out and read in one and the same order.
    let mut states = Vec::new();
    let mut details = Vec::new();
    for (address, account) in addresses.iter().zip(accounts) {
        let Some(account) = account else {
            states.push(None);
            continue;
        };
        let state = crate::chain::decode_vault(address, &account)?;
        details.extend_from_slice(&[
            state.base_mint,
            state.quote_mint,
            state.base_vault,
            state.quote_vault,
        ]);
        states.push(Some(state));
    }
    let detail_accounts = session.rpc.get_multiple_accounts(&details)?;

    let mut vaults = Vec::with_capacity(entries.len());
    let mut cursor = 0usize;
    for ((entry, address), state) in entries.iter().zip(&addresses).zip(states) {
        let Some(state) = state else {
            vaults.push(VaultStatus {
                name: entry.name.clone(),
                address: *address,
                live: None,
            });
            continue;
        };
        let window = &detail_accounts[cursor..cursor + 4];
        cursor += 4;

        let base = read_mint(&state.base_mint, window[0].as_ref())?;
        let quote = read_mint(&state.quote_mint, window[1].as_ref())?;
        let base_amount = read_amount(&state.base_vault, window[2].as_ref())?;
        let quote_amount = read_amount(&state.quote_vault, window[3].as_ref())?;

        // A zero in `mid_e9` means "no quote posted", not a price of zero — the same
        // definition as in the program and in the math.
        let quote_params = (state.mid_e9 != 0).then_some(QuoteParams {
            mid_e9: state.mid_e9,
            spread_bps: state.spread_bps,
            skew_bps: state.skew_bps,
            max_size_base: state.max_size_base,
            quote_slot: state.quote_slot,
            max_quote_age_slots: state.max_quote_age_slots,
            max_skew_bps: state.max_skew_bps,
        });
        let skew_bps = quote_params.and_then(|params| {
            inventory_skew_bps(
                &Inventory {
                    base_amount,
                    quote_amount,
                },
                params.mid_e9,
            )
            .ok()
        });

        vaults.push(VaultStatus {
            name: entry.name.clone(),
            address: *address,
            live: Some(Live {
                halted: state.halted,
                base,
                quote,
                base_amount,
                quote_amount,
                pricing_authority: state.pricing_authority,
                halt_authority: state.halt_authority,
                max_quote_age_slots: state.max_quote_age_slots,
                max_skew_bps: state.max_skew_bps,
                quote_params,
                skew_bps,
            }),
        });
    }

    Ok(Report {
        project: session.config.project.name.clone(),
        cluster: session.config.network.cluster,
        rpc_url: session.rpc.url().to_string(),
        program_id: session.program_id(),
        program_deployed: program.is_some_and(|account| account.executable),
        owner: session.owner(),
        slot,
        vaults,
    })
}

fn read_mint(address: &Pubkey, account: Option<&crate::rpc::Account>) -> Result<MintInfo> {
    let account = account.ok_or_else(|| anyhow::anyhow!("mint {address} vanished"))?;
    crate::chain::decode_mint(address, account)
}

fn read_amount(address: &Pubkey, account: Option<&crate::rpc::Account>) -> Result<u64> {
    let account =
        account.ok_or_else(|| anyhow::anyhow!("treasury {address} missing — account deleted?"))?;
    Ok(decode_token_account(address, account)?.amount)
}

/// The report in the form a human reads it.
#[must_use]
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "project {} — {} ({}), slot {}",
        report.project, report.cluster, report.rpc_url, report.slot
    );
    let _ = writeln!(
        out,
        "  program  {}{}",
        report.program_id,
        if report.program_deployed {
            ""
        } else {
            "  ⚠ not on this network"
        }
    );
    let _ = writeln!(out, "  owner    {}", report.owner);

    if report.vaults.is_empty() {
        let _ = writeln!(out, "\nthe config declares no vault");
        return out;
    }

    for vault in &report.vaults {
        let _ = writeln!(out);
        let Some(live) = &vault.live else {
            let _ = writeln!(out, "▸ {}  {}", vault.name, vault.address);
            let _ = writeln!(
                out,
                "    not deployed — forge deploy --vault {}",
                vault.name
            );
            continue;
        };
        let _ = writeln!(
            out,
            "▸ {}  {}{}",
            vault.name,
            vault.address,
            if live.halted {
                "  ⛔ HALTED — swaps refused"
            } else {
                ""
            }
        );
        let _ = writeln!(
            out,
            "    inventory {} base / {} quote",
            format_raw(live.base_amount, live.base.decimals),
            format_raw(live.quote_amount, live.quote.decimals)
        );

        match &live.quote_params {
            None => {
                let _ = writeln!(
                    out,
                    "    no quote — swaps are impossible; post one: forge quote --vault {} --mid <price> --spread-bps <n> --size <size>",
                    vault.name
                );
            }
            Some(params) => {
                let age = report.slot.saturating_sub(params.quote_slot);
                let fresh = is_fresh(params, report.slot);
                let human =
                    |value: u128| format_mid_e9(value, live.base.decimals, live.quote.decimals);
                let bid = side_price_e9(params, Side::BaseToQuote).map(human);
                let ask = side_price_e9(params, Side::QuoteToBase).map(human);
                let _ = writeln!(
                    out,
                    "    price     mid {}  bid {}  ask {}  (±{} bps, skew {} bps)",
                    human(params.mid_e9),
                    bid.as_deref().unwrap_or("—"),
                    ask.as_deref().unwrap_or("—"),
                    params.spread_bps,
                    params.skew_bps
                );
                let _ = writeln!(
                    out,
                    "    freshness {} slots of {} — {}",
                    age,
                    params.max_quote_age_slots,
                    if fresh {
                        "fresh"
                    } else {
                        "STALE — swaps refused"
                    }
                );
                let _ = writeln!(
                    out,
                    "    size      up to {} base per order",
                    format_raw(params.max_size_base, live.base.decimals)
                );
            }
        }

        if let Some(skew) = live.skew_bps {
            let over = skew.unsigned_abs() > u32::from(live.max_skew_bps);
            let _ = writeln!(
                out,
                "    skew      {skew} bps of the ±{} bound{}",
                live.max_skew_bps,
                if over { "  ⚠ past the bound" } else { "" }
            );
        }

        // The authorities are printed only when split: a line "the same as the owner"
        // would repeat a known fact on every run.
        if live.pricing_authority != report.owner {
            let _ = writeln!(out, "    quotes posted by  {}", live.pricing_authority);
        }
        if live.halt_authority != report.owner {
            let _ = writeln!(out, "    halted by         {}", live.halt_authority);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn report(vaults: Vec<VaultStatus>) -> Report {
        Report {
            project: "demo".into(),
            cluster: Cluster::Localnet,
            rpc_url: "http://127.0.0.1:8899".into(),
            program_id: key(100),
            program_deployed: true,
            owner: key(1),
            slot: 1_300,
            vaults,
        }
    }

    fn live(quote_params: Option<QuoteParams>) -> Live {
        Live {
            halted: false,
            base: mint(9),
            quote: mint(6),
            base_amount: 10_000_000_000,
            quote_amount: 5_000_000_000,
            pricing_authority: key(1),
            halt_authority: key(1),
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
            quote_params,
            skew_bps: quote_params.map(|_| 500),
        }
    }

    fn params(quote_slot: u64) -> QuoteParams {
        QuoteParams {
            mid_e9: 150_250_000,
            spread_bps: 20,
            skew_bps: 0,
            max_size_base: 10_000_000_000,
            quote_slot,
            max_quote_age_slots: 25,
            max_skew_bps: 3_000,
        }
    }

    #[test]
    fn a_vault_that_is_not_deployed_says_what_to_run() {
        let text = render(&report(vec![VaultStatus {
            name: "So11-EPjF".into(),
            address: key(2),
            live: None,
        }]));
        assert!(text.contains("not deployed"), "{text}");
        assert!(text.contains("forge deploy --vault So11-EPjF"), "{text}");
    }

    /// The price is printed in human form — the same one typed into `forge quote`.
    #[test]
    fn a_quoted_vault_prints_the_human_price_and_both_sides() {
        let text = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(live(Some(params(1_290)))),
        }]));
        assert!(text.contains("mid 150.25"), "{text}");
        // ±20 bps from 150.25: bid down, ask up — both in the vault's favour.
        assert!(text.contains("bid 149.9495"), "{text}");
        assert!(text.contains("ask 150.5505"), "{text}");
        assert!(text.contains("inventory 10 base / 5000 quote"), "{text}");
    }

    /// A stale quote has to be visible at first glance: it is what tells "the AMM
    /// works" from "the AMM shows a price nothing will happen at".
    #[test]
    fn a_stale_quote_is_named_stale() {
        let fresh = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(live(Some(params(1_290)))),
        }]));
        assert!(fresh.contains("fresh"), "{fresh}");
        assert!(!fresh.contains("STALE"), "{fresh}");

        let stale = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(live(Some(params(1_000)))),
        }]));
        assert!(stale.contains("STALE"), "{stale}");
        assert!(stale.contains("300 slots of 25"), "{stale}");
    }

    #[test]
    fn a_vault_without_a_quote_points_at_the_quote_command() {
        let text = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(live(None)),
        }]));
        assert!(text.contains("no quote"), "{text}");
        assert!(text.contains("forge quote --vault pair"), "{text}");
    }

    #[test]
    fn a_halted_vault_says_so_on_its_first_line() {
        let mut state = live(Some(params(1_290)));
        state.halted = true;
        let text = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(state),
        }]));
        assert!(text.contains("HALTED"), "{text}");
    }

    /// Split authorities are an event, and staying silent about it is not an option;
    /// unsplit ones are noise, and repeating them on every run is not worth it.
    #[test]
    fn authorities_are_printed_only_when_they_differ_from_the_owner() {
        let quiet = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(live(Some(params(1_290)))),
        }]));
        assert!(!quiet.contains("quotes posted by"), "{quiet}");

        let mut state = live(Some(params(1_290)));
        state.pricing_authority = key(77);
        let split = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(state),
        }]));
        assert!(split.contains("quotes posted by"), "{split}");
    }

    #[test]
    fn a_skew_past_the_bound_is_marked() {
        let mut state = live(Some(params(1_290)));
        state.skew_bps = Some(-3_500);
        let text = render(&report(vec![VaultStatus {
            name: "pair".into(),
            address: key(2),
            live: Some(state),
        }]));
        assert!(text.contains("past the bound"), "{text}");
    }

    /// All the project's vaults are shown together (FR-004a).
    #[test]
    fn every_vault_of_the_project_appears() {
        let text = render(&report(vec![
            VaultStatus {
                name: "first".into(),
                address: key(2),
                live: Some(live(Some(params(1_290)))),
            },
            VaultStatus {
                name: "second".into(),
                address: key(3),
                live: None,
            },
        ]));
        assert!(text.contains("▸ first"), "{text}");
        assert!(text.contains("▸ second"), "{text}");
    }

    #[test]
    fn a_missing_program_is_flagged_at_the_top() {
        let mut value = report(Vec::new());
        value.program_deployed = false;
        let text = render(&value);
        assert!(text.contains("not on this network"), "{text}");
    }
}
