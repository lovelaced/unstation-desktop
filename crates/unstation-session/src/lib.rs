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
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, sleep};

use transport_libdc::{LibDcTransport, SignalOut};
use unstation_chain::ChainSignaling;
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

/// A running session: the chain-signaling + WebRTC plumbing for one stream.
#[derive(Clone)]
pub struct Session {
    pub stream: StreamId,
    pub my_peer: PeerId,
    pub n_shards: u32,
    signaling: ChainSignaling,
    transport: LibDcTransport,
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
        key_dir: std::path::PathBuf,
        inbox: UnboundedSender<EngineEvent>,
    ) -> Result<Self, String> {
        unstation_chain::init_statement_store_persisted(&key_dir);
        let my_peer = unstation_chain::local_peer_id()
            .ok_or("statement store did not expose a public key")?;

        let (sig_tx, sig_rx) = unbounded_channel::<SignalOut>();
        let transport = LibDcTransport::new(stun, inbox, sig_tx);
        let signaling = ChainSignaling::new(stream, n_shards);

        tokio::spawn(relay_outbound(sig_rx, signaling.clone(), my_peer));
        tokio::spawn(relay_inbound(signaling.clone(), my_peer, transport.clone()));

        Ok(Self { stream, my_peer, n_shards: n_shards.max(1), signaling, transport })
    }

    /// Publisher: announce presence on our discovery shard, refreshed before TTL.
    pub fn spawn_presence(&self, caps_upload_bps: u64) {
        let signaling = self.signaling.clone();
        let me = self.my_peer;
        tokio::spawn(async move {
            let mut tick = interval(PRESENCE_REFRESH);
            loop {
                tick.tick().await;
                let p = Presence { peer_id: me, caps_upload_bps, ttl_s: PRESENCE_TTL_S };
                if let Err(e) = signaling.publish_presence(p).await {
                    log::warn!("[session] publish_presence: {e}");
                }
            }
        });
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

    /// Viewer: poll presence across all discovery shards until a publisher (any
    /// peer that isn't us) appears, and return it.
    pub async fn discover_publisher(&self) -> PeerId {
        loop {
            for shard in 0..self.n_shards {
                let topic = discovery_topic(&self.stream, shard);
                if let Ok(list) = self.signaling.read_presence(topic, 32).await {
                    if let Some(p) = list.into_iter().find(|p| p.peer_id != self.my_peer) {
                        return p.peer_id;
                    }
                }
            }
            sleep(DISCOVERY_POLL).await;
        }
    }

    /// Viewer: open a WebRTC connection to a discovered publisher. The link
    /// arrives at the node inbox as `PeerConnected` once both channels open.
    pub fn dial(&self, publisher: PeerId) {
        self.transport.dial(publisher);
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
