//! `MeshNode` — the single-actor async loop that drives the engine over a real
//! transport.
//!
//! One task owns all mutable state and consumes [`EngineEvent`]s off one mpsc
//! channel (no locks on the hot path). It serves `Want`s from the store as 16 KiB
//! `SegmentData` chunks, reassembles + hash-verifies inbound segments, feeds the
//! [`MediaSink`], and each tick runs the picker to issue new `Want`s.
//!
//! The `segment_ids` map (seq → content id) is the authenticated availability
//! learned from the signed manifest/live-edge; here it's provided up front
//! (live-edge propagation is D3). Verification is always against it.

use crate::buffermap::BufferMap;
use crate::config::{MeshConfig, Mode, Role};
use crate::media::MediaSink;
use crate::peer::PeerState;
use crate::picker::Source;
use crate::protocol::{Caps, MeshMsg};
use crate::reassembly::Reassembler;
use crate::signaling::{BanList, PresenceBook};
use crate::store::SegmentLocation;
use crate::transport::{Channel, EngineEvent, Link};
use crate::types::{PeerId, SegmentId, Seq};
use crate::{crypto, engine::MeshEngine};
use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};
use rand_chacha::ChaCha8Rng;
use rand::SeedableRng;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// 16 KiB — the safe cross-platform SCTP message size (TECH_SPEC §6.3).
const CHUNK: usize = 16 * 1024;
/// Re-request a segment if the peer hasn't fully delivered it within this window.
/// The bulk channel is unreliable (no retransmits), so a single lost chunk would
/// otherwise pin the seq in `pending` forever and freeze playback.
const PENDING_TIMEOUT_MS: u64 = 2_000;
/// RTT-probe cadence (Ping/Pong) — measured RTT feeds the picker's peer ranking.
const PING_INTERVAL_MS: u64 = 1_000;
/// Buffer-map advertise cadence — on change, else at most this often (vs every tick).
const BUFFERMAP_INTERVAL_MS: u64 = 500;
/// Reject absurd segment sizes from a hostile peer before allocating a reassembler.
const MAX_SEGMENT_BYTES: u32 = 4 * 1024 * 1024;
/// Upload fairness (TECH_SPEC §8.5). A viewer serves at most this many peers at once
/// (its "regular" unchoke slots), rewarding the best reciprocators, plus one rotating
/// optimistic slot for newcomers. Publishers/seeds don't choke — the origin is generous
/// so the swarm can bootstrap. Re-evaluated every `CHOKE_INTERVAL_MS`.
const UPLOAD_SLOTS: usize = 4;
const CHOKE_INTERVAL_MS: u64 = 5_000;
/// Push-pull (TECH_SPEC §6.4): a peer may PUSH a segment to us before our live edge has
/// told us its authenticated id (the chain edge-poll races the direct WebRTC bytes). We
/// hold such bytes until the id lands, then verify + deliver — but only up to this many
/// bytes total, so an early (or hostile) push can't grow memory without bound. Beyond it,
/// the push is dropped and the segment is pulled normally instead.
const MAX_PENDING_VERIFY_BYTES: u64 = 8 * 1024 * 1024;
/// Off-chain signaling (TECH_SPEC §6.4): remember this many recent edge seqs so a
/// gossiped `EdgeAnnounce` is applied + re-broadcast exactly once (loop/storm suppression)
/// while still tracking the live window. Pruned below `head - this` each tick.
const EDGE_SEEN_KEEP: Seq = 1024;
/// Presence-gossip cadence + per-message cap (TECH_SPEC §7.3, off-chain signaling): how
/// often we share our known-peer directory, and how many records we put in one message
/// (bounds bandwidth — tiny vs video, but capped anyway).
const PRESENCE_GOSSIP_INTERVAL_MS: u64 = 3_000;
const PRESENCE_GOSSIP_MAX: usize = 32;
/// Domain tag prefixed to the bytes a publisher signs for a live-edge gossip. Both
/// `MANIFEST_CONTEXT` (the sr25519 signing context) and this tag separate an edge
/// signature from a manifest signature, so neither can be replayed as the other.
const EDGE_SIGN_TAG: &[u8] = b"unstation-edge-v1";
/// Hostile-input bounds. Everything a peer can send that allocates on our side is
/// capped: reassembly (entries + total buffered bytes + a completion TTL), gossip
/// batch sizes, bitfield size, and the known-peer set. Violating messages are dropped.
const MAX_REASM: usize = 64;
const MAX_REASM_BYTES: u64 = 32 * 1024 * 1024;
/// Evict a reassembler that never completed within this window — a *pushed* segment
/// with a lost chunk has no `pending` entry, so the pending sweep can't reclaim it.
const REASM_TTL_MS: u64 = 2 * PENDING_TIMEOUT_MS;
/// How far ahead of our known head an unsolicited push is admitted (push-pull runs a
/// hop or two ahead of the verified edge; anything further out is a spray).
const PUSH_AHEAD: Seq = 16;
const MAX_WANT_SEQS: usize = 32;
/// Paced serving (TECH_SPEC §8.5). A `Want` enqueues onto the peer's `serve_queue`
/// and each tick drains at most this many bytes to that peer. Serving a whole
/// catch-up window inline the instant a viewer connects blasts several MB into a
/// just-connected SCTP association (still in slow-start) — its send buffer overruns
/// and the association resets (`SCTP disconnected` ~1 s after join on a 6 Mbps
/// stream; the low-bitrate RTMP path only dodged it because its segments were ~10×
/// smaller). At the 100 ms production tick this caps a single peer near ~20 Mbps —
/// far above any stream's bitrate so catch-up and live-follow stay fast — while
/// keeping the in-flight burst well under the transport's 1 MiB bulk-buffer drop
/// threshold, so we pace the connection instead of drowning it.
const SERVE_BYTES_PER_TICK: usize = 256 * 1024;
/// Cap a peer's pending serve backlog: dedup keeps it near the live window, but bound
/// it so a peer spamming `Want`s can't grow it without limit.
const SERVE_QUEUE_MAX: usize = 64;
const MAX_GOSSIP_PEERS: usize = 256;
const MAX_GOSSIP_RECORDS: usize = 64;
/// 8 KiB of bitfield = a 64k-segment window — far beyond any honest live window.
const MAX_BITFIELD_BYTES: usize = 8 * 1024;
/// `known_peers` is a stat + gossip seed, not a routing table — cap it.
const KNOWN_PEERS_MAX: usize = 4096;
/// Re-gossip the live window's signed edges this often. An `EdgeAnnounce` used to be
/// sent exactly ONCE (at produce time); on a lossy control path a lost announce meant a
/// viewer never learned that segment's id — its bytes reassembled fine but parked
/// unverifiable forever while the picker re-fetched them in a loop (found by the netsim
/// churn-under-loss scenario; the chain edge poll masks it only when a chain is
/// reachable). Re-announcing is idempotent (receivers dedup via `edge_seen`, relays
/// forward only what's new to them) and costs ~window × ~110 B per link per interval.
const EDGE_REANNOUNCE_MS: u64 = 1_000;
/// Reputation floor: crossing it bans the peer (choke + disconnect + BanList).
/// 0.05 ≈ five forged segments (0.5⁵), or a long run of timeouts/abuse.
const REPUTATION_FLOOR: f64 = 0.05;
/// Verified deliveries slowly heal reputation, so an honest peer on a lossy link
/// (whose timeouts are genuine) recovers instead of drifting toward the floor.
const REPUTATION_HEAL: f64 = 0.02;
/// A timeout is ambiguous — a dying/lying link OR just packet loss on an honest one. On
/// the unreliable bulk channel a single lost 16 KiB chunk fails a whole segment, so an
/// honest peer on a lossy link racks up timeouts fast; multiplicative `×0.8` decay would
/// otherwise cross [`REPUTATION_FLOOR`] and get it 600 s SHARED-banned (and never
/// re-dialed) purely for a bad link — the join-time "discover → 0 candidates" churn we
/// saw on-device. So timeout decay is CLAMPED here: it still deprioritizes the peer (the
/// picker divides expected delivery time by reputation, clamped to 0.1), but only
/// definitive misbehavior — forged bytes ([`Penalty::HashFail`]) or protocol abuse —
/// reaches the ban floor below this. A genuinely dead/lying peer just delivers nothing,
/// so the picker routes around it via throughput ranking; it doesn't need banning.
const TIMEOUT_REP_FLOOR: f64 = 0.1;

/// Why a peer is being penalized. Each maps to a decay factor commensurate with how
/// strong the evidence of misbehavior is — forged bytes are proof, a timeout might
/// just be loss, an oversized message is usually a hostile probe.
#[derive(Clone, Copy, Debug)]
enum Penalty {
    /// Delivered bytes hash-failed against a KNOWN authenticated id.
    HashFail,
    /// An accepted request never completed (buffer-map lie, or a dying link).
    Timeout,
    /// A message violated protocol bounds (floods, oversize, unrequested spray).
    ProtocolAbuse,
}

impl Penalty {
    fn factor(self) -> f64 {
        match self {
            Penalty::HashFail => 0.5,
            Penalty::Timeout => 0.8,
            Penalty::ProtocolAbuse => 0.9,
        }
    }
}

/// Builds the exact bytes a publisher signs (and a viewer verifies) for one live-edge
/// entry: tag ‖ stream ‖ seq ‖ content-id. A free fn so the sign + verify paths can't
/// drift apart.
fn edge_payload(stream_id: &[u8; 32], seq: Seq, id: &SegmentId) -> Vec<u8> {
    let mut p = Vec::with_capacity(EDGE_SIGN_TAG.len() + 32 + 8 + 32);
    p.extend_from_slice(EDGE_SIGN_TAG);
    p.extend_from_slice(stream_id);
    seq.encode_to(&mut p);
    p.extend_from_slice(&id.0);
    p
}

/// Signs live-edge announcements with the publisher's identity key (sr25519). Injected
/// by the session so the secret stays in the chain layer; the node only holds this handle
/// and the public key the verification side checks against.
pub trait EdgeSigner: Send + Sync {
    fn sign(&self, payload: &[u8]) -> [u8; 64];
}

/// Async fetch from the durable floor (TECH_SPEC §8.6) — Bulletin, or any other
/// origin the app wires up: given a deadline-missing segment's seq and authenticated
/// content id, resolve its bytes (or `None`). Injected by the app layer so the engine
/// stays chain-free; the node re-verifies whatever comes back before accepting it.
pub type FallbackFetch =
    Arc<dyn Fn(Seq, SegmentId) -> crate::BoxFuture<'static, Option<Bytes>> + Send + Sync>;

/// Concurrent durable-floor fetches — a panic-zone burst must not stampede the chain.
const MAX_FALLBACK_INFLIGHT: usize = 3;

