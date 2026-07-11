//! Session orchestrator — the layer that turns the proven mesh engine into a real
//! networked app.
//!
//! It owns the real chain signaling ([`unstation_chain::ChainSignaling`]) and the
//! real WebRTC transport ([`transport_libdc::LibDcTransport`]) and drives the
//! publish/watch bootstrap over the Polkadot statement store:
//!
//! * **publish** — announce presence on the discovery topic, republish the
//!   live-edge manifest, and accept incoming peers (answering their offers).
//! * **watch** — discover a publisher by name, dial it over WebRTC, and poll the
//!   live-edge manifest into `LiveEdge` events so the node knows what to fetch.
//!
//! All SDP/ICE crosses the chain via [`unstation_chain::ChainSignaling`]
//! (sender-tagged envelopes); the resulting [`Link`]s are fed to the `MeshNode`
//! through its normal `EngineEvent` inbox, so the engine is unchanged.
//!
//! [`Link`]: unstation_core::transport::Link

mod dialer;
pub use dialer::{Dialer, DIAL_TIMEOUT};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, sleep};

use transport_libdc::{LibDcTransport, SignalOut, TransportEvent};
use unstation_chain::ChainSignaling;
use unstation_core::node::EdgeSigner;
use unstation_core::signaling::{BanList, Presence, PresenceBook, PresenceRecord, Signaling, SignalMsg};
use unstation_core::topic::discovery_topic;
use unstation_core::transport::EngineEvent;
use unstation_core::types::{PeerId, SegmentId, Seq, StreamId};

/// Signaling / edge poll cadence. ACTIVE while still establishing the mesh (no peers,
/// or a handshake in flight — SDP/ICE and the first edge must arrive fast); IDLE once
/// connected, where a slower reconciliation poll suffices (the live edge propagates in
/// mesh via signed gossip, so the chain read is only a fallback). Cuts steady-state
/// chain reads ~5× — the scarce statement-store slot budget is the real constraint.
const SIGNAL_POLL_ACTIVE: Duration = Duration::from_millis(800);
const SIGNAL_POLL_IDLE: Duration = Duration::from_secs(4);
/// DORMANT cadence: a stream-agnostic volunteer seed parked with zero peers (waiting
/// for a recruitment, or between assigned streams) has no handshake in flight and
/// nothing to reconcile — stretch the reconciliation polls to a heartbeat. Safe
/// because the push wakeups stay live (see [`Session::set_dormant`]); off by default.
const SIGNAL_POLL_DORMANT: Duration = Duration::from_secs(30);
const DISCOVERY_POLL: Duration = Duration::from_secs(2);
/// Maintainer re-evaluation cadence absent any transport event.
const MAINTAIN_TICK: Duration = Duration::from_secs(1);
/// Maintainer cadence while dormant with zero peers (see [`SIGNAL_POLL_DORMANT`]).
const MAINTAIN_TICK_DORMANT: Duration = Duration::from_secs(30);
const PRESENCE_REFRESH: Duration = Duration::from_secs(10);
const EDGE_REFRESH: Duration = Duration::from_secs(2);
const PRESENCE_TTL_S: u32 = 30;
const EDGE_WINDOW: usize = 64;

/// Signs live-edge gossip with the host's on-chain identity — the SAME sr25519 key as
/// the signed manifest + presence. Injected into the publisher's `MeshNode` (via
/// [`MeshNode::with_edge_signer`]) so the secret never leaves the chain layer; viewers
/// verify each gossiped edge against the publisher's pubkey. Off-chain signaling
/// (TECH_SPEC §6.4).
pub struct IdentityEdgeSigner;
impl EdgeSigner for IdentityEdgeSigner {
    fn sign(&self, payload: &[u8]) -> [u8; 64] {
        // Publishing always implies a signed-in identity; the all-zero fallback (which
        // viewers reject) only guards the impossible "publisher without identity" case.
        unstation_chain::sign_with_identity(payload).unwrap_or([0u8; 64])
    }
}

/// peer id → (chain-verified signer account, last seen), shared between the discovery
/// writer and the shield gate — see [`Session::chain_relay_accounts`].
type ChainRelayAccounts = Arc<Mutex<HashMap<[u8; 32], ([u8; 32], Instant)>>>;

