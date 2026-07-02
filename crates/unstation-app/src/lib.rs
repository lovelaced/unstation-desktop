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

// Android camera-publish (M4): encoded-AU intake from the Kotlin capture plugin over JNI.
#[cfg(all(target_os = "android", feature = "publish"))]
mod camera;
// The opt-in WebRTC media fast tier (W3): publisher-direct sub-second egress + its signaling.
mod fasttier;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::signaling::Presence;
use unstation_core::BoxFuture;
use unstation_core::manifest::{Kind, Manifest, OriginOfRecord, SignedManifest, Track};
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::types::{PeerId, SegmentId, Seq, StreamId};
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
    /// A background seed (seed-by-default): the converted remains of a watch whose
    /// player left — still caching + resharing the live window for the mesh.
    seed: Mutex<Option<SeedSession>>,
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
    /// Canonical stream name (for the lending-bandwidth status once converted).
    name: String,
    /// Live node-stats receiver, retained so a seed conversion can keep observing.
    stats: tokio::sync::watch::Receiver<unstation_core::node::NodeStats>,
    /// Contribution level from the health monitor: 0=full, 1=reduced, 2=paused.
    /// `stop_watch` only converts to a background seed at level 0 — an unstable
    /// link makes a bad helper.
    health: Arc<std::sync::atomic::AtomicU8>,
    /// The publisher's routing `PeerId`, captured once a candidate's manifest verifies. The
    /// opt-in fast tier addresses its WebRTC media offer to this peer's fast-signaling topic.
    publisher_peer: Arc<Mutex<Option<PeerId>>>,
    /// The viewer-side fast-tier answer-reader task, while a fast-tier attempt is in flight.
    fast_task: Mutex<Option<JoinHandle<()>>>,
}

/// A watch converted into a background seed: same node/session, Role::Seed, the
/// cursor following the live edge, plus a status task that reports the contribution
/// and stops the whole thing when the stream ends.
struct SeedSession {
    _hls: HlsServer,
    node_tx: UnboundedSender<EngineEvent>,
    session: Session,
    tasks: Vec<JoinHandle<()>>,
}

/// An active publish: RTMP ingest, the self-preview HLS, the feeder task, the
/// publisher node's inbox, and the session.
#[cfg(feature = "publish")]
struct PublishSession {
    _hls: HlsServer,
    /// Every background task this publish spawned — feeder (owns the ffmpeg ingest
    /// listener; aborting it kills ffmpeg via `Drop`), stats, presence refresh, edge
    /// publisher, manifest publish. Torn down together: a survivor (the presence loop
    /// especially) would keep announcing this dead stream to the chain forever.
    tasks: Vec<JoinHandle<()>>,
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
    /// Encoder → ingest bitrate over the last window (what the encoder is producing).
    ingest_kbps: u32,
    /// Mesh uplink over the last window (what this machine is serving to viewers).
    uplink_kbps: u32,
}

/// Go-Live preflight progress (spec §10.4: `identity ✓ · announced ✓ · encoder ✓`).
/// `ok=false` with a detail is a *quiet degradation*, not an error (e.g. the durable
/// backup copy still pending while the stream is already live).
#[derive(Serialize, Clone)]
struct PublishProgressMsg {
    step: String,
    ok: bool,
    detail: String,
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

/// Lending-bandwidth status (seed-by-default, health-gated). Emitted by the watch
/// health monitor (`seeding: false` — contribution while watching) and by a
/// background seed's status task (`seeding: true`). `level` ∈ {"full","reduced",
/// "paused"}; "paused" means the link looked unstable/slow and contribution is off.
#[derive(Serialize, Clone)]
struct SeedStatsMsg {
    seeding: bool,
    stream: String,
    level: String,
    uplink_kbps: u32,
    peers: usize,
}

/// The viewer journey's honest state machine (IMPLEMENTATION_SPEC §10.3), emitted as
/// real conditions change — the finding scene, catch-up ladder, ended screen, and
/// unreachable state all key off this instead of hardcoded timers. Phases:
/// `resolving | verifying | discovering | connecting | buffering | live |
/// catching-up | ended | unreachable`.
#[derive(Serialize, Clone)]
struct WatchPhaseMsg {
    phase: String,
    detail: String,
    /// Milliseconds since this watch started (for the UI's time-to-first-frame).
    since_ms: u64,
}

#[derive(Serialize, Clone)]
struct PublishInfo {
    ingest_server: String,
    stream_key: String,
    hls_url: String,
    /// "rtmp" | "whip" | "camera" — lets the Go-Live UI show the right ingest
    /// instructions (RTMP server+key, a WHIP URL, or the phone camera).
    ingest_mode: String,
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
/// cross-subnet/NAT pairs find a route too. Overridable via `UNSTATION_STUN`
/// (comma-separated URIs; set it empty for host-candidate-only, e.g. an offline/
/// air-gapped LAN where reaching a public STUN would only add delay).
///
/// TURN escape hatch: volunteer mesh relays are the primary NAT fallback (no
/// operator infrastructure), but symmetric-NAT pairs with no reachable volunteer
/// can set `UNSTATION_TURN` to operator-provided relays — comma-separated
/// `turn:user:pass@host:port[?transport=udp]` URIs, passed straight to
/// libdatachannel (which parses embedded credentials). Off by default.
fn stun() -> Vec<String> {
    let mut servers: Vec<String> = match std::env::var("UNSTATION_STUN") {
        Ok(v) => v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect(),
        Err(_) => vec!["stun:stun.l.google.com:19302".into()],
    };
    if let Ok(turn) = std::env::var("UNSTATION_TURN") {
        servers.extend(
            turn.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from),
        );
    }
    servers
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

    // Re-watch / publisher switch: fully tear down any previous watch FIRST. Otherwise its
    // tasks keep running and its transport stays connected to the publisher under our
    // (stable) peer id, so the publisher ignores the new connection and the re-watch hangs.
    // A background seed also yields — the thing you're actually watching wins bandwidth.
    teardown_watch(&state);
    teardown_seed(&state);

    // The honest viewer state machine starts NOW (name→id is instant, but the phase
    // still renders so the journey is legible).
    let watch_started = std::time::Instant::now();
    let emit_phase = {
        let app = app.clone();
        move |phase: &str, detail: &str, since: &std::time::Instant| {
            let _ = app.emit(
                "watch-phase",
                WatchPhaseMsg {
                    phase: phase.into(),
                    detail: detail.into(),
                    since_ms: since.elapsed().as_millis() as u64,
                },
            );
        }
    };
    emit_phase("resolving", "", &watch_started);

    // Localhost HLS re-server → the webview <video> plays from here.
    // Start in standard mode; if the verified manifest advertises `ll_mode`, the candidate
    // filter flips the re-server to low-latency (before any media fragment is delivered).
    let hls = HlsServer::start(1000).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let hls_sink = hls.sink();
    let sink: Arc<dyn MediaSink> = Arc::new(hls_sink.clone());
    // A second handle to the SAME local HLS server so the dial loop can install the CMAF
    // init segment it fetches from Bulletin, alongside the media segments the node feeds.
    let sink_for_init = sink.clone();

