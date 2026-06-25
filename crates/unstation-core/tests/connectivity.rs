//! Connectivity tests: multi-peer mesh formation, resilience to churn, and the
//! signaling/discovery layer — all over the deterministic in-memory transport and
//! statement store (no real network), so they run in CI.
//!
//! Covers the failure modes that broke real LAN sessions: a peer vanishing mid
//! stream, a peer joining late, duplicate connects (glare), an adversarial peer
//! feeding corrupt bytes, presence that must expire, and per-recipient signaling.

use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
use unstation_core::clock::VirtualClock;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::{EdgeSigner, MeshNode};
use unstation_core::protocol::MeshMsg;
use unstation_core::signaling::SignalMsg;
use unstation_core::statement_store_mem::{MemStatementStore, StatementSignaling};
use unstation_core::transport::{Channel, EngineEvent, Link};
use unstation_core::transport_mem::wire;
use unstation_core::types::{PeerId, SegmentId, Seq, StreamId};

// ---- shared test rig ----

#[derive(Default)]
struct Rec {
    segs: Mutex<BTreeMap<u64, Bytes>>,
}
impl MediaSink for Rec {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, bytes: Bytes) {
        self.segs.lock().unwrap().insert(seq, bytes);
    }
    fn on_play_head(&self) -> u64 {
        0
    }
}
impl Rec {
    fn count(&self) -> usize {
        self.segs.lock().unwrap().len()
    }
    fn get(&self, s: u64) -> Option<Bytes> {
        self.segs.lock().unwrap().get(&s).cloned()
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

/// A `Link` that records every `send` instead of delivering it — lets a test observe
/// exactly what a node chose to transmit (used to prove a push happened with no `Want`).
struct RecordingLink {
    remote: PeerId,
    sent: Arc<Mutex<Vec<(Channel, Vec<u8>)>>>,
}
impl Link for RecordingLink {
    fn remote(&self) -> PeerId {
        self.remote
    }
    fn send(&self, channel: Channel, bytes: Vec<u8>) {
        self.sent.lock().unwrap().push((channel, bytes));
    }
}

/// Signs live-edge gossip with an sr25519 keypair derived from a 32-byte seed. Holds the
/// seed (not the keypair) so the test never has to name `schnorrkel::Keypair`.
struct SeedSigner {
    seed: [u8; 32],
}
impl EdgeSigner for SeedSigner {
    fn sign(&self, payload: &[u8]) -> [u8; 64] {
        crypto::sign_sr25519(&crypto::keypair_from_seed(&self.seed), payload)
    }
}

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Vod,
        role,
        window: 16,
        tick: Duration::from_millis(10),
        seg_ms: 500,
        upload_budget_bps: 50_000_000,
        weights: PickerWeights::default(),
    }
}

/// A `n`-segment VOD: the raw bytes and the authenticated seq→id map a viewer needs.
fn make_vod(n: usize, seg_len: usize) -> (Vec<Bytes>, HashMap<Seq, SegmentId>) {
    let segs: Vec<Bytes> = (0..n)
        .map(|i| Bytes::from(vec![(i as u8).wrapping_mul(7).wrapping_add(1); seg_len]))
        .collect();
    let ids = segs
        .iter()
        .enumerate()
        .map(|(i, b)| (i as Seq, crypto::segment_id(b)))
        .collect();
    (segs, ids)
}

/// Spawn a publisher preloaded with `segs`, wired to `viewid`. Returns the publisher's
/// own inbox sender (so the test can `Stop` it) and the link to hand the viewer.
fn spawn_publisher(
    pubid: PeerId,
    viewid: PeerId,
    view_tx: &UnboundedSender<EngineEvent>,
    segs: Vec<Bytes>,
    seg_len: usize,
) -> UnboundedSender<EngineEvent> {
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (link_for_pub, link_for_view) = wire(pubid, ptx.clone(), viewid, view_tx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: link_for_pub }).unwrap();
    view_tx.send(EngineEvent::PeerConnected { peer: pubid, link: link_for_view }).unwrap();
    let publisher =
        MeshNode::new_publisher(pubid, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink), segs);
    tokio::spawn(publisher.run(prx, Duration::from_millis(10), None));
    ptx
}

