//! Scale simulation: wire many real `MeshNode`s over the in-memory transport and run
//! the full live-publish + signed-edge-gossip (#17) + push-pull (#20) + relay stack at
//! load, then report mesh metrics.
//!
//! Deterministic *outcomes* are asserted (every viewer receives every segment, bytes come
//! from peers, zero corruption); the *latency* numbers (time-to-first-segment, wall-clock)
//! are printed for benchmarking — run with `--nocapture` to see the table. Topologies:
//!   * **star** — one live publisher fans out to N viewers (the origin never chokes).
//!   * **seed-relay tree** — publisher → seed relays → leaves, where leaves have NO link
//!     to the origin and learn the edge only via the relay's re-gossip and get bytes only
//!     via its reshare (seeds use `Role::Seed`, which never chokes, so they serve all
//!     their leaves — the realistic relay tier from M3/M4).

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::{EdgeSigner, MeshNode};
use unstation_core::transport::EngineEvent;
use unstation_core::transport_mem::wire;
use unstation_core::types::PeerId;

const SID: [u8; 32] = [5u8; 32];
const SEED: [u8; 32] = [4u8; 32];
const TICK: Duration = Duration::from_millis(10);
const SEG_LEN: usize = 4_000;

/// Signs live-edge gossip with an sr25519 key derived from a seed (publisher side).
struct SeedSigner;
impl EdgeSigner for SeedSigner {
    fn sign(&self, payload: &[u8]) -> [u8; 64] {
        crypto::sign_sr25519(&crypto::keypair_from_seed(&SEED), payload)
    }
}

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

/// Records delivered segments (so the play head slides like an in-order player) and the
/// instant the FIRST segment arrived (time-to-first-segment).
#[derive(Default)]
struct MetricSink {
    segs: Mutex<std::collections::BTreeSet<u64>>,
    first: Mutex<Option<Instant>>,
}
impl MediaSink for MetricSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, _: Bytes) {
        let mut f = self.first.lock().unwrap();
        if f.is_none() {
            *f = Some(Instant::now());
        }
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
impl MetricSink {
    fn first(&self) -> Option<Instant> {
        *self.first.lock().unwrap()
    }
}

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role,
        window: 16,
        tick: TICK,
        seg_ms: 500,
        upload_budget_bps: 100_000_000,
        weights: PickerWeights::default(),
    }
}

fn pubkey() -> [u8; 32] {
    crypto::public_bytes(&crypto::keypair_from_seed(&SEED))
}

/// Spawn the live publisher that signs + gossips each produced edge, and a task that
/// produces `k` segments after a short connect delay (so subscriptions are registered).
fn spawn_publisher(
    pubid: PeerId,
    ptx: &mpsc::UnboundedSender<EngineEvent>,
    prx: mpsc::UnboundedReceiver<EngineEvent>,
    k: usize,
) {
    let publisher = MeshNode::new_live_publisher(pubid, cfg(Role::Publisher), SEG_LEN as u64, Arc::new(NullSink))
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
    tokio::spawn(publisher.run(prx, TICK, None));

    let ptx = ptx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        for i in 0..k as u64 {
            let seg = Bytes::from(vec![(i as u8).wrapping_mul(7).wrapping_add(1); SEG_LEN]);
            let id = crypto::segment_id(&seg);
            let _ = ptx.send(EngineEvent::Produced { seq: i, id, bytes: seg });
        }
    });
}

struct Metrics {
    label: &'static str,
    viewers: usize,
    k: usize,
    delivered_all: usize,
    total_peer_bytes: u64,
    hash_failures: u64,
    ttfs_ms: Vec<u128>,
    wall_ms: u128,
}
impl Metrics {
    fn report(&self) {
        let mut t = self.ttfs_ms.clone();
        t.sort_unstable();
        let median = t.get(t.len() / 2).copied().unwrap_or(0);
        let max = t.last().copied().unwrap_or(0);
        println!(
            "[scale:{}] viewers={} k={} delivered_all={}/{} offload_bytes={} hash_failures={} ttfs_median={}ms ttfs_max={}ms wall={}ms",
            self.label, self.viewers, self.k, self.delivered_all, self.viewers,
            self.total_peer_bytes, self.hash_failures, median, max, self.wall_ms,
        );
    }
}

/// One live publisher fanning out to `viewers` direct viewers (star).
async fn run_star(viewers: usize, k: usize) -> Metrics {
    let pk = pubkey();
    let pubid = PeerId::from_u64(1);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();

    let mut handles = Vec::new();
    let mut sinks = Vec::new();
    for v in 0..viewers {
        let vid = PeerId::from_u64(1_000 + v as u64);
        let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
        let (lp, lv) = wire(pubid, ptx.clone(), vid, vtx.clone());
        ptx.send(EngineEvent::PeerConnected { peer: vid, link: lp }).unwrap();
        vtx.send(EngineEvent::PeerConnected { peer: pubid, link: lv }).unwrap();
        let sink = Arc::new(MetricSink::default());
        sinks.push(sink.clone());
        let viewer = MeshNode::new_viewer(vid, cfg(Role::Viewer), SEG_LEN as u64, sink, HashMap::new(), 0)
            .with_stream_id(SID)
            .with_publisher_key(pk);
        handles.push(tokio::spawn(viewer.run(vrx, TICK, Some(k))));
    }

    let start = Instant::now();
    spawn_publisher(pubid, &ptx, prx, k);

    let (mut delivered_all, mut total_peer_bytes, mut hash_failures) = (0, 0u64, 0u64);
    for (v, h) in handles.into_iter().enumerate() {
        let stats = tokio::time::timeout(Duration::from_secs(40), h)
            .await
            .unwrap_or_else(|_| panic!("star viewer {v} timed out"))
            .expect("viewer task panicked");
        if stats.delivered == k {
            delivered_all += 1;
        }
        total_peer_bytes += stats.peer_bytes;
        hash_failures += stats.hash_failures;
    }
    let wall_ms = start.elapsed().as_millis();
    let ttfs_ms = sinks.iter().filter_map(|s| s.first().map(|f| f.duration_since(start).as_millis())).collect();
    let _ = ptx.send(EngineEvent::Stop);

    Metrics { label: "star", viewers, k, delivered_all, total_peer_bytes, hash_failures, ttfs_ms, wall_ms }
}