/// A running session: the chain-signaling + WebRTC plumbing for one stream.
#[derive(Clone)]
pub struct Session {
    pub stream: StreamId,
    pub my_peer: PeerId,
    pub n_shards: u32,
    signaling: ChainSignaling,
    transport: LibDcTransport,
    /// Publisher's signed-manifest Bulletin CID, announced in presence once the
    /// manifest is published (after the encoder's init segment exists). Shared so the
    /// presence-refresh loop picks it up when [`Session::set_manifest_cid`] is called.
    manifest_cid: Arc<Mutex<Option<String>>>,
    /// Off-chain presence directory (TECH_SPEC §7.3): shared with the `MeshNode`, which
    /// gossips it in-mesh. The session refreshes our own entry + dials from it, so plain
    /// viewers never write presence to the chain (only bootstrap anchors do).
    book: PresenceBook,
    /// Shared with the `MeshNode` (via [`MeshNode::with_ban_list`]): the node convicts
    /// misbehaving peers; the session enforces the ban at its edges — banned peers are
    /// never dialed and their offers are ignored.
    ///
    /// [`MeshNode::with_ban_list`]: unstation_core::node::MeshNode::with_ban_list
    bans: BanList,
    /// Origin-shield (privacy): when set, this session ANSWERS inbound offers only from
    /// relay-advertising peers (volunteer seeds), refusing plain viewers — so the
    /// publisher's IP is exposed only to the seed tier it feeds, never to the audience.
    /// Viewers refused here fall back to dialing seeds via the normal maintainer. Off by
    /// default; a publisher opts in. Shared so it can be toggled after `start`.
    shield: Arc<std::sync::atomic::AtomicBool>,
    /// Origin-shield allowlist: the chain ACCOUNTS (statement-store signers) this
    /// shielded session will answer — the volunteers the publisher itself recruited.
    /// Empty = legacy shield (any relay-flagged peer); see [`shield_admits`]. Set via
    /// [`Session::set_shield_allow`] as the recruiter's set changes.
    shield_allow: Arc<Mutex<HashSet<[u8; 32]>>>,
    /// peer id → (chain-verified signer account, last seen) for relay-flagged presence
    /// read from the CHAIN. SECURITY PROPERTY: written ONLY by
    /// [`Session::spawn_relay_discovery`] from statement proofs the chain verified —
    /// in-mesh gossip must NEVER write it, or an attacker could vouch a fake signer
    /// for its own peer id and walk through the shield allowlist.
    chain_relay_accounts: ChainRelayAccounts,
    /// Dormant mode (stream-agnostic volunteer seed): with zero connected peers, the
    /// signaling/edge reconciliation polls and the maintainer tick stretch to the
    /// `*_DORMANT` cadences — a parked seed shouldn't hammer the chain. Push wakeups
    /// stay live, so inbound work still lands promptly. Off by default; desktop and
    /// mobile never touch it. Same shared-toggle pattern as `shield`.
    dormant: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    /// Boot the statement store with `keypair`, derive our `PeerId` from it, build
    /// the WebRTC transport, and spawn the bidirectional signaling bridge
    /// (local SDP/ICE → chain, and chain → transport).
    ///
    /// `inbox` is the target `MeshNode`'s event channel: the transport posts
    /// `PeerConnected` / `Inbound` / `PeerDisconnected` there directly.
    pub fn start(
        stream: StreamId,
        n_shards: u32,
        stun: Vec<String>,
        inbox: UnboundedSender<EngineEvent>,
    ) -> Result<Self, String> {
        // The statement store must already be initialized with the paired identity
        // (see the host's `set_chain_identity`) — its key carries the on-chain
        // allowance to write presence/signaling. We must NOT (re)init here: the SDK
        // re-initializes on every init call, which would clobber the paired key with
        // a fresh, unprovisioned one.
        // Require a signed-in identity (its key carries the on-chain allowance to write
        // presence/signaling). We do NOT reuse its value as our PeerId, though:
        if unstation_chain::local_peer_id().is_none() {
            return Err("not signed in — pair the Polkadot app to publish or watch".into());
        }
        // Fresh per-SESSION routing id: a re-watch / publisher switch then dials as a NEW
        // peer the far side accepts at once, instead of colliding with our previous
        // (torn-down) session under a process-stable id. Trust stays on the personhood key.
        let my_peer = unstation_chain::fresh_device_peer_id();

        let (sig_tx, sig_rx) = unbounded_channel::<SignalOut>();
        let transport = LibDcTransport::new(stun, inbox, sig_tx)
            .map_err(|e| format!("couldn't start the connection engine: {e}"))?;
        let signaling = ChainSignaling::new(stream, n_shards);

        let bans = BanList::new();
        let book = PresenceBook::new();
        let shield = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shield_allow = Arc::new(Mutex::new(HashSet::new()));
        let chain_relay_accounts = Arc::new(Mutex::new(HashMap::new()));
        let dormant = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(relay_outbound(sig_rx, signaling.clone(), my_peer));
        tokio::spawn(relay_inbound(
            signaling.clone(),
            my_peer,
            transport.clone(),
            bans.clone(),
            book.clone(),
            shield.clone(),
            shield_allow.clone(),
            chain_relay_accounts.clone(),
            dormant.clone(),
        ));

        Ok(Self {
            stream,
            my_peer,
            n_shards: n_shards.max(1),
            signaling,
            transport,
            shield,
            shield_allow,
            chain_relay_accounts,
            dormant,
            manifest_cid: Arc::new(Mutex::new(None)),
            book,
            bans,
        })
    }

    /// The shared ban list — hand this to the `MeshNode` (via
    /// [`MeshNode::with_ban_list`]) so its convictions bar re-dials and offers here.
    ///
    /// [`MeshNode::with_ban_list`]: unstation_core::node::MeshNode::with_ban_list
    pub fn ban_list(&self) -> BanList {
        self.bans.clone()
    }

