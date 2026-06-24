//! Performance tests against the deterministic picker (no real network, seeded RNG
//! ⇒ reproducible). Extends the D0 simulator to MULTIPLE peers with independent
//! serialized links, modeling each peer's in-flight queue so the queue-aware picker
//! (`expected_time = (pending+seg)/throughput + rtt`) load-balances the way it does
//! on a real device. Proves the core value prop — many modest peers aggregate into
//! one good stream — and that peer selection prefers faster links.

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use unstation_core::config::{MeshConfig, Mode, PickerWeights, Role};
use unstation_core::engine::MeshEngine;
use unstation_core::peer::PeerState;
use unstation_core::picker::Source;
use unstation_core::types::PeerId;

struct SimResult {
    play_seq: u64,
    stalls: u64,
    per_peer: Vec<u64>, // segments delivered by each input peer, in order
}

/// One viewer pulling a 5 Mbps live stream from `links` peers (each `(uplink_bps,
/// rtt_ms)`), each on its own serialized link, for `total_ms` of virtual time.
fn run_multi(links: &[(u64, u64)], total_ms: u64) -> SimResult {
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
    eng.seed_available = false;
    eng.bulletin_available = true;

    let ids: Vec<PeerId> = (0..links.len()).map(|i| PeerId::from_u64((i + 1) as u64)).collect();
    let mut bw = HashMap::new();
    let mut rtt = HashMap::new();
    let mut link_free = HashMap::new();
    let mut idx = HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        eng.peers.insert(*id, PeerState::new(*id));
        bw.insert(*id, links[i].0);
        rtt.insert(*id, links[i].1);
        link_free.insert(*id, 0u64);
        idx.insert(*id, i);
    }

    let mut rng = ChaCha8Rng::seed_from_u64(7);
    let tick_ms = 100u64;
    let startup = 2u64;

    let mut inflight: Vec<(u64, u64, PeerId)> = Vec::new(); // (done_at, seq, from)
    let mut requested: HashSet<u64> = HashSet::new();
    let mut per_peer = vec![0u64; links.len()];

    let mut now = 0u64;
    let mut playing = false;
    let mut last_advance = 0u64;
    let mut stalls = 0u64;

    while now <= total_ms {
        eng.head_seq = now / seg_ms;
        for id in &ids {
            let p = eng.peers.get_mut(id).unwrap();
            for s in 0..=eng.head_seq {
                p.buffer.set(s);
            }
            p.throughput_bps.update(bw[id] as f64);
            p.rtt_ms.update(rtt[id] as f64);
        }

        // Complete deliveries; free the serving peer's queue.
        let mut still = Vec::new();
        for (done, seq, from) in inflight.drain(..) {
            if done <= now {
                eng.mark_have(seq);
                if let Some(p) = eng.peers.get_mut(&from) {
                    p.pending_bytes = p.pending_bytes.saturating_sub(seg_bytes);
                }
            } else {
                still.push((done, seq, from));
            }
        }
        inflight = still;

        for r in eng.plan(now, &mut rng) {
            if let Source::Peer(pid) = r.source {
                if !eng.have(r.seq) && requested.insert(r.seq) {
                    let start = link_free[&pid].max(now);
                    let tx_ms = (seg_bytes * 8 * 1_000) / bw[&pid];
                    *link_free.get_mut(&pid).unwrap() = start + tx_ms;
                    inflight.push((start + tx_ms + rtt[&pid], r.seq, pid));
                    if let Some(p) = eng.peers.get_mut(&pid) {
                        p.pending_bytes += seg_bytes;
                    }
                    per_peer[idx[&pid]] += 1;
                }
            }
        }

        if !playing && (eng.local.count() as u64) >= startup && eng.have(0) {
            playing = true;
            eng.play_seq = 0;
            last_advance = now;
        }
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

    SimResult { play_seq: eng.play_seq, stalls, per_peer }
}

#[test]
fn aggregates_bandwidth_across_peers() {
    // A single 2 Mbps peer cannot sustain the 5 Mbps stream...
    let single = run_multi(&[(2_000_000, 80)], 30_000);
    assert!(single.stalls > 0, "one 2 Mbps peer must stall (stalls={})", single.stalls);

    // ...but three 2 Mbps peers aggregate to ~6 Mbps and carry it.
    let triple = run_multi(&[(2_000_000, 80), (2_000_000, 80), (2_000_000, 80)], 30_000);
    assert!(
        triple.stalls < single.stalls,
        "aggregating 3 peers must reduce stalls (single={}, triple={})",
        single.stalls,
        triple.stalls
    );
    assert!(triple.play_seq >= single.play_seq, "aggregate stream keeps up at least as well");
    // The queue-aware picker must spread load — every peer should serve some segments.
    assert!(
        triple.per_peer.iter().all(|&c| c > 0),
        "load should spread across all peers, got {:?}",
        triple.per_peer
    );
}

