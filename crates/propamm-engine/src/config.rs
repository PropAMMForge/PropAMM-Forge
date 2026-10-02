//! The engine's configuration, from the environment (`.env.example` lists it).
//!
//! # Where the values come from
//!
//! The process environment, and nothing else here. The binary can fill it
//! from a file first (`--env-file`), but only when told to: a `.env` picked up
//! from the working directory by default would hand a test run the operator's
//! real feed key and node.
//!
//! # Required and defaulted
//!
//! Four keys say *which* market this engine quotes and with what right — the
//! node, the vault, the pricing key, the feed — and have no sensible default.
//! The rest are tunables; a missing one takes the value `.env.example` ships
//! with, and the binary logs the values it runs with. An empty value is the
//! same as a missing one (`KEY=` in a `.env`), and `REPLACE_ME` — the
//! placeholder `.env.example` ships with — is refused rather than sent to a
//! node or a feed as if it were an address or a key.
//!
//! # What is not here
//!
//! The quote freshness limit. The rule, the model budget and the silence bound
//! are checked against the vault's `max_quote_age_slots` as read from chain
//! ([`Cycle::start`](crate::cycle::Cycle::start)); a second copy of that number
//! in the environment could only disagree with it.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use thiserror::Error;

use crate::feed::{Policy, PriceId, HERMES_DEFAULT_URL};
use crate::model::external::ModelCommand;
use crate::model::spread_skew::{ModelConfigError, SpreadSkewModel};

/// The value `.env.example` ships with in place of a real one.
pub const PLACEHOLDER: &str = "REPLACE_ME";

// The defaults are the values `.env.example` ships with.
const DEFAULT_FEED_MAX_AGE_SECONDS: u64 = 5;
const DEFAULT_FEED_MAX_CONF_BPS: u32 = 30;
const DEFAULT_FEED_MAX_SILENCE_MS: u64 = 2_000;
const DEFAULT_DEVIATION_THRESHOLD_BPS: u16 = 5;
const DEFAULT_HEARTBEAT_SLOTS: u32 = 12;
const DEFAULT_MODEL_TIMEOUT_MS: u64 = 50;
const DEFAULT_BASE_SPREAD_BPS: u16 = 10;
const DEFAULT_MAX_SPREAD_BPS: u16 = 40;
const DEFAULT_MAX_SKEW_SHIFT_BPS: u16 = 150;
const DEFAULT_SIZE_FRACTION_BPS: u16 = 500;

/// A configuration the engine will not start with.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("{0} is not set")]
    Missing(&'static str),
    #[error("{0} is still the {PLACEHOLDER} placeholder from .env.example")]
    Placeholder(&'static str),
    #[error("{key}={value:?}: {reason}")]
    Invalid {
        key: &'static str,
        value: String,
        reason: String,
    },
    #[error("the built-in model: {0}")]
    Model(#[from] ModelConfigError),
}

/// A value that must not reach a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Which pricing model the engine runs (FR-015a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChoice {
    /// `MODEL_COMMAND` is empty: the built-in spread-and-skew model, from `MODEL_*`.
    Builtin(SpreadSkewModel),
    /// `MODEL_COMMAND` is set: a model in another process.
    External(ModelCommand),
}

/// Everything the binary needs to start the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `SOLANA_RPC_URL`.
    pub rpc_url: String,
    /// `VAULT_ADDRESS` — the vault this engine quotes for.
    pub vault: Pubkey,
    /// `PRICING_AUTHORITY_KEYPAIR` — the hot key, by path.
    pub pricing_keypair: PathBuf,
    /// `PYTH_HERMES_URL`.
    pub hermes_url: String,
    /// `PYTH_API_KEY`; Hermes refuses a stream without one since 2026-08-26.
    pub api_key: Option<Secret>,
    /// `PYTH_PRICE_FEED_ID`.
    pub base_feed: PriceId,
    /// `PYTH_QUOTE_PRICE_FEED_ID`; `None` takes the quote asset at par with USD.
    pub quote_feed: Option<PriceId>,
    /// `FEED_MAX_AGE_SECONDS` and `FEED_MAX_CONF_BPS` (FR-012).
    pub feed_policy: Policy,
    /// `FEED_MAX_SILENCE_MS` (FR-014).
    pub silence: Duration,
    /// `QUOTE_DEVIATION_THRESHOLD_BPS` (FR-011).
    pub threshold_bps: u16,
    /// `QUOTE_HEARTBEAT_SLOTS` (FR-011).
    pub heartbeat_slots: u32,
    /// `RISK_MODEL_TIMEOUT_MS` (FR-015b).
    pub model_budget: Duration,
    /// `MODEL_COMMAND`, or the built-in model from `MODEL_*`.
    pub model: ModelChoice,
}

