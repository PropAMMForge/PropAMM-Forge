//! Borsh golden vectors: the single source of bytes for the TS-SDK.
//!
//! # Why
//!
//! A round-trip inside TS (encoded — decoded — matched) proves only internal
//! consistency: if the layout table in the SDK is wrong **the same way** in both
//! directions, such a test is green, while the transaction is refused on the
//! network or, worse, executes with different numbers. So the bytes here are
//! written by Rust — the same borsh the program reads them with — and TS is
//! obliged to parse them and assemble them back byte for byte.
//!
//! The vectors cover three different things, and each breaks separately:
//! **instruction data** (the Anchor discriminator + arguments — what the builders
//! assemble), the `Vault` **account data** and the **event bodies** from the log.
//!
//! # How to update
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p propamm-vault --test golden_vectors
//! ```
//!
//! Without this variable the test only compares — and must be red whenever the
//! layout changed and the fixture did not. Red here means: the layout in the SDK
//! has drifted, and it has to be fixed together with the program.
//!
//! # Sentinels
//!
//! Every number in a vector is distinct and none is zero: fields of the same width
//! swapped around would otherwise pass unnoticed. `max_size_base` deliberately
//! equals 2^53 + 1 — the smallest integer not representable in `f64`, so any
//! passage through a JS `Number` corrupts precisely it.

use std::fs;
use std::path::PathBuf;

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AnchorSerialize, Discriminator, InstructionData};

use propamm_vault::events::{
    CapitalFlow, CapitalMoved, QuoteClearReason, QuoteCleared, QuoteUpdated, Swapped,
};
use propamm_vault::instruction as ix;
use propamm_vault::instructions::authority::RiskLimits;
use propamm_vault::instructions::initialize_vault::InitializeVaultArgs;
use propamm_vault::instructions::swap::{SwapArgs, SwapSide};
use propamm_vault::instructions::treasury::TreasurySide;
use propamm_vault::instructions::update_quote::QuoteUpdate;
use propamm_vault::state::Vault;

/// The smallest integer not representable in `f64`.
const BEYOND_F64: u64 = (1u64 << 53) + 1;

fn key(byte: u8) -> Pubkey {
    Pubkey::new_from_array([byte; 32])
}