    // Viewer node inbox; the transport posts PeerConnected/Inbound here.
    let (view_tx, view_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC for this stream.
    let session = Session::start(stream, 1, stun(), view_tx.clone())?;

    // Live stats feed: the node publishes real numbers (delivered, live-edge lag)
    // once a second; the UI stats task below reads the latest.
    let (stats_tx, stats_rx) = tokio::sync::watch::channel(unstation_core::node::NodeStats::default());

    // The durable floor (TECH_SPEC §8.6): when no peer can meet a deadline, look the
    // segment up in the publisher's durable map (seq → Bulletin CID, cached; refreshed
    // at most every 2s on a miss) and fetch it from Bulletin. The node re-verifies
    // whatever comes back against the authenticated content id.
    let fallback: unstation_core::node::FallbackFetch = {
        let s = session.clone();
        let map: Arc<Mutex<HashMap<Seq, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let last_refresh: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        Arc::new(move |seq, _id| {
            let s = s.clone();
            let map = map.clone();
            let last_refresh = last_refresh.clone();
            Box::pin(async move {
                let cached =
                    map.lock().unwrap_or_else(|e| e.into_inner()).get(&seq).cloned();
                let cid = match cached {
                    Some(c) => c,
                    None => {
                        // Refresh the map at most every 2s across concurrent fetches.
                        let due = {
                            let mut g = last_refresh.lock().unwrap_or_else(|e| e.into_inner());
                            let due = g.map_or(true, |t| t.elapsed() >= Duration::from_secs(2));
                            if due {
                                *g = Some(std::time::Instant::now());
                            }
                            due
                        };
                        if !due {
                            return None;
                        }
                        let entries = s.read_durable().await.ok()?;
                        let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
                        for (sq, c) in entries {
                            g.insert(sq, c);
                        }
                        g.get(&seq).cloned()?
                    }
                };
                BulletinOrigin.fetch_bytes(&cid).await.ok()
            })
        })
    };

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
    .with_presence_book(session.presence_book())
    // Convictions (forged bytes, floods) bar re-dials + offers at the session edge.
    .with_ban_list(session.ban_list())
    .with_fallback(fallback)
    .with_stats(stats_tx);
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(async move {
        let _ = viewer.run(view_rx, TICK, None).await;
    }));

    // Learn the live edge (segment ids) from the publisher. Tracked in `tasks` so teardown
    // aborts it (otherwise it keeps polling the chain for this stale session forever).
    tasks.push(session.spawn_edge_poller(view_tx.clone()));

    // Announce ourselves so other viewers can discover + reshare from us — the mesh
    // relays through volunteer peers, so a NAT-restricted node only needs to reach
    // *someone*. relay_opt_in = false, but a viewer that proves reachable (a peer
    // connects to it inbound) auto-promotes to advertising relay-capability — emergent,
    // self-organizing volunteer relays, gated by the health monitor below (an unstable
    // link makes a bad relay). Tracked in `tasks` so teardown aborts it — else this
    // session keeps refreshing its (now-stale) presence forever, and after a
    // fresh-peer-id re-watch those old entries pile up and get re-dialed.
    let relay_gate = Arc::new(AtomicBool::new(true));
    tasks.push(session.spawn_presence(20_000_000, false, relay_gate.clone()));

    // Health-gated contribution (seed-by-default): everyone lends bandwidth unless the
    // link proves unstable or slow. Signals, sampled every 5s from the node's real
    // stats: our own live-edge lag growing while the uplink is busy (serving is
    // hurting playback), or deliveries stalling with peers connected (a struggling
    // link). Each strike steps contribution down (full 20 Mbps → reduced 4 Mbps →
    // paused) and drops the relay advertisement; 30s of clean samples steps back up.
    let health = Arc::new(std::sync::atomic::AtomicU8::new(0));
    {
        const BUDGETS: [u64; 3] = [20_000_000, 4_000_000, 0];
        let s = session.clone();
        let vtx = view_tx.clone();
        let appc = app.clone();
        let mut rx = stats_rx.clone();
        let relay_gate = relay_gate.clone();
        let health = health.clone();
        let stream_name = canonical_stream_name(&target);
        tasks.push(tokio::spawn(async move {
            let mut last = rx.borrow().clone();
            let mut healthy_streak = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let ns = rx.borrow_and_update().clone();
                let uplink_kbps =
                    (ns.sent_bytes.saturating_sub(last.sent_bytes) * 8 / 5 / 1000) as u32;
                let contended = ns.latency_s > 10.0 && uplink_kbps > 100;
                let stalled = ns.delivered == last.delivered
                    && ns.head_seq > ns.play_seq
                    && s.peer_count() > 0;
                last = ns;
                let cur = health.load(Ordering::Relaxed);
                let next = if contended || stalled {
                    healthy_streak = 0;
                    (cur + 1).min(2)
                } else {
                    healthy_streak += 1;
                    if healthy_streak >= 6 && cur > 0 {
                        healthy_streak = 0;
                        cur - 1
                    } else {
                        cur
                    }
                };
                if next != cur {
                    health.store(next, Ordering::Relaxed);
                    relay_gate.store(next == 0, Ordering::Relaxed);
                    let _ = vtx.send(EngineEvent::SetUploadBudget(BUDGETS[next as usize]));
                    log::info!(
                        "[lend] contribution → {} ({})",
                        ["full", "reduced", "paused"][next as usize],
                        if contended { "uplink contention" } else if stalled { "delivery stalled" } else { "link recovered" }
                    );
                }
                let _ = appc.emit(
                    "seed-stats",
                    SeedStatsMsg {
                        seeding: false,
                        stream: stream_name.clone(),
                        level: ["full", "reduced", "paused"][next as usize].into(),
                        uplink_kbps,
                        peers: s.peer_count(),
                    },
                );
            }
        }));
    }

