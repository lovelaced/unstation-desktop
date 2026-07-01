//! Churn suite: viewers joining, leaving mid-transfer, and rejoining under fresh
//! ids while a live stream runs. Proves a long-lived viewer's playback is
//! continuous through the churn and that per-peer state (pending requests,
//! partial reassemblies) fully drains — no leak survives a churned swarm.

use bytes::Bytes;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedSender};
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

const SEG_LEN: usize = 20_000;
const N_SEGS: usize = 40;

/// Connect two node inboxes with a mem-transport pair.
fn connect(
    a: PeerId,
    a_tx: &UnboundedSender<EngineEvent>,
    b: PeerId,
    b_tx: &UnboundedSender<EngineEvent>,
) {
    let (link_for_a, link_for_b) = wire(a, a_tx.clone(), b, b_tx.clone());
    let _ = a_tx.send(EngineEvent::PeerConnected { peer: b, link: link_for_a });
    let _ = b_tx.send(EngineEvent::PeerConnected { peer: a, link: link_for_b });
}

#[tokio::test]
async fn long_lived_viewer_survives_churning_swarm() {
    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);

    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
    connect(pubid, &ptx, viewid, &vtx);

    let publisher =
        MeshNode::new_live_publisher(pubid, cfg(Role::Publisher), SEG_LEN as u64, Arc::new(NullSink));
    let pub_handle = tokio::spawn(publisher.run(prx, Duration::from_millis(5), None));

    let rec = Arc::new(Rec::default());
    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        SEG_LEN as u64,
        rec.clone(),
        HashMap::new(),
        0,
    );

    // Live inboxes the feeder broadcasts `LiveEdge` to (long-lived viewer + whichever
    // churners are alive right now).
    let edge_subs: Arc<Mutex<Vec<UnboundedSender<EngineEvent>>>> =
        Arc::new(Mutex::new(vec![vtx.clone()]));

    // Feeder: produce N_SEGS distinct fragments over ~1.2 s of wall time.
    let ptx_f = ptx.clone();
    let edge_f = edge_subs.clone();
    tokio::spawn(async move {
        for i in 0..N_SEGS {
            let frag = Bytes::from(vec![(i as u8) ^ 0xC3; SEG_LEN]);
            let id = segment_id(&frag);
            let _ = ptx_f.send(EngineEvent::Produced { seq: i as u64, id, bytes: frag });
            for tx in edge_f.lock().unwrap().iter() {
                let _ = tx.send(EngineEvent::LiveEdge { seq: i as u64, id });
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });

    // Churn driver: waves of short-lived viewers join the publisher AND the
    // long-lived viewer, start pulling, then vanish abruptly mid-transfer (their
    // in-flight requests must re-route; their partial reassemblies must drain).
    let ptx_c = ptx.clone();
    let vtx_c = vtx.clone();
    let edge_c = edge_subs.clone();
    tokio::spawn(async move {
        for wave in 0..4u64 {
            let mut wave_nodes = Vec::new();
            for i in 0..2u64 {
                let cid = PeerId::from_u64(100 + wave * 10 + i);
                let (ctx, crx) = mpsc::unbounded_channel::<EngineEvent>();
                connect(cid, &ctx, pubid, &ptx_c);
                connect(cid, &ctx, viewid, &vtx_c);
                edge_c.lock().unwrap().push(ctx.clone());
                let churner = MeshNode::new_viewer(
                    cid,
                    cfg(Role::Viewer),
                    SEG_LEN as u64,
                    Arc::new(NullSink),
                    HashMap::new(),
                    0,
                );
                tokio::spawn(churner.run(crx, Duration::from_millis(5), None));
                wave_nodes.push((cid, ctx));
            }
            tokio::time::sleep(Duration::from_millis(160)).await;
            // Abrupt leave: stop the node and tell its peers it's gone.
            let mut subs = edge_c.lock().unwrap();
            subs.truncate(1); // keep only the long-lived viewer
            drop(subs);
            for (cid, ctx) in wave_nodes {
                let _ = ctx.send(EngineEvent::Stop);
                let _ = ptx_c.send(EngineEvent::PeerDisconnected { peer: cid });
                let _ = vtx_c.send(EngineEvent::PeerDisconnected { peer: cid });
            }
        }
    });

    let stats = tokio::time::timeout(
        Duration::from_secs(30),
        viewer.run(vrx, Duration::from_millis(5), Some(N_SEGS)),
    )
    .await
    .expect("the long-lived viewer must finish the stream through the churn");

    assert_eq!(stats.delivered, N_SEGS, "continuous playback through churn");
    assert_eq!(stats.hash_failures, 0, "every fragment verified");
    assert_eq!(rec.got.lock().unwrap().len(), N_SEGS, "player got every fragment");
    // No leak survives the churn: everything per-peer drained with the peers.
    assert_eq!(stats.pending_entries, 0, "no dangling in-flight request");
    assert_eq!(stats.reasm_entries, 0, "no orphaned partial reassembly");
    assert_eq!(stats.reasm_bytes, 0, "reassembly byte budget fully returned");

    let _ = ptx.send(EngineEvent::Stop);
    let _ = pub_handle.await;
}
