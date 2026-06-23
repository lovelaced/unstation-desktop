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

use bytes::Bytes;
use hls_server::HlsServer;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::types::{SegmentId, Seq, StreamId};
use unstation_session::Session;

/// Nominal segment size for the picker's expected-delivery-time estimates.
const SEG_BYTES: u64 = 200_000;
/// Engine tick.
const TICK: Duration = Duration::from_millis(100);

#[derive(Default)]
struct AppState {
    signed_in: Mutex<bool>,
    watch: Mutex<Option<WatchSession>>,
    publish: Mutex<Option<PublishSession>>,
}

/// An active watch: the HLS server feeding the player, the viewer node's inbox,
/// the session (kept alive to hold the transport + signaling tasks), and the
/// background tasks (discover/dial, stats, node loop).
struct WatchSession {
    _hls: HlsServer,
    node_tx: UnboundedSender<EngineEvent>,
    _session: Session,
    tasks: Vec<JoinHandle<()>>,
}

/// An active publish: RTMP ingest, the self-preview HLS, the feeder task, the
/// publisher node's inbox, and the session.
struct PublishSession {
    _hls: HlsServer,
    /// Owns the ffmpeg ingest listener; aborting it kills ffmpeg via `Drop`.
    feeder: JoinHandle<()>,
    stats: JoinHandle<()>,
    pub_tx: UnboundedSender<EngineEvent>,
    _session: Session,
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

#[derive(Serialize, Clone)]
struct PublishInfo {
    ingest_server: String,
    stream_key: String,
    hls_url: String,
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

/// Default ICE servers. Host candidates carry a LAN on their own; a public STUN
/// server lets cross-subnet/NAT pairs find a route too (full relay/TURN is M4).
fn stun() -> Vec<String> {
    vec!["stun:stun.l.google.com:19302".into()]
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
fn ss_key_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir: {e}"))?
        .join("statement-store");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create key dir: {e}"))?;
    Ok(dir)
}

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
    let stream = stream_id_from(&target);

    // Localhost HLS re-server → the webview <video> plays from here.
    let hls = HlsServer::start(1000).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let sink: Arc<dyn MediaSink> = Arc::new(hls.sink());

    // Viewer node inbox; the transport posts PeerConnected/Inbound here.
    let (view_tx, view_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC for this stream.
    let key_dir = ss_key_dir(&app)?;
    let session = Session::start(stream, 1, stun(), key_dir, view_tx.clone())?;

    // Real viewer node: starts with no known segments; the live-edge poller feeds
    // it `LiveEdge { seq, id }` so it knows what to fetch and how to verify it.
    let viewer = MeshNode::new_viewer(
        session.my_peer,
        cfg(Mode::Live, Role::Viewer),
        SEG_BYTES,
        sink,
        HashMap::new(),
        0,
    );
    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(async move {
        let _ = viewer.run(view_rx, TICK, None).await;
    }));

    // Learn the live edge (segment ids) from the publisher.
    session.spawn_edge_poller(view_tx.clone());