    // Connection upkeep now lives in the session's maintainer (dial pacing with
    // exponential backoff, hung-dial abandonment, instant reaction to drops — no
    // fixed reconnect lag). What stays here is the app's TRUST GATE, run for every
    // candidate before it's dialed:
    //
    // M2: only the publisher announces a signed-manifest CID; verify it against its
    // PERSONHOOD key (`publisher`, stable across the publisher's devices) and skip
    // impostors — `peer_id` is only a per-device routing address, so it can't be the
    // trust anchor. Resharing viewers carry no manifest; their segments are still
    // hash-verified against the publisher-authenticated live edge. On first verify,
    // install the CMAF init segment (ftyp+moov) into the local HLS server so it can
    // serve /init.mp4 — hls.js needs it before ANY media fragment (EXT-X-MAP).
    let publisher_peer: Arc<Mutex<Option<PeerId>>> = Arc::new(Mutex::new(None));
    {
        let vtx = view_tx.clone();
        let appc = app.clone();
        let sink_init = sink_for_init.clone();
        let sink_cfg = hls_sink.clone();
        let init_installed = Arc::new(AtomicBool::new(false));
        let phase = emit_phase.clone();
        let started = watch_started;
        let pub_peer = publisher_peer.clone();
        let filter: Arc<dyn Fn(Presence) -> BoxFuture<'static, bool> + Send + Sync> =
            Arc::new(move |cand: Presence| {
                let vtx = vtx.clone();
                let appc = appc.clone();
                let sink_init = sink_init.clone();
                let sink_cfg = sink_cfg.clone();
                let init_installed = init_installed.clone();
                let phase = phase.clone();
                let pub_peer = pub_peer.clone();
                Box::pin(async move {
                    let Some(cid) = cand.manifest_cid.clone() else {
                        return true; // a resharing viewer — no manifest to check
                    };
                    phase("verifying", "", &started);
                    match BulletinOrigin.fetch_manifest(cid).await {
                        Ok(m) if m.verify(&cand.publisher).is_ok() => {
                            // Verified publisher → its personhood key is the trust
                            // anchor for gossiped live-edge announcements (#17).
                            let _ = vtx.send(EngineEvent::SetPublisherKey { key: cand.publisher });
                            // Its routing peer is where the opt-in fast tier sends its offer.
                            *pub_peer.lock().unwrap_or_else(|e| e.into_inner()) = Some(cand.peer_id);
                            // Match the local re-server to the publisher's tier: LL-HLS parts
                            // if advertised (`target_segment_ms` = part duration), else standard.
                            // Idempotent + runs before the first fragment (init install follows).
                            sink_cfg.configure(
                                m.manifest.ll_mode,
                                m.manifest.target_segment_ms,
                                if m.manifest.ll_mode { 0 } else { m.manifest.target_segment_ms },
                            );
                            if !init_installed.load(Ordering::Relaxed)
                                && !m.manifest.init_segment_cid.is_empty()
                            {
                                match BulletinOrigin.fetch_bytes(&m.manifest.init_segment_cid).await {
                                    Ok(b) => {
                                        log::info!("[watch] init segment installed ({} B)", b.len());
                                        sink_init.push_init(b);
                                        init_installed.store(true, Ordering::Relaxed);
                                    }
                                    Err(e) => log::warn!("[watch] init fetch failed: {e:?}"),
                                }
                            }
                            phase("connecting", "", &started);
                            true
                        }
                        Ok(_) => {
                            let _ = appc.emit(
                                "mesh-status",
                                MeshStatusMsg { state: "error".into(), detail: "Couldn’t verify a broadcaster — skipping.".into() },
                            );
                            false
                        }
                        Err(e) => {
                            log::warn!("[watch] manifest fetch failed ({e:?}); proceeding (segments still hash-verified)");
                            phase("connecting", "", &started);
                            true
                        }
                    }
                })
            });
        tasks.push(session.spawn_maintainer(3, filter));
    }

    // The phase watcher: derives the honest viewer state from REAL conditions — peer
    // count, live-edge freshness, delivered segments, play head — and emits
    // `watch-phase` on every transition. This is what makes "finding" real instead of
    // a hardcoded animation, gives "ended" an actual trigger, and keeps "unreachable"
    // honest (the maintainer keeps retrying underneath; the UI stops pretending).
    {
        let s = session.clone();
        let mut rx = stats_rx.clone();
        let phase = emit_phase.clone();
        let started = watch_started;
        tasks.push(tokio::spawn(async move {
            let mut cur = String::from("resolving");
            let mut was_live = false;
            let mut last_head: u64 = 0;
            let mut last_head_change = std::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let ns = rx.borrow_and_update().clone();
                let peers = s.peer_count();
                if ns.head_seq > last_head {
                    last_head = ns.head_seq;
                    last_head_change = std::time::Instant::now();
                }
                let head_stale = last_head_change.elapsed();
                let playing = ns.play_seq > 0;
                let next = if was_live && head_stale > Duration::from_secs(20) {
                    // Nothing new produced for 20s: the broadcast is over (or so badly
                    // stalled the difference doesn't matter to the viewer).
                    "ended"
                } else if was_live && head_stale > Duration::from_secs(6) {
                    "catching-up"
                } else if playing {
                    was_live = true;
                    "live"
                } else if ns.delivered > 0 {
                    "buffering"
                } else if peers > 0 {
                    "connecting"
                } else if !was_live && started.elapsed() > Duration::from_secs(30) {
                    // 30s with zero connections: stop pretending. The maintainer keeps
                    // retrying underneath; if a peer appears this moves forward again.
                    "unreachable"
                } else {
                    "discovering"
                };
                if next != cur {
                    cur = next.to_string();
                    phase(next, "", &started);
                    if next == "ended" {
                        return; // terminal for this watch; a rejoin starts a new one
                    }
                }
            }
        }));
    }

    // A second stats receiver, retained on the session struct so a seed conversion
    // (stop_watch) can keep observing the node after the UI stats task below owns
    // the original.
    let watch_stats = stats_rx.clone();

    // Stream REAL mesh stats to the webview: live transport peer count + the node's
    // own numbers (delivered segments, live-edge lag) from its watch channel. Also
    // keeps the "reaching the mesh" status honest while no peer is connected.
    {
        let s = session.clone();
        let appc = app.clone();
        tasks.push(tokio::spawn(async move {
            let mut last_from_origin = 0usize;
            loop {
                let peers = s.peer_count();
                if peers == 0 {
                    let _ = appc.emit(
                        "mesh-status",
                        MeshStatusMsg { state: "connecting".into(), detail: "Reaching the mesh…".into() },
                    );
                }
                let ns = stats_rx.borrow().clone();
                // "Leaning on the backup copy" is the honest amber state: the durable
                // floor served segments since the last sample.
                let leaning = ns.from_origin > last_from_origin;
                last_from_origin = ns.from_origin;
                let rho = if ns.delivered > 0 {
                    (100 * ns.delivered.saturating_sub(ns.from_origin) / ns.delivered) as u32
                } else {
                    0
                };
                let _ = appc.emit(
                    "mesh-stats",
                    MeshStatsMsg {
                        peers,
                        rho,
                        from_seed: 0,
                        from_chain: ns.from_origin as u32,
                        latency_s: ns.latency_s,
                        ice: if peers > 0 { "direct".into() } else { "connecting".into() },
                        mode: if leaning { "seed".into() } else { "p2p".into() },
                        delivered: ns.delivered,
                    },
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }));
    }

    let info = WatchInfo {
        hls_url,
        stream_id: resolve_stream(target.clone()),
        publisher: target.clone(),
        peers: 0,
        rho: 0,
    };
    *state.watch.lock().unwrap() = Some(WatchSession {
        _hls: hls,
        node_tx: view_tx,
        session,
        tasks,
        info: info.clone(),
        name: canonical_stream_name(&target),
        stats: watch_stats,
        health,
        publisher_peer,
        fast_task: Mutex::new(None),
    });

    Ok(info)
}

/// Fully tear down the current watch (if any): stop the node, abort its tasks, and
/// actively close its WebRTC connections. The transport reactor is kept alive by detached
/// signaling tasks, so dropping the `WatchSession` alone never closes the connections —
/// `session.shutdown()` does. Without it, a re-watch or publisher switch hangs because the
/// publisher still holds our old connection (same peer id) and ignores the new offer.
fn teardown_watch(state: &AppState) {
    if let Some(sess) = state.watch.lock().unwrap().take() {
        let _ = sess.node_tx.send(EngineEvent::Stop);
        if let Some(t) = sess.fast_task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort(); // stop any in-flight fast-tier answer reader
        }
        for t in sess.tasks {
            t.abort();
        }
        sess.session.shutdown();
        // `_hls` drops here.
    }
}

