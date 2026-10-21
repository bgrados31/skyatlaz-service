//! The configuration of the service.
//!
//! Everything comes from the environment so that no secret ever ends up in a
//! file that could be committed. The values are read once at startup and never
//! again, so a bad value is a startup error and not a surprise at runtime.

use std::env;
use std::time::Duration;

use rand_core::{OsRng, RngCore};

/// The address the server listens on when nothing overrides it.
const DEFAULT_BIND: &str = "127.0.0.1:8080";
/// How many profiles are cached by default. A slim profile is a few kilobytes,
/// so this is a few megabytes of memory.
const DEFAULT_CACHE_ENTRIES: usize = 1_024;
/// The upper bound of `PROFILE_CACHE_ENTRIES`. It exists to turn a typo with
/// one zero too many into a startup error instead of an out of memory kill.
const MAX_CACHE_ENTRIES: usize = 100_000;
/// How long a profile is cached by default. One player polling the mod cannot
/// spend more than twelve upstream requests per hour with this value.
const DEFAULT_PROFILE_TTL_SECONDS: u64 = 300;
/// How long a session token is valid by default.
const DEFAULT_TOKEN_TTL_SECONDS: u64 = 3_600;
/// The shortest token secret that is accepted without a warning.
const MIN_SECRET_BYTES: usize = 32;

/// The configuration of the service.
pub struct Config {
    /// The address the HTTP server listens on.
    pub bind: String,
    /// The Hypixel developer key. It stays on the server, never in the mod.
    pub hypixel_api_key: String,
    /// The base url of the Hypixel API.
    pub hypixel_base: String,
    /// The secret that signs the session tokens.
    pub token_secret: Vec<u8>,
    /// How long a cached profile stays valid.
    pub profile_ttl: Duration,
    /// How long a session token stays valid.
    pub token_ttl: Duration,
    /// How many profiles the cache may hold at once.
    pub profile_cache_entries: usize,
}

impl Config {
    /// Reads the configuration from the environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(&|name| env::var(name).ok())
    }

    /// Reads the configuration from an arbitrary lookup, which keeps the rules
    /// testable without touching the environment of the test runner.
    fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let hypixel_api_key = required(lookup, "HYPIXEL_API_KEY")?;
        let token_secret = token_secret(lookup);

        Ok(Self {
            bind: bind(lookup)?,
            hypixel_api_key,
            hypixel_base: non_empty(lookup, "HYPIXEL_API_BASE")
                .unwrap_or_else(|| "https://api.hypixel.net".to_string()),
            token_secret,
            profile_ttl: seconds(lookup, "PROFILE_TTL_SECONDS", DEFAULT_PROFILE_TTL_SECONDS)?,
            token_ttl: seconds(lookup, "TOKEN_TTL_SECONDS", DEFAULT_TOKEN_TTL_SECONDS)?,
            profile_cache_entries: cache_entries(lookup)?,
        })
    }
}

/// Reads the token secret, or generates one for a quick local run.
fn token_secret(lookup: &dyn Fn(&str) -> Option<String>) -> Vec<u8> {
    match non_empty(lookup, "SKYATLAZ_TOKEN_SECRET") {
        Some(value) => {
            if value.len() < MIN_SECRET_BYTES {
                tracing::warn!(
                    bytes = value.len(),
                    "SKYATLAZ_TOKEN_SECRET is shorter than {MIN_SECRET_BYTES} bytes. \
                     A long random secret keeps the session tokens unguessable."
                );
            }
            value.into_bytes()
        }
        None => {
            let mut secret = vec![0u8; MIN_SECRET_BYTES];
            OsRng.fill_bytes(&mut secret);
            tracing::warn!(
                "SKYATLAZ_TOKEN_SECRET is not set, using a random secret. \
                 The session tokens will not survive a restart."
            );
            secret
        }
    }
}

/// Reads the bind address. `BIND` wins; `PORT` is the convention of every PaaS
/// and container runtime, and it implies listening on every interface because
/// that is what those platforms expect.
fn bind(lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    if let Some(value) = non_empty(lookup, "BIND") {
        return Ok(value);
    }

    match non_empty(lookup, "PORT") {
        Some(value) => match value.parse::<u16>() {
            Ok(port) if port > 0 => Ok(format!("0.0.0.0:{port}")),
            _ => Err(ConfigError::Invalid {
                name: "PORT",
                value,
                expected: "a port between 1 and 65535",
            }),
        },
        None => Ok(DEFAULT_BIND.to_string()),
    }
}

/// Reads a required variable.
fn required(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    non_empty(lookup, name).ok_or(ConfigError::Missing(name))
}

