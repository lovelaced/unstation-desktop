//! Real chain integration for Unstation, backed by an external Polkadot Rust SDK.
//! Implements `unstation-core`'s [`Signaling`] trait
//! over the Polkadot People-chain statement store, replacing the in-memory mock
//! (`statement_store_mem`).
//!
//! The SDK's statement-store client is **process-global**: call
//! [`init_statement_store`] once at startup (with the host's signing keypair)
//! before constructing a [`ChainSignaling`]. Endpoints default to the
//! `PASEO_NEXT_V2` People chain (`wss://paseo-people-next-system-rpc.polkadot.io`
//! — the same chain the QR sign-in pairs against); override with the
//! `HOST_STATEMENT_STORE_WS_ENDPOINTS` env var.
//!
//! Scope (M0): presence publish/read + targeted SDP `send_signal` round-trip
//! through the real chain. The live-edge subscription (`subscribe_edge`) and the
//! `OriginOfRecord`/Bulletin impl land in M1/M2.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use parity_scale_codec::{Decode, Encode};
use unstation_core::chat_codec;
use unstation_core::signaling::{
    LiveEdge, Presence, PresenceRecord, SignalMsg, Signaling, Subscription, TopicId,
};
use unstation_core::topic::{discovery_topic, edge_topic, shard_for, signaling_topic};
use unstation_core::types::{PeerId, SegmentId, Seq, StreamId};
use unstation_core::BoxFuture;

use useragent_native::chain::statement_store as ss;

mod bulletin;
pub use bulletin::BulletinOrigin;

/// The host's signing secret (32-byte seed or 64-byte sr25519 secret), retained when
/// the identity is initialized so we can sign the stream manifest with the SAME key
/// the statement store + presence are signed with. Its public bytes are the publisher
/// trust anchor a viewer learns from presence. Set once per process.
static IDENTITY_SECRET: OnceLock<Vec<u8>> = OnceLock::new();

/// Build a schnorrkel keypair from a 32-byte seed or a 64-byte sr25519 secret
/// (key ‖ nonce). Keeps `schnorrkel` version handling in one place.
fn keypair_from_secret(secret: &[u8]) -> Result<schnorrkel::Keypair, String> {
    match secret.len() {
        64 => Ok(schnorrkel::SecretKey::from_bytes(secret)
            .map_err(|e| format!("invalid 64-byte slot key: {e}"))?
            .to_keypair()),
        32 => {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(secret);
            Ok(schnorrkel::MiniSecretKey::from_bytes(&seed)
                .map_err(|e| format!("invalid 32-byte slot seed: {e}"))?
                .expand_to_keypair(schnorrkel::ExpansionMode::Ed25519))
        }
        n => Err(format!("statement-store slot key must be 32 or 64 bytes, got {n}")),
    }
}

/// Sign `payload` with the host identity (the same key as presence/statements) — for
/// the stream manifest. Uses the manifest signing context, so
/// [`unstation_core::manifest::SignedManifest::verify`] accepts it. `None` if no
/// identity has been initialized.
pub fn sign_with_identity(payload: &[u8]) -> Option<[u8; 64]> {
    let secret = IDENTITY_SECRET.get()?;
    let kp = keypair_from_secret(secret).ok()?;
    Some(unstation_core::crypto::sign_sr25519(&kp, payload))
}

/// The host's **personhood** public key (the statement-store account) — the manifest's
/// `publisher` field and the trust anchor a viewer verifies the signed manifest + live-edge
/// against. Stable across all of a person's devices. `None` until the identity is
/// initialized. NOTE: this is deliberately NOT [`local_peer_id`] — that is a per-device
/// routing id, distinct so two devices of the same person don't collide in the mesh.
pub fn identity_public() -> Option<[u8; 32]> {
    ss::public_key_bytes()
}

/// Point the statement-store client at specific WS endpoint(s) — e.g. a local `--dev`
/// node for e2e tests — overriding the default Paseo endpoint and the
/// `HOST_STATEMENT_STORE_WS_ENDPOINTS` env var. Call BEFORE [`init_statement_store`].
pub fn set_statement_store_endpoint(ws_endpoints: Vec<String>) {
    ss::set_endpoint_override(ws_endpoints);
}

