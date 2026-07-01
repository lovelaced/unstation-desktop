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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use unstation_core::transport::EngineEvent;
use unstation_core::types::PeerId;

/// Drop bulk (video) sends when the channel's send buffer exceeds this. For a live
/// stream skipping stale frames is correct — buffering them just grows latency and
/// memory, and the bulk channel is lossy anyway (the receiver re-requests on
/// timeout). The reliable `ctrl` channel is never dropped.
const BULK_BUFFER_MAX: usize = 1024 * 1024; // 1 MiB

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
    /// Close every peer connection and stop the reactor. Sent when a `Session` is torn
    /// down (stop/re-watch): the reactor is otherwise kept alive forever by detached
    /// signaling tasks holding clones, so without this the connections leak — and the
    /// publisher keeps our (stable) peer id connected and ignores a re-watch's new offer.
    Shutdown,
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
    fn close(&self) {
        // Reactor drops the Peer → libdatachannel closes the connection; the node
        // then receives the normal PeerDisconnected and cleans up its state.
        let _ = self.cmd.send(Cmd::Close(self.remote));
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
    /// True if this connection was inbound (we `Accept`ed an offer). A successful
    /// inbound connection proves we're reachable from the outside → relay-capable.
    inbound: bool,
}

/// Handle to the WebRTC reactor. Cheap to clone.
#[derive(Clone)]
pub struct LibDcTransport {
    cmd: UnboundedSender<Cmd>,
    /// Number of peers with both channels open (a real, live stat for the UI).
    connected: Arc<AtomicUsize>,
    /// Set once any peer connects to us inbound — i.e. we're reachable from the
    /// outside and can volunteer as a relay for NAT-restricted peers (M4).
    reachable: Arc<AtomicBool>,
}

impl LibDcTransport {
    /// Spawn the reactor thread.
    ///
    /// * `stun` — ICE/STUN server URIs (e.g. `["stun:stun.l.google.com:19302"]`);
    ///   empty is fine on a LAN (host candidates suffice).
    /// * `inbox` — the `MeshNode` event inbox: receives `PeerConnected`,
    ///   `Inbound`, `PeerDisconnected`.
    /// * `signals` — locally-generated SDP/ICE the orchestrator relays to the peer.
    ///
    /// Errors if the OS refuses the reactor thread (resource exhaustion) — callers
    /// surface that instead of the process aborting.
    pub fn new(
        stun: Vec<String>,
        inbox: UnboundedSender<EngineEvent>,
        signals: UnboundedSender<SignalOut>,
    ) -> std::io::Result<Self> {
        // Tune SCTP globals before any PeerConnection exists (applies to new conns only).
        apply_sctp_settings();
        let (cmd_tx, mut cmd_rx) = unbounded_channel::<Cmd>();
        let reactor_cmd = cmd_tx.clone();
        let connected = Arc::new(AtomicUsize::new(0));
        let reactor_connected = connected.clone();
        let reachable = Arc::new(AtomicBool::new(false));
        let reactor_reachable = reachable.clone();
        std::thread::Builder::new()
            .name("libdc-reactor".into())
            .spawn(move || {
                let mut peers: HashMap<PeerId, Peer> = HashMap::new();
                // Candidates that arrived before their peer connection existed.
                let mut orphan_cands: HashMap<PeerId, Vec<Vec<u8>>> = HashMap::new();
                // Plain blocking loop on a non-async thread (owns all !ergonomic FFI state).
                while let Some(cmd) = cmd_rx.blocking_recv() {
                    if matches!(cmd, Cmd::Shutdown) {
                        // Close every peer the same way `Cmd::Close` does (drop the `Peer`
                        // → libdatachannel closes the PC), tell the node, then exit. We
                        // drain BEFORE returning, so the thread ends with an empty map and
                        // no live PeerConnection is destroyed at thread-exit — the at-exit
                        // C++ teardown is the separate FORTIFY-abort issue (#24).
                        for (peer, p) in peers.drain() {
                            if p.announced {
                                let _ = inbox.send(EngineEvent::PeerDisconnected { peer });
                            }
                        }
                        orphan_cands.clear();
                        reactor_connected.store(0, Ordering::Relaxed);
                        log::info!("[libdc] reactor shutdown — closed all peer connections");
                        break;
                    }
                    handle_cmd(
                        cmd,
                        &mut peers,
                        &mut orphan_cands,
                        &stun,
                        &inbox,
                        &signals,
                        &reactor_cmd,
                        &reactor_connected,
                        &reactor_reachable,
                    );
                }
            })?;
        Ok(Self { cmd: cmd_tx, connected, reachable })
    }

