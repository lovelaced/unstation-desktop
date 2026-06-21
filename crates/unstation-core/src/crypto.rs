//! Content addressing (`blake2b256`) and publisher signatures (sr25519).
//!
//! `blake2b256` matches product-sdk's `crypto/src/hashing.ts` (`blake2b`, dkLen 32),
//! so segment ids and CIDs are wire-compatible with the rest of the stack.

use crate::types::SegmentId;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};

type Blake2b256 = Blake2b<U32>;

/// 32-byte BLAKE2b-256 digest.
pub fn blake2b256(data: &[u8]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    h.update(data);
    let out = h.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    a
}

/// Content-addressed segment id = `BLAKE2b-256(segment_bytes)` (TECH_SPEC §3.1).
pub fn segment_id(bytes: &[u8]) -> SegmentId {
    SegmentId(blake2b256(bytes))
}

/// Verify reassembled bytes against their advertised id. A malicious peer cannot
/// inject corrupt data undetected.
pub fn verify_segment(bytes: &[u8], id: &SegmentId) -> bool {
    blake2b256(bytes) == id.0
}

/// Signing-context label for manifests + live-edge announcements.
pub const MANIFEST_CONTEXT: &[u8] = b"unstation-manifest";

/// Verify an sr25519 signature over `msg` by `pubkey`. Returns false on any
/// malformed input rather than panicking.
pub fn verify_sr25519(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let pk = match schnorrkel::PublicKey::from_bytes(pubkey) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let s = match schnorrkel::Signature::from_bytes(sig) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let ctx = schnorrkel::signing_context(MANIFEST_CONTEXT);
    pk.verify(ctx.bytes(msg), &s).is_ok()
}

/// Deterministic keypair from a 32-byte seed. The shipping publisher signs via
/// the Polkadot app (host-signer); this exists for tests and a fully-local dev publisher.
pub fn keypair_from_seed(seed: &[u8; 32]) -> schnorrkel::Keypair {
    schnorrkel::MiniSecretKey::from_bytes(seed)
        .expect("32-byte seed")
        .expand_to_keypair(schnorrkel::ExpansionMode::Ed25519)
}

pub fn public_bytes(kp: &schnorrkel::Keypair) -> [u8; 32] {
    kp.public.to_bytes()
}

pub fn sign_sr25519(kp: &schnorrkel::Keypair, msg: &[u8]) -> [u8; 64] {
    let ctx = schnorrkel::signing_context(MANIFEST_CONTEXT);
    kp.sign(ctx.bytes(msg)).to_bytes()
}

/// Lowercase hex of a 32-byte id (for CIDs / disk filenames).
pub fn hex32(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_hash_roundtrip() {
        let data = b"hello segment";
        let id = segment_id(data);
        assert!(verify_segment(data, &id));
        assert!(!verify_segment(b"tampered", &id));
    }

    #[test]
    fn blake2b256_is_deterministic_and_32_bytes() {
        let a = blake2b256(b"x");
        let b = blake2b256(b"x");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert_ne!(blake2b256(b"x"), blake2b256(b"y"));
    }

    #[test]
    fn sr25519_sign_verify() {
        let kp = keypair_from_seed(&[7u8; 32]);
        let pk = public_bytes(&kp);
        let msg = b"manifest bytes";
        let sig = sign_sr25519(&kp, msg);
        assert!(verify_sr25519(&pk, msg, &sig));
        // wrong message, wrong key, malformed all fail closed.
        assert!(!verify_sr25519(&pk, b"other", &sig));
        let mut bad_key = pk;
        bad_key[0] ^= 1;
        assert!(!verify_sr25519(&bad_key, msg, &sig));
        let mut bad_sig = sig;
        bad_sig[0] ^= 1;
        assert!(!verify_sr25519(&pk, msg, &bad_sig));
    }
}
