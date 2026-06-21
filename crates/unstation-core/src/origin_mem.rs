//! In-memory [`OriginOfRecord`] — stands in for the Bulletin Chain in the
//! simulator and tests. The real chain client (Bulletin RPC / product-sdk
//! cloud-storage) is wired through the Tauri bridge in D3/D4.

use crate::manifest::{Manifest, OriginOfRecord};
use crate::types::{Cid, SegmentId};
use crate::{crypto, BoxFuture, Error, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct MemoryOrigin {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    manifests: HashMap<Cid, Manifest>,
    segments: HashMap<SegmentId, Bytes>,
}

impl MemoryOrigin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_manifest(&self, cid: impl Into<Cid>, m: Manifest) {
        self.inner.lock().unwrap().manifests.insert(cid.into(), m);
    }

    /// Store a segment by content id (synchronous helper for tests/seeding).
    pub fn seed_segment(&self, bytes: Bytes) -> SegmentId {
        let id = crypto::segment_id(&bytes);
        self.inner.lock().unwrap().segments.insert(id, bytes);
        id
    }
}

impl OriginOfRecord for MemoryOrigin {
    fn fetch_manifest(&self, cid: Cid) -> BoxFuture<'static, Result<Manifest>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner
                .lock()
                .unwrap()
                .manifests
                .get(&cid)
                .cloned()
                .ok_or(Error::NotFound)
        })
    }

    fn fetch_segment(&self, id: SegmentId) -> BoxFuture<'static, Result<Bytes>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner
                .lock()
                .unwrap()
                .segments
                .get(&id)
                .cloned()
                .ok_or(Error::NotFound)
        })
    }

    fn put_segment(&self, id: SegmentId, bytes: Bytes) -> BoxFuture<'static, Result<Cid>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            inner.lock().unwrap().segments.insert(id, bytes);
            Ok(format!("mem://{}", crypto::hex32(&id.0)))
        })
    }
}