impl Config {
    /// Read the process environment.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] — the first key that is missing, a placeholder, or not a value of its kind.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Read the configuration through `lookup` — the environment, or a map in a test.
    ///
    /// # Errors
    ///
    /// As [`Config::from_env`].
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let env = Env(&lookup);
        // Fields are read in the order written, so the first bad key is the one reported.
        Ok(Self {
            rpc_url: env.required("SOLANA_RPC_URL")?,
            vault: env.parsed("VAULT_ADDRESS")?,
            pricing_keypair: PathBuf::from(env.required("PRICING_AUTHORITY_KEYPAIR")?),
            hermes_url: env
                .optional("PYTH_HERMES_URL")?
                .unwrap_or_else(|| HERMES_DEFAULT_URL.to_owned()),
            api_key: env.optional("PYTH_API_KEY")?.map(Secret),
            base_feed: env.parsed("PYTH_PRICE_FEED_ID")?,
            quote_feed: env
                .optional("PYTH_QUOTE_PRICE_FEED_ID")?
                .map(|text| parse("PYTH_QUOTE_PRICE_FEED_ID", &text))
                .transpose()?,
            feed_policy: Policy::new(
                Duration::from_secs(env.or("FEED_MAX_AGE_SECONDS", DEFAULT_FEED_MAX_AGE_SECONDS)?),
                env.or("FEED_MAX_CONF_BPS", DEFAULT_FEED_MAX_CONF_BPS)?,
            ),
            silence: Duration::from_millis(
                env.or("FEED_MAX_SILENCE_MS", DEFAULT_FEED_MAX_SILENCE_MS)?,
            ),
            threshold_bps: env.or(
                "QUOTE_DEVIATION_THRESHOLD_BPS",
                DEFAULT_DEVIATION_THRESHOLD_BPS,
            )?,
            heartbeat_slots: env.or("QUOTE_HEARTBEAT_SLOTS", DEFAULT_HEARTBEAT_SLOTS)?,
            model_budget: Duration::from_millis(
                env.or("RISK_MODEL_TIMEOUT_MS", DEFAULT_MODEL_TIMEOUT_MS)?,
            ),
            model: match env.optional("MODEL_COMMAND")? {
                Some(line) => ModelChoice::External(command(&line)),
                None => ModelChoice::Builtin(SpreadSkewModel::checked(
                    env.or("MODEL_BASE_SPREAD_BPS", DEFAULT_BASE_SPREAD_BPS)?,
                    env.or("MODEL_MAX_SPREAD_BPS", DEFAULT_MAX_SPREAD_BPS)?,
                    env.or("MODEL_MAX_SKEW_SHIFT_BPS", DEFAULT_MAX_SKEW_SHIFT_BPS)?,
                    env.or("MODEL_SIZE_FRACTION_BPS", DEFAULT_SIZE_FRACTION_BPS)?,
                )?),
            },
        })
    }
}

/// `MODEL_COMMAND` split on whitespace: the program, then its arguments.
///
/// No shell quoting — a path with a space in it goes through a wrapper script.
/// The command is run directly, not through a shell, so nothing in it is expanded.
fn command(line: &str) -> ModelCommand {
    let mut words = line.split_whitespace();
    // `optional` never returns a blank value, so there is a first word.
    let program = words.next().unwrap_or_default();
    ModelCommand::new(program).args(words)
}

struct Env<'a, F>(&'a F);

