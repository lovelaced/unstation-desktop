//! The mesh wire protocol (TECH_SPEC §6.1), SCALE-encoded for consistency with
//! the Polkadot stack. One reliable-ordered `ctrl` channel carries control
//! messages; one unreliable-unordered `bulk` channel carries `SegmentData`.

use crate::signaling::PresenceRecord;
use crate::types::Seq;
use parity_scale_codec::{Decode, Encode};

#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    pub upload_bps: u64,
    pub relay: bool,
}

#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub enum MeshMsg {
    /// Sent on connect, carrying capabilities and the initial buffer map.
    Hello {
        peer_id: [u8; 32],
        stream_id: [u8; 32],
        version: u16,
        caps: Caps,
        base_seq: Seq,
        bitfield: Vec<u8>,
    },
    /// Periodic buffer-map advertise (every 500 ms or on material change).
    BufferMap { base_seq: Seq, bitfield: Vec<u8> },
    /// Request one or more segments, with an optional deadline hint.
    Want { segment_seqs: Vec<Seq>, deadline_hint_ms: u32 },
    /// Proactive low-frequency availability announce.
    Have { seq: Seq },
    /// A chunk of a segment (reassembled by `(seq, offset)`, verified by hash).
    SegmentData {
        seq: Seq,
        track_id: u16,
        total_len: u32,
        offset: u32,
        bytes: Vec<u8>,
    },
    /// Demand changed — stop sending this segment (cancels a hedged request).
    Cancel { seq: Seq },
    Choke,
    Unchoke,
    Ping { nonce: u64, t_send_ms: u64 },
    Pong { nonce: u64, t_send_ms: u64 },
    /// In-mesh peer discovery after bootstrap (TECH_SPEC §7.3).
    PeerGossip { peers: Vec<[u8; 32]> },
    /// Register standing interest in a holder's live edge (push-pull, TECH_SPEC §6.4).
    /// Once subscribed, the holder PUSHES each new segment as it lands instead of
    /// waiting for a per-segment `Want` — cutting steady-state live-edge latency from
    /// a discovery + request round-trip down to ~one hop. New variants are appended so
    /// the `SegmentData` tag (4) the hand-rolled framing depends on never shifts.
    Subscribe,
    /// Withdraw a `Subscribe` (e.g. the viewer paused / left the live edge to seek VOD).
    Unsubscribe,
    /// Signed live-edge announcement, gossiped in-mesh (TECH_SPEC §6.4, off-chain
    /// signaling). Carries a segment's authenticated content id signed by the publisher
    /// (sr25519); any node verifies it against the publisher's pubkey and re-gossips it,
    /// so the edge propagates at mesh speed instead of via the ~2.8 s chain poll. The
    /// chain edge remains a coarse fallback for cold/partitioned viewers.
    EdgeAnnounce { seq: Seq, id: [u8; 32], sig: [u8; 64] },
    /// In-mesh presence directory gossip (TECH_SPEC §7.3, off-chain signaling). Carries
    /// known peers' presence so a node that reached one peer discovers the swarm without
    /// reading the chain — replacing the per-viewer chain presence write. A dial hint
    /// only (unsigned): trust is still gated by the manifest + signed live-edge.
    PresenceGossip { records: Vec<PresenceRecord> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_want() {
        let msg = MeshMsg::Want {
            segment_seqs: vec![1, 2, 3],
            deadline_hint_ms: 1500,
        };
        let bytes = msg.encode();
        let decoded = MeshMsg::decode(&mut &bytes[..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_hello() {
        let msg = MeshMsg::Hello {
            peer_id: [7u8; 32],
            stream_id: [9u8; 32],
            version: 1,
            caps: Caps { upload_bps: 5_000_000, relay: true },
            base_seq: 100,
            bitfield: vec![0xff, 0x0f],
        };
        let bytes = msg.encode();
        assert_eq!(MeshMsg::decode(&mut &bytes[..]).unwrap(), msg);
    }

    #[test]
    fn roundtrip_subscribe() {
        for msg in [MeshMsg::Subscribe, MeshMsg::Unsubscribe] {
            let bytes = msg.encode();
            assert_eq!(MeshMsg::decode(&mut &bytes[..]).unwrap(), msg);
        }
        // Appending the push-pull variants must not have moved the SegmentData tag (4),
        // which the one-copy framing in `node::frame_segment_data` hardcodes.
        let sd = MeshMsg::SegmentData { seq: 0, track_id: 0, total_len: 1, offset: 0, bytes: vec![0] };
        assert_eq!(sd.encode()[0], 4, "SegmentData must stay variant 4");
    }

    #[test]
    fn roundtrip_edge_announce() {
        let msg = MeshMsg::EdgeAnnounce { seq: 42, id: [3u8; 32], sig: [9u8; 64] };
        let bytes = msg.encode();
        assert_eq!(MeshMsg::decode(&mut &bytes[..]).unwrap(), msg);
    }

    #[test]
    fn roundtrip_presence_gossip() {
        let msg = MeshMsg::PresenceGossip {
            records: vec![PresenceRecord {
                peer_id: [5u8; 32],
                caps_upload_bps: 20_000_000,
                ttl_s: 30,
                manifest_cid: Some("bafy-cid".into()),
                relay: true,
            }],
        };
        let bytes = msg.encode();
        assert_eq!(MeshMsg::decode(&mut &bytes[..]).unwrap(), msg);
    }

    /// Every wire variant must survive a SCALE round-trip AND keep a stable variant tag —
    /// the hand-rolled `SegmentData` framing (tag 4) and cross-version compatibility both
    /// depend on the discriminants never silently shifting. Adding a variant is fine (new
    /// tag at the end); reordering or inserting is a wire-breaking change this test catches.
    #[test]
    fn all_variants_roundtrip_with_stable_tags() {
        let rec = PresenceRecord {
            peer_id: [7u8; 32],
            caps_upload_bps: 19,
            ttl_s: 30,
            manifest_cid: Some("cid".into()),
            relay: false,
        };
        let variants: Vec<(u8, MeshMsg)> = vec![
            (0, MeshMsg::Hello { peer_id: [1; 32], stream_id: [2; 32], version: 3, caps: Caps { upload_bps: 4, relay: true }, base_seq: 5, bitfield: vec![0xAB] }),
            (1, MeshMsg::BufferMap { base_seq: 6, bitfield: vec![0xCD, 0xEF] }),
            (2, MeshMsg::Want { segment_seqs: vec![7, 8], deadline_hint_ms: 9 }),
            (3, MeshMsg::Have { seq: 10 }),
            (4, MeshMsg::SegmentData { seq: 11, track_id: 1, total_len: 12, offset: 0, bytes: vec![0xFF; 12] }),
            (5, MeshMsg::Cancel { seq: 13 }),
            (6, MeshMsg::Choke),
            (7, MeshMsg::Unchoke),
            (8, MeshMsg::Ping { nonce: 14, t_send_ms: 15 }),
            (9, MeshMsg::Pong { nonce: 16, t_send_ms: 17 }),
            (10, MeshMsg::PeerGossip { peers: vec![[3; 32], [4; 32]] }),
            (11, MeshMsg::Subscribe),
            (12, MeshMsg::Unsubscribe),
            (13, MeshMsg::EdgeAnnounce { seq: 18, id: [5; 32], sig: [6; 64] }),
            (14, MeshMsg::PresenceGossip { records: vec![rec] }),
        ];
        for (tag, msg) in &variants {
            let enc = msg.encode();
            assert_eq!(enc[0], *tag, "variant {msg:?} must encode with stable tag {tag}");
            let dec = MeshMsg::decode(&mut &enc[..]).expect("variant must decode");
            assert_eq!(&dec, msg, "variant {msg:?} must survive a SCALE round-trip");
        }
    }

    /// Malformed bytes from a hostile/buggy peer must be rejected, never panic — the node's
    /// `on_inbound` relies on a clean `Err` to drop bad frames.
    #[test]
    fn decode_rejects_garbage_and_truncation() {
        assert!(MeshMsg::decode(&mut &[][..]).is_err(), "empty input");
        assert!(MeshMsg::decode(&mut &[200u8][..]).is_err(), "unknown variant tag");
        assert!(MeshMsg::decode(&mut &[4u8][..]).is_err(), "truncated SegmentData (tag only)");
        assert!(MeshMsg::decode(&mut &[2u8, 0xFF][..]).is_err(), "truncated Want length prefix");
    }
}