/// Initialize the process-global statement store with the host's signing keypair
/// (from `WalletManager::statement_store_keypair`). Call once at startup. The
/// background subscription/poll thread starts immediately; `auto_provision`
/// requests a metered allowance on testnet builds.
pub fn init_statement_store(keypair: schnorrkel::Keypair) {
    let _ = IDENTITY_SECRET.set(keypair.secret.to_bytes().to_vec());
    ss::init_with_keypair(None, keypair, true);
}

/// Initialize the statement store from a raw 64-byte Substrate sr25519 secret
/// (32-byte key ‖ 32-byte nonce) — e.g. the QR-paired per-app **slot signing key**
/// the phone granted an on-chain statement-store allowance. Keeps `schnorrkel`
/// version handling inside this crate so callers just pass bytes.
pub fn init_statement_store_from_secret(secret: &[u8]) -> Result<(), String> {
    let keypair = keypair_from_secret(secret)?;
    // Retain the secret so we can sign the stream manifest with this same identity.
    let _ = IDENTITY_SECRET.set(secret.to_vec());
    ss::init_with_keypair(None, keypair, false);
    Ok(())
}

/// Initialize the Bulletin allowance signer from a raw 64-byte sr25519 slot secret —
/// the phone-granted `//allowance//bulletin//<product>` key. After this, durable
/// manifest / init-segment writes via [`BulletinOrigin`] are signed by (and sponsored
/// through) that allowance account instead of the SDK's unfunded Alice dev key. Safe to
/// call without it — Bulletin then falls back to the Alice dev key (local `--dev` only).
pub fn init_bulletin_from_secret(secret: &[u8]) -> Result<(), String> {
    let keypair = keypair_from_secret(secret)?;
    useragent_native::chain::bulletin::init_with_keypair(keypair);
    Ok(())
}

/// Initialize the statement store from a BIP-39 mnemonic — the derived account must
/// already carry a statement-store allowance on the target chain (e.g. a personhood-
/// provisioned key). For the public-Paseo nightly smoke test.
pub fn init_from_mnemonic(mnemonic: &str) -> Result<(), String> {
    use useragent_native::wallet::WalletManager;
    let mut wallet = WalletManager::new_ephemeral();
    wallet.load_from_mnemonic(mnemonic).map_err(|e| format!("load mnemonic: {e:?}"))?;
    let kp = wallet.statement_store_keypair().map_err(|e| format!("statement-store keypair: {e:?}"))?;
    init_statement_store(kp);
    Ok(())
}

/// Like [`init_statement_store`] but loads-or-generates a *persistent* signing key
/// under `key_dir`, so the host keeps the same statement-store identity across
/// launches (it stays signed in). `key_dir` should be a per-app, per-platform
/// data directory (e.g. the OS app-data dir).
pub fn init_statement_store_persisted(key_dir: &std::path::Path) {
    ss::init_with_options(Some(key_dir), true);
}

/// Best-effort wait for the background subscription to connect.
pub fn wait_ready(timeout: Duration) -> bool {
    ss::wait_until_subscribed(timeout)
}

/// Per-**device** mesh `PeerId`: a process-random routing id used for presence,
/// signaling-envelope addressing, dial targeting, and self-filtering — **not** a
/// signing key and **not** the personhood/statement-store pubkey.
///
/// Historically this WAS the statement-store account pubkey, which made every device a
/// person signs into share one `PeerId`; two such devices then filtered each other out of
/// discovery as "self" (`p.peer_id != my_peer`) and collided on the per-peer signaling
/// topic — so cross-machine watch between a person's own devices could never connect.
/// Decoupling it fixes that. Trust is unaffected: the personhood key (the manifest/edge
/// signer + verify anchor) is still exposed via [`identity_public`] and carried in
/// [`Presence::publisher`]. The id need not persist across launches — presence is
/// re-announced each session with a short TTL — so a fresh per-process value is fine and
/// avoids any on-disk key management.
///
/// `None` until an identity is initialized (preserves the "not signed in yet" signal
/// callers rely on).
pub fn local_peer_id() -> Option<PeerId> {
    ss::public_key_bytes()?;
    Some(PeerId(*DEVICE_PEER_ID.get_or_init(random_device_id)))
}

/// The per-process device routing id (see [`local_peer_id`]). Opaque 32 bytes.
static DEVICE_PEER_ID: OnceLock<[u8; 32]> = OnceLock::new();

