//! Per-peer state and EWMA throughput/RTT estimators (TECH_SPEC §8.4).

use crate::buffermap::BufferMap;
use crate::types::PeerId;

/// Exponentially-weighted moving average, seeded on first sample.
#[derive(Clone, Copy, Debug)]
pub struct Ewma {
    value: f64,
    alpha: f64,
    seeded: bool,
}

impl Ewma {
    pub fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha, seeded: false }
    }
    pub fn update(&mut self, sample: f64) {
        if self.seeded {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        } else {
            self.value = sample;
            self.seeded = true;
        }
    }
    /// Current estimate, or `default` if no sample has been seen yet.
    pub fn or(&self, default: f64) -> f64 {
        if self.seeded {
            self.value
        } else {
            default
        }
    }
}

#[derive(Clone, Debug)]
pub struct PeerState {
    pub id: PeerId,
    pub buffer: BufferMap,
    pub throughput_bps: Ewma,
    pub rtt_ms: Ewma,
    pub pending_bytes: u64,
    /// Reputation in `[0, 1]`; decays on hash mismatch / repeated choke (TECH_SPEC §8.5).
    pub reputation: f64,
}

impl PeerState {
    pub fn new(id: PeerId) -> Self {
        Self {
            id,
            buffer: BufferMap::new(0),
            throughput_bps: Ewma::new(0.3),
            rtt_ms: Ewma::new(0.3),
            pending_bytes: 0,
            reputation: 1.0,
        }
    }
}
