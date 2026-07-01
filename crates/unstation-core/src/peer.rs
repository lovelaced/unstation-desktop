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
    /// Reputation in `[0, 1]` (TECH_SPEC §8.5): decays on forged bytes, request
    /// timeouts, and protocol abuse; heals slowly on verified deliveries. The picker
    /// scales expected delivery time by it, and crossing the floor bans the peer.
    pub reputation: f64,
    /// Count of accepted-then-never-served requests (buffer-map lies / dead links).
    pub strikes: u32,
    /// Reputation crossed the floor: choked, disconnected, and barred from re-dial
    /// (via the session's shared `BanList`) until the ban expires.
    pub banned: bool,
    /// Upload fairness (TECH_SPEC §8.5). `choked` = WE are withholding upload from this
    /// peer (not in one of our upload slots); `choked_by` = THEY told us they won't serve
    /// us (so the picker shouldn't waste `Want`s on them). Both default to "open".
    pub choked: bool,
    pub choked_by: bool,
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
            strikes: 0,
            banned: false,
            choked: false,
            choked_by: false,
        }
    }
}
