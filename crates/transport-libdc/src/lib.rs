//! libdatachannel-backed WebRTC transport (datachannel-rs, MPL-2.0).
//!
//! Establishes real peer-to-peer data-channel links. The SDP/ICE handshake is
//! exchanged out-of-band over the statement store (the orchestrator relays the
//! [`SignalOut`] events this transport emits, and feeds remote SDP/ICE back in).
//!
//! # Threading
//!
//! datachannel-rs addresses libdatachannel by integer id, so `RtcPeerConnection`
//! / `RtcDataChannel` are `Send` (when their handlers are). We still keep every
//! peer connection and data channel on a **single reactor thread** and drive it
//! through a command channel: callbacks (which fire on libdatachannel's own
//! threads) only *forward* events over channels and never re-enter the FFI, so
//! there is no callback↔send reentrancy deadlock. [`LibDcLink::send`] posts a
//! [`Cmd::Send`] to the reactor; inbound `on_message` posts
//! [`EngineEvent::Inbound`] straight to the node inbox.
//!
//! Each peer gets two channels (TECH_SPEC §6): `ctrl` (reliable, ordered) and
//! `bulk` (`maxRetransmits=0`, unordered) so a late segment chunk never
//! head-of-line-blocks control.

pub use unstation_core::transport::{Channel, Link};