/// A random 32-byte routing id, sourced from schnorrkel's CSPRNG (already a dependency).
/// The bytes are a fresh public key we never sign with — purely an opaque, collision-free
/// mesh address distinct from the shared personhood key.
fn random_device_id() -> [u8; 32] {
    schnorrkel::Keypair::generate().public.to_bytes()
}

/// A FRESH per-call device routing id (new random 32 bytes) — NOT the process-stable
/// [`local_peer_id`]. [`Session::start`] uses this so each watch/publish session dials
/// under a distinct `PeerId`: a re-watch or publisher switch then presents as a NEW peer,
/// which the far side accepts immediately instead of ignoring it as a still-connected
/// duplicate of our just-torn-down session (the far side prunes the stale one on its own
/// ICE timeout). Trust is unaffected — the personhood key ([`identity_public`]) remains the
/// manifest/edge anchor. `PeerId` here is `unstation_core`'s type.
///
/// [`Session::start`]: https://docs.rs/unstation-session
pub fn fresh_device_peer_id() -> PeerId {
    PeerId(random_device_id())
}

/// Tear down the global statement-store client (stops the poll thread).
pub fn shutdown() {
    ss::shutdown();
}

fn err<E: std::fmt::Display>(e: E) -> unstation_core::Error {
    unstation_core::Error::Signaling(e.to_string())
}

/// [`Signaling`] implemented over the People-chain statement store.
///
/// Cheap to clone; the keypair + live connection live in the process-global SDK client
/// initialized by [`init_statement_store`]. The signaling `outbox`/`prio` are shared across
/// clones (Arc) so every send to a peer accumulates into the same bundle.
#[derive(Clone)]
pub struct ChainSignaling {
    stream: StreamId,
    n_shards: u32,
    /// Per-recipient outbound-signal accumulator (see [`ChainSignaling::publish_signal`]):
    /// to-peer → the envelopes we have sent it, resent as one growing bundle so the
    /// statement store's last-write-wins doesn't drop all but the latest signal.
    outbox: Arc<Mutex<HashMap<[u8; 32], Vec<Vec<u8>>>>>,
    /// Monotonic statement priority so each rewritten bundle supersedes the previous.
    prio: Arc<AtomicU32>,
}

impl ChainSignaling {
    pub fn new(stream: StreamId, n_shards: u32) -> Self {
        Self {
            stream,
            n_shards: n_shards.max(1),
            outbox: Arc::new(Mutex::new(HashMap::new())),
            prio: Arc::new(AtomicU32::new(1)),
        }
    }
}

impl Signaling for ChainSignaling {
    fn publish_presence(&self, p: Presence) -> BoxFuture<'static, unstation_core::Result<()>> {
        // Announce into our discovery shard (TECH_SPEC §7.2).
        let topic = discovery_topic(&self.stream, shard_for(&p.peer_id, self.n_shards));
        let data = PresenceRecord::from(&p).encode();
        Box::pin(async move {
            // SDK statement-store calls are blocking (sync WS I/O) — keep them off
            // the async reactor.
            tokio::task::spawn_blocking(move || ss::submit_fixed_topic(topic, &[topic], &data, 0))
                .await
                .map_err(err)?
                .map_err(err)
        })
    }

    fn read_presence(
        &self,
        topic: TopicId,
        max: usize,
    ) -> BoxFuture<'static, unstation_core::Result<Vec<Presence>>> {
        Box::pin(async move {
            let statements =
                tokio::task::spawn_blocking(move || ss::rpc_get_broadcasts(&[topic]))
                    .await
                    .map_err(err)?
                    .map_err(err)?;
            let raw = statements.len();
            let mut out = Vec::new();
            for st in statements.into_iter().take(max) {
                // Drop anything that isn't a well-formed presence record.
                if let Ok(rec) = PresenceRecord::decode(&mut &st.data[..]) {
                    out.push(rec.into());
                }
            }
            log::debug!("[sig] read_presence → {raw} raw statement(s), {} presence", out.len());
            Ok(out)
        })
    }

    fn send_signal(
        &self,
        to: PeerId,
        msg: SignalMsg,
    ) -> BoxFuture<'static, unstation_core::Result<()>> {
        // Targeted SDP/ICE delivery topic, carried via the app's chat codec
        // (`STREAM_MESH` purpose) so it is wire-compatible with the Polkadot app.
        let topic = signaling_topic(&self.stream, &to);
        let data = chat_codec::encode_signal(&msg);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || ss::submit_fixed_topic(topic, &[topic], &data, 0))
                .await
                .map_err(err)?
                .map_err(err)
        })
    }

    fn subscribe_edge(&self, _stream: StreamId) -> Subscription<LiveEdge> {
        // Real live-edge subscription (set_on_statement → bounded channel) lands
        // with the live path in M1.
        Subscription::default()
    }
}