#[tokio::test]
async fn viewer_fetches_from_two_publishers() {
    let (n, seg_len) = (12usize, 24_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let viewid = PeerId::from_u64(100);
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();

    let p1 = spawn_publisher(PeerId::from_u64(1), viewid, &vtx, segs.clone(), seg_len);
    let p2 = spawn_publisher(PeerId::from_u64(2), viewid, &vtx, segs.clone(), seg_len);

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(8), viewer.run(vrx, Duration::from_millis(10), Some(n)))
        .await
        .expect("two-publisher mesh should deliver within 8s");

    assert_eq!(stats.delivered, n, "all segments delivered across two peers");
    assert_eq!(stats.hash_failures, 0, "no corruption");
    assert_eq!(rec.count(), n, "all fed to the player");
    let _ = p1.send(EngineEvent::Stop);
    let _ = p2.send(EngineEvent::Stop);
}

#[tokio::test]
async fn recovers_after_peer_disconnect_midstream() {
    let (n, seg_len) = (16usize, 20_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let viewid = PeerId::from_u64(100);
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();

    let p1id = PeerId::from_u64(1);
    let p1 = spawn_publisher(p1id, viewid, &vtx, segs.clone(), seg_len);
    let p2 = spawn_publisher(PeerId::from_u64(2), viewid, &vtx, segs.clone(), seg_len);

    // Drop publisher #1 shortly after the stream starts — the viewer must finish from #2.
    let vtx_drop = vtx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = vtx_drop.send(EngineEvent::PeerDisconnected { peer: p1id });
        let _ = p1.send(EngineEvent::Stop);
    });

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(8), viewer.run(vrx, Duration::from_millis(10), Some(n)))
        .await
        .expect("viewer should recover from a mid-stream peer drop");

    assert_eq!(stats.delivered, n, "all segments delivered despite a peer vanishing");
    assert_eq!(stats.hash_failures, 0);
    let _ = p2.send(EngineEvent::Stop);
}

#[tokio::test]
async fn late_joining_publisher_serves() {
    let (n, seg_len) = (10usize, 20_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let viewid = PeerId::from_u64(100);
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();

    // The viewer starts with NO peers; a publisher appears only after a delay.
    let vtx_late = vtx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        spawn_publisher(PeerId::from_u64(1), viewid, &vtx_late, segs, seg_len);
    });

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(8), viewer.run(vrx, Duration::from_millis(10), Some(n)))
        .await
        .expect("viewer should pick up a late-joining publisher");
    assert_eq!(stats.delivered, n, "late publisher's segments all delivered");
}

#[tokio::test]
async fn duplicate_peer_connected_is_safe() {
    // Glare: the same peer is announced twice. The node must not double-count or panic.
    let (n, seg_len) = (8usize, 16_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let viewid = PeerId::from_u64(100);
    let pubid = PeerId::from_u64(1);
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();

    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (l_pub, l_view) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: l_pub }).unwrap();
    // Two PeerConnected for the same peer (a second link object).
    let (_l_pub2, l_view2) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: l_view }).unwrap();
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: l_view2 }).unwrap();
    let publisher = MeshNode::new_publisher(pubid, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink), segs);
    tokio::spawn(publisher.run(prx, Duration::from_millis(10), None));

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(6), viewer.run(vrx, Duration::from_millis(10), Some(n)))
        .await
        .expect("duplicate connects must not wedge delivery");
    assert_eq!(stats.delivered, n);
    let _ = ptx.send(EngineEvent::Stop);
}

