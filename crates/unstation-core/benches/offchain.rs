//! Criterion benchmarks for the off-chain signaling hot paths (#17/#20): the live-edge
//! signature verify run on every gossiped edge, the new wire messages, and the in-mesh
//! presence book.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use parity_scale_codec::{Decode, Encode};
use unstation_core::crypto;
use unstation_core::protocol::MeshMsg;
use unstation_core::signaling::{PresenceBook, PresenceRecord};
use unstation_core::types::PeerId;

fn rec(i: u64) -> PresenceRecord {
    PresenceRecord {
        peer_id: PeerId::from_u64(i).0,
        publisher: PeerId::from_u64(i).0,
        caps_upload_bps: 20_000_000,
        ttl_s: 30,
        manifest_cid: if i % 5 == 0 { Some("bafy-some-manifest-cid-placeholder".into()) } else { None },
        relay: i % 4 == 0,
        enc_pub: None,
    }
}

fn bench_offchain(c: &mut Criterion) {
    // ---- signed live-edge sign/verify (verify runs on every gossiped edge) ----
    let kp = crypto::keypair_from_seed(&[9u8; 32]);
    let pk = crypto::public_bytes(&kp);
    // Representative edge payload: domain tag ‖ stream(32) ‖ seq(8) ‖ id(32).
    let mut payload = b"unstation-edge-v1".to_vec();
    payload.extend_from_slice(&[5u8; 32]);
    payload.extend_from_slice(&123u64.to_le_bytes());
    payload.extend_from_slice(&[7u8; 32]);
    let sig = crypto::sign_sr25519(&kp, &payload);
    c.bench_function("edge_sign", |b| b.iter(|| black_box(crypto::sign_sr25519(&kp, black_box(&payload)))));
    c.bench_function("edge_verify", |b| {
        b.iter(|| black_box(crypto::verify_sr25519(&pk, black_box(&payload), &sig)))
    });

    // ---- new wire messages ----
    let edge = MeshMsg::EdgeAnnounce { seq: 123, id: [7u8; 32], sig };
    let edge_enc = edge.encode();
    c.bench_function("edge_announce_encode", |b| b.iter(|| black_box(black_box(&edge).encode().len())));
    c.bench_function("edge_announce_decode", |b| {
        b.iter(|| black_box(MeshMsg::decode(&mut &edge_enc[..]).unwrap()))
    });

    let gossip = MeshMsg::PresenceGossip { records: (0..32).map(rec).collect() };
    let gossip_enc = gossip.encode();
    c.bench_function("presence_gossip_encode_32", |b| b.iter(|| black_box(black_box(&gossip).encode().len())));
    c.bench_function("presence_gossip_decode_32", |b| {
        b.iter(|| black_box(MeshMsg::decode(&mut &gossip_enc[..]).unwrap()))
    });

    // ---- presence book (in-mesh discovery) ----
    let me = PeerId::from_u64(999_999);
    let incoming: Vec<PresenceRecord> = (0..32).map(rec).collect();
    c.bench_function("presence_book_merge_32_into_500", |b| {
        b.iter_batched(
            || {
                let bk = PresenceBook::new();
                for i in 0..500 {
                    bk.insert(rec(1_000 + i));
                }
                bk
            },
            |bk| {
                bk.merge(black_box(incoming.clone()), &me);
                black_box(bk.len())
            },
            BatchSize::SmallInput,
        )
    });
    let big = PresenceBook::new();
    for i in 0..500 {
        big.insert(rec(1_000 + i));
    }
    c.bench_function("presence_book_sample_32_of_500", |b| b.iter(|| black_box(big.sample(32).len())));
}

criterion_group!(benches, bench_offchain);
criterion_main!(benches);
