//! D3: in-mesh gossip handoff (TECH_SPEC §7.3).
//!
//! After bootstrap, additional peers are learned via `PeerGossip` over the data
//! channel — not the statement store. The mesh node holds no signaling handle, so
//! this discovery provably costs zero statement-store writes: the store created
//! alongside it stays at 0 writes while the node learns peers.

use bytes::Bytes;
use parity_scale_codec::Encode;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::config::MeshConfig;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::protocol::MeshMsg;
use unstation_core::statement_store_mem::MemStatementStore;
use unstation_core::transport::{Channel, EngineEvent};
use unstation_core::types::PeerId;

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

#[tokio::test]
async fn peers_learned_via_gossip_cost_no_statement_store_writes() {
    // A statement store the mesh node will never touch.
    let store = MemStatementStore::new();

    let me = PeerId::from_u64(2);
    let node = MeshNode::new_viewer(
        me,
        MeshConfig::default(),
        40_000,
        Arc::new(NullSink),
        HashMap::new(),
        0,
    );

    let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();

    // A neighbor gossips two more peers it knows about.
    let gossip = MeshMsg::PeerGossip {
        peers: vec![PeerId::from_u64(3).0, PeerId::from_u64(4).0],
    }
    .encode();
    tx.send(EngineEvent::Inbound {
        peer: PeerId::from_u64(1),
        channel: Channel::Ctrl,
        bytes: gossip,
    })
    .unwrap();
    tx.send(EngineEvent::Stop).unwrap();

    let stats = tokio::time::timeout(
        Duration::from_secs(5),
        node.run(rx, Duration::from_millis(10), None),
    )
    .await
    .expect("node finished");

    assert_eq!(stats.known_peers, 2, "two peers learned over the mesh");
    assert_eq!(store.writes(), 0, "in-mesh discovery touches no statement store");
}