#[tokio::test]
async fn corrupt_chunk_is_rejected_then_recovered() {
    // An adversarial peer injects wrong bytes for seq 0. The viewer must reject it
    // (hash mismatch), keep the real bytes out of the player, and re-fetch the
    // correct segment from the honest publisher.
    let (n, seg_len) = (6usize, 20_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let viewid = PeerId::from_u64(100);
    let badid = PeerId::from_u64(9);
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();

    let honest = spawn_publisher(PeerId::from_u64(1), viewid, &vtx, segs.clone(), seg_len);

    // Forge a complete-but-wrong chunk for seq 0 from a peer the viewer never dialed.
    let forged = MeshMsg::SegmentData {
        seq: 0,
        track_id: 0,
        total_len: seg_len as u32,
        offset: 0,
        bytes: vec![0xFF; seg_len],
    }
    .encode();
    vtx.send(EngineEvent::Inbound { peer: badid, channel: Channel::Bulk, bytes: forged }).unwrap();

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(8), viewer.run(vrx, Duration::from_millis(10), Some(n)))
        .await
        .expect("viewer should recover from a forged chunk");

    assert_eq!(stats.delivered, n, "every segment ultimately delivered");
    assert!(stats.hash_failures >= 1, "the forged chunk must be counted as a hash failure");
    assert_eq!(rec.get(0), Some(segs[0].clone()), "the player got the REAL seg 0, never the forgery");
    let _ = honest.send(EngineEvent::Stop);
}

#[tokio::test]
async fn inbound_from_unconnected_peer_does_not_crash() {
    // A stray control message from a peer that was never connected must be handled
    // gracefully (no panic, the node still stops cleanly).
    let me = PeerId::from_u64(2);
    let node = MeshNode::new_viewer(me, MeshConfig::default(), 40_000, Arc::new(NullSink), HashMap::new(), 0);
    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let stray = MeshMsg::Have { seq: 5 }.encode();
    tx.send(EngineEvent::Inbound { peer: PeerId::from_u64(77), channel: Channel::Ctrl, bytes: stray }).unwrap();
    let bm = MeshMsg::BufferMap { base_seq: 0, bitfield: vec![0xFF, 0xFF] }.encode();
    tx.send(EngineEvent::Inbound { peer: PeerId::from_u64(78), channel: Channel::Ctrl, bytes: bm }).unwrap();
    tx.send(EngineEvent::Stop).unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(5), node.run(rx, Duration::from_millis(10), None))
        .await
        .expect("node handled stray inbound and stopped");
    assert_eq!(stats.delivered, 0);
}

#[tokio::test]
async fn viewer_relays_stream_to_a_peer_that_cant_reach_the_origin() {
    // Mesh-as-relay (M4): viewer B connects ONLY to viewer A — never to the publisher —
    // and still receives the whole stream, because A reshares what it pulls. This is the
    // decentralized substitute for a TURN relay: a NAT-restricted peer only needs to reach
    // *some* peer, and the swarm relays through volunteers.
    let (n, seg_len) = (10usize, 16_000usize);
    let (segs, ids) = make_vod(n, seg_len);
    let (p, a, b) = (PeerId::from_u64(1), PeerId::from_u64(2), PeerId::from_u64(3));

    let (atx, arx) = mpsc::unbounded_channel::<EngineEvent>();
    let (btx, brx) = mpsc::unbounded_channel::<EngineEvent>();

    // Publisher P ↔ viewer A (A is the only peer that can reach the origin).
    let ptx = spawn_publisher(p, a, &atx, segs.clone(), seg_len);

    // Viewer A ↔ viewer B. B has NO link to the publisher.
    let (la_for_a, la_for_b) = wire(a, atx.clone(), b, btx.clone());
    atx.send(EngineEvent::PeerConnected { peer: b, link: la_for_a }).unwrap();
    btx.send(EngineEvent::PeerConnected { peer: a, link: la_for_b }).unwrap();

    // A keeps running (and reshares) until we stop it — so it can serve B to completion.
    let a_rec = Arc::new(Rec::default());
    let viewer_a =
        MeshNode::new_viewer(a, cfg(Role::Viewer), seg_len as u64, a_rec.clone(), ids.clone(), (n - 1) as Seq);
    let a_handle = tokio::spawn(viewer_a.run(arx, Duration::from_millis(10), None));

    let b_rec = Arc::new(Rec::default());
    let viewer_b =
        MeshNode::new_viewer(b, cfg(Role::Viewer), seg_len as u64, b_rec.clone(), ids, (n - 1) as Seq);
    let b_stats = tokio::time::timeout(Duration::from_secs(12), viewer_b.run(brx, Duration::from_millis(10), Some(n)))
        .await
        .expect("B should receive the full stream relayed through A");

    assert_eq!(b_stats.delivered, n, "B got every segment via A's reshare");
    assert_eq!(b_stats.hash_failures, 0);
    assert!(b_stats.peer_bytes > 0, "B's bytes came from a peer (A), never the origin");
    assert_eq!(b_rec.count(), n, "all relayed segments reached B's player");

    let _ = atx.send(EngineEvent::Stop);
    let _ = ptx.send(EngineEvent::Stop);
    let _ = tokio::time::timeout(Duration::from_secs(2), a_handle).await;
}

