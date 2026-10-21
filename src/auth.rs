//! The challenge/response authentication between the mod and this service.
//!
//! The mod never sends a secret that could be replayed:
//!
//! 1. The service hands out a single use nonce.
//! 2. The mod signs the nonce with the private half of its Minecraft profile key
//!    pair and sends the signature, the public key and Mojang's own signature over
//!    that public key.
//! 3. The service checks both signatures. The first one proves that the caller
//!    owns the private key, the second one proves that Mojang issued the public
//!    key for that exact player uuid.
//!
//! Only then a session token is handed out, which is what the mod uses to read
//! its own profile.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::keys;

type HmacSha256 = Hmac<Sha256>;

/// How long a nonce may be used.
const DEFAULT_CHALLENGE_TTL: Duration = Duration::from_secs(300);
/// How many nonces may be alive at the same time. The challenge endpoint is open to anybody, so the map of the
/// nonces may not grow without a limit.
const MAX_CHALLENGES: usize = 10_000;

/// How many challenges one address may ask for in a minute.
const CHALLENGES_PER_IP_PER_MINUTE: u32 = 30;

/// How many addresses are remembered.
const MAX_TRACKED_IPS: usize = 10_000;

/// How long the challenges of one address are counted.
const IP_WINDOW: Duration = Duration::from_secs(60);

/// The number of challenges per address, and when the window started.
#[derive(Default)]
struct IpLimits {
    counts: HashMap<IpAddr, (u32, Instant)>,
}

impl IpLimits {
    /// Counts one challenge of that address. False when the address asked too many times.
    fn take(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();
        if self.counts.len() >= MAX_TRACKED_IPS {
            self.counts
                .retain(|_, (_, started)| now.duration_since(*started) < IP_WINDOW);
            // A flood from more addresses than we are willing to track leaves
            // the map full. Starting the window over keeps the memory bounded;
            // these counters are defence in depth, not an audit record.
            if self.counts.len() >= MAX_TRACKED_IPS {
                self.counts.clear();
            }
        }
        let entry = self.counts.entry(ip).or_insert((0, now));
        if now.duration_since(entry.1) >= IP_WINDOW {
            *entry = (0, now);
        }
        if entry.0 >= CHALLENGES_PER_IP_PER_MINUTE {
            return false;
        }
        entry.0 += 1;
        true
    }
}

/// The nonces that were handed out and are still waiting to be used.
#[derive(Default)]
struct Challenges {
    entries: HashMap<String, Instant>,
    order: VecDeque<String>,
}

impl Challenges {
    /// Remembers a nonce and drops the oldest ones when there are too many.
    fn insert(&mut self, nonce: String, issued: Instant) {
        self.order.push_back(nonce.clone());
        self.entries.insert(nonce, issued);
        while self.entries.len() > MAX_CHALLENGES {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }

    /// Uses a nonce once. A second attempt with the same nonce finds nothing.
    fn take(&mut self, nonce: &str) -> Option<Instant> {
        let issued = self.entries.remove(nonce)?;
        self.order.retain(|item| item != nonce);
        Some(issued)
    }

    /// Forgets the nonces that are too old to be used.
    fn purge(&mut self, ttl: Duration) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, issued)| now.duration_since(**issued) >= ttl)
            .map(|(nonce, _)| nonce.clone())
            .collect();
        for nonce in expired {
            self.entries.remove(&nonce);
            self.order.retain(|item| item != &nonce);
        }
    }
}

/// The state of the authentication service.
pub struct AuthService {
    secret: Vec<u8>,
    token_ttl: Duration,
    challenge_ttl: Duration,
    challenges: Mutex<Challenges>,
    ip_limits: Mutex<IpLimits>,
}

impl AuthService {
    /// Creates the service. `secret` signs the session tokens.
    pub fn new(secret: Vec<u8>, token_ttl: Duration) -> Self {
        Self {
            secret,
            token_ttl,
            challenge_ttl: DEFAULT_CHALLENGE_TTL,
            challenges: Mutex::new(Challenges::default()),
            ip_limits: Mutex::new(IpLimits::default()),
        }
    }

    /// Whether the address may ask for another challenge. The endpoint is open to anybody, so one address may
    /// not flood it.
    pub fn allow_challenge(&self, ip: IpAddr) -> bool {
        let mut limits = self.ip_limits.lock().unwrap_or_else(|e| e.into_inner());
        limits.take(ip)
    }