use datachannel::{
    ConnectionState, DataChannelHandler, DataChannelInfo, DataChannelInit, IceCandidate,
    PeerConnectionHandler, Reliability, RtcConfig, RtcDataChannel, RtcPeerConnection,
    SessionDescription,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use unstation_core::transport::EngineEvent;
use unstation_core::types::PeerId;

/// Locally-generated signaling the orchestrator must relay to the peer over the
/// statement store (as `SignalMsg::Offer/Answer/IceCandidate`). The `sdp`/`cand`
/// payloads are JSON of datachannel-rs's `SessionDescription` / `IceCandidate`.
#[derive(Debug, Clone)]
pub enum SignalOut {
    /// SDP offer or answer (distinguish by the embedded `"type"` field).
    LocalDescription { peer: PeerId, sdp: Vec<u8> },
    LocalCandidate { peer: PeerId, cand: Vec<u8> },
}

/// Commands processed serially on the reactor thread.
enum Cmd {
    Dial(PeerId),
    Accept(PeerId, Vec<u8>),
    RemoteDescription(PeerId, Vec<u8>),
    RemoteCandidate(PeerId, Vec<u8>),
    Send(PeerId, Channel, Vec<u8>),
    DcArrived(PeerId, String, Box<RtcDataChannel<DcSink>>),
    DcOpen(PeerId, Channel),
    StateChange(PeerId, ConnectionState),
    Close(PeerId),
}

/// Per-data-channel handler: forwards inbound bytes to the node inbox and open
/// events to the reactor. Cheap and `Send` (only channel senders + ids).
pub struct DcSink {
    inbox: UnboundedSender<EngineEvent>,
    cmd: UnboundedSender<Cmd>,
    peer: PeerId,
    channel: Channel,
}

impl DataChannelHandler for DcSink {
    fn on_open(&mut self) {
        let _ = self.cmd.send(Cmd::DcOpen(self.peer, self.channel));
    }
    fn on_message(&mut self, msg: &[u8]) {
        let _ = self.inbox.send(EngineEvent::Inbound {
            peer: self.peer,
            channel: self.channel,
            bytes: msg.to_vec(),
        });
    }
}

/// Per-peer-connection handler: forwards local SDP/ICE to the orchestrator and
/// inbound (answerer-side) data channels + connection-state changes to the reactor.
pub struct Conn {
    peer: PeerId,
    cmd: UnboundedSender<Cmd>,
    signals: UnboundedSender<SignalOut>,
    inbox: UnboundedSender<EngineEvent>,
    /// Label of the data channel currently being constructed (set in
    /// `data_channel_handler`, consumed by the immediately-following
    /// `on_data_channel` on the same callback thread).
    pending_label: Option<String>,
}

impl PeerConnectionHandler for Conn {
    type DCH = DcSink;

    fn data_channel_handler(&mut self, info: DataChannelInfo) -> DcSink {
        let channel = channel_for(&info.label);
        self.pending_label = Some(info.label);
        DcSink { inbox: self.inbox.clone(), cmd: self.cmd.clone(), peer: self.peer, channel }
    }
    fn on_description(&mut self, desc: SessionDescription) {
        if let Ok(sdp) = serde_json::to_vec(&desc) {
            let _ = self.signals.send(SignalOut::LocalDescription { peer: self.peer, sdp });
        }
    }
    fn on_candidate(&mut self, cand: IceCandidate) {
        if let Ok(cand) = serde_json::to_vec(&cand) {
            let _ = self.signals.send(SignalOut::LocalCandidate { peer: self.peer, cand });
        }
    }
    fn on_data_channel(&mut self, dc: Box<RtcDataChannel<DcSink>>) {
        let label = self.pending_label.take().unwrap_or_default();
        let _ = self.cmd.send(Cmd::DcArrived(self.peer, label, dc));
    }
    fn on_connection_state_change(&mut self, state: ConnectionState) {
        let _ = self.cmd.send(Cmd::StateChange(self.peer, state));
    }
}

fn channel_for(label: &str) -> Channel {
    if label == "bulk" {
        Channel::Bulk
    } else {
        Channel::Ctrl
    }
}

/// A connected peer link held by the node. `send` posts to the reactor, which
/// owns the data channels and performs the FFI write.
struct LibDcLink {
    remote: PeerId,
    cmd: UnboundedSender<Cmd>,
}

impl Link for LibDcLink {
    fn remote(&self) -> PeerId {
        self.remote
    }
    fn send(&self, channel: Channel, bytes: Vec<u8>) {
        let _ = self.cmd.send(Cmd::Send(self.remote, channel, bytes));
    }
}

/// State the reactor owns for one peer connection.
struct Peer {
    pc: Box<RtcPeerConnection<Conn>>,
    ctrl: Option<Box<RtcDataChannel<DcSink>>>,
    bulk: Option<Box<RtcDataChannel<DcSink>>>,
    ctrl_open: bool,
    bulk_open: bool,
    announced: bool,
    /// libdatachannel rejects remote candidates added before the remote
    /// description is set, and (with trickle ICE) candidates can arrive in any
    /// order. Buffer them until `remote_set`, then flush.
    remote_set: bool,
    pending_cands: Vec<Vec<u8>>,
}

/// Handle to the WebRTC reactor. Cheap to clone.
#[derive(Clone)]
pub struct LibDcTransport {
    cmd: UnboundedSender<Cmd>,
    /// Number of peers with both channels open (a real, live stat for the UI).
    connected: Arc<AtomicUsize>,
}

impl LibDcTransport {
    /// Spawn the reactor thread.
    ///
    /// * `stun` — ICE/STUN server URIs (e.g. `["stun:stun.l.google.com:19302"]`);
    ///   empty is fine on a LAN (host candidates suffice).
    /// * `inbox` — the `MeshNode` event inbox: receives `PeerConnected`,
    ///   `Inbound`, `PeerDisconnected`.
    /// * `signals` — locally-generated SDP/ICE the orchestrator relays to the peer.
    pub fn new(
        stun: Vec<String>,
        inbox: UnboundedSender<EngineEvent>,
        signals: UnboundedSender<SignalOut>,
    ) -> Self {
        let (cmd_tx, mut cmd_rx) = unbounded_channel::<Cmd>();
        let reactor_cmd = cmd_tx.clone();
        let connected = Arc::new(AtomicUsize::new(0));
        let reactor_connected = connected.clone();
        std::thread::Builder::new()
            .name("libdc-reactor".into())
            .spawn(move || {
                let mut peers: HashMap<PeerId, Peer> = HashMap::new();
                // Candidates that arrived before their peer connection existed.
                let mut orphan_cands: HashMap<PeerId, Vec<Vec<u8>>> = HashMap::new();
                // Plain blocking loop on a non-async thread (owns all !ergonomic FFI state).
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    handle_cmd(
                        cmd,
                        &mut peers,
                        &mut orphan_cands,
                        &stun,
                        &inbox,
                        &signals,
                        &reactor_cmd,
                        &reactor_connected,
                    );
                }
            })
            .expect("spawn libdc reactor");
        Self { cmd: cmd_tx, connected }
    }

    /// Number of peers currently connected (both channels open). A real,
    /// live mesh stat for the UI.
    pub fn peer_count(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    /// Initiator: open a connection to `peer` (creates the two data channels; the
    /// local offer is emitted via `signals`).
    pub fn dial(&self, peer: PeerId) {
        let _ = self.cmd.send(Cmd::Dial(peer));
    }
    /// Answerer: accept `peer`'s offer (JSON `SessionDescription`); the local
    /// answer is emitted via `signals`.
    pub fn accept(&self, peer: PeerId, offer_json: Vec<u8>) {
        let _ = self.cmd.send(Cmd::Accept(peer, offer_json));
    }
    /// Feed a remote SDP answer (initiator side) — JSON `SessionDescription`.
    pub fn remote_description(&self, peer: PeerId, sdp_json: Vec<u8>) {
        let _ = self.cmd.send(Cmd::RemoteDescription(peer, sdp_json));
    }
    /// Feed a remote ICE candidate — JSON `IceCandidate`.
    pub fn remote_candidate(&self, peer: PeerId, cand_json: Vec<u8>) {
        let _ = self.cmd.send(Cmd::RemoteCandidate(peer, cand_json));
    }
    /// Tear down a peer connection.
    pub fn close(&self, peer: PeerId) {
        let _ = self.cmd.send(Cmd::Close(peer));
    }
}

fn rtc_config(stun: &[String]) -> RtcConfig {
    RtcConfig::new(stun)
}

fn new_conn(
    peer: PeerId,
    cmd: &UnboundedSender<Cmd>,
    signals: &UnboundedSender<SignalOut>,
    inbox: &UnboundedSender<EngineEvent>,
) -> Conn {
    Conn {
        peer,
        cmd: cmd.clone(),
        signals: signals.clone(),
        inbox: inbox.clone(),
        pending_label: None,
    }
}

fn dc_sink(
    peer: PeerId,
    channel: Channel,
    cmd: &UnboundedSender<Cmd>,
    inbox: &UnboundedSender<EngineEvent>,
) -> DcSink {
    DcSink { inbox: inbox.clone(), cmd: cmd.clone(), peer, channel }
}

fn add_remote_candidate(p: &mut Peer, json: &[u8]) {
    match serde_json::from_slice::<IceCandidate>(json) {
        Ok(c) => {
            let _ = p.pc.add_remote_candidate(&c);
        }
        Err(e) => log::warn!("[libdc] bad candidate json: {e}"),
    }
}

fn flush_candidates(p: &mut Peer) {
    for c in std::mem::take(&mut p.pending_cands) {
        add_remote_candidate(p, &c);
    }
}

fn handle_cmd(
    cmd: Cmd,
    peers: &mut HashMap<PeerId, Peer>,
    orphan_cands: &mut HashMap<PeerId, Vec<Vec<u8>>>,
    stun: &[String],
    inbox: &UnboundedSender<EngineEvent>,
    signals: &UnboundedSender<SignalOut>,
    reactor_cmd: &UnboundedSender<Cmd>,
    connected: &AtomicUsize,
) {
    match cmd {
        Cmd::Dial(peer) => {
            let conf = rtc_config(stun);
            let mut pc = match RtcPeerConnection::new(&conf, new_conn(peer, reactor_cmd, signals, inbox)) {
                Ok(pc) => pc,
                Err(e) => {
                    log::warn!("[libdc] dial {peer:?}: pc create failed: {e}");
                    return;
                }
            };
            // ctrl: reliable + ordered (default). bulk: unordered, no retransmits.
            let ctrl = pc
                .create_data_channel("ctrl", dc_sink(peer, Channel::Ctrl, reactor_cmd, inbox))
                .ok();
            let bulk_init =
                DataChannelInit::default().reliability(Reliability::default().unordered().max_retransmits(0));
            let bulk = pc
                .create_data_channel_ex("bulk", dc_sink(peer, Channel::Bulk, reactor_cmd, inbox), &bulk_init)
                .ok();
            peers.insert(
                peer,
                Peer {
                    pc,
                    ctrl,
                    bulk,
                    ctrl_open: false,
                    bulk_open: false,
                    announced: false,
                    // Remote answer not applied yet; candidates wait for it.
                    remote_set: false,
                    pending_cands: orphan_cands.remove(&peer).unwrap_or_default(),
                },
            );
        }
        Cmd::Accept(peer, offer_json) => {
            let conf = rtc_config(stun);
            let mut pc = match RtcPeerConnection::new(&conf, new_conn(peer, reactor_cmd, signals, inbox)) {
                Ok(pc) => pc,
                Err(e) => {
                    log::warn!("[libdc] accept {peer:?}: pc create failed: {e}");
                    return;
                }
            };
            let mut remote_set = false;
            match serde_json::from_slice::<SessionDescription>(&offer_json) {
                Ok(offer) => match pc.set_remote_description(&offer) {
                    Ok(()) => remote_set = true,
                    Err(e) => log::warn!("[libdc] accept {peer:?}: set_remote_description failed: {e}"),
                },
                Err(e) => log::warn!("[libdc] accept {peer:?}: bad offer json: {e}"),
            }
            // The answerer's data channels arrive via on_data_channel (DcArrived).
            let mut p = Peer {
                pc,
                ctrl: None,
                bulk: None,
                ctrl_open: false,
                bulk_open: false,
                announced: false,
                remote_set,
                pending_cands: orphan_cands.remove(&peer).unwrap_or_default(),
            };
            // The offer is the remote description, so any buffered candidates apply now.
            if remote_set {
                flush_candidates(&mut p);
            }
            peers.insert(peer, p);
        }
        Cmd::RemoteDescription(peer, json) => {
            if let Some(p) = peers.get_mut(&peer) {
                match serde_json::from_slice::<SessionDescription>(&json) {
                    Ok(desc) => match p.pc.set_remote_description(&desc) {
                        Ok(()) => {
                            p.remote_set = true;
                            flush_candidates(p);
                        }
                        Err(e) => log::warn!("[libdc] {peer:?}: set_remote_description failed: {e}"),
                    },
                    Err(e) => log::warn!("[libdc] {peer:?}: bad sdp json: {e}"),
                }
            }
        }
        Cmd::RemoteCandidate(peer, json) => {
            match peers.get_mut(&peer) {
                // Candidates rejected before the remote description is set — buffer
                // them (per-peer if the connection exists, else as orphans).
                Some(p) if p.remote_set => add_remote_candidate(p, &json),
                Some(p) => p.pending_cands.push(json),
                None => orphan_cands.entry(peer).or_default().push(json),
            }
        }
        Cmd::Send(peer, channel, bytes) => {
            if let Some(p) = peers.get_mut(&peer) {
                let dc = match channel {
                    Channel::Ctrl => p.ctrl.as_mut(),
                    Channel::Bulk => p.bulk.as_mut(),
                };
                if let Some(dc) = dc {
                    if let Err(e) = dc.send(&bytes) {
                        log::debug!("[libdc] {peer:?}: send on {channel:?} failed: {e}");
                    }
                }
            }
        }
        Cmd::DcArrived(peer, label, dc) => {
            if let Some(p) = peers.get_mut(&peer) {
                match channel_for(&label) {
                    Channel::Ctrl => p.ctrl = Some(dc),
                    Channel::Bulk => p.bulk = Some(dc),
                }
            }
        }
        Cmd::DcOpen(peer, channel) => {
            if let Some(p) = peers.get_mut(&peer) {
                match channel {
                    Channel::Ctrl => p.ctrl_open = true,
                    Channel::Bulk => p.bulk_open = true,
                }
                if p.ctrl_open && p.bulk_open && !p.announced {
                    p.announced = true;
                    connected.fetch_add(1, Ordering::Relaxed);
                    let link: Arc<dyn Link> =
                        Arc::new(LibDcLink { remote: peer, cmd: reactor_cmd.clone() });
                    let _ = inbox.send(EngineEvent::PeerConnected { peer, link });
                }
            }
        }
        Cmd::StateChange(peer, state) => {
            if matches!(state, ConnectionState::Disconnected | ConnectionState::Failed | ConnectionState::Closed)
            {
                orphan_cands.remove(&peer);
                if let Some(p) = peers.remove(&peer) {
                    if p.announced {
                        connected.fetch_sub(1, Ordering::Relaxed);
                        let _ = inbox.send(EngineEvent::PeerDisconnected { peer });
                    }
                }
            }
        }
        Cmd::Close(peer) => {
            orphan_cands.remove(&peer);
            peers.remove(&peer);
        }
    }
}
