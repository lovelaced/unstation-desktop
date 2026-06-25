//! The mesh wire protocol (TECH_SPEC §6.1), SCALE-encoded for consistency with
//! the Polkadot stack. One reliable-ordered `ctrl` channel carries control
//! messages; one unreliable-unordered `bulk` channel carries `SegmentData`.

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
}