#[tokio::test]
async fn publisher_pushes_a_produced_segment_to_a_subscriber_without_a_want() {
    // Push-pull (TECH_SPEC §6.4): a subscriber registers interest ONCE; thereafter the
    // publisher PUSHES each produced segment proactively. We record everything the
    // publisher transmits and assert it emitted SegmentData for the new segment although
    // the subscriber never sent a single `Want`.
    let seg_len = 20_000usize;
    let pubid = PeerId::from_u64(1);
    let subid = PeerId::from_u64(2);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();

    let sent = Arc::new(Mutex::new(Vec::new()));
    let link = Arc::new(RecordingLink { remote: subid, sent: sent.clone() });
    ptx.send(EngineEvent::PeerConnected { peer: subid, link }).unwrap();
    // Subscribe, then produce — and crucially, no Want is ever sent.
    ptx.send(EngineEvent::Inbound {
        peer: subid,
        channel: Channel::Ctrl,
        bytes: MeshMsg::Subscribe.encode(),
    })
    .unwrap();
    let seg = Bytes::from(vec![0x5Au8; seg_len]);
    let id = crypto::segment_id(&seg);
    ptx.send(EngineEvent::Produced { seq: 0, id, bytes: seg }).unwrap();
    ptx.send(EngineEvent::Stop).unwrap();

    let publisher =
        MeshNode::new_live_publisher(pubid, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink));
    tokio::time::timeout(Duration::from_secs(5), publisher.run(prx, Duration::from_millis(10), None))
        .await
        .expect("publisher loop should stop");

    let msgs = sent.lock().unwrap();
    let pushed = msgs
        .iter()
        .filter(|(ch, b)| {
            *ch == Channel::Bulk
                && matches!(MeshMsg::decode(&mut &b[..]), Ok(MeshMsg::SegmentData { seq: 0, .. }))
        })
        .count();
    assert!(pushed >= 1, "the publisher must PUSH the produced segment to its subscriber");
}

#[tokio::test]
async fn pushed_segment_buffers_until_live_edge_then_delivers() {
    // Push-pull receive path: a segment is pushed to a viewer BEFORE it has learned the
    // segment's authenticated id (the chain edge-poll races the direct bytes). The viewer
    // holds the bytes and delivers them the instant the id lands — no re-fetch, no Want.
    let seg_len = 20_000usize;
    let me = PeerId::from_u64(100);
    let src = PeerId::from_u64(1);
    let seg = Bytes::from(vec![0x33u8; seg_len]);
    let id = crypto::segment_id(&seg);

    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let rec = Arc::new(Rec::default());
    // A viewer that does NOT yet know any segment ids (empty map, head 0).
    let viewer = MeshNode::new_viewer(me, cfg(Role::Viewer), seg_len as u64, rec.clone(), HashMap::new(), 0);

    // Bytes first (the push), id second (the live edge catches up).
    let pushed = MeshMsg::SegmentData {
        seq: 0,
        track_id: 0,
        total_len: seg_len as u32,
        offset: 0,
        bytes: seg.to_vec(),
    }
    .encode();
    tx.send(EngineEvent::Inbound { peer: src, channel: Channel::Bulk, bytes: pushed }).unwrap();
    tx.send(EngineEvent::LiveEdge { seq: 0, id }).unwrap();
    tx.send(EngineEvent::Stop).unwrap();

    let stats = tokio::time::timeout(Duration::from_secs(5), viewer.run(rx, Duration::from_millis(10), None))
        .await
        .expect("viewer loop should stop");

    assert_eq!(stats.delivered, 1, "the buffered early push is delivered once its id arrives");
    assert_eq!(stats.hash_failures, 0, "an early push is not a hash failure");
    assert_eq!(rec.get(0), Some(seg), "the player received exactly the pushed segment");
}

