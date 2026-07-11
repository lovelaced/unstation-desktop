//! One stream's worth of seeding: session + mesh node + the per-stream service
//! tasks, extracted verbatim from the old single-stream main so the supervisor can
//! run several side by side. Each worker's `Session` dials under its own fresh
//! per-session peer id — unrelated to the process-stable `local_peer_id()` the
//! supervisor announces as its recruit-inbox address.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use unstation_chain::BulletinOrigin;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::crypto;
use unstation_core::manifest::OriginOfRecord;
use unstation_core::node::{MeshNode, NodeStats};
use unstation_core::signaling::Presence;
use unstation_core::transport::EngineEvent;
use unstation_core::types::StreamId;
use unstation_core::BoxFuture;
use unstation_session::Session;

/// Mirrors the app's segment-size hint (SEG_BYTES) — a picker heuristic, not a limit.
const SEG_BYTES: u64 = 200_000;
/// Engine tick (matches the app).
const TICK: Duration = Duration::from_millis(100);

/// A point-in-time view of one worker, for the supervisor's policy sweep + heartbeat.
pub struct WorkerSnapshot {
    pub stream: StreamId,
    pub canon_name: String,
    pub pinned: bool,
    pub peers: usize,
    pub head_seq: u64,
    pub sent_bytes: u64,
    pub hash_failures: u64,
    pub spawned_at: Instant,
    pub last_head_advance: Instant,
    pub dormant: bool,
}

/// A running per-stream seed: the session, the mesh node task, and its service tasks
/// (edge poller, presence, maintainer). Owned by the supervisor.
pub struct StreamWorker {
    pub stream: StreamId,
    pub canon_name: String,
    pub pinned: bool,
    /// The recruiting publisher's personhood key, when this worker came from a
    /// verified recruitment (policy's per-publisher cap keys off it). Pins have none.
    pub publisher: Option<[u8; 32]>,
    session: Session,
    node_tx: UnboundedSender<EngineEvent>,
    stats_rx: tokio::sync::watch::Receiver<NodeStats>,
    node_task: tokio::task::JoinHandle<NodeStats>,
    edge_task: tokio::task::JoinHandle<()>,
    presence_task: tokio::task::JoinHandle<()>,
    maintainer_task: tokio::task::JoinHandle<()>,
    spawned_at: Instant,
    /// `(prev_head, when it last grew)` — updated lazily in [`StreamWorker::snapshot`].
    head_track: Mutex<(u64, Instant)>,
    dormant: AtomicBool,
}