/// Choose which peers to unchoke (serve) this round — the pure core of the upload-slot
/// manager, separated for testability. Viewers reward reciprocation (rank by the
/// throughput a peer has given US); seeds/publishers spread by proximity (lowest RTT).
/// One extra "optimistic" slot rotates among the remaining peers so newcomers get a
/// chance to prove themselves. `peers` is `(id, throughput_bps_from_them, rtt_ms)`.
fn select_unchokes(
    peers: &[(PeerId, f64, f64)],
    role: Role,
    slots: usize,
    optimistic_rr: u64,
) -> HashSet<PeerId> {
    let mut ranked: Vec<&(PeerId, f64, f64)> = peers.iter().collect();
    match role {
        // Tit-for-tat: best reciprocators first (highest throughput they've given us).
        Role::Viewer => ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal)),
        // No reciprocation to measure — spread to the closest peers (fastest to reshare).
        Role::Publisher | Role::Seed => {
            ranked.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(Ordering::Equal))
        }
    }
    let mut set: HashSet<PeerId> = ranked.iter().take(slots).map(|p| p.0).collect();
    // Optimistic unchoke: rotate one slot among the peers not already chosen.
    let rest: Vec<PeerId> = ranked.iter().skip(slots).map(|p| p.0).collect();
    if !rest.is_empty() {
        set.insert(rest[(optimistic_rr as usize) % rest.len()]);
    }
    set
}

/// Chunk + frame a whole segment onto a link's bulk channel. A free function so the
/// off-loop disk-serve path (`spawn_blocking`) can use the exact same wire framing
/// as the in-loop memory path — `Link::send` is a channel post, safe from any thread.
fn send_segment_on(link: &Arc<dyn Link>, seq: Seq, bytes: &[u8]) {
    let total = bytes.len() as u32;
    let mut offset = 0u32;
    for chunk in bytes.chunks(CHUNK) {
        // Frame manually so the chunk lands in the output buffer ONCE. The naive
        // `SegmentData { bytes: chunk.to_vec() }.encode()` copied every byte twice
        // (into the Vec, then into the encode buffer) on the busiest path — serving.
        link.send(Channel::Bulk, frame_segment_data(seq, total, offset, chunk));
        offset += chunk.len() as u32;
    }
}

/// Manually SCALE-frame a `MeshMsg::SegmentData` chunk so the payload is copied ONCE
/// (straight into the output buffer) instead of twice (`chunk.to_vec()` then
/// `encode()`). The `frame_segment_data_matches_derive` test pins this byte-for-byte
/// against the derived encoding, so a future change to the `MeshMsg` layout can't
/// silently drift the hand-rolled framing.
fn frame_segment_data(seq: Seq, total_len: u32, offset: u32, chunk: &[u8]) -> Vec<u8> {
    use parity_scale_codec::Compact;
    /// `MeshMsg` variant index of `SegmentData` (Hello, BufferMap, Want, Have, SegmentData, …).
    const SEGMENT_DATA_TAG: u8 = 4;
    let mut out = Vec::with_capacity(1 + 8 + 2 + 4 + 4 + 5 + chunk.len());
    out.push(SEGMENT_DATA_TAG);
    seq.encode_to(&mut out);
    0u16.encode_to(&mut out); // track_id
    total_len.encode_to(&mut out);
    offset.encode_to(&mut out);
    Compact(chunk.len() as u32).encode_to(&mut out);
    out.extend_from_slice(chunk);
    out
}

#[derive(Debug, Default, Clone)]
pub struct NodeStats {
    pub delivered: usize,
    pub peer_bytes: u64,
    /// Segment bytes served TO peers (a publisher's uplink contribution — the
    /// dashboard's real "you're carrying N kbps" number). The off-loop disk-serve
    /// path isn't counted; the app's live nodes are memory-only, so nothing is missed.
    pub sent_bytes: u64,
    /// Segments delivered by the durable floor (TECH_SPEC §8.6) rather than a peer —
    /// the UI's honest "leaning on the backup copy" signal.
    pub from_origin: usize,
    pub hash_failures: u64,
    /// Live-edge lag in seconds: `(head_seq − play_seq) × seg_ms` — how far behind
    /// the newest known segment playback currently is. The real number the UI's
    /// latency display and "skip to live" affordance hang off.
    pub latency_s: f64,
    pub head_seq: Seq,
    pub play_seq: Seq,
    /// Leak/bound gauges, snapshotted when the loop exits: the adversarial + churn
    /// suites assert these stay inside their caps and drain to zero after churn.
    pub reasm_entries: usize,
    pub reasm_bytes: u64,
    pub pending_entries: usize,
    /// Connected neighbors in the peer table.
    pub peers: usize,
    /// Peers learned via in-mesh `PeerGossip` (no statement-store writes).
    pub known_peers: usize,
}

/// A media sink that drops everything — for seed/relay nodes that cache + reshare
/// segments but never play them.
struct DiscardSink;
impl MediaSink for DiscardSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

/// One in-progress reassembly, keyed by `(sender, seq)` — per-sender so a hostile
/// peer's chunks can only ever poison a segment *it* is delivering, never a slot an
/// honest peer is filling (cross-peer injection would hash-fail the honest peer's
/// bytes and decay the wrong reputation).
struct ReasmEntry {
    r: Reassembler,
    /// `now_ms` when the first chunk arrived — for the completion TTL sweep.
    created_ms: u64,
}

pub struct MeshNode {
    me: PeerId,
    eng: MeshEngine,
    sink: Arc<dyn MediaSink>,
    links: HashMap<PeerId, Arc<dyn Link>>,
    reasm: HashMap<(PeerId, Seq), ReasmEntry>,
    /// Total bytes buffered across `reasm` — bounded by [`MAX_REASM_BYTES`].
    reasm_bytes: u64,
    segment_ids: HashMap<Seq, SegmentId>,
    /// Off-chain signaling (TECH_SPEC §6.4). `stream_id` binds edge signatures to this
    /// stream; `edge_signer` (publisher only) signs each new edge; `publisher_key` (viewer
    /// only) is the trust anchor each gossiped edge is verified against — the SAME pubkey
    /// the signed manifest was checked against; `edge_seen` dedups gossip per seq.
    stream_id: [u8; 32],
    edge_signer: Option<Arc<dyn EdgeSigner>>,
    publisher_key: Option<[u8; 32]>,
    edge_seen: HashSet<Seq>,
    /// The publisher's recent signed edges (≤ window), re-gossiped every
    /// [`EDGE_REANNOUNCE_MS`] so a viewer that lost the one-shot announce still learns
    /// each segment's id (see the constant's rationale).
    recent_edges: std::collections::VecDeque<(Seq, SegmentId, [u8; 64])>,
    last_edge_regossip_ms: u64,
    /// Off-chain presence directory (TECH_SPEC §7.3), shared with the session: the node
    /// periodically gossips a sample of it to peers and merges what it receives, so the
    /// session can dial in-mesh-discovered peers without a per-viewer chain write.
    presence_book: Option<PresenceBook>,
    /// Shared with the session (see [`MeshNode::with_ban_list`]): convictions land
    /// here so the dial/accept edges stay closed to a banned peer.
    ban_list: Option<BanList>,
    last_presence_gossip_ms: u64,
    known_peers: HashSet<PeerId>,
    /// Push-pull (TECH_SPEC §6.4): peers that subscribed to our live edge. We PUSH each
    /// newly-available segment to them (subject to choke + budget) instead of waiting for
    /// their per-segment `Want`.
    subscribers: HashSet<PeerId>,
    /// Segments pushed to us ahead of our learning their authenticated id: held here
    /// (seq → who sent it + the raw bytes) until `LiveEdge` provides the id, then verified
    /// and delivered. Bounded by `pending_verify_bytes` ≤ [`MAX_PENDING_VERIFY_BYTES`].
    pending_verify: HashMap<Seq, (PeerId, Bytes)>,
    pending_verify_bytes: u64,
    /// In-flight segment requests: seq → (peer we asked, when we asked, in `now_ms`).
    pending: HashMap<Seq, (PeerId, u64)>,
    /// The panic-zone hedge (TECH_SPEC §8.4): ONE additional concurrent request per seq,
    /// to a DIFFERENT peer than `pending`'s primary. The picker plans dual-holder fetches
    /// for deadline-critical segments (`Request::redundant`); realizing them here means a
    /// slow/dead primary no longer stalls the segment until the 2 s pending timeout —
    /// whichever peer lands first wins, and delivery clears both entries.
    hedge: HashMap<Seq, (PeerId, u64)>,
    rng: ChaCha8Rng,
    now_ms: u64,
    last_ping_ms: u64,
    last_bm_ms: u64,
    last_bm_count: usize,
    last_choke_ms: u64,
    optimistic_rr: u64,
    /// Upload rate cap (TECH_SPEC §8.5): a token bucket in bytes, refilled at
    /// `upload_budget_bps`. We serve a segment only if we hold enough tokens, so a
    /// node never uploads faster than its declared budget. 0 budget = unmetered.
    upload_tokens: f64,
    last_token_ms: u64,
    /// Monotonic count of segments this node has obtained (produced or delivered).
    /// The local buffer map's `count()` no longer works for this: a live viewer
    /// prunes its window as playback advances, so that count *shrinks*.
    delivered_total: usize,
    /// Optional live stats feed (see [`MeshNode::with_stats`]).
    stats_tx: Option<tokio::sync::watch::Sender<NodeStats>>,
    last_stats_ms: u64,
    /// Durable-floor fetch hook (see [`MeshNode::with_fallback`]) + its in-flight set
    /// and the channel completed fetches re-enter the actor loop through.
    fallback: Option<FallbackFetch>,
    fallback_inflight: HashSet<Seq>,
    fallback_tx: tokio::sync::mpsc::UnboundedSender<(Seq, SegmentId, Option<Bytes>)>,
    fallback_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(Seq, SegmentId, Option<Bytes>)>>,
    /// Floor of the last live-window prune, so the per-tick prune only does work
    /// (store + id map + buffer map) when the play head actually advanced.
    last_prune_floor: Seq,
    stats: NodeStats,
    /// The stream's init segment (CMAF `ftyp`+`moov`). A publisher receives it via
    /// [`EngineEvent::InitSegment`]; a viewer fills it from a peer's `InitData` (and then
    /// installs it into its sink + can reshare it). Serving this over the mesh decouples
    /// playback bootstrap from the Bulletin gateway. `None` until known.
    init: Option<Bytes>,
}

impl MeshNode {
    /// A viewer that already knows the authenticated seq→id map (from the live edge)
    /// and the current head; it fetches from peers only (no seed/Bulletin in D2).
    pub fn new_viewer(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
        segment_ids: HashMap<Seq, SegmentId>,
        head_seq: Seq,
    ) -> Self {
        let mut eng = MeshEngine::new(cfg, seg_bytes);
        eng.head_seq = head_seq;
        eng.seed_available = false;
        eng.bulletin_available = false;
        Self::with_engine(me, eng, sink, segment_ids)
    }

    /// A publisher preloaded with the full VOD (genesis seed). Serves `Want`s; its
    /// own picker finds nothing missing.
    pub fn new_publisher(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
        segments: Vec<Bytes>,
    ) -> Self {
        let mut eng = MeshEngine::new(cfg, seg_bytes);
        let mut segment_ids = HashMap::new();
        for (i, bytes) in segments.into_iter().enumerate() {
            let seq = i as Seq;
            let id = crypto::segment_id(&bytes);
            eng.store.insert(seq, id, bytes);
            eng.local.set(seq);
            segment_ids.insert(seq, id);
        }
        eng.head_seq = segment_ids.len().saturating_sub(1) as Seq;
        Self::with_engine(me, eng, sink, segment_ids)
    }

