//! In-memory statement store + a [`Signaling`] client over it (D3).
//!
//! Stands in for the Polkadot statement store so the discovery → SDP-exchange →
//! in-mesh-gossip-handoff flow is testable in-process. A write counter lets tests
//! assert the §7.3 scaling rule (statement-store writes are O(joins), not
//! O(connections)). The real client (`@parity/product-sdk-statement-store` over a
//! Paseo endpoint) is bridged through Tauri later; this exercises the same shape.

use crate::chat_codec;
use crate::clock::Clock;
use crate::signaling::{Presence, PresenceRecord, Signaling, SignalMsg, Subscription, TopicId};
use crate::topic::{discovery_topic, shard_for, signaling_topic};
use crate::types::{PeerId, StreamId};
use crate::BoxFuture;
use parity_scale_codec::{Decode, Encode};
use std::sync::{Arc, Mutex};

struct Stored {
    topic: TopicId,
    signer: PeerId,
    data: Vec<u8>,
    expiry_ms: u64,
}

#[derive(Clone, Default)]
pub struct MemStatementStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    statements: Vec<Stored>,
    writes: u64,
}

impl MemStatementStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, topic: TopicId, signer: PeerId, data: Vec<u8>, expiry_ms: u64) {
        let mut g = self.inner.lock().unwrap();
        g.writes += 1;
        g.statements.push(Stored { topic, signer, data, expiry_ms });
    }

    /// All live (`expiry_ms > now_ms`) statements on `topic`, as `(signer, data)`.
    pub fn read(&self, topic: TopicId, now_ms: u64) -> Vec<(PeerId, Vec<u8>)> {
        let g = self.inner.lock().unwrap();
        g.statements
            .iter()
            .filter(|s| s.topic == topic && s.expiry_ms > now_ms)
            .map(|s| (s.signer, s.data.clone()))
            .collect()
    }

    /// Total writes ever — the metered-budget proxy for the §7.3 assertion.
    pub fn writes(&self) -> u64 {
        self.inner.lock().unwrap().writes
    }
}

/// A [`Signaling`] client for one peer over a shared [`MemStatementStore`].
pub struct StatementSignaling {
    store: MemStatementStore,
    stream: StreamId,
    me: PeerId,
    n_shards: u32,
    ttl_s: u32,
    clock: Arc<dyn Clock>,
}

impl StatementSignaling {
    pub fn new(
        store: MemStatementStore,
        stream: StreamId,
        me: PeerId,
        n_shards: u32,
        ttl_s: u32,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { store, stream, me, n_shards, ttl_s, clock }
    }

    fn expiry(&self) -> u64 {
        self.clock.now_ms() + (self.ttl_s as u64) * 1000
    }

    /// Announce our presence into our discovery shard.
    pub fn publish_presence_now(&self, caps_upload_bps: u64) {
        let rec = PresenceRecord {
            peer_id: self.me.0,
            publisher: self.me.0,
            caps_upload_bps,
            ttl_s: self.ttl_s,
            manifest_cid: None,
            relay: false,
        };
        let topic = discovery_topic(&self.stream, shard_for(&self.me, self.n_shards));
        self.store.publish(topic, self.me, rec.encode(), self.expiry());
    }

