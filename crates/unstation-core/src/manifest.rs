//! Stream manifest (TECH_SPEC §3.2) + the `OriginOfRecord` trait (Bulletin Chain).
//!
//! The manifest is SCALE-encoded (canonical bytes) and signed with the publisher's
//! sr25519 key; peers verify against the publisher pubkey learned out-of-band
//! (the trust anchor, TECH_SPEC §3.3) before trusting any buffer map or live edge.

use crate::crypto;
use crate::types::{Cid, SegmentId, StreamId};
use crate::BoxFuture;
use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Encode, Decode)]
pub enum Kind {
    Live,
    Vod,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Track {
    pub id: String,
    pub bitrate: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Manifest {
    pub stream_id: StreamId,
    pub kind: Kind,
    pub codec: String,
    pub init_segment_cid: Cid,
    pub target_segment_ms: u32,
    pub ll_mode: bool,
    pub tracks: Vec<Track>,
    /// Publisher sr25519 public key — the trust anchor.
    pub publisher: [u8; 32],
    pub created_at: u64,
    /// Invite-only encryption (Tier 1 privacy): when true, every segment and the init
    /// are sealed with a stream key that travels ONLY in the invite link — the mesh,
    /// seeds, and Bulletin hold ciphertext. A viewer without the key cannot play. The
    /// flag is inside the SIGNED manifest, so a relay can't strip it to trick a viewer
    /// into treating ciphertext as plaintext.
    pub encrypted: bool,
}

impl Manifest {
    /// The canonical bytes that are signed (and over which a signature is verified).
    pub fn signing_payload(&self) -> Vec<u8> {
        self.encode()
    }

    /// Verify `sig` is a valid publisher signature over this manifest, and that the
    /// embedded publisher matches the `expected_publisher` trust anchor.
    pub fn verify(&self, expected_publisher: &[u8; 32], sig: &[u8; 64]) -> crate::Result<()> {
        if &self.publisher != expected_publisher {
            return Err(crate::Error::BadSignature);
        }
        if crypto::verify_sr25519(&self.publisher, &self.signing_payload(), sig) {
            Ok(())
        } else {
            Err(crate::Error::BadSignature)
        }
    }
}

/// A manifest plus the publisher's signature over its canonical bytes — the unit
/// stored on the origin and verified before any of the stream's data is trusted.
/// The publisher key is the trust anchor; a viewer learns it from the publisher's
/// allowance-backed presence record (its `PeerId` *is* its sr25519 pubkey).
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SignedManifest {
    pub manifest: Manifest,
    pub sig: [u8; 64],
}

impl SignedManifest {
    /// Verify the signature and that the embedded publisher matches the expected
    /// trust anchor. Call before trusting any buffer map, live edge, or segment.
    pub fn verify(&self, expected_publisher: &[u8; 32]) -> crate::Result<()> {
        self.manifest.verify(expected_publisher, &self.sig)
    }
}

/// Backed by the Bulletin chain: the durable, censorship-resistant trust + boot
/// anchor (signed manifest + init segment). Bulk segment bytes are deliberately NOT
/// stored here — the metered chain allowance (~tens of MB) holds the manifest, never
/// a multi-GB stream; bulk durability is the CDN/origin floor's job. Reads fetch
/// content-addressed bytes by CID; the publisher writes once per stream.
pub trait OriginOfRecord: Send + Sync {
    fn fetch_manifest(&self, cid: Cid) -> BoxFuture<'static, crate::Result<SignedManifest>>;
    fn put_manifest(&self, manifest: SignedManifest) -> BoxFuture<'static, crate::Result<Cid>>;
    fn fetch_segment(&self, id: SegmentId) -> BoxFuture<'static, crate::Result<Bytes>>;
    fn put_segment(&self, id: SegmentId, bytes: Bytes) -> BoxFuture<'static, crate::Result<Cid>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(publisher: [u8; 32]) -> Manifest {
        Manifest {
            stream_id: StreamId([1u8; 32]),
            kind: Kind::Vod,
            codec: "avc1.640028,mp4a.40.2".into(),
            init_segment_cid: "bafyinit".into(),
            target_segment_ms: 2000,
            ll_mode: true,
            tracks: vec![Track { id: "v1080".into(), bitrate: 5_000_000, w: 1920, h: 1080 }],
            publisher,
            created_at: 1_734_820_000,
            encrypted: false,
        }
    }

    #[test]
    fn scale_roundtrip() {
        let m = sample([3u8; 32]);
        let bytes = m.encode();
        assert_eq!(Manifest::decode(&mut &bytes[..]).unwrap(), m);
    }

    #[test]
    fn sign_and_verify() {
        let kp = crypto::keypair_from_seed(&[9u8; 32]);
        let pk = crypto::public_bytes(&kp);
        let m = sample(pk);
        let sig = crypto::sign_sr25519(&kp, &m.signing_payload());
        assert!(m.verify(&pk, &sig).is_ok());

        // Tampered manifest: signature no longer matches.
        let mut tampered = m.clone();
        tampered.created_at += 1;
        assert!(tampered.verify(&pk, &sig).is_err());

        // Wrong trust anchor is rejected even with a valid self-signature.
        let other = crypto::keypair_from_seed(&[10u8; 32]);
        let other_pk = crypto::public_bytes(&other);
        assert!(m.verify(&other_pk, &sig).is_err());
    }

    #[test]
    fn signed_manifest_roundtrip_and_verify() {
        let kp = crypto::keypair_from_seed(&[9u8; 32]);
        let pk = crypto::public_bytes(&kp);
        let m = sample(pk);
        let sig = crypto::sign_sr25519(&kp, &m.signing_payload());
        let signed = SignedManifest { manifest: m, sig };

        // SCALE roundtrip survives intact.
        let bytes = signed.encode();
        assert_eq!(SignedManifest::decode(&mut &bytes[..]).unwrap(), signed);

        // Verifies against the right anchor; a wrong anchor is rejected.
        assert!(signed.verify(&pk).is_ok());
        let other = crypto::public_bytes(&crypto::keypair_from_seed(&[11u8; 32]));
        assert!(signed.verify(&other).is_err());
    }
}
