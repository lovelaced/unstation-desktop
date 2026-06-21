//! Injected clock — the prerequisite for deterministic tests and reproducible benches.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic millisecond clock. Real time in production; a [`VirtualClock`] in the simulator.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// A virtual clock the simulator steps explicitly — no wall-clock dependence, so
/// scenarios are bit-for-bit reproducible.
#[derive(Default)]
pub struct VirtualClock {
    t: AtomicU64,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self { t: AtomicU64::new(0) }
    }
    pub fn advance(&self, ms: u64) {
        self.t.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.t.store(ms, Ordering::SeqCst);
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.t.load(Ordering::SeqCst)
    }
}

/// Wall-clock implementation for production.
pub struct SystemClock {
    start: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self { start: std::time::Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}