    /// Enable/disable origin-shield (see [`Session::shield`]). A publisher turns this on
    /// to answer only relay volunteers, keeping its IP off the audience's peer list.
    pub fn set_shield(&self, on: bool) {
        self.shield.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Replace the origin-shield allowlist with `accounts` — the chain accounts of the
    /// volunteers this publisher recruited (see [`Session::shield_allow`]). While the
    /// set is non-empty, a shielded session answers ONLY offers from peers whose
    /// chain-verified presence signer is in it; an empty set falls back to the legacy
    /// relay-flag gate. The app pushes the current set after every recruitment.
    pub fn set_shield_allow(&self, accounts: HashSet<[u8; 32]>) {
        *self.shield_allow.lock().unwrap_or_else(|e| e.into_inner()) = accounts;
    }

    /// Enable/disable dormant mode (see [`Session::dormant`]). A stream-agnostic
    /// volunteer seed turns this on while it has no assigned stream; the moment it has
    /// peers (or the flag is cleared) every cadence is back to normal.
    pub fn set_dormant(&self, on: bool) {
        self.dormant.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// A clone of the chain signaling handle — for the opt-in WebRTC media fast tier (W3),
    /// which drives its own `fast_signal` offer/answer over the statement store beside the
    /// mesh's data-channel negotiation. The mesh path is untouched.
    pub fn signaling(&self) -> ChainSignaling {
        self.signaling.clone()
    }

    /// The shared off-chain presence directory — hand this to the `MeshNode`
    /// (via [`MeshNode::with_presence_book`]) so node + session see the same peers.
    ///
    /// [`MeshNode::with_presence_book`]: unstation_core::node::MeshNode::with_presence_book
    pub fn presence_book(&self) -> PresenceBook {
        self.book.clone()
    }

    /// Close all WebRTC connections and stop the transport reactor. Call when abandoning
    /// this session (stop/re-watch): the reactor is kept alive by detached signaling tasks,
    /// so dropping the `Session` alone never closes the connections — and a publisher would
    /// keep our (stable) peer id connected and ignore a re-watch's new offer.
    pub fn shutdown(&self) {
        self.transport.shutdown();
    }

    /// Publisher: announce presence on our discovery shard, refreshed before TTL. Returns
    /// the task handle so the caller can abort it on teardown — otherwise it would keep
    /// refreshing this (now-stale) session's presence forever.
    ///
    /// `relay_gate` is the live health switch (seed-by-default): when the app's health
    /// monitor detects an unstable/slow link it flips the gate off, and this node stops
    /// advertising relay capability regardless of opt-in or proven reachability — an
    /// unhealthy relay hurts the mesh more than no relay.
    pub fn spawn_presence(
        &self,
        caps_upload_bps: u64,
        relay_opt_in: bool,
        relay_gate: Arc<std::sync::atomic::AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let signaling = self.signaling.clone();
        let me = self.my_peer;
        let manifest_cid = self.manifest_cid.clone();
        let transport = self.transport.clone();
        let book = self.book.clone();
        tokio::spawn(async move {
            let mut tick = interval(PRESENCE_REFRESH);
            loop {
                tick.tick().await;
                let mc = manifest_cid.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // Advertise relay-capability if healthy AND (explicitly opted in OR
                // we've proven reachable — a peer connected to us inbound).
                let relay = relay_gate.load(std::sync::atomic::Ordering::Relaxed)
                    && (relay_opt_in || transport.reachable());
                // `peer_id` is our per-device routing id; `publisher` is our stable
                // personhood key — the trust anchor viewers verify the manifest/edge
                // against. Splitting them lets two devices of the same person coexist in
                // the mesh (they share `publisher` but differ in `peer_id`).
                let publisher = unstation_chain::identity_public().unwrap_or(me.0);
                // Advertise our X25519 signaling key so peers seal SDP/ICE to us (Tier 0
                // privacy). Zero if no identity yet — a dialer then can't seal to us and
                // won't leak; but presence is only written once signed in, so this is set.
                let enc_pub = unstation_chain::identity_enc_public().unwrap_or([0u8; 32]);
                let p = Presence { peer_id: me, publisher, caps_upload_bps, ttl_s: PRESENCE_TTL_S, manifest_cid: mc, relay, enc_pub };
                // Off-chain presence (TECH_SPEC §7.3): always refresh our own entry in the
                // in-mesh book so neighbors gossip us onward. Only ANCHORS (publishers +
                // reachable relay volunteers) ALSO write to the chain — the bootstrap set a
                // cold joiner reads. Plain viewers skip the chain write, taking presence
                // from O(viewers) writes down to O(anchors).
                book.insert(PresenceRecord::from(&p));
                if relay {
                    if let Err(e) = signaling.publish_presence(p).await {
                        log::warn!("[session] publish_presence: {e}");
                    }
                }
            }
        })
    }

    /// Publisher: set the signed-manifest Bulletin CID announced in presence. Call once
    /// the manifest has been published to Bulletin (after the encoder's init segment
    /// exists); the presence-refresh loop picks it up on its next tick.
    pub fn set_manifest_cid(&self, cid: String) {
        *self.manifest_cid.lock().unwrap_or_else(|e| e.into_inner()) = Some(cid);
    }

    /// Publisher: republish the live-edge manifest as new `(seq, content-id)` pairs
    /// are produced by the segmenter (drained from `edge_rx`). Returns the task handle
    /// so the caller can abort it on teardown — otherwise a stopped stream's edge
    /// window would keep republishing to the chain forever.
    pub fn spawn_edge_publisher(
        &self,
        mut edge_rx: UnboundedReceiver<(Seq, SegmentId)>,
    ) -> tokio::task::JoinHandle<()> {
        let signaling = self.signaling.clone();
        tokio::spawn(async move {
            let mut window: BTreeMap<Seq, SegmentId> = BTreeMap::new();
            let mut tick = interval(EDGE_REFRESH);
            loop {
                tick.tick().await;
                while let Ok((seq, id)) = edge_rx.try_recv() {
                    window.insert(seq, id);
                    while window.len() > EDGE_WINDOW {
                        if let Some(&oldest) = window.keys().next() {
                            window.remove(&oldest);
                        }
                    }
                }
                if !window.is_empty() {
                    let entries: Vec<(Seq, SegmentId)> =
                        window.iter().map(|(s, i)| (*s, *i)).collect();
                    if let Err(e) = signaling.publish_edge(entries).await {
                        log::warn!("[session] publish_edge: {e}");
                    }
                }
            }
        })
    }

    /// Origin-shield support: periodically read the stream's discovery shards and admit
    /// relay-capable anchors (the volunteer seeds) into the presence book, so
    /// [`relay_inbound`]'s shield gate can recognize a seed's offer. A publisher does not
    /// otherwise do discovery (it is the answerer), so without this its book holds only
    /// itself and shield would refuse everyone. Also caches each seed's signaling key,
    /// and — for the hardened allowlist gate — records each relay peer's CHAIN-VERIFIED
    /// signer account. This task is the ONLY writer of that map (see
    /// [`Session::chain_relay_accounts`]): the signer comes from the statement proof the
    /// chain checked, never from gossip. Only needed while shielded; spawn it alongside
    /// `set_shield(true)`.
    pub fn spawn_relay_discovery(&self) -> tokio::task::JoinHandle<()> {
        let signaling = self.signaling.clone();
        let book = self.book.clone();
        let chain_relay_accounts = self.chain_relay_accounts.clone();
        let stream = self.stream;
        let n_shards = self.n_shards;
        tokio::spawn(async move {
            let mut tick = interval(DISCOVERY_POLL);
            loop {
                tick.tick().await;
                for shard in 0..n_shards {
                    let topic = discovery_topic(&stream, shard);
                    if let Ok(list) = signaling.read_presence_signed(topic, 32).await {
                        for (p, signer) in list {
                            if p.relay {
                                signaling.note_enc_key(p.peer_id, p.enc_pub);
                                book.insert(PresenceRecord::from(&p));
                                if let Some(signer) = signer {
                                    chain_relay_accounts
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .insert(p.peer_id.0, (signer, Instant::now()));
                                }
                            }
                        }
                    }
                }
                // Prune signer records the chain has stopped delivering (~3 ticks): a
                // retired presence statement must not vouch for its peer id forever.
                let ttl = 3 * DISCOVERY_POLL;
                chain_relay_accounts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|_, (_, seen)| seen.elapsed() <= ttl);
            }
        })
    }

    /// Publisher: republish the rolling durable-copy map — `(seq → Bulletin CID)` for
    /// the sparse segments the app uploads to the durable floor (TECH_SPEC §8.6).
    /// Mirrors [`Session::spawn_edge_publisher`]; abort the handle on teardown.
    pub fn spawn_durable_publisher(
        &self,
        mut cid_rx: UnboundedReceiver<(Seq, String)>,
    ) -> tokio::task::JoinHandle<()> {
        const DURABLE_WINDOW: usize = 16; // ~1 KB of CIDs — well inside a statement
        let signaling = self.signaling.clone();
        tokio::spawn(async move {
            let mut window: BTreeMap<Seq, String> = BTreeMap::new();
            let mut tick = interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let mut dirty = false;
                while let Ok((seq, cid)) = cid_rx.try_recv() {
                    window.insert(seq, cid);
                    dirty = true;
                    while window.len() > DURABLE_WINDOW {
                        if let Some(&oldest) = window.keys().next() {
                            window.remove(&oldest);
                        }
                    }
                }
                if dirty && !window.is_empty() {
                    let entries: Vec<(Seq, String)> =
                        window.iter().map(|(s, c)| (*s, c.clone())).collect();
                    if let Err(e) = signaling.publish_durable(entries).await {
                        log::warn!("[session] publish_durable: {e}");
                    }
                }
            }
        })
    }