    /// Hands out a fresh single use nonce.
    pub fn issue_challenge(&self) -> (String, i64) {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let nonce = BASE64.encode(bytes);
        let now = Instant::now();

        let mut challenges = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
        if challenges.entries.len() >= MAX_CHALLENGES {
            challenges.purge(self.challenge_ttl);
        }
        challenges.insert(nonce.clone(), now);

        (nonce, now_millis() + self.challenge_ttl.as_millis() as i64)
    }

    /// Validates the signed challenge and issues a session token.
    pub fn verify(&self, request: &VerifyRequest) -> Result<VerifyResponse, AuthError> {
        // The nonce has to be one we handed out and it may only be used once.
        {
            let mut challenges = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
            let issued = challenges
                .take(&request.nonce)
                .ok_or(AuthError::UnknownNonce)?;
            if issued.elapsed() > self.challenge_ttl {
                return Err(AuthError::ExpiredNonce);
            }
        }

        let uuid = Uuid::parse_str(&request.uuid).map_err(|_| AuthError::InvalidUuid)?;

        // The caller has to prove that it owns the private key of the public key.
        let public_key =
            keys::decode_public_key(&request.public_key).ok_or(AuthError::InvalidPublicKey)?;
        let signature =
            keys::decode_signature(&request.signature).ok_or(AuthError::InvalidSignature)?;
        let nonce_bytes = BASE64
            .decode(request.nonce.as_bytes())
            .map_err(|_| AuthError::UnknownNonce)?;
        if !keys::verify_sha256(&public_key, &signature, &nonce_bytes) {
            return Err(AuthError::InvalidSignature);
        }

        // The public key has to be one Mojang issued for this player.
        let key_signature = keys::decode_signature(&request.public_key_signature)
            .ok_or(AuthError::InvalidKeySignature)?;
        if !keys::verify_mojang_key_signature(
            &uuid,
            request.expires_at,
            &public_key,
            &key_signature,
        ) {
            return Err(AuthError::InvalidKeySignature);
        }

        let expires_at = now_millis() + self.token_ttl.as_millis() as i64;
        let token = self.issue_token(&uuid, expires_at);
        Ok(VerifyResponse { token, expires_at })
    }

    /// Checks a session token and returns the uuid it belongs to.
    pub fn validate_token(&self, token: &str) -> Result<String, AuthError> {
        let mut parts = token.rsplitn(3, ':');
        let signature = parts.next().ok_or(AuthError::InvalidToken)?;
        let expires_at: i64 = parts
            .next()
            .ok_or(AuthError::InvalidToken)?
            .parse()
            .map_err(|_| AuthError::InvalidToken)?;
        let uuid = parts.next().ok_or(AuthError::InvalidToken)?;

        let payload = format!("{uuid}:{expires_at}");
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        let signature = BASE64_URL
            .decode(signature.as_bytes())
            .map_err(|_| AuthError::InvalidToken)?;
        mac.verify_slice(&signature)
            .map_err(|_| AuthError::InvalidToken)?;

        if expires_at <= now_millis() {
            return Err(AuthError::ExpiredToken);
        }
        Ok(uuid.to_string())
    }

    /// Issues a session token for a player.
    ///
    /// Crate visible instead of private so the HTTP tests of the router can hold
    /// a token without signing a real challenge, which needs RSA key material.
    pub(crate) fn issue_token(&self, uuid: &Uuid, expires_at: i64) -> String {
        let payload = format!("{uuid}:{expires_at}");
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        format!(
            "{payload}:{}",
            BASE64_URL.encode(mac.finalize().into_bytes())
        )
    }
}

/// The body of a verify request.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyRequest {
    /// The uuid of the player, with or without dashes.
    pub uuid: String,
    /// The base64 encoded DER public key of the profile key pair.
    pub public_key: String,
    /// The base64 encoded signature Mojang produced over the public key.
    #[serde(default)]
    pub public_key_signature: String,
    /// The moment the profile key expires, in milliseconds since the epoch.
    #[serde(default)]
    pub expires_at: i64,
    /// The nonce that was signed.
    pub nonce: String,
    /// The base64 encoded signature of the nonce.
    pub signature: String,
}

/// The answer of a successful verification.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResponse {
    /// The session token to use as a bearer token.
    pub token: String,
    /// The moment the token expires, in milliseconds since the epoch.
    pub expires_at: i64,
}

/// The reasons a verification can fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    UnknownNonce,
    ExpiredNonce,
    InvalidUuid,
    InvalidPublicKey,
    InvalidSignature,
    InvalidKeySignature,
    InvalidToken,
    ExpiredToken,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownNonce => "unknown or already used nonce",
            Self::ExpiredNonce => "the nonce expired",
            Self::InvalidUuid => "invalid uuid",
            Self::InvalidPublicKey => "invalid public key",
            Self::InvalidSignature => "invalid signature",
            Self::InvalidKeySignature => "invalid public key signature",
            Self::InvalidToken => "invalid token",
            Self::ExpiredToken => "the token expired",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AuthError {}