    /// Read presence across all discovery shards, excluding self, deduped, capped.
    pub fn read_candidates(&self, max: usize) -> Vec<Presence> {
        let now = self.clock.now_ms();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for shard in 0..self.n_shards.max(1) {
            let topic = discovery_topic(&self.stream, shard);
            for (_signer, data) in self.store.read(topic, now) {
                if let Ok(rec) = PresenceRecord::decode(&mut &data[..]) {
                    let p: Presence = rec.into();
                    if p.peer_id != self.me && seen.insert(p.peer_id) {
                        out.push(p);
                        if out.len() >= max {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    /// Send an SDP/ICE signal to `to` (STREAM_MESH chat content on their sig topic).
    pub fn send_signal_now(&self, to: PeerId, msg: &SignalMsg) {
        let topic = signaling_topic(&self.stream, &to);
        self.store.publish(topic, self.me, chat_codec::encode_signal(msg), self.expiry());
    }

    /// Read signals addressed to us, as `(sender, msg)`.
    pub fn read_signals(&self) -> Vec<(PeerId, SignalMsg)> {
        let now = self.clock.now_ms();
        let topic = signaling_topic(&self.stream, &self.me);
        self.store
            .read(topic, now)
            .into_iter()
            .filter_map(|(signer, data)| chat_codec::decode_signal(&data).map(|m| (signer, m)))
            .collect()
    }
}

impl Signaling for StatementSignaling {
    fn publish_presence(&self, p: Presence) -> BoxFuture<'static, crate::Result<()>> {
        // Honor the caps from the presence; identity is always `me`.
        self.publish_presence_now(p.caps_upload_bps);
        Box::pin(async { Ok(()) })
    }

    fn read_presence(&self, topic: TopicId, max: usize) -> BoxFuture<'static, crate::Result<Vec<Presence>>> {
        let now = self.clock.now_ms();
        let raw = self.store.read(topic, now);
        let me = self.me;
        Box::pin(async move {
            let mut out = Vec::new();
            for (_s, data) in raw {
                if let Ok(rec) = PresenceRecord::decode(&mut &data[..]) {
                    let p: Presence = rec.into();
                    if p.peer_id != me {
                        out.push(p);
                        if out.len() >= max {
                            break;
                        }
                    }
                }
            }
            Ok(out)
        })
    }

    fn send_signal(&self, to: PeerId, msg: SignalMsg) -> BoxFuture<'static, crate::Result<()>> {
        self.send_signal_now(to, &msg);
        Box::pin(async { Ok(()) })
    }

    fn subscribe_edge(&self, _stream: StreamId) -> Subscription<crate::signaling::LiveEdge> {
        Subscription::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::VirtualClock;
    use crate::signaling::Signaling;

    fn sig(store: &MemStatementStore, me: PeerId, clock: Arc<VirtualClock>) -> StatementSignaling {
        StatementSignaling::new(store.clone(), StreamId([1u8; 32]), me, 2, 30, clock)
    }

    /// Exercise the `Signaling` trait surface (publish/read presence, send signal,
    /// subscribe) through a `&dyn Signaling`, the way the orchestrator uses it.
    #[test]
    fn statement_signaling_trait_surface_round_trips() {
        let store = MemStatementStore::new();
        let clock = Arc::new(VirtualClock::new());
        let (me, other) = (PeerId::from_u64(7), PeerId::from_u64(8));

        let s7 = sig(&store, me, clock.clone());
        let s8 = sig(&store, other, clock.clone());
        let a: &dyn Signaling = &s7;
        let b: &dyn Signaling = &s8;

        // Peer 8 announces; peer 7 reads peer 8's discovery shard and finds it (read_presence
        // filters out the reader's own record, so peer 7 must look at peer 8's shard).
        let pres8 = Presence { peer_id: other, publisher: other.0, caps_upload_bps: 9, ttl_s: 30, manifest_cid: None, relay: true };
        pollster::block_on(b.publish_presence(pres8)).expect("publish via trait");
        let topic8 = crate::topic::discovery_topic(&StreamId([1u8; 32]), shard_for(&other, 2));
        let found = pollster::block_on(a.read_presence(topic8, 8)).expect("read via trait");
        assert!(found.iter().any(|p| p.peer_id == other), "trait read_presence finds the other peer");
        assert!(found.iter().all(|p| p.peer_id != me), "read_presence excludes the reader's own record");

        // send_signal + subscribe_edge smoke (the trait wrappers over the inherent methods).
        pollster::block_on(a.send_signal(other, SignalMsg::Offer { sdp: vec![1, 2, 3] })).expect("send_signal");
        let _sub = a.subscribe_edge(StreamId([1u8; 32]));
    }
}
