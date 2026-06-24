//! Property tests (D1): the codec/reassembly/buffer-map invariants that must hold
//! for arbitrary inputs, including hostile peer data.

use proptest::prelude::*;
use unstation_core::buffermap::BufferMap;
use unstation_core::crypto::segment_id;
use unstation_core::reassembly::Reassembler;

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
}
