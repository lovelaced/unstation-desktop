//! Two-tier segment store: a bounded in-memory map backed by a content-addressed
//! disk spill, serving both playback and upload.
//!
//! The in-memory tier holds the hot window; when it overflows, the oldest segment
//! is spilled to `dir/<segment_id_hex>.seg` (content-addressed, so identical bytes
//! dedup) and dropped from memory. `get` falls back to disk transparently.
//! A true access-ordered LRU is a later refinement; oldest-seq eviction matches the
//! live-window access pattern.

use crate::crypto;
use crate::types::{SegmentId, Seq};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct SegmentStore {
    mem: BTreeMap<Seq, Bytes>,
    /// seq → content id, retained even after a segment spills to disk.
    index: BTreeMap<Seq, SegmentId>,
    capacity: usize,
    dir: Option<PathBuf>,
}

impl SegmentStore {
    /// Memory-only store (used by the simulator and unit tests).
    pub fn new(capacity: usize) -> Self {
        Self { mem: BTreeMap::new(), index: BTreeMap::new(), capacity, dir: None }
    }

    /// Store with a disk spill directory (created if absent).
    pub fn with_disk(capacity: usize, dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { mem: BTreeMap::new(), index: BTreeMap::new(), capacity, dir: Some(dir) })
    }

    fn path_for(&self, id: &SegmentId) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{}.seg", crypto::hex32(&id.0))))
    }

    pub fn has(&self, seq: Seq) -> bool {
        if self.mem.contains_key(&seq) {
            return true;
        }
        match (self.index.get(&seq), self.dir.as_ref()) {
            (Some(id), Some(_)) => self.path_for(id).map(|p| p.exists()).unwrap_or(false),
            _ => false,
        }
    }

    /// Insert a (already hash-verified) segment, spilling the oldest to disk on overflow.
    pub fn insert(&mut self, seq: Seq, id: SegmentId, bytes: Bytes) {
        self.index.insert(seq, id);
        self.mem.insert(seq, bytes);
        while self.mem.len() > self.capacity {
            let oldest = match self.mem.keys().next().copied() {
                Some(k) => k,
                None => break,
            };
            if let Some(b) = self.mem.remove(&oldest) {
                self.spill(oldest, &b);
            }
        }
    }

    fn spill(&self, seq: Seq, bytes: &[u8]) {
        if let Some(id) = self.index.get(&seq) {
            if let Some(path) = self.path_for(id) {
                // Best-effort: a failed spill just means a re-fetch later.
                let _ = std::fs::write(path, bytes);
            }
        }
    }

    pub fn get(&self, seq: Seq) -> Option<Bytes> {
        if let Some(b) = self.mem.get(&seq) {
            return Some(b.clone());
        }
        let id = self.index.get(&seq)?;
        let path = self.path_for(id)?;
        std::fs::read(path).ok().map(Bytes::from)
    }

    /// Drop everything below `floor` — memory, index, and (best-effort) spilled files.
    /// A live viewer calls this as its play head advances, so a multi-hour stream can't
    /// grow the disk tier without bound; publishers/VOD keep everything and never call
    /// it. A spilled file is only unlinked when no retained seq still references the
    /// same content id (identical bytes dedup to one file).
    pub fn prune_below(&mut self, floor: Seq) {
        self.mem = self.mem.split_off(&floor);
        let dropped = {
            let kept = self.index.split_off(&floor);
            std::mem::replace(&mut self.index, kept)
        };
        if self.dir.is_some() {
            for id in dropped.values() {
                if !self.index.values().any(|kept| kept == id) {
                    if let Some(path) = self.path_for(id) {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_only_insert_get() {
        let mut s = SegmentStore::new(8);
        let bytes = Bytes::from_static(b"abc");
        let id = crypto::segment_id(&bytes);
        s.insert(1, id, bytes.clone());
        assert!(s.has(1));
        assert_eq!(s.get(1).unwrap(), bytes);
        assert!(!s.has(2));
    }

    #[test]
    fn prune_below_drops_memory_and_disk_but_keeps_shared_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SegmentStore::with_disk(1, dir.path().to_path_buf()).unwrap();
        // seq 0 and seq 5 share identical bytes (one content-addressed file);
        // seq 1 is unique. Capacity 1 forces everything old onto disk.
        let shared = Bytes::from(vec![7u8; 32]);
        let unique = Bytes::from(vec![9u8; 32]);
        s.insert(0, crypto::segment_id(&shared), shared.clone());
        s.insert(1, crypto::segment_id(&unique), unique.clone());
        s.insert(5, crypto::segment_id(&shared), shared.clone());

        s.prune_below(2);

        assert!(!s.has(0), "pruned seq gone");
        assert!(!s.has(1));
        assert!(s.has(5), "retained seq still readable");
        assert_eq!(s.get(5).unwrap(), shared, "shared content file survived the prune");
        assert_eq!(s.len(), 1);
        // The unique segment's spill file is actually unlinked.
        let unique_path = dir.path().join(format!("{}.seg", crypto::hex32(&crypto::segment_id(&unique).0)));
        assert!(!unique_path.exists(), "unreferenced spill file removed");
    }

    #[test]
    fn spills_to_disk_when_over_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SegmentStore::with_disk(2, dir.path().to_path_buf()).unwrap();
        // Insert 3 distinct segments into a capacity-2 memory tier.
        for i in 0..3u64 {
            let bytes = Bytes::from(vec![i as u8; 64]);
            let id = crypto::segment_id(&bytes);
            s.insert(i, id, bytes);
        }
        // Oldest (seq 0) was spilled but is still retrievable from disk.
        assert!(s.has(0));
        assert_eq!(s.get(0).unwrap(), Bytes::from(vec![0u8; 64]));
        assert!(s.has(2));
        assert_eq!(s.len(), 3);
    }
}
