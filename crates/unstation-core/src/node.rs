//! `MeshNode` — the single-actor async loop that drives the engine over a real
//! transport.
//!
//! One task owns all mutable state and consumes [`EngineEvent`]s off one mpsc
//! channel (no locks on the hot path). It serves `Want`s from the store as 16 KiB
//! `SegmentData` chunks, reassembles + hash-verifies inbound segments, feeds the
//! [`MediaSink`], and each tick runs the picker to issue new `Want`s.
//!
//! The `segment_ids` map (seq → content id) is the authenticated availability
//! learned from the signed manifest/live-edge; here it's provided up front
//! (live-edge propagation is D3). Verification is always against it.

use crate::buffermap::BufferMap;
use crate::config::MeshConfig;
use crate::media::MediaSink;
use crate::peer::PeerState;
use crate::picker::Source;
use crate::protocol::{Caps, MeshMsg};
use crate::reassembly::Reassembler;
use crate::transport::{Channel, EngineEvent, Link};
use crate::types::{PeerId, SegmentId, Seq};
use crate::{crypto, engine::MeshEngine};
use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};
use rand_chacha::ChaCha8Rng;
use rand::SeedableRng;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// 16 KiB — the safe cross-platform SCTP message size (TECH_SPEC §6.3).
const CHUNK: usize = 16 * 1024;

#[derive(Debug, Default, Clone)]
pub struct NodeStats {
    pub delivered: usize,
    pub peer_bytes: u64,
    pub hash_failures: u64,
    /// Connected neighbors in the peer table.
    pub peers: usize,
    /// Peers learned via in-mesh `PeerGossip` (no statement-store writes).
    pub known_peers: usize,
}

pub struct MeshNode {
    me: PeerId,
    eng: MeshEngine,
    sink: Arc<dyn MediaSink>,
    links: HashMap<PeerId, Arc<dyn Link>>,
    reasm: HashMap<Seq, Reassembler>,
    segment_ids: HashMap<Seq, SegmentId>,
    known_peers: HashSet<PeerId>,
    pending: HashMap<Seq, PeerId>,
    rng: ChaCha8Rng,
    now_ms: u64,
    stats: NodeStats,
}

impl MeshNode {
    /// A viewer that already knows the authenticated seq→id map (from the live edge)
    /// and the current head; it fetches from peers only (no seed/Bulletin in D2).
    pub fn new_viewer(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
        segment_ids: HashMap<Seq, SegmentId>,
        head_seq: Seq,
    ) -> Self {
        let mut eng = MeshEngine::new(cfg, seg_bytes);
        eng.head_seq = head_seq;
        eng.seed_available = false;
        eng.bulletin_available = false;
        Self::with_engine(me, eng, sink, segment_ids)
    }

    /// A publisher preloaded with the full VOD (genesis seed). Serves `Want`s; its
    /// own picker finds nothing missing.
    pub fn new_publisher(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
        segments: Vec<Bytes>,
    ) -> Self {
        let mut eng = MeshEngine::new(cfg, seg_bytes);
        let mut segment_ids = HashMap::new();
        for (i, bytes) in segments.into_iter().enumerate() {
            let seq = i as Seq;
            let id = crypto::segment_id(&bytes);
            eng.store.insert(seq, id, bytes);
            eng.local.set(seq);
            segment_ids.insert(seq, id);
        }
        eng.head_seq = segment_ids.len().saturating_sub(1) as Seq;
        Self::with_engine(me, eng, sink, segment_ids)
    }

    /// A publisher fed **live** (no preloaded VOD): segments arrive as
    /// [`EngineEvent::Produced`] from the segmenter, and the node serves them to
    /// the mesh as they land.
    pub fn new_live_publisher(
        me: PeerId,
        cfg: MeshConfig,
        seg_bytes: u64,
        sink: Arc<dyn MediaSink>,
    ) -> Self {
        Self::with_engine(me, MeshEngine::new(cfg, seg_bytes), sink, HashMap::new())
    }

