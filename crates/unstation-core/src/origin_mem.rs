//! In-memory [`OriginOfRecord`] — stands in for the Bulletin Chain in the
//! simulator and tests. The real chain client (Bulletin RPC / product-sdk
//! cloud-storage) is wired through the Tauri bridge in D3/D4.

use crate::manifest::{OriginOfRecord, SignedManifest};
use crate::types::{Cid, SegmentId};
use crate::{crypto, BoxFuture, Error, Result};
use bytes::Bytes;
use parity_scale_codec::Encode;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct MemoryOrigin {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    manifests: HashMap<Cid, SignedManifest>,
    segments: HashMap<SegmentId, Bytes>,
}

impl MemoryOrigin {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a segment by content id (synchronous helper for tests/seeding).
    pub fn seed_segment(&self, bytes: Bytes) -> SegmentId {
        let id = crypto::segment_id(&bytes);
        self.inner.lock().unwrap().segments.insert(id, bytes);
        id
    }
}

impl OriginOfRecord for MemoryOrigin {
    fn fetch_manifest(&self, cid: Cid) -> BoxFuture<'static, Result<SignedManifest>> {
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

    fn put_manifest(&self, m: SignedManifest) -> BoxFuture<'static, Result<Cid>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            // Content-address the signed bytes (the chain does the same via its preimage key).
            let cid = format!("mem://{}", crypto::hex32(&crypto::blake2b256(&m.encode())));
            inner.lock().unwrap().manifests.insert(cid.clone(), m);
            Ok(cid)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_fetch_segment_round_trips_and_missing_is_not_found() {
        let origin = MemoryOrigin::new();
        let bytes = Bytes::from(vec![9u8; 1234]);
        let id = crypto::segment_id(&bytes);
        let cid = pollster::block_on(origin.put_segment(id, bytes.clone())).expect("put_segment");
        assert!(cid.starts_with("mem://"));
        assert_eq!(pollster::block_on(origin.fetch_segment(id)).expect("fetch_segment"), bytes);
        let missing = crypto::segment_id(b"nope");
        assert!(matches!(pollster::block_on(origin.fetch_segment(missing)), Err(Error::NotFound)));
    }
}