    /// Viewer: the current durable-copy map (seq → Bulletin CID) for this stream.
    pub async fn read_durable(&self) -> unstation_core::Result<Vec<(Seq, String)>> {
        self.signaling.read_durable().await
    }

    /// Viewer: poll presence across all discovery shards until a publisher (any peer
    /// that isn't us) appears, and return its full presence record — including the
    /// signed-manifest CID the viewer fetches + verifies before trusting the stream.
    pub async fn discover(&self) -> Presence {
        loop {
            for shard in 0..self.n_shards {
                let topic = discovery_topic(&self.stream, shard);
                if let Ok(list) = self.signaling.read_presence(topic, 32).await {
                    if let Some(p) = list.into_iter().find(|p| p.peer_id != self.my_peer) {
                        return p;
                    }
                }
            }
            sleep(DISCOVERY_POLL).await;
        }
    }

    /// Viewer: like [`Session::discover`] but returns only the publisher's `PeerId`
    /// (for callers that don't need the manifest CID).
    pub async fn discover_publisher(&self) -> PeerId {
        self.discover().await.peer_id
    }

    /// Viewer: a one-shot snapshot of up to `max` candidate peers (anyone that isn't us)
    /// across the discovery shards, deduped. The dial loop tries several, so a
    /// NAT-restricted node only needs to reach *one* — the swarm relays the rest, with
    /// no central relay required. Returns whatever is present right now (possibly empty).
    pub async fn discover_peers(&self, max: usize) -> Vec<Presence> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        // Chain anchors (publishers + relay volunteers) — the bootstrap entry points a
        // cold joiner needs before it has reached the mesh.
        for shard in 0..self.n_shards {
            let topic = discovery_topic(&self.stream, shard);
            if let Ok(list) = self.signaling.read_presence(topic, 32).await {
                for p in list {
                    if p.peer_id != self.my_peer && seen.insert(p.peer_id) {
                        out.push(p);
                    }
                }
            }
        }
        // In-mesh presence (TECH_SPEC §7.3): peers learned via gossip once we've reached
        // the swarm — these incur NO chain read, and most plain viewers appear ONLY here.
        for rec in self.book.snapshot() {
            let p: Presence = rec.into();
            if p.peer_id != self.my_peer && seen.insert(p.peer_id) {
                out.push(p);
            }
        }
        // Never hand back a peer the node has convicted — a banned peer that keeps
        // announcing presence would otherwise be redialed forever.
        out.retain(|p| !self.bans.contains(&p.peer_id));
        // Cache each candidate's signaling key BEFORE we dial it: the offer we send is
        // sealed to this key (Tier 0). Learned here (from presence) for the outbound
        // direction; the answerer learns ours from the offer envelope it opens.
        for p in &out {
            self.signaling.note_enc_key(p.peer_id, p.enc_pub);
        }
        // Relay-capable volunteers first: a NAT-restricted node should try the reliable
        // bridges before random peers (the decentralized stand-in for a TURN server).
        out.sort_by_key(|p| !p.relay);
        out.truncate(max);
        log::info!("[session] discover_peers → {} candidate(s)", out.len());
        out
    }

    /// Viewer: open a WebRTC connection to a discovered publisher. The link
    /// arrives at the node inbox as `PeerConnected` once both channels open.
    pub fn dial(&self, publisher: PeerId) {
        self.transport.dial(publisher);
    }

    /// Raise/lower the transport's inbound-offer admission cap (publishers serve
    /// far more inbound viewers than a plain viewer ever holds).
    pub fn set_max_inbound(&self, n: usize) {
        self.transport.set_max_inbound(n);
    }

    /// Viewer: own the connection lifecycle — keep `target_degree` connections
    /// healthy, react to a drop the moment it happens (no polling lag), pace
    /// per-peer retries with exponential backoff + jitter, and abandon dials that
    /// hang mid-handshake so the transport's glare guard frees up.
    ///
    /// `filter` is the app's async trust gate (e.g. verify a publisher's signed
    /// manifest): a discovered candidate is dialed only if it returns true. Banned
    /// peers never reach it (`discover_peers` already screens them).
    pub fn spawn_maintainer(
        &self,
        target_degree: usize,
        filter: Arc<dyn Fn(Presence) -> unstation_core::BoxFuture<'static, bool> + Send + Sync>,
    ) -> tokio::task::JoinHandle<()> {
        let transport = self.transport.clone();
        let session = self.clone();
        let dormant = self.dormant.clone();
        let dialer = Dialer::new();
        let (ev_tx, mut ev_rx) = unbounded_channel::<TransportEvent>();
        transport.set_event_sink(ev_tx);
        tokio::spawn(async move {
            // Live connections, from the transport's own lifecycle events. The maintainer
            // MUST consult this before dialing or abandoning: re-dialing a peer that is
            // already connected is silently ignored by the transport's glare guard, but
            // it used to leave a phantom "in-flight" entry in the dialer — and 12 s later
            // the stalled sweep would `close()` that peer, KILLING the live, working
            // connection. On-device that was a perpetual ~25 s connect→kill→re-dial loop
            // (27 re-dials of an already-connected publisher in one 6-minute watch):
            // playback drained during every dark window, the UI flapped LIVE→Connecting,
            // and viewers drifted >6 s behind live. Never dial into, never abandon, a
            // connected peer.
            let mut connected: std::collections::HashSet<PeerId> = std::collections::HashSet::new();
            loop {
                // Abandon dials that hung mid-handshake (lost signaling / ICE failure):
                // close them so a fresh dial isn't blocked by the duplicate guard, and
                // let the backoff schedule own the retry.
                for p in dialer.stalled() {
                    if connected.contains(&p) {
                        // Not a stall — a dial recorded against a peer that (already or
                        // meanwhile) connected. Clear it instead of closing the link.
                        dialer.record_connected(&p);
                        continue;
                    }
                    log::info!("[session] dial to {p:?} timed out — abandoning");
                    transport.close(p);
                    dialer.record_failed(p);
                }
                // Top up toward the target degree, budgeted so one sweep never dials
                // more candidates than the connections it's actually missing (+1 hedge).
                let have = transport.peer_count();
                if have < target_degree {
                    let mut budget = target_degree - have + 1;
                    for cand in session.discover_peers(16).await {
                        if budget == 0 || transport.peer_count() >= target_degree {
                            break;
                        }
                        if connected.contains(&cand.peer_id) {
                            continue; // already live — see the header comment
                        }
                        if !dialer.should_dial(&cand.peer_id) {
                            continue;
                        }
                        let peer = cand.peer_id;
                        if !(filter)(cand).await {
                            continue;
                        }
                        log::info!("[session] maintainer dialing {peer:?}");
                        dialer.record_started(peer);
                        transport.dial(peer);
                        budget -= 1;
                    }
                }
                // Sleep until something changes: a lifecycle event (a drop triggers an
                // immediate re-evaluation — this kills the old fixed reconnect lag) or
                // the periodic tick (ages stalled dials + retries after backoff). A
                // DORMANT session with zero peers stretches the tick to the heartbeat
                // cadence — safe because lifecycle events still wake this select
                // immediately, and a parked seed has no stalled dials or backoffs for
                // the tick to age (per the poll-cadence header comment, the tick is
                // reconciliation only).
                let tick = if dormant.load(std::sync::atomic::Ordering::Relaxed)
                    && transport.peer_count() == 0
                {
                    MAINTAIN_TICK_DORMANT
                } else {
                    MAINTAIN_TICK
                };
                tokio::select! {
                    ev = ev_rx.recv() => match ev {
                        Some(TransportEvent::Connected(p)) => { connected.insert(p); dialer.record_connected(&p); }
                        Some(TransportEvent::Disconnected(p)) => { connected.remove(&p); dialer.record_failed(p); }
                        None => break,
                    },
                    _ = sleep(tick) => {}
                }
                // Coalesce any burst before re-evaluating.
                while let Ok(ev) = ev_rx.try_recv() {
                    match ev {
                        TransportEvent::Connected(p) => { connected.insert(p); dialer.record_connected(&p); }
                        TransportEvent::Disconnected(p) => { connected.remove(&p); dialer.record_failed(p); }
                    }
                }
            }
        })
    }

    /// Tear down a peer connection (e.g. to abandon a stalled dial before retrying —
    /// the transport ignores a re-`dial` while the peer entry still exists).
    pub fn close(&self, peer: PeerId) {
        self.transport.close(peer);
    }

    /// Number of peers currently connected over real WebRTC (a live UI stat).
    pub fn peer_count(&self) -> usize {
        self.transport.peer_count()
    }

    /// Viewer: poll the live-edge manifest into `LiveEdge` events so the node
    /// learns which segments exist and the hash to verify each against.
    pub fn spawn_edge_poller(&self, inbox: UnboundedSender<EngineEvent>) -> tokio::task::JoinHandle<()> {
        let signaling = self.signaling.clone();
        let transport = self.transport.clone();
        // Push-based signaling: new edge statements wake the read immediately —
        // the periodic read below is only reconciliation for anything push missed.
        let mut push = self.signaling.subscribe_edge_push();
        let dormant = self.dormant.clone();
        tokio::spawn(async move {
            let mut seen: HashSet<Seq> = HashSet::new();
            loop {
                // Adaptive: poll fast until connected (the first edge must land quickly),
                // then back off — once in-mesh, pushes + signed edge gossip deliver new
                // segments immediately and this chain read is only reconciliation.
                // Stretching to the dormant cadence is safe for the same reason the idle
                // backoff is (the header comment on these constants): the edge push above
                // stays live and wakes the read the moment a statement lands — the sleep
                // only paces the reconciliation fallback.
                let period =
                    poll_period(dormant.load(std::sync::atomic::Ordering::Relaxed), transport.peer_count());
                tokio::select! {
                    _ = sleep(period) => {}
                    Some(_) = push.recv() => {
                        while push.try_recv().is_ok() {} // coalesce a burst into one read
                    }
                }
                if let Ok(edge) = signaling.read_edge().await {
                    if !edge.is_empty() {
                        log::debug!("[edge] chain poll → {} entr(ies)", edge.len());
                    }
                    for (seq, id) in edge {
                        if seen.insert(seq) {
                            log::info!("[edge] chain → LiveEdge seq={seq}");
                            let _ = inbox.send(EngineEvent::LiveEdge { seq, id });
                        }
                    }
                }
            }
        })
    }
}

