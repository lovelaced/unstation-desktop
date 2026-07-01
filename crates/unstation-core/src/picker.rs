//! The deadline-aware piece-picker (TECH_SPEC §8).
//!
//! `plan_tick` is a **pure function** of a snapshot — window, peer buffer maps,
//! peer stats, deadlines, and an RNG — returning the request decisions for one
//! scheduler tick. Purity is what makes it unit-testable and bit-for-bit
//! replayable in the simulator.

use crate::buffermap::BufferMap;
use crate::config::PickerWeights;
use crate::types::{PeerId, Seq};
use rand::Rng;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Zone {
    /// Next ~3 s: earliest-deadline-first, ignore rarity, dual-peer + immediate fallback.
    Panic,
    /// Bulk of the buffer: hybrid utility with probabilistic spreading.
    Mid,
    /// Far edge: rarest-first, build upload value, no seed/Bulletin fallback.
    Prefetch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Peer(PeerId),
    Seed,
    Bulletin,
}

#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub seq: Seq,
    pub source: Source,
    /// A redundant (hedged) request — the loser is `Cancel`led on first chunk.
    pub redundant: bool,
}

/// An immutable snapshot of one neighbor for the duration of a tick.
pub struct PeerView<'a> {
    pub id: PeerId,
    pub buffer: &'a BufferMap,
    pub throughput_bps: f64,
    pub rtt_ms: f64,
    pub pending_bytes: u64,
    /// `[0, 1]` (TECH_SPEC §8.5): scales expected delivery time, so a peer with a
    /// history of forgeries/timeouts is continuously deprioritized well before any
    /// ban. Banned peers never appear in the view at all.
    pub reputation: f64,
}

/// All inputs the picker needs for one tick.
pub struct PickInput<'a> {
    pub play_seq: Seq,
    pub head_seq: Seq,
    pub now_ms: u64,
    pub window: u32,
    pub seg_bytes: u64,
    pub seg_ms: u64,
    pub peers: &'a [PeerView<'a>],
    pub weights: PickerWeights,
    pub seed_available: bool,
    pub bulletin_available: bool,
}

impl PickInput<'_> {
    fn deadline_ms(&self, seq: Seq) -> u64 {
        self.now_ms + seq.saturating_sub(self.play_seq) * self.seg_ms
    }

    fn time_to_deadline(&self, seq: Seq) -> u64 {
        self.deadline_ms(seq).saturating_sub(self.now_ms)
    }

    fn zone(&self, seq: Seq) -> Zone {
        if self.time_to_deadline(seq) <= self.weights.panic_horizon_ms {
            Zone::Panic
        } else if seq > self.play_seq + (self.window as u64) * 2 / 3 {
            Zone::Prefetch
        } else {
            Zone::Mid
        }
    }

    fn availability(&self, seq: Seq) -> usize {
        self.peers.iter().filter(|p| p.buffer.has(seq)).count()
    }

    /// Expected delivery time in ms: `(pending + seg)/throughput + RTT`, scaled up
    /// as reputation drops (TECH_SPEC §8.4/§8.5) — an unreliable peer has to *look*
    /// slow, or the picker keeps rewarding a fast forger with requests.
    fn expected_time_ms(&self, p: &PeerView) -> f64 {
        let bw = p.throughput_bps.max(1.0);
        (((p.pending_bytes + self.seg_bytes) as f64 * 8.0 / bw) * 1000.0 + p.rtt_ms)
            / p.reputation.clamp(0.1, 1.0)
    }

    fn holders_by_time(&self, seq: Seq) -> Vec<(&PeerView<'_>, f64)> {
        let mut v: Vec<_> = self
            .peers
            .iter()
            .filter(|p| p.buffer.has(seq))
            .map(|p| (p, self.expected_time_ms(p)))
            .collect();
        v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        v
    }

    fn utility(&self, seq: Seq) -> f64 {
        let dl = self.time_to_deadline(seq).max(1) as f64;
        let urgency = 1.0 / dl;
        let rarity = 1.0 / self.availability(seq).max(1) as f64;
        self.weights.w_d * urgency + self.weights.w_r * rarity
    }
}

/// Plan the requests for one scheduler tick. `have(seq)` reports whether the
/// local node already holds a segment.
pub fn plan_tick<R: Rng, F: Fn(Seq) -> bool>(
    input: &PickInput,
    have: F,
    rng: &mut R,
) -> Vec<Request> {
    let mut out = Vec::new();
    let end = input.play_seq + input.window as u64;

    // Normalization for Mid probabilistic spreading: the strongest mid candidate
    // is always requested; rarer/less-urgent ones are sampled relative to it.
    let mut u_max = f64::MIN_POSITIVE;
    for seq in input.play_seq..end {
        if seq > input.head_seq || have(seq) {
            continue;
        }
        if input.zone(seq) == Zone::Mid {
            u_max = u_max.max(input.utility(seq));
        }
    }

    for seq in input.play_seq..end {
        if seq > input.head_seq || have(seq) {
            continue; // not produced yet, or already held
        }
        match input.zone(seq) {
            Zone::Panic => {
                let holders = input.holders_by_time(seq);
                let slack = input.time_to_deadline(seq) as f64
                    + input.weights.fallback_slack_ms as f64;
                if holders.is_empty() || holders[0].1 > slack {
                    // No peer can meet the deadline — escalate the fallback chain.
                    if input.seed_available {
                        out.push(Request { seq, source: Source::Seed, redundant: false });
                    } else if input.bulletin_available {
                        out.push(Request { seq, source: Source::Bulletin, redundant: false });
                    }
                    if let Some((p, _)) = holders.first() {
                        out.push(Request { seq, source: Source::Peer(p.id), redundant: true });
                    }
                } else {
                    // Hedge: request from the top-2 holders, Cancel the loser later.
                    for (i, (p, _)) in holders.iter().take(2).enumerate() {
                        out.push(Request {
                            seq,
                            source: Source::Peer(p.id),
                            redundant: i > 0,
                        });
                    }
                }
            }
            Zone::Mid => {
                let holders = input.holders_by_time(seq);
                if holders.is_empty() {
                    continue;
                }
                // P(request) ∝ (U / U_max)^β  (TECH_SPEC §8.3) — avoids request stampedes.
                let p = (input.utility(seq) / u_max).powf(input.weights.beta).min(1.0);
                if rng.gen::<f64>() <= p {
                    out.push(Request {
                        seq,
                        source: Source::Peer(holders[0].0.id),
                        redundant: false,
                    });
                }
            }
            Zone::Prefetch => {
                // Rarest-first: build upload value on the scarce segments; no fallback here.
                if input.availability(seq) <= 2 {
                    if let Some((p, _)) = input.holders_by_time(seq).first() {
                        out.push(Request {
                            seq,
                            source: Source::Peer(p.id),
                            redundant: false,
                        });
                    }
                }
            }
        }
    }
    out
}
