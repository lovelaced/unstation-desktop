//! Deterministic mesh simulator (D0).
//!
//! A stepped, virtual-clock simulation with a simple serialized-link network
//! model (RTT + bandwidth). It drives the real [`MeshEngine`] picker, delivers
//! segments per the link model, and measures playback continuity. Seeded RNG ⇒
//! bit-for-bit reproducible. This is where the picker is tuned before any real
//! device (IMPLEMENTATION_SPEC §11).

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::collections::HashSet;
use std::time::Duration;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::engine::MeshEngine;
use unstation_core::peer::PeerState;
use unstation_core::picker::Source;
use unstation_core::types::PeerId;

struct SimResult {
    playing: bool,
    play_seq: u64,
    stalls: u64,
    peer_segments: u64,
}

/// One viewer pulling a 5 Mbps live stream from a single publisher peer over a
/// link with the given uplink bandwidth and RTT, for `total_ms` of virtual time.
fn run_single_publisher(bw_bps: u64, rtt_ms: u64, total_ms: u64) -> SimResult {
    let seg_ms = 2_000u64;
    let bitrate = 5_000_000u64;
    let seg_bytes = bitrate / 8 * seg_ms / 1_000; // 1_250_000 bytes/segment

    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Viewer,
        window: 16,
        tick: Duration::from_millis(100),
        seg_ms,
        upload_budget_bps: 0,
        weights: PickerWeights::default(),
    };
    let mut eng = MeshEngine::new(cfg, seg_bytes);

    let pubid = PeerId::from_u64(1);
    eng.peers.insert(pubid, PeerState::new(pubid));
    eng.seed_available = false;
    eng.bulletin_available = true;

    let mut rng = ChaCha8Rng::seed_from_u64(42);
    let tick_ms = 100u64;
    let startup = 2u64; // startup buffer before playback begins

    let mut link_free_at = 0u64; // serialized-link availability time
    let mut inflight: Vec<(u64, u64)> = Vec::new(); // (done_at_ms, seq)
    let mut requested: HashSet<u64> = HashSet::new();

    let mut now = 0u64;
    let mut playing = false;
    let mut last_advance = 0u64;
    let mut stalls = 0u64;
    let mut peer_segments = 0u64;

    while now <= total_ms {
        // Publisher produces a new segment every seg_ms; its buffer holds 0..=head.
        eng.head_seq = now / seg_ms;
        {
            let p = eng.peers.get_mut(&pubid).unwrap();
            for s in 0..=eng.head_seq {
                p.buffer.set(s);
            }
            p.throughput_bps.update(bw_bps as f64);
            p.rtt_ms.update(rtt_ms as f64);
        }

        // Complete any deliveries whose time has come.
        let mut still = Vec::new();
        for (done, seq) in inflight.drain(..) {
            if done <= now {
                eng.mark_have(seq);
            } else {
                still.push((done, seq));
            }
        }
        inflight = still;

        // Plan the tick; schedule peer transfers on the serialized link.
        let reqs = eng.plan(now, &mut rng);
        for r in reqs {
            if let Source::Peer(_) = r.source {
                if !eng.have(r.seq) && requested.insert(r.seq) {
                    let start = link_free_at.max(now);
                    let tx_ms = (seg_bytes * 8 * 1_000) / bw_bps;
                    link_free_at = start + tx_ms;
                    inflight.push((start + tx_ms + rtt_ms, r.seq));
                    peer_segments += 1;
                }
            }
        }

        // Start playback once a startup buffer is present.
        if !playing && (eng.local.count() as u64) >= startup && eng.have(0) {
            playing = true;
            eng.play_seq = 0;
            last_advance = now;
        }
        // Advance the play head every seg_ms; live deadline miss => skip forward.
        if playing && now >= last_advance + seg_ms {
            let next = eng.play_seq + 1;
            if next <= eng.head_seq {
                if !eng.have(next) {
                    stalls += 1;
                }
                eng.play_seq = next;
                last_advance = now;
            }
        }

        now += tick_ms;
    }

    SimResult { playing, play_seq: eng.play_seq, stalls, peer_segments }
}

#[test]
fn cold_start_single_publisher_no_stalls() {
    // 30 Mbps uplink, 80 ms RTT, 5 Mbps stream — the publisher alone bootstraps fine.
    let r = run_single_publisher(30_000_000, 80, 30_000);
    assert!(r.playing, "viewer should start playback");
    assert!(r.peer_segments > 0, "segments should be delivered from the publisher peer");
    assert!(r.play_seq >= 8, "play head should advance (got {})", r.play_seq);
    assert!(r.stalls <= 1, "ample bandwidth should not stall (stalls={})", r.stalls);
}

#[test]
fn starved_link_stalls() {
    // 2 Mbps uplink cannot sustain a 5 Mbps stream — the model must surface stalls.
    let r = run_single_publisher(2_000_000, 80, 30_000);
    assert!(r.playing, "viewer still starts playback from the startup buffer");
    assert!(r.stalls > 0, "an under-provisioned link should stall (stalls={})", r.stalls);
}