    /// A publisher fed **live** (no preloaded VOD): segments arrive as
    /// [`EngineEvent::Produced`] from the segmenter, and the node serves them to
    /// the mesh as they land.
    pub fn new_live_publisher(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
    ) -> Self {
        Self::with_engine(me, MeshEngine::new(cfg, seg_bytes), sink, HashMap::new())
    }

    /// A seed / relay node (TECH_SPEC §8.5): it fetches a stream like a viewer — so it
    /// caches segments and can reshare them — but exists to OFFLOAD the origin, not to
    /// play. Pass a `Role::Seed` config with a generous `upload_budget_bps`; the node
    /// then never chokes (spreading by proximity) and discards media (no playback).
    pub fn new_seed(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        segment_ids: HashMap<Seq, SegmentId>,
        head_seq: Seq,
    ) -> Self {
        let mut eng = MeshEngine::new(cfg, seg_bytes);
        eng.head_seq = head_seq;
        eng.seed_available = false;
        eng.bulletin_available = false;
        Self::with_engine(me, eng, Arc::new(DiscardSink), segment_ids)
    }

    fn with_engine(
        me: PeerId,
        eng: MeshEngine,
        sink: Arc<dyn MediaSink>,
        segment_ids: HashMap<Seq, SegmentId>,
    ) -> Self {
        // Start the upload bucket full so a fresh node can serve immediately, then
        // refill at the budget rate. Burst = 0.5 s of budget (min two segments).
        let upload_tokens = if eng.cfg.upload_budget_bps > 0 {
            (eng.cfg.upload_budget_bps as f64 / 8.0 * 0.5).max(2.0 * eng.seg_bytes as f64)
        } else {
            0.0
        };
        let preloaded = eng.local.count(); // a genesis-seed publisher starts full
        let (fallback_tx, fallback_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            me,
            eng,
            sink,
            links: HashMap::new(),
            reasm: HashMap::new(),
            reasm_bytes: 0,
            segment_ids,
            stream_id: [0u8; 32],
            edge_signer: None,
            publisher_key: None,
            edge_seen: HashSet::new(),
            recent_edges: std::collections::VecDeque::new(),
            last_edge_regossip_ms: 0,
            presence_book: None,
            ban_list: None,
            last_presence_gossip_ms: 0,
            known_peers: HashSet::new(),
            subscribers: HashSet::new(),
            pending_verify: HashMap::new(),
            pending_verify_bytes: 0,
            pending: HashMap::new(),
            hedge: HashMap::new(),
            rng: ChaCha8Rng::seed_from_u64(me.0[0] as u64 + 1),
            now_ms: 0,
            last_ping_ms: 0,
            last_bm_ms: 0,
            last_bm_count: usize::MAX, // force the first buffer-map advertise
            last_choke_ms: 0,
            optimistic_rr: 0,
            upload_tokens,
            last_token_ms: 0,
            delivered_total: preloaded,
            stats_tx: None,
            last_stats_ms: 0,
            fallback: None,
            fallback_inflight: HashSet::new(),
            fallback_tx,
            fallback_rx: Some(fallback_rx),
            last_prune_floor: 0,
            stats: NodeStats::default(),
            init: None,
        }
    }

    /// Bind this node to a stream id (used as the signing/verifying domain for live-edge
    /// gossip). Builder-style so existing constructors stay unchanged.
    pub fn with_stream_id(mut self, stream_id: [u8; 32]) -> Self {
        self.stream_id = stream_id;
        self
    }

    /// Publisher: attach the identity signer so each produced segment's edge is signed
    /// and gossiped in-mesh (off-chain signaling, TECH_SPEC §6.4).
    pub fn with_edge_signer(mut self, signer: Arc<dyn EdgeSigner>) -> Self {
        self.edge_signer = Some(signer);
        self
    }

    /// Viewer: the publisher pubkey every gossiped edge is verified against (the same
    /// trust anchor used for the signed manifest). Without it, gossiped edges are ignored.
    pub fn with_publisher_key(mut self, key: [u8; 32]) -> Self {
        self.publisher_key = Some(key);
        self
    }

    /// Share the off-chain presence directory (TECH_SPEC §7.3) with the session: the node
    /// gossips a sample of it to peers + merges what it receives, so the session can dial
    /// in-mesh-discovered peers instead of every viewer writing presence to the chain.
    pub fn with_presence_book(mut self, book: PresenceBook) -> Self {
        self.presence_book = Some(book);
        self
    }

    /// Share the session's ban list: the node convicts (reputation floor), the
    /// session enforces at the edges (no re-dial, offers refused) — without this a
    /// banned peer just reconnects under the same id with a fresh `PeerState`.
    pub fn with_ban_list(mut self, bans: BanList) -> Self {
        self.ban_list = Some(bans);
        self
    }

    /// Publish a [`NodeStats`] snapshot once a second while running (a `watch`
    /// channel: readers always see the latest, no backpressure on the loop). This
    /// is where the UI's REAL numbers come from — peer count, delivered segments,
    /// live-edge lag — instead of fabricated placeholders.
    pub fn with_stats(mut self, tx: tokio::sync::watch::Sender<NodeStats>) -> Self {
        self.stats_tx = Some(tx);
        self
    }

    /// Wire the durable floor (TECH_SPEC §8.6): when the picker's panic zone finds no
    /// peer able to meet a deadline, the node fetches the segment through this hook
    /// instead of stalling. Flips the picker's `bulletin_available` escalation on.
    pub fn with_fallback(mut self, fetch: FallbackFetch) -> Self {
        self.fallback = Some(fetch);
        self.eng.bulletin_available = true;
        self
    }

    /// Re-evaluate upload slots (TECH_SPEC §8.5) and send Choke/Unchoke on transitions.
    /// Publishers/seeds never choke (the origin stays generous so the swarm can
    /// bootstrap); only viewers ration their upload across slots.
    fn recompute_unchokes(&mut self) {
        self.last_choke_ms = self.now_ms;
        if !matches!(self.eng.cfg.role, Role::Viewer) {
            return; // origin stays open; peers keep their default `choked = false`.
        }
        self.optimistic_rr = self.optimistic_rr.wrapping_add(1);
        let stats: Vec<(PeerId, f64, f64)> = self
            .eng
            .peers
            .iter()
            .map(|(id, p)| (*id, p.throughput_bps.or(0.0), p.rtt_ms.or(f64::MAX)))
            .collect();
        let unchoked = select_unchokes(&stats, Role::Viewer, UPLOAD_SLOTS, self.optimistic_rr);
        let (mut to_choke, mut to_unchoke) = (Vec::new(), Vec::new());
        for (id, p) in self.eng.peers.iter_mut() {
            let want = unchoked.contains(id);
            if want && p.choked {
                p.choked = false;
                to_unchoke.push(*id);
            } else if !want && !p.choked {
                p.choked = true;
                to_choke.push(*id);
            }
        }
        for id in to_unchoke {
            if let Some(l) = self.links.get(&id) {
                l.send(Channel::Ctrl, MeshMsg::Unchoke.encode());
            }
        }
        for id in to_choke {
            if let Some(l) = self.links.get(&id) {
                l.send(Channel::Ctrl, MeshMsg::Choke.encode());
            }
        }
    }