/// On-wire signaling payload: the sender's ephemeral `PeerId` (32 bytes) followed
/// by the chat-codec-encoded [`SignalMsg`].
///
/// A bare `SignalMsg::Offer` carries no sender, but the answerer (publisher) must
/// know *which* peer to route its answer/candidates back to. Rather than mutate
/// the app-compatible `chat_codec` wire layout, unstation↔unstation signaling
/// wraps the message in this envelope on its own (`"sig"`) topic.
fn encode_envelope(from: PeerId, msg: &SignalMsg) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 96);
    out.extend_from_slice(&from.0);
    out.extend_from_slice(&chat_codec::encode_signal(msg));
    out
}

fn decode_envelope(bytes: &[u8]) -> Option<(PeerId, SignalMsg)> {
    if bytes.len() < 32 {
        return None;
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[..32]);
    let msg = chat_codec::decode_signal(&bytes[32..])?;
    Some((PeerId(id), msg))
}

/// Signaling bundle codec. The statement store keeps only the highest-priority statement per
/// (account, channel), and reads match on that channel — so a sender cannot spread an offer,
/// its answer, and its trickled ICE candidates across separate statements (they would evict
/// one another, and per-message channels don't come back on a topic read). Instead a sender
/// resends the FULL set of envelopes it has produced for a peer as one statement each time
/// (see [`ChainSignaling::publish_signal`]); the bundle is a SCALE `Vec<Vec<u8>>` of envelopes.
fn encode_bundle(envelopes: &[Vec<u8>]) -> Vec<u8> {
    envelopes.to_vec().encode()
}

fn decode_bundle(data: &[u8]) -> Vec<Vec<u8>> {
    Vec::<Vec<u8>>::decode(&mut &data[..]).unwrap_or_default()
}

impl ChainSignaling {
    /// Post a signaling message to `to`'s signaling topic, tagged with our own
    /// `PeerId` so the recipient can route its reply back.
    pub async fn publish_signal(
        &self,
        from: PeerId,
        to: PeerId,
        msg: SignalMsg,
    ) -> unstation_core::Result<()> {
        let topic = signaling_topic(&self.stream, &to);
        let envelope = encode_envelope(from, &msg);
        // The statement store keeps only ONE statement per (account, channel=topic) —
        // last-write-wins — so an offer/answer and its trickled ICE candidates would evict
        // one another (only the newest survives). Instead ACCUMULATE every signal we've sent
        // this peer and rewrite the whole set as one statement, with a strictly-increasing
        // priority so the newest (largest) bundle always wins. `read_signals` unpacks the set;
        // the caller dedups by (from, msg), so resending earlier envelopes is harmless.
        let (data, prio) = {
            let mut outbox = self.outbox.lock().unwrap_or_else(|e| e.into_inner());
            let list = outbox.entry(to.0).or_default();
            list.push(envelope);
            (encode_bundle(list), self.prio.fetch_add(1, Ordering::SeqCst))
        };
        tokio::task::spawn_blocking(move || ss::submit_fixed_topic(topic, &[topic], &data, prio))
            .await
            .map_err(err)?
            .map_err(err)
    }

    /// Read and decode all signaling envelopes currently addressed to `me`.
    /// (The caller dedups by `(from, msg)`; statements have a ~30s TTL.)
    pub async fn read_signals(
        &self,
        me: PeerId,
    ) -> unstation_core::Result<Vec<(PeerId, SignalMsg)>> {
        let topic = signaling_topic(&self.stream, &me);
        let statements = tokio::task::spawn_blocking(move || ss::rpc_get_broadcasts(&[topic]))
            .await
            .map_err(err)?
            .map_err(err)?;
        let mut out = Vec::new();
        for st in statements {
            // Each statement is a bundle of envelopes (see `publish_signal`).
            for env in decode_bundle(&st.data) {
                if let Some(pair) = decode_envelope(&env) {
                    out.push(pair);
                }
            }
        }
        Ok(out)
    }