#[tokio::test]
async fn signed_edge_gossips_multihop_and_relays_to_a_viewer_with_no_link_to_the_origin() {
    // Off-chain signaling (#17 piece 1), the flagship case: a publisher SIGNS each
    // segment's live edge and gossips it in-mesh. Viewer A (connected to P) verifies it
    // against the publisher key and RE-GOSSIPS to viewer B — which has NO link to the
    // origin and NO chain edge poller. B can therefore only learn each segment's id from
    // A's relayed gossip, and only get the bytes from A's reshare. Full delivery proves
    // multi-hop signed-edge propagation + the push relay, with zero chain involvement.
    let seg_len = 16_000usize;
    let seed = [4u8; 32];
    let pubkey = crypto::public_bytes(&crypto::keypair_from_seed(&seed));
    let sid = [5u8; 32];
    let n = 4usize;

    let (p, a, b) = (PeerId::from_u64(1), PeerId::from_u64(2), PeerId::from_u64(3));
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (atx, arx) = mpsc::unbounded_channel::<EngineEvent>();
    let (btx, brx) = mpsc::unbounded_channel::<EngineEvent>();

    // P ↔ A
    let (lp_for_p, lp_for_a) = wire(p, ptx.clone(), a, atx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: a, link: lp_for_p }).unwrap();
    atx.send(EngineEvent::PeerConnected { peer: p, link: lp_for_a }).unwrap();
    // A ↔ B (B has no link to P)
    let (la_for_a, la_for_b) = wire(a, atx.clone(), b, btx.clone());
    atx.send(EngineEvent::PeerConnected { peer: b, link: la_for_a }).unwrap();
    btx.send(EngineEvent::PeerConnected { peer: a, link: la_for_b }).unwrap();

    let publisher = MeshNode::new_live_publisher(p, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink))
        .with_stream_id(sid)
        .with_edge_signer(Arc::new(SeedSigner { seed }));
    tokio::spawn(publisher.run(prx, Duration::from_millis(10), None));

    let a_rec = Arc::new(Rec::default());
    let viewer_a = MeshNode::new_viewer(a, cfg(Role::Viewer), seg_len as u64, a_rec.clone(), HashMap::new(), 0)
        .with_stream_id(sid)
        .with_publisher_key(pubkey);
    let a_handle = tokio::spawn(viewer_a.run(arx, Duration::from_millis(10), None));

    // Produce only after both viewers have connected + subscribed (so the push cascade
    // fires); each Produced triggers a signed edge gossip + a push.
    let ptx_prod = ptx.clone();
    let segs: Vec<Bytes> = (0..n as u64)
        .map(|i| Bytes::from(vec![(i as u8).wrapping_mul(11).wrapping_add(3); seg_len]))
        .collect();
    let segs_task = segs.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        for (i, seg) in segs_task.into_iter().enumerate() {
            let id = crypto::segment_id(&seg);
            let _ = ptx_prod.send(EngineEvent::Produced { seq: i as u64, id, bytes: seg });
        }
    });

    let b_rec = Arc::new(Rec::default());
    let viewer_b = MeshNode::new_viewer(b, cfg(Role::Viewer), seg_len as u64, b_rec.clone(), HashMap::new(), 0)
        .with_stream_id(sid)
        .with_publisher_key(pubkey);
    let b_stats = tokio::time::timeout(Duration::from_secs(12), viewer_b.run(brx, Duration::from_millis(10), Some(n)))
        .await
        .expect("B should learn edges via A's re-gossip and receive the relayed stream");

    assert_eq!(b_stats.delivered, n, "B got every segment: edge via gossip relay, bytes via reshare");
    assert_eq!(b_stats.hash_failures, 0);
    for (i, seg) in segs.iter().enumerate() {
        assert_eq!(b_rec.get(i as u64).as_ref(), Some(seg), "B played the real segment {i}");
    }
    let _ = atx.send(EngineEvent::Stop);
    let _ = ptx.send(EngineEvent::Stop);
    let _ = tokio::time::timeout(Duration::from_secs(2), a_handle).await;
}

