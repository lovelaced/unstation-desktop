//! The `Signaling` trait — backed by the Polkadot statement store.
//!
//! Used **only** for the initial SDP/ICE exchange to establish the first 1–3
//! data-channel links; after that, peer discovery is gossiped in-mesh
//! (`PeerGossip`) to respect the scarce per-user slot budget (TECH_SPEC §7.3).

use crate::types::{PeerId, StreamId};
use crate::BoxFuture;
use parity_scale_codec::{Decode, Encode};
use std::marker::PhantomData;

/// A presence announcement published to the (sharded) discovery topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub peer_id: PeerId,
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
}

/// SCALE wire form of a presence record (the statement `data` payload).
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq)]
pub struct PresenceRecord {
    pub peer_id: [u8; 32],
    pub caps_upload_bps: u64,
    pub ttl_s: u32,
}

impl From<&Presence> for PresenceRecord {
    fn from(p: &Presence) -> Self {
        Self { peer_id: p.peer_id.0, caps_upload_bps: p.caps_upload_bps, ttl_s: p.ttl_s }
    }
}
impl From<PresenceRecord> for Presence {
    fn from(r: PresenceRecord) -> Self {
        Self { peer_id: PeerId(r.peer_id), caps_upload_bps: r.caps_upload_bps, ttl_s: r.ttl_s }
    }
}

/// 32-byte topic hash = `BLAKE2b-256(..)` (sharded discovery, TECH_SPEC §7.2).
pub type TopicId = [u8; 32];

/// The SDP-over-statement messages — carried via the app's chat codec with a
/// `STREAM_MESH` purpose (see [`crate::chat_codec`]). `offer_id` links an answer/
/// candidate/closed back to its offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalMsg {
    Offer { sdp: Vec<u8> },
    Answer { offer_id: String, sdp: Vec<u8> },
    IceCandidate { offer_id: String, sdp: Vec<u8> },
    Closed { offer_id: String },
}

/// The publisher's signed current-segment pointer (TECH_SPEC §6.4).
#[derive(Clone, Debug)]
pub struct LiveEdge {
    pub head_seq: u64,
    pub segment_seqs: Vec<u64>,
}

/// Handle to a live-edge subscription. Concrete stream wiring lands with the live path.
pub struct Subscription<T> {
    _marker: PhantomData<T>,
}

impl<T> Default for Subscription<T> {
    fn default() -> Self {
        Self { _marker: PhantomData }
    }
}

pub trait Signaling: Send + Sync {
    fn publish_presence(&self, p: Presence) -> BoxFuture<'static, crate::Result<()>>;
    fn read_presence(&self, topic: TopicId, max: usize) -> BoxFuture<'static, crate::Result<Vec<Presence>>>;
    fn send_signal(&self, to: PeerId, msg: SignalMsg) -> BoxFuture<'static, crate::Result<()>>;
    fn subscribe_edge(&self, stream: StreamId) -> Subscription<LiveEdge>;
}
