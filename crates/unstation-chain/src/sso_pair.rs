//! Headless pairing with the Polkadot phone app — the seed node's sign-in.
//!
//! Drives the SDK's Mobile SSO V2 flow over the statement store: show a QR
//! (`polkadotapp://pair?handshake=…`), the phone scans + approves, the phone
//! funds the device statement account with an on-chain allowance during the
//! handshake, then we request the per-product statement-store allowance slot
//! key (`ResourceAllocationRequest`) and hand its raw secret to the caller to
//! persist. All SDK API contact is confined to this module on purpose: if the
//! SDK surface shifts, this is the only file in the app repo that moves.
//!
//! Process model: `pair` runs to completion and exits; the run path is a fresh
//! process that reads the persisted slot secret. Pairing itself performs no
//! signed writes with any process-global key — V2 session statements are
//! signed inside the SDK with the device statement key — so this module never
//! initializes the crate's global statement-store client, only the raw RPC
//! transport (which respects `HOST_STATEMENT_STORE_WS_ENDPOINTS`).

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use useragent_native::chain::statement_store as ss;
use useragent_sso::traits::{SsoSecretStore, SsoStatementTransport, SubmitError};
use useragent_sso::{
    HandshakeMetadata, NoopProductKeyStore, PersistedSessionMeta, SsoEventSink, SsoManager,
    SsoSessionStore, SsoState, SsoTransport, SsoV2Secrets,
};
use zeroize::Zeroizing;

/// Progress events surfaced to the CLI while pairing.
pub enum PairEvent {
    /// Render this URI as a QR code (and print it raw as a fallback).
    QrReady { uri: String },
    /// The phone approved the pairing; the allowance request is next.
    Paired { address: String, display_name: String },
    /// Free-form progress line.
    Info { msg: String },
}

/// The result of a successful pairing: the allowance-backed slot secret and
/// what it derives to.
pub struct PairOutcome {
    /// Raw sr25519 slot secret (64 bytes; 32 accepted) whose account the phone
    /// granted a statement-store allowance. The caller persists this.
    pub slot_secret: Zeroizing<Vec<u8>>,
    /// The slot account's public key — the seed's on-chain identity.
    pub identity_public: [u8; 32],
    /// SS58 address of the paired phone account (for operator feedback only).
    pub phone_address: String,
    /// Device name the phone reported (for operator feedback only).
    pub display_name: String,
}

/// Full pairing flow: connect to the statement store, run the V2 QR handshake,
/// request the `product_id` statement-store allowance slot key, and return its
/// secret. Blocking; takes minutes (the operator has to pick up their phone) —
/// `timeout` bounds the QR-scan phase; the allowance round trip has the SDK's
/// own 240s budget on top.
///
/// Starts from a clean slate every time: a fresh device identity gives a fresh
/// pairing topic, so a stale `Failed` statement from an earlier attempt can
/// never poison this one. (The slot key is deterministic per phone+product, so
/// re-pairing always converges on the same seed identity anyway.) Writes
/// `sso_secrets.json` + `sso_session.json` into `key_dir` (0600). Does NOT
/// write the slot secret — that's the caller's job (`unstation-node` owns the
/// key-dir layout).
pub fn pair_with_phone(
    key_dir: &Path,
    product_id: &str,
    timeout: Duration,
    on_event: &mut dyn FnMut(PairEvent),
) -> Result<PairOutcome, String> {
    // Fail fast if the chain is unreachable — better than a silent 5-minute hang.
    on_event(PairEvent::Info { msg: "connecting to the statement-store chain…".into() });
    ss::rpc_get_broadcasts_raw(&[[0u8; 32]])
        .map_err(|e| format!("cannot reach the statement-store chain: {e}"))?;

    std::fs::create_dir_all(key_dir).map_err(|e| format!("create {}: {e}", key_dir.display()))?;
    let session_store = FileSessionStore { path: key_dir.join("sso_session.json") };
    let secret_store = Arc::new(FileSecretStore { path: key_dir.join("sso_secrets.json") });
    // Clean slate (see doc comment).
    session_store.clear().ok();
    secret_store.clear().ok();

    let (state_tx, state_rx) = mpsc::channel::<SsoState>();

    let manager = SsoManager::with_statement_transport(
        NoopV1Transport,
        StubV1Signer,
        session_store,
        ChannelSink { tx: Mutex::new(state_tx) },
        NoopProductKeyStore,
        String::new(),
        useragent_sso::manager::SsoV2Config {
            statement_transport: Arc::new(PollingStatementTransport::default()),
            secret_store: secret_store as Arc<dyn SsoSecretStore>,
            slot_key_store: None,
            handshake_metadata: HandshakeMetadata {
                host_name: Some("Unstation seed".into()),
                host_version: Some(env!("CARGO_PKG_VERSION").into()),
                host_icon: None,
                platform_type: Some("desktop".into()),
                platform_version: None,
                custom: Vec::new(),
            },
        },
    );

    manager.pair_v2().map_err(|e| format!("start pairing: {e}"))?;

    // Drive the QR phase off the state sink until the session is established.
    let deadline = Instant::now() + timeout;
    let (address, display_name) = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("pairing timed out — no approval from the phone")?;
        match state_rx.recv_timeout(remaining) {
            Ok(SsoState::AwaitingScan { qr_uri }) => on_event(PairEvent::QrReady { uri: qr_uri }),
            Ok(SsoState::PairingPending { stage }) => on_event(PairEvent::Info {
                msg: match stage.as_str() {
                    "AllowanceAllocation" => {
                        "phone acknowledged — allocating the on-chain allowance…".into()
                    }
                    other => format!("pairing: {other}"),
                },
            }),
            Ok(SsoState::Paired { address, display_name, .. }) => break (address, display_name),
            Ok(SsoState::Failed { reason }) => return Err(format!("pairing failed: {reason}")),
            Ok(SsoState::Idle) => {}
            Ok(_) => {} // SsoState is #[non_exhaustive]; ignore states we don't drive UI from
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err("pairing timed out — no approval from the phone".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("pairing aborted internally (state channel closed)".into())
            }
        }
    };
    on_event(PairEvent::Paired { address: address.clone(), display_name: display_name.clone() });

    // The allowance slot key round trip (SDK budget: 240s — the phone shows an
    // approval prompt and then writes the grant on-chain).
    let slot_secret = manager
        .request_statement_store_slot_key(product_id)
        .map_err(|e| format!("allowance request failed: {e}"))?;

    let identity_public = crate::keypair_from_secret(&slot_secret)
        .map_err(|e| format!("phone returned an unusable slot key: {e}"))?
        .public
        .to_bytes();

    Ok(PairOutcome { slot_secret, identity_public, phone_address: address, display_name })
}

