//! `forge init` on a real file system (T022, FR-001).
//!
//! The call is tested, not a spawned process with stdout parsing: a test on
//! output catches a change of wording and misses a change of behaviour.

use std::path::{Path, PathBuf};

use anchor_lang::prelude::Pubkey;
use propamm_cli::config::{
    Cluster, ProjectConfig, CONFIG_FILE, DEFAULT_MAX_QUOTE_AGE_SLOTS, DEFAULT_MAX_SKEW_BPS,
    SCHEMA_VERSION,
};
use propamm_cli::init::{self, Options};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// The owner key in a file — the way `solana-keygen` writes it.
///
/// The format is a JSON array of 64 bytes. `{:?}` over a slice gives exactly
/// that, so no second dependency appears here for one line.
fn write_keypair(dir: &Path) -> (PathBuf, Pubkey) {
    let keypair = Keypair::new();
    let path = dir.join("owner.json");
    std::fs::write(&path, format!("{:?}", &keypair.to_bytes()[..])).expect("key not written");
    (path, Pubkey::new_from_array(keypair.pubkey().to_bytes()))
}

fn mint(tag: u8) -> Pubkey {
    Pubkey::new_from_array([tag; 32])
}

fn options(dir: &Path, keypair: PathBuf) -> Options {
    Options {
        path: dir.join("my-amm"),
        name: None,
        base_mint: mint(1),
        quote_mint: mint(2),
        owner_keypair: Some(keypair),
        cluster: Cluster::Devnet,
        vault_name: None,
        max_quote_age_slots: DEFAULT_MAX_QUOTE_AGE_SLOTS,
        max_skew_bps: DEFAULT_MAX_SKEW_BPS,
        force: false,
    }
}

/// The main claim of T022: in an empty directory a project appears that the
/// CLI can read back.
///
/// The round-trip here is not a formality. A config assembled from strings and
/// a config the command can parse are two different things exactly until
/// someone adds a field to one of them.
#[test]
fn an_empty_directory_becomes_a_project_the_cli_can_read_back() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, owner) = write_keypair(temp.path());

    let created = init::run(&options(temp.path(), keypair)).expect("init failed");

    for name in [CONFIG_FILE, "README.md", ".gitignore"] {
        assert!(
            created.dir.join(name).is_file(),
            "{name} was not created: {:?}",
            created.files
        );
    }

    let read_back = ProjectConfig::read_from(&created.dir).expect("the config does not read back");
    assert_eq!(read_back, created.config, "written and read diverged");

    assert_eq!(read_back.version, SCHEMA_VERSION);
    assert_eq!(read_back.project.name, "my-amm");
    assert_eq!(read_back.network.cluster, Cluster::Devnet);
    assert_eq!(read_back.network.rpc_url, Cluster::Devnet.default_rpc_url());
    assert_eq!(read_back.network.program_id.0, propamm_vault::ID);
    assert_eq!(read_back.owner.pubkey.0, owner);

    // One vault, and in it the same pair that was asked for. The authorities start
    // with the owner: they can be handed over separately, but not taken back silently.
    assert_eq!(read_back.vaults.len(), 1);
    let vault = &read_back.vaults[0];
    assert_eq!(vault.base_mint.0, mint(1));
    assert_eq!(vault.quote_mint.0, mint(2));
    assert_eq!(vault.pricing_authority.0, owner);
    assert_eq!(vault.halt_authority.0, owner);
    assert_eq!(vault.max_quote_age_slots, DEFAULT_MAX_QUOTE_AGE_SLOTS);
    assert_eq!(vault.max_skew_bps, DEFAULT_MAX_SKEW_BPS);

    // The selector for the T023 commands finds the vault by name.
    assert_eq!(
        read_back.vault(&vault.name).expect("vault not found"),
        vault
    );
}

