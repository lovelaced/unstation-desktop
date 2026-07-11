//! The open-relay supervisor: owns every [`StreamWorker`], announces spare capacity
//! on the volunteer rendezvous, admits verified recruitments through the pure
//! [`policy`] layer, and sweeps workers between serving / dormant / evicted.
//!
//! Pinned streams (`--stream`) are the operator's explicit choice: they are spawned
//! up-front and never evicted, only parked while nobody watches. Recruited streams
//! come and go entirely by policy.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::unbounded_channel;
use unstation_core::volunteer::{RecruitAction, VolunteerRecord, VOLUNTEER_VERSION};

use crate::policy::{self, PolicyCfg, Verdict, WorkerView};
use crate::recruit::{self, VerifiedRecruitment};
use crate::streams;
use crate::worker::{StreamWorker, WorkerSnapshot};

/// Policy sweep + heartbeat cadence (the old single-stream heartbeat's 10 s).
const SWEEP: Duration = Duration::from_secs(10);
/// Volunteer re-announce cadence; comfortably inside the record's TTL.
const ANNOUNCE_PERIOD: Duration = Duration::from_secs(120);
/// Announce validity window — two announce periods, so one missed write doesn't
/// drop us out of the rendezvous.
const VOLUNTEER_TTL_S: u32 = 240;

/// One managed worker + the supervisor-side state policy needs about it.
struct Entry {
    worker: StreamWorker,
    /// The publisher posted a Release (drain, short idle clock — never hard-kill).
    released: bool,
    /// When this worker last dropped to zero peers (None while any peer is on).
    /// Reset by an LRU refresh (re-recruitment): the publisher just re-claimed it.
    zero_since: Option<Instant>,
    last_budget: u64,
    /// `sent_bytes` at the previous sweep, for the heartbeat's uplink delta.
    prev_sent: u64,
}

impl Entry {
    fn new(worker: StreamWorker, initial_budget: u64) -> Self {
        Self {
            worker,
            released: false,
            zero_since: Some(Instant::now()),
            last_budget: initial_budget,
            prev_sent: 0,
        }
    }
}

pub struct Supervisor {
    cfg: PolicyCfg,
    /// Stream NAMES to pin (canonicalized + resolved at spawn).
    pins: Vec<String>,
    /// Open relay: announce on the rendezvous and accept recruitments.
    open: bool,
    stun: Vec<String>,
}

impl Supervisor {
    pub fn new(cfg: PolicyCfg, pins: Vec<String>, open: bool, stun: Vec<String>) -> Self {
        Self { cfg, pins, open, stun }
    }