// ---------------------------------------------------------------------------
// Statement transport: raw pre-encoded statements over the SDK's RPC layer
// ---------------------------------------------------------------------------

/// [`SsoStatementTransport`] over the SDK's raw statement RPC (pooled-socket
/// snapshots + `statement_submit`). Subscriptions are 2s polls — the same
/// cadence the SDK's own session reconciliation uses — deduped by statement id;
/// fine for a short-lived pairing process.
#[derive(Default)]
struct PollingStatementTransport {
    subs: Mutex<Vec<(u64, Arc<AtomicBool>)>>,
    next_id: AtomicU64,
}

const SUBSCRIBE_POLL: Duration = Duration::from_secs(2);

impl SsoStatementTransport for PollingStatementTransport {
    fn submit_statement(&self, encoded: &[u8]) -> Result<(), SubmitError> {
        ss::rpc_submit_structured(encoded).map_err(|e| match e {
            ss::SubmitFailure::Rejected(r) => match r.reason.as_str() {
                "channelPriorityTooLow" | "accountFull" => {
                    SubmitError::PriorityTooLow { min: r.min_expiry.unwrap_or(0) }
                }
                "noAllowance" => SubmitError::NoAllowance,
                other => SubmitError::Other(format!("statement rejected: {other}")),
            },
            ss::SubmitFailure::Transport(msg) => SubmitError::Other(msg),
        })
    }

    fn query_statements(&self, topic: &[u8; 32]) -> Result<Vec<Vec<u8>>, String> {
        Ok(ss::rpc_get_broadcasts_raw(&[*topic])?.into_iter().map(|raw| raw.encoded).collect())
    }