/// The README has to be a document, not a template with holes.
///
/// An unreplaced `{{owner}}` does not break the file — it makes it plausible
/// and wrong; it would be noticed when someone sent funds to the wrong place.
#[test]
fn the_readme_carries_real_values_and_no_placeholders() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, owner) = write_keypair(temp.path());

    let created = init::run(&options(temp.path(), keypair)).expect("init failed");
    let readme =
        std::fs::read_to_string(created.dir.join("README.md")).expect("cannot read the README");

    assert!(!readme.contains("{{"), "a template placeholder remains");
    for expected in [
        owner.to_string(),
        mint(1).to_string(),
        mint(2).to_string(),
        propamm_vault::ID.to_string(),
        created.config.vaults[0].name.clone(),
        "forge deploy".to_string(),
    ] {
        assert!(readme.contains(&expected), "README lacks \"{expected}\"");
    }
}

/// Keys must not travel into git together with the project.
#[test]
fn the_generated_gitignore_covers_keys() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, _) = write_keypair(temp.path());

    let created = init::run(&options(temp.path(), keypair)).expect("init failed");
    let ignore =
        std::fs::read_to_string(created.dir.join(".gitignore")).expect("cannot read .gitignore");

    for pattern in ["*-keypair.json", ".env"] {
        assert!(ignore.contains(pattern), ".gitignore lacks \"{pattern}\"");
    }
}

/// A repeated `init` must not silently wipe a config with already deployed vaults.
#[test]
fn an_existing_project_is_not_overwritten_by_accident() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, _) = write_keypair(temp.path());
    let first = options(temp.path(), keypair.clone());
    init::run(&first).expect("the first init failed");

    let err = init::run(&first).unwrap_err();
    assert!(
        format!("{err}").contains("--force"),
        "the message does not name the way out: {err}"
    );

    // With `--force` it overwrites, and to the new values at that.
    let mut second = options(temp.path(), keypair);
    second.force = true;
    second.vault_name = Some("second-attempt".to_string());
    let created = init::run(&second).expect("init --force failed");
    assert_eq!(created.config.vaults[0].name, "second-attempt");

    let read_back = ProjectConfig::read_from(&created.dir).expect("cannot read the config");
    assert_eq!(read_back.vaults[0].name, "second-attempt");
}

/// Without the owner key the project makes no sense: its address goes into the vault seeds.
#[test]
fn a_missing_owner_key_says_how_to_get_one() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let mut options = options(temp.path(), temp.path().join("missing.json"));
    options.owner_keypair = Some(temp.path().join("missing.json"));

    let err = init::run(&options).unwrap_err();
    let message = format!("{err}");
    assert!(message.contains("solana-keygen"), "{message}");
    assert!(
        !options.path.exists(),
        "the project directory was created despite the missing key"
    );
}

/// The config is edited by hand, and a typo in a field name is a risk limit
/// the owner believes is set while the program does not see it.
#[test]
fn a_misspelled_field_in_the_config_is_refused_not_ignored() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, _) = write_keypair(temp.path());
    let created = init::run(&options(temp.path(), keypair)).expect("init failed");

    let path = created.dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path).expect("cannot read the config");
    let broken = text.replace("max_skew_bps", "max_skew_bp");
    assert_ne!(broken, text, "the config has no field to break");
    std::fs::write(&path, broken).expect("cannot write the config");

    let err = ProjectConfig::read_from(&created.dir).unwrap_err();
    assert!(
        format!("{err:#}").contains("max_skew_bp"),
        "the parser did not name the field: {err:#}"
    );
}

/// A selector with a typo must not look like "the vault is not deployed".
#[test]
fn an_unknown_vault_selector_lists_the_known_ones() {
    let temp = tempfile::tempdir().expect("no temporary directory");
    let (keypair, _) = write_keypair(temp.path());
    let created = init::run(&options(temp.path(), keypair)).expect("init failed");

    let err = created.config.vault("no-such-vault").unwrap_err();
    let message = format!("{err}");
    assert!(
        message.contains(&created.config.vaults[0].name),
        "{message}"
    );
}
