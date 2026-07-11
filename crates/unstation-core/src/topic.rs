//! Statement-store topic derivation and discovery sharding (TECH_SPEC §4, §7.2).
//!
//! All topics are `BLAKE2b-256` over a domain prefix + the stream id (+ shard or
//! peer). The discovery topic is sharded so a joining peer reads a few shards, not
//! the whole audience.

use crate::crypto::blake2b256;
use crate::signaling::TopicId;
use crate::types::{PeerId, StreamId};

/// Peer rendezvous topic for a given shard: `BLAKE2b-256("disc" ‖ stream_id ‖ shard)`.
pub fn discovery_topic(stream: &StreamId, shard: u32) -> TopicId {
    let mut buf = Vec::with_capacity(4 + 32 + 4);
    buf.extend_from_slice(b"disc");
    buf.extend_from_slice(&stream.0);
    buf.extend_from_slice(&shard.to_le_bytes());
    blake2b256(&buf)
}

/// Targeted SDP/ICE delivery topic: `BLAKE2b-256("sig" ‖ stream_id ‖ peer_id)`.
pub fn signaling_topic(stream: &StreamId, peer: &PeerId) -> TopicId {
    let mut buf = Vec::with_capacity(3 + 32 + 32);
    buf.extend_from_slice(b"sig");
    buf.extend_from_slice(&stream.0);
    buf.extend_from_slice(&peer.0);
    blake2b256(&buf)
}

/// Fast-tier (WebRTC media) SDP/ICE delivery topic: `BLAKE2b-256("fastsig" ‖ stream ‖ peer)`.
/// Domain-separated from [`signaling_topic`] so the opt-in media fast tier's offer/answer
/// never mix with the mesh's data-channel negotiation — they share the `SignalMsg` envelope
/// but are read on independent topics, so neither transport ever sees the other's messages.
pub fn fast_signaling_topic(stream: &StreamId, peer: &PeerId) -> TopicId {
    let mut buf = Vec::with_capacity(7 + 32 + 32);
    buf.extend_from_slice(b"fastsig");
    buf.extend_from_slice(&stream.0);
    buf.extend_from_slice(&peer.0);
    blake2b256(&buf)
}

/// Signed current-segment announcements topic: `BLAKE2b-256("edge" ‖ stream_id)`.
pub fn edge_topic(stream: &StreamId) -> TopicId {
    let mut buf = Vec::with_capacity(4 + 32);
    buf.extend_from_slice(b"edge");
    buf.extend_from_slice(&stream.0);
    blake2b256(&buf)
}

/// Durable-copy map topic: `BLAKE2b-256("durable" ‖ stream_id)`. The publisher posts
/// rolling `(seq → Bulletin CID)` entries here so a viewer whose deadline no peer can
/// meet can fetch the segment from the durable floor (TECH_SPEC §8.6).
pub fn durable_topic(stream: &StreamId) -> TopicId {
    let mut buf = Vec::with_capacity(7 + 32);
    buf.extend_from_slice(b"durable");
    buf.extend_from_slice(&stream.0);
    blake2b256(&buf)
}

/// Global volunteer rendezvous topic: `BLAKE2b-256("unstation/volunteers/v1")`.
///
/// This is deliberately the ONLY non-per-stream topic besides the readiness probe:
/// streams are non-enumerable (every other topic hashes in a stream id you must
/// already know), so a publisher looking for spare capacity has no way to find idle
/// seeds — and an idle seed has no stream to announce on. Open volunteer seeds
/// announce a `VolunteerRecord` here; publishers read it and recruit via each
/// volunteer's per-peer [`recruit_topic`] inbox.
pub fn volunteers_topic() -> TopicId {
    blake2b256(b"unstation/volunteers/v1")
}

/// Per-volunteer recruitment inbox: `BLAKE2b-256("recruit" ‖ peer_id)`. A publisher
/// posts sealed `Recruitment`s here to assign (or release) a stream on that volunteer.
pub fn recruit_topic(volunteer: &PeerId) -> TopicId {
    let mut buf = Vec::with_capacity(7 + 32);
    buf.extend_from_slice(b"recruit");
    buf.extend_from_slice(&volunteer.0);
    blake2b256(&buf)
}

/// Which discovery shard a peer announces into: `peer_id mod N_shards`.
pub fn shard_for(peer: &PeerId, n_shards: u32) -> u32 {
    if n_shards <= 1 {
        return 0;
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(&peer.0[..4]);
    u32::from_le_bytes(b) % n_shards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_deterministic_and_domain_separated() {
        let s = StreamId([1u8; 32]);
        assert_eq!(discovery_topic(&s, 0), discovery_topic(&s, 0));
        assert_ne!(discovery_topic(&s, 0), discovery_topic(&s, 1)); // shards differ
        let p = PeerId::from_u64(9);
        // Different domains never collide.
        assert_ne!(discovery_topic(&s, 0), edge_topic(&s));
        assert_ne!(signaling_topic(&s, &p), edge_topic(&s));
        assert_ne!(discovery_topic(&s, 0), signaling_topic(&s, &p));
        // The fast tier's per-peer topic is distinct from the mesh signaling topic.
        assert_ne!(fast_signaling_topic(&s, &p), signaling_topic(&s, &p));
        assert_eq!(fast_signaling_topic(&s, &p), fast_signaling_topic(&s, &p));
    }

    #[test]
    fn volunteer_topics_are_pinned_and_domain_separated() {
        // The global rendezvous is a fixed hash — pinned so the derivation (and every
        // deployed seed's announce address) can never drift silently.
        assert_eq!(
            crate::crypto::hex32(&volunteers_topic()),
            "afac5db536e197cbfbb6181edf7d542cc6f7d3a6f3e9f7ad417ee1dc9caf33ed"
        );

        // Recruit inboxes are per-volunteer.
        let a = PeerId::from_u64(1);
        let b = PeerId::from_u64(2);
        assert_eq!(recruit_topic(&a), recruit_topic(&a));
        assert_ne!(recruit_topic(&a), recruit_topic(&b));

        // And domain-separated from every existing topic even when the volunteer's
        // peer bytes equal a stream's bytes.
        let bytes = [1u8; 32];
        let (s, p) = (StreamId(bytes), PeerId(bytes));
        let recruit = recruit_topic(&p);
        assert_ne!(recruit, volunteers_topic());
        assert_ne!(recruit, discovery_topic(&s, 0));
        assert_ne!(recruit, signaling_topic(&s, &p));
        assert_ne!(recruit, fast_signaling_topic(&s, &p));
        assert_ne!(recruit, edge_topic(&s));
        assert_ne!(recruit, durable_topic(&s));
    }

    #[test]
    fn shard_for_is_bounded_and_spread() {
        let n = 4;
        let mut counts = [0u32; 4];
        for i in 0..400u64 {
            let shard = shard_for(&PeerId::from_u64(i), n);
            assert!(shard < n);
            counts[shard as usize] += 1;
        }
        // Every shard gets some peers (from_u64 puts i in the low bytes, so it's
        // a clean modulo spread).
        for c in counts {
            assert!(c > 0, "every shard should be used: {counts:?}");
        }
    }

    #[test]
    fn single_shard_is_zero() {
        assert_eq!(shard_for(&PeerId::from_u64(123), 1), 0);
        assert_eq!(shard_for(&PeerId::from_u64(123), 0), 0);
    }
}
