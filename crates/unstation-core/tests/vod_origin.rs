//! D1 end-to-end (data layer): a VOD plays from the Bulletin origin floor with
//! full verification — fetch manifest by CID, verify the publisher signature,
//! then pull each content-addressed segment, reassemble, and hash-verify in order.
//!
//! This exercises crypto + reassembly + manifest + `OriginOfRecord` together.
//! Real chain transport (and visual playback via the localhost HLS re-server) is
//! wired in later milestones; here we prove the fetch+verify pipeline.

use bytes::Bytes;
use pollster::block_on;
use unstation_core::crypto;
use unstation_core::manifest::{Kind, Manifest, OriginOfRecord, SignedManifest, Track};
use unstation_core::origin_mem::MemoryOrigin;
use unstation_core::reassembly::Reassembler;
use unstation_core::types::StreamId;

#[test]
fn vod_plays_from_origin_with_verification() {
    let kp = crypto::keypair_from_seed(&[5u8; 32]);
    let pubkey = crypto::public_bytes(&kp);

    // A 6-segment VOD, each segment content-addressed on the origin floor.
    let origin = MemoryOrigin::new();
    let mut seg_ids = Vec::new();
    let mut originals: Vec<Bytes> = Vec::new();
    for i in 0..6u8 {
        let data = Bytes::from(vec![i; 4096]);
        originals.push(data.clone());
        seg_ids.push(origin.seed_segment(data));
    }

    let manifest = Manifest {
        stream_id: StreamId([1u8; 32]),
        kind: Kind::Vod,
        codec: "avc1.640028".into(),
        init_segment_cid: "bafyinit".into(),
        target_segment_ms: 2000,
        ll_mode: false,
        tracks: vec![Track { id: "v1080".into(), bitrate: 5_000_000, w: 1920, h: 1080 }],
        publisher: pubkey,
        created_at: 1,
            encrypted: false,
    };
    let sig = crypto::sign_sr25519(&kp, &manifest.signing_payload());
    let cid = block_on(origin.put_manifest(SignedManifest { manifest, sig })).unwrap();

    // Viewer resolves the signed manifest by CID and verifies it against the publisher.
    let fetched = block_on(origin.fetch_manifest(cid)).unwrap();
    fetched.verify(&pubkey).expect("manifest must verify against the publisher key");

    // Pull each segment from the origin, deliver it in 1 KiB chunks, reassemble, verify, "play".
    let mut played = 0usize;
    for (i, id) in seg_ids.iter().enumerate() {
        let bytes = block_on(origin.fetch_segment(*id)).unwrap();
        let mut r = Reassembler::new(bytes.len() as u32);
        for (off, c) in bytes.chunks(1024).enumerate().map(|(j, c)| ((j * 1024) as u32, c)) {
            r.add(off, c);
        }
        let verified = r.finish_verified(id).expect("segment must hash-verify");
        assert_eq!(verified.as_slice(), originals[i].as_ref());
        played += 1;
    }
    assert_eq!(played, 6);

    // A missing segment is a clean NotFound, not a panic.
    assert!(block_on(origin.fetch_segment(crypto::segment_id(b"never stored"))).is_err());
}
