//! Reassembles a segment from `SegmentData` chunks (16 KiB on the wire) and
//! verifies the result against its content id. Chunks may arrive out of order
//! (the `bulk` channel is unordered), so reassembly is keyed by `(offset)`.

use crate::crypto;
use crate::types::SegmentId;
use std::collections::BTreeMap;

pub struct Reassembler {
    total_len: u32,
    chunks: BTreeMap<u32, Vec<u8>>,
    /// Running sum of `chunks` lengths — kept so `add` can enforce the total-len
    /// bound in O(1) and `buffered_bytes` is a field read, not a fold.
    buffered: u32,
}

impl Reassembler {
    pub fn new(total_len: u32) -> Self {
        Self { total_len, chunks: BTreeMap::new(), buffered: 0 }
    }

    /// Add a chunk at `offset`, returning how many bytes were actually buffered (0 for
    /// duplicates and rejects) so the caller can keep a global byte budget. Empty,
    /// out-of-range, and **overshooting** chunks are ignored — a peer must not be able
    /// to write past `total_len`. Crucially, total buffered bytes are capped at
    /// `total_len`: overlapping chunks at distinct offsets would otherwise let a
    /// hostile peer inflate one reassembler far past the segment size (amplification
    /// past the per-entry admission estimate).
    pub fn add(&mut self, offset: u32, bytes: &[u8]) -> usize {
        let len = bytes.len() as u32;
        if len == 0 || offset.saturating_add(len) > self.total_len {
            return 0;
        }
        if self.buffered.saturating_add(len) > self.total_len {
            return 0; // would exceed the segment size ⇒ overlap/duplication, reject
        }
        match self.chunks.entry(offset) {
            std::collections::btree_map::Entry::Occupied(_) => 0,
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(bytes.to_vec());
                self.buffered += len;
                len as usize
            }
        }
    }

    /// Bytes currently buffered — the counterpart of [`Reassembler::add`]'s return
    /// value, released back to the caller's budget when this reassembler is dropped.
    pub fn buffered_bytes(&self) -> u64 {
        self.buffered as u64
    }

    /// All bytes present and contiguous from 0..total_len.
    pub fn is_complete(&self) -> bool {
        let mut expected = 0u32;
        for (&off, data) in &self.chunks {
            if off != expected {
                return false;
            }
            expected = expected.saturating_add(data.len() as u32);
        }
        expected == self.total_len
    }

    /// Concatenate contiguous chunks, or `None` if there's a gap.
    pub fn assemble(self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::with_capacity(self.total_len as usize);
        for (_, data) in self.chunks {
            out.extend_from_slice(&data);
        }
        Some(out)
    }

    /// Assemble and verify against `expected`. `None` on gap or hash mismatch ⇒
    /// the segment is discarded and re-requested elsewhere (TECH_SPEC §6.3).
    pub fn finish_verified(self, expected: &SegmentId) -> Option<Vec<u8>> {
        let bytes = self.assemble()?;
        if crypto::verify_segment(&bytes, expected) {
            Some(bytes)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_out_of_order_and_verifies() {
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let id = crypto::segment_id(&data);
        let mut r = Reassembler::new(data.len() as u32);
        // feed in reverse-chunk order
        let chunk = 64usize;
        let parts: Vec<(u32, &[u8])> = data
            .chunks(chunk)
            .enumerate()
            .map(|(i, c)| ((i * chunk) as u32, c))
            .collect();
        for (off, c) in parts.into_iter().rev() {
            r.add(off, c);
        }
        assert!(r.is_complete());
        assert_eq!(r.finish_verified(&id).unwrap(), data);
    }

    #[test]
    fn gap_is_incomplete() {
        let mut r = Reassembler::new(100);
        r.add(0, &[0u8; 32]);
        // skip 32..64
        r.add(64, &[0u8; 36]);
        assert!(!r.is_complete());
        assert!(r.assemble().is_none());
    }

    #[test]
    fn add_reports_buffered_bytes_and_rejects_free_of_charge() {
        let mut r = Reassembler::new(100);
        assert_eq!(r.add(0, &[1u8; 40]), 40, "fresh chunk buffers its bytes");
        assert_eq!(r.add(0, &[2u8; 40]), 0, "duplicate offset is free");
        assert_eq!(r.add(90, &[3u8; 20]), 0, "overshoot rejected");
        assert_eq!(r.add(40, &[]), 0, "empty chunk rejected");
        assert_eq!(r.buffered_bytes(), 40, "accounting matches what add reported");
    }
}
