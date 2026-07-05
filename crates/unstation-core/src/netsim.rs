//! Deterministic network-impairment simulation harness (test/bench-only).
//!
//! Drives many real [`MeshNode`]s in lockstep on a **virtual clock** over impaired
//! in-memory links, so the full protocol (picker, reputation, reassembly, live-edge,
//! serve pacing, fallback) can be hardened + tuned under loss / latency / jitter /
//! bandwidth / duplication / corruption / partition — the class of conditions the
//! instant, lossless [`crate::transport_mem`] never exercises.
//!
//! It is a discrete-event simulation: every inter-node send is scheduled into a
//! virtual-time min-heap by [`ImpairedLink`] (with the impairment applied at send
//! time), and [`Sim::run`] advances time to the next tick-or-delivery, steps the
//! affected nodes by hand ([`MeshNode::sim_tick`]/[`MeshNode::sim_deliver`]), and loops.
//! No wall clock, no `tokio`, no real sleeps — so a `(seed, scenario)` pair is
//! bit-for-bit reproducible and a whole stream simulates in microseconds.
//!
//! The template link model (serialized-link `tx = bytes*8000/bps` + RTT) is the same
//! one the picker-only `tests/sim.rs` already uses, lifted onto the real node loop.

use crate::config::{MeshConfig, Mode, PickerWeights, Role};
use crate::media::MediaSink;
use crate::node::{EdgeSigner, MeshNode, NodeStats};
use crate::signaling::BanList;
use crate::transport::{Channel, EngineEvent, Link};
use crate::types::{PeerId, Seq};
use crate::crypto;
use bytes::Bytes;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Impairment model
// ---------------------------------------------------------------------------

/// Per-link, per-channel network impairment. `Default` is a perfect wire (instant,
/// lossless, unmetered). Builders layer on adversity.
#[derive(Clone, Debug, Default)]
pub struct NetModel {
    /// Base one-way latency in ms.
    pub delay_ms: u64,
    /// Uniform extra latency in `[0, jitter_ms]` per message — reorders naturally.
    pub jitter_ms: u64,
    /// Probability in `[0,1]` a message is dropped outright.
    pub loss_prob: f64,
    /// Probability a delivered message is also duplicated.
    pub dup_prob: f64,
    /// Serialized-link bandwidth in bits/sec; `0` = unmetered (no transfer time).
    pub bandwidth_bps: u64,
    /// Probability a delivered message has one byte flipped (exercises hash-verify /
    /// decode-rejects-garbage). Apply mainly to the bulk channel.
    pub corrupt_prob: f64,
}

impl NetModel {
    /// A perfect wire: instant, lossless, unmetered.
    pub fn perfect() -> Self {
        Self::default()
    }
    /// A wire with a base latency (ms) and bandwidth (bps; `0` = unmetered).
    pub fn link(delay_ms: u64, bandwidth_bps: u64) -> Self {
        Self { delay_ms, bandwidth_bps, ..Self::default() }
    }
    pub fn loss(mut self, p: f64) -> Self {
        self.loss_prob = p;
        self
    }
    pub fn jitter(mut self, ms: u64) -> Self {
        self.jitter_ms = ms;
        self
    }
    pub fn dup(mut self, p: f64) -> Self {
        self.dup_prob = p;
        self
    }
    pub fn corrupt(mut self, p: f64) -> Self {
        self.corrupt_prob = p;
        self
    }
}

// ---------------------------------------------------------------------------
// Discrete-event scheduler
// ---------------------------------------------------------------------------

