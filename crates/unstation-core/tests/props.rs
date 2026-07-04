//! Property tests (D1): the codec/reassembly/buffer-map invariants that must hold
//! for arbitrary inputs, including hostile peer data.

use proptest::prelude::*;
use unstation_core::buffermap::BufferMap;
use unstation_core::crypto::segment_id;
use unstation_core::reassembly::Reassembler;
use unstation_core::signaling::{PresenceBook, PresenceRecord};
use unstation_core::types::PeerId;

proptest! {
    /// Any byte string, split into any chunk size and delivered out of order,
    /// reassembles identically and verifies against its content id.
    #[test]
    fn reassembly_roundtrip(
        data in proptest::collection::vec(any::<u8>(), 0..4096usize),
        chunk in 1usize..=512,
    ) {
        let id = segment_id(&data);
        let parts: Vec<(u32, Vec<u8>)> = data
            .chunks(chunk)
            .enumerate()
            .map(|(i, c)| ((i * chunk) as u32, c.to_vec()))
            .collect();
        let mut r = Reassembler::new(data.len() as u32);
        for (off, c) in parts.into_iter().rev() {
            r.add(off, &c);
        }
        prop_assert!(r.is_complete());
        let out = r.finish_verified(&id).unwrap();
        prop_assert_eq!(out, data);
    }

    /// A buffer map's `has`/`count` always reflect exactly the set of inserted seqs.
    #[test]
    fn buffermap_reflects_insertions(
        base in 0u64..1000,
        offsets in proptest::collection::vec(0u64..256, 0..64),
    ) {
        let mut b = BufferMap::new(base);
        let mut expected = std::collections::BTreeSet::new();
        for o in &offsets {
            b.set(base + o);
            expected.insert(base + o);
        }
        for o in 0u64..256 {
            prop_assert_eq!(b.has(base + o), expected.contains(&(base + o)));
        }
        prop_assert_eq!(b.count(), expected.len());
    }

    /// A buffer map survives a bytes round-trip: `has`/`count` are identical after
    /// `to_bytes` → `from_bytes` (the on-wire `BufferMap` advertisement).
    #[test]
    fn buffermap_bytes_roundtrip(
        base in 0u64..10_000,
        offsets in proptest::collection::vec(0u64..512, 0..96),
    ) {
        let mut b = BufferMap::new(base);
        let mut expected = std::collections::BTreeSet::new();
        for o in &offsets {
            b.set(base + o);
            expected.insert(base + o);
        }
        let b2 = BufferMap::from_bytes(base, &b.to_bytes());
        for o in 0u64..512 {
            prop_assert_eq!(b2.has(base + o), expected.contains(&(base + o)));
        }
        prop_assert_eq!(b2.count(), expected.len());
    }

    /// Hostile/overshooting chunks (offset at or past `total_len`) are ignored and
    /// never corrupt an otherwise-valid reassembly.
    #[test]
    fn reassembly_ignores_overshoot(
        data in proptest::collection::vec(any::<u8>(), 1..2048usize),
        chunk in 1usize..=256,
        junk in proptest::collection::vec(any::<u8>(), 1..256usize),
    ) {
        let id = segment_id(&data);
        let mut r = Reassembler::new(data.len() as u32);
        r.add(data.len() as u32, &junk);        // offset == total_len
        for (i, c) in data.chunks(chunk).enumerate() {
            r.add((i * chunk) as u32, c);
        }
        r.add((data.len() as u32) + 7, &junk);  // offset past the end
        prop_assert!(r.is_complete());
        prop_assert_eq!(r.finish_verified(&id).unwrap(), data);
    }

    /// `finish_verified` rejects content whose hash doesn't match the requested id —
    /// the line of defense against a peer serving forged bytes.
    #[test]
    fn reassembly_rejects_wrong_id(
        data in proptest::collection::vec(any::<u8>(), 1..1024usize),
    ) {
        let mut wrong = data.clone();
        wrong[0] ^= 0xFF;                  // a different payload ⇒ a different id
        let wrong_id = segment_id(&wrong);
        let mut r = Reassembler::new(data.len() as u32);
        r.add(0, &data);
        prop_assert!(r.is_complete());
        prop_assert!(r.finish_verified(&wrong_id).is_none());
    }

    /// `Reassembler::add`'s return values always sum to `buffered_bytes()` — the
    /// contract the node's global reassembly byte budget depends on. Any drift and
    /// the cap either leaks or starts refusing honest deliveries.
    #[test]
    fn reassembly_byte_accounting_never_drifts(
        total in 1u32..4096,
        chunks in proptest::collection::vec((0u32..4096, 1usize..256), 0..64),
    ) {
        let mut r = Reassembler::new(total);
        let mut accounted: u64 = 0;
        for (off, len) in chunks {
            accounted += r.add(off, &vec![0xA5u8; len]) as u64;
        }
        prop_assert_eq!(r.buffered_bytes(), accounted);
        prop_assert!(accounted <= total as u64, "never buffers past total_len");
    }

    /// The presence book never exceeds its cap and never admits an oversized CID,
    /// for arbitrary gossip batches — the invariant behind unbounded-peer safety.
    #[test]
    fn presence_book_stays_bounded(
        batches in proptest::collection::vec(
            proptest::collection::vec((any::<u8>(), any::<u8>(), 0u32..600, 0usize..200), 0..64),
            1..8,
        ),
    ) {
        let book = PresenceBook::new();
        let me = PeerId([255u8; 32]);
        for batch in batches {
            let recs: Vec<PresenceRecord> = batch
                .into_iter()
                .map(|(b0, b1, ttl, cid_len)| {
                    let mut id = [0u8; 32];
                    id[0] = b0;
                    id[1] = b1;
                    PresenceRecord {
                        peer_id: id,
                        publisher: id,
                        caps_upload_bps: 1,
                        ttl_s: ttl,
                        manifest_cid: if cid_len == 0 { None } else { Some("x".repeat(cid_len)) },
                        relay: b0 % 2 == 0,
                        enc_pub: None,
                    }
                })
                .collect();
            book.merge(recs, &me);
        }
        prop_assert!(book.len() <= 1024, "book cap held: {}", book.len());
        for rec in book.snapshot() {
            prop_assert!(rec.manifest_cid.map_or(0, |c| c.len()) <= 128, "no oversized CID admitted");
            prop_assert_ne!(PeerId(rec.peer_id), me, "own entry never merged");
        }
    }
}
