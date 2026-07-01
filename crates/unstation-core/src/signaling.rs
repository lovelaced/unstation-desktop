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
use std::time::{Duration, Instant};

/// Presence records are dial hints gossiped by arbitrary peers — cap what one message
/// can grow the book by, how large the book gets, and how long an entry stays alive
/// (its own `ttl_s`, clamped so a forged record can't pin itself for hours).
const PRESENCE_BOOK_MAX: usize = 1024;
const PRESENCE_MERGE_MAX: usize = 32;
const PRESENCE_CID_MAX: usize = 128;
const PRESENCE_TTL_CLAMP_S: (u64, u64) = (5, 300);

fn record_ttl(rec: &PresenceRecord) -> Duration {
    Duration::from_secs((rec.ttl_s as u64).clamp(PRESENCE_TTL_CLAMP_S.0, PRESENCE_TTL_CLAMP_S.1))
}

fn is_live(entry: &(PresenceRecord, Instant)) -> bool {
    entry.1.elapsed() < record_ttl(&entry.0)
}

/// A presence announcement published to the (sharded) discovery topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    /// Per-**device** mesh routing id (signaling address, dial target, self-filter).
    /// NOT a signing key and NOT the personhood key: two devices of the SAME person
    /// share a personhood/statement-store key but MUST have distinct `peer_id`s, else
    /// each filters the other out of discovery as "self" (the bug that made cross-machine
    /// watch impossible). The personhood key lives in `publisher`.
    pub peer_id: PeerId,
    /// The publisher's **personhood** (statement-store) public key — the trust anchor a
    /// viewer verifies the signed manifest + gossiped live-edge against. Stable across a
    /// person's devices (the `peer_id` is not). For plain viewers this equals their own
    /// personhood key and is unused by the dial trust gate (they carry no manifest).
    pub publisher: [u8; 32],
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
    /// Bulletin CID of the publisher's signed manifest (M2). A viewer fetches +
    /// verifies it against `publisher` (the publisher's sr25519 personhood pubkey)
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
    /// Personhood/statement-store pubkey — trust anchor (see [`Presence::publisher`]).
    pub publisher: [u8; 32],
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
    pub manifest_cid: Option<String>,
    pub relay: bool,
}

impl From<&Presence> for PresenceRecord {
    fn from(p: &Presence) -> Self {
        Self {
            peer_id: p.peer_id.0,
            publisher: p.publisher,
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
            publisher: r.publisher,
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
    inner: Arc<Mutex<HashMap<PeerId, (PresenceRecord, Instant)>>>,
}

impl PresenceBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Poison-tolerant lock: the book is shared across the node loop and session tasks,
    /// and a panic elsewhere must not cascade into every reader/writer of a plain map.
    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, (PresenceRecord, Instant)>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Validate + stamp + store one record, holding the book at its cap: expired
    /// entries go first, then the stalest — never silently past `PRESENCE_BOOK_MAX`.
    fn admit(g: &mut HashMap<PeerId, (PresenceRecord, Instant)>, rec: PresenceRecord) {
        if rec.manifest_cid.as_ref().map_or(false, |c| c.len() > PRESENCE_CID_MAX) {
            return; // oversized CID — hostile allocation bait, not a dial hint
        }
        let key = PeerId(rec.peer_id);
        if !g.contains_key(&key) && g.len() >= PRESENCE_BOOK_MAX {
            g.retain(|_, e| is_live(e));
        }
        if !g.contains_key(&key) && g.len() >= PRESENCE_BOOK_MAX {
            if let Some(stalest) = g.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k) {
                g.remove(&stalest);
            }
        }
        g.insert(key, (rec, Instant::now()));
    }

    /// Record/refresh one peer's presence (latest write wins).
    pub fn insert(&self, rec: PresenceRecord) {
        Self::admit(&mut self.guard(), rec);
    }

    /// Merge gossiped records, skipping our own entry (`me`). At most
    /// `PRESENCE_MERGE_MAX` records are taken per call — one message can't flood the book.
    pub fn merge(&self, recs: impl IntoIterator<Item = PresenceRecord>, me: &PeerId) {
        let mut g = self.guard();
        for rec in recs.into_iter().take(PRESENCE_MERGE_MAX) {
            if PeerId(rec.peer_id) != *me {
                Self::admit(&mut g, rec);
            }
        }
    }

