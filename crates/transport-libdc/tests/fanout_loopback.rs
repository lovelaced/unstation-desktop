//! Real wide fan-out over libdatachannel (loopback): ONE publisher `MeshNode` serving
//! EIGHT viewer nodes, each over its own real WebRTC peer connection (offer/answer +
//! trickle ICE + SCTP data channel). This is the desktop→many-clients topology — the
//! deterministic in-memory equivalent is `unstation-core/tests/scale_sim.rs::run_star`,
//! but this one proves the *production* transport does it: 9 live `RtcPeerConnection`s,
//! real DTLS handshakes, real per-channel backpressure, one shared upload budget.
//!
//! A second phase then stops two viewers mid-stream (a phone locking / walking away)
//! and produces more segments — the remaining six must still drain the stream, proving
//! a viewer's departure doesn't wedge the publisher's send path.
//!
//! Needs loopback UDP for ICE (`UNSTATION_BIND_ADDR=127.0.0.1`); `--ignored` like its
//! sibling `mesh_loopback.rs`, and wired into `scripts/test-all.sh`'s fast tier.

use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use transport_libdc::{LibDcTransport, SignalOut};
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::{EdgeSigner, MeshNode};
use unstation_core::transport::EngineEvent;
use unstation_core::types::PeerId;

const SID: [u8; 32] = [9u8; 32];
const SEED: [u8; 32] = [4u8; 32];
const VIEWERS: usize = 8;
const K1: usize = 6; // segments produced while all 8 watch
const K2: usize = 4; // segments produced after 2 viewers left
const SEG_LEN: usize = 8_000;

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

/// Same signaling shim as `mesh_loopback.rs`: route each transport's SDP/ICE to its
/// target; the dialer's first description is the offer.
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
                        let is_offer = dial_pairs.contains(&(from, to))
                            && offered.lock().unwrap().insert((from, to));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
async fn one_publisher_fans_out_to_eight_viewers_and_survives_two_leaving() {
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");

    let p = PeerId::from_u64(1);
    let viewers: Vec<PeerId> = (0..VIEWERS as u64).map(|i| PeerId::from_u64(10 + i)).collect();
    let pubkey = crypto::public_bytes(&crypto::keypair_from_seed(&SEED));

    // Publisher plumbing.
    let (p_in_tx, p_in_rx) = unbounded_channel::<EngineEvent>();
    let (p_sig_tx, p_sig_rx) = unbounded_channel::<SignalOut>();
    let tp = LibDcTransport::new(vec![], p_in_tx.clone(), p_sig_tx).expect("spawn reactor");

    let mut transports = HashMap::from([(p, tp.clone())]);
    let mut sig_rxs = vec![(p, p_sig_rx)];
    let mut dial_pairs = HashSet::new();

    // Viewer plumbing: every viewer dials the publisher directly (star).
    struct Viewer {
        id: PeerId,
        in_tx: UnboundedSender<EngineEvent>,
        transport: LibDcTransport,
        handle: tokio::task::JoinHandle<unstation_core::node::NodeStats>,
    }
    let mut vs: Vec<Viewer> = Vec::new();
    for &v in &viewers {
        let (in_tx, in_rx) = unbounded_channel::<EngineEvent>();
        let (sig_tx, sig_rx) = unbounded_channel::<SignalOut>();
        let t = LibDcTransport::new(vec![], in_tx.clone(), sig_tx).expect("spawn reactor");
        transports.insert(v, t.clone());
        sig_rxs.push((v, sig_rx));
        dial_pairs.insert((v, p));
        let node = MeshNode::new_viewer(v, cfg(Role::Viewer), SEG_LEN as u64, Arc::new(NullSink), HashMap::new(), 0)
            .with_stream_id(SID)
            .with_publisher_key(pubkey);
        // No target count: we read stats at the end via Stop.
        let handle = tokio::spawn(node.run(in_rx, Duration::from_millis(50), Some(K1 + K2)));
        vs.push(Viewer { id: v, in_tx, transport: t, handle });
    }
    spawn_signaling(transports, sig_rxs, dial_pairs);

    let node_p = MeshNode::new_live_publisher(p, cfg(Role::Publisher), SEG_LEN as u64, Arc::new(NullSink))
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
    tokio::spawn(node_p.run(p_in_rx, Duration::from_millis(50), None));

    // All eight dial in; give real ICE time to converge across 8 PCs.
    for v in &vs {
        v.transport.dial(p);
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Phase 1: produce with the full room watching.
    let mut produced = 0u64;
    for _ in 0..K1 {
        let seg = Bytes::from(vec![(produced as u8).wrapping_mul(7).wrapping_add(1); SEG_LEN]);
        let id = crypto::segment_id(&seg);
        p_in_tx.send(EngineEvent::Produced { seq: produced, id, bytes: seg }).unwrap();
        produced += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Phase 2: two viewers leave mid-stream (stop their nodes; their PCs go quiet).
    // The publisher must keep serving the rest without wedging on the dead channels.
    let leavers: Vec<Viewer> = vec![vs.remove(0), vs.remove(0)];
    for l in &leavers {
        let _ = l.in_tx.send(EngineEvent::Stop);
    }
    for _ in 0..K2 {
        let seg = Bytes::from(vec![(produced as u8).wrapping_mul(7).wrapping_add(1); SEG_LEN]);
        let id = crypto::segment_id(&seg);
        p_in_tx.send(EngineEvent::Produced { seq: produced, id, bytes: seg }).unwrap();
        produced += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Every remaining viewer drains the FULL stream (K1+K2) over its own real PC.
    for v in vs {
        let stats = tokio::time::timeout(Duration::from_secs(60), v.handle)
            .await
            .unwrap_or_else(|_| panic!("viewer {:?} timed out waiting for the stream", v.id))
            .expect("viewer task panicked");
        assert_eq!(
            stats.delivered,
            K1 + K2,
            "viewer {:?} must receive all {} segments over real WebRTC",
            v.id,
            K1 + K2
        );
        assert_eq!(stats.hash_failures, 0, "no corruption in the fan-out");
        assert!(stats.peer_bytes > 0, "bytes came over the wire, not an origin floor");
    }

    let _ = p_in_tx.send(EngineEvent::Stop);
    for l in &leavers {
        let _ = l.handle.abort();
    }
}
