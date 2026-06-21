//! Buffer map: a bitfield of held segments relative to `base_seq` (TECH_SPEC §6.2).
//!
//! Advertised on connect (in `Hello`) and then every 500 ms or on material change.
//! Backed by a `u8` bitfield so `to_bytes`/`from_bytes` are byte-granular and
//! portable on the wire (a 256-segment window = 32 bytes).

use crate::types::Seq;
use bitvec::prelude::*;
use bitvec::view::BitView;

type Bits = BitVec<u8, Lsb0>;

#[derive(Clone, Debug, Default)]
pub struct BufferMap {
    base: Seq,
    bits: Bits,
}

impl BufferMap {
    pub fn new(base: Seq) -> Self {
        Self { base, bits: Bits::new() }
    }

    /// Reconstruct from a wire bitfield (`base_seq` + length-prefixed bytes).
    pub fn from_bytes(base: Seq, bytes: &[u8]) -> Self {
        Self { base, bits: bytes.view_bits::<Lsb0>().to_bitvec() }
    }

    pub fn base(&self) -> Seq {
        self.base
    }

    /// Does this map hold `seq`?
    pub fn has(&self, seq: Seq) -> bool {
        if seq < self.base {
            return false;
        }
        let idx = (seq - self.base) as usize;
        self.bits.get(idx).map(|b| *b).unwrap_or(false)
    }

    /// Mark `seq` present, growing the bitfield as needed.
    pub fn set(&mut self, seq: Seq) {
        if seq < self.base {
            return;
        }
        let idx = (seq - self.base) as usize;
        if idx >= self.bits.len() {
            self.bits.resize(idx + 1, false);
        }
        self.bits.set(idx, true);
    }

    /// Number of segments held.
    pub fn count(&self) -> usize {
        self.bits.count_ones()
    }

    /// Highest held sequence number, if any.
    pub fn highest(&self) -> Option<Seq> {
        self.bits.iter_ones().last().map(|i| self.base + i as Seq)
    }

    /// Serialize to the wire bitfield carried in `BufferMap`/`Hello`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bits.clone().into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_has_count() {
        let mut b = BufferMap::new(100);
        assert!(!b.has(100));
        assert!(!b.has(50)); // below base
        b.set(100);
        b.set(105);
        assert!(b.has(100));
        assert!(b.has(105));
        assert!(!b.has(104));
        assert_eq!(b.count(), 2);
        assert_eq!(b.highest(), Some(105));
    }

    #[test]
    fn bytes_roundtrip() {
        let mut b = BufferMap::new(0);
        for s in [0u64, 3, 7, 8, 20, 63] {
            b.set(s);
        }
        let restored = BufferMap::from_bytes(0, &b.to_bytes());
        for s in 0u64..=63 {
            assert_eq!(b.has(s), restored.has(s), "seq {s}");
        }
        assert_eq!(restored.count(), 6);
    }
}
