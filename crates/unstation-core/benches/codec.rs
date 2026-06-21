//! Criterion benchmarks: wire-codec throughput and content-hash verification.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use parity_scale_codec::{Decode, Encode};
use unstation_core::crypto::{blake2b256, segment_id, verify_segment};
use unstation_core::protocol::MeshMsg;

fn bench_codec(c: &mut Criterion) {
    let msg = MeshMsg::SegmentData {
        seq: 42,
        track_id: 0,
        total_len: 16384,
        offset: 0,
        bytes: vec![7u8; 16384],
    };
    let encoded = msg.encode();

    c.bench_function("meshmsg_encode_segmentdata_16k", |b| {
        b.iter(|| black_box(black_box(&msg).encode().len()))
    });
    c.bench_function("meshmsg_decode_segmentdata_16k", |b| {
        b.iter(|| {
            let m = MeshMsg::decode(&mut &encoded[..]).unwrap();
            black_box(m);
        })
    });

    let data = vec![3u8; 1_250_000];
    let id = segment_id(&data);
    c.bench_function("blake2b256_verify_1_25MB", |b| {
        b.iter(|| black_box(verify_segment(&data, &id)))
    });

    let chunk = vec![1u8; 65536];
    c.bench_function("blake2b256_hash_64k", |b| {
        b.iter(|| black_box(blake2b256(&chunk)))
    });
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
