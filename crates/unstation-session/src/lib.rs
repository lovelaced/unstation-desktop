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

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, sleep};

use transport_libdc::{LibDcTransport, SignalOut};
use unstation_chain::ChainSignaling;
use unstation_core::node::EdgeSigner;
use unstation_core::signaling::{Presence, Signaling, SignalMsg};
use unstation_core::topic::discovery_topic;
use unstation_core::transport::EngineEvent;
use unstation_core::types::{PeerId, SegmentId, Seq, StreamId};

const SIGNAL_POLL: Duration = Duration::from_millis(800);
const DISCOVERY_POLL: Duration = Duration::from_secs(2);
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
        let my_peer = unstation_chain::local_peer_id()
            .ok_or("not signed in — pair the Polkadot app to publish or watch")?;

        let (sig_tx, sig_rx) = unbounded_channel::<SignalOut>();
        let transport = LibDcTransport::new(stun, inbox, sig_tx);
        let signaling = ChainSignaling::new(stream, n_shards);

        tokio::spawn(relay_outbound(sig_rx, signaling.clone(), my_peer));
        tokio::spawn(relay_inbound(signaling.clone(), my_peer, transport.clone()));

        Ok(Self {
            stream,
            my_peer,
            n_shards: n_shards.max(1),
            signaling,
            transport,
            manifest_cid: Arc::new(Mutex::new(None)),
        })
    }

    /// Publisher: announce presence on our discovery shard, refreshed before TTL.
    pub fn spawn_presence(&self, caps_upload_bps: u64, relay_opt_in: bool) {
        let signaling = self.signaling.clone();
        let me = self.my_peer;
        let manifest_cid = self.manifest_cid.clone();
        let transport = self.transport.clone();
        tokio::spawn(async move {
            let mut tick = interval(PRESENCE_REFRESH);
            loop {
                tick.tick().await;
                let mc = manifest_cid.lock().unwrap().clone();
                // Advertise relay-capability if explicitly opted in OR we've proven
                // reachable (a peer connected to us inbound) — emergent volunteer relay.
                let relay = relay_opt_in || transport.reachable();
                let p = Presence { peer_id: me, caps_upload_bps, ttl_s: PRESENCE_TTL_S, manifest_cid: mc, relay };
                if let Err(e) = signaling.publish_presence(p).await {
                    log::warn!("[session] publish_presence: {e}");
                }
            }
        });
    }

    /// Publisher: set the signed-manifest Bulletin CID announced in presence. Call once
    /// the manifest has been published to Bulletin (after the encoder's init segment
    /// exists); the presence-refresh loop picks it up on its next tick.
    pub fn set_manifest_cid(&self, cid: String) {
        *self.manifest_cid.lock().unwrap() = Some(cid);
    }

    /// Publisher: republish the live-edge manifest as new `(seq, content-id)` pairs
    /// are produced by the segmenter (drained from `edge_rx`).
    pub fn spawn_edge_publisher(&self, mut edge_rx: UnboundedReceiver<(Seq, SegmentId)>) {
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
        });
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
        // Relay-capable volunteers first: a NAT-restricted node should try the reliable
        // bridges before random peers (the decentralized stand-in for a TURN server).
        out.sort_by_key(|p| !p.relay);
        out.truncate(max);
        out
    }

    /// Viewer: open a WebRTC connection to a discovered publisher. The link
    /// arrives at the node inbox as `PeerConnected` once both channels open.
    pub fn dial(&self, publisher: PeerId) {
        self.transport.dial(publisher);
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
    pub fn spawn_edge_poller(&self, inbox: UnboundedSender<EngineEvent>) {
        let signaling = self.signaling.clone();
        tokio::spawn(async move {
            let mut seen: HashSet<Seq> = HashSet::new();
            let mut tick = interval(SIGNAL_POLL);
            loop {
                tick.tick().await;
                if let Ok(edge) = signaling.read_edge().await {
                    for (seq, id) in edge {
                        if seen.insert(seq) {
                            let _ = inbox.send(EngineEvent::LiveEdge { seq, id });
                        }
                    }
                }
            }
        });
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
        if let Err(e) = signaling.publish_signal(me, to, msg).await {
            log::warn!("[session] publish_signal: {e}");
        }
    }
}

/// Poll our signaling topic and feed remote SDP/ICE into the transport. A unified
/// handler works for both roles: a viewer never receives an `Offer`, a publisher
/// never receives an `Answer`. Offers are applied before candidates so the peer
/// connection exists first (the transport also buffers early candidates).
async fn relay_inbound(signaling: ChainSignaling, me: PeerId, transport: LibDcTransport) {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut tick = interval(SIGNAL_POLL);
    loop {
        tick.tick().await;
        let mut sigs = match signaling.read_signals(me).await {
            Ok(s) => s,
            Err(e) => {
                log::warn!("[session] read_signals: {e}");
                continue;
            }
        };
        sigs.sort_by_key(|(_, m)| sig_order(m));
        for (from, msg) in sigs {
            if !seen.insert(dedup_key(&from, &msg)) {
                continue;
            }
            match msg {
                SignalMsg::Offer { sdp } => transport.accept(from, sdp),
                SignalMsg::Answer { sdp, .. } => transport.remote_description(from, sdp),
                SignalMsg::IceCandidate { sdp, .. } => transport.remote_candidate(from, sdp),
                SignalMsg::Closed { .. } => transport.close(from),
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
