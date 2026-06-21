//! libdatachannel-backed transport (datachannel-rs, MPL-2.0).
//!
//! **D2 status:** the native build is proven (libdatachannel compiles via CMake and
//! a real `DataChannel` round-trips our wire protocol — see `tests/libdc_loopback.rs`).
//!
//! datachannel-rs's `RtcPeerConnection`/`RtcDataChannel` are **not `Send`/`Sync`**
//! (only `RtcConfig` is). The [`Link`](unstation_core::transport::Link) trait,
//! however, requires `Send + Sync`. So a production [`Link`] cannot hold the
//! `RtcDataChannel` directly — it must be owned by a dedicated thread (paired with
//! the node actor) and driven via a command channel: `Link::send` posts bytes to
//! that channel, and inbound `on_message` callbacks post `EngineEvent::Inbound`
//! into the node's inbox. That wiring lands with the transport actor; here we
//! expose the reusable handler types and prove the native path end to end.

pub use unstation_core::transport::{Channel, Link};

use datachannel::{
    DataChannelHandler, DataChannelInfo, IceCandidate, PeerConnectionHandler, RtcDataChannel,
    SessionDescription,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// Forwards a data channel's lifecycle to plain channels: inbound bytes to a
/// `Sender`, and an `opened` flag flipped on `on_open`.
pub struct DcSink {
    pub incoming: Sender<Vec<u8>>,
    pub opened: Arc<AtomicBool>,
}

impl DataChannelHandler for DcSink {
    fn on_open(&mut self) {
        self.opened.store(true, Ordering::SeqCst);
    }
    fn on_message(&mut self, msg: &[u8]) {
        let _ = self.incoming.send(msg.to_vec());
    }
}

/// A peer-connection handler that ferries locally-generated SDP and ICE to
/// channels (the test/actor pumps them to the remote), stashes an inbound data
/// channel, and builds a [`DcSink`] for each channel.
pub struct Conn {
    pub local_desc: Sender<SessionDescription>,
    pub local_cand: Sender<IceCandidate>,
    pub incoming: Sender<Vec<u8>>,
    pub opened: Arc<AtomicBool>,
    pub recv_dc: Arc<Mutex<Option<Box<RtcDataChannel<DcSink>>>>>,
}

impl PeerConnectionHandler for Conn {
    type DCH = DcSink;

    fn data_channel_handler(&mut self, _info: DataChannelInfo) -> DcSink {
        DcSink { incoming: self.incoming.clone(), opened: self.opened.clone() }
    }
    fn on_description(&mut self, desc: SessionDescription) {
        let _ = self.local_desc.send(desc);
    }
    fn on_candidate(&mut self, cand: IceCandidate) {
        let _ = self.local_cand.send(cand);
    }
    fn on_data_channel(&mut self, dc: Box<RtcDataChannel<DcSink>>) {
        *self.recv_dc.lock().unwrap() = Some(dc);
    }
}
