//! Per-peer dial pacing: exponential backoff with per-peer jitter, and detection
//! of dials that hung mid-handshake. Pure state (no IO) so the schedule is
//! unit-testable; the connection maintainer drives it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unstation_core::types::PeerId;

/// First retry delay; doubles per failure up to [`BACKOFF_MAX`].
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A dial that hasn't produced a connection within this window is abandoned
/// (closed + backed off) so the transport will accept a fresh attempt.
///
/// Sized ABOVE the worst statement-store signaling round trip observed on-device, not
/// above the typical one: on WiFi offer→answer is 2–15 s, but on cellular right after a
/// network switch (half-dead RPC websocket riding out TCP timeouts) it measured
/// **33–81 s** — and every abandoned dial threw away an answer that was still in
/// flight, so the phone could never connect over 4G at all. The timeout's only job is
/// to free the transport's glare guard from truly dead attempts; retry pacing belongs
/// to the backoff and candidate freshness to the presence TTL, so a generous window
/// costs nothing on a healthy network (the connect clears it) and is the difference
/// between "slow join" and "never joins" on a degraded one.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(90);
/// Forget a peer's dial history after this long with no attempts — the map stays
/// bounded across hours of churn without a separate sweeper.
const HISTORY_TTL: Duration = Duration::from_secs(300);

struct DialState {
    attempts: u32,
    next_at: Instant,
    in_flight_since: Option<Instant>,
    touched: Instant,
}

/// Cheap-to-clone handle, shared between the maintainer loop and anything that
/// observes connection outcomes.
#[derive(Clone, Default)]
pub struct Dialer {
    inner: Arc<Mutex<HashMap<PeerId, DialState>>>,
}

impl Dialer {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<PeerId, DialState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// May we start a dial to `peer` right now? False while one is in flight or
    /// the peer is inside its backoff window.
    pub fn should_dial(&self, peer: &PeerId) -> bool {
        match self.guard().get(peer) {
            None => true,
            Some(st) => st.in_flight_since.is_none() && Instant::now() >= st.next_at,
        }
    }

    /// Record that a dial to `peer` was just started.
    pub fn record_started(&self, peer: PeerId) {
        let mut g = self.guard();
        let now = Instant::now();
        let st = g.entry(peer).or_insert(DialState {
            attempts: 0,
            next_at: now,
            in_flight_since: None,
            touched: now,
        });
        st.in_flight_since = Some(now);
        st.touched = now;
    }

    /// A connection to `peer` opened — clear its history (fresh start next time).
    pub fn record_connected(&self, peer: &PeerId) {
        self.guard().remove(peer);
    }

    /// A dial failed (or an established connection dropped): schedule the next
    /// attempt with exponential backoff, jittered per peer so a swarm that lost the
    /// same relay doesn't re-dial it in lockstep.
    pub fn record_failed(&self, peer: PeerId) {
        let mut g = self.guard();
        let now = Instant::now();
        let st = g.entry(peer).or_insert(DialState {
            attempts: 0,
            next_at: now,
            in_flight_since: None,
            touched: now,
        });
        st.attempts = st.attempts.saturating_add(1);
        st.in_flight_since = None;
        st.touched = now;
        let exp = BACKOFF_BASE.saturating_mul(1u32 << (st.attempts - 1).min(5)).min(BACKOFF_MAX);
        // Deterministic per-peer jitter in [0.8, 1.2] — stable for one peer (testable),
        // different across peers (staggers a swarm's retries).
        let jitter = 0.8 + (peer.0[0] as f64 / 255.0) * 0.4;
        st.next_at = now + exp.mul_f64(jitter);
    }

    /// Peers whose in-flight dial exceeded [`DIAL_TIMEOUT`] — the maintainer closes
    /// them (freeing the transport's glare guard) and records the failure. Also
    /// drops stale history so the map stays bounded.
    pub fn stalled(&self) -> Vec<PeerId> {
        let mut g = self.guard();
        g.retain(|_, st| st.in_flight_since.is_some() || st.touched.elapsed() < HISTORY_TTL);
        g.iter()
            .filter(|(_, st)| st.in_flight_since.map_or(false, |t| t.elapsed() >= DIAL_TIMEOUT))
            .map(|(p, _)| *p)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u8) -> PeerId {
        PeerId([n; 32])
    }

    #[test]
    fn fresh_peer_is_diallable_and_in_flight_is_not() {
        let d = Dialer::new();
        let p = pid(1);
        assert!(d.should_dial(&p));
        d.record_started(p);
        assert!(!d.should_dial(&p), "one dial at a time");
        d.record_connected(&p);
        assert!(d.should_dial(&p), "success resets the history");
    }

    #[test]
    fn failures_back_off_exponentially_with_a_cap() {
        let d = Dialer::new();
        let p = pid(0); // jitter factor 0.8 exactly (byte 0)
        // Peer byte 0 ⇒ jitter 0.8 exactly: waits are 1.6, 3.2, 6.4, … capped at 48 s.
        // Lower bounds allow a little test-runtime slack below the nominal value.
        for expect_min_s in [1.0f64, 3.0, 6.0, 12.0, 24.0, 47.5, 47.5, 47.5] {
            d.record_failed(p);
            assert!(!d.should_dial(&p), "inside the backoff window");
            let g = d.guard();
            let wait = g[&p].next_at.duration_since(Instant::now()).as_secs_f64();
            drop(g);
            assert!(
                wait >= expect_min_s && wait <= 48.1,
                "attempt wait {wait:.1}s outside [{expect_min_s}, 48.1] (cap 60 × 0.8 jitter)"
            );
        }
    }

    #[test]
    fn jitter_staggers_different_peers() {
        let d = Dialer::new();
        let (a, b) = (pid(0), pid(255));
        d.record_failed(a);
        d.record_failed(b);
        let g = d.guard();
        let wa = g[&a].next_at.duration_since(Instant::now());
        let wb = g[&b].next_at.duration_since(Instant::now());
        assert!(wb > wa, "peer-dependent jitter separates retry times");
    }

    #[test]
    fn stalled_reports_only_timed_out_dials() {
        let d = Dialer::new();
        let p = pid(3);
        d.record_started(p);
        assert!(d.stalled().is_empty(), "fresh dial is not stalled");
        // Simulate an old in-flight dial.
        d.guard().get_mut(&p).unwrap().in_flight_since =
            Some(Instant::now() - DIAL_TIMEOUT - Duration::from_secs(1));
        assert_eq!(d.stalled(), vec![p]);
        d.record_failed(p);
        assert!(d.stalled().is_empty(), "failure clears the in-flight state");
    }
}
