//! The Hypixel API client and the slimmed down profile it produces.
//!
//! The raw profile is a couple of megabytes (collection, inventory, pets, ...).
//! Only the parts the mod actually needs are kept, which keeps the cache small
//! and the traffic low.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::now_millis;
use crate::ratelimit::RateLimits;

/// The profile of a player, reduced to what the mod needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlimProfile {
    /// The uuid of the player.
    pub uuid: String,
    /// The name of the player, when the API reports it.
    #[serde(default)]
    pub name: Option<String>,
    /// The state of the mining tree.
    pub mining: MiningProfile,
    /// The dungeon statistics, passed through untouched.
    #[serde(default)]
    pub dungeons: Value,
    /// The currencies of the player, passed through untouched.
    #[serde(default)]
    pub currencies: Value,
    /// The moment the profile was fetched, in milliseconds since the epoch.
    pub fetched_at: i64,
}

/// The mining tree of a player.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MiningProfile {
    /// The level of every perk of the tree, keyed by the perk id.
    #[serde(default)]
    pub perks: BTreeMap<String, u32>,
    /// Whether every perk is currently toggled on, keyed by the perk id.
    #[serde(default)]
    pub toggles: BTreeMap<String, bool>,
    /// The pickaxe ability that is currently selected.
    #[serde(default)]
    pub ability: Option<String>,
    /// The mined powder amounts, keyed by the powder id.
    #[serde(default)]
    pub powder: BTreeMap<String, u32>,
}

/// The errors that can happen while talking to the Hypixel API.
#[derive(Debug)]
pub enum HypixelError {
    /// The request itself failed.
    Request(reqwest::Error),
    /// The API answered with an error.
    Api { status: u16, cause: String },
    /// The answer did not contain the expected fields.
    Missing(&'static str),
    /// The key has used up its rate limit.
    RateLimited(std::time::Duration),
}

impl std::fmt::Display for HypixelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) => write!(formatter, "request failed: {error}"),
            Self::Api { status, cause } => write!(formatter, "api error {status}: {cause}"),
            Self::Missing(field) => write!(formatter, "the answer is missing '{field}'"),
            Self::RateLimited(wait) => {
                write!(formatter, "the rate limit is used up, retry in {wait:?}")
            }
        }
    }
}

impl std::error::Error for HypixelError {}

/// A client for the Hypixel public API.
pub struct HypixelClient {
    http: reqwest::Client,
    api_key: String,
    base: String,
    limits: RateLimits,
}

impl HypixelClient {
    /// Creates the client. The API key never leaves the service.
    pub fn new(api_key: &str, base: &str) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .user_agent("skyatlaz-service/0.1")
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api_key: api_key.to_string(),
            base: base.trim_end_matches('/').to_string(),
            limits: RateLimits::default(),
        })
    }

    /// How many requests the key may do in the window, and how many are left.
    pub fn rate_limit(&self) -> (u32, u32) {
        self.limits.state()
    }

    /// Fetches the profile of a player and reduces it to the slim version.
    pub async fn fetch_profile(&self, uuid: &str) -> Result<SlimProfile, HypixelError> {
        if let Some(wait) = self.limits.exhausted() {
            return Err(HypixelError::RateLimited(wait));
        }
        let url = format!(
            "{}/v2/skyblock/profiles?uuid={}",
            self.base,
            uuid.replace('-', "")
        );
        let response = self
            .http
            .get(&url)
            .header("API-Key", &self.api_key)
            .send()
            .await
            .map_err(HypixelError::Request)?;

        let status = response.status();
        let headers = response.headers().clone();
        self.limits.update(
            header_number(&headers, "ratelimit-limit").map(|value| value as u32),
            header_number(&headers, "ratelimit-remaining").map(|value| value as u32),
            header_number(&headers, "ratelimit-reset"),
        );

        let body: Value = response.json().await.map_err(HypixelError::Request)?;

        let cause = body
            .get("cause")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        if !status.is_success() || body.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(HypixelError::Api {
                status: status.as_u16(),
                cause,
            });
        }

        parse_profile(uuid, &body)
    }
}

/// Reads a number out of a header of the answer.
fn header_number(
    headers: &reqwest::header::HeaderMap,
    name: &str,
) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Reads the slim profile out of a raw API answer.
pub fn parse_profile(uuid: &str, root: &Value) -> Result<SlimProfile, HypixelError> {
    let profiles = root
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or(HypixelError::Missing("profiles"))?;
    // The API returns every island of the player and flags the one that is
    // being played with `selected`, which is not necessarily the first entry.
    let profile = profiles
        .iter()
        .find(|profile| profile.get("selected").and_then(Value::as_bool) == Some(true))
        .or_else(|| profiles.first())
        .ok_or(HypixelError::Missing("profile"))?;
    let members = profile
        .get("members")
        .and_then(Value::as_object)
        .ok_or(HypixelError::Missing("members"))?;
    let member = members
        .get(uuid)
        .or_else(|| members.get(&uuid.replace('-', "")))
        .ok_or(HypixelError::Missing("member"))?;

    Ok(SlimProfile {
        uuid: uuid.to_string(),
        name: member
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        mining: parse_mining(member),
        dungeons: member.get("dungeons").cloned().unwrap_or(Value::Null),
        currencies: member.get("currencies").cloned().unwrap_or(Value::Null),
        fetched_at: now_millis(),
    })
}