/// One fixture row.
struct Vector {
    /// Path-like name: `instruction/swap/base_to_quote`, `event/Swapped`, ...
    name: String,
    /// Codec name in the SDK: a type from `codecs.ts`, or `pubkey` / `u64` / `none`
    /// for instructions whose argument is a single scalar or nothing.
    codec: &'static str,
    /// How many leading bytes are the discriminator (8 or 0).
    skip: usize,
    bytes: Vec<u8>,
    /// Field values under **IDL names** (snake_case), all as strings: JSON has no
    /// integers wider than 53 bits, and `max_size_base` here is exactly that.
    fields: Vec<(&'static str, String)>,
}

fn borsh(value: &impl AnchorSerialize) -> Vec<u8> {
    // This borsh version has no `try_to_vec()` — serialize into a buffer.
    let mut bytes = Vec::new();
    value.serialize(&mut bytes).expect("borsh serialize");
    bytes
}

fn with_discriminator(discriminator: &[u8], payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = discriminator.to_vec();
    bytes.extend(payload);
    bytes
}

// ─── Vector values ───────────────────────────────────────────────────────────

fn initialize_args() -> InitializeVaultArgs {
    InitializeVaultArgs {
        pricing_authority: key(0x11),
        halt_authority: key(0x22),
        max_quote_age_slots: 150,
        max_skew_bps: 2_500,
    }
}

fn initialize_args_fields() -> Vec<(&'static str, String)> {
    let args = initialize_args();
    vec![
        ("pricing_authority", args.pricing_authority.to_string()),
        ("halt_authority", args.halt_authority.to_string()),
        ("max_quote_age_slots", args.max_quote_age_slots.to_string()),
        ("max_skew_bps", args.max_skew_bps.to_string()),
    ]
}

fn risk_limits() -> RiskLimits {
    RiskLimits {
        max_quote_age_slots: 77,
        max_skew_bps: 1_234,
    }
}

fn risk_limits_fields() -> Vec<(&'static str, String)> {
    let limits = risk_limits();
    vec![
        (
            "max_quote_age_slots",
            limits.max_quote_age_slots.to_string(),
        ),
        ("max_skew_bps", limits.max_skew_bps.to_string()),
    ]
}

fn quote_update() -> QuoteUpdate {
    QuoteUpdate {
        mid_e9: 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10,
        spread_bps: 4_321,
        skew_bps: -1_234,
        max_size_base: BEYOND_F64,
    }
}

fn quote_update_fields() -> Vec<(&'static str, String)> {
    let quote = quote_update();
    vec![
        ("mid_e9", quote.mid_e9.to_string()),
        ("spread_bps", quote.spread_bps.to_string()),
        ("skew_bps", quote.skew_bps.to_string()),
        ("max_size_base", quote.max_size_base.to_string()),
    ]
}

fn swap_args(side: SwapSide) -> SwapArgs {
    SwapArgs {
        side,
        amount_in: 123_456_789,
        min_amount_out: BEYOND_F64,
    }
}

fn swap_args_fields(side: SwapSide) -> Vec<(&'static str, String)> {
    let args = swap_args(side);
    vec![
        ("side", format!("{:?}", args.side)),
        ("amount_in", args.amount_in.to_string()),
        ("min_amount_out", args.min_amount_out.to_string()),
    ]
}

fn vault() -> Vault {
    Vault {
        owner: key(0x01),
        pricing_authority: key(0x02),
        halt_authority: key(0x03),
        base_mint: key(0x04),
        quote_mint: key(0x05),
        base_vault: key(0x06),
        quote_vault: key(0x07),
        mid_e9: 0x1122_3344_5566_7788_99AA_BBCC_DDEE_FF00,
        max_size_base: BEYOND_F64,
        quote_slot: 987_654_321,
        max_quote_age_slots: 150,
        spread_bps: 25,
        skew_bps: -300,
        max_skew_bps: 2_500,
        halted: true,
        bump: 254,
    }
}

fn vault_fields() -> Vec<(&'static str, String)> {
    let vault = vault();
    vec![
        ("owner", vault.owner.to_string()),
        ("pricing_authority", vault.pricing_authority.to_string()),
        ("halt_authority", vault.halt_authority.to_string()),
        ("base_mint", vault.base_mint.to_string()),
        ("quote_mint", vault.quote_mint.to_string()),
        ("base_vault", vault.base_vault.to_string()),
        ("quote_vault", vault.quote_vault.to_string()),
        ("mid_e9", vault.mid_e9.to_string()),
        ("max_size_base", vault.max_size_base.to_string()),
        ("quote_slot", vault.quote_slot.to_string()),
        ("max_quote_age_slots", vault.max_quote_age_slots.to_string()),
        ("spread_bps", vault.spread_bps.to_string()),
        ("skew_bps", vault.skew_bps.to_string()),
        ("max_skew_bps", vault.max_skew_bps.to_string()),
        ("halted", vault.halted.to_string()),
        ("bump", vault.bump.to_string()),
    ]
}

fn quote_updated() -> QuoteUpdated {
    QuoteUpdated {
        vault: key(0x0A),
        slot: 1_000_000_007,
        mid_e9: 0x0F0E_0D0C_0B0A_0908_0706_0504_0302_0100,
        spread_bps: 33,
        skew_bps: -44,
        max_size_base: BEYOND_F64,
    }
}

fn swapped() -> Swapped {
    Swapped {
        vault: key(0x0B),
        slot: 1_000_000_009,
        side: SwapSide::QuoteToBase,
        amount_in: 555_666_777,
        amount_out: 111_222_333,
        price_e9: 0xDEAD_BEEF_CAFE_BABE_0123_4567_89AB_CDEF,
        quote_slot: 999_999_999,
        base_amount_after: BEYOND_F64,
        quote_amount_after: 42,
    }
}

fn capital_moved(flow: CapitalFlow, side: TreasurySide) -> CapitalMoved {
    CapitalMoved {
        vault: key(0x0C),
        slot: 1_000_000_011,
        flow,
        side,
        amount: 7_777_777,
        treasury_amount_after: BEYOND_F64,
    }
}

// ─── Assembling the fixture ──────────────────────────────────────────────────

fn vectors() -> Vec<Vector> {
    let mut out = Vec::new();

    // — the types themselves, without a discriminator —
    out.push(Vector {
        name: "type/InitializeVaultArgs".into(),
        codec: "InitializeVaultArgs",
        skip: 0,
        bytes: borsh(&initialize_args()),
        fields: initialize_args_fields(),
    });
    out.push(Vector {
        name: "type/RiskLimits".into(),
        codec: "RiskLimits",
        skip: 0,
        bytes: borsh(&risk_limits()),
        fields: risk_limits_fields(),
    });
    out.push(Vector {
        name: "type/QuoteUpdate".into(),
        codec: "QuoteUpdate",
        skip: 0,
        bytes: borsh(&quote_update()),
        fields: quote_update_fields(),
    });
    for side in [SwapSide::BaseToQuote, SwapSide::QuoteToBase] {
        out.push(Vector {
            name: format!("type/SwapArgs/{side:?}"),
            codec: "SwapArgs",
            skip: 0,
            bytes: borsh(&swap_args(side)),
            fields: swap_args_fields(side),
        });
    }

    // — instruction data: exactly what the SDK builder must assemble —
    out.push(Vector {
        name: "instruction/initialize_vault".into(),
        codec: "InitializeVaultArgs",
        skip: 8,
        bytes: ix::InitializeVault {
            args: initialize_args(),
        }
        .data(),
        fields: initialize_args_fields(),
    });
    out.push(Vector {
        name: "instruction/set_pricing_authority".into(),
        codec: "pubkey",
        skip: 8,
        bytes: ix::SetPricingAuthority {
            new_authority: key(0x33),
        }
        .data(),
        fields: vec![("new_authority", key(0x33).to_string())],
    });
    out.push(Vector {
        name: "instruction/set_halt_authority".into(),
        codec: "pubkey",
        skip: 8,
        bytes: ix::SetHaltAuthority {
            new_authority: key(0x44),
        }
        .data(),
        fields: vec![("new_authority", key(0x44).to_string())],
    });
    out.push(Vector {
        name: "instruction/set_risk_limits".into(),
        codec: "RiskLimits",
        skip: 8,
        bytes: ix::SetRiskLimits {
            limits: risk_limits(),
        }
        .data(),
        fields: risk_limits_fields(),
    });
    out.push(Vector {
        name: "instruction/update_quote".into(),
        codec: "QuoteUpdate",
        skip: 8,
        bytes: ix::UpdateQuote {
            quote: quote_update(),
        }
        .data(),
        fields: quote_update_fields(),
    });
    out.push(Vector {
        name: "instruction/clear_quote".into(),
        codec: "none",
        skip: 8,
        bytes: ix::ClearQuote {}.data(),
        fields: Vec::new(),
    });
    out.push(Vector {
        name: "instruction/swap".into(),
        codec: "SwapArgs",
        skip: 8,
        bytes: ix::Swap {
            args: swap_args(SwapSide::QuoteToBase),
        }
        .data(),
        fields: swap_args_fields(SwapSide::QuoteToBase),
    });
    out.push(Vector {
        name: "instruction/deposit".into(),
        codec: "u64",
        skip: 8,
        bytes: ix::Deposit { amount: BEYOND_F64 }.data(),
        fields: vec![("amount", BEYOND_F64.to_string())],
    });
    out.push(Vector {
        name: "instruction/withdraw".into(),
        codec: "u64",
        skip: 8,
        bytes: ix::Withdraw { amount: 1_000_001 }.data(),
        fields: vec![("amount", 1_000_001u64.to_string())],
    });

    // — account —
    out.push(Vector {
        name: "account/Vault".into(),
        codec: "Vault",
        skip: 8,
        bytes: with_discriminator(Vault::DISCRIMINATOR, borsh(&vault())),
        fields: vault_fields(),
    });

    // — events: as they sit in `Program data:` —
    let updated = quote_updated();
    out.push(Vector {
        name: "event/QuoteUpdated".into(),
        codec: "QuoteUpdated",
        skip: 8,
        bytes: with_discriminator(QuoteUpdated::DISCRIMINATOR, borsh(&updated)),
        fields: vec![
            ("vault", updated.vault.to_string()),
            ("slot", updated.slot.to_string()),
            ("mid_e9", updated.mid_e9.to_string()),
            ("spread_bps", updated.spread_bps.to_string()),
            ("skew_bps", updated.skew_bps.to_string()),
            ("max_size_base", updated.max_size_base.to_string()),
        ],
    });

    // All three reasons — the variant number on the wire matters, and a mixed-up
    // reason in the history does not look like an error.
    for reason in [
        QuoteClearReason::Explicit,
        QuoteClearReason::CapitalWithdrawn,
        QuoteClearReason::PricingAuthorityChanged,
    ] {
        let cleared = QuoteCleared {
            vault: key(0x0D),
            slot: 1_000_000_013,
            reason,
        };
        out.push(Vector {
            name: format!("event/QuoteCleared/{reason:?}"),
            codec: "QuoteCleared",
            skip: 8,
            bytes: with_discriminator(QuoteCleared::DISCRIMINATOR, borsh(&cleared)),
            fields: vec![
                ("vault", cleared.vault.to_string()),
                ("slot", cleared.slot.to_string()),
                ("reason", format!("{reason:?}")),
            ],
        });
    }

    let swap = swapped();
    out.push(Vector {
        name: "event/Swapped".into(),
        codec: "Swapped",
        skip: 8,
        bytes: with_discriminator(Swapped::DISCRIMINATOR, borsh(&swap)),
        fields: vec![
            ("vault", swap.vault.to_string()),
            ("slot", swap.slot.to_string()),
            ("side", format!("{:?}", swap.side)),
            ("amount_in", swap.amount_in.to_string()),
            ("amount_out", swap.amount_out.to_string()),
            ("price_e9", swap.price_e9.to_string()),
            ("quote_slot", swap.quote_slot.to_string()),
            ("base_amount_after", swap.base_amount_after.to_string()),
            ("quote_amount_after", swap.quote_amount_after.to_string()),
        ],
    });

    for (flow, side) in [
        (CapitalFlow::Deposit, TreasurySide::Base),
        (CapitalFlow::Withdraw, TreasurySide::Quote),
    ] {
        let moved = capital_moved(flow, side);
        out.push(Vector {
            name: format!("event/CapitalMoved/{flow:?}/{side:?}"),
            codec: "CapitalMoved",
            skip: 8,
            bytes: with_discriminator(CapitalMoved::DISCRIMINATOR, borsh(&moved)),
            fields: vec![
                ("vault", moved.vault.to_string()),
                ("slot", moved.slot.to_string()),
                ("flow", format!("{flow:?}")),
                ("side", format!("{side:?}")),
                ("amount", moved.amount.to_string()),
                (
                    "treasury_amount_after",
                    moved.treasury_amount_after.to_string(),
                ),
            ],
        });
    }

    out
}

/// Address vectors: PDA derivation is part of the protocol too.
///
/// A mistake here does not give wrong bytes, it gives **the address of an account that
/// does not exist**, and on the network it looks like "account not found" — a message
/// from which nobody would guess the SDK has the seeds reordered. So the addresses are
/// computed by the same `find_program_address` as the program, and the SDK must match.
///
/// `vault_mints_swapped` is here on purpose: the pair (base, quote) and the pair
/// (quote, base) are two different vaults with opposite meanings of `mid_e9`, and
/// the SDK has no right to normalize the mint order "so it is the same".
fn render_addresses() -> String {
    let owner = key(0x01);
    let base_mint = key(0x04);
    let quote_mint = key(0x05);
    let program = propamm_vault::ID;

    let seeds = |o: &Pubkey, b: &Pubkey, q: &Pubkey| {
        Pubkey::find_program_address(&[b"vault", o.as_ref(), b.as_ref(), q.as_ref()], &program)
    };
    let (vault, bump) = seeds(&owner, &base_mint, &quote_mint);
    let (swapped_vault, _) = seeds(&owner, &quote_mint, &base_mint);

    let ata = |owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey| {
        Pubkey::find_program_address(
            &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
            &anchor_spl::associated_token::ID,
        )
        .0
    };
    let base_treasury = ata(&vault, &anchor_spl::token::ID, &base_mint);
    let quote_treasury = ata(&vault, &anchor_spl::token_2022::ID, &quote_mint);

    format!(
        "  \"addresses\": {{\n    \
           \"program\": \"{program}\",\n    \
           \"owner\": \"{owner}\",\n    \
           \"base_mint\": \"{base_mint}\",\n    \
           \"quote_mint\": \"{quote_mint}\",\n    \
           \"token_program\": \"{token}\",\n    \
           \"token_2022_program\": \"{token22}\",\n    \
           \"vault\": \"{vault}\",\n    \
           \"vault_bump\": {bump},\n    \
           \"vault_mints_swapped\": \"{swapped_vault}\",\n    \
           \"base_treasury\": \"{base_treasury}\",\n    \
           \"quote_treasury\": \"{quote_treasury}\"\n  \
         }},\n",
        token = anchor_spl::token::ID,
        token22 = anchor_spl::token_2022::ID,
    )
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The JSON is written by hand, without `serde_json`: every value here is base58,
/// a decimal number or an ASCII name, i.e. there is nothing to escape. An extra
/// dev-dependency in a crate that goes to the network would cost more than twenty lines.
fn render(vectors: &[Vector]) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(
        "  \"generator\": \"programs/propamm-vault/tests/golden_vectors.rs \
         (UPDATE_GOLDEN=1 cargo test -p propamm-vault --test golden_vectors)\",\n",
    );
    out.push_str(&render_addresses());
    out.push_str("  \"vectors\": [\n");
    for (index, vector) in vectors.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"name\": \"{}\",\n", vector.name));
        out.push_str(&format!("      \"codec\": \"{}\",\n", vector.codec));
        out.push_str(&format!("      \"skip\": {},\n", vector.skip));
        out.push_str(&format!("      \"hex\": \"{}\",\n", hex(&vector.bytes)));
        if vector.fields.is_empty() {
            out.push_str("      \"fields\": {}\n");
        } else {
            out.push_str("      \"fields\": {\n");
            for (position, (name, value)) in vector.fields.iter().enumerate() {
                let comma = if position + 1 == vector.fields.len() {
                    ""
                } else {
                    ","
                };
                out.push_str(&format!("        \"{name}\": \"{value}\"{comma}\n"));
            }
            out.push_str("      }\n");
        }
        let comma = if index + 1 == vectors.len() { "" } else { "," };
        out.push_str(&format!("    }}{comma}\n"));
    }
    out.push_str("  ]\n");
    out.push_str("}\n");
    out
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/sdk/tests/fixtures/borsh-golden.json")
}