/// Stop a background seed (if any): abort its tasks, stop the node, close connections.
fn teardown_seed(state: &AppState) {
    if let Some(sess) = state.seed.lock().unwrap().take() {
        let _ = sess.node_tx.send(EngineEvent::Stop);
        for t in sess.tasks {
            t.abort();
        }
        sess.session.shutdown();
    }
}

/// Seed-by-default: lending bandwidth is on unless explicitly disabled.
fn seed_by_default() -> bool {
    std::env::var("UNSTATION_SEED").map(|v| v != "0").unwrap_or(true)
}

/// Background-seed contribution ceiling (bits/sec) once the player is gone.
const SEED_BUDGET_BPS: u64 = 10_000_000;

/// The background seed's status heartbeat: report the contribution every 5s and stop
/// the whole seed when the stream ends (live edge stale >30s — a dead stream needs
/// no helpers) so it never lingers as a zombie.
fn spawn_seed_status(
    app: AppHandle,
    session: Session,
    node_tx: UnboundedSender<EngineEvent>,
    mut stats: tokio::sync::watch::Receiver<unstation_core::node::NodeStats>,
    name: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = stats.borrow().clone();
        let mut last_head_change = std::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let ns = stats.borrow_and_update().clone();
            if ns.head_seq > last.head_seq {
                last_head_change = std::time::Instant::now();
            }
            let uplink_kbps =
                (ns.sent_bytes.saturating_sub(last.sent_bytes) * 8 / 5 / 1000) as u32;
            last = ns;
            if last_head_change.elapsed() > Duration::from_secs(30) {
                log::info!("[lend] stream over — stopping the background seed for {name}");
                let _ = node_tx.send(EngineEvent::Stop);
                session.shutdown();
                let _ = app.emit(
                    "seed-stats",
                    SeedStatsMsg {
                        seeding: false,
                        stream: name.clone(),
                        level: "off".into(),
                        uplink_kbps: 0,
                        peers: 0,
                    },
                );
                return; // the inert SeedSession husk is cleared on the next teardown_seed
            }
            let _ = app.emit(
                "seed-stats",
                SeedStatsMsg {
                    seeding: true,
                    stream: name.clone(),
                    level: "full".into(),
                    uplink_kbps,
                    peers: session.peer_count(),
                },
            );
        }
    })
}

#[tauri::command]
fn stop_watch(app: AppHandle, state: State<'_, AppState>) {
    // Seed-by-default: leaving a stream converts the session into a background seed
    // (same node + connections, Role::Seed, cursor pinned to the live edge) so the
    // mesh keeps a helper — but only on a link the health monitor rates FULL. An
    // unstable or slow connection tears down like before; so does opting out
    // (UNSTATION_SEED=0).
    let Some(sess) = state.watch.lock().unwrap().take() else { return };
    let healthy = sess.health.load(Ordering::Relaxed) == 0;
    if !(seed_by_default() && healthy) {
        let _ = sess.node_tx.send(EngineEvent::Stop);
        for t in sess.tasks {
            t.abort();
        }
        sess.session.shutdown();
        return;
    }
    let _ = sess.node_tx.send(EngineEvent::SetRole(Role::Seed));
    let _ = sess.node_tx.send(EngineEvent::SetUploadBudget(SEED_BUDGET_BPS));
    log::info!("[lend] watch of {} converted to a background seed", sess.name);
    let mut tasks = sess.tasks;
    tasks.push(spawn_seed_status(
        app,
        sess.session.clone(),
        sess.node_tx.clone(),
        sess.stats.clone(),
        sess.name.clone(),
    ));
    let seed = SeedSession {
        _hls: sess._hls,
        node_tx: sess.node_tx,
        session: sess.session,
        tasks,
    };
    if let Some(prev) = state.seed.lock().unwrap().replace(seed) {
        // Only one background seed at a time — retire the previous one.
        let _ = prev.node_tx.send(EngineEvent::Stop);
        for t in prev.tasks {
            t.abort();
        }
        prev.session.shutdown();
    }
}

/// Stop lending bandwidth (the Settings control).
#[tauri::command]
fn stop_seed(state: State<'_, AppState>) {
    teardown_seed(&state);
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

/// Fast tier (opt-in, unverified, sub-second): the webview built a recvonly `RTCPeerConnection`
/// and gathered `offer_sdp`. Relay it to the publisher over the fast-tier signaling topic and
/// pump the answer back to the webview (`fast-answer`), or `fast-closed` if the publisher
/// declines / isn't reachable — in which case the webview stays on the verified mesh player.
/// Requires an active watch whose publisher has been resolved.
#[tauri::command]
async fn fast_watch_start(
    app: AppHandle,
    state: State<'_, AppState>,
    offer_sdp: String,
) -> Result<(), String> {
    let (signaling, my_peer, publisher) = {
        let g = state.watch.lock().unwrap();
        let sess = g.as_ref().ok_or("not watching")?;
        let publisher = (*sess.publisher_peer.lock().unwrap_or_else(|e| e.into_inner()))
            .ok_or("publisher not resolved yet — try again in a moment")?;
        (sess.session.signaling(), sess.session.my_peer, publisher)
    };
    let handle = fasttier::spawn_answer_reader(app, signaling, my_peer, publisher, offer_sdp);
    // Store the reader on the session, replacing any prior attempt.
    let g = state.watch.lock().unwrap();
    match g.as_ref() {
        Some(sess) => {
            if let Some(old) =
                sess.fast_task.lock().unwrap_or_else(|e| e.into_inner()).replace(handle)
            {
                old.abort();
            }
        }
        None => handle.abort(), // watch ended while we were setting up
    }
    Ok(())
}

/// Leave the fast tier: stop the answer reader and tell the publisher to free the slot. The
/// webview falls back to the verified mesh player (which stayed warm underneath).
#[tauri::command]
async fn fast_watch_stop(state: State<'_, AppState>) -> Result<(), String> {
    let close = {
        let g = state.watch.lock().unwrap();
        match g.as_ref() {
            Some(sess) => {
                if let Some(t) = sess.fast_task.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    t.abort();
                }
                (*sess.publisher_peer.lock().unwrap_or_else(|e| e.into_inner()))
                    .map(|p| (sess.session.signaling(), sess.session.my_peer, p))
            }
            None => None,
        }
    };
    if let Some((sig, me, publisher)) = close {
        fasttier::send_fast_close(&sig, me, publisher).await;
    }
    Ok(())
}

