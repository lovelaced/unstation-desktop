//! `unstation-node` — a headless volunteer seed/relay (TECH_SPEC §8.5 / D7).
//!
//! The decentralized answer to a TURN/CDN tier: anyone can run this binary anywhere
//! and it joins a stream's swarm as a `Role::Seed` node — fetching the live window
//! like a viewer (every segment hash-verified against the publisher's signed live
//! edge), caching it, and reserving its uplink for peers who can't reach the
//! publisher directly. More volunteers = more capacity and more reachable entry
//! points, with no operator to take down. It plays nothing and stores nothing
//! beyond the rolling live window.
//!
//! Usage:
//!   unstation-node <stream-name>
//!
//! Identity (statement-store writes need an on-chain allowance):
//!   UNSTATION_NODE_MNEMONIC   sign with a pre-provisioned account (public Paseo)
//!   UNSTATION_NODE_KEY_DIR    persist a generated key here and auto-provision it
//!                             (local/dev chains only; default ~/.unstation-node)
//!
//! Tuning:
//!   UNSTATION_NODE_BUDGET_MBPS  uplink to donate (default 50)
//!   UNSTATION_STUN / UNSTATION_TURN  ICE servers (comma-separated)
//!   HOST_STATEMENT_STORE_WS_ENDPOINTS  override the statement-store endpoints

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::unbounded_channel;
use unstation_chain::BulletinOrigin;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::manifest::OriginOfRecord;
use unstation_core::node::MeshNode;
use unstation_core::signaling::Presence;
use unstation_core::transport::EngineEvent;
use unstation_core::types::StreamId;
use unstation_core::BoxFuture;
use unstation_session::Session;

/// Mirrors the app's segment-size hint (SEG_BYTES) — a picker heuristic, not a limit.
const SEG_BYTES: u64 = 200_000;
/// Engine tick (matches the app).
const TICK: Duration = Duration::from_millis(100);

/// Same canonicalization as the app's `canonical_stream_name`: lowercase, runs of
/// non-alphanumerics → single hyphens, `.dot` suffix dropped, empty → "my-stream".
/// The two MUST agree byte-for-byte or the seed joins the wrong (empty) swarm.
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
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "my-stream".into()
    } else {
        out
    }
}

fn stream_id_from(name: &str) -> StreamId {
    StreamId(crypto::blake2b256(canonical_stream_name(name).as_bytes()))
}

