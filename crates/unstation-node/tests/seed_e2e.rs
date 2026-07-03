//! Real-chain e2e for the volunteer seed (#[ignore]): a publisher session in this
//! process produces a synthetic stream over a local `--dev` statement-store node;
//! the REAL `unstation-node` binary is spawned as a child process with its own
//! identity, discovers the stream on-chain, dials the publisher over real WebRTC,
//! and must cache the live window (its heartbeat reports the advancing head).
//!
//! Prereqs (the same ones `scripts/test-all.sh --chain` establishes):
//!   scripts/dev-chain.sh run &            # local kitchensink node at :9944
//!   scripts/provision-allowance.sh        # publisher key ([11u8;32])
//! The SEED's key ([13u8;32]) is provisioned by this test itself via the same script.
//!
//!   NODE_WS=ws://127.0.0.1:9944 cargo test -p unstation-node \
//!     --test seed_e2e -- --ignored --nocapture

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::unbounded_channel;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::types::StreamId;
use unstation_session::{IdentityEdgeSigner, Session};

const STREAM_NAME: &str = "seed-e2e";
const SEED_KEY_SEED: [u8; 32] = [13u8; 32];

fn node_ws() -> String {
    std::env::var("NODE_WS").unwrap_or_else(|_| "ws://127.0.0.1:9944".to_string())
}

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: bytes::Bytes) {}
    fn push_segment(&self, _: u64, _: bytes::Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

/// Grant the seed child's key a statement allowance (sudo Alice on the dev node),
/// mirroring what test-all.sh does for the publisher key.
fn provision_seed_key(pubkey_hex: &str) {
    let root = env!("CARGO_MANIFEST_DIR"); // crates/unstation-node
    let script = format!("{root}/../../scripts/provision-allowance.sh");
    let status = Command::new("bash")
        .arg(&script)
        .arg(pubkey_hex)
        .env("NODE_WS", node_ws())
        .stdout(Stdio::null())
        .status()
        .expect("run provision-allowance.sh");
    assert!(status.success(), "seed-key provisioning failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "local chain: needs a --dev node with pallet-statement (see header)"]
async fn seed_binary_discovers_dials_and_caches_a_real_stream() {
    let _ = env_logger::try_init();
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");

    // ---- publisher identity (this process): the fixed, pre-provisioned e2e key ----
    // No dialect pin needed: the SDK's statement dialect is a deterministic default
    // (legacy) rather than hostname-inferred, and encode/decode are symmetric — this
    // very test caught the asymmetry that used to break cross-process reads here.
    unstation_chain::set_statement_store_endpoint(vec![node_ws()]);
    let kp = crypto::keypair_from_seed(&[11u8; 32]);
    unstation_chain::init_statement_store(kp);
    assert!(
        unstation_chain::wait_ready(Duration::from_secs(20)),
        "statement store must subscribe (is the dev node up + the key provisioned?)"
    );

    // ---- seed child identity: fixed key, written to a temp key dir, provisioned ----
    let seed_kp = crypto::keypair_from_seed(&SEED_KEY_SEED);
    let seed_pub_hex: String =
        crypto::public_bytes(&seed_kp).iter().map(|b| format!("{b:02x}")).collect();
    let key_dir = std::env::temp_dir().join("unstation-seed-e2e-key");
    let _ = std::fs::create_dir_all(&key_dir);
    std::fs::write(key_dir.join("peer_key"), SEED_KEY_SEED).expect("write seed key");
    provision_seed_key(&seed_pub_hex);

    // ---- publisher: session + live-publisher node + presence + on-chain live edge ----
    let stream = StreamId(crypto::blake2b256(STREAM_NAME.as_bytes()));
    let (pub_tx, pub_rx) = unbounded_channel::<EngineEvent>();
    let session = Session::start(stream, 1, vec![], pub_tx.clone()).expect("publisher session");
    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Publisher,
        window: 64,
        tick: Duration::from_millis(50),
        seg_ms: 250,
        upload_budget_bps: 100_000_000,
        weights: Default::default(),
    };
    let publisher = MeshNode::new_live_publisher(session.my_peer, cfg, 8_000, Arc::new(NullSink))
        .with_stream_id(stream.0)
        .with_edge_signer(Arc::new(IdentityEdgeSigner))
        .with_presence_book(session.presence_book())
        .with_ban_list(session.ban_list());
    tokio::spawn(publisher.run(pub_rx, Duration::from_millis(50), None));
    let _presence = session.spawn_presence(80_000_000, true, Arc::new(AtomicBool::new(true)));
    let (edge_tx, edge_rx) = unbounded_channel();
    let _edge = session.spawn_edge_publisher(edge_rx);

    // ---- spawn the REAL seed binary against the same chain + stream ----
    let mut child = Command::new(env!("CARGO_BIN_EXE_unstation-node"))
        .arg(STREAM_NAME)
        .env("HOST_STATEMENT_STORE_WS_ENDPOINTS", node_ws())
        .env("UNSTATION_NODE_KEY_DIR", &key_dir)
        .env("UNSTATION_BIND_ADDR", "127.0.0.1")
        .env("UNSTATION_STUN", " ") // loopback: no external STUN
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn unstation-node");

    // Tail the seed's stderr (env_logger writes there) for heartbeat lines.
    let cached_head = Arc::new(Mutex::new(0u64));
    let seed_peers = Arc::new(Mutex::new(0usize));
    {
        let stderr = child.stderr.take().expect("child stderr");
        let cached_head = cached_head.clone();
        let seed_peers = seed_peers.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                // e.g. "[seed] peers=1 cached_head=42 uplink=0kbps …"
                if let Some(rest) = line.split("cached_head=").nth(1) {
                    if let Ok(h) = rest.split_whitespace().next().unwrap_or("").parse::<u64>() {
                        *cached_head.lock().unwrap() = h;
                    }
                }
                if let Some(rest) = line.split("peers=").nth(1) {
                    if let Ok(p) = rest.split_whitespace().next().unwrap_or("").parse::<usize>() {
                        *seed_peers.lock().unwrap() = p;
                    }
                }
                eprintln!("[seed-child] {line}");
            }
        });
    }

    // ---- produce a live stream: one 8KB segment every 250ms ----
    // The seed needs time to boot its identity, discover on-chain, and complete ICE;
    // the edge poller + picker then backfill whatever it missed.
    let producer = {
        let pub_tx = pub_tx.clone();
        tokio::spawn(async move {
            for i in 0..240u64 {
                let seg = bytes::Bytes::from(vec![(i as u8).wrapping_mul(7).wrapping_add(1); 8_000]);
                let id = crypto::segment_id(&seg);
                let _ = pub_tx.send(EngineEvent::Produced { seq: i, id, bytes: seg });
                let _ = edge_tx.send((i, id));
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };

    // ---- assert: the seed connects and its cached head follows the live edge ----
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut ok_peers, mut ok_head) = (false, false);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        ok_peers = *seed_peers.lock().unwrap() >= 1;
        ok_head = *cached_head.lock().unwrap() >= 10;
        if ok_peers && ok_head {
            break;
        }
    }
    producer.abort();
    let _ = child.kill();
    let _ = child.wait();
    session.shutdown();

    assert!(ok_peers, "the seed never connected to the publisher over real WebRTC");
    assert!(
        ok_head,
        "the seed's cached head never reached the live window (last={})",
        *cached_head.lock().unwrap()
    );
}