#[tokio::test]
async fn forged_edge_gossip_is_rejected() {
    // A peer forges an EdgeAnnounce (garbage signature). The viewer must NOT accept the
    // id from it, so even though the *correct* bytes for that seq are pushed, they can
    // never be matched to a publisher-authenticated id and are never delivered.
    let seg_len = 16_000usize;
    let pubkey = crypto::public_bytes(&crypto::keypair_from_seed(&[4u8; 32]));
    let sid = [5u8; 32];
    let me = PeerId::from_u64(100);
    let attacker = PeerId::from_u64(7);

    let seg = Bytes::from(vec![0x42u8; seg_len]);
    let id = crypto::segment_id(&seg);

    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(me, cfg(Role::Viewer), seg_len as u64, rec.clone(), HashMap::new(), 0)
        .with_stream_id(sid)
        .with_publisher_key(pubkey);

    let forged = MeshMsg::EdgeAnnounce { seq: 0, id: id.0, sig: [0u8; 64] }.encode();
    tx.send(EngineEvent::Inbound { peer: attacker, channel: Channel::Ctrl, bytes: forged }).unwrap();
    let pushed = MeshMsg::SegmentData {
        seq: 0,
        track_id: 0,
        total_len: seg_len as u32,
        offset: 0,
        bytes: seg.to_vec(),
    }
    .encode();
    tx.send(EngineEvent::Inbound { peer: attacker, channel: Channel::Bulk, bytes: pushed }).unwrap();
    tx.send(EngineEvent::Stop).unwrap();

    let stats = tokio::time::timeout(Duration::from_secs(5), viewer.run(rx, Duration::from_millis(10), None))
        .await
        .expect("viewer loop should stop");
    assert_eq!(stats.delivered, 0, "a forged edge must not let any segment be accepted");
    assert_eq!(rec.count(), 0, "nothing reached the player");
}

// ---- signaling / discovery layer ----

fn sig(store: &MemStatementStore, stream: StreamId, me: PeerId, clock: Arc<VirtualClock>) -> StatementSignaling {
    StatementSignaling::new(store.clone(), stream, me, 2, 30, clock)
}

#[test]
fn presence_expires_after_ttl() {
    let store = MemStatementStore::new();
    let clock = Arc::new(VirtualClock::new());
    let stream = StreamId([9u8; 32]);
    let (a, b) = (PeerId::from_u64(1), PeerId::from_u64(2));
    let sa = sig(&store, stream, a, clock.clone());
    let sb = sig(&store, stream, b, clock.clone());

    sb.publish_presence_now(5_000_000);
    assert!(sa.read_candidates(10).iter().any(|p| p.peer_id == b), "fresh presence is discoverable");

    // ttl is 30 s; jump past it and the presence must be gone (no stale ghosts).
    clock.advance(31_000);
    assert!(!sa.read_candidates(10).iter().any(|p| p.peer_id == b), "expired presence must not be returned");
}

#[test]
fn signal_is_delivered_only_to_recipient() {
    let store = MemStatementStore::new();
    let clock = Arc::new(VirtualClock::new());
    let stream = StreamId([3u8; 32]);
    let (a, p, b) = (PeerId::from_u64(1), PeerId::from_u64(2), PeerId::from_u64(3));
    let sa = sig(&store, stream, a, clock.clone());
    let sp = sig(&store, stream, p, clock.clone());
    let sb = sig(&store, stream, b, clock.clone());

    sa.send_signal_now(p, &SignalMsg::Offer { sdp: b"v=0 to-P".to_vec() });

    assert_eq!(sp.read_signals().len(), 1, "the addressed peer receives the offer");
    assert_eq!(sb.read_signals().len(), 0, "an unrelated peer receives nothing");
}