/// Go Live: start the local RTMP ingest (point OBS here), run a live publisher
/// node, announce the stream on the statement store, and serve a self-preview.
/// Publish the signed manifest to Bulletin + announce its CID. Waits for the encoder's CMAF
/// init segment (filled into `init_slot` by the feeder), puts it on Bulletin, references it in
/// the manifest's `init_segment_cid`, signs, and publishes — spawned so a slow chain never
/// blocks going live. Shared by the desktop (ffmpeg) and Android (camera) publish paths.
/// Sparse durable-floor uploads (TECH_SPEC §8.6): every Nth produced segment goes to
/// Bulletin within a byte budget, and its `(seq → CID)` entry is announced on the
/// durable topic so a viewer whose deadline no peer can meet can fetch it there.
/// Sparse is the point — the metered allowance is scarce. Tunables:
/// `UNSTATION_DURABLE_EVERY` (default 5; 0 disables) and
/// `UNSTATION_DURABLE_BUDGET_MB` (default 50, per stream).
#[cfg(feature = "publish")]
fn spawn_durable_uploader(
    session: &Session,
    mut seg_rx: tokio::sync::mpsc::UnboundedReceiver<(Seq, Bytes)>,
) -> Vec<JoinHandle<()>> {
    let every: u64 = std::env::var("UNSTATION_DURABLE_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let budget: u64 = std::env::var("UNSTATION_DURABLE_BUDGET_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        * 1024
        * 1024;
    let (cid_tx, cid_rx) = unbounded_channel::<(Seq, String)>();
    let map_task = session.spawn_durable_publisher(cid_rx);
    let up_task = tokio::spawn(async move {
        if every == 0 {
            return;
        }
        let mut spent: u64 = 0;
        while let Some((seq, bytes)) = seg_rx.recv().await {
            if seq % every != 0 {
                continue;
            }
            if spent + bytes.len() as u64 > budget {
                log::info!("[durable] budget exhausted ({spent} B) — no further uploads this stream");
                break;
            }
            match BulletinOrigin.put_bytes(bytes.to_vec()).await {
                Ok(cid) => {
                    spent += bytes.len() as u64;
                    log::info!("[durable] seq={seq} → Bulletin {cid} ({spent} B of budget used)");
                    let _ = cid_tx.send((seq, cid));
                }
                Err(e) => log::warn!("[durable] seq={seq} upload failed: {e:?}"),
            }
        }
    });
    vec![map_task, up_task]
}

/// Publisher dashboard numbers, every 2s: live viewer count plus ingest/uplink
/// bitrates from windowed byte deltas (feeder meter / the node's sent_bytes stat).
#[cfg(feature = "publish")]
fn spawn_publish_stats(
    app: AppHandle,
    session: Session,
    ingest_bytes: Arc<AtomicU64>,
    stats_rx: tokio::sync::watch::Receiver<unstation_core::node::NodeStats>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_ingest = 0u64;
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let ing = ingest_bytes.load(Ordering::Relaxed);
            let sent = stats_rx.borrow().sent_bytes;
            let ingest_kbps = (ing.saturating_sub(last_ingest) * 8 / 2 / 1000) as u32;
            let uplink_kbps = (sent.saturating_sub(last_sent) * 8 / 2 / 1000) as u32;
            last_ingest = ing;
            last_sent = sent;
            let _ = app.emit(
                "publish-stats",
                PublishStatsMsg { viewers: session.peer_count(), ingest_kbps, uplink_kbps },
            );
        }
    })
}

/// CMAF part duration for the low-latency (LL-HLS) tier — the in-process muxer emits a part
/// this often, and it's the `target_segment_ms` we advertise in the manifest so viewers run
/// their local HLS re-server in LL mode too. ~250ms trades a little mesh overhead (more,
/// smaller addressable units) for the ~1.5s glass-to-glass the LL path targets.
#[cfg(feature = "publish")]
const LL_PART_MS: u32 = 250;

/// Max concurrent opt-in fast-tier viewers a publisher serves directly (uplink-bound: the
/// publisher sends full RTP to each). Beyond this, a viewer's fast-tier offer is declined and
/// it stays on the verified mesh — which scales to the crowd. A handful of friends.
#[cfg(all(feature = "publish", not(target_os = "android")))]
const FAST_TIER_CAP: usize = 5;

#[cfg(feature = "publish")]
fn spawn_manifest_publish(
    app: AppHandle,
    session: &Session,
    stream: StreamId,
    init_slot: Arc<Mutex<Option<Bytes>>>,
    // `Some(part_ms)` → advertise low-latency (LL-HLS) so viewers serve parts; `None` → standard.
    ll_part_ms: Option<u32>,
) -> JoinHandle<()> {
    let session_mc = session.clone();
    tokio::spawn(async move {
        let progress = |ok: bool, detail: &str| {
            let _ = app.emit(
                "publish-progress",
                PublishProgressMsg { step: "announced".into(), ok, detail: detail.into() },
            );
        };
        let Some(publisher) = unstation_chain::identity_public() else {
            log::warn!("[publish] no chain identity — skipping signed-manifest publish");
            progress(false, "No signed-in identity — the stream isn’t announced.");
            return;
        };
        let init_bytes = loop {
            if let Some(b) = init_slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
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
                // Degrade quietly, never into an error: the live mesh works without
                // the durable copy; only the cold-start/late-joiner anchor is pending.
                progress(false, "Live now — backup copy pending.");
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
            // LL: advertise the part duration so viewers run their re-server in LL mode.
            // Standard: the ~2s segment cadence ffmpeg/RTMP produces.
            target_segment_ms: ll_part_ms.unwrap_or(2000),
            ll_mode: ll_part_ms.is_some(),
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
                progress(true, "");
            }
            Err(e) => {
                log::warn!("[publish] manifest put to Bulletin failed: {e:?}");
                progress(false, "Live now — backup copy pending.");
            }
        }
    })
}