    /// Publish the current live-edge manifest — the `(seq, content-id)` pairs for
    /// the recent window — so viewers learn which segments exist and the hash to
    /// verify each against. (M1: unsigned; M2 signs it + adds the publisher trust
    /// anchor so a Sybil can't forge segment ids.)
    pub async fn publish_edge(
        &self,
        entries: Vec<(Seq, SegmentId)>,
    ) -> unstation_core::Result<()> {
        let topic = edge_topic(&self.stream);
        let raw: Vec<(u64, [u8; 32])> = entries.iter().map(|(s, id)| (*s, id.0)).collect();
        let data = raw.encode();
        tokio::task::spawn_blocking(move || ss::submit_fixed_topic(topic, &[topic], &data, 0))
            .await
            .map_err(err)?
            .map_err(err)
    }

    /// Read the live-edge manifest, merging every published statement on the edge
    /// topic into one `(seq → content-id)` view.
    pub async fn read_edge(&self) -> unstation_core::Result<Vec<(Seq, SegmentId)>> {
        let topic = edge_topic(&self.stream);
        let statements = tokio::task::spawn_blocking(move || ss::rpc_get_broadcasts(&[topic]))
            .await
            .map_err(err)?
            .map_err(err)?;
        let mut merged: std::collections::BTreeMap<Seq, SegmentId> = std::collections::BTreeMap::new();
        for st in statements {
            if let Ok(raw) = <Vec<(u64, [u8; 32])>>::decode(&mut &st.data[..]) {
                for (seq, id) in raw {
                    merged.insert(seq, SegmentId(id));
                }
            }
        }
        Ok(merged.into_iter().collect())
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn envelope_round_trips_sender_and_message() {
        let from = PeerId::from_u64(99);
        let msg = SignalMsg::Answer { offer_id: "abc".into(), sdp: vec![1, 2, 3, 4] };
        let bytes = encode_envelope(from, &msg);
        let (got_from, got_msg) = decode_envelope(&bytes).expect("decodes");
        assert_eq!(got_from, from);
        assert_eq!(got_msg, msg);
    }

    #[test]
    fn decode_rejects_short_input() {
        assert!(decode_envelope(&[0u8; 10]).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use useragent_native::wallet::WalletManager;

    /// Live presence round-trip against Paseo People
    /// (`paseo-people-next-system-rpc`). Ignored by default: needs network and a
    /// provisioned statement-store allowance. Run on a connected machine with:
    ///   cargo test -p unstation-chain -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "live chain: needs network + statement-store allowance"]
    async fn presence_round_trips_on_paseo() {
        let mut wallet = WalletManager::new_ephemeral();
        wallet
            .load_from_mnemonic(
                "bottom drive obey lake curtain smoke basket hold race lonely fit walk",
            )
            .expect("load mnemonic");
        let kp = wallet.statement_store_keypair().expect("statement-store keypair");
        init_statement_store(kp);

        if !wait_ready(Duration::from_secs(15)) {
            eprintln!("[m0] statement store not subscribed (infra unavailable?) — skipping");
            return;
        }

        let stream = StreamId([7u8; 32]);
        let sig = ChainSignaling::new(stream, 1);
        let me = PeerId::from_u64(42);
        let pres = Presence { peer_id: me, publisher: me.0, caps_upload_bps: 5_000_000, ttl_s: 30, manifest_cid: None, relay: false };

        // Best-effort: a fresh ephemeral key has no allowance until provisioned,
        // so a `noAllowance` here is an environment skip, not a code failure.
        if let Err(e) = sig.publish_presence(pres).await {
            eprintln!("[m0] publish_presence skipped (allowance/infra): {e}");
            return;
        }

        let topic = discovery_topic(&stream, shard_for(&me, 1));
        let found = sig.read_presence(topic, 16).await.expect("read_presence");
        assert!(
            found.iter().any(|p| p.peer_id == me),
            "our own presence should round-trip through the statement store",
        );
        shutdown();
    }
}