impl StreamWorker {
    /// Join `stream`'s swarm as a `Role::Seed` node. `publisher_hint` carries the
    /// `(publisher key, seg_ms, manifest_cid)` from an already-VERIFIED recruitment
    /// (recruit.rs fetched + checked the manifest), so the node gets its gossip
    /// verification anchor and real part duration immediately instead of waiting for
    /// the maintainer's first verified candidate.
    pub fn spawn(
        stream: StreamId,
        canon_name: String,
        pinned: bool,
        publisher_hint: Option<([u8; 32], u32 /* seg_ms */, String /* manifest_cid */)>,
        initial_budget_bps: u64,
        stun: Vec<String>,
    ) -> Result<Self, String> {
        log::info!(
            "[seed] joining swarm for \"{canon_name}\" (stream {})",
            crypto::hex32(&stream.0)
        );

        let (node_tx, node_rx) = unbounded_channel::<EngineEvent>();
        let session = Session::start(stream, 1, stun, node_tx.clone())?;
        // A relay exists to be dialed — accept a wide inbound set.
        session.set_max_inbound(256);

        let cfg = MeshConfig {
            mode: Mode::Live,
            role: Role::Seed,
            window: 64,
            tick: TICK,
            seg_ms: 1000, // retimed by SetSegMs once the verified manifest arrives
            upload_budget_bps: initial_budget_bps,
            weights: Default::default(),
        };
        let (stats_tx, stats_rx) =
            tokio::sync::watch::channel(unstation_core::node::NodeStats::default());
        let node = MeshNode::new_seed(session.my_peer, cfg, SEG_BYTES, HashMap::new(), 0)
            .with_stream_id(stream.0)
            .with_presence_book(session.presence_book())
            .with_ban_list(session.ban_list())
            .with_stats(stats_tx);
        let node_task = tokio::spawn(node.run(node_rx, TICK, None));

        // A recruited worker already carries a VERIFIED manifest (recruit.rs fetched
        // and checked it against the recruitment's publisher) — hand the node its
        // trust anchor + part duration now; the trust gate below still re-verifies
        // whatever candidates discovery turns up.
        let publisher = publisher_hint.as_ref().map(|(pk, _, _)| *pk);
        if let Some((pk, seg_ms, _cid)) = &publisher_hint {
            let _ = node_tx.send(EngineEvent::SetPublisherKey { key: *pk });
            if *seg_ms > 0 {
                let _ = node_tx.send(EngineEvent::SetSegMs(*seg_ms as u64));
            }
        }

        // Live edge from the chain (reconciliation; signed gossip covers steady state).
        let edge_task = session.spawn_edge_poller(node_tx.clone());

        // Announce ourselves WITH relay opt-in: volunteering reachable uplink is this
        // binary's entire purpose. The gate stays on — a dedicated seed has no player
        // whose experience could degrade; peers that find it slow will deprioritize it.
        let always_healthy = Arc::new(AtomicBool::new(true));
        let presence_task = session.spawn_presence(initial_budget_bps, true, always_healthy);

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

        let now = Instant::now();
        Ok(Self {
            stream,
            canon_name,
            pinned,
            publisher,
            session,
            node_tx,
            stats_rx,
            node_task,
            edge_task,
            presence_task,
            maintainer_task,
            spawned_at: now,
            head_track: Mutex::new((0, now)),
            dormant: AtomicBool::new(false),
        })
    }

    /// Current state for the supervisor. Head-advance tracking is lazy: the watch
    /// channel always holds the latest stats, so comparing here (once per sweep) is
    /// as fresh as the policy needs.
    pub fn snapshot(&self) -> WorkerSnapshot {
        let ns = self.stats_rx.borrow().clone();
        let last_head_advance = {
            let mut track = self.head_track.lock().unwrap_or_else(|e| e.into_inner());
            if ns.head_seq > track.0 {
                *track = (ns.head_seq, Instant::now());
            }
            track.1
        };
        WorkerSnapshot {
            stream: self.stream,
            canon_name: self.canon_name.clone(),
            pinned: self.pinned,
            peers: self.session.peer_count(),
            head_seq: ns.head_seq,
            sent_bytes: ns.sent_bytes,
            hash_failures: ns.hash_failures,
            spawned_at: self.spawned_at,
            last_head_advance,
            dormant: self.dormant.load(Ordering::Relaxed),
        }
    }

    /// Retarget this worker's uplink budget (the policy rebalances every sweep).
    pub fn set_budget(&self, bps: u64) {
        let _ = self.node_tx.send(EngineEvent::SetUploadBudget(bps));
    }

    /// Park/unpark the session's chain-poll cadence (see [`Session::set_dormant`]).
    /// Safe to leave set while serving — connected peers always win over the flag.
    pub fn set_dormant(&self, on: bool) {
        self.session.set_dormant(on);
        self.dormant.store(on, Ordering::Relaxed);
    }

    /// Tear down cleanly (close PCs so peers fail over fast) — mirrors the old
    /// ctrl-c block: Stop the engine, abort the service tasks, shut the transport,
    /// and give the node task 3s to drain.
    pub async fn shutdown(self) {
        let _ = self.node_tx.send(EngineEvent::Stop);
        self.edge_task.abort();
        self.presence_task.abort();
        self.maintainer_task.abort();
        self.session.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.node_task).await;
    }
}