/// Desktop publish: ffmpeg listens for an RTMP ingest (OBS) and segments it into CMAF.
#[cfg(all(feature = "publish", not(target_os = "android")))]
#[tauri::command]
async fn start_publish(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
    ingest_mode: Option<String>,
) -> Result<PublishInfo, String> {
    if !*state.chain_ready.lock().unwrap() {
        return Err("Sign in with the Polkadot app to go live — your stream is announced under your verified identity.".into());
    }
    // WHIP ingest (RFC 9725, sub-second) takes WebRTC media straight from OBS 30+ and
    // needs no ffmpeg; RTMP does. Default to RTMP.
    let whip = ingest_mode.as_deref() == Some("whip");
    if !whip && !segmenter::ffmpeg_available() {
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
        teardown_publish(prev);
    }
    // Broadcasting owns the uplink — retire any background seed.
    teardown_seed(&state);

    // Self-preview HLS + the publisher node inbox. WHIP ingest muxes in-process, so it emits
    // CMAF parts → run the re-server in low-latency mode. RTMP goes through ffmpeg (whole
    // segments on disk) → standard mode.
    let hls = if whip {
        HlsServer::start_ll(1000, LL_PART_MS)
    } else {
        HlsServer::start(1000)
    }
    .map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let preview = hls.sink();
    let (pub_tx, pub_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC, then the live publisher node (its PeerId is
    // the statement-store account it announces under). The origin serves many more
    // inbound viewers than a plain viewer's default admission cap allows.
    let session = Session::start(stream, 1, stun(), pub_tx.clone())?;
    session.set_max_inbound(128);
    // Preflight (spec §10.4): identity is already proven (chain_ready gated above).
    let _ = app.emit(
        "publish-progress",
        PublishProgressMsg { step: "identity".into(), ok: true, detail: String::new() },
    );
    // Uplink metering: the node reports sent_bytes on its stats channel.
    let (pub_stats_tx, pub_stats_rx) =
        tokio::sync::watch::channel(unstation_core::node::NodeStats::default());
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
    .with_presence_book(session.presence_book())
    // Convictions (forged bytes, floods) bar re-dials + offers at the session edge.
    .with_ban_list(session.ban_list())
    .with_stats(pub_stats_tx);
    tokio::spawn(async move {
        let _ = publisher.run(pub_rx, TICK, None).await;
    });

    // Announce presence + republish the live-edge manifest as segments are made. The
    // publisher advertises relay-capability (relay = true): it's the origin/bridge, so
    // NAT-restricted viewers should prefer dialing it. Every spawned task's handle goes
    // into `tasks` so teardown really ends the stream (a surviving presence loop would
    // keep announcing it to the chain forever).
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    tasks.push(session.spawn_presence(80_000_000, true, Arc::new(AtomicBool::new(true))));

    // M2 — publish the signed manifest to Bulletin + announce its CID in presence (the durable
    // trust anchor). The feeder fills `init_slot` with the encoder's CMAF init; the shared
    // publisher waits for it, puts it on Bulletin, and references it in the signed manifest so
    // viewers can initialize MSE before any media fragment.
    let init_slot: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
    tasks.push(spawn_manifest_publish(
        app.clone(),
        &session,
        stream,
        init_slot.clone(),
        whip.then_some(LL_PART_MS),
    ));
    let (edge_tx, edge_rx) = unbounded_channel::<(Seq, SegmentId)>();
    tasks.push(session.spawn_edge_publisher(edge_rx));

    // Encoder→ingest byte meter (the feeder counts fragment bytes; the stats task
    // turns the delta into a bitrate for the publisher dashboard).
    let ingest_bytes = Arc::new(AtomicU64::new(0));

    // Durable floor (TECH_SPEC §8.6): sparse segment uploads + the (seq → CID) map.
    let (dur_tx, dur_rx) = unbounded_channel::<(Seq, Bytes)>();
    tasks.extend(spawn_durable_uploader(&session, dur_rx));

    // Feeder: tail the ingest dir → preview sink + the publisher's mesh seed +
    // the live-edge manifest. Emits `publish-state` and keeps `live_flag` current so
    // a re-attaching UI can read the true live state via `publish_status`.
    let live_flag = Arc::new(AtomicBool::new(false));

    // The feeder differs by ingest; everything downstream (mesh, edge, durable, sinks)
    // is identical. WHIP → WebRTC access units → FragmentBuilder (mirrors the Android
    // camera path); RTMP → ffmpeg tails CMAF fragments off disk.
    let (feeder, ingest_server, stream_key) = if whip {
        let ptx = pub_tx.clone();
        let appc = app.clone();
        let init_slot_feeder = init_slot.clone();
        let live_w = live_flag.clone();
        let ingest_w = ingest_bytes.clone();
        // Fast tier: publisher-direct sub-second WebRTC media to opt-in viewers. WHIP exposes
        // raw access units to packetize; the accept loop answers fast-tier offers and the
        // feeder fans each AU onto the connected viewers' tracks. (RTMP has no raw AUs, so it
        // offers no fast tier — those viewers stay on the mesh.)
        let fast = fasttier::FastTier::new(FAST_TIER_CAP, stun());
        tasks.push(fasttier::spawn_accept_loop(session.signaling(), session.my_peer, fast.clone()));
        let fast_feed = fast;
        let (whip_tx, whip_rx) = std::sync::mpsc::channel::<whip_ingest::server::IngestAu>();
        let server = whip_ingest::server::start(whip_tx, stun())
            .map_err(|e| format!("couldn't start the WHIP endpoint: {e}"))?;
        let ingest_url = server.url();
        // Bridge the WHIP server's std channel (tiny_http thread) into async.
        let (au_tx, mut au_rx) = unbounded_channel::<whip_ingest::server::IngestAu>();
        std::thread::spawn(move || {
            while let Ok(iu) = whip_rx.recv() {
                if au_tx.send(iu).is_err() {
                    break;
                }
            }
        });
        let feeder = tokio::spawn(async move {
            let _server = server; // hold the endpoint alive for the feeder's lifetime
            let mut fb: Option<segmenter::FragmentBuilder> = None;
            let mut last_pts: Option<i64> = None;
            let mut live = false;
            let mut last_fresh = std::time::Instant::now();
            let announce_live = |appc: &AppHandle, live: bool| {
                let _ = appc.emit("publish-state", PublishStateMsg { live });
                let _ = appc.emit(
                    "publish-progress",
                    PublishProgressMsg { step: "encoder".into(), ok: live, detail: String::new() },
                );
            };
            loop {
                // The live flag flips off if RTP dries up (encoder stopped/roaming).
                let iu = match tokio::time::timeout(Duration::from_millis(500), au_rx.recv()).await {
                    Ok(Some(iu)) => iu,
                    Ok(None) => break, // bridge closed
                    Err(_) => {
                        if live && last_fresh.elapsed() > Duration::from_millis(2000) {
                            live = false;
                            live_w.store(false, Ordering::Relaxed);
                            announce_live(&appc, false);
                        }
                        continue;
                    }
                };
                // Build the muxer once the codec config (SPS/PPS) arrives, deriving the
                // frame dimensions from the SPS (WHIP carries no explicit size).
                if fb.is_none() {
                    if let Some((sps, pps)) = iu.config {
                        let (width, height) =
                            segmenter::sps::dimensions(&sps).unwrap_or((1280, 720));
                        // WHIP → low-latency: emit ~250ms CMAF parts, not whole GOPs.
                        let builder = segmenter::FragmentBuilder::new_ll(
                            segmenter::H264Params { sps, pps, width, height },
                            LL_PART_MS,
                        );
                        let init = builder.init_segment();
                        *init_slot_feeder.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(init.clone());
                        preview.push_init(init);
                        fb = Some(builder);
                    } else {
                        continue; // wait for config before any media fragment
                    }
                }
                let builder = fb.as_mut().unwrap();
                // Sample duration in 90 kHz ticks from PTS deltas (first AU ≈30 fps).
                let dur = match last_pts {
                    Some(prev) => (((iu.au.pts_us - prev).max(1)) as u64 * 90_000 / 1_000_000) as u32,
                    None => 3_000,
                };
                last_pts = Some(iu.au.pts_us);
                // Fan the raw access unit onto any fast-tier viewers' WebRTC tracks (before
                // muxing — the fast tier is the un-segmented, sub-second path).
                fast_feed.broadcast(&iu.au.data, fasttier::pts_us_to_rtp90k(iu.au.pts_us));
                if let Some(seg) = builder.push_au(&iu.au.data, dur.max(1), iu.au.keyframe) {
                    ingest_w.fetch_add(seg.bytes.len() as u64, Ordering::Relaxed);
                    preview.push_segment(seg.seq, seg.bytes.clone());
                    let _ = dur_tx.send((seg.seq, seg.bytes.clone()));
                    let _ = ptx.send(EngineEvent::Produced { seq: seg.seq, id: seg.id, bytes: seg.bytes });
                    let _ = edge_tx.send((seg.seq, seg.id));
                    last_fresh = std::time::Instant::now();
                    if !live {
                        live = true;
                        live_w.store(true, Ordering::Relaxed);
                        announce_live(&appc, true);
                    }
                }
            }
        });
        (feeder, ingest_url, String::new())
    } else {
        let port = 21935u16;
        let key = "unstation";
        let url = segmenter::rtmp_url(port, key);
        // The ingest dir — wiped to a clean slate each session. Stale fragments from a
        // previous stream belong to an unrelated encode timeline (they'd make the player
        // replay old video then stall at the discontinuity); a clean dir also keeps the
        // feeder's index-based segment sequence correct.
        let dir = std::env::temp_dir().join("unstation-publish");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let ingest_server = format!("rtmp://127.0.0.1:{port}/live");
        let ptx = pub_tx.clone();
        let appc = app.clone();
        let init_slot_feeder = init_slot.clone();
        let live_w = live_flag.clone();
        let ingest_w = ingest_bytes.clone();
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
                            *init_slot_feeder.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(init.clone());
                            preview.push_init(init);
                            init_sent = true;
                        }
                    }
                    if init_sent {
                        for s in news {
                            ingest_w.fetch_add(s.bytes.len() as u64, Ordering::Relaxed);
                            preview.push_segment(s.seq, s.bytes.clone());
                            let _ = dur_tx.send((s.seq, s.bytes.clone())); // sparse durable floor
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
                // Preflight: the encoder check flips with the real fragment flow.
                let _ = appc.emit(
                    "publish-progress",
                    PublishProgressMsg { step: "encoder".into(), ok: live, detail: String::new() },
                );
            }
        }
        });
        (feeder, ingest_server, key.to_string())
    };

    // Publisher dashboard numbers: viewer count + ingest/uplink bitrates.
    let stats = spawn_publish_stats(app.clone(), session.clone(), ingest_bytes, pub_stats_rx);

    tasks.push(feeder);
    tasks.push(stats);
    let info = PublishInfo {
        ingest_server,
        stream_key,
        hls_url,
        ingest_mode: if whip { "whip".into() } else { "rtmp".into() },
    };
    *state.publish.lock().unwrap() = Some(PublishSession {
        _hls: hls,
        tasks,
        pub_tx,
        session,
        name: canon,
        info: info.clone(),
        live: live_flag,
    });

    Ok(info)
}

