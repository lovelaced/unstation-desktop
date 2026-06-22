//! Deterministic Go-Live path (no ffmpeg): a live publisher fed `Produced`
//! segments serves them over the mesh to a viewer that learns them via `LiveEdge`.
//!
//! This is the always-green counterpart to the real-media `go_live` e2e — it
//! proves the live publisher→viewer plumbing (`new_live_publisher`,
//! `EngineEvent::Produced`/`LiveEdge`) without subprocess timing.

use bytes::Bytes;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto::segment_id;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::transport_mem::wire;
use unstation_core::types::PeerId;

#[derive(Default)]
struct Rec {
    got: Mutex<BTreeSet<u64>>,
}
impl MediaSink for Rec {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, seq: u64, _: Bytes) {
        self.got.lock().unwrap().insert(seq);
    }
    fn on_play_head(&self) -> u64 {
        0
    }
}
impl Rec {
    fn count(&self) -> usize {
        self.got.lock().unwrap().len()
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

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role,
        window: 64,
        tick: Duration::from_millis(5),
        seg_ms: 1000,
        upload_budget_bps: 80_000_000,
        weights: Default::default(),
    }
}

#[tokio::test]
async fn live_publisher_feeds_viewer_over_mesh() {
    let n = 8usize;
    // Distinct fragments of varying size, content-addressed like real CMAF.
    let frags: Vec<Bytes> = (0..n)
        .map(|i| Bytes::from(vec![(i as u8) ^ 0x5a; 30_000 + i * 1500]))
        .collect();

    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
    let (lp, lv) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: lp }).unwrap();
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: lv }).unwrap();

    let publisher = MeshNode::new_live_publisher(pubid, cfg(Role::Publisher), 40_000, Arc::new(NullSink));
    tokio::spawn(publisher.run(prx, Duration::from_millis(5), None));

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(viewid, cfg(Role::Viewer), 40_000, rec.clone(), HashMap::new(), 0);

    // Feeder: emit fragments over time — Produced to the publisher, LiveEdge to the viewer.
    let ptx_f = ptx.clone();
    let vtx_f = vtx.clone();
    tokio::spawn(async move {
        for (i, f) in frags.into_iter().enumerate() {
            let id = segment_id(&f);
            let _ = ptx_f.send(EngineEvent::Produced { seq: i as u64, id, bytes: f });
            let _ = vtx_f.send(EngineEvent::LiveEdge { seq: i as u64, id });
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    });

    let stats = tokio::time::timeout(
        Duration::from_secs(10),
        viewer.run(vrx, Duration::from_millis(5), Some(n)),
    )
    .await
    .expect("viewer should pull all live fragments");

    assert_eq!(stats.delivered, n, "all live fragments delivered");
    assert_eq!(stats.hash_failures, 0, "every fragment hash-verified");
    assert_eq!(rec.count(), n, "all fragments fed to the player");

    let _ = ptx.send(EngineEvent::Stop);
}
