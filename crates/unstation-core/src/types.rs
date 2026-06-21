//! Core identifier and sequence types.

use parity_scale_codec::{Decode, Encode};

/// Segment sequence number within a stream's live window.
pub type Seq = u64;

/// Content identifier on the Bulletin Chain (CIDv1 string).
pub type Cid = String;

/// 32-byte ephemeral session public key identifying a peer (TECH_SPEC §4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Encode, Decode)]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// Build a deterministic `PeerId` from a small integer — used by the simulator
    /// and tests so peers have stable, legible identities.
    pub fn from_u64(n: u64) -> Self {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&n.to_le_bytes());
        PeerId(b)
    }
}

/// 32-byte stream identifier = `BLAKE2b-256(publisher_pubkey ‖ stream_nonce)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Encode, Decode)]
pub struct StreamId(pub [u8; 32]);

/// 32-byte content-addressed segment id = `BLAKE2b-256(segment_bytes)` (TECH_SPEC §3.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Encode, Decode)]
pub struct SegmentId(pub [u8; 32]);