    /// Number of peers currently connected (both channels open). A real,
    /// live mesh stat for the UI.
    pub fn peer_count(&self) -> usize {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether a peer has ever connected to us inbound — i.e. we're reachable from the
    /// outside and can volunteer as a relay (M4). Emergent: no NAT-type probing needed.
    pub fn reachable(&self) -> bool {
        self.reachable.load(Ordering::Relaxed)
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

    /// Close every peer connection and stop the reactor. Idempotent; safe to call even
    /// though detached tasks still hold clones (they become no-ops once the reactor exits).
    /// Call when abandoning a session (stop/re-watch) so the far side prunes us promptly.
    pub fn shutdown(&self) {
        let _ = self.cmd.send(Cmd::Shutdown);
    }
}

fn rtc_config(stun: &[String]) -> RtcConfig {
    let cfg = RtcConfig::new(stun);
    // Headless CI / loopback meshes: libjuice deliberately excludes 127.0.0.1 host
    // candidates (RFC 8445), so a same-host mesh gathers none and never connects. An
    // explicit bind address makes libjuice short-circuit and return exactly that address
    // as the single host candidate. Set `UNSTATION_BIND_ADDR=127.0.0.1` only for
    // tests/CI; production leaves it unset and gathers all interfaces as usual.
    match std::env::var("UNSTATION_BIND_ADDR") {
        Ok(addr) if !addr.is_empty() => cfg.bind_address(&addr),
        _ => cfg,
    }
}

/// Tune libdatachannel's global SCTP settings once, before any `PeerConnection` is
/// created (the C API applies them to newly-created connections only).
///
/// The stock defaults — a 256 KiB SCTP window, 200 ms delayed-SACK, a 3-MTU initial
/// congestion window — are the single biggest WAN throughput ceiling: goodput is
/// bounded by `window / RTT`, so at 200 ms RTT a relay tops out near 17 Mbit/s (≈2
/// viewers of a 1080p stream) regardless of link capacity. Lifting the window to 4 MiB
/// and dropping delayed-SACK to 20 ms takes the same path to ~167 Mbit/s — a ~10x
/// fan-out multiplier. Invisible on LAN (RTT≈0); decisive off-LAN. The safe `datachannel`
/// wrapper doesn't expose this, so we call the C binding directly. See
/// `docs/SCALING_RESEARCH.md` (Transport §1).
fn apply_sctp_settings() {
    static SCTP_INIT: Once = Once::new();
    SCTP_INIT.call_once(|| {
        let settings = datachannel_sys::rtcSctpSettings {
            recvBufferSize: 4 * 1024 * 1024,           // 4 MiB — covers a 200 ms BDP at ~167 Mbit/s
            sendBufferSize: 4 * 1024 * 1024,
            maxChunksOnQueue: 0,                       // 0 = libdatachannel optimized default
            initialCongestionWindow: 10,               // RFC 6928 (vs the 3-MTU default) — faster ramp
            maxBurst: 0,
            congestionControlModule: 0,                // RFC2581 (default); H-TCP measurably worsens drops
            delayedSackTimeMs: 20,                     // vs the 200 ms default that starves cwnd growth
            minRetransmitTimeoutMs: 100,               // vs ~1000 ms — a lost ctrl/SDP message recovers fast
            maxRetransmitTimeoutMs: 0,
            initialRetransmitTimeoutMs: 500,
            maxRetransmitAttempts: 0,
            heartbeatIntervalMs: 0,
        };
        // Safety: POD struct passed by const pointer; libdatachannel copies it synchronously.
        let rc = unsafe { datachannel_sys::rtcSetSctpSettings(&settings) };
        if rc < 0 {
            log::warn!("[transport] rtcSetSctpSettings failed (rc={rc}); using libdatachannel defaults");
        } else {
            log::info!("[transport] SCTP tuned for WAN: 4 MiB windows, delayed-SACK 20ms, ICW 10");
        }
    });
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
    reachable: &AtomicBool,
) {
    match cmd {
        Cmd::Dial(peer) => {
            // Glare/duplicate guard: never overwrite an in-flight or live connection
            // (insert below would Drop the existing PC mid-handshake).
            if peers.contains_key(&peer) {
                log::debug!("[libdc] dial {peer:?}: already connecting/connected, ignoring");
                return;
            }
            let conf = rtc_config(stun);
            let mut pc = match RtcPeerConnection::new(&conf, new_conn(peer, reactor_cmd, signals, inbox)) {
                Ok(pc) => pc,
                Err(e) => {
                    log::warn!("[libdc] dial {peer:?}: pc create failed: {e}");
                    return;
                }
            };
            // ctrl: reliable + ordered (default). bulk: unordered, no retransmits.
            let ctrl = match pc.create_data_channel("ctrl", dc_sink(peer, Channel::Ctrl, reactor_cmd, inbox)) {
                Ok(dc) => Some(dc),
                Err(e) => { log::warn!("[libdc] dial {peer:?}: ctrl channel create failed: {e}"); None }
            };
            let bulk_init =
                DataChannelInit::default().reliability(Reliability::default().unordered().max_retransmits(0));
            let bulk = match pc.create_data_channel_ex("bulk", dc_sink(peer, Channel::Bulk, reactor_cmd, inbox), &bulk_init) {
                Ok(dc) => Some(dc),
                Err(e) => { log::warn!("[libdc] dial {peer:?}: bulk channel create failed: {e}"); None }
            };
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
                    inbound: false, // we initiated (Dial) — outbound.
                },
            );
        }
        Cmd::Accept(peer, offer_json) => {
            // Duplicate-offer guard: keep the existing handshake rather than dropping
            // its PeerConnection by overwriting the map entry.
            if peers.contains_key(&peer) {
                log::debug!("[libdc] accept {peer:?}: already have a connection, ignoring duplicate offer");
                return;
            }
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
                inbound: true, // a peer reached IN to us (Accept) — proves reachability.
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
                    // Backpressure: never let the bulk send buffer grow without bound.
                    // For live video, drop the chunk (receiver re-requests on timeout)
                    // rather than buffer stale frames and balloon latency/memory.
                    if matches!(channel, Channel::Bulk) && dc.buffered_amount() > BULK_BUFFER_MAX {
                        log::debug!(
                            "[libdc] {peer:?}: bulk buffer {}B over limit — dropping chunk",
                            dc.buffered_amount()
                        );
                        return;
                    }
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
                    // A completed inbound connection proves we're reachable from the
                    // outside → we can volunteer as a relay (M4).
                    if p.inbound {
                        reachable.store(true, Ordering::Relaxed);
                    }
                    log::info!("[libdc] {peer:?}: PeerConnected — ctrl+bulk channels open");
                    let link: Arc<dyn Link> =
                        Arc::new(LibDcLink { remote: peer, cmd: reactor_cmd.clone() });
                    let _ = inbox.send(EngineEvent::PeerConnected { peer, link });
                }
            }
        }
        Cmd::StateChange(peer, state) => match state {
            // `Disconnected` is transient (brief packet loss) and usually recovers to
            // `Connected` on its own — tearing down here would turn a blip into a
            // permanent drop. Wait for the terminal `Failed`/`Closed`.
            ConnectionState::Disconnected => {
                log::debug!("[libdc] {peer:?}: ICE disconnected (transient) — awaiting recover/fail");
            }
            ConnectionState::Failed | ConnectionState::Closed => {
                orphan_cands.remove(&peer);
                if let Some(p) = peers.remove(&peer) {
                    if p.announced {
                        connected.fetch_sub(1, Ordering::Relaxed);
                        let _ = inbox.send(EngineEvent::PeerDisconnected { peer });
                    }
                }
            }
            _ => {}
        },
        Cmd::Close(peer) => {
            orphan_cands.remove(&peer);
            // Keep the live count honest and tell the node, same as a disconnect.
            if let Some(p) = peers.remove(&peer) {
                if p.announced {
                    connected.fetch_sub(1, Ordering::Relaxed);
                    let _ = inbox.send(EngineEvent::PeerDisconnected { peer });
                }
            }
        }
        // Handled by the reactor loop (drains peers + exits) before dispatch reaches here.
        Cmd::Shutdown => {}
    }
}
