//! The configuration of the service. Everything comes from the environment so
//! that no secret ever ends up in a file that could be committed.

use std::env;
use std::time::Duration;

use rand_core::{OsRng, RngCore};

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
}

impl Config {
    /// Reads the configuration from the environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        let hypixel_api_key = required("HYPIXEL_API_KEY")?;
        let token_secret = match env::var("SKYATLAZ_TOKEN_SECRET") {
            Ok(value) if !value.trim().is_empty() => value.into_bytes(),
            _ => {
                let mut secret = vec![0u8; 32];
                OsRng.fill_bytes(&mut secret);
                tracing::warn!(
                    "SKYATLAZ_TOKEN_SECRET is not set, using a random secret. \
                     The session tokens will not survive a restart."
                );
                secret
            }
        };

        Ok(Self {
            bind: env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
            hypixel_api_key,
            hypixel_base: env::var("HYPIXEL_API_BASE")
                .unwrap_or_else(|_| "https://api.hypixel.net".to_string()),
            token_secret,
            profile_ttl: seconds("PROFILE_TTL_SECONDS", 300),
            token_ttl: seconds("TOKEN_TTL_SECONDS", 3600),
        })
    }
}

/// Reads a required variable.
fn required(name: &'static str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ConfigError::Missing(name)),
    }
}

/// Reads an optional duration in seconds.
fn seconds(name: &'static str, default: u64) -> Duration {
    match env::var(name) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(_) => {
                tracing::warn!("{name} is not a number, using {default} seconds");
                Duration::from_secs(default)
            }
        },
        Err(_) => Duration::from_secs(default),
    }
}

/// The reasons the service refuses to start.
#[derive(Debug)]
pub enum ConfigError {
    /// A required variable is missing or empty.
    Missing(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(formatter, "the environment variable {name} is required"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_missing_key() {
        // The environment of the test runner does not contain the key.
        if env::var("HYPIXEL_API_KEY").is_ok() {
            return;
        }
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::Missing("HYPIXEL_API_KEY"))
        ));
    }
}
