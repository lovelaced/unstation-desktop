//! Paced catch-up: a viewer that joins onto a full, already-buffered window pulls the
//! WHOLE backlog of big (≈200 KiB, a 6 Mbps encoder's part) segments over a real peer
//! connection, and must drain every one. This exercises the publisher's paced serve
//! queue (a `Want` enqueues; the tick drains `SERVE_BYTES_PER_TICK` at a time) end to
//! end over real DTLS/SCTP — a guard that pacing still delivers a full window rather
//! than stranding segments in the queue or wedging the drain.
//!
//! SCOPE: this does NOT reproduce the on-device failure that motivated pacing — a
//! whole-window blast overrunning a just-connected SCTP association in slow-start
//! (`SCTP disconnected` ~1 s after a viewer joins a 6 Mbps stream). That is
//! RTT-dependent: on loopback the round-trip is ~microseconds, so slow-start ramps
//! instantly and even an unpaced blast drains without buildup (this test passes with
//! OR without the fix). Reproducing the reset needs a real constrained link (a phone
//! on WiFi, or a `dnctl`/dummynet delay+bandwidth pipe on loopback); it was verified
//! on-device. What this test locks in is that the paced path stays correct.
//!
//! Needs loopback UDP for ICE (`UNSTATION_BIND_ADDR=127.0.0.1`); `--ignored` like its
//! sibling `fanout_loopback.rs`.

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
use unstation_core::types::{PeerId, SegmentId};

const SID: [u8; 32] = [9u8; 32];
const SEED: [u8; 32] = [4u8; 32];
const WINDOW: usize = 16;
// ≈200 KiB: a 6 Mbps encoder's ~267 ms LL part. The whole point of the repro — small
// segments never trigger the cold-start blast.
const SEG_LEN: usize = 200_000;

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
        window: WINDOW as u32,
        tick: Duration::from_millis(50),
        seg_ms: 267,
        upload_budget_bps: 80_000_000, // the real publisher budget
        weights: PickerWeights::default(),
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
async fn late_joiner_drains_a_full_window_without_dropping() {
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");
    let _ = env_logger::builder().is_test(false).try_init();

    let p = PeerId::from_u64(1);
    let v = PeerId::from_u64(10);
    let pubkey = crypto::public_bytes(&crypto::keypair_from_seed(&SEED));

    // Publisher.
    let (p_in_tx, p_in_rx) = unbounded_channel::<EngineEvent>();
    let (p_sig_tx, p_sig_rx) = unbounded_channel::<SignalOut>();
    let tp = LibDcTransport::new(vec![], p_in_tx.clone(), p_sig_tx).expect("spawn reactor");
    let node_p = MeshNode::new_live_publisher(p, cfg(Role::Publisher), SEG_LEN as u64, Arc::new(NullSink))
        .with_stream_id(SID)
        .with_edge_signer(Arc::new(SeedSigner));
    tokio::spawn(node_p.run(p_in_rx, Duration::from_millis(50), None));

    // Fill the publisher's window BEFORE anyone connects — this is what makes the
    // eventual join a whole-window catch-up rather than a paced live follow.
    let mut ids: HashMap<u64, SegmentId> = HashMap::new();
    for seq in 0..WINDOW as u64 {
        let seg = Bytes::from(vec![(seq as u8).wrapping_mul(7).wrapping_add(1); SEG_LEN]);
        let id = crypto::segment_id(&seg);
        ids.insert(seq, id);
        p_in_tx.send(EngineEvent::Produced { seq, id, bytes: seg }).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Viewer dials in AFTER the backlog exists. A real late joiner learns the backlog's
    // seq→id map + live edge from the chain's signed edge before it can verify what it
    // pulls; there's no chain in loopback, so hand it those directly (as `new_seed`
    // is bootstrapped) — otherwise it can't hash-verify the catch-up and delivers 0.
    let (v_in_tx, v_in_rx) = unbounded_channel::<EngineEvent>();
    let (v_sig_tx, v_sig_rx) = unbounded_channel::<SignalOut>();
    let tv = LibDcTransport::new(vec![], v_in_tx.clone(), v_sig_tx).expect("spawn reactor");
    let node_v = MeshNode::new_viewer(v, cfg(Role::Viewer), SEG_LEN as u64, Arc::new(NullSink), ids, WINDOW as u64 - 1)
        .with_stream_id(SID)
        .with_publisher_key(pubkey);
    let v_handle = tokio::spawn(node_v.run(v_in_rx, Duration::from_millis(50), Some(WINDOW)));

    let transports = HashMap::from([(p, tp.clone()), (v, tv.clone())]);
    let sig_rxs = vec![(p, p_sig_rx), (v, v_sig_rx)];
    let dial_pairs = HashSet::from([(v, p)]);
    spawn_signaling(transports, sig_rxs, dial_pairs);
    tv.dial(p);

    // The late joiner must drain the ENTIRE buffered window over its real peer
    // connection. If the cold-start burst drops the association it never finishes.
    let stats = tokio::time::timeout(Duration::from_secs(30), v_handle)
        .await
        .expect("late joiner timed out — paced serve failed to drain the full window")
        .expect("viewer task panicked");
    assert_eq!(
        stats.delivered, WINDOW,
        "late joiner must drain all {WINDOW} buffered segments over real WebRTC (got {})",
        stats.delivered
    );
    assert_eq!(stats.hash_failures, 0, "no corruption");
    assert!(stats.peer_bytes > 0, "bytes came over the wire");

    let _ = p_in_tx.send(EngineEvent::Stop);
    let _ = v_in_tx.send(EngineEvent::Stop);
}
