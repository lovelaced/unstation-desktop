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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_advances_and_sets() {
        let c = VirtualClock::new();
        assert_eq!(c.now_ms(), 0);
        c.advance(100);
        c.advance(50);
        assert_eq!(c.now_ms(), 150);
        c.set(42);
        assert_eq!(c.now_ms(), 42);
    }

    #[test]
    fn system_clock_is_monotonic_nonnegative() {
        let c = SystemClock::default();
        let a = c.now_ms();
        let b = c.now_ms();
        assert!(b >= a, "wall clock must not go backwards");
    }
}