/// Reconciliation-poll cadence for the signaling/edge readers. Pure so the truth table
/// is unit-testable. Peers win over dormancy: a dormant flag left set while a
/// recruitment connects peers must never slow a live mesh (it just lands on the normal
/// idle pace); and with zero peers, dormant stretches the ACTIVE fast-poll to the
/// heartbeat — which is safe only because the push subscriptions
/// (`subscribe_edge_push`/`subscribe_signals_push`) remain live at any cadence.
fn poll_period(dormant: bool, connected_peers: usize) -> Duration {
    if connected_peers > 0 {
        SIGNAL_POLL_IDLE
    } else if dormant {
        SIGNAL_POLL_DORMANT
    } else {
        SIGNAL_POLL_ACTIVE
    }
}

/// Origin-shield admission gate, pure so the truth table is unit-testable.
///
/// * `!shield` → everyone is admitted (the shield is off).
/// * `shield` with a non-empty `allow` (hardened): admit only a peer whose
///   CHAIN-VERIFIED presence signer maps into the allowlist — the volunteers this
///   publisher recruited. The self-asserted relay flag is deliberately ignored here:
///   it rides unsigned gossip, so an attacker can set it on itself.
/// * `shield` with an empty `allow` (legacy, env-forced shield without a recruiter):
///   fall back to the relay flag — weaker, but better than refusing everyone.
fn shield_admits(
    shield: bool,
    allow: &HashSet<[u8; 32]>,
    signer_of_peer: Option<&[u8; 32]>,
    is_relay_flagged: bool,
) -> bool {
    if !shield {
        return true;
    }
    if !allow.is_empty() {
        signer_of_peer.is_some_and(|s| allow.contains(s))
    } else {
        is_relay_flagged
    }
}

