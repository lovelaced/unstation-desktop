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

use std::sync::OnceLock;
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

/// The host's public key (== its `PeerId`) — the manifest's `publisher` field and the
/// trust anchor a viewer checks against. `None` until the identity is initialized.
pub fn identity_public() -> Option<[u8; 32]> {
    local_peer_id().map(|p| p.0)
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

/// Our ephemeral `PeerId` = the statement-store account public key. Ties the
/// mesh identity (presence, signaling envelopes, discovery) to the on-chain
/// signer, so the answerer can route replies and (in M2) verify the sender.
/// `None` until [`init_statement_store`] has run.
pub fn local_peer_id() -> Option<PeerId> {
    ss::public_key_bytes().map(PeerId)
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
/// Cheap to clone; all state (the signing keypair, the live connection) lives in
/// the process-global SDK client initialized by [`init_statement_store`].
#[derive(Clone)]
pub struct ChainSignaling {
    stream: StreamId,
    n_shards: u32,
}

impl ChainSignaling {
    pub fn new(stream: StreamId, n_shards: u32) -> Self {
        Self { stream, n_shards: n_shards.max(1) }
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
            let mut out = Vec::new();
            for st in statements.into_iter().take(max) {
                // Drop anything that isn't a well-formed presence record.
                if let Ok(rec) = PresenceRecord::decode(&mut &st.data[..]) {
                    out.push(rec.into());
                }
            }
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
        let data = encode_envelope(from, &msg);
        tokio::task::spawn_blocking(move || ss::submit_fixed_topic(topic, &[topic], &data, 0))
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
            if let Some(pair) = decode_envelope(&st.data) {
                out.push(pair);
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
        let pres = Presence { peer_id: me, caps_upload_bps: 5_000_000, ttl_s: 30 };

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
