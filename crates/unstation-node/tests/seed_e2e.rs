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

/// Honest-failure path: a seed whose key has NO allowance must not sit alive-but-
/// invisible (the old behavior: "joining swarm", peers=0, noAllowance warn spam).
/// It must exit 78 (EX_CONFIG — the systemd unit's RestartPreventExitStatus) with
/// the pair instruction on stderr. Uses a fresh, deliberately unprovisioned key dir
/// against the same local dev chain.
#[test]
#[ignore = "local chain: needs a --dev node with pallet-statement (see header)"]
fn seed_binary_fails_loud_without_an_allowance() {
    let key_dir = std::env::temp_dir().join(format!("unstation-seed-noallow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&key_dir);
    std::fs::create_dir_all(&key_dir).expect("create key dir");

    // The boot probe retries for its full 60s window before classifying — this test
    // runs ~100s. `output()` waits for exit and captures stderr.
    let output = Command::new(env!("CARGO_BIN_EXE_unstation-node"))
        .arg("noallow-e2e")
        .env("HOST_STATEMENT_STORE_WS_ENDPOINTS", node_ws())
        .env("UNSTATION_NODE_KEY_DIR", &key_dir)
        .env_remove("UNSTATION_NODE_MNEMONIC")
        .env("RUST_LOG", "info")
        .output()
        .expect("run unstation-node");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(78),
        "expected exit 78 (identity unusable), got {:?}; stderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("unstation-node pair"),
        "the error must tell the operator the fix; stderr:\n{stderr}"
    );
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

// ═════════════════════════ open-relay recruitment e2e ═════════════════════════
//
// A BARE `unstation-node` (no stream arg) announces a VolunteerRecord on the global
// rendezvous and serves whatever verified recruitments land in its inbox. This test
// exercises that whole loop against the real binary + real chain: one publisher
// identity runs TWO live streams in-process, recruits the open seed onto both (the
// per-publisher cap is 2), and the seed must fetch + verify each stream's signed
// manifest, join both swarms over real WebRTC, and cache both live windows. Stopping
// one stream must then get its worker stall-evicted while the other keeps serving.
//
// The seed verifies a recruitment by fetching its manifest CID through the Bulletin
// IPFS-gateway path (`HOST_BULLETIN_IPFS_GATEWAYS`), where the body is checked
// against the content key (blake2b-256(body) == key). The test serves the two signed
// manifests from a tiny local HTTP server that answers the gateway's `/ipfs/<cid>`
// paths with the exact signed bytes — hermetic, and the content-address check still
// runs for real in the child.

/// The open seed child's fixed identity seed (distinct from the other tests' keys).
const OPEN_SEED_KEY_SEED: [u8; 32] = [17u8; 32];
const RECRUIT_STREAM_A: &str = "recruit-e2e-a";
const RECRUIT_STREAM_B: &str = "recruit-e2e-b";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every provisioning in this suite is a sudo-Alice extrinsic via
/// provision-allowance.sh, which has NO retry: two api-cli processes that fetch
/// Alice's nonce in the same instant sign identical nonces and the second submit
/// dies with `1014: Priority is too low`. The tests here run in parallel and all
/// reach their provisioning call a few seconds after suite start, so the stampede is
/// LIKELY, and this test cannot add a retry to its sibling. Instead it yields the
/// turn: watch briefly for a sibling's provision-allowance.sh to appear, and if one
/// does, wait (bounded) for it to finish before our own calls start. Our own calls
/// additionally retry (see [`provision_key_retrying`]), so losing a residual race
/// costs a retry here, never a suite failure.
fn yield_provisioning_turn() {
    let sibling_running = || {
        Command::new("pgrep")
            .args(["-f", "provision-allowance.sh"])
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // Long enough for a parallel sibling to get through its statement-store init and
    // reach ITS provisioning call; nothing seen by then means nothing is coming.
    let watch_until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < watch_until {
        if sibling_running() {
            let drain_until = Instant::now() + Duration::from_secs(60);
            while Instant::now() < drain_until && sibling_running() {
                std::thread::sleep(Duration::from_millis(500));
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Provision a statement allowance with a few retries: the suite's tests run in
/// parallel and each provisioning is a sudo-Alice extrinsic, so two calls signed in
/// the same instant can race on the nonce — the loser just tries again.
fn provision_key_retrying(pubkey_hex: &str) {
    let root = env!("CARGO_MANIFEST_DIR"); // crates/unstation-node
    let script = format!("{root}/../../scripts/provision-allowance.sh");
    for attempt in 1..=3 {
        let status = Command::new("bash")
            .arg(&script)
            .arg(pubkey_hex)
            .env("NODE_WS", node_ws())
            .stdout(Stdio::null())
            .status()
            .expect("run provision-allowance.sh");
        if status.success() {
            return;
        }
        eprintln!("[recruit-e2e] provisioning attempt {attempt} for {pubkey_hex} failed; retrying");
        std::thread::sleep(Duration::from_secs(3));
    }
    panic!("provisioning failed for {pubkey_hex} after 3 attempts");
}

/// lowercase base32, no padding — byte-for-byte the encoder in useragent-kit's
/// `host-chain/src/bulletin.rs` (`base32_lower_no_pad`), which the child uses to turn
/// a preimage key into the gateway CID. Reimplemented here because the SDK crate is
/// not a dependency of this test; `bulletin_cid_for_digest` pins it to the SDK's own
/// test vector so drift fails loudly.
fn base32_lower_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut output = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut buffer = 0u16;
    let mut bits = 0u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            output.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        output.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    output
}

/// The gateway URL path component for a Bulletin preimage: CIDv1 ‖ raw codec ‖
/// blake2b-256 multihash (code 0xb220) ‖ 32-byte digest, base32-lower with the `b`
/// multibase prefix — exactly `preimage_key_to_cid` in the SDK's bulletin.rs.
fn bulletin_cid_for_digest(digest: &[u8; 32]) -> String {
    let mut cid = Vec::with_capacity(36);
    cid.push(0x01); // CIDv1
    cid.push(0x55); // raw codec
    cid.extend_from_slice(&[0xa0, 0xe4, 0x02]); // blake2b-256 multihash code 0xb220
    cid.push(0x20); // digest length
    cid.extend_from_slice(digest);
    format!("b{}", base32_lower_no_pad(&cid))
}

/// Build + sign a stream manifest exactly the way the app's `spawn_manifest_publish`
/// does (same fields, same identity signer under MANIFEST_CONTEXT), and return
/// `(signed_manifest_scale_bytes, preimage_key_hex, gateway_cid)`.
///
/// The wire form of `SignedManifest { manifest, sig }` is SCALE = `manifest.encode()
/// ‖ sig` ([u8; 64] encodes as its raw bytes), and `Manifest::signing_payload()` IS
/// `manifest.encode()` — so the signed bytes are built from public API without this
/// test needing the codec crate. The child decodes and signature-checks them for
/// real, so any drift here fails the test, not the assertion.
fn signed_manifest_for(stream: StreamId) -> (Vec<u8>, String, String) {
    use unstation_core::manifest::{Kind, Manifest, Track};
    let manifest = Manifest {
        stream_id: stream,
        kind: Kind::Live,
        codec: "avc1.640028,mp4a.40.2".into(),
        init_segment_cid: String::new(), // the app degrades to this when the init put fails
        target_segment_ms: 250,          // matches the producer's 250ms cadence (LL parts)
        ll_mode: true,
        tracks: vec![Track { id: "v".into(), bitrate: 0, w: 0, h: 0 }],
        publisher: unstation_chain::identity_public().expect("chain identity initialized"),
        created_at: unix_now(),
        encrypted: false,
    };
    let mut bytes = manifest.signing_payload();
    let sig = unstation_chain::sign_with_identity(&bytes).expect("identity signs the manifest");
    bytes.extend_from_slice(&sig);
    let digest = crypto::blake2b256(&bytes);
    let key = format!("0x{}", crypto::hex32(&digest));
    (bytes, key, bulletin_cid_for_digest(&digest))
}

/// A tiny content-addressed gateway: serves `routes[path]` (e.g. "/ipfs/<cid>") as
/// raw bytes on a loopback port so the CHILD's `HOST_BULLETIN_IPFS_GATEWAYS` fetches
/// resolve hermetically. Binds port 0 (no reuse flakes); returns the base URL.
fn spawn_manifest_gateway(routes: std::collections::HashMap<String, Vec<u8>>) -> String {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind manifest gateway");
    let addr = listener.local_addr().expect("gateway addr");
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let routes = routes.clone();
            std::thread::spawn(move || {
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if req.windows(4).any(|w| w == b"\r\n\r\n") || req.len() > 65_536 {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&req);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let resp = match routes.get(&path) {
                    Some(body) => {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = sock.write_all(&resp);
                let _ = sock.flush();
            });
        }
    });
    format!("http://{addr}")
}

/// One in-process live publisher: session + publisher mesh node + presence + on-chain
/// edge + a synthetic segment producer — the existing single-stream test's publisher
/// block, packaged so the recruitment test can run two side by side.
struct TestPublisher {
    session: Session,
    pub_tx: tokio::sync::mpsc::UnboundedSender<EngineEvent>,
    producer: tokio::task::JoinHandle<()>,
    presence: tokio::task::JoinHandle<()>,
    edge: tokio::task::JoinHandle<()>,
}

impl TestPublisher {
    fn start(stream: StreamId, fill_salt: u8) -> Self {
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
        let node = MeshNode::new_live_publisher(session.my_peer, cfg, 8_000, Arc::new(NullSink))
            .with_stream_id(stream.0)
            .with_edge_signer(Arc::new(IdentityEdgeSigner))
            .with_presence_book(session.presence_book())
            .with_ban_list(session.ban_list());
        tokio::spawn(node.run(pub_rx, Duration::from_millis(50), None));
        let presence = session.spawn_presence(80_000_000, true, Arc::new(AtomicBool::new(true)));
        let (edge_tx, edge_rx) = unbounded_channel();
        let edge = session.spawn_edge_publisher(edge_rx);
        // One 8KB segment every 250ms, long enough to outlive every phase deadline.
        let producer = {
            let pub_tx = pub_tx.clone();
            tokio::spawn(async move {
                for i in 0..4000u64 {
                    let fill = (i as u8).wrapping_mul(7).wrapping_add(fill_salt);
                    let seg = bytes::Bytes::from(vec![fill; 8_000]);
                    let id = crypto::segment_id(&seg);
                    let _ = pub_tx.send(EngineEvent::Produced { seq: i, id, bytes: seg });
                    let _ = edge_tx.send((i, id));
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
        };
        Self { session, pub_tx, producer, presence, edge }
    }

    /// Stop producing AND leave the swarm (close the PCs): the seed's view of this
    /// stream must go head-stalled + peerless, exactly like a publisher going away.
    fn stop(&self) {
        self.producer.abort();
        self.presence.abort();
        self.edge.abort();
        let _ = self.pub_tx.send(EngineEvent::Stop);
        self.session.shutdown();
    }
}

/// Parsed view of the open seed child's stderr (supervisor heartbeats + events).
#[derive(Default)]
struct OpenSeedState {
    /// Latest aggregate `[seed] streams=N/...` count.
    streams_serving: Option<usize>,
    /// Streams a worker was spawned for (`joining swarm for "<name>"`).
    joined: std::collections::HashSet<String>,
    /// Latest per-stream heartbeat: name → (peers, cached_head).
    per_stream: std::collections::HashMap<String, (usize, u64)>,
    /// `[seed] evicting stream=<name> reason=<reason>` lines, verbatim tails.
    evictions: Vec<String>,
}

fn parse_open_seed_line(line: &str, state: &Mutex<OpenSeedState>) {
    fn num_after<T: std::str::FromStr>(rest: &str, key: &str) -> Option<T> {
        rest.split(key).nth(1)?.split_whitespace().next()?.parse().ok()
    }
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    // Aggregate: `[seed] streams=2/4 (0 pinned) peers_total=…`
    if let Some(rest) = line.split("[seed] streams=").nth(1) {
        if let Some(n) = rest.split('/').next().and_then(|s| s.parse::<usize>().ok()) {
            st.streams_serving = Some(n);
        }
        return;
    }
    // Worker spawn: `[seed] joining swarm for "<name>" (stream <hex>)`
    if let Some(rest) = line.split("joining swarm for \"").nth(1) {
        if let Some(name) = rest.split('"').next() {
            st.joined.insert(name.to_string());
        }
        return;
    }
    // Eviction: `[seed] evicting stream=<name> reason=<reason> …`
    if let Some(rest) = line.split("evicting stream=").nth(1) {
        st.evictions.push(rest.trim().to_string());
        return;
    }
    // Per-stream heartbeat: `[seed] stream=<name> peers=N cached_head=M …`
    if let Some(rest) = line.split("[seed] stream=").nth(1) {
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        if let (Some(p), Some(h)) = (num_after::<usize>(rest, "peers="), num_after::<u64>(rest, "cached_head=")) {
            st.per_stream.insert(name, (p, h));
        }
    }
}

/// Poll `cond` every second until it holds or `secs` elapse.
async fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    cond()
}

/// Sign + publish a Recruit for `stream` into `vol`'s sealed inbox, from the
/// process identity (the publisher). Mirrors the intended app-side flow: fresh
/// `issued_at`, signature under RECRUIT_CONTEXT over a payload that binds the
/// volunteer's inbox, and the same time-derived statement priority scheme
/// unstation-chain uses for rendezvous records (later always supersedes).
async fn publish_recruit(
    stream: StreamId,
    manifest_key: &str,
    vol: &unstation_core::volunteer::VolunteerRecord,
) -> Result<(), String> {
    use unstation_core::types::PeerId;
    use unstation_core::volunteer::{RecruitAction, Recruitment, RECRUITMENT_VERSION, RECRUIT_CONTEXT};
    let mut rec = Recruitment {
        version: RECRUITMENT_VERSION,
        stream_id: stream.0,
        manifest_cid: manifest_key.to_string(),
        publisher: unstation_chain::identity_public().ok_or("no identity")?,
        issued_at: unix_now(),
        action: RecruitAction::Recruit,
        sig: [0u8; 64],
    };
    let payload = rec.signing_payload(&PeerId(vol.peer_id));
    rec.sig = unstation_chain::sign_with_identity_ctx(RECRUIT_CONTEXT, &payload)
        .ok_or("no identity to sign the recruitment")?;
    // Wire form = SCALE(every field except `sig`) ‖ sig, and `signing_payload` is
    // exactly that prefix ‖ the 32-byte volunteer peer id (see Recruitment docs; the
    // layout is pinned by unstation-core's recruitment_wire_format_is_frozen test) —
    // so the encoded statement is the payload minus its binding suffix, plus the sig.
    let mut encoded = payload[..payload.len() - 32].to_vec();
    encoded.extend_from_slice(&rec.sig);
    // Same prio derivation as unstation-chain's announce_prio: seconds since its
    // PRIO_EPOCH (June 2025), shifted one bit — later recruitments supersede.
    let prio = ((rec.issued_at.saturating_sub(1_750_000_000) & 0x7FFF_FFFF) as u32) << 1;
    unstation_chain::volunteer::publish_recruitment(&vol.enc_pub, &vol.peer_id, &encoded, prio)
        .await
        .map_err(|e| format!("publish_recruitment: {e:?}"))
}

/// The full open-relay loop against the real binary: announce → recruit (2 streams,
/// one publisher) → verify manifests via the (local) Bulletin gateway → serve both →
/// stall-evict the stream whose publisher goes away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "local chain: needs a --dev node with pallet-statement (see header)"]
async fn open_seed_serves_two_streams_on_recruitment() {
    let _ = env_logger::try_init();
    let t0 = Instant::now();
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");

    // ---- this process = the publisher identity (fixed e2e key), like the other test ----
    unstation_chain::set_statement_store_endpoint(vec![node_ws()]);
    let kp = crypto::keypair_from_seed(&[11u8; 32]);
    let publisher_pub_hex: String =
        crypto::public_bytes(&kp).iter().map(|b| format!("{b:02x}")).collect();
    unstation_chain::init_statement_store(kp);
    assert!(
        unstation_chain::wait_ready(Duration::from_secs(20)),
        "statement store must subscribe (is the dev node up?)"
    );

    // ---- provision the publisher key + the open seed child's key ----
    let seed_kp = crypto::keypair_from_seed(&OPEN_SEED_KEY_SEED);
    let seed_pub = crypto::public_bytes(&seed_kp);
    let seed_pub_hex: String = seed_pub.iter().map(|b| format!("{b:02x}")).collect();
    yield_provisioning_turn();
    provision_key_retrying(&publisher_pub_hex);
    provision_key_retrying(&seed_pub_hex);
    let key_dir = std::env::temp_dir().join("unstation-open-seed-e2e-key");
    let _ = std::fs::create_dir_all(&key_dir);
    std::fs::write(key_dir.join("peer_key"), OPEN_SEED_KEY_SEED).expect("write seed key");
    eprintln!("[recruit-e2e] +{:?} provisioned publisher + seed keys", t0.elapsed());

    // ---- two live streams from the ONE publisher identity ----
    let stream_a = StreamId(crypto::blake2b256(RECRUIT_STREAM_A.as_bytes()));
    let stream_b = StreamId(crypto::blake2b256(RECRUIT_STREAM_B.as_bytes()));
    // Heartbeat names for recruited workers are the stream id's 16-hex-char prefix
    // (recruit.rs's canon hint — recruitments carry no canonical name).
    let hint_a = crypto::hex32(&stream_a.0)[..16].to_string();
    let hint_b = crypto::hex32(&stream_b.0)[..16].to_string();
    let pub_a = TestPublisher::start(stream_a, 1);
    let pub_b = TestPublisher::start(stream_b, 101);

    // Publisher A goes SHIELDED with the hardened allowlist, BEFORE any recruitment:
    // it answers only peers whose chain-verified presence signer is the seed child's
    // account. The recruited seed connecting and stream A serving below is then the
    // end-to-end proof of the hardened origin-shield path; B stays unshielded as the
    // control. Shield needs relay discovery running (the publisher is otherwise a
    // pure answerer and would never learn the seed's chain-signed presence).
    pub_a.session.set_shield(true);
    pub_a.session.set_shield_allow(std::iter::once(seed_pub).collect());
    let relay_discovery_a = pub_a.session.spawn_relay_discovery();

    // ---- signed manifests, served from a local content-addressed gateway ----
    let (bytes_a, key_a, cid_a) = signed_manifest_for(stream_a);
    let (bytes_b, key_b, cid_b) = signed_manifest_for(stream_b);
    // Pin the CID derivation to the SDK's own bulletin.rs test vector, so a drift in
    // our re-implementation fails HERE and not as an opaque fetch timeout.
    {
        let mut digest = [0u8; 32];
        let raw = "743fbba4834597a4aa439de3d34ffba2f8f95897c1e1f457f2c4616a49390414";
        for (i, chunk) in raw.as_bytes().chunks(2).enumerate() {
            digest[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
        }
        assert_eq!(
            bulletin_cid_for_digest(&digest),
            "bafk2bzaceb2d7o5eqnczpjfkioo6hu2p7orpr6kys7a6d5cx6lcgc2sjhecbi",
            "CID derivation drifted from the SDK's preimage_key_to_cid"
        );
    }
    let mut routes = std::collections::HashMap::new();
    routes.insert(format!("/ipfs/{cid_a}"), bytes_a);
    routes.insert(format!("/ipfs/{cid_b}"), bytes_b);
    let gateway = spawn_manifest_gateway(routes);
    eprintln!("[recruit-e2e] manifest gateway at {gateway} (a={key_a} b={key_b})");

    // ---- spawn the REAL binary BARE: an open relay with no stream configured ----
    let spawned_at_unix = unix_now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_unstation-node"))
        .env("HOST_STATEMENT_STORE_WS_ENDPOINTS", node_ws())
        .env("UNSTATION_NODE_KEY_DIR", &key_dir)
        .env_remove("UNSTATION_NODE_MNEMONIC")
        .env("HOST_BULLETIN_IPFS_GATEWAYS", &gateway)
        .env("HOST_BULLETIN_HTTP_RPC_ENDPOINTS", &gateway) // hermetic: never used for lookups
        .env("UNSTATION_BIND_ADDR", "127.0.0.1")
        .env("UNSTATION_STUN", " ") // loopback: no external STUN
        .env("UNSTATION_NODE_MAX_STREAMS", "4")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn unstation-node (open relay)");

    let state = Arc::new(Mutex::new(OpenSeedState::default()));
    {
        let stderr = child.stderr.take().expect("child stderr");
        let state = state.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                parse_open_seed_line(&line, &state);
                eprintln!("[open-seed] {line}");
            }
        });
    }

    // ---- the phases, with teardown guaranteed after (asserts deferred) ----
    let outcome: Result<(), String> = async {
        // 1. The child's volunteer announce lands on the global rendezvous. Filter by
        //    THIS boot's issue time: a previous run's record for the same account can
        //    linger on the chain for ~1h with a stale (process-random) inbox peer id.
        let mut volunteer = None;
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            match unstation_chain::volunteer::read_volunteers(32, unix_now()).await {
                Ok(records) => {
                    if let Some(rec) = records.into_iter().find(|r| {
                        r.account == seed_pub && r.issued_at + 5 >= spawned_at_unix
                    }) {
                        volunteer = Some(rec);
                        break;
                    }
                }
                Err(e) => eprintln!("[recruit-e2e] read_volunteers failed (retrying): {e:?}"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let vol = volunteer.ok_or("the open seed never announced on the volunteer rendezvous")?;
        eprintln!(
            "[recruit-e2e] +{:?} volunteer announce seen (max_streams={} caps={}bps)",
            t0.elapsed(),
            vol.max_streams,
            vol.caps_upload_bps
        );

        // 2. Recruit stream A, and wait for its worker to spawn BEFORE recruiting B:
        //    both recruitments share one statement channel (same publisher account,
        //    same inbox topic), so publishing B too early would replace A on-chain
        //    before the seed has read it.
        publish_recruit(stream_a, &key_a, &vol).await?;
        if !wait_for(75, || state.lock().unwrap().joined.contains(&hint_a)).await {
            return Err(format!(
                "the seed never spawned a worker for stream A ({hint_a}); state: joined={:?}",
                state.lock().unwrap().joined
            ));
        }
        eprintln!("[recruit-e2e] +{:?} worker A spawned (manifest verified)", t0.elapsed());

        publish_recruit(stream_b, &key_b, &vol).await?;
        if !wait_for(75, || state.lock().unwrap().joined.contains(&hint_b)).await {
            return Err(format!(
                "the seed never spawned a worker for stream B ({hint_b}); state: joined={:?}",
                state.lock().unwrap().joined
            ));
        }
        eprintln!("[recruit-e2e] +{:?} worker B spawned (manifest verified)", t0.elapsed());

        // 3. Both workers serve: aggregate streams=2, and each stream's heartbeat
        //    shows a live WebRTC peer and an advancing cached head.
        let serving = |st: &OpenSeedState| {
            st.streams_serving == Some(2)
                && st.per_stream.get(&hint_a).is_some_and(|&(p, h)| p >= 1 && h >= 10)
                && st.per_stream.get(&hint_b).is_some_and(|&(p, h)| p >= 1 && h >= 10)
        };
        if !wait_for(120, || serving(&state.lock().unwrap())).await {
            let st = state.lock().unwrap();
            return Err(format!(
                "the seed never reached full service on both streams: streams={:?} per_stream={:?}",
                st.streams_serving, st.per_stream
            ));
        }
        eprintln!("[recruit-e2e] +{:?} both streams serving (peers + advancing heads)", t0.elapsed());

        // 4. Publisher A goes away: its worker must stall-evict (head stops advancing
        //    for stall_evict=60s, sweep every 10s) while B keeps serving.
        pub_a.stop();
        if !wait_for(150, || state.lock().unwrap().streams_serving == Some(1)).await {
            let st = state.lock().unwrap();
            return Err(format!(
                "the seed never evicted the dead stream: streams={:?} evictions={:?}",
                st.streams_serving, st.evictions
            ));
        }
        let st = state.lock().unwrap();
        if !st.evictions.iter().any(|e| e.starts_with(&hint_a)) {
            return Err(format!(
                "streams dropped to 1 but stream A ({hint_a}) was not the evictee: {:?}",
                st.evictions
            ));
        }
        if st.evictions.iter().any(|e| e.starts_with(&hint_b)) {
            return Err(format!(
                "the healthy stream B ({hint_b}) was evicted: {:?}",
                st.evictions
            ));
        }
        if !st.per_stream.get(&hint_b).is_some_and(|&(p, h)| p >= 1 && h >= 10) {
            return Err(format!(
                "stream B stopped serving after A's eviction: {:?}",
                st.per_stream
            ));
        }
        eprintln!("[recruit-e2e] +{:?} stream A evicted ({:?}); B still serving", t0.elapsed(), st.evictions);
        Ok(())
    }
    .await;

    // ---- teardown, same discipline as the sibling test (repeatable suite runs) ----
    let _ = child.kill();
    let _ = child.wait();
    relay_discovery_a.abort();
    pub_a.stop();
    pub_b.stop();

    assert!(outcome.is_ok(), "open-relay recruitment e2e failed: {}", outcome.unwrap_err());
    eprintln!("[recruit-e2e] PASS in {:?}", t0.elapsed());
}