/// Drain locally-generated SDP/ICE and publish it to the peer's signaling topic.
async fn relay_outbound(
    mut rx: UnboundedReceiver<SignalOut>,
    signaling: ChainSignaling,
    me: PeerId,
) {
    while let Some(out) = rx.recv().await {
        let (to, msg) = match out {
            SignalOut::LocalDescription { peer, sdp } => {
                let msg = if sdp_is_offer(&sdp) {
                    SignalMsg::Offer { sdp }
                } else {
                    SignalMsg::Answer { offer_id: String::new(), sdp }
                };
                (peer, msg)
            }
            SignalOut::LocalCandidate { peer, cand } => {
                (peer, SignalMsg::IceCandidate { offer_id: String::new(), sdp: cand })
            }
        };
        let kind = match &msg {
            SignalMsg::Offer { .. } => "Offer",
            SignalMsg::Answer { .. } => "Answer",
            SignalMsg::IceCandidate { .. } => "IceCandidate",
            SignalMsg::Closed { .. } => "Closed",
        };
        if let Err(e) = signaling.publish_signal(me, to, msg).await {
            log::warn!("[session] → {kind} to {to:?} FAILED: {e}");
        } else {
            log::info!("[session] → {kind} to {to:?} sent");
        }
    }
}

/// Poll our signaling topic and feed remote SDP/ICE into the transport. A unified
/// handler works for both roles: a viewer never receives an `Offer`, a publisher
/// never receives an `Answer`. Offers are applied before candidates so the peer
/// connection exists first (the transport also buffers early candidates).
#[allow(clippy::too_many_arguments)] // session-local shared state, threaded once from `start`
async fn relay_inbound(
    signaling: ChainSignaling,
    me: PeerId,
    transport: LibDcTransport,
    bans: BanList,
    book: PresenceBook,
    shield: Arc<std::sync::atomic::AtomicBool>,
    shield_allow: Arc<Mutex<HashSet<[u8; 32]>>>,
    chain_relay_accounts: ChainRelayAccounts,
    dormant: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    // Push-based signaling: an SDP/ICE statement addressed to us wakes the read
    // immediately — handshakes stop paying the poll interval.
    let mut push = signaling.subscribe_signals_push(me);
    loop {
        // Adaptive: while establishing (no live peer) poll fast so SDP/ICE round-trips
        // promptly; once connected, pushes handle new offers instantly and the idle
        // poll is only reconciliation. A DORMANT seed with zero peers drops to the
        // heartbeat cadence — safe because the signals push above stays live, so an
        // inbound offer still wakes this read the moment its statement lands.
        let period =
            poll_period(dormant.load(std::sync::atomic::Ordering::Relaxed), transport.peer_count());
        tokio::select! {
            _ = sleep(period) => {}
            Some(_) = push.recv() => {
                while push.try_recv().is_ok() {} // coalesce a burst into one read
            }
        }
        let mut sigs = match signaling.read_signals(me).await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[session] read_signals: {e}");
                continue;
            }
        };
        sigs.sort_by_key(|(_, m)| sig_order(m));
        for (from, msg) in sigs {
            if bans.contains(&from) {
                continue; // convicted by the node — its offers/answers are refused
            }
            let key = dedup_key(&from, &msg);
            if seen.contains(&key) {
                continue;
            }
            // Origin-shield: a shielded publisher answers only its seed tier, so its IP
            // never reaches an ordinary viewer. A refused viewer's dial simply times out
            // and it connects to a seed instead (the maintainer already prefers relay
            // peers). See [`shield_admits`] for the policy: with an allowlist the gate
            // keys off the CHAIN-VERIFIED presence signer (only `spawn_relay_discovery`
            // writes that map — gossip can't), so a self-flagged attacker can't get
            // answered.
            if matches!(msg, SignalMsg::Offer { .. })
                && shield.load(std::sync::atomic::Ordering::Relaxed)
            {
                let allow = shield_allow.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let signer = chain_relay_accounts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&from.0)
                    .map(|(signer, _)| *signer);
                if !shield_admits(true, &allow, signer.as_ref(), book.is_relay(&from)) {
                    let reason = if allow.is_empty() {
                        "not relay-flagged"
                    } else if signer.is_none() {
                        "no chain-verified signer"
                    } else {
                        "signer not in the allowlist"
                    };
                    // Deliberately NOT marked seen: the offer stays in the sender's
                    // signal bundle, so the next read re-evaluates it. A recruited seed
                    // dials the instant it discovers us — often BEFORE our discovery
                    // tick has read its chain presence back — and consuming that first
                    // offer would cost the seed its whole 90s dial timeout before a
                    // retry. Deferring admits it one poll after discovery catches up;
                    // a truly unwanted peer is just re-refused at debug level until
                    // its bundle expires.
                    log::debug!("[session] ⊘ Offer from {from:?} refused (origin-shield: {reason})");
                    continue;
                }
            }
            seen.insert(key);
            match msg {
                SignalMsg::Offer { sdp } => {
                    log::info!("[session] ← Offer from {from:?}");
                    transport.accept(from, sdp);
                }
                SignalMsg::Answer { sdp, .. } => { log::info!("[session] ← Answer from {from:?}"); transport.remote_description(from, sdp); }
                SignalMsg::IceCandidate { sdp, .. } => { log::debug!("[session] ← IceCandidate from {from:?}"); transport.remote_candidate(from, sdp); }
                SignalMsg::Closed { .. } => { log::info!("[session] ← Closed from {from:?}"); transport.close(from); }
            }
        }
    }
}

