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
}
