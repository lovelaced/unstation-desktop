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

// ---- signaling-envelope confidentiality (Tier 0 privacy) ------------------------------
//
// SDP/ICE carries both peers' IP addresses; posted plaintext to the public statement
// store it lets anyone who knows a stream name harvest publisher + viewer IPs remotely,
// passively, without joining. We seal each envelope to its recipient: static-static
// X25519 ECDH (the keypair derived deterministically from the identity secret, so there
// is no new key to manage or persist) keys an XChaCha20-Poly1305 box. One ECDH per
// signaling message — nothing on the media path, no measurable latency cost.
//
// This is a CONFIDENTIALITY layer against a passive on-chain observer, not a new
// authentication layer: the statement itself is already signed by the sender's
// statement-store key, and segments/manifests stay hash/-signature-verified downstream.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// Domain-separation labels — a derived key can never be reused across purposes.
const ENC_KDF_LABEL: &[u8] = b"unstation-x25519-enc-v1";
const SEAL_KDF_LABEL: &[u8] = b"unstation-seal-v1";

/// Derive the static X25519 encryption keypair `(secret, public)` from a 32-byte
/// identity seed (the sr25519 mini-secret / statement-store secret bytes). Domain-
/// separated from every signing use, and deterministic — the same identity always
/// yields the same encryption key, so a recipient's key is stable and advertisable.
pub fn enc_keypair_from_seed(seed: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut material = Vec::with_capacity(ENC_KDF_LABEL.len() + 32);
    material.extend_from_slice(ENC_KDF_LABEL);
    material.extend_from_slice(seed);
    let sk_bytes = blake2b256(&material);
    let secret = x25519_dalek::StaticSecret::from(sk_bytes);
    let public = x25519_dalek::PublicKey::from(&secret);
    (secret.to_bytes(), public.to_bytes())
}

/// The public half of [`enc_keypair_from_seed`] — for advertising in a presence record.
pub fn enc_public_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    enc_keypair_from_seed(seed).1
}

/// Shared secret → AEAD key: `BLAKE2b-256(label ‖ ECDH)`. Symmetric in the pair
/// (`DH(a_sec, b_pub) == DH(b_sec, a_pub)`), so both ends derive the same key.
fn seal_key(shared: &[u8; 32]) -> [u8; 32] {
    let mut material = Vec::with_capacity(SEAL_KDF_LABEL.len() + 32);
    material.extend_from_slice(SEAL_KDF_LABEL);
    material.extend_from_slice(shared);
    blake2b256(&material)
}

/// Seal `plaintext` to `recipient_pub` using our `sender_secret`. Output is
/// `nonce(24) ‖ ciphertext+tag`; the caller transmits the sender's public key alongside
/// (it's needed to open, and reveals nothing — it's a pseudonymous, rotatable key).
/// A fresh random 24-byte XNonce per call makes nonce reuse a non-issue even with a
/// static key pair.
pub fn seal(recipient_pub: &[u8; 32], sender_secret: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let secret = x25519_dalek::StaticSecret::from(*sender_secret);
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(*recipient_pub));
    let cipher = XChaCha20Poly1305::new(seal_key(shared.as_bytes()).as_slice().into());
    let mut nonce = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let mut out = Vec::with_capacity(24 + plaintext.len() + 16);
    out.extend_from_slice(&nonce);
    // XChaCha20-Poly1305 encryption cannot fail for in-memory input.
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .expect("aead encrypt");
    out.extend_from_slice(&ct);
    out
}

/// Open a [`seal`]ed message from `sender_pub` with our `recipient_secret`. Returns
/// `None` on any malformed input or authentication failure — never panics.
pub fn open(sender_pub: &[u8; 32], recipient_secret: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 24 + 16 {
        return None;
    }
    let secret = x25519_dalek::StaticSecret::from(*recipient_secret);
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(*sender_pub));
    let cipher = XChaCha20Poly1305::new(seal_key(shared.as_bytes()).as_slice().into());
    cipher
        .decrypt(XNonce::from_slice(&sealed[..24]), &sealed[24..])
        .ok()
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
    fn sealed_envelope_roundtrips_and_fails_closed() {
        // Two identities, derived from their seeds like the statement key is.
        let (a_sec, a_pub) = enc_keypair_from_seed(&[1u8; 32]);
        let (b_sec, b_pub) = enc_keypair_from_seed(&[2u8; 32]);
        let msg = b"v=0\r\na=candidate 192.0.2.7 ...";

        // A seals to B; only B opens it.
        let sealed = seal(&b_pub, &a_sec, msg);
        assert_ne!(&sealed[24..], &msg[..], "ciphertext is not the plaintext");
        assert_eq!(open(&a_pub, &b_sec, &sealed).as_deref(), Some(&msg[..]));

        // The reply direction shares the same secret (static-static ECDH is symmetric).
        let back = seal(&a_pub, &b_sec, b"answer");
        assert_eq!(open(&b_pub, &a_sec, &back).as_deref(), Some(&b"answer"[..]));

        // A third party cannot open a box addressed to B, a tampered tag fails, truncation fails.
        let (c_sec, _c_pub) = enc_keypair_from_seed(&[3u8; 32]);
        assert!(open(&a_pub, &c_sec, &sealed).is_none(), "wrong recipient key fails");
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&a_pub, &b_sec, &tampered).is_none(), "tampered tag fails");
        assert!(open(&a_pub, &b_sec, &sealed[..20]).is_none(), "truncated fails");

        // Deterministic derivation: same seed → same keypair (advertisable, stable).
        assert_eq!(enc_keypair_from_seed(&[1u8; 32]), (a_sec, a_pub));
        // Two seals of the same message differ (fresh nonce).
        assert_ne!(seal(&b_pub, &a_sec, msg), seal(&b_pub, &a_sec, msg));
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
