//! Tauri bridge for Unstation.
//!
//! A thin command/event layer over the **real** stack:
//!   - `start_publish` runs the RTMP→CMAF segmenter, a live publisher `MeshNode`,
//!     and a [`Session`] that announces the stream on the Polkadot statement store
//!     and seeds the live-edge manifest — so other machines can discover it.
//!   - `start_watch` resolves a stream by name, discovers the publisher over the
//!     statement store, connects to it over **real WebRTC** (`transport-libdc`),
//!     and plays the verified segments through the localhost HLS re-server.
//!
//! Identity: each process derives a fresh statement-store signing keypair from a
//! generated mnemonic (the chain SDK's `WalletManager`); the `Session` boots the
//! statement store with it. Stable/phone-paired identity (spec §7) is M4.

// Publish-only items (PublishSession, NullSink, the manifest/Bulletin + segmenter code)
// are `#[cfg(feature = "publish")]`; when publish is off (the Android watch build) they're
// absent, so silence the resulting dead-code / unused-import noise for that config only.
#![cfg_attr(not(feature = "publish"), allow(dead_code, unused_imports))]

use bytes::Bytes;
use hls_server::HlsServer;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::manifest::{Kind, Manifest, OriginOfRecord, SignedManifest, Track};
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::types::{SegmentId, Seq, StreamId};
use unstation_chain::BulletinOrigin;
use unstation_session::{IdentityEdgeSigner, Session};

/// Nominal segment size for the picker's expected-delivery-time estimates.
const SEG_BYTES: u64 = 200_000;
/// Engine tick.
const TICK: Duration = Duration::from_millis(100);

#[derive(Default)]
struct AppState {
    signed_in: Mutex<bool>,
    watch: Mutex<Option<WatchSession>>,
    #[cfg(feature = "publish")]
    publish: Mutex<Option<PublishSession>>,
    /// True once the statement store has been initialized with the paired
    /// (allowance-backed) identity via `set_chain_identity`. Publishing/watching
    /// requires this — an unprovisioned key can't write to the chain.
    chain_ready: Mutex<bool>,
}

/// An active watch: the HLS server feeding the player, the viewer node's inbox,
/// the session (kept alive to hold the transport + signaling tasks), and the
/// background tasks (discover/dial, stats, node loop).
struct WatchSession {
    _hls: HlsServer,
    node_tx: UnboundedSender<EngineEvent>,
    session: Session,
    tasks: Vec<JoinHandle<()>>,
    /// Retained so the UI can rebuild the player when navigating back to it.
    info: WatchInfo,
}

/// An active publish: RTMP ingest, the self-preview HLS, the feeder task, the
/// publisher node's inbox, and the session.
#[cfg(feature = "publish")]
struct PublishSession {
    _hls: HlsServer,
    /// Owns the ffmpeg ingest listener; aborting it kills ffmpeg via `Drop`.
    feeder: JoinHandle<()>,
    stats: JoinHandle<()>,
    pub_tx: UnboundedSender<EngineEvent>,
    session: Session,
    /// Canonical stream name — lets `start_publish` re-attach instead of restarting.
    name: String,
    /// Retained so the UI can rebuild the Go-Live console when navigating back.
    info: PublishInfo,
    /// Whether fresh fragments are arriving right now (the feeder updates this);
    /// read by `publish_status` so a re-attaching UI gets the true live state.
    live: Arc<AtomicBool>,
}

/// A no-op sink for nodes that only cache + serve (publisher/seed), never render.
struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

#[derive(Serialize, Clone)]
struct SigninInfo {
    uri: String,
    signed_in: bool,
}

#[derive(Serialize, Clone)]
struct WatchInfo {
    hls_url: String,
    stream_id: String,
    publisher: String,
    peers: usize,
    rho: u32,
}

#[derive(Serialize, Clone)]
struct MeshStatsMsg {
    peers: usize,
    rho: u32,
    from_seed: u32,
    from_chain: u32,
    latency_s: f64,
    ice: String,
    mode: String,
    delivered: usize,
}

