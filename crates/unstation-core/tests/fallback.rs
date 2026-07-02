//! Durable-floor fallback (TECH_SPEC §8.6): when the panic zone finds no peer able
//! to meet a deadline, the node fetches the segment through the injected hook,
//! re-verifies it, and plays on — and hostile floor bytes are rejected.

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::{FallbackFetch, MeshNode};
use unstation_core::transport::EngineEvent;
use unstation_core::types::{SegmentId, Seq};
use unstation_core::PeerId;

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

fn cfg() -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role: Role::Viewer,
        window: 8,
        tick: Duration::from_millis(10),
        seg_ms: 500,
        upload_budget_bps: 0,
        weights: PickerWeights::default(),
    }
}

#[tokio::test]
async fn deadline_missing_segment_is_fetched_from_the_durable_floor() {
    // One known segment, zero peers: every deadline lands in the panic zone with no
    // holders, so the picker escalates to the durable floor.
    let payload = Bytes::from(vec![0x5Au8; 4096]);
    let id = crypto::segment_id(&payload);
    let ids: HashMap<Seq, SegmentId> = HashMap::from([(3u64, id)]);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_c = calls.clone();
    let floor: FallbackFetch = Arc::new(move |seq, want_id| {
        let payload = payload.clone();
        calls_c.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            assert_eq!(seq, 3, "only the known segment reaches the floor");
            assert_eq!(want_id, crypto::segment_id(&payload));
            Some(payload)
        })
    });

    let (_tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let viewer = MeshNode::new_viewer(PeerId::from_u64(2), cfg(), 4096, Arc::new(NullSink), ids, 3)
        .with_fallback(floor);

    let stats = tokio::time::timeout(
        Duration::from_secs(5),
        viewer.run(rx, Duration::from_millis(10), Some(1)),
    )
    .await
    .expect("the floor must deliver before the timeout");

    assert_eq!(stats.delivered, 1, "segment delivered");
    assert_eq!(stats.from_origin, 1, "credited to the durable floor");
    assert_eq!(stats.peer_bytes, 0, "no peer was involved");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "in-flight dedup: exactly one fetch");
}

#[tokio::test]
async fn forged_floor_bytes_are_rejected() {
    // The floor returns garbage that doesn't hash to the authenticated id: the node
    // must reject it and keep asking (never play unverified bytes).
    let real = Bytes::from(vec![0x11u8; 1024]);
    let id = crypto::segment_id(&real);
    let ids: HashMap<Seq, SegmentId> = HashMap::from([(1u64, id)]);

    let floor: FallbackFetch = Arc::new(move |_seq, _id| {
        Box::pin(async move { Some(Bytes::from(vec![0xFFu8; 1024])) }) // wrong hash
    });

    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();
    let viewer = MeshNode::new_viewer(PeerId::from_u64(2), cfg(), 1024, Arc::new(NullSink), ids, 1)
        .with_fallback(floor);

    let handle = tokio::spawn(viewer.run(rx, Duration::from_millis(10), Some(1)));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!handle.is_finished(), "forged bytes must never count as a delivery");
    let _ = tx.send(EngineEvent::Stop);
    let stats = handle.await.unwrap();
    assert_eq!(stats.delivered, 0);
    assert_eq!(stats.from_origin, 0);
}
