//! Scalability tests: the picker stays bounded as the neighbor set grows, one
//! publisher fans out to many viewers, large streams and large buffer maps behave,
//! and a big segment reassembles from many chunks. All deterministic / in-memory.

use bytes::Bytes;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::buffermap::BufferMap;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto::{self, segment_id};
use unstation_core::engine::MeshEngine;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::peer::PeerState;
use unstation_core::reassembly::Reassembler;
use unstation_core::transport::EngineEvent;
use unstation_core::transport_mem::wire;
use unstation_core::types::{PeerId, SegmentId, Seq};

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

#[derive(Default)]
struct Counter {
    n: Mutex<BTreeSet<u64>>,
}
impl MediaSink for Counter {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, _: Bytes) {
        self.n.lock().unwrap().insert(seq);
    }
    // Model an in-order player: the play head is the first not-yet-delivered seq, so
    // the picker's window slides forward as contiguous segments arrive (a VOD longer
    // than the window can only drain if the head advances).
    fn on_play_head(&self) -> u64 {
        let set = self.n.lock().unwrap();
        let mut h = 0u64;
        while set.contains(&h) {
            h += 1;
        }
        h
    }
}
impl Counter {
    fn count(&self) -> usize {
        self.n.lock().unwrap().len()
    }
}

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Vod,
        role,
        window: 16,
        tick: Duration::from_millis(5),
        seg_ms: 500,
        upload_budget_bps: 80_000_000,
        weights: PickerWeights::default(),
    }
}

#[test]
fn plan_is_bounded_with_many_peers() {
    // 100 neighbors all holding everything must NOT produce 100× the requests — the
    // plan is bounded by the buffer window, not the peer count.
    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Viewer,
        window: 16,
        tick: Duration::from_millis(100),
        seg_ms: 2_000,
        upload_budget_bps: 0,
        weights: PickerWeights::default(),
    };
    let mut eng = MeshEngine::new(cfg, 1_250_000);
    eng.head_seq = 50;
    eng.seed_available = false;
    eng.bulletin_available = false;
    for i in 0..100u64 {
        let id = PeerId::from_u64(i + 1);
        let mut ps = PeerState::new(id);
        for s in 0..=eng.head_seq {
            ps.buffer.set(s);
        }
        ps.throughput_bps.update(10_000_000.0);
        ps.rtt_ms.update(50.0);
        eng.peers.insert(id, ps);
    }
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    let reqs = eng.plan(0, &mut rng);
    assert!(!reqs.is_empty(), "the viewer should request something");
    assert!(
        reqs.len() <= 2 * eng.cfg.window as usize,
        "requests must be window-bounded ({}), not peer-bounded — got {}",
        2 * eng.cfg.window,
        reqs.len()
    );
}

#[tokio::test]
async fn wide_fanout_one_publisher_many_viewers() {
    // One publisher serves N independent viewers concurrently over the mesh.
    let (n, seg_len, viewers) = (8usize, 12_000usize, 10usize);
    let segs: Vec<Bytes> = (0..n).map(|i| Bytes::from(vec![(i as u8) ^ 0x33; seg_len])).collect();
    let ids: HashMap<Seq, SegmentId> =
        segs.iter().enumerate().map(|(i, b)| (i as Seq, segment_id(b))).collect();

    let pubid = PeerId::from_u64(1);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();

    let mut handles = Vec::new();
    for v in 0..viewers {
        let viewid = PeerId::from_u64(1000 + v as u64);
        let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
        let (link_for_pub, link_for_view) = wire(pubid, ptx.clone(), viewid, vtx.clone());
        ptx.send(EngineEvent::PeerConnected { peer: viewid, link: link_for_pub }).unwrap();
        vtx.send(EngineEvent::PeerConnected { peer: pubid, link: link_for_view }).unwrap();
        let viewer =
            MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, Arc::new(NullSink), ids.clone(), (n - 1) as Seq);
        handles.push(tokio::spawn(viewer.run(vrx, Duration::from_millis(5), Some(n))));
    }

    let publisher = MeshNode::new_publisher(pubid, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink), segs);
    tokio::spawn(publisher.run(prx, Duration::from_millis(5), None));

    for (v, h) in handles.into_iter().enumerate() {
        let stats = tokio::time::timeout(Duration::from_secs(15), h)
            .await
            .unwrap_or_else(|_| panic!("viewer {v} timed out"))
            .expect("viewer task panicked");
        assert_eq!(stats.delivered, n, "viewer {v} should receive every segment");
        assert_eq!(stats.hash_failures, 0, "viewer {v} saw corruption");
    }
    let _ = ptx.send(EngineEvent::Stop);
}