    /// Run until ctrl-c: spawn the pins, then serve recruitments + sweeps.
    pub async fn run(self) {
        let Self { cfg, pins, open, stun } = self;
        let mut entries: BTreeMap<[u8; 32], Entry> = BTreeMap::new();

        // ---- pinned workers, up-front (dedup: two spellings of one canonical name
        // are the same swarm) ----
        let mut canons: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for name in &pins {
            let canon = streams::canonical_stream_name(name);
            if seen.insert(canon.clone()) {
                canons.push(canon);
            }
        }
        let pin_budget = if canons.is_empty() {
            cfg.min_stream_budget_bps
        } else {
            (cfg.total_budget_bps / canons.len() as u64).max(cfg.min_stream_budget_bps)
        };
        for canon in canons {
            let stream = streams::stream_id_from(&canon);
            match StreamWorker::spawn(stream, canon, true, None, pin_budget, stun.clone()) {
                Ok(w) => {
                    entries.insert(stream.0, Entry::new(w, pin_budget));
                }
                Err(e) => {
                    // Same posture as the old single-stream boot: a pin that can't
                    // start is a config/transport problem — fail loud, let the
                    // service manager retry.
                    eprintln!("session start failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        // ---- open-relay plumbing: recruit inbox + rendezvous announces ----
        let (recruit_tx, mut recruit_rx) = unbounded_channel::<VerifiedRecruitment>();
        let active = Arc::new(AtomicU32::new(entries.len() as u32));
        let mut service_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        if open {
            service_tasks.push(recruit::spawn_recruit_listener(recruit_tx));
            service_tasks.push(spawn_announce_loop(
                cfg.total_budget_bps,
                cfg.max_streams as u32,
                active.clone(),
            ));
        }
        // (!open: recruit_tx dropped here — the recv arm below never fires.)

        let mut sweep = tokio::time::interval(SWEEP);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                Some(vr) = recruit_rx.recv(), if open => {
                    on_recruitment(vr, &mut entries, &cfg, &stun).await;
                }
                _ = sweep.tick() => {
                    sweep_once(&mut entries, &cfg, open).await;
                }
            }
            active.store(entries.len() as u32, Ordering::Relaxed);
        }

        // ---- teardown: leave the rendezvous, then close every swarm cleanly ----
        log::info!("[seed] shutting down");
        for t in service_tasks {
            t.abort();
        }
        if open {
            withdraw(cfg.total_budget_bps, cfg.max_streams as u32).await;
        }
        let mut closing = Vec::new();
        for (_, e) in entries {
            closing.push(tokio::spawn(e.worker.shutdown()));
        }
        for c in closing {
            let _ = c.await;
        }
    }
}

/// One verified recruitment. Recruit → admit by policy (spawning, possibly after
/// bumping an idle worker); Release → mark the worker draining. Both are advisory:
/// the sweep owns actual evictions.
async fn on_recruitment(
    vr: VerifiedRecruitment,
    entries: &mut BTreeMap<[u8; 32], Entry>,
    cfg: &PolicyCfg,
    stun: &[String],
) {
    match vr.action {
        RecruitAction::Release => {
            match entries.get_mut(&vr.stream.0) {
                // Only the publisher that owns the worker can release it — a Release
                // carries no manifest proof, so a third-party signature must not be
                // able to drain someone else's stream off us (pins have no publisher
                // and can never be released).
                Some(e) if e.worker.publisher == Some(vr.publisher) => {
                    e.released = true;
                    log::info!(
                        "[seed] release for stream={} — draining (evicts once idle)",
                        e.worker.canon_name
                    );
                }
                Some(_) => log::debug!(
                    "[seed] ignoring release for {} from a non-owning publisher",
                    vr.canon_hint
                ),
                None => log::debug!("[seed] release for {} — not serving it", vr.canon_hint),
            }
        }
        RecruitAction::Recruit => {
            if let Some(e) = entries.get_mut(&vr.stream.0) {
                // Already serving: treat the re-recruitment as an LRU refresh — the
                // publisher still wants us, so clear any drain mark and restart the
                // idle clock.
                e.released = false;
                if e.zero_since.is_some() {
                    e.zero_since = Some(Instant::now());
                }
                log::debug!(
                    "[seed] recruit refresh for stream={} (issued_at={})",
                    e.worker.canon_name,
                    vr.issued_at
                );
                return;
            }
            let (_, views) = collect_views(entries);
            match policy::admit(vr.publisher, vr.stream.0, &views, cfg) {
                policy::Admit::Accept => {}
                policy::Admit::AcceptEvicting(victim) => {
                    if let Some(e) = entries.remove(&victim) {
                        log::info!(
                            "[seed] evicting stream={} reason=bumped (making room for a recruitment)",
                            e.worker.canon_name
                        );
                        e.worker.shutdown().await;
                    }
                }
                policy::Admit::Reject(reason) => {
                    log::debug!("[seed] recruitment for {} rejected: {reason}", vr.canon_hint);
                    return;
                }
            }
            let spawned = StreamWorker::spawn(
                vr.stream,
                vr.canon_hint.clone(),
                false,
                Some((vr.publisher, vr.seg_ms, vr.manifest_cid.clone())),
                cfg.min_stream_budget_bps,
                stun.to_vec(),
            );
            match spawned {
                Ok(w) => {
                    entries.insert(vr.stream.0, Entry::new(w, cfg.min_stream_budget_bps));
                    rebalance(entries, cfg);
                }
                Err(e) => {
                    log::warn!("[seed] worker spawn failed for {}: {e}", vr.canon_hint);
                }
            }
        }
    }
}

/// The 10 s sweep: snapshot everyone, apply policy (evict / park / wake), rebalance
/// budgets, and emit the heartbeat.
async fn sweep_once(entries: &mut BTreeMap<[u8; 32], Entry>, cfg: &PolicyCfg, open: bool) {
    let (snaps, views) = collect_views(entries);

    // Verdicts first (immutable pass), then act.
    let mut evictions: Vec<([u8; 32], policy::EvictReason)> = Vec::new();
    for (view, (key, snap)) in views.iter().zip(&snaps) {
        match policy::evaluate(view, cfg) {
            Verdict::Evict(reason) => evictions.push((*key, reason)),
            Verdict::Dormant => {
                if !snap.dormant {
                    let e = &entries[key];
                    log::info!(
                        "[seed] stream={} dormant (no peers for {}s)",
                        e.worker.canon_name,
                        view.secs_at_zero_peers
                    );
                    e.worker.set_dormant(true);
                }
            }
            Verdict::Serve => {
                if snap.dormant {
                    entries[key].worker.set_dormant(false);
                }
            }
        }
    }
    for (key, reason) in evictions {
        if let Some(e) = entries.remove(&key) {
            log::info!("[seed] evicting stream={} reason={reason}", e.worker.canon_name);
            e.worker.shutdown().await;
        }
    }

    rebalance(entries, cfg);

    // ---- heartbeat: the numbers a volunteer wants to see, per stream ----
    let mut lines = Vec::with_capacity(entries.len());
    let mut peers_total = 0usize;
    let mut uplink_total_kbps = 0u64;
    let mut n_pinned = 0usize;
    for ((key, snap), view) in snaps.iter().zip(&views) {
        let Some(e) = entries.get_mut(key) else { continue }; // evicted this sweep
        let uplink_kbps = snap.sent_bytes.saturating_sub(e.prev_sent) * 8 / SWEEP.as_secs() / 1000;
        e.prev_sent = snap.sent_bytes;
        peers_total += snap.peers;
        uplink_total_kbps += uplink_kbps;
        n_pinned += snap.pinned as usize;
        let state = match policy::evaluate(view, cfg) {
            Verdict::Dormant => "dormant",
            _ => "serving",
        };
        lines.push(format!(
            "[seed] stream={}{} peers={} cached_head={} uplink={}kbps served_total={}MB verify_fail={} state={state}",
            snap.canon_name,
            if snap.pinned { " pinned" } else { "" },
            snap.peers,
            snap.head_seq,
            uplink_kbps,
            snap.sent_bytes / 1_000_000,
            snap.hash_failures,
        ));
    }
    log::info!(
        "[seed] streams={}/{} ({} pinned) peers_total={} uplink={}kbps budget={}Mbps open={} chain_write_fail={}",
        entries.len(),
        cfg.max_streams,
        n_pinned,
        peers_total,
        uplink_total_kbps,
        cfg.total_budget_bps / 1_000_000,
        open,
        unstation_chain::chain_write_failures(),
    );
    for line in lines {
        log::info!("{line}");
    }
}

/// Snapshot every worker, roll the zero-peer clocks forward, and build the policy
/// views. One shared path so admission and the sweep judge identical state.
fn collect_views(
    entries: &mut BTreeMap<[u8; 32], Entry>,
) -> (Vec<([u8; 32], WorkerSnapshot)>, Vec<WorkerView>) {
    let now = Instant::now();
    let mut snaps = Vec::with_capacity(entries.len());
    let mut views = Vec::with_capacity(entries.len());
    for e in entries.values_mut() {
        let snap = e.worker.snapshot();
        if snap.peers == 0 {
            e.zero_since.get_or_insert(now);
        } else {
            e.zero_since = None;
        }
        views.push(WorkerView {
            stream: snap.stream.0,
            publisher: e.worker.publisher,
            pinned: snap.pinned,
            peers: snap.peers,
            head_seq: snap.head_seq,
            secs_since_head_advance: now.duration_since(snap.last_head_advance).as_secs(),
            secs_since_spawn: now.duration_since(snap.spawned_at).as_secs(),
            secs_at_zero_peers: e
                .zero_since
                .map(|t| now.duration_since(t).as_secs())
                .unwrap_or(0),
            released: e.released,
        });
        snaps.push((snap.stream.0, snap));
    }
    (snaps, views)
}

/// Apply the policy's budget split, sending `SetUploadBudget` only on change.
fn rebalance(entries: &mut BTreeMap<[u8; 32], Entry>, cfg: &PolicyCfg) {
    let (_, views) = collect_views(entries);
    for (stream, bps) in policy::budgets(&views, cfg) {
        if let Some(e) = entries.get_mut(&stream) {
            if e.last_budget != bps {
                e.worker.set_budget(bps);
                e.last_budget = bps;
            }
        }
    }
}

/// The rendezvous announce loop: every [`ANNOUNCE_PERIOD`], publish a fresh
/// [`VolunteerRecord`] under the process-stable `local_peer_id()` — the address of
/// our recruit inbox. (Workers dial under their own per-session ids; unrelated.)
fn spawn_announce_loop(
    caps_upload_bps: u64,
    max_streams: u32,
    active: Arc<AtomicU32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(ANNOUNCE_PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(rec) =
                volunteer_record(caps_upload_bps, max_streams, active.load(Ordering::Relaxed))
            else {
                log::warn!("[seed] skipping volunteer announce — identity not fully initialized");
                continue;
            };
            if let Err(e) = unstation_chain::volunteer::publish_volunteer(rec).await {
                log::warn!("[seed] volunteer announce failed: {e:?}");
            }
        }
    })
}