#[test]
fn no_stall_across_rtt_sweep() {
    // An ample (30 Mbps) single link should carry a 5 Mbps stream at any sane RTT.
    for rtt in [20u64, 80, 200] {
        let r = run_multi(&[(30_000_000, rtt)], 30_000);
        assert!(r.stalls <= 1, "ample bandwidth at {rtt}ms RTT should not stall (stalls={})", r.stalls);
        assert!(r.play_seq >= 8, "play head should advance at {rtt}ms RTT (got {})", r.play_seq);
    }
}

/// Count how often each of two equal-capacity-except-one-dimension peers is chosen,
/// over many planning ticks. `pending_bytes` is left at 0 so the only differentiator
/// is the dimension under test.
fn pick_tally(thr: [f64; 2], rtt: [f64; 2], samples: u64) -> [u64; 2] {
    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Viewer,
        window: 16,
        tick: Duration::from_millis(100),
        seg_ms: 2_000,
        upload_budget_bps: 0,
        weights: PickerWeights::default(),
    };
    let mut eng = MeshEngine::new(cfg, 1_250_000);
    eng.head_seq = 60; // far ahead, so the whole window is requestable
    eng.seed_available = false;
    eng.bulletin_available = false;
    let ids = [PeerId::from_u64(1), PeerId::from_u64(2)];
    for (i, id) in ids.iter().enumerate() {
        let mut ps = PeerState::new(*id);
        for s in 0..=eng.head_seq {
            ps.buffer.set(s);
        }
        ps.throughput_bps.update(thr[i]);
        ps.rtt_ms.update(rtt[i]);
        eng.peers.insert(*id, ps);
    }
    let mut rng = ChaCha8Rng::seed_from_u64(11);
    let mut tally = [0u64; 2];
    // plan() is pure (no local mutation), so repeated calls just resample the RNG.
    for _ in 0..samples {
        for r in eng.plan(0, &mut rng) {
            if let Source::Peer(p) = r.source {
                if p == ids[0] {
                    tally[0] += 1;
                } else if p == ids[1] {
                    tally[1] += 1;
                }
            }
        }
    }
    tally
}

#[test]
fn picker_prefers_lower_rtt_peer() {
    // Equal throughput; peer 0 has far lower RTT ⇒ lower expected delivery time.
    let t = pick_tally([10_000_000.0, 10_000_000.0], [20.0, 250.0], 200);
    assert!(
        t[0] > t[1],
        "the lower-RTT peer must be picked more often (low={}, high={})",
        t[0],
        t[1]
    );
}

#[test]
fn picker_prefers_higher_throughput_peer() {
    // Equal RTT; peer 0 has far higher throughput ⇒ lower expected delivery time.
    let t = pick_tally([25_000_000.0, 2_000_000.0], [80.0, 80.0], 200);
    assert!(
        t[0] > t[1],
        "the higher-throughput peer must be picked more often (fast={}, slow={})",
        t[0],
        t[1]
    );
}

#[test]
fn panic_zone_always_covers_the_most_urgent_segment() {
    // At the live edge, the segment due next (lowest missing seq) is earliest-
    // deadline-first: it must be requested on every tick until held.
    let cfg = MeshConfig {
        mode: Mode::Live,
        role: Role::Viewer,
        window: 16,
        tick: Duration::from_millis(100),
        seg_ms: 2_000,
        upload_budget_bps: 0,
        weights: PickerWeights::default(),
    };
    let mut eng = MeshEngine::new(cfg, 1_250_000);
    eng.head_seq = 30;
    eng.play_seq = 5;
    eng.seed_available = false;
    eng.bulletin_available = false;
    let id = PeerId::from_u64(1);
    let mut ps = PeerState::new(id);
    for s in 0..=eng.head_seq {
        ps.buffer.set(s);
    }
    ps.throughput_bps.update(30_000_000.0);
    ps.rtt_ms.update(50.0);
    eng.peers.insert(id, ps);

    let mut rng = ChaCha8Rng::seed_from_u64(3);
    for _ in 0..20 {
        let reqs = eng.plan(0, &mut rng);
        assert!(
            reqs.iter().any(|r| r.seq == eng.play_seq),
            "the most urgent segment (seq {}) must always be requested",
            eng.play_seq
        );
    }
}