#[derive(Serialize, Clone)]
struct PublishStatsMsg {
    viewers: usize,
}

#[derive(Serialize, Clone)]
struct PublishHintMsg {
    message: String,
}

/// Live/idle state of the publisher, derived from whether fresh fragments are
/// actually arriving — NOT from the ffmpeg process state, so the UI matches the
/// video the viewer would see.
#[derive(Serialize, Clone)]
struct PublishStateMsg {
    live: bool,
}

/// Chain/network status surfaced to the UI (so failures like a missing statement-
/// store allowance aren't silent). `state` ∈ {"connecting","ready","error"}.
#[derive(Serialize, Clone)]
struct MeshStatusMsg {
    state: String,
    detail: String,
}

#[derive(Serialize, Clone)]
struct PublishInfo {
    ingest_server: String,
    stream_key: String,
    hls_url: String,
}

/// Snapshot of the current publish session, for the UI to re-attach the console.
#[derive(Serialize, Clone)]
struct PublishStatus {
    info: PublishInfo,
    name: String,
    live: bool,
    viewers: usize,
}

/// Snapshot of the current watch session, for the UI to re-attach the player.
#[derive(Serialize, Clone)]
struct WatchStatus {
    info: WatchInfo,
    peers: usize,
}

fn cfg(mode: Mode, role: Role) -> MeshConfig {
    MeshConfig {
        mode,
        role,
        window: 64,
        tick: TICK,
        seg_ms: 1000,
        upload_budget_bps: 80_000_000,
        weights: Default::default(),
    }
}

/// ICE servers. Host candidates carry a LAN on their own; a public STUN server lets
/// cross-subnet/NAT pairs find a route too (full relay/TURN is M4). Overridable via
/// `UNSTATION_STUN` (comma-separated URIs; set it empty for host-candidate-only,
/// e.g. an offline/air-gapped LAN where reaching a public STUN would only add delay).
fn stun() -> Vec<String> {
    match std::env::var("UNSTATION_STUN") {
        Ok(v) => v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect(),
        Err(_) => vec!["stun:stun.l.google.com:19302".into()],
    }
}

/// A stream's 32-byte id, derived from its human name (both sides resolve the
/// same name to the same id, so discovery topics line up).
/// Canonicalize a stream name so the publisher and a viewer derive the SAME id.
///
/// The publisher names a stream from a free-text title (e.g. "Friday Night
/// Football") while a viewer types the friendly share link ("friday-night-
/// football.dot"). Both must hash to one canonical string or discovery never
/// matches. This mirrors the UI's `slugify`: drop an optional `.dot` suffix,
/// lowercase, collapse runs of non-alphanumerics to single hyphens (trimmed).
/// Empty input → "my-stream" (same fallback the UI uses).
fn canonical_stream_name(input: &str) -> String {
    let s = input.trim();
    let s = s.strip_suffix(".dot").unwrap_or(s);
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "my-stream".to_string()
    } else {
        trimmed.to_string()
    }
}

fn stream_id_from(name: &str) -> StreamId {
    StreamId(crypto::blake2b256(canonical_stream_name(name).as_bytes()))
}

/// Per-app statement-store key directory — the OS-native app-data location
/// (`~/Library/Application Support/…` on macOS, `%APPDATA%` on Windows,
/// `~/.local/share/…` on Linux). The SDK persists the signing key here, so the
/// host keeps the same identity — and stays signed in — across launches.
/// The host OS (`macos` / `windows` / `linux`) — lets the UI choose native window
/// chrome per platform instead of assuming macOS.
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

#[tauri::command]
fn signin_status(state: State<'_, AppState>) -> bool {
    *state.signed_in.lock().unwrap()
}

#[tauri::command]
fn begin_signin() -> SigninInfo {
    // Real QR pairing (host-papp) runs in the webview (sso.js). This Rust command
    // remains a UI seam; the phone-granted session is threaded to the signer in M4.
    SigninInfo { uri: "polkadot://unstation/pair?v=1".into(), signed_in: false }
}

#[tauri::command]
fn complete_signin(state: State<'_, AppState>) -> bool {
    *state.signed_in.lock().unwrap() = true;
    true
}