    // Discover the publisher and dial it (in the background — watch returns now so
    // the UI can attach the player while the mesh comes up).
    {
        let s = session.clone();
        tasks.push(tokio::spawn(async move {
            let publisher = s.discover_publisher().await;
            s.dial(publisher);
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

    *state.watch.lock().unwrap() = Some(WatchSession {
        _hls: hls,
        node_tx: view_tx,
        _session: session,
        tasks,
    });

    Ok(WatchInfo {
        hls_url,
        stream_id: resolve_stream(target.clone()),
        publisher: target,
        peers: 0,
        rho: 0,
    })
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

/// Go Live: start the local RTMP ingest (point OBS here), run a live publisher
/// node, announce the stream on the statement store, and serve a self-preview.
#[tauri::command]
async fn start_publish(
    app: AppHandle,
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<PublishInfo, String> {
    if !segmenter::ffmpeg_available() {
        return Err("ffmpeg not found. Install it (e.g. `brew install ffmpeg`), or set \
                    UNSTATION_FFMPEG to its full path, then try again."
            .into());
    }
    // Replacing any prior publish session (e.g. reopening the ingest after an
    // encoder disconnect): abort its background tasks now — the feeder's Drop also
    // kills its ffmpeg ingest — so we don't leak them or fight over the RTMP port.
    if let Some(prev) = state.publish.lock().unwrap().take() {
        prev.feeder.abort();
        prev.stats.abort();
    }
    let name = title.unwrap_or_else(|| "unstation".into());
    let stream = stream_id_from(&name);
    let port = 21935u16;
    let key = "unstation";
    let url = segmenter::rtmp_url(port, key);

    // The ingest dir. The feeder owns the ffmpeg listener and (re)starts it, so the
    // ingest is always available regardless of when the encoder connects.
    let dir = std::env::temp_dir().join("unstation-publish");

    // Self-preview HLS + the publisher node inbox.
    let hls = HlsServer::start(1000).map_err(|e| e.to_string())?;
    let hls_url = hls.url();
    let preview = hls.sink();
    let (pub_tx, pub_rx) = unbounded_channel::<EngineEvent>();

    // Boot chain signaling + WebRTC, then the live publisher node (its PeerId is
    // the statement-store account it announces under).
    let key_dir = ss_key_dir(&app)?;
    let session = Session::start(stream, 1, stun(), key_dir, pub_tx.clone())?;
    let publisher = MeshNode::new_live_publisher(
        session.my_peer,
        cfg(Mode::Live, Role::Publisher),
        SEG_BYTES,
        Arc::new(NullSink),
    );
    tokio::spawn(async move {
        let _ = publisher.run(pub_rx, TICK, None).await;
    });

    // Announce presence + republish the live-edge manifest as segments are made.
    session.spawn_presence(80_000_000);
    let (edge_tx, edge_rx) = unbounded_channel::<(Seq, SegmentId)>();
    session.spawn_edge_publisher(edge_rx);

    // Feeder: tail the ingest dir → preview sink + the publisher's mesh seed +
    // the live-edge manifest. Announces `publish-live` on the first fragment.
    let ptx = pub_tx.clone();
    let appc = app.clone();
    let feeder = tokio::spawn(async move {
        // One ingest listener per Go-Live. ffmpeg's RTMP `-listen` binds the port and
        // waits for the encoder however long it takes, segments it, then exits when the
        // encoder disconnects — at which point we end the session cleanly. (The previous
        // self-healing respawn tore the ingest down + rebuilt it the instant ffmpeg
        // exited, racing the player and the HLS reset; that churn is gone.) `seg` is
        // owned, so dropping it on stop/disconnect kills ffmpeg and frees the port.
        let mut seg = match segmenter::spawn(&segmenter::Source::RtmpListen { url: &url }, &dir, 1) {
            Ok(s) => s,
            Err(e) => {
                let _ = appc.emit(
                    "publish-hint",
                    PublishHintMsg { message: format!("ingest failed to start: {e}") },
                );
                return;
            }
        };
        let mut seen = 0u64;
        let mut init_sent = false;
        let mut announced = false;
        let mut waited_ms = 0u64;
        let mut hinted = false;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;

            if !init_sent {
                if let Some(init) = segmenter::load_init(&dir) {
                    preview.push_init(init);
                    init_sent = true;
                }
            }
            if let Ok(news) = segmenter::load_segments_from(&dir, seen) {
                for s in news {
                    preview.push_segment(s.seq, s.bytes.clone());
                    let _ = ptx.send(EngineEvent::Produced { seq: s.seq, id: s.id, bytes: s.bytes });
                    let _ = edge_tx.send((s.seq, s.id));
                    seen = s.seq + 1;
                }
                if !announced && seen > 0 {
                    announced = true;
                    let _ = appc.emit("publish-live", ());
                }
            }

            // Encoder gone (ffmpeg exited)? End the session cleanly. The UI reopens a
            // fresh ingest on `publish-ended`, so reconnecting an encoder just works.
            if !seg.running() {
                if announced {
                    let _ = appc.emit("publish-ended", ());
                } else {
                    let log = std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap_or_default();
                    let mut tail: Vec<&str> = log.trim().lines().rev().take(3).collect();
                    tail.reverse();
                    let message = if tail.is_empty() {
                        "Ingest stopped before any video arrived — is another app using the port?".to_string()
                    } else {
                        format!("Ingest stopped. ffmpeg: {}", tail.join(" · "))
                    };
                    let _ = appc.emit("publish-hint", PublishHintMsg { message });
                }
                break;
            }

            // Still waiting on the first frame? Surface ffmpeg's own log, don't hang.
            if !announced {
                waited_ms += 200;
                if !hinted && waited_ms >= 8_000 {
                    hinted = true;
                    let log = std::fs::read_to_string(dir.join("ffmpeg.log")).unwrap_or_default();
                    let mut tail: Vec<&str> = log.trim().lines().rev().take(3).collect();
                    tail.reverse();
                    let message = if tail.is_empty() {
                        format!("No video yet — point your encoder at rtmp://127.0.0.1:{port}/live (key: unstation), then start streaming.")
                    } else {
                        format!("No video yet. ffmpeg: {}", tail.join(" · "))
                    };
                    let _ = appc.emit("publish-hint", PublishHintMsg { message });
                }
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

    *state.publish.lock().unwrap() = Some(PublishSession {
        _hls: hls,
        feeder,
        stats,
        pub_tx,
        _session: session,
    });

    Ok(PublishInfo {
        ingest_server: format!("rtmp://127.0.0.1:{port}/live"),
        stream_key: key.into(),
        hls_url,
    })
}

#[tauri::command]
fn stop_publish(state: State<'_, AppState>) {
    if let Some(sess) = state.publish.lock().unwrap().take() {
        sess.feeder.abort(); // dropping the feeder kills the ffmpeg ingest (Drop)
        sess.stats.abort();
        let _ = sess.pub_tx.send(EngineEvent::Stop);
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            platform,
            signin_status,
            begin_signin,
            complete_signin,
            resolve_stream,
            start_watch,
            stop_watch,
            start_publish,
            stop_publish
        ])
        .run(tauri::generate_context!())
        .expect("error while running Unstation");
}
