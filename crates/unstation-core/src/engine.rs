//! `MeshEngine` — owns all mutable state (peer table, buffer maps, store, play/head
//! cursors) and turns it into picker decisions each tick.
//!
//! In production the engine is driven by a single-actor `tokio` loop that reads
//! events off one mpsc channel (no locks on the hot path); that wiring lands with
//! the real transport in D2. Here the engine exposes the synchronous, pure core
//! (`plan`) so the deterministic simulator and benchmarks can drive it directly.

use crate::buffermap::BufferMap;
use crate::config::MeshConfig;
use crate::peer::PeerState;
use crate::picker::{self, PeerView, PickInput, Request};
use crate::store::SegmentStore;
use crate::types::{PeerId, Seq};
use rand::Rng;
use std::collections::HashMap;

pub struct MeshEngine {
    pub cfg: MeshConfig,
    pub store: SegmentStore,
    pub local: BufferMap,
    pub peers: HashMap<PeerId, PeerState>,
    pub play_seq: Seq,
    pub head_seq: Seq,
    pub seed_available: bool,
    pub bulletin_available: bool,
    /// Nominal segment size in bytes (used for expected-delivery-time estimates).
    pub seg_bytes: u64,
}

impl MeshEngine {
    pub fn new(cfg: MeshConfig, seg_bytes: u64) -> Self {
        Self {
            cfg,
            store: SegmentStore::new(4096),
            local: BufferMap::new(0),
            peers: HashMap::new(),
            play_seq: 0,
            head_seq: 0,
            seed_available: false,
            bulletin_available: true,
            seg_bytes,
        }
    }

    pub fn have(&self, seq: Seq) -> bool {
        self.local.has(seq)
    }

    pub fn mark_have(&mut self, seq: Seq) {
        self.local.set(seq);
    }

    /// Plan the requests for one scheduler tick at `now_ms`.
    pub fn plan<R: Rng>(&self, now_ms: u64, rng: &mut R) -> Vec<Request> {
        let views: Vec<PeerView> = self
            .peers
            .values()
            .map(|p| PeerView {
                id: p.id,
                buffer: &p.buffer,
                throughput_bps: p.throughput_bps.or(5_000_000.0),
                rtt_ms: p.rtt_ms.or(80.0),
                pending_bytes: p.pending_bytes,
            })
            .collect();

        let input = PickInput {
            play_seq: self.play_seq,
            head_seq: self.head_seq,
            now_ms,
            window: self.cfg.window,
            seg_bytes: self.seg_bytes,
            seg_ms: self.cfg.seg_ms,
            peers: &views,
            weights: self.cfg.weights,
            seed_available: self.seed_available,
            bulletin_available: self.bulletin_available,
        };

        let local = &self.local;
        picker::plan_tick(&input, |s| local.has(s), rng)
    }
}