/// Reads an optional duration in seconds. Zero and garbage are refused: both
/// would quietly turn off the cache or the tokens instead of failing loudly.
fn seconds(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &'static str,
    default: u64,
) -> Result<Duration, ConfigError> {
    let Some(value) = non_empty(lookup, name) else {
        return Ok(Duration::from_secs(default));
    };

    match value.parse::<u64>() {
        Ok(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err(ConfigError::Invalid {
            name,
            value,
            expected: "a positive number of seconds",
        }),
    }
}

/// Reads how many profiles the cache may hold.
fn cache_entries(lookup: &dyn Fn(&str) -> Option<String>) -> Result<usize, ConfigError> {
    let Some(value) = non_empty(lookup, "PROFILE_CACHE_ENTRIES") else {
        return Ok(DEFAULT_CACHE_ENTRIES);
    };

    match value.parse::<usize>() {
        Ok(entries) if (1..=MAX_CACHE_ENTRIES).contains(&entries) => Ok(entries),
        _ => Err(ConfigError::Invalid {
            name: "PROFILE_CACHE_ENTRIES",
            value,
            expected: "a number of entries between 1 and 100000",
        }),
    }
}

/// Reads a variable that is set to something other than whitespace.
fn non_empty(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The reasons the service refuses to start.
#[derive(Debug)]
pub enum ConfigError {
    /// A required variable is missing or empty.
    Missing(&'static str),
    /// A variable is set to a value the service cannot work with.
    Invalid {
        /// The name of the variable.
        name: &'static str,
        /// The value that was read, so the error points at the typo.
        value: String,
        /// What the service expected.
        expected: &'static str,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(formatter, "the environment variable {name} is required"),
            Self::Invalid {
                name,
                value,
                expected,
            } => write!(
                formatter,
                "{name} is '{value}', the service expects {expected}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Reads a configuration out of a fixed set of variables.
    fn config_with(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        Config::from_lookup(&|name| variables.get(name).cloned())
    }

    fn with_key(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut pairs = vec![("HYPIXEL_API_KEY", "a-test-key")];
        pairs.extend_from_slice(extra);
        config_with(&pairs)
    }

    #[test]
    fn reports_a_missing_key() {
        assert!(matches!(
            config_with(&[]),
            Err(ConfigError::Missing("HYPIXEL_API_KEY"))
        ));
    }

    #[test]
    fn a_blank_key_counts_as_missing() {
        assert!(matches!(
            config_with(&[("HYPIXEL_API_KEY", "   ")]),
            Err(ConfigError::Missing("HYPIXEL_API_KEY"))
        ));
    }

    #[test]
    fn uses_the_documented_defaults() {
        let config = with_key(&[]).expect("the defaults are valid");

        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.hypixel_base, "https://api.hypixel.net");
        assert_eq!(config.profile_ttl, Duration::from_secs(300));
        assert_eq!(config.token_ttl, Duration::from_secs(3600));
        assert_eq!(config.profile_cache_entries, 1_024);
        assert_eq!(config.token_secret.len(), 32);
    }

    #[test]
    fn port_is_used_when_there_is_no_bind() {
        // PaaS runtimes set PORT and expect a service that listens on every
        // interface, while a plain local run stays on the loopback address.
        let config = with_key(&[("PORT", "9000")]).expect("a port is valid");

        assert_eq!(config.bind, "0.0.0.0:9000");
    }

    #[test]
    fn bind_wins_over_port() {
        let config = with_key(&[("BIND", "127.0.0.1:7000"), ("PORT", "9000")])
            .expect("the bind address is valid");

        assert_eq!(config.bind, "127.0.0.1:7000");
    }

    #[test]
    fn rejects_a_port_that_is_not_a_port() {
        assert!(matches!(
            with_key(&[("PORT", "eighty")]),
            Err(ConfigError::Invalid { name: "PORT", .. })
        ));
        assert!(matches!(
            with_key(&[("PORT", "0")]),
            Err(ConfigError::Invalid { name: "PORT", .. })
        ));
    }

    #[test]
    fn rejects_a_profile_ttl_of_zero() {
        // A zero ttl would cache every profile for no time at all, so the cache
        // would be pure overhead while the rate limit is spent as fast as the
        // mod polls.
        assert!(matches!(
            with_key(&[("PROFILE_TTL_SECONDS", "0")]),
            Err(ConfigError::Invalid {
                name: "PROFILE_TTL_SECONDS",
                ..
            })
        ));
    }

    #[test]
    fn rejects_a_token_ttl_that_is_not_a_number() {
        assert!(matches!(
            with_key(&[("TOKEN_TTL_SECONDS", "an hour")]),
            Err(ConfigError::Invalid {
                name: "TOKEN_TTL_SECONDS",
                ..
            })
        ));
    }

    #[test]
    fn rejects_a_cache_size_outside_the_bounds() {
        for value in ["0", "100001"] {
            assert!(
                matches!(
                    with_key(&[("PROFILE_CACHE_ENTRIES", value)]),
                    Err(ConfigError::Invalid {
                        name: "PROFILE_CACHE_ENTRIES",
                        ..
                    })
                ),
                "{value} should be refused"
            );
        }
    }

    #[test]
    fn keeps_the_token_secret_it_is_given() {
        let config = with_key(&[("SKYATLAZ_TOKEN_SECRET", "a-secret-that-is-long-enough")])
            .expect("the secret is valid");

        assert_eq!(config.token_secret, b"a-secret-that-is-long-enough");
    }
}
