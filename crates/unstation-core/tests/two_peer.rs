//! D2 integration: a real two-peer mesh over the in-memory loopback transport.
//!
//! A publisher preloaded with a 12-segment VOD (the genesis seed) and a viewer
//! that knows the authenticated seq→id map. The viewer's picker issues `Want`s,
//! the publisher serves 16 KiB `SegmentData` chunks, and the viewer reassembles +
//! hash-verifies every segment and feeds the player. Asserts full delivery,
//! 100% peer offload, and zero hash failures.

use bytes::Bytes;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::transport_mem::wire;
use unstation_core::types::{PeerId, SegmentId, Seq};

#[derive(Default)]
struct RecordingSink {
    segs: Mutex<BTreeMap<u64, usize>>,
}
impl MediaSink for RecordingSink {
    fn push_init(&self, _bytes: Bytes) {}
    fn push_segment(&self, seq: u64, bytes: Bytes) {
        self.segs.lock().unwrap().insert(seq, bytes.len());
    }
    fn on_play_head(&self) -> u64 {
        0
    }
}
impl RecordingSink {
    fn count(&self) -> usize {
        self.segs.lock().unwrap().len()
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
        mode: Mode::Vod,
        role,
        window: 16,
        tick: Duration::from_millis(10),
        seg_ms: 500,
        upload_budget_bps: 50_000_000,
        weights: PickerWeights::default(),
    }
}

#[tokio::test]
async fn two_peer_viewer_fetches_all_segments() {
    let n = 12usize;
    let seg_len = 40_000usize; // ~3 chunks per segment
    let segments: Vec<Bytes> = (0..n).map(|i| Bytes::from(vec![i as u8; seg_len])).collect();
    let segment_ids: HashMap<Seq, SegmentId> = segments
        .iter()
        .enumerate()
        .map(|(i, b)| (i as Seq, crypto::segment_id(b)))
        .collect();

    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);

    let (pub_tx, pub_rx) = mpsc::unbounded_channel::<EngineEvent>();
    let (view_tx, view_rx) = mpsc::unbounded_channel::<EngineEvent>();

    let (link_for_pub, link_for_view) = wire(pubid, pub_tx.clone(), viewid, view_tx.clone());
    pub_tx
        .send(EngineEvent::PeerConnected { peer: viewid, link: link_for_pub })
        .unwrap();
    view_tx
        .send(EngineEvent::PeerConnected { peer: pubid, link: link_for_view })
        .unwrap();

    let publisher = MeshNode::new_publisher(
        pubid,
        cfg(Role::Publisher),
        seg_len as u64,
        Arc::new(NullSink),
        segments.clone(),
    );
    let pub_handle = tokio::spawn(publisher.run(pub_rx, Duration::from_millis(10), None));

    let rec = Arc::new(RecordingSink::default());
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        seg_len as u64,
        rec.clone(),
        segment_ids,
        (n - 1) as Seq,
    );

    let stats = tokio::time::timeout(
        Duration::from_secs(5),
        viewer.run(view_rx, Duration::from_millis(10), Some(n)),
    )
    .await
    .expect("viewer should finish within 5s");

    assert_eq!(stats.delivered, n, "all segments delivered");
    assert_eq!(stats.hash_failures, 0, "no hash failures");
    assert_eq!(
        stats.peer_bytes as usize,
        n * seg_len,
        "all bytes came from the peer (ρ = 1.0)"
    );
    assert_eq!(rec.count(), n, "all segments fed to the player");

    let _ = pub_tx.send(EngineEvent::Stop);
    let _ = pub_handle.await;
}