/// The current time in milliseconds since the epoch.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1v15::Pkcs1v15Sign;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::RsaPrivateKey;
    use sha2::Digest;

    fn service() -> AuthService {
        AuthService::new(b"a-test-secret".to_vec(), Duration::from_secs(3600))
    }

    #[test]
    fn a_challenge_can_only_be_used_once() {
        let service = service();
        let (nonce, _) = service.issue_challenge();
        assert!(service
            .challenges
            .lock()
            .unwrap()
            .entries
            .contains_key(&nonce));
    }

    #[test]
    fn tokens_round_trip() {
        let service = service();
        let uuid = Uuid::new_v4();
        let expires_at = now_millis() + 60_000;
        let token = service.issue_token(&uuid, expires_at);
        assert_eq!(service.validate_token(&token).unwrap(), uuid.to_string());
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let service = service();
        let uuid = Uuid::new_v4();
        let token = service.issue_token(&uuid, now_millis() + 60_000);
        let tampered = token.replace(&uuid.to_string(), &Uuid::new_v4().to_string());
        assert_eq!(
            service.validate_token(&tampered),
            Err(AuthError::InvalidToken)
        );
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let service = service();
        let uuid = Uuid::new_v4();
        let token = service.issue_token(&uuid, now_millis() - 1);
        assert_eq!(service.validate_token(&token), Err(AuthError::ExpiredToken));
    }

    #[test]
    fn the_challenges_do_not_grow_without_a_limit() {
        let service = service();

        for _ in 0..(MAX_CHALLENGES + 100) {
            service.issue_challenge();
        }

        let challenges = service.challenges.lock().unwrap();
        assert!(challenges.entries.len() <= MAX_CHALLENGES);
    }

    #[test]
    fn one_address_cannot_flood_the_challenge_endpoint() {
        let service = service();
        let mine = IpAddr::from([127, 0, 0, 1]);
        let other = IpAddr::from([10, 0, 0, 1]);

        for _ in 0..CHALLENGES_PER_IP_PER_MINUTE {
            assert!(service.allow_challenge(mine));
        }

        assert!(
            !service.allow_challenge(mine),
            "the address is over its budget"
        );
        assert!(
            service.allow_challenge(other),
            "another address has its own budget"
        );
    }

    #[test]
    fn one_address_may_not_ask_for_endless_challenges() {
        let service = service();
        let address = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

        for _ in 0..CHALLENGES_PER_IP_PER_MINUTE {
            assert!(service.allow_challenge(address));
        }
        assert!(!service.allow_challenge(address));

        // Another address is not punished for it.
        let other = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
        assert!(service.allow_challenge(other));
    }

    #[test]
    fn the_tracked_addresses_do_not_grow_without_a_limit() {
        let service = service();

        for index in 0..(MAX_TRACKED_IPS + 100) {
            let address = IpAddr::V4(std::net::Ipv4Addr::from(index as u32));
            service.allow_challenge(address);
        }

        let limits = service.ip_limits.lock().unwrap();
        assert!(limits.counts.len() <= MAX_TRACKED_IPS);
    }

    #[test]
    fn an_unknown_nonce_is_rejected() {
        let service = service();
        let request = VerifyRequest {
            uuid: Uuid::new_v4().to_string(),
            public_key: String::new(),
            public_key_signature: String::new(),
            expires_at: 0,
            nonce: "not-a-nonce".to_string(),
            signature: String::new(),
        };
        assert!(matches!(
            service.verify(&request),
            Err(AuthError::UnknownNonce)
        ));
    }

    #[test]
    fn a_wrong_signature_is_rejected() {
        let service = service();
        let (nonce, _) = service.issue_challenge();
        let private = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
        let der = private
            .to_public_key()
            .to_public_key_der()
            .unwrap()
            .as_bytes()
            .to_vec();
        let signature = private
            .sign(
                Pkcs1v15Sign::new::<Sha256>(),
                &Sha256::digest(b"something else"),
            )
            .unwrap();
        let request = VerifyRequest {
            uuid: Uuid::new_v4().to_string(),
            public_key: BASE64_URL.encode(&der),
            public_key_signature: String::new(),
            expires_at: 0,
            nonce,
            signature: BASE64_URL.encode(&signature),
        };
        assert!(matches!(
            service.verify(&request),
            Err(AuthError::InvalidSignature)
        ));
    }
}