/// publisher → `relays` seed nodes → `leaves_per` leaves each. Leaves have NO link to the
/// origin: they learn the edge via the seed's re-gossip and get bytes via its reshare.
async fn run_seed_tree(relays: usize, leaves_per: usize, k: usize) -> Metrics {
    let pk = pubkey();
    let pubid = PeerId::from_u64(1);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();

    let mut leaf_handles = Vec::new();
    let mut leaf_sinks = Vec::new();
    let mut stoppers = Vec::new();

    for r in 0..relays {
        let seedid = PeerId::from_u64(100 + r as u64);
        let (stx, srx) = mpsc::unbounded_channel::<EngineEvent>();
        let (lp, ls) = wire(pubid, ptx.clone(), seedid, stx.clone());
        ptx.send(EngineEvent::PeerConnected { peer: seedid, link: lp }).unwrap();
        stx.send(EngineEvent::PeerConnected { peer: pubid, link: ls }).unwrap();

        for l in 0..leaves_per {
            let leafid = PeerId::from_u64(10_000 + (r * 100 + l) as u64);
            let (ltx, lrx) = mpsc::unbounded_channel::<EngineEvent>();
            let (sl, lsl) = wire(seedid, stx.clone(), leafid, ltx.clone());
            stx.send(EngineEvent::PeerConnected { peer: leafid, link: sl }).unwrap();
            ltx.send(EngineEvent::PeerConnected { peer: seedid, link: lsl }).unwrap();
            let sink = Arc::new(MetricSink::default());
            leaf_sinks.push(sink.clone());
            let leaf = MeshNode::new_viewer(leafid, cfg(Role::Viewer), SEG_LEN as u64, sink, HashMap::new(), 0)
                .with_stream_id(SID)
                .with_publisher_key(pk);
            leaf_handles.push(tokio::spawn(leaf.run(lrx, TICK, Some(k))));
        }

        // A seed relay: never chokes, reshares + re-gossips. Runs until we stop it.
        let seed = MeshNode::new_seed(seedid, cfg(Role::Seed), SEG_LEN as u64, HashMap::new(), 0)
            .with_stream_id(SID)
            .with_publisher_key(pk);
        tokio::spawn(seed.run(srx, TICK, None));
        stoppers.push(stx);
    }

    let start = Instant::now();
    spawn_publisher(pubid, &ptx, prx, k);

    let leaves = leaf_handles.len();
    let (mut delivered_all, mut total_peer_bytes, mut hash_failures) = (0, 0u64, 0u64);
    for (l, h) in leaf_handles.into_iter().enumerate() {
        let stats = tokio::time::timeout(Duration::from_secs(40), h)
            .await
            .unwrap_or_else(|_| panic!("leaf {l} timed out"))
            .expect("leaf task panicked");
        if stats.delivered == k {
            delivered_all += 1;
        }
        total_peer_bytes += stats.peer_bytes;
        hash_failures += stats.hash_failures;
    }
    let wall_ms = start.elapsed().as_millis();
    let ttfs_ms = leaf_sinks.iter().filter_map(|s| s.first().map(|f| f.duration_since(start).as_millis())).collect();
    let _ = ptx.send(EngineEvent::Stop);
    for s in stoppers {
        let _ = s.send(EngineEvent::Stop);
    }

    Metrics { label: "seed-tree", viewers: leaves, k, delivered_all, total_peer_bytes, hash_failures, ttfs_ms, wall_ms }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn star_fanout_delivers_at_scale() {
    let (viewers, k) = (40usize, 12usize);
    let m = run_star(viewers, k).await;
    m.report();
    assert_eq!(m.delivered_all, viewers, "every viewer in the star must receive all {k} segments");
    assert_eq!(m.hash_failures, 0, "no corruption across the fan-out");
    assert!(m.total_peer_bytes > 0, "viewers were served by the publisher peer, not an origin floor");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_relay_tree_delivers_at_scale() {
    let (relays, leaves_per, k) = (6usize, 4usize, 10usize);
    let m = run_seed_tree(relays, leaves_per, k).await;
    m.report();
    assert_eq!(
        m.delivered_all,
        relays * leaves_per,
        "every leaf must receive all {k} segments via its seed relay (edge gossip + reshare, no origin link)",
    );
    assert_eq!(m.hash_failures, 0, "no corruption through the relay tier");
    assert!(m.total_peer_bytes > 0, "leaves were served entirely by peers");
}