impl<F: Fn(&str) -> Option<String>> Env<'_, F> {
    /// The value, trimmed; blank is absent, the placeholder is an error.
    fn optional(&self, key: &'static str) -> Result<Option<String>, ConfigError> {
        match (self.0)(key) {
            None => Ok(None),
            Some(value) => {
                let value = value.trim();
                if value.is_empty() {
                    Ok(None)
                } else if value == PLACEHOLDER {
                    Err(ConfigError::Placeholder(key))
                } else {
                    Ok(Some(value.to_owned()))
                }
            }
        }
    }

    fn required(&self, key: &'static str) -> Result<String, ConfigError> {
        self.optional(key)?.ok_or(ConfigError::Missing(key))
    }

    fn parsed<T: FromStr>(&self, key: &'static str) -> Result<T, ConfigError>
    where
        T::Err: fmt::Display,
    {
        parse(key, &self.required(key)?)
    }

    fn or<T: FromStr>(&self, key: &'static str, default: T) -> Result<T, ConfigError>
    where
        T::Err: fmt::Display,
    {
        self.optional(key)?
            .map_or(Ok(default), |text| parse(key, &text))
    }
}

fn parse<T: FromStr>(key: &'static str, text: &str) -> Result<T, ConfigError>
where
    T::Err: fmt::Display,
{
    text.parse().map_err(|error: T::Err| ConfigError::Invalid {
        key,
        value: text.to_owned(),
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const SOL_USD: &str = "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
    const VAULT: &str = "77Y9n3vWE2noN1u9PTshuWxdDRsrw9UMtejBypUD9wjq";

    fn required() -> HashMap<&'static str, String> {
        HashMap::from([
            ("SOLANA_RPC_URL", "http://127.0.0.1:8899".to_owned()),
            ("VAULT_ADDRESS", VAULT.to_owned()),
            ("PRICING_AUTHORITY_KEYPAIR", "/keys/pricing.json".to_owned()),
            ("PYTH_PRICE_FEED_ID", SOL_USD.to_owned()),
        ])
    }

    fn read(env: &HashMap<&'static str, String>) -> Result<Config, ConfigError> {
        Config::from_lookup(|key| env.get(key).cloned())
    }

    #[test]
    fn the_four_required_keys_are_enough() {
        let config = read(&required()).unwrap();
        assert_eq!(config.vault.to_string(), VAULT);
        assert_eq!(config.base_feed, SOL_USD.parse().unwrap());
        assert_eq!(config.hermes_url, HERMES_DEFAULT_URL);
        assert_eq!(config.api_key, None);
        assert_eq!(config.quote_feed, None);
        assert_eq!(config.threshold_bps, 5);
        assert_eq!(config.heartbeat_slots, 12);
        assert_eq!(config.silence, Duration::from_secs(2));
        assert_eq!(config.model_budget, Duration::from_millis(50));
        assert_eq!(config.feed_policy, Policy::new(Duration::from_secs(5), 30));
        assert_eq!(
            config.model,
            ModelChoice::Builtin(SpreadSkewModel::checked(10, 40, 150, 500).unwrap())
        );
    }

    /// Every default is the value `.env.example` ships with: a key left out
    /// of a `.env` behaves as if it had been copied over unchanged.
    #[test]
    fn the_defaults_are_what_env_example_ships() {
        let example =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env.example"))
                .unwrap();
        let shipped: HashMap<&str, String> = example
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim().to_owned()))
            .collect();
        let defaults = read(&required()).unwrap();
        let mut env = required();
        for key in [
            "FEED_MAX_AGE_SECONDS",
            "FEED_MAX_CONF_BPS",
            "FEED_MAX_SILENCE_MS",
            "QUOTE_DEVIATION_THRESHOLD_BPS",
            "QUOTE_HEARTBEAT_SLOTS",
            "RISK_MODEL_TIMEOUT_MS",
            "MODEL_BASE_SPREAD_BPS",
            "MODEL_MAX_SPREAD_BPS",
            "MODEL_MAX_SKEW_SHIFT_BPS",
            "MODEL_SIZE_FRACTION_BPS",
            "PYTH_HERMES_URL",
        ] {
            let value = shipped
                .get(key)
                .unwrap_or_else(|| panic!("{key} is not in .env.example"));
            env.insert(key, value.clone());
        }
        assert_eq!(read(&env).unwrap(), defaults);
    }

    #[test]
    fn a_missing_required_key_is_named() {
        for key in [
            "SOLANA_RPC_URL",
            "VAULT_ADDRESS",
            "PRICING_AUTHORITY_KEYPAIR",
            "PYTH_PRICE_FEED_ID",
        ] {
            let mut env = required();
            env.remove(key);
            assert_eq!(read(&env), Err(ConfigError::Missing(key)));
            // `KEY=` in a .env is the same as no key at all.
            env.insert(key, "  ".to_owned());
            assert_eq!(read(&env), Err(ConfigError::Missing(key)));
        }
    }

    #[test]
    fn the_placeholder_is_refused_even_where_a_value_is_optional() {
        let mut env = required();
        env.insert("PYTH_API_KEY", PLACEHOLDER.to_owned());
        assert_eq!(read(&env), Err(ConfigError::Placeholder("PYTH_API_KEY")));

        let mut env = required();
        env.insert("VAULT_ADDRESS", PLACEHOLDER.to_owned());
        assert_eq!(read(&env), Err(ConfigError::Placeholder("VAULT_ADDRESS")));
    }

    #[test]
    fn a_value_of_the_wrong_kind_is_refused_with_the_key() {
        let mut env = required();
        env.insert("QUOTE_HEARTBEAT_SLOTS", "twelve".to_owned());
        assert!(matches!(
            read(&env),
            Err(ConfigError::Invalid {
                key: "QUOTE_HEARTBEAT_SLOTS",
                ..
            })
        ));

        let mut env = required();
        env.insert("VAULT_ADDRESS", "not-base58".to_owned());
        assert!(matches!(
            read(&env),
            Err(ConfigError::Invalid {
                key: "VAULT_ADDRESS",
                ..
            })
        ));

        let mut env = required();
        env.insert("PYTH_QUOTE_PRICE_FEED_ID", "0x12".to_owned());
        assert!(matches!(
            read(&env),
            Err(ConfigError::Invalid {
                key: "PYTH_QUOTE_PRICE_FEED_ID",
                ..
            })
        ));
    }

    #[test]
    fn a_built_in_model_that_cannot_steer_is_refused() {
        let mut env = required();
        env.insert("MODEL_MAX_SKEW_SHIFT_BPS", "30".to_owned());
        assert!(matches!(
            read(&env),
            Err(ConfigError::Model(
                ModelConfigError::ShiftDoesNotBeatWidening { .. }
            ))
        ));
    }

    /// The built-in model's levers are not read at all once another model is
    /// named: a firm swapping its own model in should not have to keep ours valid.
    #[test]
    fn a_model_command_replaces_the_built_in_model() {
        let mut env = required();
        env.insert(
            "MODEL_COMMAND",
            "python3  examples/models/fixed_spread.py --spread-bps 25".to_owned(),
        );
        env.insert("MODEL_MAX_SKEW_SHIFT_BPS", "30".to_owned());
        let config = read(&env).unwrap();
        assert_eq!(
            config.model,
            ModelChoice::External(ModelCommand::new("python3").args([
                "examples/models/fixed_spread.py",
                "--spread-bps",
                "25"
            ]))
        );
    }

    #[test]
    fn the_feed_key_and_the_cross_leg_are_taken_when_set() {
        let mut env = required();
        env.insert("PYTH_API_KEY", " key-123 ".to_owned());
        env.insert("PYTH_QUOTE_PRICE_FEED_ID", format!("0x{SOL_USD}"));
        let config = read(&env).unwrap();
        assert_eq!(config.api_key.as_ref().map(Secret::expose), Some("key-123"));
        assert_eq!(config.quote_feed, Some(SOL_USD.parse().unwrap()));
    }

    #[test]
    fn the_feed_key_does_not_reach_a_debug_print() {
        let mut env = required();
        env.insert("PYTH_API_KEY", "key-123".to_owned());
        let printed = format!("{:?}", read(&env).unwrap());
        assert!(!printed.contains("key-123"), "{printed}");
    }
}
