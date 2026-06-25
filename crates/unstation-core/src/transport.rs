//! Transport abstraction — WebRTC data channels in production, injected into the node.
//!
//! The node sees two things: a [`Link`] per connected peer (to send framed
//! `MeshMsg` bytes on the `ctrl`/`bulk` channel) and a stream of [`EngineEvent`]s
//! (connections + inbound bytes). Implemented by `transport-libdc` (D2/native) and
//! by the in-memory [`crate::transport_mem`] loopback used in tests/sim.

use crate::types::{PeerId, SegmentId, Seq};
use crate::BoxFuture;
use bytes::Bytes;
use std::sync::Arc;

/// Session Description Protocol payload (raw bytes, carried over signaling).
pub type Sdp = Vec<u8>;

/// The two channels per peer: reliable-ordered control vs unreliable-unordered bulk
/// (so a late segment chunk never head-of-line-blocks control — TECH_SPEC §6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Ctrl,
    Bulk,
}

/// A connected peer link. `send` is fire-and-forget; backpressure (the data
/// channel's `bufferedAmount` threshold) is handled inside the implementation.
pub trait Link: Send + Sync {
    fn remote(&self) -> PeerId;
    fn send(&self, channel: Channel, bytes: Vec<u8>);
}

/// Events the node's single-actor loop consumes.
pub enum EngineEvent {
    PeerConnected { peer: PeerId, link: Arc<dyn Link> },
    Inbound { peer: PeerId, channel: Channel, bytes: Vec<u8> },
    PeerDisconnected { peer: PeerId },
    /// Publisher-side, locally injected: the segmenter produced a content-addressed
    /// segment. The node stores it and starts serving it to the mesh.
    Produced { seq: Seq, id: SegmentId, bytes: Bytes },
    /// Viewer-side, locally injected: the signed live-edge announced a segment's
    /// content id, so the node knows it exists and how to verify it (TECH_SPEC §6.4).
    LiveEdge { seq: Seq, id: SegmentId },
    /// Viewer-side, locally injected once the publisher is discovered and its signed
    /// manifest verifies: the publisher pubkey to authenticate gossiped live-edge
    /// announcements against (off-chain signaling, TECH_SPEC §6.4).
    SetPublisherKey { key: [u8; 32] },
    /// Scheduler tick (also emitted by a timer in production).
    Tick,
    Stop,
}

/// Callback invoked when an inbound offer arrives via signaling.
pub type OfferHandler = Box<dyn Fn(PeerId, Sdp) + Send + Sync>;

/// High-level transport: establishes [`Link`]s from signaling. The libdatachannel
/// implementation lands in D2/native; the engine only depends on this trait + `Link`.
pub trait Transport: Send + Sync {
    fn connect(&self, peer: PeerId, offer: Sdp) -> BoxFuture<'static, crate::Result<Arc<dyn Link>>>;
    fn listen(&self, on_offer: OfferHandler);
}