/// ICE servers — same env contract as the app.
fn stun() -> Vec<String> {
    let mut servers: Vec<String> = match std::env::var("UNSTATION_STUN") {
        Ok(v) => v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from).collect(),
        Err(_) => vec![
            "stun:stun.l.google.com:19302".into(),
            "stun:stun.cloudflare.com:3478".into(),
        ],
    };
    if let Ok(turn) = std::env::var("UNSTATION_TURN") {
        servers.extend(turn.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from));
    }
    servers
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let name = match std::env::args().nth(1) {
        Some(n) if n != "--help" && n != "-h" => n,
        _ => {
            eprintln!("usage: unstation-node <stream-name>   (see the header of main.rs for env config)");
            std::process::exit(2);
        }
    };
    let canon = canonical_stream_name(&name);
    let stream = stream_id_from(&name);
    let budget_bps = env_u64("UNSTATION_NODE_BUDGET_MBPS", 50) * 1_000_000;

    // ---- identity: an allowance-backed statement-store key ----
    if let Ok(mnemonic) = std::env::var("UNSTATION_NODE_MNEMONIC") {
        if let Err(e) = unstation_chain::init_from_mnemonic(mnemonic.trim()) {
            eprintln!("identity from mnemonic failed: {e}");
            std::process::exit(1);
        }
        log::info!("[seed] identity: mnemonic-derived (pre-provisioned)");
    } else {
        let dir = std::env::var("UNSTATION_NODE_KEY_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.unstation-node")
        });
        let _ = std::fs::create_dir_all(&dir);
        unstation_chain::init_statement_store_persisted(std::path::Path::new(&dir));
        log::info!("[seed] identity: persisted key in {dir} (auto-provision — dev/testnet chains)");
    }
    if !unstation_chain::wait_ready(Duration::from_secs(30)) {
        log::warn!("[seed] statement store not confirmed subscribed after 30s — continuing (it may still connect)");
    }

    log::info!("[seed] joining swarm for \"{canon}\" (stream {})", crypto::hex32(&stream.0));

    // ---- seed node + session ----
    let (node_tx, node_rx) = unbounded_channel::<EngineEvent>();
    let session = match Session::start(stream, 1, stun(), node_tx.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("session start failed: {e}");
            std::process::exit(1);
        }
    };
    // A relay exists to be dialed — accept a wide inbound set.
    session.set_max_inbound(256);

    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Seed,
        window: 64,
        tick: TICK,
        seg_ms: 1000, // retimed by SetSegMs once the verified manifest arrives
        upload_budget_bps: budget_bps,
        weights: Default::default(),
    };
    let (stats_tx, stats_rx) = tokio::sync::watch::channel(unstation_core::node::NodeStats::default());
    let node = MeshNode::new_seed(session.my_peer, cfg, SEG_BYTES, HashMap::new(), 0)
        .with_stream_id(stream.0)
        .with_presence_book(session.presence_book())
        .with_ban_list(session.ban_list())
        .with_stats(stats_tx);
    let node_task = tokio::spawn(node.run(node_rx, TICK, None));

    // Live edge from the chain (reconciliation; signed gossip covers steady state).
    let edge_task = session.spawn_edge_poller(node_tx.clone());

    // Announce ourselves WITH relay opt-in: volunteering reachable uplink is this
    // binary's entire purpose. The gate stays on — a dedicated seed has no player
    // whose experience could degrade; peers that find it slow will deprioritize it.
    let always_healthy = Arc::new(AtomicBool::new(true));
    let presence_task = session.spawn_presence(budget_bps, true, always_healthy);

    // Trust gate, same shape as the app's watch path: a candidate carrying a manifest
    // CID must verify against its personhood key before we join its swarm; verified
    // manifests hand the node the publisher key (gossip verification anchor) and the
    // stream's real part duration. Manifest-less candidates are resharing viewers —
    // fine to fetch from, their segments still hash-verify against the signed edge.
    let maintainer_task = {
        let vtx = node_tx.clone();
        let filter: Arc<dyn Fn(Presence) -> BoxFuture<'static, bool> + Send + Sync> =
            Arc::new(move |cand: Presence| {
                let vtx = vtx.clone();
                Box::pin(async move {
                    let Some(cid) = cand.manifest_cid.clone() else { return true };
                    match BulletinOrigin.fetch_manifest(cid).await {
                        Ok(m) if m.verify(&cand.publisher).is_ok() => {
                            let _ = vtx.send(EngineEvent::SetPublisherKey { key: cand.publisher });
                            if m.manifest.target_segment_ms > 0 {
                                let _ = vtx.send(EngineEvent::SetSegMs(m.manifest.target_segment_ms as u64));
                            }
                            log::info!("[seed] verified publisher {}", crypto::hex32(&cand.publisher));
                            true
                        }
                        Ok(_) => {
                            log::warn!("[seed] manifest signature check FAILED for a candidate — skipping impostor");
                            false
                        }
                        Err(e) => {
                            log::warn!("[seed] manifest fetch failed ({e:?}); proceeding (segments still hash-verified)");
                            true
                        }
                    }
                })
            });
        session.spawn_maintainer(3, filter)
    };

    // Heartbeat: the numbers a volunteer wants to see (peers served, uplink donated).
    let hb_session = session.clone();
    let heartbeat = tokio::spawn(async move {
        let mut last_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let ns = stats_rx.borrow().clone();
            let uplink_kbps = ns.sent_bytes.saturating_sub(last_sent) * 8 / 10 / 1000;
            last_sent = ns.sent_bytes;
            log::info!(
                "[seed] peers={} cached_head={} uplink={}kbps served_total={}MB verify_fail={}",
                hb_session.peer_count(),
                ns.head_seq,
                uplink_kbps,
                ns.sent_bytes / 1_000_000,
                ns.hash_failures,
            );
        }
    });

    // Run until Ctrl-C, then tear down cleanly (close PCs so peers fail over fast).
    let _ = tokio::signal::ctrl_c().await;
    log::info!("[seed] shutting down");
    let _ = node_tx.send(EngineEvent::Stop);
    edge_task.abort();
    presence_task.abort();
    maintainer_task.abort();
    heartbeat.abort();
    session.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), node_task).await;
}