/// Reads the mining tree out of a profile member.
pub fn parse_mining(member: &Value) -> MiningProfile {
    let mut mining = MiningProfile::default();

    if let Some(nodes) = member
        .get("skill_tree")
        .and_then(|tree| tree.get("nodes"))
        .and_then(Value::as_object)
    {
        for (tier, value) in nodes {
            if !tier.starts_with("mining_") {
                continue;
            }
            let Some(entries) = value.as_object() else {
                continue;
            };
            for (perk, level) in entries {
                if let Some(name) = perk.strip_prefix("toggle_") {
                    mining
                        .toggles
                        .insert(name.to_string(), level.as_bool().unwrap_or(false));
                } else if let Some(number) = level.as_u64() {
                    mining.perks.insert(perk.clone(), number as u32);
                }
            }
        }
    }

    if let Some(core) = member.get("mining_core").and_then(Value::as_object) {
        for (name, amount) in core {
            if let Some(powder) = name.strip_prefix("powder_") {
                if let Some(number) = amount.as_u64() {
                    mining.powder.insert(powder.to_string(), number as u32);
                }
            }
        }
    }

    if let Some(abilities) = member
        .get("skill_tree")
        .and_then(|tree| tree.get("selected_ability"))
        .and_then(Value::as_object)
    {
        for (_, ability) in abilities {
            if let Some(name) = ability.as_str() {
                if !name.is_empty() && mining.ability.is_none() {
                    mining.ability = Some(name.to_string());
                }
            }
        }
    }

    mining
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "success": true,
            "profiles": [{
                "profileId": "abc",
                "selected": true,
                "members": {
                    "0123456789abcdef0123456789abcdef": {
                        "skill_tree": {
                            "nodes": {
                                "mining_1": {
                                    "compact_mining": 3,
                                    "toggle_compact_mining": true
                                },
                                "mining_2": { "mining_fortune": 50 },
                                "farming_1": { "harvesting": 5 }
                            },
                            "selected_ability": {
                                "mining_1": "pickobulus",
                                "mining_2": null
                            }
                        },
                        "mining_core": {
                            "powder_mithril": 1234,
                            "powder_gemstone": 56,
                            "other": 7
                        },
                        "dungeons": { "catacombs": { "completed": 3 } },
                        "currencies": { "coin": 42 }
                    }
                }
            }]
        })
    }

    #[test]
    fn reads_the_mining_tree() {
        let profile = parse_profile("01234567-89ab-cdef-0123-456789abcdef", &sample()).unwrap();
        assert_eq!(profile.mining.perks.get("compact_mining"), Some(&3));
        assert_eq!(profile.mining.perks.get("mining_fortune"), Some(&50));
        assert_eq!(profile.mining.perks.get("harvesting"), None);
        assert_eq!(profile.mining.toggles.get("compact_mining"), Some(&true));
        assert_eq!(profile.mining.powder.get("mithril"), Some(&1234));
        assert_eq!(profile.mining.powder.get("other"), None);
        assert_eq!(profile.mining.ability.as_deref(), Some("pickobulus"));
        assert_eq!(profile.uuid, "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn passes_the_other_sections_through() {
        let profile = parse_profile("01234567-89ab-cdef-0123-456789abcdef", &sample()).unwrap();
        assert_eq!(profile.dungeons["catacombs"]["completed"], 3);
        assert_eq!(profile.currencies["coin"], 42);
    }

    #[test]
    fn reports_a_missing_profile() {
        let empty = json!({ "success": true, "profiles": [] });
        assert!(parse_profile("01234567-89ab-cdef-0123-456789abcdef", &empty).is_err());
    }

    #[test]
    fn picks_the_profile_the_player_is_on() {
        // The API orders the islands arbitrarily, so the one the player is
        // actually on is not necessarily the first one.
        let two_islands = json!({
            "success": true,
            "profiles": [
                {
                    "profileId": "first",
                    "selected": false,
                    "members": {
                        "0123456789abcdef0123456789abcdef": { "currencies": { "coin": 1 } }
                    }
                },
                {
                    "profileId": "second",
                    "selected": true,
                    "members": {
                        "0123456789abcdef0123456789abcdef": { "currencies": { "coin": 2 } }
                    }
                }
            ]
        });
        let profile =
            parse_profile("0123456789abcdef0123456789abcdef", &two_islands).unwrap();
        assert_eq!(profile.currencies["coin"], 2);
    }

    #[test]
    fn falls_back_to_the_first_profile_when_none_is_selected() {
        let none_selected = json!({
            "success": true,
            "profiles": [
                { "members": { "0123456789abcdef0123456789abcdef": { "currencies": { "coin": 1 } } } },
                { "members": { "0123456789abcdef0123456789abcdef": { "currencies": { "coin": 2 } } } }
            ]
        });
        let profile =
            parse_profile("0123456789abcdef0123456789abcdef", &none_selected).unwrap();
        assert_eq!(profile.currencies["coin"], 1);
    }

    #[test]
    fn ignores_an_ability_outside_the_skill_tree() {
        // The API keeps the selected ability inside the skill tree. A copy at the
        // root of the member is not what it reports.
        let misplaced = json!({
            "success": true,
            "profiles": [{
                "members": {
                    "0123456789abcdef0123456789abcdef": {
                        "selected_ability": { "mining_1": "pickobulus" }
                    }
                }
            }]
        });
        let profile =
            parse_profile("0123456789abcdef0123456789abcdef", &misplaced).unwrap();
        assert_eq!(profile.mining.ability, None);
    }
}
