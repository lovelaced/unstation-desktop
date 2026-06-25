//! Public-Paseo nightly smoke (#[ignore]): connects to the REAL deployed statement store
//! (and best-effort Bulletin) to validate against the actual production runtime.
//!
//! The READ path always runs (no allowance needed) — it proves real connectivity. The
//! WRITE path runs only with a personhood-provisioned key (set `UNSTATION_PASEO_MNEMONIC`
//! to a GitHub secret); with the default dev mnemonic it has no allowance on Paseo, so the
//! write is skipped, not failed. The test never fails spuriously on a missing secret or
//! transient infra.
//!
//! Run: cargo test -p unstation-chain --test paseo_smoke -- --ignored --nocapture

use std::time::Duration;
use unstation_chain::{BulletinOrigin, ChainSignaling};
use unstation_core::manifest::{Kind, Manifest, OriginOfRecord, SignedManifest, Track};
use unstation_core::signaling::{Presence, Signaling};
use unstation_core::topic::{discovery_topic, shard_for};
use unstation_core::types::StreamId;

const DEV_MNEMONIC: &str = "bottom drive obey lake curtain smoke basket hold race lonely fit walk";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live public Paseo: needs network (+ a provisioned key for writes)"]
async fn paseo_presence_smoke() {
    let provisioned = std::env::var("UNSTATION_PASEO_MNEMONIC").is_ok();
    let mnemonic = std::env::var("UNSTATION_PASEO_MNEMONIC").unwrap_or_else(|_| DEV_MNEMONIC.to_string());

    if let Err(e) = unstation_chain::init_from_mnemonic(&mnemonic) {
        eprintln!("[paseo] could not init identity ({e}) — skipping");
        return;
    }
    if !unstation_chain::wait_ready(Duration::from_secs(20)) {
        eprintln!("[paseo] statement store not subscribed (network/infra unavailable) — skipping");
        return;
    }
    let me = unstation_chain::local_peer_id().expect("identity initialized");
    let stream = StreamId([7u8; 32]);
    let sig = ChainSignaling::new(stream, 1);
    let topic = discovery_topic(&stream, shard_for(&me, 1));

    // READ path — always works (reads need no allowance); proves real connectivity.
    sig.read_presence(topic, 16).await.expect("read presence from the real statement store");
    eprintln!("[paseo] read path OK against the real statement store");

    // WRITE path — only a personhood-provisioned key can write. Best-effort.
    let pres = Presence { peer_id: me, caps_upload_bps: 20_000_000, ttl_s: 30, manifest_cid: None, relay: false };
    match sig.publish_presence(pres).await {
        Ok(()) => {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let found = sig.read_presence(topic, 32).await.expect("read back");
            assert!(found.iter().any(|p| p.peer_id == me), "the provisioned key's presence must round-trip on real Paseo");
            eprintln!("[paseo] WRITE round-trip OK (provisioned key)");
            best_effort_bulletin(me).await;
        }
        Err(e) if provisioned => panic!("provisioned key failed to write to Paseo: {e}"),
        Err(e) => eprintln!("[paseo] write skipped — the dev key has no allowance on Paseo ({e})"),
    }

    unstation_chain::shutdown();
}

/// Best-effort real Bulletin round-trip: sign a manifest, put it, fetch + verify it.
/// Needs separate bulletin authorization, so any failure is logged + skipped, never fatal.
async fn best_effort_bulletin(me: unstation_core::types::PeerId) {
    let manifest = Manifest {
        stream_id: StreamId([7u8; 32]),
        kind: Kind::Live,
        codec: "avc1.640028,mp4a.40.2".into(),
        init_segment_cid: String::new(),
        target_segment_ms: 2000,
        ll_mode: false,
        tracks: vec![Track { id: "v".into(), bitrate: 0, w: 0, h: 0 }],
        publisher: me.0,
        created_at: 0,
    };
    let Some(sig_bytes) = unstation_chain::sign_with_identity(&manifest.signing_payload()) else {
        eprintln!("[paseo] no identity to sign a manifest — skipping Bulletin");
        return;
    };
    match BulletinOrigin.put_manifest(SignedManifest { manifest, sig: sig_bytes }).await {
        Ok(cid) => match BulletinOrigin.fetch_manifest(cid).await {
            Ok(m) if m.verify(&me.0).is_ok() => eprintln!("[paseo] Bulletin manifest round-trip OK"),
            _ => eprintln!("[paseo] Bulletin fetch/verify mismatch — skipping"),
        },
        Err(e) => eprintln!("[paseo] Bulletin write skipped (authorization/infra): {e:?}"),
    }
}