/// Best-effort rendezvous withdrawal on shutdown (bounded — teardown must not hang
/// on a dead chain link).
async fn withdraw(caps_upload_bps: u64, max_streams: u32) {
    let Some(rec) = volunteer_record(caps_upload_bps, max_streams, 0) else { return };
    match tokio::time::timeout(
        Duration::from_secs(5),
        unstation_chain::volunteer::withdraw_volunteer(rec),
    )
    .await
    {
        Ok(Ok(())) => log::info!("[seed] withdrew from the volunteer rendezvous"),
        Ok(Err(e)) => log::warn!("[seed] rendezvous withdrawal failed: {e:?}"),
        Err(_) => log::warn!("[seed] rendezvous withdrawal timed out"),
    }
}

/// Our current rendezvous record; `None` if any identity piece is missing (the
/// caller warns and skips the cycle rather than announcing an unreachable inbox).
fn volunteer_record(
    caps_upload_bps: u64,
    max_streams: u32,
    active_streams: u32,
) -> Option<VolunteerRecord> {
    Some(VolunteerRecord {
        version: VOLUNTEER_VERSION,
        peer_id: unstation_chain::local_peer_id()?.0,
        account: unstation_chain::identity_public()?,
        enc_pub: unstation_chain::identity_enc_public()?,
        caps_upload_bps,
        active_streams,
        max_streams,
        ttl_s: VOLUNTEER_TTL_S,
        issued_at: unix_now(),
    })
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
