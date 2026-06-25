//! Real multi-node mesh over libdatachannel (loopback): three real `LibDcTransport`s
//! driving three real `MeshNode`s, connected publisher → A → B where **B has no link to
//! the origin**. Exercises, over the *production* transport (real offer/answer + trickle
//! ICE + SCTP data channels), the full off-chain stack: signed live-edge gossip (#17)
//! relayed P→A→B, push-pull reshare (#20), and mesh-as-relay (M4). B receives the whole
//! stream having only ever talked to A.
//!
//! Needs loopback UDP for ICE; libjuice excludes 127.0.0.1 by default, so the test sets
//! `UNSTATION_BIND_ADDR=127.0.0.1` (see `transport_libdc::rtc_config`). Fails closed with
//! a clear timeout if a sandbox forbids UDP. The deterministic equivalent is
//! `unstation-core/tests/connectivity.rs::signed_edge_gossips_multihop...`.

use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use transport_libdc::{LibDcTransport, SignalOut};
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::{EdgeSigner, MeshNode};
use unstation_core::transport::EngineEvent;
use unstation_core::types::PeerId;

const SID: [u8; 32] = [5u8; 32];
const SEED: [u8; 32] = [4u8; 32];

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

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role,
        window: 16,
        tick: Duration::from_millis(50),
        seg_ms: 500,
        upload_budget_bps: 100_000_000,
        weights: PickerWeights::default(),
    }
}

/// Route each transport's locally-generated SDP/ICE to its target transport. The dialer's
/// first description is the offer (→ `accept`); everything else is an answer/candidate.
fn spawn_signaling(
    transports: HashMap<PeerId, LibDcTransport>,
    sig_rxs: Vec<(PeerId, UnboundedReceiver<SignalOut>)>,
    dial_pairs: HashSet<(PeerId, PeerId)>,
) {
    let offered: Arc<Mutex<HashSet<(PeerId, PeerId)>>> = Arc::new(Mutex::new(HashSet::new()));
    for (from, mut rx) in sig_rxs {
        let transports = transports.clone();
        let dial_pairs = dial_pairs.clone();
        let offered = offered.clone();
        tokio::spawn(async move {
            while let Some(sig) = rx.recv().await {
                match sig {
                    SignalOut::LocalDescription { peer: to, sdp } => {
                        let Some(t) = transports.get(&to) else { continue };
                        let is_offer =
                            dial_pairs.contains(&(from, to)) && offered.lock().unwrap().insert((from, to));
                        if is_offer {
                            t.accept(from, sdp);
                        } else {
                            t.remote_description(from, sdp);
                        }
                    }
                    SignalOut::LocalCandidate { peer: to, cand } => {
                        if let Some(t) = transports.get(&to) {
                            t.remote_candidate(from, cand);
                        }
                    }
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
async fn three_node_mesh_relays_signed_edge_and_segments_over_real_webrtc() {
    // libjuice skips 127.0.0.1 unless told to bind it explicitly.
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");

    let (p, a, b) = (PeerId::from_u64(1), PeerId::from_u64(2), PeerId::from_u64(3));
    let pubkey = crypto::public_bytes(&crypto::keypair_from_seed(&SEED));
    let k = 6usize;
    let seg_len = 8_000usize;

    // One inbox per node (the transport posts PeerConnected/Inbound there; the node
    // consumes it) + one signaling channel per transport.
    let (p_in_tx, p_in_rx) = unbounded_channel::<EngineEvent>();
    let (a_in_tx, a_in_rx) = unbounded_channel::<EngineEvent>();
    let (b_in_tx, b_in_rx) = unbounded_channel::<EngineEvent>();
    let (p_sig_tx, p_sig_rx) = unbounded_channel::<SignalOut>();
    let (a_sig_tx, a_sig_rx) = unbounded_channel::<SignalOut>();
    let (b_sig_tx, b_sig_rx) = unbounded_channel::<SignalOut>();

    let tp = LibDcTransport::new(vec![], p_in_tx.clone(), p_sig_tx);
    let ta = LibDcTransport::new(vec![], a_in_tx.clone(), a_sig_tx);
    let tb = LibDcTransport::new(vec![], b_in_tx, b_sig_tx);

    let transports = HashMap::from([(p, tp.clone()), (a, ta.clone()), (b, tb.clone())]);
    // A dials P; B dials A. B never talks to P.
    let dial_pairs = HashSet::from([(a, p), (b, a)]);
    spawn_signaling(
        transports,
        vec![(p, p_sig_rx), (a, a_sig_rx), (b, b_sig_rx)],
        dial_pairs,
    );

    // Nodes: P live publisher (signs + gossips edges); A, B viewers verifying against P.
    let node_p = MeshNode::new_live_publisher(p, cfg(Role::Publisher), seg_len as u64, Arc::new(NullSink))
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
    tokio::spawn(node_p.run(p_in_rx, Duration::from_millis(50), None));

    let node_a = MeshNode::new_viewer(a, cfg(Role::Viewer), seg_len as u64, Arc::new(NullSink), HashMap::new(), 0)
        .with_stream_id(SID)
        .with_publisher_key(pubkey);
    let a_handle = tokio::spawn(node_a.run(a_in_rx, Duration::from_millis(50), None));

    let node_b = MeshNode::new_viewer(b, cfg(Role::Viewer), seg_len as u64, Arc::new(NullSink), HashMap::new(), 0)
        .with_stream_id(SID)
        .with_publisher_key(pubkey);
    let b_handle = tokio::spawn(node_b.run(b_in_rx, Duration::from_millis(50), Some(k)));

    // Bring the links up, give ICE time, then produce. (Even if a segment is produced
    // before B is fully connected, P retains it and the picker/gossip backfills.)
    ta.dial(p);
    tb.dial(a);
    tokio::time::sleep(Duration::from_secs(4)).await;
    for i in 0..k as u64 {
        let seg = Bytes::from(vec![(i as u8).wrapping_mul(7).wrapping_add(1); seg_len]);
        let id = crypto::segment_id(&seg);
        p_in_tx.send(EngineEvent::Produced { seq: i, id, bytes: seg }).unwrap();
    }

    // B must receive the full stream via A's relay (edge gossip + reshare), with no link
    // to the origin.
    let b_stats = tokio::time::timeout(Duration::from_secs(60), b_handle)
        .await
        .expect("B should receive the relayed stream within 60s")
        .expect("B task panicked");
    assert_eq!(b_stats.delivered, k, "B got every segment relayed through A over real WebRTC");
    assert_eq!(b_stats.hash_failures, 0, "no corruption across the real-transport relay");
    assert!(b_stats.peer_bytes > 0, "B's bytes came from a peer (A), never an origin");

    // Stop A + P.
    let _ = a_in_tx.send(EngineEvent::Stop);
    let _ = p_in_tx.send(EngineEvent::Stop);
    let _ = tokio::time::timeout(Duration::from_secs(3), a_handle).await;
}