/// One scheduled delivery. Ordered by `(deliver_at, seq)` — `seq` is a monotonic
/// tiebreaker so same-time events resolve in a deterministic (enqueue) order.
struct Scheduled {
    deliver_at: u64,
    seq: u64,
    to: PeerId,
    event: EngineEvent,
}
impl PartialEq for Scheduled {
    fn eq(&self, o: &Self) -> bool {
        self.deliver_at == o.deliver_at && self.seq == o.seq
    }
}
impl Eq for Scheduled {}
impl Ord for Scheduled {
    fn cmp(&self, o: &Self) -> Ordering {
        // Reversed so `BinaryHeap` (a max-heap) pops the *earliest* delivery first.
        o.deliver_at.cmp(&self.deliver_at).then_with(|| o.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Shared mutable simulation state — the event heap + virtual clock + one RNG for all
/// impairment decisions (single RNG keeps a scenario deterministic given a seed, since
/// the driver processes sends in a fixed order).
struct NetState {
    now_ms: u64,
    next_seq: u64,
    heap: BinaryHeap<Scheduled>,
    rng: ChaCha8Rng,
    /// Serialized-link "free at" time per directed link, so bandwidth queues rather
    /// than teleports (a large segment's chunks stack up behind each other).
    link_busy: HashMap<(PeerId, PeerId), u64>,
}

impl NetState {
    fn push(&mut self, deliver_at: u64, to: PeerId, event: EngineEvent) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Scheduled { deliver_at, seq, to, event });
    }
}

// ---------------------------------------------------------------------------
// Impaired link
// ---------------------------------------------------------------------------

/// A [`Link`] that, instead of delivering instantly, schedules delivery into the shared
/// [`NetState`] heap with its channel's [`NetModel`] applied.
struct ImpairedLink {
    from: PeerId,
    to: PeerId,
    net: Arc<Mutex<NetState>>,
    ctrl: NetModel,
    bulk: NetModel,
}

impl Link for ImpairedLink {
    fn remote(&self) -> PeerId {
        self.to
    }
    fn send(&self, channel: Channel, bytes: Vec<u8>) {
        let model = match channel {
            Channel::Ctrl => &self.ctrl,
            Channel::Bulk => &self.bulk,
        };
        let mut s = self.net.lock().unwrap();
        // Loss.
        if model.loss_prob > 0.0 && s.rng.gen::<f64>() < model.loss_prob {
            return;
        }
        let now = s.now_ms;
        // Serialized-link bandwidth: this message can't start until the wire is free.
        let tx_ms = if model.bandwidth_bps > 0 {
            (bytes.len() as u64 * 8000) / model.bandwidth_bps
        } else {
            0
        };
        let start = s.link_busy.get(&(self.from, self.to)).copied().unwrap_or(0).max(now);
        s.link_busy.insert((self.from, self.to), start + tx_ms);
        let jitter =
            if model.jitter_ms > 0 { s.rng.gen_range(0..=model.jitter_ms) } else { 0 };
        let deliver_at = start + tx_ms + model.delay_ms + jitter;
        let copies = if model.dup_prob > 0.0 && s.rng.gen::<f64>() < model.dup_prob { 2 } else { 1 };
        for _ in 0..copies {
            let payload = if model.corrupt_prob > 0.0 && s.rng.gen::<f64>() < model.corrupt_prob {
                let mut b = bytes.clone();
                if !b.is_empty() {
                    let i = s.rng.gen_range(0..b.len());
                    b[i] ^= 0xFF;
                }
                b
            } else {
                bytes.clone()
            };
            s.push(deliver_at, self.to, EngineEvent::Inbound { peer: self.from, channel, bytes: payload });
        }
    }
    fn close(&self) {
        let mut s = self.net.lock().unwrap();
        let now = s.now_ms;
        s.push(now, self.to, EngineEvent::PeerDisconnected { peer: self.from });
    }
}

// ---------------------------------------------------------------------------
// Sink that models an in-order player (deterministic; no wall clock)
// ---------------------------------------------------------------------------

/// Tracks delivered segments so the play head slides like an in-order player. Deterministic
/// (unlike `tests/*`'s `Instant`-based sinks).
#[derive(Default)]
pub struct SimSink {
    segs: Mutex<BTreeSet<u64>>,
}
impl MediaSink for SimSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, _: Bytes) {
        self.segs.lock().unwrap().insert(seq);
    }
    fn on_play_head(&self) -> u64 {
        let s = self.segs.lock().unwrap();
        let mut h = 0u64;
        while s.contains(&h) {
            h += 1;
        }
        h
    }
}
impl SimSink {
    /// How many distinct segments have been delivered to the player.
    pub fn delivered(&self) -> usize {
        self.segs.lock().unwrap().len()
    }
    /// The contiguous-from-0 play head (the largest gap-free prefix).
    pub fn contiguous(&self) -> u64 {
        self.on_play_head()
    }
}

// ---------------------------------------------------------------------------
// The simulation driver
// ---------------------------------------------------------------------------