#[test]
fn golden_vectors_match_the_committed_fixture() {
    let rendered = render(&vectors());
    let path = fixture_path();

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the fixtures directory");
        }
        // Explicitly as bytes: `write` does not rewrite line endings, and the fixture
        // stays LF even when it is updated from Windows.
        fs::write(&path, rendered.as_bytes()).expect("write the fixture");
        return;
    }

    let committed = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}\ncreate it: UPDATE_GOLDEN=1 cargo test -p propamm-vault --test golden_vectors",
            path.display()
        )
    });

    assert_eq!(
        committed, rendered,
        "the Borsh layout changed but the fixture did not — the TS-SDK now encodes different bytes \
         than the program reads; update: UPDATE_GOLDEN=1 cargo test -p propamm-vault --test golden_vectors"
    );
}

/// Sentinels must stay sentinels: a zero or a repeat makes the vector blind to
/// fields of the same width swapped around.
#[test]
fn sentinels_are_distinct_and_nonzero() {
    let vault = vault();
    let numbers: Vec<u128> = vec![
        vault.mid_e9,
        u128::from(vault.max_size_base),
        u128::from(vault.quote_slot),
        u128::from(vault.max_quote_age_slots),
        u128::from(vault.spread_bps),
        u128::from(vault.max_skew_bps),
        u128::from(vault.bump),
    ];
    assert!(
        numbers.iter().all(|value| *value != 0),
        "a zero among the sentinels"
    );
    for (index, value) in numbers.iter().enumerate() {
        assert!(
            !numbers[index + 1..].contains(value),
            "sentinel {value} repeats"
        );
    }
    assert_eq!(
        BEYOND_F64 as f64 as u64,
        BEYOND_F64 - 1,
        "the sentinel must break when passed through f64 — otherwise it does not catch a JS Number"
    );
}