    /// Run the actor loop until `Stop`, the inbox closes, or (for a viewer)
    /// `stop_at_count` segments are held locally.
    pub async fn run(
        mut self,
        mut inbox: UnboundedReceiver<EngineEvent>,
        tick: Duration,
        stop_at_count: Option<usize>,
    ) -> NodeStats {
        let tick_ms = tick.as_millis() as u64;
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Completed durable-floor fetches re-enter the actor loop through this channel
        // (the sender half stays on `self`, so `recv` never yields `None`).
        let mut fallback_rx = match self.fallback_rx.take() {
            Some(rx) => rx,
            None => tokio::sync::mpsc::unbounded_channel().1,
        };
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.now_ms += tick_ms;
                    self.on_tick();
                }
                ev = inbox.recv() => {
                    match ev {
                        Some(EngineEvent::Stop) | None => break,
                        Some(e) => self.on_event(e),
                    }
                }
                Some((seq, id, bytes)) = fallback_rx.recv() => self.on_fallback(seq, id, bytes),
            }
            if let Some(target) = stop_at_count {
                // Monotonic: a live viewer prunes its window, so `local.count()` shrinks.
                if self.delivered_total >= target {
                    break;
                }
            }
        }
        self.stats.delivered = self.delivered_total;
        self.stats.known_peers = self.known_peers.len();
        self.stats.peers = self.eng.peers.len();
        self.stats.reasm_entries = self.reasm.len();
        self.stats.reasm_bytes = self.reasm_bytes;
        self.stats.pending_entries = self.pending.len();
        self.stats
    }

    /// Test/simulation hooks for the deterministic impairment harness (`crate::netsim`).
    /// Gated so production never compiles them. They expose the otherwise-private step
    /// methods so a discrete-event driver can advance many nodes in lockstep on a virtual
    /// clock (instead of the wall-clock `run()` above): `on_event`/`on_tick` never touch a
    /// real timer, so stepping them by hand is bit-for-bit deterministic.
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        self.on_tick();
    }
    /// Deliver one event at the node's current virtual time — matches `run()`, where
    /// `on_event` does not advance the clock (only `sim_tick` does).
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_deliver(&mut self, ev: EngineEvent) {
        self.on_event(ev);
    }
    /// Snapshot the invariant-bearing stats without consuming the node (the fields `run()`
    /// fills on exit).
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_stats(&self) -> NodeStats {
        let mut s = self.stats.clone();
        s.delivered = self.delivered_total;
        s.known_peers = self.known_peers.len();
        s.peers = self.eng.peers.len();
        s.reasm_entries = self.reasm.len();
        s.reasm_bytes = self.reasm_bytes;
        s.pending_entries = self.pending.len();
        s
    }
    /// A peer's current reputation in `[0,1]`, or `None` if we don't know the peer.
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_reputation(&self, peer: &PeerId) -> Option<f64> {
        self.eng.peers.get(peer).map(|p| p.reputation)
    }
    /// Whether we've banned this peer (reputation crossed the floor).
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_banned(&self, peer: &PeerId) -> bool {
        self.eng.peers.get(peer).map(|p| p.banned).unwrap_or(false)
    }
    /// `(play_seq, head_seq, locally buffered count)` — the live-lag + retention view,
    /// for asserting snap-to-live convergence and the never-an-empty-playlist prune
    /// invariant after a partition heals.
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_window(&self) -> (Seq, Seq, usize) {
        (self.eng.play_seq, self.eng.head_seq, self.eng.local.count())
    }
    /// Debug view of one seq for scenario diagnosis: `(local.has, pending-to, hedge-to,
    /// [(peer, buffer.has, reputation, pending_bytes)])`.
    #[cfg(any(test, feature = "netsim"))]
    pub fn sim_seq_debug(
        &self,
        seq: Seq,
    ) -> (bool, Option<PeerId>, Option<PeerId>, Vec<(PeerId, bool, f64, u64)>) {
        (
            self.eng.local.has(seq),
            self.pending.get(&seq).map(|(p, _)| *p),
            self.hedge.get(&seq).map(|(p, _)| *p),
            self.eng
                .peers
                .values()
                .map(|p| (p.id, p.buffer.has(seq), p.reputation, p.pending_bytes))
                .collect(),
        )
    }

    fn on_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::PeerConnected { peer, link } => {
                // Defense in depth: the session refuses banned dials/offers, but a
                // connection already mid-handshake when the ban landed still arrives.
                if self.ban_list.as_ref().map_or(false, |b| b.contains(&peer)) {
                    link.close();
                    return;
                }
                self.eng.peers.entry(peer).or_insert_with(|| PeerState::new(peer));
                self.links.insert(peer, link.clone());
                self.send_hello(&link);
                // Push-pull (TECH_SPEC §6.4): register standing interest so this peer
                // pushes us its live edge. The publisher is the source — it subscribes to
                // no one (a relaying viewer, however, both subscribes upstream and serves
                // its own downstream subscribers).
                if !matches!(self.eng.cfg.role, Role::Publisher) {
                    link.send(Channel::Ctrl, MeshMsg::Subscribe.encode());
                }
                // Init-segment bootstrap over the mesh (not the Bulletin gateway). If we
                // hold the init, push it straight to the new peer — a viewer needs it
                // before any fragment plays. If we DON'T have it yet, ask (the peer may be
                // the publisher or a seed that has it); the on-tick retry covers races.
                match &self.init {
                    Some(bytes) => {
                        link.send(Channel::Ctrl, MeshMsg::InitData { bytes: bytes.to_vec() }.encode());
                    }
                    None => {
                        link.send(Channel::Ctrl, MeshMsg::WantInit.encode());
                    }
                }
            }
            EngineEvent::PeerDisconnected { peer } => {
                self.links.remove(&peer);
                self.eng.peers.remove(&peer);
                self.subscribers.remove(&peer);
                // Release in-flight requests to this peer, and drop ALL of its partial
                // reassemblies (pulled AND pushed) so the picker re-requests elsewhere
                // and the byte budget is returned.
                let dropped: Vec<Seq> = self
                    .pending
                    .iter()
                    .filter(|(_, (p, _))| *p == peer)
                    .map(|(s, _)| *s)
                    .collect();
                for seq in dropped {
                    self.pending.remove(&seq);
                    // The hedge outlives its dead primary: promote it so its delivery is
                    // still admitted and the timeout sweep still covers it.
                    if let Some(h) = self.hedge.remove(&seq) {
                        self.pending.insert(seq, h);
                    }
                }
                // And drop hedge entries POINTING AT the dead peer (their primary lives on;
                // the dead peer's budget accounting died with its PeerState).
                self.hedge.retain(|_, (p, _)| *p != peer);
                let keys: Vec<(PeerId, Seq)> =
                    self.reasm.keys().filter(|(p, _)| *p == peer).copied().collect();
                for k in keys {
                    self.remove_reasm(&k);
                }
            }
            EngineEvent::Inbound { peer, channel, bytes } => self.on_inbound(peer, channel, &bytes),
            EngineEvent::Produced { seq, id, bytes } => {
                // Publisher pipeline produced a segment — store it and start serving.
                self.eng.store.insert(seq, id, bytes);
                if !self.eng.local.has(seq) {
                    self.delivered_total += 1;
                }
                self.eng.local.set(seq);
                self.segment_ids.insert(seq, id);
                self.eng.head_seq = self.eng.head_seq.max(seq);
                // Off-chain signaling (TECH_SPEC §6.4): sign this edge + gossip it in-mesh
                // so viewers learn the id at mesh speed (the chain edge is the fallback).
                if let Some(signer) = self.edge_signer.clone() {
                    if self.edge_seen.insert(seq) {
                        let sig = signer.sign(&edge_payload(&self.stream_id, seq, &id));
                        self.gossip_edge(seq, id, sig, None);
                        // Remember it for the periodic re-announce (loss recovery).
                        self.recent_edges.push_back((seq, id, sig));
                        while self.recent_edges.len() > self.eng.cfg.window as usize {
                            self.recent_edges.pop_front();
                        }
                    }
                }
                // Push-pull: push it straight to subscribers (don't wait for their Want).
                self.push_to_subscribers(seq);
            }
            EngineEvent::InitSegment { bytes } => {
                // Publisher learned its init segment — hold it and push to everyone already
                // connected (a viewer that connected before the encoder was up now gets it).
                let is_new = self.init.is_none();
                self.init = Some(bytes.clone());
                if is_new {
                    let msg = MeshMsg::InitData { bytes: bytes.to_vec() }.encode();
                    for link in self.links.values() {
                        link.send(Channel::Ctrl, msg.clone());
                    }
                }
            }
            EngineEvent::LiveEdge { seq, id } => {
                // Learn a segment's content id from the chain edge poll (the fallback path).
                self.apply_edge(seq, id);
            }
            EngineEvent::SetPublisherKey { key } => {
                // Trust anchor learned at runtime — gossiped edges now get verified.
                self.publisher_key = Some(key);
            }
            EngineEvent::SetRole(role) => {
                // A viewer whose player left converts to a background seed (or back).
                // The role drives the unchoke policy and, for Live seeds, the
                // edge-following cursor in on_tick.
                log::info!("[mesh] role → {:?}", role);
                self.eng.cfg.role = role;
            }
            EngineEvent::SetUploadBudget(bps) => {
                // Health-tuned contribution: clamp the token bucket into the new
                // budget's burst immediately so a cut takes effect this tick.
                log::info!("[mesh] upload budget → {bps} bps");
                self.eng.cfg.upload_budget_bps = bps;
                if bps > 0 {
                    let burst =
                        (bps as f64 / 8.0 * 0.5).max(2.0 * self.eng.seg_bytes as f64);
                    self.upload_tokens = self.upload_tokens.min(burst);
                }
            }
            EngineEvent::SetSegMs(ms) => {
                // The verified manifest told us the stream's real segment/part duration —
                // retiming the picker's deadlines and the live-lag stat (see the event doc).
                log::info!("[mesh] seg_ms → {ms} (was {})", self.eng.cfg.seg_ms);
                self.eng.cfg.seg_ms = ms.max(1);
            }
            EngineEvent::Tick => self.on_tick(),
            EngineEvent::Stop => {}
        }
    }

    fn send_hello(&self, link: &Arc<dyn Link>) {
        let msg = MeshMsg::Hello {
            peer_id: self.me.0,
            stream_id: [0u8; 32],
            version: 1,
            caps: Caps { upload_bps: self.eng.cfg.upload_budget_bps, relay: false },
            base_seq: self.eng.local.base(),
            bitfield: self.eng.local.to_bytes(),
        };
        link.send(Channel::Ctrl, msg.encode());
    }

    /// Record a segment's authenticated content id (from the chain edge OR a verified
    /// gossip), advance the head, and deliver any bytes that were pushed to us before
    /// the id was known (push-pull receive).
    fn apply_edge(&mut self, seq: Seq, id: SegmentId) {
        self.segment_ids.insert(seq, id);
        self.eng.head_seq = self.eng.head_seq.max(seq);
        log::info!("[edge] learned seq={seq} → head_seq={}", self.eng.head_seq);
        self.try_verify_pending(seq);
    }

    /// Broadcast a signed live-edge to every connected peer except `exclude` (the peer it
    /// arrived from). Re-broadcasting verbatim is safe: the signature travels with it, so
    /// every hop re-verifies against the same publisher key.
    fn gossip_edge(&self, seq: Seq, id: SegmentId, sig: [u8; 64], exclude: Option<PeerId>) {
        let encoded = MeshMsg::EdgeAnnounce { seq, id: id.0, sig }.encode();
        for (pid, link) in self.links.iter() {
            if Some(*pid) != exclude {
                link.send(Channel::Ctrl, encoded.clone());
            }
        }
    }

    fn on_tick(&mut self) {
        // Follow the player's play head so the picker's window (panic/mid/prefetch
        // zones) tracks real playback. The localhost HLS server advances it as the
        // viewer fetches segments; a publisher/seed sink reports 0 (no-op here).
        let head = self.sink.on_play_head();
        if head > self.eng.play_seq {
            self.eng.play_seq = head;
        }

        // A live SEED has no player advancing the cursor — pin it near the live edge
        // so the picker keeps fetching the fresh window (a stale cache serves nobody)
        // and the prune below keeps memory bounded as the stream runs on.
        if matches!(self.eng.cfg.role, Role::Seed) && matches!(self.eng.cfg.mode, Mode::Live) {
            let follow = self.eng.head_seq.saturating_sub(self.eng.cfg.window as Seq / 2);
            if follow > self.eng.play_seq {
                self.eng.play_seq = follow;
            }
        }

        // A live VIEWER whose fetch cursor is more than a full window behind the live edge
        // can never catch up part-by-part — this is a fresh join onto a running stream (the
        // publisher advertises from seq 0, keeping the whole history as the origin), or a
        // stall that fell off the back of the window. Snap the cursor to the live edge so it
        // plays live instead of grinding through the entire backlog from seq 0. Within a
        // window of the edge we leave it alone and follow the player's real play head (above).
        if matches!(self.eng.cfg.role, Role::Viewer) && matches!(self.eng.cfg.mode, Mode::Live) {
            let edge = self.eng.head_seq.saturating_sub(self.eng.cfg.window as Seq / 2);
            if edge > self.eng.play_seq + self.eng.cfg.window as Seq {
                log::info!(
                    "[mesh] snapping to live edge: play_seq {} → {} (head {})",
                    self.eng.play_seq, edge, self.eng.head_seq
                );
                self.eng.play_seq = edge;
            }
        }

        // Live viewers slide their retention window with playback: prune the store
        // (memory + disk spill), the seq→id map, and the local buffer map below
        // `play − window`. Without this a multi-hour stream grows all three without
        // bound — and re-advertises an ever-longer bitfield to every peer. Publishers
        // keep everything (they're the origin); a seed's play head stays 0, so its
        // cache is bounded by the store's own capacity instead.
        if matches!(self.eng.cfg.mode, Mode::Live) && !matches!(self.eng.cfg.role, Role::Publisher)
        {
            // Cap the floor at what we've actually DELIVERED, not the play cursor: when the
            // fetch lags a live-edge snap (a peer drops, so `play_seq` races to the chain
            // edge while delivery stalls behind it), pruning to `play_seq − window` would
            // wipe the fetched tail and hand the player an EMPTY playlist. Keeping a window
            // below the delivered head leaves it the most-recent contiguous content to show
            // while the fetch recovers. When delivery is keeping up (`highest ≥ play_seq`)
            // this is exactly the old `play_seq − window`.
            let anchor = self
                .eng
                .local
                .highest()
                .map_or(self.eng.play_seq, |h| self.eng.play_seq.min(h));
            let floor = anchor.saturating_sub(self.eng.cfg.window as Seq);
            if floor > self.last_prune_floor {
                self.last_prune_floor = floor;
                self.eng.store.prune_below(floor);
                self.eng.local.prune_below(floor);
                self.segment_ids.retain(|s, _| *s >= floor);
            }
        }

        // Refill the upload token bucket at the budget rate (capped at the burst).
        if self.eng.cfg.upload_budget_bps > 0 {
            let dt = self.now_ms.saturating_sub(self.last_token_ms);
            self.last_token_ms = self.now_ms;
            let refill = (self.eng.cfg.upload_budget_bps as f64 / 8.0) * (dt as f64 / 1000.0);
            let burst = (self.eng.cfg.upload_budget_bps as f64 / 8.0 * 0.5)
                .max(2.0 * self.eng.seg_bytes as f64);
            self.upload_tokens = (self.upload_tokens + refill).min(burst);
        }

        // Paced serve: drain each peer's `serve_queue` at up to SERVE_BYTES_PER_TICK,
        // easing segments into the connection over successive ticks instead of blasting
        // a whole catch-up window into a just-connected association (which overruns its
        // SCTP send buffer and resets it). The token bucket above still caps aggregate
        // upload across all peers; this caps the per-peer, per-tick burst.
        let peer_ids: Vec<PeerId> = self.eng.peers.keys().copied().collect();
        for pid in peer_ids {
            // Honor choking here too, not just at enqueue: a peer we started withholding
            // upload from after it queued must stop being served (its backlog resumes
            // when we unchoke it). Publishers/seeds never choke, so they always drain.
            if self.eng.peers.get(&pid).map(|p| p.choked).unwrap_or(true) {
                continue;
            }
            let mut served = 0usize;
            while served < SERVE_BYTES_PER_TICK {
                let Some(seq) = self
                    .eng
                    .peers
                    .get(&pid)
                    .and_then(|p| p.serve_queue.front().copied())
                else {
                    break;
                };
                match self.eng.store.location(seq) {
                    SegmentLocation::Memory => {
                        let Some(b) = self.eng.store.get_mem(seq) else {
                            // Pruned out from under the queue — drop it, keep draining.
                            self.pop_serve(pid);
                            continue;
                        };
                        let cost = b.len() as f64;
                        if self.eng.cfg.upload_budget_bps > 0 {
                            if self.upload_tokens < cost {
                                break; // out of budget this tick — leave it queued
                            }
                            self.upload_tokens -= cost;
                        }
                        self.pop_serve(pid);
                        served += b.len();
                        if let Some(link) = self.links.get(&pid).cloned() {
                            self.send_segment(&link, seq, &b);
                        }
                    }
                    // Spilled to disk: never `fs::read` inline (a cold read stalls the
                    // whole actor loop). Charge the nominal size, then hash-verify +
                    // chunk it out off-loop over the thread-safe link.
                    SegmentLocation::Disk { id, path } => {
                        let cost = self.eng.seg_bytes as f64;
                        if self.eng.cfg.upload_budget_bps > 0 {
                            if self.upload_tokens < cost {
                                break;
                            }
                            self.upload_tokens -= cost;
                        }
                        self.pop_serve(pid);
                        served += self.eng.seg_bytes as usize;
                        let Some(link) = self.links.get(&pid).cloned() else { continue };
                        tokio::task::spawn_blocking(move || {
                            let Ok(data) = std::fs::read(&path) else { return };
                            if !crypto::verify_segment(&data, &id) {
                                return;
                            }
                            send_segment_on(&link, seq, &data);
                        });
                    }
                    // We don't have it (never did, or it was pruned) — drop from queue.
                    SegmentLocation::Absent => self.pop_serve(pid),
                }
            }
        }

        // Expire stale in-flight requests (a lost chunk on the unreliable bulk channel
        // never completes and never hash-fails) so the picker re-requests them.
        let now = self.now_ms;
        let stale: Vec<Seq> = self
            .pending
            .iter()
            .filter(|(_, (_, sent))| now.saturating_sub(*sent) >= PENDING_TIMEOUT_MS)
            .map(|(s, _)| *s)
            .collect();
        for seq in stale {
            if let Some((pid, _)) = self.clear_pending(seq) {
                self.remove_reasm(&(pid, seq)); // re-request starts fresh
                // The peer accepted a request it never served — a buffer-map lie or a
                // dying link. Genuine loss heals back on the next verified delivery.
                self.penalize(pid, Penalty::Timeout);
            }
        }

        // Evict reassemblies that never completed within their TTL. The pending sweep
        // above can't reach these: a *pushed* segment has no `pending` entry, so a
        // single lost chunk would otherwise pin its buffered bytes forever.
        if !self.reasm.is_empty() {
            let expired: Vec<(PeerId, Seq)> = self
                .reasm
                .iter()
                .filter(|(_, e)| now.saturating_sub(e.created_ms) >= REASM_TTL_MS)
                .map(|(k, _)| *k)
                .collect();
            for k in expired {
                self.remove_reasm(&k);
            }
        }

        // Evict push-ahead buffers whose seq slid below the play head: their id never
        // arrived in time, playback has moved past them, so they'll never be delivered.
        if !self.pending_verify.is_empty() {
            let play = self.eng.play_seq;
            let drop: Vec<Seq> =
                self.pending_verify.keys().copied().filter(|s| *s < play).collect();
            for seq in drop {
                if let Some((_, raw)) = self.pending_verify.remove(&seq) {
                    self.pending_verify_bytes =
                        self.pending_verify_bytes.saturating_sub(raw.len() as u64);
                }
            }
        }

        // Bound the gossip-dedup set to the recent live window (edges are monotonic, so
        // far-behind seqs won't legitimately re-appear near the head).
        if self.edge_seen.len() as Seq > EDGE_SEEN_KEEP {
            let cutoff = self.eng.head_seq.saturating_sub(EDGE_SEEN_KEEP);
            self.edge_seen.retain(|&s| s >= cutoff);
        }

        // Publisher: re-announce the live window's signed edges (loss recovery — see
        // EDGE_REANNOUNCE_MS). Receivers dedup via `edge_seen`, so a heard announce
        // costs one decode; a MISSED one finally delivers the id that unwedges any
        // bytes parked unverifiable in `pending_verify`.
        if !self.recent_edges.is_empty()
            && self.now_ms.saturating_sub(self.last_edge_regossip_ms) >= EDGE_REANNOUNCE_MS
        {
            self.last_edge_regossip_ms = self.now_ms;
            for (seq, id, sig) in self.recent_edges.clone() {
                self.gossip_edge(seq, id, sig, None);
            }
        }

        // Advertise the buffer map only on change, or at most every BUFFERMAP_INTERVAL_MS
        // (was every 100ms tick — pure overhead that scales with peer count).
        let count = self.eng.local.count();
        if count != self.last_bm_count
            || self.now_ms.saturating_sub(self.last_bm_ms) >= BUFFERMAP_INTERVAL_MS
        {
            self.last_bm_count = count;
            self.last_bm_ms = self.now_ms;
            let bm = MeshMsg::BufferMap {
                base_seq: self.eng.local.base(),
                bitfield: self.eng.local.to_bytes(),
            };
            let encoded = bm.encode();
            for link in self.links.values() {
                link.send(Channel::Ctrl, encoded.clone());
            }
        }

        // Probe RTT periodically; the Pong handler feeds the picker real latency.
        if !self.links.is_empty()
            && self.now_ms.saturating_sub(self.last_ping_ms) >= PING_INTERVAL_MS
        {
            self.last_ping_ms = self.now_ms;
            let ping = MeshMsg::Ping { nonce: self.now_ms, t_send_ms: self.now_ms }.encode();
            for link in self.links.values() {
                link.send(Channel::Ctrl, ping.clone());
            }
        }

        // Off-chain presence gossip (TECH_SPEC §7.3): share our known-peer directory so
        // neighbors discover the swarm without a chain read/write. Fires promptly once we
        // have peers + something to share (so a fresh link gets the book right away), then
        // every interval; the slot is only consumed when we actually send.
        if let Some(book) = self.presence_book.clone() {
            let due = self.last_presence_gossip_ms == 0
                || self.now_ms.saturating_sub(self.last_presence_gossip_ms)
                    >= PRESENCE_GOSSIP_INTERVAL_MS;
            if !self.links.is_empty() && due {
                let records = book.sample(PRESENCE_GOSSIP_MAX);
                if !records.is_empty() {
                    self.last_presence_gossip_ms = self.now_ms;
                    let msg = MeshMsg::PresenceGossip { records }.encode();
                    for link in self.links.values() {
                        link.send(Channel::Ctrl, msg.clone());
                    }
                }
            }
        }

        // Upload-slot fairness: re-evaluate which peers we serve (viewers ration; the
        // first eval fires as soon as we have peers, then every CHOKE_INTERVAL_MS).
        if !self.eng.peers.is_empty()
            && (self.last_choke_ms == 0
                || self.now_ms.saturating_sub(self.last_choke_ms) >= CHOKE_INTERVAL_MS)
        {
            self.recompute_unchokes();
        }

        // Publish a live stats snapshot (1/s) for the UI — real numbers, not
        // placeholders: peers, delivered, and the live-edge lag playback actually has.
        if self.stats_tx.is_some()
            && self.now_ms.saturating_sub(self.last_stats_ms) >= 1_000
        {
            self.last_stats_ms = self.now_ms;
            let mut s = self.stats.clone();
            s.delivered = self.delivered_total;
            s.peers = self.eng.peers.len();
            s.known_peers = self.known_peers.len();
            s.head_seq = self.eng.head_seq;
            s.play_seq = self.eng.play_seq;
            s.latency_s = self.eng.head_seq.saturating_sub(self.eng.play_seq) as f64
                * self.eng.cfg.seg_ms as f64
                / 1000.0;
            s.reasm_entries = self.reasm.len();
            s.reasm_bytes = self.reasm_bytes;
            s.pending_entries = self.pending.len();
            if let Some(tx) = &self.stats_tx {
                let _ = tx.send(s);
            }
        }

        // Run the picker and issue Wants to peers (and, when the panic zone finds no
        // peer able to meet a deadline, escalate to the durable floor).
        let reqs = self.eng.plan(self.now_ms, &mut self.rng);
        for r in reqs {
            // Bytes already reassembled but parked awaiting their id (`pending_verify` —
            // the announce that carries it was lost) must not be re-fetched in a loop;
            // the periodic edge re-announce delivers the id and unparks them.
            if self.pending_verify.contains_key(&r.seq) {
                continue;
            }
            match r.source {
                Source::Peer(pid) => {
                    let hedging = match self.pending.get(&r.seq) {
                        // Free seq — this Want becomes the primary.
                        None => false,
                        // Already in flight. The picker's panic-zone hedge (`redundant`)
                        // plans ONE second concurrent holder — realize it (deduping it
                        // away made the hedge a no-op end-to-end, so a slow primary
                        // stalled every deadline-critical segment until the 2 s pending
                        // timeout). Same-peer re-requests and a second hedge still dedup.
                        Some((primary, _)) => {
                            if !r.redundant || *primary == pid || self.hedge.contains_key(&r.seq)
                            {
                                continue;
                            }
                            true
                        }
                    };
                    // Don't waste a Want on a peer that's choking us.
                    if self.eng.peers.get(&pid).map(|p| p.choked_by).unwrap_or(false) {
                        continue;
                    }
                    if let Some(link) = self.links.get(&pid).cloned() {
                        let want = MeshMsg::Want {
                            segment_seqs: vec![r.seq],
                            deadline_hint_ms: 0,
                        };
                        link.send(Channel::Ctrl, want.encode());
                        if hedging {
                            log::info!("[mesh] → hedge Want seq={} to {:?}", r.seq, pid);
                            self.hedge.insert(r.seq, (pid, self.now_ms));
                        } else {
                            log::info!("[mesh] → Want seq={} to {:?}", r.seq, pid);
                            self.pending.insert(r.seq, (pid, self.now_ms));
                        }
                        if let Some(p) = self.eng.peers.get_mut(&pid) {
                            p.pending_bytes =
                                p.pending_bytes.saturating_add(self.eng.seg_bytes);
                        }
                    }
                }
                // TECH_SPEC §8.6: the deadline is about to be missed and no peer can
                // help — fetch from the durable floor instead of stalling. A stale
                // `pending` entry does NOT block this (that slow peer is the reason
                // we're here); delivery clears it and releases the peer's budget.
                Source::Bulletin | Source::Seed => self.request_fallback(r.seq),
            }
        }
    }

    /// Kick off one bounded, off-loop durable-floor fetch for `seq` (see
    /// [`MeshNode::with_fallback`]). Requires the authenticated id — bytes from
    /// untrusted storage are only accepted if they hash to it.
    fn request_fallback(&mut self, seq: Seq) {
        let Some(fetch) = self.fallback.clone() else { return };
        if self.fallback_inflight.contains(&seq)
            || self.fallback_inflight.len() >= MAX_FALLBACK_INFLIGHT
        {
            return;
        }
        let Some(id) = self.segment_ids.get(&seq).copied() else { return };
        self.fallback_inflight.insert(seq);
        let tx = self.fallback_tx.clone();
        log::info!("[fallback] seq={seq} → durable floor");
        tokio::spawn(async move {
            let bytes = fetch(seq, id).await;
            let _ = tx.send((seq, id, bytes));
        });
    }

    /// A durable-floor fetch completed — verify and deliver (the receive half of the
    /// TECH_SPEC §8.6 escalation).
    fn on_fallback(&mut self, seq: Seq, id: SegmentId, bytes: Option<Bytes>) {
        self.fallback_inflight.remove(&seq);
        let Some(b) = bytes else { return };
        if self.eng.local.has(seq) {
            return; // a peer beat the floor to it — fine
        }
        // Re-verify: the hook is app code and the floor is untrusted storage.
        if !crypto::verify_segment(&b, &id) {
            log::warn!("[fallback] seq={seq} bytes from the durable floor failed verification");
            return;
        }
        // Release any in-flight peer request for this seq (the slow peer's budget).
        self.clear_pending(seq);
        self.stats.from_origin += 1;
        log::info!("[fallback] seq={seq} delivered from the durable floor ({} B)", b.len());
        self.deliver_verified(seq, id, b);
    }

    fn on_inbound(&mut self, peer: PeerId, _channel: Channel, bytes: &[u8]) {
        // A banned peer's connection is being torn down — nothing it says matters.
        if self.eng.peers.get(&peer).map(|p| p.banned).unwrap_or(false) {
            return;
        }
        let msg = match MeshMsg::decode(&mut &bytes[..]) {
            Ok(m) => m,
            Err(_) => return, // hostile / malformed input is dropped, never fatal
        };
        match msg {
            MeshMsg::Hello { base_seq, bitfield, .. } | MeshMsg::BufferMap { base_seq, bitfield } => {
                if bitfield.len() > MAX_BITFIELD_BYTES {
                    // No honest live window is this wide — hostile allocation bait.
                    self.penalize(peer, Penalty::ProtocolAbuse);
                    return;
                }
                let entry = self.eng.peers.entry(peer).or_insert_with(|| PeerState::new(peer));
                entry.buffer = BufferMap::from_bytes(base_seq, &bitfield);
                log::debug!("[mesh] ← Hello/BufferMap from {:?}: base_seq={base_seq}, {}B bitfield", peer, bitfield.len());
            }
            MeshMsg::Want { segment_seqs, .. } => {
                // Batch cap: one Want never asks for more than the picker would issue.
                if segment_seqs.len() > MAX_WANT_SEQS {
                    self.penalize(peer, Penalty::ProtocolAbuse);
                    return;
                }
                // Upload fairness: serve only peers we've unchoked (publishers/seeds
                // never choke, so they always serve).
                if self.eng.peers.get(&peer).map(|p| p.choked).unwrap_or(false) {
                    return;
                }
                // Enqueue rather than serve inline: the tick drains `serve_queue` at a
                // paced rate (SERVE_BYTES_PER_TICK) so a fresh connection isn't blasted
                // with a whole catch-up window at once (see the constant's rationale).
                // Dedup so a re-`Want` (its pending timed out) doesn't double-queue, and
                // bound the backlog. Absent seqs are dropped by the drain, not here — a
                // viewer only asks for what our buffer-map advertises anyway.
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    for seq in segment_seqs {
                        if p.serve_queue.len() >= SERVE_QUEUE_MAX {
                            break;
                        }
                        if !p.serve_queue.contains(&seq) {
                            p.serve_queue.push_back(seq);
                        }
                    }
                }
            }
            MeshMsg::SegmentData { seq, total_len, offset, bytes, .. } => {
                self.on_segment_data(peer, seq, total_len, offset, &bytes);
            }
            MeshMsg::Have { seq } => {
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.buffer.set(seq);
                }
            }
            MeshMsg::WantInit => {
                // A peer needs the bootstrap init and we may hold it — serve it directly.
                if let (Some(bytes), Some(link)) = (self.init.clone(), self.links.get(&peer).cloned()) {
                    link.send(Channel::Ctrl, MeshMsg::InitData { bytes: bytes.to_vec() }.encode());
                }
            }
            MeshMsg::InitData { bytes } => {
                // Received the init from a peer: install it into the sink so playback can
                // start (the decrypting sink opens it if the stream is encrypted), hold it
                // so we can serve it onward, and forward to our OTHER links — so a seed that
                // just learned the init immediately hands it to its downstream viewers
                // (covers a viewer that asked a peer which didn't have it yet). Install once.
                if self.init.is_none() && !bytes.is_empty() {
                    let b = Bytes::from(bytes);
                    self.init = Some(b.clone());
                    self.sink.push_init(b.clone());
                    log::info!("[init] installed init segment from peer {peer:?} over the mesh");
                    let msg = MeshMsg::InitData { bytes: b.to_vec() }.encode();
                    for (p, link) in self.links.iter() {
                        if *p != peer {
                            link.send(Channel::Ctrl, msg.clone());
                        }
                    }
                }
            }
            MeshMsg::Ping { nonce, t_send_ms } => {
                if let Some(link) = self.links.get(&peer).cloned() {
                    link.send(Channel::Ctrl, MeshMsg::Pong { nonce, t_send_ms }.encode());
                }
            }
            MeshMsg::PeerGossip { peers } => {
                // In-mesh peer discovery after bootstrap (TECH_SPEC §7.3): learned
                // over the data channel, never the statement store. The node holds
                // no signaling handle, so this provably incurs zero store writes.
                if peers.len() > MAX_GOSSIP_PEERS {
                    self.penalize(peer, Penalty::ProtocolAbuse);
                    return;
                }
                for p in peers {
                    let pid = PeerId(p);
                    if pid != self.me && self.known_peers.len() < KNOWN_PEERS_MAX {
                        self.known_peers.insert(pid);
                    }
                }
            }
            MeshMsg::PresenceGossip { records } => {
                // Off-chain presence directory (TECH_SPEC §7.3): merge what a peer knows
                // into the shared book (the session dials from it) and count them as
                // discovered. Zero statement-store writes — pure in-mesh discovery.
                if records.len() > MAX_GOSSIP_RECORDS {
                    self.penalize(peer, Penalty::ProtocolAbuse);
                    return;
                }
                for r in &records {
                    let pid = PeerId(r.peer_id);
                    if pid != self.me && self.known_peers.len() < KNOWN_PEERS_MAX {
                        self.known_peers.insert(pid);
                    }
                }
                if let Some(book) = &self.presence_book {
                    book.merge(records, &self.me);
                }
            }
            MeshMsg::Pong { t_send_ms, .. } => {
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    let rtt = self.now_ms.saturating_sub(t_send_ms) as f64;
                    p.rtt_ms.update(rtt.max(1.0));
                }
            }
            // Upload fairness: a peer telling us it won't serve us — stop wasting Wants
            // on it (the picker skips `choked_by` peers; the pending timeout re-routes).
            MeshMsg::Choke => {
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.choked_by = true;
                }
            }
            MeshMsg::Unchoke => {
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.choked_by = false;
                }
            }
            // Push-pull (TECH_SPEC §6.4): a peer registers/withdraws standing interest in
            // our live edge. While subscribed, we push it each new segment proactively.
            MeshMsg::Subscribe => {
                self.subscribers.insert(peer);
            }
            MeshMsg::Unsubscribe => {
                self.subscribers.remove(&peer);
            }
            // Off-chain signaling (TECH_SPEC §6.4): a gossiped signed live-edge. Verify it
            // against the publisher trust anchor BEFORE acting on or relaying it, dedup by
            // seq (loop/storm suppression), apply it, then re-gossip to our other peers.
            MeshMsg::EdgeAnnounce { seq, id, sig } => {
                let pubkey = match self.publisher_key {
                    Some(k) => k,
                    None => {
                        log::warn!("[edge] ← gossip seq={seq} dropped: no publisher_key yet");
                        return; // no trust anchor → can't trust a gossiped edge.
                    }
                };
                let sid = SegmentId(id);
                if !crypto::verify_sr25519(&pubkey, &edge_payload(&self.stream_id, seq, &sid), &sig) {
                    log::warn!("[edge] ← gossip seq={seq} verify FAILED vs publisher_key");
                    return; // forged / wrong stream → never propagate it.
                }
                log::debug!("[edge] ← gossip seq={seq} verified");
                if !self.edge_seen.insert(seq) {
                    return; // already seen this edge — don't re-apply or re-storm.
                }
                self.apply_edge(seq, sid);
                self.gossip_edge(seq, sid, sig, Some(peer)); // fan out, minus the sender.
            }
            // Cancel: no-op (we never hedge — `pending` dedups per seq — so there's no
            // losing request to cancel).
            _ => {}
        }
    }

    fn send_segment(&mut self, link: &Arc<dyn Link>, seq: Seq, bytes: &[u8]) {
        self.stats.sent_bytes += bytes.len() as u64;
        send_segment_on(link, seq, bytes);
    }

    /// Drop the head of a peer's paced serve queue (no-op if peer/queue is gone).
    fn pop_serve(&mut self, pid: PeerId) {
        if let Some(p) = self.eng.peers.get_mut(&pid) {
            p.serve_queue.pop_front();
        }
    }

    fn on_segment_data(&mut self, peer: PeerId, seq: Seq, total_len: u32, offset: u32, bytes: &[u8]) {
        if self.eng.local.has(seq) {
            return;
        }
        if offset == 0 {
            log::debug!("[seg] ← seq={seq} from {:?} ({total_len} B total)", peer);
        }
        // Reject absurd/zero sizes before allocating a reassembler (hostile peer guard).
        if total_len == 0 || total_len > MAX_SEGMENT_BYTES {
            return;
        }
        // Admission gate: buffer only bytes we asked THIS peer for, or an in-window
        // unsolicited push (push-pull runs a little ahead of the verified edge). An
        // out-of-window spray for segments nobody asked about is dropped before it
        // can allocate anything.
        let asked_this_peer = self.pending.get(&seq).map(|(p, _)| *p == peer).unwrap_or(false)
            || self.hedge.get(&seq).map(|(p, _)| *p == peer).unwrap_or(false);
        let in_push_window =
            seq >= self.eng.play_seq && seq <= self.eng.head_seq.saturating_add(PUSH_AHEAD);
        if !asked_this_peer && !in_push_window {
            // A near-miss behind the play head is just a push that lost a race and
            // costs nothing; anything far outside every honest window is a spray.
            if seq > self.eng.head_seq.saturating_add(PUSH_AHEAD * 4)
                || seq.saturating_add(self.eng.cfg.window as Seq) < self.eng.play_seq
            {
                self.penalize(peer, Penalty::ProtocolAbuse);
            }
            return;
        }
        // Global caps. Refusing a NEW reassembly (rather than evicting an old one) means
        // a flood can never displace a slot attached to a request we actually made.
        let key = (peer, seq);
        if !self.reasm.contains_key(&key)
            && (self.reasm.len() >= MAX_REASM
                || self.reasm_bytes.saturating_add(total_len as u64) > MAX_REASM_BYTES)
        {
            return;
        }
        let now = self.now_ms;
        let complete = {
            let e = self
                .reasm
                .entry(key)
                .or_insert_with(|| ReasmEntry { r: Reassembler::new(total_len), created_ms: now });
            self.reasm_bytes += e.r.add(offset, bytes) as u64;
            e.r.is_complete()
        };
        if !complete {
            return;
        }
        let Some(entry) = self.reasm.remove(&key) else { return };
        self.reasm_bytes = self.reasm_bytes.saturating_sub(entry.r.buffered_bytes());
        let r = entry.r;
        match self.segment_ids.get(&seq).copied() {
            // We already know the authenticated id → verify against it now.
            Some(id) => match r.finish_verified(&id) {
                Some(data) => self.accept_segment(peer, seq, id, Bytes::from(data)),
                None => {
                    // Hash mismatch against a KNOWN id — a genuinely forged segment:
                    // discard, penalize the sender, re-request elsewhere.
                    self.stats.hash_failures += 1;
                    self.clear_pending(seq);
                    self.penalize(peer, Penalty::HashFail);
                }
            },
            // Pushed to us before our live edge learned this segment's id (push-pull):
            // buffer the bytes until `LiveEdge` provides the id, then verify. This is NOT
            // proof of a bad peer, so we don't penalize — and it's bounded, so an early
            // (or hostile) push can't grow memory without limit.
            None => {
                self.clear_pending(seq);
                if let Some(raw) = r.assemble() {
                    self.buffer_pending_verify(peer, seq, raw);
                }
            }
        }
    }

    /// Remove a seq's in-flight request, releasing the **asked** peer's `pending_bytes`
    /// budget. Every `pending` removal must go through here: the asked peer may differ
    /// from whoever delivered the bytes (a push racing a pull), and a removal that skips
    /// the release inflates that peer's `pending_bytes` forever, permanently poisoning
    /// the picker's expected-delivery-time ranking for it.
    fn clear_pending(&mut self, seq: Seq) -> Option<(PeerId, u64)> {
        let entry = self.pending.remove(&seq);
        let seg_bytes = self.eng.seg_bytes;
        if let Some((asked, _)) = entry {
            if let Some(p) = self.eng.peers.get_mut(&asked) {
                p.pending_bytes = p.pending_bytes.saturating_sub(seg_bytes);
            }
        }
        // The hedge rode alongside the primary — release its budget too (whichever peer
        // delivered, the other's in-flight copy is now moot; late bytes are just dropped
        // by the reassembler admission since the seq is held).
        if let Some((hedged, _)) = self.hedge.remove(&seq) {
            if let Some(p) = self.eng.peers.get_mut(&hedged) {
                p.pending_bytes = p.pending_bytes.saturating_sub(seg_bytes);
            }
        }
        entry
    }

    /// Drop one in-progress reassembly and return its buffered bytes to the global
    /// budget. Every `reasm` removal must go through here, or `reasm_bytes` drifts
    /// and the [`MAX_REASM_BYTES`] cap starts refusing honest deliveries.
    fn remove_reasm(&mut self, key: &(PeerId, Seq)) {
        if let Some(e) = self.reasm.remove(key) {
            self.reasm_bytes = self.reasm_bytes.saturating_sub(e.r.buffered_bytes());
        }
    }

    /// Decay a peer's reputation for observed misbehavior; crossing the floor bans
    /// it — choked, its partial state dropped, the connection actively closed, and
    /// (via the shared [`BanList`]) barred from re-dialing/being redialed while the
    /// ban lasts. All misbehavior handling funnels through here so evidence
    /// accumulates on one scale instead of each site inventing its own.
    fn penalize(&mut self, peer: PeerId, why: Penalty) {
        let crossed = match self.eng.peers.get_mut(&peer) {
            Some(p) => {
                p.reputation *= why.factor();
                if matches!(why, Penalty::Timeout) {
                    p.strikes += 1;
                    // Timeouts deprioritize but never alone ban — see TIMEOUT_REP_FLOOR.
                    p.reputation = p.reputation.max(TIMEOUT_REP_FLOOR);
                }
                let crossed = p.reputation < REPUTATION_FLOOR && !p.banned;
                if crossed {
                    p.banned = true;
                    log::warn!(
                        "[mesh] peer {:?} banned ({:?}, reputation {:.3}, strikes {})",
                        peer, why, p.reputation, p.strikes
                    );
                }
                crossed
            }
            None => false,
        };
        if !crossed {
            return;
        }
        self.subscribers.remove(&peer);
        let keys: Vec<(PeerId, Seq)> =
            self.reasm.keys().filter(|(p, _)| *p == peer).copied().collect();
        for k in keys {
            self.remove_reasm(&k);
        }
        if let Some(bans) = &self.ban_list {
            bans.ban(peer);
        }
        if let Some(link) = self.links.get(&peer) {
            link.send(Channel::Ctrl, MeshMsg::Choke.encode());
            link.close();
        }
        // The rest of the local state (links / eng.peers / pending) unwinds through
        // the PeerDisconnected the closed link produces.
    }

    /// Store a verified segment, feed the player, update the sender's throughput, and
    /// (push-pull) reshare it to our own subscribers so the live edge propagates
    /// hop-by-hop without each downstream viewer paying a discovery + `Want` round-trip.
    fn accept_segment(&mut self, peer: PeerId, seq: Seq, id: SegmentId, b: Bytes) {
        let n = b.len();
        self.stats.peer_bytes += n as u64;
        // Real throughput: bytes delivered / time since we asked (push deliveries have no
        // matching `pending` entry, so they just don't update the estimate — correct).
        // `clear_pending` releases the ASKED peer's byte budget, which may not be the
        // sender when a push races a pull; the throughput estimate only updates when the
        // deliverer really is the peer we asked (otherwise the elapsed time is meaningless).
        let asked = self.clear_pending(seq);
        if let Some((asked_peer, sent_ms)) = asked {
            if asked_peer == peer {
                let elapsed = self.now_ms.saturating_sub(sent_ms).max(1);
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.throughput_bps.update((n as f64) * 8_000.0 / (elapsed as f64));
                }
            }
        }
        // Verified bytes slowly heal reputation: honest peers on lossy links recover
        // from genuine-loss timeouts instead of drifting toward the ban floor.
        if let Some(p) = self.eng.peers.get_mut(&peer) {
            if !p.banned {
                p.reputation = (p.reputation + REPUTATION_HEAL).min(1.0);
            }
        }
        self.deliver_verified(seq, id, b);
    }

    /// Store a verified segment, feed the player, and reshare it to subscribers —
    /// the source-agnostic tail of every delivery (peer, push, or durable floor).
    fn deliver_verified(&mut self, seq: Seq, id: SegmentId, b: Bytes) {
        let n = b.len();
        self.eng.store.insert(seq, id, b.clone());
        if !self.eng.local.has(seq) {
            self.delivered_total += 1;
        }
        self.eng.local.set(seq);
        log::info!("[seg] seq={seq} verified → sink ({n} B)");
        self.sink.push_segment(seq, b);
        self.push_to_subscribers(seq);
    }

    /// Hold a segment pushed to us before its id is known, within the global byte budget.
    fn buffer_pending_verify(&mut self, peer: PeerId, seq: Seq, raw: Vec<u8>) {
        let len = raw.len() as u64;
        if self.pending_verify.contains_key(&seq)
            || self.pending_verify_bytes + len > MAX_PENDING_VERIFY_BYTES
        {
            return; // already buffered, or over budget — let the picker pull it instead.
        }
        self.pending_verify_bytes += len;
        self.pending_verify.insert(seq, (peer, Bytes::from(raw)));
    }

    /// A pushed-ahead segment's id just arrived via the live edge — verify the buffered
    /// bytes against it and deliver (the receive half of push-pull).
    fn try_verify_pending(&mut self, seq: Seq) {
        let (peer, raw) = match self.pending_verify.remove(&seq) {
            Some(v) => v,
            None => return,
        };
        self.pending_verify_bytes = self.pending_verify_bytes.saturating_sub(raw.len() as u64);
        if self.eng.local.has(seq) {
            return; // already pulled it in the meantime.
        }
        let id = match self.segment_ids.get(&seq).copied() {
            Some(id) => id,
            None => return,
        };
        if crypto::verify_segment(&raw[..], &id) {
            self.accept_segment(peer, seq, id, raw);
        } else {
            self.stats.hash_failures += 1;
            self.penalize(peer, Penalty::HashFail);
        }
    }

    /// Push a freshly-available segment to every subscriber that (per its buffer map)
    /// still needs it, honoring choke + the upload budget — the proactive half of
    /// push-pull. A redundant push is harmless: the receiver dedups by `seq`.
    fn push_to_subscribers(&mut self, seq: Seq) {
        if self.subscribers.is_empty() {
            return;
        }
        let bytes = match self.eng.store.get(seq) {
            Some(b) => b,
            None => return,
        };
        let cost = bytes.len() as f64;
        let metered = self.eng.cfg.upload_budget_bps > 0;
        let targets: Vec<PeerId> = self
            .subscribers
            .iter()
            .copied()
            .filter(|s| match self.eng.peers.get(s) {
                // Skip a peer we're choking, or one the buffer map shows already has it.
                Some(p) => !p.choked && !p.buffer.has(seq),
                None => true, // subscribed before its Hello — no state yet, push anyway.
            })
            .collect();
        for s in targets {
            if metered {
                if self.upload_tokens < cost {
                    break; // out of budget this round — subscribers fall back to pulling.
                }
                self.upload_tokens -= cost;
            }
            if let Some(link) = self.links.get(&s).cloned() {
                self.send_segment(&link, seq, &bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u64) -> PeerId {
        PeerId::from_u64(n)
    }

    #[test]
    fn viewer_unchokes_top_reciprocators_plus_one_rotating_optimistic() {
        // 6 peers; peer i has throughput (7-i) Mbit/s, so 1 is the best reciprocator.
        let peers: Vec<(PeerId, f64, f64)> =
            (1..=6u64).map(|i| (pid(i), (7 - i) as f64 * 1_000_000.0, 50.0)).collect();

        let set = select_unchokes(&peers, Role::Viewer, 4, 0);
        // The four best reciprocators are always unchoked.
        for i in 1..=4 {
            assert!(set.contains(&pid(i)), "top reciprocator {i} must be unchoked");
        }
        // Plus exactly one optimistic slot from the remaining peers (5, 6).
        assert_eq!(set.len(), 5);
        assert!(set.contains(&pid(5)) ^ set.contains(&pid(6)), "exactly one optimistic slot");
        // The optimistic slot rotates with the round-robin counter.
        let set2 = select_unchokes(&peers, Role::Viewer, 4, 1);
        assert_ne!(set.contains(&pid(5)), set2.contains(&pid(5)), "optimistic slot rotates");
    }

    #[test]
    fn seed_spreads_by_proximity_not_throughput() {
        // A fast-but-far peer loses its regular slot to a slow-but-close one.
        let peers = vec![
            (pid(1), 9_000_000.0, 200.0), // far, fast
            (pid(2), 1_000_000.0, 20.0),  // close, slow
            (pid(3), 5_000_000.0, 300.0), // farthest
        ];
        let set = select_unchokes(&peers, Role::Seed, 1, 0);
        assert!(set.contains(&pid(2)), "a seed's regular slot goes to the closest peer");
    }

    #[test]
    fn fewer_peers_than_slots_unchokes_all() {
        let peers = vec![(pid(1), 1.0, 10.0), (pid(2), 2.0, 10.0)];
        assert_eq!(select_unchokes(&peers, Role::Viewer, 4, 0).len(), 2);
    }

    /// A sink whose play head the test controls (for window-prune behavior).
    struct FixedHeadSink(u64);
    impl MediaSink for FixedHeadSink {
        fn push_init(&self, _: Bytes) {}
        fn push_segment(&self, _: u64, _: Bytes) {}
        fn on_play_head(&self) -> u64 {
            self.0
        }
    }

    fn viewer_with_ids(ids: HashMap<Seq, SegmentId>, head: Seq) -> MeshNode {
        MeshNode::new_viewer(
            pid(9),
            MeshConfig::default(),
            100,
            Arc::new(DiscardSink),
            ids,
            head,
        )
    }

    #[test]
    fn injected_chunk_cannot_poison_an_honest_peers_delivery() {
        // We asked A for seq 5; hostile M injects a garbage chunk for the same seq.
        // Reassembly is keyed by (sender, seq), so M's bytes land in M's own slot: A's
        // delivery still verifies, and A's reputation is untouched. (Under the old
        // seq-only keying, the combined buffer hash-failed and *A* took the blame.)
        let a = pid(1);
        let m = pid(6);
        let payload: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
        let id = crypto::segment_id(&payload);
        let mut node = viewer_with_ids(HashMap::from([(5u64, id)]), 5);
        for p in [a, m] {
            node.eng.peers.insert(p, PeerState::new(p));
        }
        node.pending.insert(5, (a, 0));

        node.on_segment_data(m, 5, 100, 0, &[0xFFu8; 50]); // M's injection
        node.on_segment_data(a, 5, 100, 0, &payload[..50]);
        node.on_segment_data(a, 5, 100, 50, &payload[50..]);

        assert!(node.eng.local.has(5), "honest delivery survives the injection");
        assert_eq!(node.eng.peers[&a].reputation, 1.0, "honest peer keeps its reputation");
        assert_eq!(node.reasm.len(), 1, "only M's own partial slot remains");
        assert_eq!(node.reasm_bytes, 50, "byte budget tracks M's leftover chunk");
    }

    #[test]
    fn out_of_window_spray_is_dropped_before_allocating() {
        let mut node = viewer_with_ids(HashMap::new(), 5);
        let p = pid(1);
        node.eng.peers.insert(p, PeerState::new(p));
        // Nothing pending and seq 1000 is far beyond head+PUSH_AHEAD.
        node.on_segment_data(p, 1000, 100_000, 0, &[0u8; 1000]);
        assert!(node.reasm.is_empty(), "no reassembler for an unrequested far seq");
        assert_eq!(node.reasm_bytes, 0);
    }

    #[test]
    fn reassembly_entry_cap_holds_under_a_multi_peer_flood() {
        let mut node = viewer_with_ids(HashMap::new(), 5);
        // 70 distinct peers each push a partial chunk for an in-window seq.
        for i in 0..70u64 {
            let p = pid(100 + i);
            node.eng.peers.insert(p, PeerState::new(p));
            node.on_segment_data(p, 6, 100_000, 0, &[1u8; 100]);
        }
        assert_eq!(node.reasm.len(), MAX_REASM, "entry cap enforced");
        assert_eq!(node.reasm_bytes, (MAX_REASM * 100) as u64);
    }

    #[test]
    fn incomplete_push_is_reclaimed_by_the_ttl_sweep() {
        // A pushed segment with a lost chunk has no `pending` entry — only the TTL
        // sweep can reclaim its buffered bytes.
        let mut node = viewer_with_ids(HashMap::new(), 5);
        let p = pid(1);
        node.eng.peers.insert(p, PeerState::new(p));
        node.on_segment_data(p, 6, 100_000, 0, &[1u8; 100]);
        assert_eq!(node.reasm.len(), 1);

        node.now_ms += REASM_TTL_MS;
        node.on_tick();
        assert!(node.reasm.is_empty(), "TTL sweep evicted the stalled push");
        assert_eq!(node.reasm_bytes, 0, "its bytes returned to the budget");
    }

    #[test]
    fn live_viewer_prunes_store_ids_and_buffer_map_behind_the_play_head() {
        let ids: HashMap<Seq, SegmentId> = (0..50u64)
            .map(|s| (s, crypto::segment_id(&[s as u8; 8])))
            .collect();
        let mut node = MeshNode::new_viewer(
            pid(9),
            MeshConfig::default(), // window 16
            100,
            Arc::new(FixedHeadSink(40)),
            ids,
            50,
        );
        for s in 0..40u64 {
            let bytes = Bytes::from(vec![s as u8; 8]);
            let id = crypto::segment_id(&bytes);
            node.eng.store.insert(s, id, bytes);
            node.eng.local.set(s);
        }

        node.on_tick(); // play head 40, window 16 → floor 24

        assert!(!node.eng.store.has(0), "store pruned below the floor");
        assert!(node.eng.store.has(30), "window contents retained");
        assert!(!node.eng.local.has(0), "buffer map rebased");
        assert!(node.eng.local.has(30));
        assert!(!node.segment_ids.contains_key(&0), "id map pruned");
        assert!(node.segment_ids.contains_key(&30));
    }

    #[test]
    fn repeated_forgeries_cross_the_floor_and_ban_the_peer() {
        let m = pid(6);
        let payload = vec![0xAAu8; 64];
        let id = crypto::segment_id(&payload); // authenticated id for every seq
        let ids: HashMap<Seq, SegmentId> = (5..15u64).map(|s| (s, id)).collect();
        let mut node = viewer_with_ids(ids, 14);
        node.eng.peers.insert(m, PeerState::new(m));
        let bans = BanList::new();
        node.ban_list = Some(bans.clone());

        // M keeps delivering forged bytes for segments we asked it for.
        for seq in 5..11u64 {
            node.pending.insert(seq, (m, 0));
            node.on_segment_data(m, seq, 64, 0, &[0xEEu8; 64]); // wrong hash
        }

        let p = &node.eng.peers[&m];
        assert!(p.banned, "reputation {:.3} should have crossed the floor", p.reputation);
        assert!(bans.contains(&m), "conviction shared with the session's ban list");
        assert!(!node.subscribers.contains(&m));
        assert_eq!(node.stats.hash_failures, 6);
        // The picker never asks a banned peer for anything again.
        let reqs = node.eng.plan(1_000, &mut ChaCha8Rng::seed_from_u64(1));
        assert!(
            reqs.iter().all(|r| !matches!(r.source, Source::Peer(x) if x == m)),
            "no requests routed to a banned peer"
        );
    }

    #[test]
    fn timeouts_strike_and_verified_delivery_heals() {
        let a = pid(1);
        let payload = vec![0x11u8; 64];
        let id = crypto::segment_id(&payload);
        let mut node = viewer_with_ids(HashMap::from([(6u64, id)]), 6);
        node.eng.peers.insert(a, PeerState::new(a));

        // A accepted a request and never served it → strike + decay.
        node.pending.insert(5, (a, 0));
        node.now_ms = PENDING_TIMEOUT_MS;
        node.on_tick();
        let (rep_after_timeout, strikes) = {
            let p = &node.eng.peers[&a];
            (p.reputation, p.strikes)
        };
        assert_eq!(strikes, 1);
        assert!(rep_after_timeout < 1.0 && !node.eng.peers[&a].banned);

        // A verified delivery heals it back toward 1.0.
        node.pending.insert(6, (a, node.now_ms));
        node.on_segment_data(a, 6, 64, 0, &payload);
        assert!(node.eng.local.has(6));
        assert!(node.eng.peers[&a].reputation > rep_after_timeout, "heal applied");
    }

    #[test]
    fn delivery_releases_the_asked_peers_budget_not_the_senders() {
        // Ask peer A for seq 5, but let peer B deliver it (a push racing the pull):
        // A's `pending_bytes` budget must be released even though B sent the bytes —
        // otherwise A's expected-delivery-time ranking inflates permanently and the
        // picker stops asking a perfectly good peer.
        let a = pid(1);
        let b = pid(2);
        let payload = vec![0xCDu8; 64];
        let id = crypto::segment_id(&payload);
        let mut node = MeshNode::new_viewer(
            pid(9),
            MeshConfig::default(),
            64, // seg_bytes == payload size for easy budget math
            Arc::new(DiscardSink),
            HashMap::from([(5u64, id)]),
            5,
        );
        for p in [a, b] {
            node.eng.peers.insert(p, PeerState::new(p));
        }
        // Simulate the picker having asked A for seq 5.
        node.pending.insert(5, (a, 0));
        node.eng.peers.get_mut(&a).unwrap().pending_bytes = 64;
        node.now_ms = 40;

        // B delivers the whole segment in one chunk.
        node.on_segment_data(b, 5, 64, 0, &payload);

        assert!(node.eng.local.has(5), "segment delivered + verified");
        assert!(node.pending.is_empty(), "in-flight entry cleared");
        assert_eq!(node.eng.peers[&a].pending_bytes, 0, "ASKED peer's budget released");
        assert_eq!(node.eng.peers[&b].pending_bytes, 0, "sender's budget untouched");
    }

    #[test]
    fn frame_segment_data_matches_derive() {
        // The one-copy manual framing must be byte-identical to the derived encoding,
        // and decode back to the same message — this guards the hardcoded variant tag.
        let chunk = vec![0xABu8; 1234];
        let manual = frame_segment_data(7, 9999, 16, &chunk);
        let derived = MeshMsg::SegmentData {
            seq: 7,
            track_id: 0,
            total_len: 9999,
            offset: 16,
            bytes: chunk.clone(),
        }
        .encode();
        assert_eq!(manual, derived, "manual framing must byte-match the derive");
        match MeshMsg::decode(&mut &manual[..]).unwrap() {
            MeshMsg::SegmentData { seq, track_id, total_len, offset, bytes } => {
                assert_eq!((seq, track_id, total_len, offset), (7, 0, 9999, 16));
                assert_eq!(bytes, chunk);
            }
            other => panic!("expected SegmentData, got {other:?}"),
        }
    }
}