#[tauri::command]
fn resolve_stream(target: String) -> String {
    crypto::hex32(&stream_id_from(&target).0)
}

/// Watch a stream by name: discover the publisher on the statement store, connect
/// over real WebRTC, and play verified segments via localhost HLS.
#[tauri::command]
async fn start_watch(
    app: AppHandle,
    state: State<'_, AppState>,
    target: String,
) -> Result<WatchInfo, String> {
    if !*state.chain_ready.lock().unwrap() {
        return Err("Sign in with the Polkadot app to watch — peers need a verified identity.".into());
    }
    let stream = stream_id_from(&target);
    log::info!("[watch] target={target:?} → stream_id={}", crypto::hex32(&stream.0));

    // Localhost HLS re-server → the webview <video> plays from here.
    let hls = HlsServer::start(1000).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let sink: Arc<dyn MediaSink> = Arc::new(hls.sink());
    // A second handle to the SAME local HLS server so the dial loop can install the CMAF
    // init segment it fetches from Bulletin, alongside the media segments the node feeds.
    let sink_for_init = sink.clone();

    // Viewer node inbox; the transport posts PeerConnected/Inbound here.
    let (view_tx, view_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC for this stream.
    let session = Session::start(stream, 1, stun(), view_tx.clone())?;

    // Real viewer node: starts with no known segments; the live-edge poller feeds
    // it `LiveEdge { seq, id }` so it knows what to fetch and how to verify it.
    let viewer = MeshNode::new_viewer(
        session.my_peer,
        cfg(Mode::Live, Role::Viewer),
        SEG_BYTES,
        sink,
        HashMap::new(),
        0,
    )
    // Off-chain signaling (#17): bind to this stream so gossiped live-edge signatures
    // verify; the publisher key arrives via SetPublisherKey once discovery confirms it.
    .with_stream_id(stream.0)
    // Discover + reshare peers in-mesh via the shared presence book (no per-viewer
    // chain write); the session dials from the same book.
    .with_presence_book(session.presence_book());
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(async move {
        let _ = viewer.run(view_rx, TICK, None).await;
    }));

    // Learn the live edge (segment ids) from the publisher.
    session.spawn_edge_poller(view_tx.clone());

    // Announce ourselves so other viewers can discover + reshare from us — the mesh
    // relays through volunteer peers, so a NAT-restricted node only needs to reach
    // *someone*. relay_opt_in = false, but a viewer that proves reachable (a peer
    // connects to it inbound) auto-promotes to advertising relay-capability — emergent,
    // self-organizing volunteer relays. (Presence write moves off-chain at scale.)
    session.spawn_presence(20_000_000, false);

    // Discover the publisher and dial it, then keep the connection alive: if the dial
    // stalls (no connect within the timeout) or the peer later drops, re-discover and
    // re-dial. (watch returns now so the UI can attach the player while this runs.)
    {
        let s = session.clone();
        let appc = app.clone();
        let vtx = view_tx.clone();
        let sink_init = sink_for_init.clone();
        tasks.push(tokio::spawn(async move {
            let mut init_installed = false;
            loop {
                // Mesh-as-relay (M4): hold a few peer connections, dialing whichever
                // discovered candidates we can reach. A NAT-restricted viewer only needs
                // ONE reachable peer — the swarm relays the rest. No central relay required.
                const TARGET_DEGREE: usize = 3;
                let mut dialed = Vec::new();
                if s.peer_count() < TARGET_DEGREE {
                    for cand in s.discover_peers(8).await {
                        if s.peer_count() >= TARGET_DEGREE {
                            break;
                        }
                        // M2 trust gate: only the publisher announces a signed-manifest CID;
                        // verify it against its PERSONHOOD key (`publisher`, stable across
                        // the publisher's devices) and skip impostors. `peer_id` is only a
                        // per-device routing address, so it can't be the trust anchor.
                        // Resharing viewers carry no manifest — their segments are still
                        // hash-verified against the publisher-authenticated live edge.
                        if let Some(cid) = cand.manifest_cid.clone() {
                            match BulletinOrigin.fetch_manifest(cid).await {
                                Ok(m) if m.verify(&cand.publisher).is_ok() => {
                                    // Verified publisher → its personhood key is the trust
                                    // anchor for gossiped live-edge announcements (#17).
                                    let _ = vtx.send(EngineEvent::SetPublisherKey {
                                        key: cand.publisher,
                                    });
                                    // Install the CMAF init segment (ftyp+moov) into the local
                                    // HLS server so it can serve /init.mp4 — hls.js needs it
                                    // before ANY media fragment (the playlist's EXT-X-MAP), else
                                    // it can't initialize MSE (fragLoadError). Once per watch.
                                    if !init_installed && !m.manifest.init_segment_cid.is_empty() {
                                        match BulletinOrigin.fetch_bytes(&m.manifest.init_segment_cid).await {
                                            Ok(b) => {
                                                log::info!("[watch] init segment installed ({} B)", b.len());
                                                sink_init.push_init(b);
                                                init_installed = true;
                                            }
                                            Err(e) => log::warn!("[watch] init fetch failed: {e:?}"),
                                        }
                                    }
                                }
                                Ok(_) => {
                                    let _ = appc.emit(
                                        "mesh-status",
                                        MeshStatusMsg { state: "error".into(), detail: "Couldn’t verify a broadcaster — skipping.".into() },
                                    );
                                    continue;
                                }
                                Err(e) => log::warn!("[watch] manifest fetch failed ({e:?}); proceeding (segments still hash-verified)"),
                            }
                        }
                        s.dial(cand.peer_id);
                        dialed.push(cand.peer_id);
                    }
                }
                if s.peer_count() == 0 && dialed.is_empty() {
                    // No candidates discovered yet — keep looking.
                    let _ = appc.emit(
                        "mesh-status",
                        MeshStatusMsg { state: "connecting".into(), detail: "Reaching the mesh…".into() },
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                // Give fresh dials up to ~12s to open a channel.
                let mut waited = 0u64;
                while s.peer_count() == 0 && waited < 12_000 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    waited += 500;
                }
                if s.peer_count() == 0 {
                    // All dials stalled (lost signal / ICE failure): abandon them so the
                    // transport accepts fresh dials, then retry other candidates.
                    let _ = appc.emit(
                        "mesh-status",
                        MeshStatusMsg { state: "connecting".into(), detail: "Still reaching the mesh…".into() },
                    );
                    for pid in dialed {
                        s.close(pid);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                // Connected to at least one peer — the mesh delivers. Re-evaluate
                // periodically to top up toward TARGET_DEGREE and replace dropped peers.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }));
    }

    // Stream real mesh stats to the webview (live peer count from the transport).
    {
        let s = session.clone();
        let appc = app.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let peers = s.peer_count();
                let _ = appc.emit(
                    "mesh-stats",
                    MeshStatsMsg {
                        peers,
                        rho: if peers > 0 { 100 } else { 0 },
                        from_seed: 0,
                        from_chain: 0,
                        latency_s: 0.0,
                        ice: if peers > 0 { "direct".into() } else { "connecting".into() },
                        mode: "p2p".into(),
                        delivered: 0,
                    },
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }));
    }

    let info = WatchInfo {
        hls_url,
        stream_id: resolve_stream(target.clone()),
        publisher: target,
        peers: 0,
        rho: 0,
    };
    *state.watch.lock().unwrap() = Some(WatchSession {
        _hls: hls,
        node_tx: view_tx,
        session,
        tasks,
        info: info.clone(),
    });

    Ok(info)
}

#[tauri::command]
fn stop_watch(state: State<'_, AppState>) {
    if let Some(sess) = state.watch.lock().unwrap().take() {
        let _ = sess.node_tx.send(EngineEvent::Stop);
        for t in sess.tasks {
            t.abort();
        }
        // `_hls` / `_session` drop here.
    }
}

/// Bridge the QR-paired statement-store allowance to the Rust signer. The JS side
/// extracts the per-app **slot signing key** (which the phone granted an on-chain
/// allowance at pairing) and hands it here; we initialize the process-global
/// statement store with it so every mesh write (presence/SDP/edge) is allowance-
/// backed. Without this, a fresh unprovisioned key is rejected `noAllowance` and
/// nothing is discoverable. Idempotent (the store is process-global; init once).
#[tauri::command]
fn set_chain_identity(
    app: AppHandle,
    state: State<'_, AppState>,
    slot_secret: Vec<u8>,
) -> Result<(), String> {
    if *state.chain_ready.lock().unwrap() {
        return Ok(());
    }
    unstation_chain::init_statement_store_from_secret(&slot_secret)?;
    *state.chain_ready.lock().unwrap() = true;
    log::info!("statement store initialized with paired identity");
    // Surface readiness (the subscription connects in the background) to the UI.
    let appc = app.clone();
    std::thread::spawn(move || {
        let ok = unstation_chain::wait_ready(Duration::from_secs(20));
        let _ = appc.emit(
            "mesh-status",
            MeshStatusMsg {
                state: if ok { "ready" } else { "connecting" }.into(),
                detail: if ok {
                    "Connected to the network.".into()
                } else {
                    "Still connecting to the network…".into()
                },
            },
        );
    });
    Ok(())
}

/// Bridge the QR-paired **Bulletin** allowance to the Rust signer, so durable-origin
/// writes (the signed manifest + init segment) are signed by — and sponsored through —
/// the phone-granted `//allowance//bulletin//<product>` slot account instead of the
/// SDK's unfunded Alice dev key. Independent of `set_chain_identity` and best-effort:
/// the live stream works without it; this only restores the cold-start / late-joiner
/// Bulletin anchor.
#[tauri::command]
fn set_bulletin_identity(slot_secret: Vec<u8>) -> Result<(), String> {
    unstation_chain::init_bulletin_from_secret(&slot_secret)?;
    log::info!("bulletin allowance signer installed");
    Ok(())
}

/// Live network-connection status for the Settings screen: `offline` before sign-in,
/// `connecting` while the statement-store subscription is coming up, `ready` once it's
/// connected. Non-blocking — reads the current subscription flag (unlike the one-shot
/// `mesh-status` event), so the Settings row reflects reality each time it's opened.
#[tauri::command]
fn chain_status(state: State<'_, AppState>) -> String {
    if !*state.chain_ready.lock().unwrap() {
        return "offline".into();
    }
    if unstation_chain::wait_ready(std::time::Duration::from_millis(0)) {
        "ready".into()
    } else {
        "connecting".into()
    }
}

/// Is a publish session running, and what are its details? Lets the UI rebuild the
/// Go-Live console on tab-back/relaunch without touching the running stream.
#[cfg(feature = "publish")]
#[tauri::command]
fn publish_status(state: State<'_, AppState>) -> Option<PublishStatus> {
    let g = state.publish.lock().unwrap();
    g.as_ref().map(|s| PublishStatus {
        info: s.info.clone(),
        name: s.name.clone(),
        live: s.live.load(Ordering::Relaxed),
        viewers: s.session.peer_count(),
    })
}

/// Is a watch session running, and what are its details? Lets the UI rebuild the
/// player on tab-back without restarting it.
#[tauri::command]
fn watch_status(state: State<'_, AppState>) -> Option<WatchStatus> {
    let g = state.watch.lock().unwrap();
    g.as_ref().map(|s| WatchStatus {
        info: s.info.clone(),
        peers: s.session.peer_count(),
    })
}

/// Go Live: start the local RTMP ingest (point OBS here), run a live publisher
/// node, announce the stream on the statement store, and serve a self-preview.
#[cfg(feature = "publish")]
#[tauri::command]
async fn start_publish(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<PublishInfo, String> {
    if !*state.chain_ready.lock().unwrap() {
        return Err("Sign in with the Polkadot app to go live — your stream is announced under your verified identity.".into());
    }
    if !segmenter::ffmpeg_available() {
        return Err("ffmpeg not found. Install it (e.g. `brew install ffmpeg`), or set \
                    UNSTATION_FFMPEG to its full path, then try again."
            .into());
    }
    let name = title.unwrap_or_else(|| "unstation".into());
    let canon = canonical_stream_name(&name);
    let stream = stream_id_from(&name);

    // Re-attach: if we're already publishing this exact stream, hand back its
    // existing details instead of tearing it down. This is what lets the UI reopen
    // the Go-Live console on tab-back / relaunch without interrupting the stream.
    if let Some(s) = state.publish.lock().unwrap().as_ref() {
        if s.name == canon {
            return Ok(s.info.clone());
        }
    }
    // A genuinely different stream (or a stale one): replace the prior session —
    // aborting the feeder also kills its ffmpeg ingest (Drop) so we don't fight over
    // the RTMP port.
    if let Some(prev) = state.publish.lock().unwrap().take() {
        prev.feeder.abort();
        prev.stats.abort();
        let _ = prev.pub_tx.send(EngineEvent::Stop);
    }
    let port = 21935u16;
    let key = "unstation";
    let url = segmenter::rtmp_url(port, key);

    // The ingest dir — wiped to a clean slate each session. The dir is reused across
    // streams, and stale fragments from a previous one belong to an unrelated encode
    // timeline: leaving them makes the player replay old video and then stall at the
    // discontinuity (the "counts up ~2 s then freezes, even after the encoder is gone"
    // bug). A clean dir also keeps the feeder's index-based segment sequence correct.
    let dir = std::env::temp_dir().join("unstation-publish");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);

    // Self-preview HLS + the publisher node inbox.
    let hls = HlsServer::start(1000).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let preview = hls.sink();
    let (pub_tx, pub_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC, then the live publisher node (its PeerId is
    // the statement-store account it announces under).
    let session = Session::start(stream, 1, stun(), pub_tx.clone())?;
    let publisher = MeshNode::new_live_publisher(
        session.my_peer,
        cfg(Mode::Live, Role::Publisher),
        SEG_BYTES,
        Arc::new(NullSink),
    )
    // Off-chain signaling (#17): sign each produced segment's live edge with our identity
    // and gossip it in-mesh, so viewers learn ids at mesh speed (chain edge = fallback).
    .with_stream_id(stream.0)
    .with_edge_signer(Arc::new(IdentityEdgeSigner))
    // Gossip the presence book so viewers discover the swarm in-mesh.
    .with_presence_book(session.presence_book());
    tokio::spawn(async move {
        let _ = publisher.run(pub_rx, TICK, None).await;
    });

    // Announce presence + republish the live-edge manifest as segments are made. The
    // publisher advertises relay-capability (relay = true): it's the origin/bridge, so
    // NAT-restricted viewers should prefer dialing it.
    session.spawn_presence(80_000_000, true);

    // M2 — publish the SIGNED MANIFEST to the Bulletin chain (the durable trust
    // anchor) and announce its CID in presence. Signed with this host's identity
    // (the same key as presence), so a viewer verifies it against our PeerId before
    // trusting the stream. Spawned, not awaited, so a slow/unavailable chain never
    // blocks going live; the presence loop picks up the CID once it's set.
    // The encoder's CMAF init segment (ftyp+moov), handed from the feeder (which loads it off
    // disk) to the manifest publisher (which puts it on Bulletin so viewers can initialize MSE
    // before any media fragment — see the viewer's init install in `start_watch`).
    let init_slot: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
    {
        let session_mc = session.clone();
        let init_slot = init_slot.clone();
        tokio::spawn(async move {
            let Some(publisher) = unstation_chain::identity_public() else {
                log::warn!("[publish] no chain identity — skipping signed-manifest publish");
                return;
            };
            // Wait for the init segment, then publish it to Bulletin and reference it in the
            // manifest (`init_segment_cid`). Viewers fetch + install it before any media
            // fragment; without it hls.js can't initialize MSE (fragLoadError on init.mp4).
            let init_bytes = loop {
                if let Some(b) = init_slot.lock().unwrap().clone() {
                    break b;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            let init_segment_cid = match BulletinOrigin.put_bytes(init_bytes.to_vec()).await {
                Ok(cid) => {
                    log::info!("[publish] init segment on Bulletin: {cid}");
                    cid
                }
                Err(e) => {
                    log::warn!("[publish] init put to Bulletin failed: {e:?}");
                    String::new()
                }
            };
            let created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let manifest = Manifest {
                stream_id: stream,
                kind: Kind::Live,
                // TODO(M2.1): derive codec / track dims from the CMAF init.
                codec: "avc1.640028,mp4a.40.2".into(),
                init_segment_cid,
                target_segment_ms: 2000,
                ll_mode: false,
                tracks: vec![Track { id: "v".into(), bitrate: 0, w: 0, h: 0 }],
                publisher,
                created_at,
            };
            let Some(sig) = unstation_chain::sign_with_identity(&manifest.signing_payload()) else {
                log::warn!("[publish] could not sign manifest");
                return;
            };
            match BulletinOrigin.put_manifest(SignedManifest { manifest, sig }).await {
                Ok(cid) => {
                    log::info!("[publish] signed manifest on Bulletin: {cid}");
                    session_mc.set_manifest_cid(cid);
                }
                Err(e) => log::warn!("[publish] manifest put to Bulletin failed: {e:?}"),
            }
        });
    }
    let (edge_tx, edge_rx) = unbounded_channel::<(Seq, SegmentId)>();
    session.spawn_edge_publisher(edge_rx);

    // Feeder: tail the ingest dir → preview sink + the publisher's mesh seed +
    // the live-edge manifest. Emits `publish-state` and keeps `live_flag` current so
    // a re-attaching UI can read the true live state via `publish_status`.
    let ptx = pub_tx.clone();
    let appc = app.clone();
    let init_slot_feeder = init_slot.clone();
    let live_flag = Arc::new(AtomicBool::new(false));
    let live_w = live_flag.clone();
    let feeder = tokio::spawn(async move {
        // Keep an ingest listener available AT ALL TIMES so the encoder can connect or
        // reconnect whenever — no ordering required. ffmpeg's RTMP `-listen` is one-shot,
        // so we respawn it (into a clean dir, with a reset preview) whenever it isn't up.
        // The UI's LIVE/idle state is driven by whether fresh fragments are actually
        // arriving (below), NOT by the ffmpeg process — so the indicator always matches
        // the video a viewer would see, and an encoder restart resumes on its own.
        let mut seg: Option<segmenter::SegmenterProcess> = None;
        let mut seen = 0u64;
        let mut init_sent = false;
        let mut live = false;
        let mut state_sent = false;
        let mut spawn_hinted = false;
        let mut last_fresh = std::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;

            // (Re)open the listener if it isn't running. Each new connection is a fresh
            // encode timeline, so clear the dir + preview and restart sequencing.
            let running = seg.as_mut().map(|s| s.running()).unwrap_or(false);
            if !running {
                let _ = std::fs::remove_dir_all(&dir);
                let _ = std::fs::create_dir_all(&dir);
                preview.reset();
                seen = 0;
                init_sent = false;
                seg = segmenter::spawn(&segmenter::Source::RtmpListen { url: &url }, &dir, 1).ok();
                if seg.is_none() && !spawn_hinted {
                    spawn_hinted = true;
                    let _ = appc.emit("publish-hint", PublishHintMsg {
                        message: "Couldn't start the local ingest (ffmpeg). Reinstall ffmpeg, then reopen the stream.".into(),
                    });
                }
            }

            // Consume only COMPLETE fragments — `load_live_segments_from` holds back the
            // newest `.m4s` (the one ffmpeg is still writing), which would otherwise be
            // a truncated, undecodable segment.
            if let Ok(news) = segmenter::load_live_segments_from(&dir, seen) {
                if !news.is_empty() {
                    if !init_sent {
                        if let Some(init) = segmenter::load_init(&dir) {
                            // Hand the init to the manifest publisher (→ Bulletin → viewers)
                            // and feed our own self-preview. (Bytes clone is cheap.)
                            *init_slot_feeder.lock().unwrap() = Some(init.clone());
                            preview.push_init(init);
                            init_sent = true;
                        }
                    }
                    if init_sent {
                        for s in news {
                            preview.push_segment(s.seq, s.bytes.clone());
                            let _ = ptx.send(EngineEvent::Produced { seq: s.seq, id: s.id, bytes: s.bytes });
                            let _ = edge_tx.send((s.seq, s.id));
                            seen = s.seq + 1;
                        }
                        last_fresh = std::time::Instant::now();
                    }
                }
            }

            // Live iff fresh fragments are arriving — accurate to the actual video.
            let now_live = seen > 0 && last_fresh.elapsed() < Duration::from_millis(4000);
            if now_live != live || !state_sent {
                live = now_live;
                state_sent = true;
                live_w.store(live, Ordering::Relaxed);
                let _ = appc.emit("publish-state", PublishStateMsg { live });
            }
        }
    });

    // Stream the live viewer count (peers connected over WebRTC) to the UI.
    let stats = {
        let s = session.clone();
        let appc = app.clone();
        tokio::spawn(async move {
            loop {
                let _ = appc.emit("publish-stats", PublishStatsMsg { viewers: s.peer_count() });
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    };

    let info = PublishInfo {
        ingest_server: format!("rtmp://127.0.0.1:{port}/live"),
        stream_key: key.into(),
        hls_url,
    };
    *state.publish.lock().unwrap() = Some(PublishSession {
        _hls: hls,
        feeder,
        stats,
        pub_tx,
        session,
        name: canon,
        info: info.clone(),
        live: live_flag,
    });

    Ok(info)
}

#[cfg(feature = "publish")]
#[tauri::command]
fn stop_publish(state: State<'_, AppState>) {
    if let Some(sess) = state.publish.lock().unwrap().take() {
        sess.feeder.abort(); // dropping the feeder kills the ffmpeg ingest (Drop)
        sess.stats.abort();
        let _ = sess.pub_tx.send(EngineEvent::Stop);
    }
}

/// Initialize stderr logging so chain/transport/SDK errors are visible (default: `info`;
/// override with `RUST_LOG`). Idempotent — safe to call once at each shell's startup.
pub fn init_logging() {
    // Quiet the statement-store subscribe/notification spam (per-poll "fetched N statements"),
    // and turn UP the mesh/signaling/transport internals we actually debug against (discovery,
    // dial, SDP/ICE, PeerConnected). RUST_LOG still overrides all of this when set.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        "info,sqlx=warn,jsonrpsee=warn,\
         useragent_chain::statement_store=warn,\
         unstation_session=debug,transport_libdc=debug,unstation_chain=debug,unstation_core=debug",
    ))
    .try_init();
    log::info!("Unstation starting");
}

/// The shared Tauri builder — managed [`AppState`] + the command handlers — used by both
/// the desktop and Android shells. Each shell supplies its own `tauri::generate_context!()`
/// (its own `tauri.conf.json`/capabilities) and calls `.run(..)`. The publish commands are
/// present only under the `publish` feature (desktop; the Android publish path is M4).
pub fn builder() -> tauri::Builder<tauri::Wry> {
    // Pin the runtime to Wry up front so `generate_handler!` can infer its `R`: assigning
    // the macro's output to a `let` first fails type inference (E0282) — it must be inlined
    // into `.invoke_handler()` on an already-typed builder.
    let b = tauri::Builder::<tauri::Wry>::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default());
    #[cfg(feature = "publish")]
    let b = b.invoke_handler(tauri::generate_handler![
        platform,
        signin_status,
        begin_signin,
        complete_signin,
        resolve_stream,
        set_chain_identity,
        set_bulletin_identity,
        chain_status,
        start_watch,
        stop_watch,
        watch_status,
        start_publish,
        stop_publish,
        publish_status
    ]);
    #[cfg(not(feature = "publish"))]
    let b = b.invoke_handler(tauri::generate_handler![
        platform,
        signin_status,
        begin_signin,
        complete_signin,
        resolve_stream,
        set_chain_identity,
        set_bulletin_identity,
        chain_status,
        start_watch,
        stop_watch,
        watch_status
    ]);
    b
}