/// A discrete-event driver over real [`MeshNode`]s. Add nodes, connect them with a
/// [`NetModel`], schedule the workload, then [`run`](Sim::run) to a virtual deadline.
pub struct Sim {
    net: Arc<Mutex<NetState>>,
    nodes: HashMap<PeerId, MeshNode>,
    /// Deterministic node iteration order.
    order: Vec<PeerId>,
    tick_ms: u64,
    next_tick: HashMap<PeerId, u64>,
}

impl Sim {
    /// A fresh simulation with a seeded impairment RNG and the given node tick period.
    pub fn new(seed: u64, tick_ms: u64) -> Self {
        let net = NetState {
            now_ms: 0,
            next_seq: 0,
            heap: BinaryHeap::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            link_busy: HashMap::new(),
        };
        Self {
            net: Arc::new(Mutex::new(net)),
            nodes: HashMap::new(),
            order: Vec::new(),
            tick_ms,
            next_tick: HashMap::new(),
        }
    }

    /// Register a node under `id`. Its first tick fires at `tick_ms`.
    pub fn add(&mut self, id: PeerId, node: MeshNode) {
        self.order.push(id);
        self.next_tick.insert(id, self.tick_ms);
        self.nodes.insert(id, node);
    }

    /// Connect `a`↔`b` with one symmetric model on both channels/directions.
    pub fn connect(&mut self, a: PeerId, b: PeerId, model: &NetModel) {
        self.connect_ex(a, b, model.clone(), model.clone());
    }

    /// Connect `a`↔`b` with distinct control- and bulk-channel models (same both
    /// directions). Injects the `PeerConnected` on each side immediately (virtual t=0).
    pub fn connect_ex(&mut self, a: PeerId, b: PeerId, ctrl: NetModel, bulk: NetModel) {
        let link_a: Arc<dyn Link> = Arc::new(ImpairedLink {
            from: a,
            to: b,
            net: self.net.clone(),
            ctrl: ctrl.clone(),
            bulk: bulk.clone(),
        });
        let link_b: Arc<dyn Link> =
            Arc::new(ImpairedLink { from: b, to: a, net: self.net.clone(), ctrl, bulk });
        if let Some(n) = self.nodes.get_mut(&a) {
            n.sim_deliver(EngineEvent::PeerConnected { peer: b, link: link_a });
        }
        if let Some(n) = self.nodes.get_mut(&b) {
            n.sim_deliver(EngineEvent::PeerConnected { peer: a, link: link_b });
        }
    }

    /// Schedule an arbitrary local event onto `to` at virtual time `at_ms`.
    pub fn inject_at(&mut self, at_ms: u64, to: PeerId, event: EngineEvent) {
        self.net.lock().unwrap().push(at_ms, to, event);
    }

    /// Schedule `Produced(seq)` of a `seg_bytes`-sized deterministic segment onto the
    /// publisher `at_ms`.
    pub fn produce(&mut self, publisher: PeerId, at_ms: u64, seq: Seq, seg_bytes: usize) {
        let seg = Bytes::from(vec![(seq as u8).wrapping_mul(7).wrapping_add(1); seg_bytes]);
        let id = crypto::segment_id(&seg);
        self.inject_at(at_ms, publisher, EngineEvent::Produced { seq, id, bytes: seg });
    }

    /// Borrow a node for assertions.
    pub fn node(&self, id: &PeerId) -> &MeshNode {
        self.nodes.get(id).expect("unknown node")
    }

    /// Snapshot a node's stats.
    pub fn stats(&self, id: &PeerId) -> NodeStats {
        self.node(id).sim_stats()
    }