fn sig_order(m: &SignalMsg) -> u8 {
    match m {
        SignalMsg::Offer { .. } => 0,
        SignalMsg::Answer { .. } => 1,
        SignalMsg::IceCandidate { .. } => 2,
        SignalMsg::Closed { .. } => 3,
    }
}

fn dedup_key(from: &PeerId, m: &SignalMsg) -> Vec<u8> {
    let mut k = from.0.to_vec();
    match m {
        SignalMsg::Offer { sdp } => {
            k.push(0);
            k.extend_from_slice(sdp);
        }
        SignalMsg::Answer { sdp, .. } => {
            k.push(1);
            k.extend_from_slice(sdp);
        }
        SignalMsg::IceCandidate { sdp, .. } => {
            k.push(2);
            k.extend_from_slice(sdp);
        }
        SignalMsg::Closed { .. } => k.push(3),
    }
    k
}

/// datachannel-rs serializes `SessionDescription` with a `"type"` field
/// (`offer`/`answer`); we relay offers and answers as the matching `SignalMsg`.
fn sdp_is_offer(sdp_json: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(rename = "type")]
        ty: String,
    }
    serde_json::from_slice::<Probe>(sdp_json)
        .map(|p| p.ty == "offer")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_offer_vs_answer() {
        assert!(sdp_is_offer(br#"{"type":"offer","sdp":"v=0..."}"#));
        assert!(!sdp_is_offer(br#"{"type":"answer","sdp":"v=0..."}"#));
        assert!(!sdp_is_offer(b"not json"));
    }

    #[test]
    fn dedup_key_separates_peers_variants_and_payloads() {
        let a = PeerId::from_u64(1);
        let b = PeerId::from_u64(2);
        let off = SignalMsg::Offer { sdp: vec![1, 2, 3] };
        let ans = SignalMsg::Answer { offer_id: String::new(), sdp: vec![1, 2, 3] };
        // Same payload, different peer / variant → different keys.
        assert_ne!(dedup_key(&a, &off), dedup_key(&b, &off));
        assert_ne!(dedup_key(&a, &off), dedup_key(&a, &ans));
        // Identical → identical (so re-reads dedup).
        assert_eq!(dedup_key(&a, &off), dedup_key(&a, &SignalMsg::Offer { sdp: vec![1, 2, 3] }));
    }

    #[test]
    fn poll_period_truth_table() {
        // Establishing (no peers, not dormant): fast, so SDP/ICE round-trips promptly.
        assert_eq!(poll_period(false, 0), SIGNAL_POLL_ACTIVE);
        // Parked volunteer seed (no peers, dormant): heartbeat reconciliation only.
        assert_eq!(poll_period(true, 0), SIGNAL_POLL_DORMANT);
        // Connected: idle reconciliation, and peers WIN over a stale dormant flag —
        // a recruited seed serving a stream must never be slowed by it.
        assert_eq!(poll_period(false, 1), SIGNAL_POLL_IDLE);
        assert_eq!(poll_period(true, 1), SIGNAL_POLL_IDLE);
        assert_eq!(poll_period(true, 7), SIGNAL_POLL_IDLE);
    }

    #[test]
    fn offers_sort_before_candidates() {
        assert!(sig_order(&SignalMsg::Offer { sdp: vec![] }) < sig_order(&SignalMsg::IceCandidate { offer_id: String::new(), sdp: vec![] }));
    }

    #[test]
    fn shield_admits_truth_table() {
        let recruited = [0xAAu8; 32];
        let stranger = [0xBBu8; 32];
        let allow: HashSet<[u8; 32]> = [recruited].into_iter().collect();
        let empty: HashSet<[u8; 32]> = HashSet::new();

        // Shield off: everyone is admitted, whatever the other inputs say.
        assert!(shield_admits(false, &empty, None, false));
        assert!(shield_admits(false, &allow, Some(&stranger), false));

        // Hardened (allowlist set): ONLY a chain-verified signer in the allowlist
        // passes — the self-asserted relay flag is ignored in both directions.
        assert!(shield_admits(true, &allow, Some(&recruited), false));
        assert!(shield_admits(true, &allow, Some(&recruited), true));
        assert!(!shield_admits(true, &allow, Some(&stranger), true));
        assert!(!shield_admits(true, &allow, None, true), "no chain signer → refused");

        // Legacy (env-only shield, no recruiter yet): fall back to the relay flag.
        assert!(shield_admits(true, &empty, None, true));
        assert!(!shield_admits(true, &empty, None, false));
        assert!(!shield_admits(true, &empty, Some(&recruited), false), "signer alone can't pass the legacy gate");
    }
}
