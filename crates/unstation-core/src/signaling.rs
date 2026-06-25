//! The `Signaling` trait — backed by the Polkadot statement store.
//!
//! Used **only** for the initial SDP/ICE exchange to establish the first 1–3
//! data-channel links; after that, peer discovery is gossiped in-mesh
//! (`PeerGossip`) to respect the scarce per-user slot budget (TECH_SPEC §7.3).

use crate::types::{PeerId, StreamId};
use crate::BoxFuture;
use parity_scale_codec::{Decode, Encode};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

/// A presence announcement published to the (sharded) discovery topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub peer_id: PeerId,
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
    /// Bulletin CID of the publisher's signed manifest (M2). A viewer fetches +
    /// verifies it against `peer_id` (which is the publisher's sr25519 pubkey)
    /// before trusting the stream. `None` until the publisher has published it
    /// (e.g. before the encoder's init segment exists), or for plain viewers.
    pub manifest_cid: Option<String>,
    /// Relay-capability hint (M4): a well-connected volunteer advertises `true` so
    /// NAT-restricted peers preferentially dial it — the decentralized stand-in for
    /// a TURN server (publishers + seed/relay nodes set it; plain viewers don't).
    pub relay: bool,
}

/// SCALE wire form of a presence record (the statement `data` payload).
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub struct PresenceRecord {
    pub peer_id: [u8; 32],
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
    pub manifest_cid: Option<String>,
    pub relay: bool,
}

impl From<&Presence> for PresenceRecord {
    fn from(p: &Presence) -> Self {
        Self {
            peer_id: p.peer_id.0,
            caps_upload_bps: p.caps_upload_bps,
            ttl_s: p.ttl_s,
            manifest_cid: p.manifest_cid.clone(),
            relay: p.relay,
        }
    }
}
impl From<PresenceRecord> for Presence {
    fn from(r: PresenceRecord) -> Self {
        Self {
            peer_id: PeerId(r.peer_id),
            caps_upload_bps: r.caps_upload_bps,
            ttl_s: r.ttl_s,
            manifest_cid: r.manifest_cid,
            relay: r.relay,
        }
    }
}

/// Shared, in-mesh presence directory (off-chain signaling, TECH_SPEC §7.3). Plain
/// viewers no longer write presence to the chain every refresh (O(viewers) writes) —
/// instead they gossip presence peer-to-peer into this book, so a node that has reached
/// *one* peer learns the rest of the swarm without reading the chain. Only bootstrap
/// **anchors** (publishers + reachable relay volunteers) still write to the chain, so a
/// cold joiner can find an entry point. Cheap to clone; shared between the `MeshNode`
/// (which gossips + ingests) and the `Session` (which dials from it).
///
/// Presence is a dial *hint*, not a trust claim — a forged record only costs a wasted
/// dial, since the manifest + signed live-edge still gate what a peer is trusted to serve.
#[derive(Clone, Default)]
pub struct PresenceBook {
    inner: Arc<Mutex<HashMap<PeerId, PresenceRecord>>>,
}

impl PresenceBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record/refresh one peer's presence (latest write wins).
    pub fn insert(&self, rec: PresenceRecord) {
        self.inner.lock().unwrap().insert(PeerId(rec.peer_id), rec);
    }

    /// Merge gossiped records, skipping our own entry (`me`).
    pub fn merge(&self, recs: impl IntoIterator<Item = PresenceRecord>, me: &PeerId) {
        let mut g = self.inner.lock().unwrap();
        for rec in recs {
            if PeerId(rec.peer_id) != *me {
                g.insert(PeerId(rec.peer_id), rec);
            }
        }
    }

    /// Every known record (for the session's discovery merge).
    pub fn snapshot(&self) -> Vec<PresenceRecord> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Up to `max` records to gossip onward, relay-capable peers first so reachable
    /// volunteers propagate fastest (bounds per-message size at scale).
    pub fn sample(&self, max: usize) -> Vec<PresenceRecord> {
        let mut v: Vec<PresenceRecord> = self.inner.lock().unwrap().values().cloned().collect();
        v.sort_by_key(|r| !r.relay);
        v.truncate(max);
        v
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 32-byte topic hash = `BLAKE2b-256(..)` (sharded discovery, TECH_SPEC §7.2).
pub type TopicId = [u8; 32];

/// The SDP-over-statement messages — carried via the app's chat codec with a
/// `STREAM_MESH` purpose (see [`crate::chat_codec`]). `offer_id` links an answer/
/// candidate/closed back to its offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalMsg {
    Offer { sdp: Vec<u8> },
    Answer { offer_id: String, sdp: Vec<u8> },
    IceCandidate { offer_id: String, sdp: Vec<u8> },
    Closed { offer_id: String },
}

