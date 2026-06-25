//! [`OriginOfRecord`] over the Polkadot Bulletin chain (preimage / transaction storage).
//!
//! Bulletin holds the small, durable trust + boot anchor — the **signed manifest** and
//! the **init segment** — content-addressed by Bulletin's own preimage key. Bulk segment
//! bytes are deliberately NOT stored here: the metered allowance (~tens of MB) can't hold
//! a multi-GB stream, so that's the CDN/origin floor's job. Consequently `fetch_segment`
//! by our `SegmentId` is always `NotFound` — Bulletin is addressed by content key, fetched
//! via [`BulletinOrigin::fetch_bytes`] / [`OriginOfRecord::fetch_manifest`].

use bytes::Bytes;
use parity_scale_codec::{Decode, Encode};
use unstation_core::manifest::{OriginOfRecord, SignedManifest};
use unstation_core::types::{Cid, SegmentId};
use unstation_core::{BoxFuture, Error, Result};
use useragent_native::chain::bulletin;

/// Stateless handle to the Bulletin origin (the SDK client is process-global, like the
/// statement store).
#[derive(Clone, Copy, Default)]
pub struct BulletinOrigin;

impl BulletinOrigin {
    pub fn new() -> Self {
        Self
    }

    /// Fetch arbitrary content-addressed bytes by Bulletin CID (e.g. the manifest's
    /// `init_segment_cid`). Used directly by the publish/watch wiring for the init segment.
    pub async fn fetch_bytes(&self, cid: &str) -> Result<Bytes> {
        match bulletin::lookup_preimage_testnet(cid).await {
            Ok(Some(v)) => Ok(Bytes::from(v)),
            Ok(None) => Err(Error::NotFound),
            Err(e) => Err(Error::Signaling(e)),
        }
    }

    /// Store arbitrary bytes (e.g. the init segment); returns the Bulletin CID.
    pub async fn put_bytes(&self, bytes: Vec<u8>) -> Result<Cid> {
        bulletin::submit_preimage_testnet(bytes).await.map_err(Error::Signaling)
    }
}

impl OriginOfRecord for BulletinOrigin {
    fn fetch_manifest(&self, cid: Cid) -> BoxFuture<'static, Result<SignedManifest>> {
        Box::pin(async move {
            match bulletin::lookup_preimage_testnet(&cid).await {
                Ok(Some(bytes)) => SignedManifest::decode(&mut &bytes[..])
                    .map_err(|e| Error::Signaling(format!("manifest decode: {e}"))),
                Ok(None) => Err(Error::NotFound),
                Err(e) => Err(Error::Signaling(e)),
            }
        })
    }

    fn put_manifest(&self, manifest: SignedManifest) -> BoxFuture<'static, Result<Cid>> {
        Box::pin(async move {
            bulletin::submit_preimage_testnet(manifest.encode())
                .await
                .map_err(Error::Signaling)
        })
    }

    /// Bulletin keys by its own preimage hash (not our `SegmentId`) and bulk segments
    /// aren't stored here — always `NotFound`. Fetch the init segment by its manifest
    /// CID via [`BulletinOrigin::fetch_bytes`].
    fn fetch_segment(&self, _id: SegmentId) -> BoxFuture<'static, Result<Bytes>> {
        Box::pin(async move { Err(Error::NotFound) })
    }

    /// Store a small (e.g. init) segment; returns its Bulletin CID. The caller uses the
    /// returned CID — Bulletin content-addresses by its own key, not our `SegmentId`.
    fn put_segment(&self, _id: SegmentId, bytes: Bytes) -> BoxFuture<'static, Result<Cid>> {
        Box::pin(async move {
            bulletin::submit_preimage_testnet(bytes.to_vec())
                .await
                .map_err(Error::Signaling)
        })
    }
}