    fn with_engine(
        me: PeerId,
        eng: MeshEngine,
        sink: Arc<dyn MediaSink>,
        segment_ids: HashMap<Seq, SegmentId>,
    ) -> Self {
        Self {
            me,
            eng,
            sink,
            links: HashMap::new(),
            reasm: HashMap::new(),
            segment_ids,
            known_peers: HashSet::new(),
            pending: HashMap::new(),
            rng: ChaCha8Rng::seed_from_u64(me.0[0] as u64 + 1),
            now_ms: 0,
            stats: NodeStats::default(),
        }
    }

    /// Run the actor loop until `Stop`, the inbox closes, or (for a viewer)
    /// `stop_at_count` segments are held locally.
    pub async fn run(
        mut self,
        mut inbox: UnboundedReceiver<EngineEvent>,
        tick: Duration,
        stop_at_count: Option<usize>,
    ) -> NodeStats {
        let tick_ms = tick.as_millis() as u64;
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.now_ms += tick_ms;
                    self.on_tick();
                }
                ev = inbox.recv() => {
                    match ev {
                        Some(EngineEvent::Stop) | None => break,
                        Some(e) => self.on_event(e),
                    }
                }
            }
            if let Some(target) = stop_at_count {
                if self.eng.local.count() >= target {
                    break;
                }
            }
        }
        self.stats.delivered = self.eng.local.count();
        self.stats.known_peers = self.known_peers.len();
        self.stats.peers = self.eng.peers.len();
        self.stats
    }

    fn on_event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::PeerConnected { peer, link } => {
                self.eng.peers.entry(peer).or_insert_with(|| PeerState::new(peer));
                self.links.insert(peer, link.clone());
                self.send_hello(&link);
            }
            EngineEvent::PeerDisconnected { peer } => {
                self.links.remove(&peer);
                self.eng.peers.remove(&peer);
            }
            EngineEvent::Inbound { peer, channel, bytes } => self.on_inbound(peer, channel, &bytes),
            EngineEvent::Produced { seq, id, bytes } => {
                // Publisher pipeline produced a segment — store it and start serving.
                self.eng.store.insert(seq, id, bytes);
                self.eng.local.set(seq);
                self.segment_ids.insert(seq, id);
                self.eng.head_seq = self.eng.head_seq.max(seq);
            }
            EngineEvent::LiveEdge { seq, id } => {
                // Learn that a segment exists + its content id (so we can fetch + verify).
                self.segment_ids.insert(seq, id);
                self.eng.head_seq = self.eng.head_seq.max(seq);
            }
            EngineEvent::Tick => self.on_tick(),
            EngineEvent::Stop => {}
        }
    }

    fn send_hello(&self, link: &Arc<dyn Link>) {
        let msg = MeshMsg::Hello {
            peer_id: self.me.0,
            stream_id: [0u8; 32],
            version: 1,
            caps: Caps { upload_bps: self.eng.cfg.upload_budget_bps, relay: false },
            base_seq: self.eng.local.base(),
            bitfield: self.eng.local.to_bytes(),
        };
        link.send(Channel::Ctrl, msg.encode());
    }

    fn on_tick(&mut self) {
        // Follow the player's play head so the picker's window (panic/mid/prefetch
        // zones) tracks real playback. The localhost HLS server advances it as the
        // viewer fetches segments; a publisher/seed sink reports 0 (no-op here).
        let head = self.sink.on_play_head();
        if head > self.eng.play_seq {
            self.eng.play_seq = head;
        }

        // Advertise our current buffer map.
        let bm = MeshMsg::BufferMap {
            base_seq: self.eng.local.base(),
            bitfield: self.eng.local.to_bytes(),
        };
        let encoded = bm.encode();
        for link in self.links.values() {
            link.send(Channel::Ctrl, encoded.clone());
        }

        // Run the picker and issue Wants to peers.
        let reqs = self.eng.plan(self.now_ms, &mut self.rng);
        for r in reqs {
            if let Source::Peer(pid) = r.source {
                if self.pending.contains_key(&r.seq) {
                    continue;
                }
                if let Some(link) = self.links.get(&pid).cloned() {
                    let want = MeshMsg::Want {
                        segment_seqs: vec![r.seq],
                        deadline_hint_ms: 0,
                    };
                    link.send(Channel::Ctrl, want.encode());
                    self.pending.insert(r.seq, pid);
                    if let Some(p) = self.eng.peers.get_mut(&pid) {
                        p.pending_bytes = p.pending_bytes.saturating_add(self.eng.seg_bytes);
                    }
                }
            }
        }
    }

    fn on_inbound(&mut self, peer: PeerId, _channel: Channel, bytes: &[u8]) {
        let msg = match MeshMsg::decode(&mut &bytes[..]) {
            Ok(m) => m,
            Err(_) => return, // hostile / malformed input is dropped, never fatal
        };
        match msg {
            MeshMsg::Hello { base_seq, bitfield, .. } | MeshMsg::BufferMap { base_seq, bitfield } => {
                let entry = self.eng.peers.entry(peer).or_insert_with(|| PeerState::new(peer));
                entry.buffer = BufferMap::from_bytes(base_seq, &bitfield);
            }
            MeshMsg::Want { segment_seqs, .. } => {
                for seq in segment_seqs {
                    if let Some(b) = self.eng.store.get(seq) {
                        if let Some(link) = self.links.get(&peer).cloned() {
                            self.send_segment(&link, seq, &b);
                        }
                    }
                }
            }
            MeshMsg::SegmentData { seq, total_len, offset, bytes, .. } => {
                self.on_segment_data(peer, seq, total_len, offset, &bytes);
            }
            MeshMsg::Have { seq } => {
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.buffer.set(seq);
                }
            }
            MeshMsg::Ping { nonce, t_send_ms } => {
                if let Some(link) = self.links.get(&peer).cloned() {
                    link.send(Channel::Ctrl, MeshMsg::Pong { nonce, t_send_ms }.encode());
                }
            }
            MeshMsg::PeerGossip { peers } => {
                // In-mesh peer discovery after bootstrap (TECH_SPEC §7.3): learned
                // over the data channel, never the statement store. The node holds
                // no signaling handle, so this provably incurs zero store writes.
                for p in peers {
                    let pid = PeerId(p);
                    if pid != self.me {
                        self.known_peers.insert(pid);
                    }
                }
            }
            // Cancel / Choke / Unchoke / Pong: no-ops for now.
            _ => {}
        }
    }

    fn send_segment(&self, link: &Arc<dyn Link>, seq: Seq, bytes: &[u8]) {
        let total = bytes.len() as u32;
        let mut offset = 0u32;
        for chunk in bytes.chunks(CHUNK) {
            let msg = MeshMsg::SegmentData {
                seq,
                track_id: 0,
                total_len: total,
                offset,
                bytes: chunk.to_vec(),
            };
            link.send(Channel::Bulk, msg.encode());
            offset += chunk.len() as u32;
        }
    }

    fn on_segment_data(&mut self, peer: PeerId, seq: Seq, total_len: u32, offset: u32, bytes: &[u8]) {
        if self.eng.local.has(seq) {
            return;
        }
        {
            let r = self.reasm.entry(seq).or_insert_with(|| Reassembler::new(total_len));
            r.add(offset, bytes);
            if !r.is_complete() {
                return;
            }
        }
        let r = self.reasm.remove(&seq).expect("present and complete");
        let expected = self.segment_ids.get(&seq).copied();
        match expected.and_then(|id| r.finish_verified(&id).map(|b| (id, b))) {
            Some((id, data)) => {
                self.stats.peer_bytes += data.len() as u64;
                let b = Bytes::from(data);
                self.eng.store.insert(seq, id, b.clone());
                self.eng.local.set(seq);
                self.sink.push_segment(seq, b);
                self.pending.remove(&seq);
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.throughput_bps.update(20_000_000.0);
                    p.rtt_ms.update(5.0);
                    p.pending_bytes = p.pending_bytes.saturating_sub(self.eng.seg_bytes);
                }
            }
            None => {
                // Hash mismatch (or unknown id): discard, decay reputation, re-request later.
                self.stats.hash_failures += 1;
                self.pending.remove(&seq);
                if let Some(p) = self.eng.peers.get_mut(&peer) {
                    p.reputation *= 0.5;
                }
            }
        }
    }
}