    /// Every live (non-expired) record, for the session's discovery merge.
    pub fn snapshot(&self) -> Vec<PresenceRecord> {
        self.guard().values().filter(|e| is_live(e)).map(|(r, _)| r.clone()).collect()
    }

    /// Up to `max` live records to gossip onward, relay-capable peers first so reachable
    /// volunteers propagate fastest (bounds per-message size at scale).
    pub fn sample(&self, max: usize) -> Vec<PresenceRecord> {
        let mut v = self.snapshot();
        v.sort_by_key(|r| !r.relay);
        v.truncate(max);
        v
    }

    pub fn len(&self) -> usize {
        self.guard().values().filter(|e| is_live(e)).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Ban duration + set cap. Bans are temporal (a routing PeerId is ephemeral by design,
/// so damnation-forever only wastes memory) and the set is capped so a Sybil churning
/// through identities can't grow it without bound.
const BAN_TTL: Duration = Duration::from_secs(600);
const BAN_LIST_MAX: usize = 1024;

/// Peers convicted of forgery/abuse by the `MeshNode` (which watches the bytes),
/// shared with the `Session` (which must stop dialing them and refuse their offers).
/// Same clone-handle pattern as [`PresenceBook`].
#[derive(Clone, Default)]
pub struct BanList {
    inner: Arc<Mutex<HashMap<PeerId, Instant>>>,
}

impl BanList {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, Instant>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn ban(&self, peer: PeerId) {
        let mut g = self.guard();
        if !g.contains_key(&peer) && g.len() >= BAN_LIST_MAX {
            g.retain(|_, t| t.elapsed() < BAN_TTL);
        }
        if !g.contains_key(&peer) && g.len() >= BAN_LIST_MAX {
            if let Some(oldest) = g.iter().min_by_key(|(_, t)| *t).map(|(k, _)| *k) {
                g.remove(&oldest);
            }
        }
        g.insert(peer, Instant::now());
    }

    pub fn contains(&self, peer: &PeerId) -> bool {
        self.guard().get(peer).map_or(false, |t| t.elapsed() < BAN_TTL)
    }

    pub fn len(&self) -> usize {
        self.guard().values().filter(|t| t.elapsed() < BAN_TTL).count()
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
            publisher: [8u8; 32],
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
            publisher: [10u8; 32],
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
        PresenceRecord { peer_id: [id; 32], publisher: [id; 32], caps_upload_bps: 1, ttl_s: 30, manifest_cid: None, relay }
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
    fn merge_is_capped_and_validates_records() {
        let book = PresenceBook::new();
        let me = PeerId([255u8; 32]);
        // 40 gossiped records in one message → only PRESENCE_MERGE_MAX admitted.
        let batch: Vec<PresenceRecord> = (1..=40u8).map(|i| rec(i, false)).collect();
        book.merge(batch, &me);
        assert_eq!(book.len(), PRESENCE_MERGE_MAX, "one message can't flood the book");

        // An oversized manifest CID is allocation bait, not a dial hint.
        let mut bad = rec(200, true);
        bad.manifest_cid = Some("x".repeat(PRESENCE_CID_MAX + 1));
        book.insert(bad);
        assert_eq!(book.len(), PRESENCE_MERGE_MAX, "oversized CID rejected");
    }

    #[test]
    fn book_evicts_at_the_cap_instead_of_growing() {
        let book = PresenceBook::new();
        for i in 0..(PRESENCE_BOOK_MAX + 50) {
            // Distinct peer ids across the u8 range boundary.
            let mut id = [0u8; 32];
            id[0] = (i % 256) as u8;
            id[1] = (i / 256) as u8;
            book.insert(PresenceRecord {
                peer_id: id,
                publisher: id,
                caps_upload_bps: 1,
                ttl_s: 30,
                manifest_cid: None,
                relay: false,
            });
        }
        assert!(book.len() <= PRESENCE_BOOK_MAX, "cap held: {}", book.len());
    }

    #[test]
    fn subscription_and_live_edge_construct() {
        let _sub: Subscription<LiveEdge> = Subscription::default();
        let e = LiveEdge { head_seq: 5, segment_seqs: vec![1, 2, 3] };
        assert_eq!((e.head_seq, e.segment_seqs.len()), (5, 3));
    }
}