    fn subscribe_statements(
        &self,
        topic: &[u8; 32],
    ) -> Result<(u64, mpsc::Receiver<Vec<u8>>), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stop = Arc::new(AtomicBool::new(false));
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).push((id, Arc::clone(&stop)));

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let topic = *topic;
        std::thread::spawn(move || {
            let mut seen: HashSet<[u8; 32]> = HashSet::new();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(broadcasts) = ss::rpc_get_broadcasts_raw(&[topic]) {
                    for raw in broadcasts {
                        if seen.insert(raw.statement_id) && tx.send(raw.encoded).is_err() {
                            return; // receiver dropped — subscription is dead
                        }
                    }
                }
                std::thread::sleep(SUBSCRIBE_POLL);
            }
        });
        Ok((id, rx))
    }

    fn unsubscribe(&self, id: u64) {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|(sub_id, stop)| {
            if *sub_id == id {
                stop.store(true, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
    }
}

// ---------------------------------------------------------------------------
// File-backed stores (0600, in the seed's key dir)
// ---------------------------------------------------------------------------

fn write_0600(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    file.sync_all().map_err(|e| format!("sync {}: {e}", path.display()))
}

struct FileSessionStore {
    path: PathBuf,
}

impl SsoSessionStore for FileSessionStore {
    fn save(&self, session: &PersistedSessionMeta) -> Result<(), String> {
        let json = serde_json::to_vec(session).map_err(|e| e.to_string())?;
        write_0600(&self.path, &json)
    }

    fn load(&self) -> Result<Option<PersistedSessionMeta>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// `SsoV2Secrets` has no serde derives (deliberately — it's key material), so
/// persist a minimal hex JSON by hand.
struct FileSecretStore {
    path: PathBuf,
}

impl SsoSecretStore for FileSecretStore {
    fn save(&self, secrets: &SsoV2Secrets) -> Result<(), String> {
        let json = serde_json::json!({
            "statement_account_secret": hex::encode(&*secrets.statement_account_secret),
            "encryption_private_key": hex::encode(&*secrets.encryption_private_key),
            "identity_chat_private_key":
                secrets.identity_chat_private_key.as_ref().map(|k| hex::encode(&**k)),
        });
        write_0600(&self.path, json.to_string().as_bytes())
    }

    fn load(&self) -> Result<Option<SsoV2Secrets>, String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let field = |name: &str| -> Result<Vec<u8>, String> {
            hex::decode(v.get(name).and_then(|x| x.as_str()).unwrap_or_default())
                .map_err(|e| format!("{name}: {e}"))
        };
        let statement: [u8; 64] = field("statement_account_secret")?
            .try_into()
            .map_err(|_| "statement_account_secret: wrong length".to_string())?;
        let encryption: [u8; 32] = field("encryption_private_key")?
            .try_into()
            .map_err(|_| "encryption_private_key: wrong length".to_string())?;
        let chat = match v.get("identity_chat_private_key").and_then(|x| x.as_str()) {
            Some(hex_str) => Some(Zeroizing::new(
                hex::decode(hex_str)
                    .map_err(|e| format!("identity_chat_private_key: {e}"))?
                    .try_into()
                    .map_err(|_| "identity_chat_private_key: wrong length".to_string())?,
            )),
            None => None,
        };
        Ok(Some(SsoV2Secrets {
            statement_account_secret: Zeroizing::new(statement),
            encryption_private_key: Zeroizing::new(encryption),
            identity_chat_private_key: chat,
        }))
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// v1 stubs + event sink
// ---------------------------------------------------------------------------

/// The manager's legacy v1 channel is unused by the V2 flow; satisfy the
/// constructor with inert stubs (mirrors the SDK's own V2 test harness).
struct NoopV1Transport;

impl SsoTransport for NoopV1Transport {
    fn subscribe(&self, _topic_hex: &str) -> Result<(u64, mpsc::Receiver<(String, String)>), String> {
        let (_tx, rx) = mpsc::channel();
        Ok((0, rx))
    }
    fn unsubscribe(&self, _id: u64) {}
    fn write(&self, _topic_hex: &str, _value: &str) -> Result<(), String> {
        Err("v1 SSO channel is not wired in the seed".into())
    }
}

struct StubV1Signer;

impl useragent_native::wallet::HostSigner for StubV1Signer {
    fn public_key(&self) -> Result<[u8; 32], useragent_native::wallet::SignerError> {
        Ok([0u8; 32])
    }
    fn sign(&self, _payload: &[u8]) -> Result<[u8; 64], useragent_native::wallet::SignerError> {
        Err(useragent_native::wallet::SignerError::Locked)
    }
}

/// Forwards manager state transitions into the pairing driver's channel.
struct ChannelSink {
    tx: Mutex<mpsc::Sender<SsoState>>,
}

impl SsoEventSink for ChannelSink {
    fn on_state_changed(&self, state: &SsoState) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(state.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("unstation-sso-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileSecretStore { path: dir.join("sso_secrets.json") };
        assert!(store.load().unwrap().is_none());
        let secrets = SsoV2Secrets {
            statement_account_secret: Zeroizing::new([7u8; 64]),
            encryption_private_key: Zeroizing::new([9u8; 32]),
            identity_chat_private_key: Some(Zeroizing::new([3u8; 32])),
        };
        store.save(&secrets).unwrap();
        let loaded = store.load().unwrap().expect("present");
        assert_eq!(*loaded.statement_account_secret, [7u8; 64]);
        assert_eq!(*loaded.encryption_private_key, [9u8; 32]);
        assert_eq!(loaded.identity_chat_private_key.as_deref(), Some(&[3u8; 32]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                std::fs::metadata(dir.join("sso_secrets.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
        store.clear().unwrap(); // idempotent
    }

    #[test]
    fn session_store_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("unstation-sso-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileSessionStore { path: dir.join("sso_session.json") };
        assert!(store.load().unwrap().is_none());
        let meta = PersistedSessionMeta {
            session_id: "sess".into(),
            address: "15oF4".into(),
            display_name: "phone".into(),
            ..Default::default()
        };
        store.save(&meta).unwrap();
        let loaded = store.load().unwrap().expect("present");
        assert_eq!(loaded.session_id, "sess");
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