/// The publisher's signed current-segment pointer (TECH_SPEC §6.4).
#[derive(Clone, Debug)]
pub struct LiveEdge {
    pub head_seq: u64,
    pub segment_seqs: Vec<u64>,
}

/// Handle to a live-edge subscription. Concrete stream wiring lands with the live path.
pub struct Subscription<T> {
    _marker: PhantomData<T>,
}

impl<T> Default for Subscription<T> {
    fn default() -> Self {
        Self { _marker: PhantomData }
    }
}

pub trait Signaling: Send + Sync {
    fn publish_presence(&self, p: Presence) -> BoxFuture<'static, crate::Result<()>>;
    fn read_presence(&self, topic: TopicId, max: usize) -> BoxFuture<'static, crate::Result<Vec<Presence>>>;
    fn send_signal(&self, to: PeerId, msg: SignalMsg) -> BoxFuture<'static, crate::Result<()>>;
    fn subscribe_edge(&self, stream: StreamId) -> Subscription<LiveEdge>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_record_roundtrips_manifest_cid_and_relay() {
        // The discovery wire format must preserve the M2 manifest CID + the M4 relay
        // flag through a SCALE round-trip (the chain publishes/reads via PresenceRecord).
        let rec = PresenceRecord {
            peer_id: [7u8; 32],
            caps_upload_bps: 20_000_000,
            ttl_s: 30,
            manifest_cid: Some("bafy-manifest-cid".into()),
            relay: true,
        };
        let bytes = rec.encode();
        assert_eq!(PresenceRecord::decode(&mut &bytes[..]).unwrap(), rec);

        // And the None / non-relay shape round-trips too.
        let plain = PresenceRecord { manifest_cid: None, relay: false, ..rec };
        assert_eq!(PresenceRecord::decode(&mut &plain.encode()[..]).unwrap(), plain);
    }

    #[test]
    fn presence_record_converts_to_and_from_presence() {
        let p = Presence {
            peer_id: PeerId([9u8; 32]),
            caps_upload_bps: 5,
            ttl_s: 30,
            manifest_cid: Some("cid".into()),
            relay: true,
        };
        let rec = PresenceRecord::from(&p);
        let back: Presence = rec.into();
        assert_eq!(back, p);
    }

    fn rec(id: u8, relay: bool) -> PresenceRecord {
        PresenceRecord { peer_id: [id; 32], caps_upload_bps: 1, ttl_s: 30, manifest_cid: None, relay }
    }

    #[test]
    fn presence_book_insert_merge_sample_len() {
        let book = PresenceBook::new();
        assert!(book.is_empty());
        book.insert(rec(1, false));
        book.insert(rec(2, true));
        assert_eq!(book.len(), 2);
        assert!(!book.is_empty());

        // merge skips our own entry, adds new peers, and latest-write-wins on a dup.
        let me = PeerId([0u8; 32]);
        book.merge(vec![rec(0, true), rec(3, false), rec(1, true)], &me);
        assert_eq!(book.len(), 3, "peer 0 (== me) skipped; 3 added; 1 updated");
        assert_eq!(book.snapshot().len(), 3);

        // sample(2) returns relay-capable peers first (1 was just updated to relay, 2 is relay).
        let s = book.sample(2);
        assert_eq!(s.len(), 2);
        assert!(s.iter().all(|r| r.relay), "relay-capable peers are sampled first");
    }

    #[test]
    fn subscription_and_live_edge_construct() {
        let _sub: Subscription<LiveEdge> = Subscription::default();
        let e = LiveEdge { head_seq: 5, segment_seqs: vec![1, 2, 3] };
        assert_eq!((e.head_seq, e.segment_seqs.len()), (5, 3));
    }
}