#[tokio::test]
async fn large_segment_count_delivers() {
    // A long VOD (150 small segments) must drain completely through the picker window.
    let (n, seg_len) = (150usize, 3_000usize);
    let segs: Vec<Bytes> = (0..n).map(|i| Bytes::from(vec![(i as u8).wrapping_mul(13).wrapping_add(2); seg_len])).collect();
    let ids: HashMap<Seq, SegmentId> =
        segs.iter().enumerate().map(|(i, b)| (i as Seq, segment_id(b))).collect();

    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
    let (lp, lv) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: lp }).unwrap();
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: lv }).unwrap();

    let publisher = MeshNode::new_publisher(pubid, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink), segs);
    tokio::spawn(publisher.run(prx, Duration::from_millis(5), None));

    let rec = Arc::new(Counter::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), seg_len as u64, rec.clone(), ids, (n - 1) as Seq);
    let stats = tokio::time::timeout(Duration::from_secs(20), viewer.run(vrx, Duration::from_millis(5), Some(n)))
        .await
        .expect("a 150-segment VOD should fully drain");
    assert_eq!(stats.delivered, n);
    assert_eq!(stats.hash_failures, 0);
    assert_eq!(rec.count(), n);
    let _ = ptx.send(EngineEvent::Stop);
}

#[test]
fn large_buffer_map_roundtrips() {
    // A buffer map spanning a wide seq range survives a bytes round-trip exactly.
    let base = 10_000u64;
    let mut b = BufferMap::new(base);
    let mut expected = BTreeSet::new();
    // A dense block plus scattered far entries.
    for s in base..base + 400 {
        if s % 3 == 0 {
            b.set(s);
            expected.insert(s);
        }
    }
    for s in [base + 800, base + 1500, base + 4096] {
        b.set(s);
        expected.insert(s);
    }
    assert_eq!(b.count(), expected.len());
    assert_eq!(b.highest(), expected.iter().last().copied());

    let bytes = b.to_bytes();
    let b2 = BufferMap::from_bytes(base, &bytes);
    for s in base..base + 5000 {
        assert_eq!(b2.has(s), expected.contains(&s), "mismatch at seq {s}");
    }
    assert_eq!(b2.count(), expected.len(), "count preserved across round-trip");
}

#[test]
fn deep_reassembly_many_chunks() {
    // A 1 MiB segment delivered as 64 × 16 KiB chunks, out of order, reassembles and
    // hash-verifies — the chunking path at realistic segment sizes.
    let data: Vec<u8> = (0..1_048_576usize).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let id = crypto::segment_id(&data);
    let chunk = 16 * 1024usize;

    let mut parts: Vec<(u32, &[u8])> =
        data.chunks(chunk).enumerate().map(|(i, c)| ((i * chunk) as u32, c)).collect();
    // Deterministic shuffle: deliver odds first, then evens (out of order).
    parts.sort_by_key(|(off, _)| (*off / chunk as u32 % 2 == 0, *off));

    let mut r = Reassembler::new(data.len() as u32);
    for (off, c) in parts {
        r.add(off, c);
    }
    assert!(r.is_complete(), "all chunks present");
    let out = r.finish_verified(&id).expect("1 MiB segment must hash-verify");
    assert_eq!(out.len(), data.len());
    assert_eq!(out, data);
}