/// Android publish: the Kotlin camera plugin (CameraX → MediaCodec) pushes encoded H.264
/// access units over JNI (see `camera`); the feeder muxes them into CMAF with
/// `FragmentBuilder` and drives the SAME mesh path as the desktop's ffmpeg feeder.
#[cfg(all(feature = "publish", target_os = "android"))]
#[tauri::command]
async fn start_publish(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<PublishInfo, String> {
    if !*state.chain_ready.lock().unwrap() {
        return Err("Sign in with the Polkadot app to go live — your stream is announced under your verified identity.".into());
    }
    let name = title.unwrap_or_else(|| "unstation".into());
    let canon = canonical_stream_name(&name);
    let stream = stream_id_from(&name);

    // Re-attach: already publishing this stream → hand back its details unchanged.
    if let Some(s) = state.publish.lock().unwrap().as_ref() {
        if s.name == canon {
            return Ok(s.info.clone());
        }
    }
    // Replace a prior/stale session (aborting its feeder + closing the AU intake).
    if let Some(prev) = state.publish.lock().unwrap().take() {
        teardown_publish(prev);
        camera::close_stream();
    }
    // Broadcasting owns the uplink — retire any background seed.
    teardown_seed(&state);

    // Self-preview HLS + the publisher node inbox — identical to desktop. The camera path
    // muxes in-process (like WHIP), so it emits CMAF parts → low-latency re-server.
    let hls = HlsServer::start_ll(1000, LL_PART_MS).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let preview = hls.sink();
    let (pub_tx, pub_rx) = unbounded_channel::<EngineEvent>();
    let session = Session::start(stream, 1, stun(), pub_tx.clone())?;
    // Phone uplinks fan out to fewer directly-served viewers than desktop, but the
    // origin still shouldn't sit at the viewer default.
    session.set_max_inbound(64);
    let _ = app.emit(
        "publish-progress",
        PublishProgressMsg { step: "identity".into(), ok: true, detail: String::new() },
    );
    let (pub_stats_tx, pub_stats_rx) =
        tokio::sync::watch::channel(unstation_core::node::NodeStats::default());
    let publisher = MeshNode::new_live_publisher(
        session.my_peer,
        cfg(Mode::Live, Role::Publisher),
        SEG_BYTES,
        Arc::new(NullSink),
    )
    .with_stream_id(stream.0)
    .with_edge_signer(Arc::new(IdentityEdgeSigner))
    .with_presence_book(session.presence_book())
    // Convictions (forged bytes, floods) bar re-dials + offers at the session edge.
    .with_ban_list(session.ban_list())
    .with_stats(pub_stats_tx);
    tokio::spawn(async move {
        let _ = publisher.run(pub_rx, TICK, None).await;
    });
    // Track every spawned task so teardown really ends the stream (see the desktop path).
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    tasks.push(session.spawn_presence(80_000_000, true, Arc::new(AtomicBool::new(true))));

    let init_slot: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
    tasks.push(spawn_manifest_publish(
        app.clone(),
        &session,
        stream,
        init_slot.clone(),
        Some(LL_PART_MS),
    ));

    let (edge_tx, edge_rx) = unbounded_channel::<(Seq, SegmentId)>();
    tasks.push(session.spawn_edge_publisher(edge_rx));

    // Camera → CMAF byte meter for the dashboard's ingest bitrate.
    let ingest_bytes = Arc::new(AtomicU64::new(0));

    // Durable floor (TECH_SPEC §8.6): sparse segment uploads + the (seq → CID) map.
    let (dur_tx, dur_rx) = unbounded_channel::<(Seq, Bytes)>();
    tasks.extend(spawn_durable_uploader(&session, dur_rx));

    // Feeder: drain encoded AUs from the camera plugin → mux to CMAF → the three sinks
    // (self-preview, mesh `Produced`, live edge), exactly like the ffmpeg feeder's tail.
    let ptx = pub_tx.clone();
    let appc = app.clone();
    let live_flag = Arc::new(AtomicBool::new(false));
    let live_w = live_flag.clone();
    let ingest_w = ingest_bytes.clone();
    // Open the AU intake synchronously — BEFORE returning to JS (which then starts the camera
    // plugin) — so the encoder's config/frames aren't dropped, nor its config cleared by a late
    // open_stream racing the plugin's `nativeConfig`.
    let mut rx = camera::open_stream();
    let feeder = tokio::spawn(async move {
        // Wait for the encoder's codec-specific data (SPS/PPS) before building the muxer.
        let config = loop {
            if let Some(c) = camera::take_config() {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        log::info!(
            "[publish] camera config {}x{}, sps={}B pps={}B",
            config.width, config.height, config.sps.len(), config.pps.len()
        );
        // Low-latency: emit ~250ms CMAF parts (matches the WHIP path + the advertised manifest).
        let mut fb = segmenter::FragmentBuilder::new_ll(
            segmenter::H264Params {
                sps: config.sps,
                pps: config.pps,
                width: config.width,
                height: config.height,
            },
            LL_PART_MS,
        );
        // Init segment → self-preview + the manifest publisher (Bulletin, via `init_slot`).
        let init = fb.init_segment();
        *init_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(init.clone());
        preview.push_init(init);
        live_w.store(true, Ordering::Relaxed);
        let _ = appc.emit("publish-state", PublishStateMsg { live: true });
        let _ = appc.emit(
            "publish-progress",
            PublishProgressMsg { step: "encoder".into(), ok: true, detail: String::new() },
        );

        let mut last_pts: Option<i64> = None;
        while let Some(au) = rx.recv().await {
            // Sample duration in 90 kHz ticks from PTS deltas (≈constant-fps; the first AU
            // gets a ~30fps default). Close enough for CMAF timing at low latency.
            let dur = match last_pts {
                Some(prev) => (((au.pts_us - prev).max(1)) as u64 * 90_000 / 1_000_000) as u32,
                None => 3_000,
            };
            last_pts = Some(au.pts_us);
            if let Some(seg) = fb.push_au(&au.data, dur.max(1), au.keyframe) {
                log::info!("[publish] fragment seq={} ({} B)", seg.seq, seg.bytes.len());
                ingest_w.fetch_add(seg.bytes.len() as u64, Ordering::Relaxed);
                preview.push_segment(seg.seq, seg.bytes.clone());
                let _ = dur_tx.send((seg.seq, seg.bytes.clone())); // sparse durable floor
                let _ = ptx.send(EngineEvent::Produced { seq: seg.seq, id: seg.id, bytes: seg.bytes });
                let _ = edge_tx.send((seg.seq, seg.id));
            }
        }
    });

    let stats = spawn_publish_stats(app.clone(), session.clone(), ingest_bytes, pub_stats_rx);

    // No RTMP ingest on Android — the camera is the source; the UI hides the OBS rail.
    let info = PublishInfo { ingest_server: String::new(), stream_key: String::new(), hls_url, ingest_mode: "camera".into() };
    tasks.push(feeder);
    tasks.push(stats);
    *state.publish.lock().unwrap() = Some(PublishSession {
        _hls: hls,
        tasks,
        pub_tx,
        session,
        name: canon,
        info: info.clone(),
        live: live_flag,
    });
    Ok(info)
}

/// Fully tear down a publish: abort every background task (feeder — which kills the
/// ffmpeg ingest via `Drop` — stats, presence refresh, edge publisher, manifest publish),
/// stop the node, and actively close the WebRTC connections. Mirrors `teardown_watch`:
/// without `session.shutdown()` the transport reactor (kept alive by detached signaling
/// tasks) holds viewer connections open, and without aborting the presence/edge tasks the
/// stopped stream keeps announcing itself to the chain forever.
#[cfg(feature = "publish")]
fn teardown_publish(sess: PublishSession) {
    for t in sess.tasks {
        t.abort();
    }
    let _ = sess.pub_tx.send(EngineEvent::Stop);
    sess.session.shutdown();
    // `_hls` drops here.
}

#[cfg(feature = "publish")]
#[tauri::command]
fn stop_publish(state: State<'_, AppState>) {
    if let Some(sess) = state.publish.lock().unwrap().take() {
        teardown_publish(sess);
    }
    // Android: stop accepting encoded AUs from the camera plugin.
    #[cfg(target_os = "android")]
    camera::close_stream();
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

/// Handle to the registered Android camera plugin, so the app-command wrappers below can drive
/// it. (Plugin commands need an ACL permission our inline plugin can't define; app commands
/// don't — so JS calls `camera_start`/`camera_stop`, and we forward to the plugin here.)
#[cfg(all(target_os = "android", feature = "publish"))]
static CAMERA_PLUGIN: std::sync::OnceLock<tauri::plugin::PluginHandle<tauri::Wry>> =
    std::sync::OnceLock::new();

/// Start the Android camera capture (Camera2 → MediaCodec → the Rust muxer). No-op on desktop,
/// which ingests via RTMP/OBS. JS calls this right after `start_publish` opens the AU intake.
#[cfg(feature = "publish")]
#[tauri::command]
fn camera_start() -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        return CAMERA_PLUGIN
            .get()
            .ok_or("camera plugin not registered")?
            .run_mobile_plugin::<()>("startCapture", ())
            .map_err(|e| e.to_string());
    }
    #[cfg(not(target_os = "android"))]
    Ok(())
}

/// Stop the Android camera capture. No-op on desktop.
#[cfg(feature = "publish")]
#[tauri::command]
fn camera_stop() -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        return CAMERA_PLUGIN
            .get()
            .ok_or("camera plugin not registered")?
            .run_mobile_plugin::<()>("stopCapture", ())
            .map_err(|e| e.to_string());
    }
    #[cfg(not(target_os = "android"))]
    Ok(())
}

