//! Criterion benchmark: piece-picker tick latency vs peer count.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use unstation_core::buffermap::BufferMap;
use unstation_core::config::PickerWeights;
use unstation_core::picker::{plan_tick, PeerView, PickInput};
use unstation_core::types::PeerId;

fn make_peers(n: usize) -> Vec<BufferMap> {
    (0..n)
        .map(|_| {
            let mut b = BufferMap::new(0);
            for s in 0..256 {
                b.set(s);
            }
            b
        })
        .collect()
}

fn bench_picker(c: &mut Criterion) {
    for &n in &[8usize, 50, 200] {
        let bufs = make_peers(n);
        let peers: Vec<PeerView> = bufs
            .iter()
            .enumerate()
            .map(|(i, b)| PeerView {
                id: PeerId::from_u64(i as u64),
                buffer: b,
                throughput_bps: 5_000_000.0,
                rtt_ms: 80.0,
                pending_bytes: 0,
                reputation: 1.0,
            })
            .collect();
        let input = PickInput {
            play_seq: 0,
            head_seq: 256,
            now_ms: 1_000,
            window: 16,
            seg_bytes: 1_250_000,
            seg_ms: 2_000,
            peers: &peers,
            weights: PickerWeights::default(),
            seed_available: true,
            bulletin_available: true,
        };
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        c.bench_function(&format!("plan_tick_{n}peers_w16"), |b| {
            b.iter(|| {
                let r = plan_tick(black_box(&input), |_s| false, &mut rng);
                black_box(r.len());
            })
        });
    }
}

criterion_group!(benches, bench_picker);
criterion_main!(benches);