    /// Advance virtual time to `end_ms`, delivering scheduled events and firing node
    /// ticks in `(deliver_at, seq)` / node-order — deterministically.
    pub fn run(&mut self, end_ms: u64) {
        loop {
            let next_tick = self.order.iter().map(|p| self.next_tick[p]).min().unwrap_or(u64::MAX);
            let next_event =
                self.net.lock().unwrap().heap.peek().map(|e| e.deliver_at).unwrap_or(u64::MAX);
            let t = next_tick.min(next_event);
            if t == u64::MAX || t > end_ms {
                break;
            }
            self.net.lock().unwrap().now_ms = t;
            // Deliver everything due at `t` (lock only to pop — never while stepping a
            // node, since its sends re-enter the heap through the same lock).
            loop {
                let due = {
                    let mut s = self.net.lock().unwrap();
                    match s.heap.peek() {
                        Some(top) if top.deliver_at <= t => s.heap.pop(),
                        _ => None,
                    }
                };
                match due {
                    Some(sc) => {
                        if let Some(n) = self.nodes.get_mut(&sc.to) {
                            n.sim_deliver(sc.event);
                        }
                    }
                    None => break,
                }
            }
            // Fire every node whose tick is due at `t`.
            for p in self.order.clone() {
                if self.next_tick[&p] <= t {
                    if let Some(n) = self.nodes.get_mut(&p) {
                        n.sim_tick(t);
                    }
                    *self.next_tick.get_mut(&p).unwrap() += self.tick_ms;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared scenario fixtures
// ---------------------------------------------------------------------------

/// Publisher's stream id + signing seed (edges are signed with this; viewers verify
/// against [`pubkey`]).
pub const SID: [u8; 32] = [7u8; 32];
const SEED: [u8; 32] = [4u8; 32];

/// Signs live-edge gossip with the scenario publisher key.
pub struct SeedSigner;
impl EdgeSigner for SeedSigner {
    fn sign(&self, payload: &[u8]) -> [u8; 64] {
        crypto::sign_sr25519(&crypto::keypair_from_seed(&SEED), payload)
    }
}

/// The publisher's public key (viewers pass this to `with_publisher_key`).
pub fn pubkey() -> [u8; 32] {
    crypto::public_bytes(&crypto::keypair_from_seed(&SEED))
}

/// A live-stream `MeshConfig` with the given role, segment duration, and window.
pub fn cfg(role: Role, seg_ms: u64, window: u32, tick_ms: u64) -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role,
        window,
        tick: std::time::Duration::from_millis(tick_ms),
        seg_ms,
        upload_budget_bps: 100_000_000,
        weights: PickerWeights::default(),
    }
}

// ---------------------------------------------------------------------------
// Scenarios (the hardening suite grows here)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod scenarios {
    use super::*;

    const SEG_BYTES: usize = 60_000;

    /// Build a 1-publisher + 1-viewer sim wired by `ctrl`/`bulk` models. Returns
    /// `(sim, pubid, vid, shared BanList)`.
    fn pub_viewer(seed: u64, ctrl: NetModel, bulk: NetModel) -> (Sim, PeerId, PeerId, BanList) {
        let mut sim = Sim::new(seed, 100);
        let pubid = PeerId::from_u64(1);
        let vid = PeerId::from_u64(2);
        let bans = BanList::new();

        let publisher =
            MeshNode::new_live_publisher(pubid, cfg(Role::Publisher, 400, 16, 100), SEG_BYTES as u64, Arc::new(SimSink::default()))
                .with_stream_id(SID)
                .with_edge_signer(Arc::new(SeedSigner));
        let viewer =
            MeshNode::new_viewer(vid, cfg(Role::Viewer, 400, 16, 100), SEG_BYTES as u64, Arc::new(SimSink::default()), HashMap::new(), 0)
                .with_stream_id(SID)
                .with_publisher_key(pubkey())
                .with_ban_list(bans.clone());
        sim.add(pubid, publisher);
        sim.add(vid, viewer);
        sim.connect_ex(pubid, vid, ctrl, bulk);
        (sim, pubid, vid, bans)
    }

    /// TARGET #1 — an HONEST publisher on a LOSSY link must not be banned. Loss makes the
    /// viewer miss pushed segments, so it PULLS them; on the unreliable bulk channel a
    /// single lost 16 KiB chunk means the whole segment never completes and just times out
    /// at the flat `PENDING_TIMEOUT_MS` (2 s). Reputation decays `×0.8` per timeout but
    /// heals only `+0.02` per verified delivery, so on a bad-but-not-malicious link it
    /// crosses the `0.05` floor and the peer is wrongly banned for 600 s. (Expected to
    /// FAIL until the timeout scales with the peer's measured throughput / the
    /// decay-vs-heal asymmetry softens.)
    #[test]
    fn honest_lossy_peer_is_not_wrongly_banned() {
        let ctrl = NetModel::link(20, 0); // control plane fast + reliable (real WebRTC ctrl is reliable)
        let bulk = NetModel::link(20, 8_000_000).loss(0.4); // fast link, but 40% bulk-chunk loss
        let (mut sim, pubid, vid, bans) = pub_viewer(1, ctrl, bulk);
        // A live stream the viewer keeps trying to follow (and keeps missing chunks of).
        for i in 0..80u64 {
            sim.produce(pubid, 100 + i * 400, i, SEG_BYTES);
        }
        sim.run(40_000);

        let st = sim.stats(&vid);
        let rep = sim.node(&vid).sim_reputation(&pubid);
        eprintln!(
            "[target#1] delivered={} peer_bytes={} pending={} rep={:?} banned={} banlist={}",
            st.delivered,
            st.peer_bytes,
            st.pending_entries,
            rep,
            sim.node(&vid).sim_banned(&pubid),
            bans.contains(&pubid),
        );
        assert!(
            !sim.node(&vid).sim_banned(&pubid) && !bans.contains(&pubid),
            "honest peer on a lossy link was wrongly banned — reputation={rep:?}",
        );
        // It should still make progress (deprioritized, not cut off).
        assert!(st.delivered > 0, "banned/cut off the only peer, so nothing was delivered");
    }

    /// A star: 1 live publisher + `viewers` viewers on the same `(ctrl, bulk)` model.
    /// Returns `(sim, pubid, [(vid, sink)], shared BanList)`.
    fn star(
        seed: u64,
        viewers: usize,
        ctrl: NetModel,
        bulk: NetModel,
    ) -> (Sim, PeerId, Vec<(PeerId, Arc<SimSink>)>, BanList) {
        let mut sim = Sim::new(seed, 100);
        let pubid = PeerId::from_u64(1);
        let bans = BanList::new();
        let publisher = MeshNode::new_live_publisher(
            pubid,
            cfg(Role::Publisher, 400, 16, 100),
            SEG_BYTES as u64,
            Arc::new(SimSink::default()),
        )
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
        sim.add(pubid, publisher);
        let mut vs = Vec::new();
        for i in 0..viewers {
            let vid = PeerId::from_u64(100 + i as u64);
            let sink = Arc::new(SimSink::default());
            let viewer = MeshNode::new_viewer(
                vid,
                cfg(Role::Viewer, 400, 16, 100),
                SEG_BYTES as u64,
                sink.clone(),
                HashMap::new(),
                0,
            )
            .with_stream_id(SID)
            .with_publisher_key(pubkey())
            .with_ban_list(bans.clone());
            sim.add(vid, viewer);
            sim.connect_ex(pubid, vid, ctrl.clone(), bulk.clone());
            vs.push((vid, sink));
        }
        (sim, pubid, vs, bans)
    }

    /// SAFETY-INVARIANT MATRIX — a sweep of network conditions on a star, every one of
    /// which must uphold the invariants that do NOT depend on how harsh the link is: the
    /// honest publisher is never banned, reassembly memory stays bounded, and every viewer
    /// makes at least some progress (never a total stall). Delivery *rate* degrades with
    /// the link (printed for observation), but these safety properties never do.
    /// (`corrupt` is deliberately absent — a peer emitting bad bytes is suspicious by
    /// definition, and DTLS makes in-transit corruption unreachable at this layer; the
    /// hash-verify/ban path is covered by the adversarial tests.)
    #[ignore = "netsim hardening suite — run via test-all.sh's netsim step"]
    #[test]
    fn matrix_star_safety_invariants() {
        let k = 50u64;
        let conditions: &[(&str, NetModel, NetModel)] = &[
            ("clean", NetModel::link(20, 0), NetModel::link(20, 0)),
            ("loss5", NetModel::link(20, 0), NetModel::link(20, 0).loss(0.05)),
            ("loss20", NetModel::link(20, 0), NetModel::link(20, 0).loss(0.20)),
            ("latency150", NetModel::link(150, 0), NetModel::link(150, 0)),
            ("jitter120", NetModel::link(40, 0), NetModel::link(40, 0).jitter(120)),
            ("bw_tight", NetModel::link(20, 0), NetModel::link(20, 2_000_000)),
            ("dup10", NetModel::link(20, 0), NetModel::link(20, 0).dup(0.10)),
            (
                "combined",
                NetModel::link(80, 0).loss(0.02),
                NetModel::link(80, 4_000_000).loss(0.08).jitter(40).dup(0.02),
            ),
        ];
        for (name, ctrl, bulk) in conditions {
            for seed in 0..3u64 {
                let (mut sim, pubid, vs, bans) = star(seed, 3, ctrl.clone(), bulk.clone());
                for i in 0..k {
                    sim.produce(pubid, 100 + i * 300, i, SEG_BYTES);
                }
                sim.run(60_000);
                let mut min_deliv = usize::MAX;
                for (vid, sink) in &vs {
                    let st = sim.stats(vid);
                    assert!(
                        !sim.node(vid).sim_banned(&pubid) && !bans.contains(&pubid),
                        "{name}/{seed}: honest publisher wrongly banned by {vid:?}",
                    );
                    assert!(
                        st.reasm_bytes <= 32 * 1024 * 1024 && st.reasm_entries <= 64,
                        "{name}/{seed}: reassembly unbounded ({} B, {} entries)",
                        st.reasm_bytes,
                        st.reasm_entries,
                    );
                    assert!(
                        sink.delivered() > 0,
                        "{name}/{seed}: viewer {vid:?} made zero progress (total stall)",
                    );
                    min_deliv = min_deliv.min(sink.delivered());
                }
                eprintln!("[matrix] {name:<11} seed={seed} min_delivered={min_deliv}/{k}");
            }
        }
    }

    /// A 3-hop relay: publisher P → seed relay S (`Role::Seed`, never chokes, reshares +
    /// re-gossips) → leaf viewer L, where L has NO link to P — it learns the edge only
    /// via S's gossip and gets bytes only via S's reshare (the decentralized-TURN
    /// substitute). Impair BOTH hops and verify the multi-hop stream still delivers and
    /// neither honest peer is wrongly banned on the relay path.
    #[ignore = "netsim hardening suite — run via test-all.sh's netsim step"]
    #[test]
    fn relay_chain_survives_lossy_hops() {
        let mut sim = Sim::new(3, 100);
        let (pid, sid, lid) = (PeerId::from_u64(1), PeerId::from_u64(2), PeerId::from_u64(3));
        let bans = BanList::new();

        let publisher = MeshNode::new_live_publisher(
            pid,
            cfg(Role::Publisher, 400, 16, 100),
            SEG_BYTES as u64,
            Arc::new(SimSink::default()),
        )
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
        let seed = MeshNode::new_seed(sid, cfg(Role::Seed, 400, 16, 100), SEG_BYTES as u64, HashMap::new(), 0)
            .with_stream_id(SID)
            .with_publisher_key(pubkey())
            .with_ban_list(bans.clone());
        let leaf_sink = Arc::new(SimSink::default());
        let leaf = MeshNode::new_viewer(
            lid,
            cfg(Role::Viewer, 400, 16, 100),
            SEG_BYTES as u64,
            leaf_sink.clone(),
            HashMap::new(),
            0,
        )
        .with_stream_id(SID)
        .with_publisher_key(pubkey())
        .with_ban_list(bans.clone());
        sim.add(pid, publisher);
        sim.add(sid, seed);
        sim.add(lid, leaf);
        // P↔S mildly lossy; S↔L (the last mile) lossier.
        sim.connect_ex(pid, sid, NetModel::link(20, 0), NetModel::link(20, 0).loss(0.05));
        sim.connect_ex(sid, lid, NetModel::link(30, 0), NetModel::link(30, 0).loss(0.15));
        for i in 0..50u64 {
            sim.produce(pid, 100 + i * 300, i, SEG_BYTES);
        }
        sim.run(60_000);

        eprintln!(
            "[relay] leaf_delivered={} seed_banned_pub={} leaf_banned_seed={} banlist={}",
            leaf_sink.delivered(),
            sim.node(&sid).sim_banned(&pid),
            sim.node(&lid).sim_banned(&sid),
            bans.len(),
        );
        assert!(leaf_sink.delivered() > 0, "relay leaf received nothing over the multi-hop path");
        assert!(!sim.node(&lid).sim_banned(&sid), "leaf wrongly banned the honest seed relay");
        assert!(!sim.node(&sid).sim_banned(&pid), "seed wrongly banned the honest publisher");
        assert_eq!(bans.len(), 0, "an honest peer was banned on the relay path");
    }

    // ---- property fuzzing: random bad-but-usable links must uphold the invariants ----
    use proptest::prelude::*;

    proptest! {
        // Bounded cases so the (fast, deterministic) fuzz stays in the core suite; the
        // tuning bench will run wider sweeps under `--features netsim`.
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// Over a random bad-but-usable bulk link, a 2-viewer star must never wrongly ban
        /// the honest publisher, never blow the reassembly caps, and never totally stall.
        /// proptest shrinks any violating `(seed, loss, delay, jitter, dup, bw)` to a
        /// minimal repro.
        #[ignore = "netsim hardening suite — run via test-all.sh's netsim step"]
        #[test]
        fn fuzz_star_safety(
            seed in 0u64..10_000,
            loss in 0.0f64..0.30,
            delay in 0u64..180,
            jitter in 0u64..140,
            dup in 0.0f64..0.15,
            bw in prop::sample::select(vec![0u64, 3_000_000, 6_000_000]),
        ) {
            let ctrl = NetModel::link(delay, 0);
            let bulk = NetModel::link(delay, bw).loss(loss).jitter(jitter).dup(dup);
            let (mut sim, pubid, vs, bans) = star(seed, 2, ctrl, bulk);
            for i in 0..30u64 {
                sim.produce(pubid, 100 + i * 300, i, SEG_BYTES);
            }
            sim.run(45_000);
            for (vid, sink) in &vs {
                let st = sim.stats(vid);
                prop_assert!(
                    !sim.node(vid).sim_banned(&pubid),
                    "honest publisher banned — loss={loss:.3} delay={delay} jitter={jitter} bw={bw}",
                );
                prop_assert!(
                    st.reasm_bytes <= 32 * 1024 * 1024 && st.reasm_entries <= 64,
                    "reassembly unbounded — {} B / {} entries", st.reasm_bytes, st.reasm_entries,
                );
                prop_assert!(
                    sink.delivered() > 0,
                    "total stall — loss={loss:.3} delay={delay} jitter={jitter} dup={dup:.3} bw={bw}",
                );
            }
            prop_assert_eq!(bans.len(), 0, "an honest peer was banned");
        }
    }

    /// TUNING/characterization bench (prints, doesn't assert): sweep the bulk-loss rate on
    /// a 1-viewer star and report delivery %, the honest publisher's final reputation, and
    /// whether it got banned — turning constant-choice into data instead of guesswork. It
    /// documents the protocol's operating envelope AND validates the target-#1 fix
    /// quantitatively: across the whole loss range the honest peer stays usable and unbanned
    /// (before the fix it crossed the 0.05 ban floor by ~20% loss). Run with `--nocapture`.
    #[ignore = "characterization bench (prints metrics) — run via test-all.sh's netsim step"]
    #[test]
    fn bench_loss_sweep() {
        let k = 40u64;
        eprintln!("[bench] bulk-loss sweep (1-viewer star, avg of 3 seeds, k={k})");
        eprintln!("[bench]  loss%   delivered%   final_rep   banned");
        for loss_pct in [0u64, 5, 10, 15, 20, 30, 40, 50] {
            let (mut deliv_frac, mut rep_sum) = (0.0f64, 0.0f64);
            let mut banned = 0u32;
            for seed in 0..3u64 {
                let bulk = NetModel::link(30, 0).loss(loss_pct as f64 / 100.0);
                let (mut sim, pubid, vs, bans) = star(seed, 1, NetModel::link(20, 0), bulk);
                for i in 0..k {
                    sim.produce(pubid, 100 + i * 300, i, SEG_BYTES);
                }
                sim.run(50_000);
                let (vid, sink) = &vs[0];
                deliv_frac += sink.delivered() as f64 / k as f64;
                rep_sum += sim.node(vid).sim_reputation(&pubid).unwrap_or(0.0);
                if bans.contains(&pubid) {
                    banned += 1;
                }
            }
            eprintln!(
                "[bench]  {:>4}%   {:>9.0}%   {:>9.3}   {:>3}/3",
                loss_pct,
                deliv_frac / 3.0 * 100.0,
                rep_sum / 3.0,
                banned,
            );
        }
    }
}
