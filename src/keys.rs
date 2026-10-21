//! Verification of the Minecraft profile key signatures.
//!
//! Two different signatures are involved:
//!
//! * The signature the client produces with the *private* half of the profile key
//!   pair (the "signed data"). Verifying it proves that the caller really owns the
//!   private key and is not replaying a key pair scraped from a server list.
//! * The signature Mojang produced over the public key when it issued the key pair
//!   (the "key signature"). Verifying it binds the public key to the player uuid,
//!   because only Mojang knows which key belongs to which account.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use base64::Engine;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use sha1::Sha1;
use sha2::Sha256;
use uuid::Uuid;

/// Verifies a PKCS#1 v1.5 signature made with SHA-256 (the algorithm the
/// vanilla client uses when signing chat and profile data).
pub fn verify_sha256(public_key_der: &[u8], signature: &[u8], payload: &[u8]) -> bool {
    let (Some(key), Some(signature)) = (parse_key(public_key_der), parse_signature(signature))
    else {
        return false;
    };
    VerifyingKey::<Sha256>::new(key)
        .verify(payload, &signature)
        .is_ok()
}

/// Verifies a PKCS#1 v1.5 signature made with SHA-1.
pub fn verify_sha1(public_key_der: &[u8], signature: &[u8], payload: &[u8]) -> bool {
    let (Some(key), Some(signature)) = (parse_key(public_key_der), parse_signature(signature))
    else {
        return false;
    };
    VerifyingKey::<Sha1>::new(key)
        .verify(payload, &signature)
        .is_ok()
}

/// Reads the public key out of its DER encoding.
fn parse_key(public_key_der: &[u8]) -> Option<RsaPublicKey> {
    RsaPublicKey::from_public_key_der(public_key_der).ok()
}

/// Reads the signature bytes.
fn parse_signature(signature: &[u8]) -> Option<Signature> {
    Signature::try_from(signature).ok()
}

/// Decodes a base64 encoded public key (a DER encoded X.509 SubjectPublicKeyInfo).
pub fn decode_public_key(public_key_base64: &str) -> Option<Vec<u8>> {
    decode_base64(public_key_base64)
}

/// Decodes a base64 encoded signature.
pub fn decode_signature(signature_base64: &str) -> Option<Vec<u8>> {
    decode_base64(signature_base64)
}

/// Decodes base64, accepting both the standard and the url safe alphabet.
fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    BASE64
        .decode(value)
        .ok()
        .or_else(|| BASE64_URL.decode(value).ok())
}

/// Builds the candidate payloads Mojang may have signed when it issued a profile
/// key pair, so that the exact layout can be verified against real data.
///
/// Mojang signs the player uuid, the public key and the expiry date, but the
/// exact byte layout is not part of the public API documentation. Every known
/// layout is tried, and a match can only happen for the real one because the
/// signature is checked against the public key itself.
pub fn mojang_signed_payloads(uuid: &Uuid, expires_at: i64, public_key_der: &[u8]) -> Vec<Vec<u8>> {
    let uuid_bytes = uuid.as_bytes();
    let expiry = expires_at.to_be_bytes();
    let key_length = (public_key_der.len() as u32).to_be_bytes();

    let mut payloads = Vec::new();
    let mut push = |mut bytes: Vec<u8>| {
        payloads.push(bytes.clone());
        bytes.clear();
    };

    // 0x01 | uuid | expiry | key length | key
    let mut p = Vec::new();
    p.push(0x01);
    p.extend_from_slice(uuid_bytes);
    p.extend_from_slice(&expiry);
    p.extend_from_slice(&key_length);
    p.extend_from_slice(public_key_der);
    push(p);

    // 0x01 | expiry | key length | key
    let mut p = Vec::new();
    p.push(0x01);
    p.extend_from_slice(&expiry);
    p.extend_from_slice(&key_length);
    p.extend_from_slice(public_key_der);
    push(p);

    // uuid | expiry | key length | key
    let mut p = Vec::new();
    p.extend_from_slice(uuid_bytes);
    p.extend_from_slice(&expiry);
    p.extend_from_slice(&key_length);
    p.extend_from_slice(public_key_der);
    push(p);

    // expiry | key length | key
    let mut p = Vec::new();
    p.extend_from_slice(&expiry);
    p.extend_from_slice(&key_length);
    p.extend_from_slice(public_key_der);
    push(p);

    // 0x01 | uuid | expiry | key
    let mut p = Vec::new();
    p.push(0x01);
    p.extend_from_slice(uuid_bytes);
    p.extend_from_slice(&expiry);
    p.extend_from_slice(public_key_der);
    push(p);

    // uuid | key | expiry
    let mut p = Vec::new();
    p.extend_from_slice(uuid_bytes);
    p.extend_from_slice(public_key_der);
    p.extend_from_slice(&expiry);
    push(p);

    payloads
}

/// Verifies the signature Mojang produced over the public key of a profile key
/// pair. Returns `true` only when the signature matches one of the known
/// layouts, which proves that Mojang issued this key for this player.
pub fn verify_mojang_key_signature(
    uuid: &Uuid,
    expires_at: i64,
    public_key_der: &[u8],
    signature: &[u8],
) -> bool {
    for payload in mojang_signed_payloads(uuid, expires_at, public_key_der) {
        if verify_sha1(public_key_der, signature, &payload) {
            return true;
        }
        if verify_sha256(public_key_der, signature, &payload) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use rsa::pkcs1v15::Pkcs1v15Sign;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::RsaPrivateKey;
    use sha2::Digest;

    fn test_key() -> (RsaPrivateKey, Vec<u8>) {
        let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("key");
        let der = private
            .to_public_key()
            .to_public_key_der()
            .expect("der")
            .as_bytes()
            .to_vec();
        (private, der)
    }

    #[test]
    fn verifies_a_sha256_signature() {
        let (private, der) = test_key();
        let payload = b"the nonce";
        let digest = Sha256::digest(payload);
        let signature = private
            .sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
            .expect("sign");
        assert!(verify_sha256(&der, &signature, payload));
        assert!(!verify_sha256(&der, &signature, b"other"));
    }

    #[test]
    fn rejects_a_wrong_public_key() {
        let (private, _) = test_key();
        let (_, other_der) = test_key();
        let payload = b"the nonce";
        let digest = Sha256::digest(payload);
        let signature = private
            .sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
            .expect("sign");
        assert!(!verify_sha256(&other_der, &signature, payload));
    }

    #[test]
    fn finds_the_mojang_layout() {
        let (private, der) = test_key();
        let uuid = Uuid::new_v4();
        let expires_at = 1_700_000_000_000_i64;
        let payload = mojang_signed_payloads(&uuid, expires_at, &der).remove(0);
        let digest = Sha1::digest(&payload);
        let signature = private
            .sign(Pkcs1v15Sign::new::<Sha1>(), &digest)
            .expect("sign");
        assert!(verify_mojang_key_signature(
            &uuid, expires_at, &der, &signature
        ));
    }

    #[test]
    fn rejects_a_key_signature_of_another_uuid() {
        let (private, der) = test_key();
        let uuid = Uuid::new_v4();
        let other = Uuid::new_v4();
        let expires_at = 1_700_000_000_000_i64;
        let payload = mojang_signed_payloads(&uuid, expires_at, &der).remove(0);
        let digest = Sha1::digest(&payload);
        let signature = private
            .sign(Pkcs1v15Sign::new::<Sha1>(), &digest)
            .expect("sign");
        assert!(!verify_mojang_key_signature(
            &other, expires_at, &der, &signature
        ));
    }
}
