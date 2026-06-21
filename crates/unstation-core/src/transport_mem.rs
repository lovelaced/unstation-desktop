//! In-memory loopback transport — paired [`Link`]s over tokio channels.
//!
//! Used by tests and the simulator to exercise the real [`crate::node::MeshNode`]
//! loop (Hello / BufferMap / Want / SegmentData, reassembly, verify) without any
//! native WebRTC dependency. Delivery is instant; link latency/loss modelling lives
//! in the deterministic simulator, not here.

use crate::transport::{Channel, EngineEvent, Link};
use crate::types::PeerId;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

struct MemLink {
    /// This link's local identity — the remote will see `Inbound { peer: me }`.
    me: PeerId,
    remote: PeerId,
    /// The remote node's inbox.
    out: UnboundedSender<EngineEvent>,
}

impl Link for MemLink {
    fn remote(&self) -> PeerId {
        self.remote
    }
    fn send(&self, channel: Channel, bytes: Vec<u8>) {
        let _ = self.out.send(EngineEvent::Inbound { peer: self.me, channel, bytes });
    }
}

/// Wire nodes `a` and `b` together. Returns `(link_for_a, link_for_b)` — hand each
/// to the corresponding node via `EngineEvent::PeerConnected`. When `a` sends on
/// `link_for_a`, `b` receives `Inbound { peer: a, .. }`, and vice versa.
pub fn wire(
    a: PeerId,
    a_inbox: UnboundedSender<EngineEvent>,
    b: PeerId,
    b_inbox: UnboundedSender<EngineEvent>,
) -> (Arc<dyn Link>, Arc<dyn Link>) {
    let link_for_a: Arc<dyn Link> = Arc::new(MemLink { me: a, remote: b, out: b_inbox });
    let link_for_b: Arc<dyn Link> = Arc::new(MemLink { me: b, remote: a, out: a_inbox });
    (link_for_a, link_for_b)
}