/// Keep the screen on while actively watching or broadcasting (a phone that dims
/// mid-match kills the party). Android only; a no-op elsewhere — the JS side calls
/// it unconditionally through the `__keepAwake` seam.
#[tauri::command]
fn set_keep_awake(on: bool) -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "publish"))]
    {
        #[derive(serde::Serialize)]
        struct Args {
            on: bool,
        }
        return CAMERA_PLUGIN
            .get()
            .ok_or("camera plugin not registered")?
            .run_mobile_plugin::<()>("setKeepAwake", Args { on })
            .map_err(|e| e.to_string());
    }
    #[cfg(not(all(target_os = "android", feature = "publish")))]
    {
        let _ = on;
        Ok(())
    }
}

/// Open this app's system settings page — the recovery path after a "don't ask
/// again" camera-permission denial. Android only; a no-op elsewhere.
#[tauri::command]
fn open_app_settings() -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "publish"))]
    {
        return CAMERA_PLUGIN
            .get()
            .ok_or("camera plugin not registered")?
            .run_mobile_plugin::<()>("openAppSettings", ())
            .map_err(|e| e.to_string());
    }
    #[cfg(not(all(target_os = "android", feature = "publish")))]
    Ok(())
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
        // Inbound invite links (unstation://watch/<name>) for both shells: the JS side
        // subscribes via `window.__TAURI__.deepLink` (onOpenUrl/getCurrent). The desktop
        // shell additionally registers single-instance (with its `deep-link` feature) so a
        // second launch forwards its argv URL here instead of opening a new window.
        .plugin(tauri_plugin_deep_link::init())
        .manage(AppState::default());
    // Android camera-publish (M4): register the Kotlin CameraPlugin (Camera2 + MediaCodec →
    // encoded AUs into the Rust core via CameraBridge). JS drives it via `plugin:unstation-camera`.
    #[cfg(all(target_os = "android", feature = "publish"))]
    let b = b.plugin(
        tauri::plugin::Builder::<tauri::Wry, ()>::new("unstation-camera")
            .setup(|_app, api| {
                let handle = api.register_android_plugin("io.parity.unstation.android", "CameraPlugin")?;
                let _ = CAMERA_PLUGIN.set(handle);
                Ok(())
            })
            .build(),
    );
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
        stop_seed,
        watch_status,
        fast_watch_start,
        fast_watch_stop,
        start_publish,
        stop_publish,
        publish_status,
        camera_start,
        camera_stop,
        set_keep_awake,
        open_app_settings
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
        stop_seed,
        watch_status,
        fast_watch_start,
        fast_watch_stop,
        set_keep_awake,
        open_app_settings
    ]);
    b
}
