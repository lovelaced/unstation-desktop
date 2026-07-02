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

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
const DISCOVERY_POLL: Duration = Duration::from_secs(2);
/// Maintainer re-evaluation cadence absent any transport event.
const MAINTAIN_TICK: Duration = Duration::from_secs(1);
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
        tokio::spawn(relay_outbound(sig_rx, signaling.clone(), my_peer));
        tokio::spawn(relay_inbound(signaling.clone(), my_peer, transport.clone(), bans.clone()));

        Ok(Self {
            stream,
            my_peer,
            n_shards: n_shards.max(1),
            signaling,
            transport,
            manifest_cid: Arc::new(Mutex::new(None)),
            book: PresenceBook::new(),
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
    pub fn spawn_presence(&self, caps_upload_bps: u64, relay_opt_in: bool) -> tokio::task::JoinHandle<()> {
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
                // Advertise relay-capability if explicitly opted in OR we've proven
                // reachable (a peer connected to us inbound) — emergent volunteer relay.
                let relay = relay_opt_in || transport.reachable();
                // `peer_id` is our per-device routing id; `publisher` is our stable
                // personhood key — the trust anchor viewers verify the manifest/edge
                // against. Splitting them lets two devices of the same person coexist in
                // the mesh (they share `publisher` but differ in `peer_id`).
                let publisher = unstation_chain::identity_public().unwrap_or(me.0);
                let p = Presence { peer_id: me, publisher, caps_upload_bps, ttl_s: PRESENCE_TTL_S, manifest_cid: mc, relay };
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
        let dialer = Dialer::new();
        let (ev_tx, mut ev_rx) = unbounded_channel::<TransportEvent>();
        transport.set_event_sink(ev_tx);
        tokio::spawn(async move {
            loop {
                // Abandon dials that hung mid-handshake (lost signaling / ICE failure):
                // close them so a fresh dial isn't blocked by the duplicate guard, and
                // let the backoff schedule own the retry.
                for p in dialer.stalled() {
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
                // the periodic tick (ages stalled dials + retries after backoff).
                tokio::select! {
                    ev = ev_rx.recv() => match ev {
                        Some(TransportEvent::Connected(p)) => dialer.record_connected(&p),
                        Some(TransportEvent::Disconnected(p)) => dialer.record_failed(p),
                        None => break,
                    },
                    _ = sleep(MAINTAIN_TICK) => {}
                }
                // Coalesce any burst before re-evaluating.
                while let Ok(ev) = ev_rx.try_recv() {
                    match ev {
                        TransportEvent::Connected(p) => dialer.record_connected(&p),
                        TransportEvent::Disconnected(p) => dialer.record_failed(p),
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
        tokio::spawn(async move {
            let mut seen: HashSet<Seq> = HashSet::new();
            loop {
                // Adaptive: poll fast until connected (the first edge must land quickly),
                // then back off — once in-mesh, signed edge gossip delivers new segments
                // at mesh speed and this chain read is only reconciliation.
                let period = if transport.peer_count() == 0 {
                    SIGNAL_POLL_ACTIVE
                } else {
                    SIGNAL_POLL_IDLE
                };
                sleep(period).await;
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
async fn relay_inbound(
    signaling: ChainSignaling,
    me: PeerId,
    transport: LibDcTransport,
    bans: BanList,
) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    loop {
        // Adaptive: while establishing (no live peer) poll fast so SDP/ICE round-trips
        // promptly; once connected, offers still arrive within the idle period (worst
        // case a few seconds to add a new peer) at a fraction of the chain reads.
        let period = if transport.peer_count() == 0 { SIGNAL_POLL_ACTIVE } else { SIGNAL_POLL_IDLE };
        sleep(period).await;
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
            if !seen.insert(dedup_key(&from, &msg)) {
                continue;
            }
            match msg {
                SignalMsg::Offer { sdp } => { log::info!("[session] ← Offer from {from:?}"); transport.accept(from, sdp); }
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
    fn offers_sort_before_candidates() {
        assert!(sig_order(&SignalMsg::Offer { sdp: vec![] }) < sig_order(&SignalMsg::IceCandidate { offer_id: String::new(), sdp: vec![] }));
    }
}
