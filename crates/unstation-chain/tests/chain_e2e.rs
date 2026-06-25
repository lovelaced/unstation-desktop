//! Local real-chain e2e (#[ignore]): round-trips presence, signaling, and the live-edge
//! manifest through a REAL `pallet-statement` store on a local `--dev` node.
//!
//! Production "Paseo People Next" can't be reproduced locally (its personhood pallets are
//! non-public), but our code only depends on the `:statement_allowance:` key + the
//! `statement_submit`/`statement_subscribeStatement` RPC — which a local kitchensink
//! `substrate-node` reproduces faithfully. With the `testnet-provisioning` feature,
//! `init_statement_store` auto-grants this key an allowance via Alice's sudo (Alice IS
//! sudo on a --dev node; she is NOT on public Paseo — that's the production blocker).
//!
//! Boot a node first (see `scripts/dev-chain.sh`), then:
//!   NODE_WS=ws://127.0.0.1:9944 cargo test -p unstation-chain \
//!     --features testnet-provisioning --test chain_e2e -- --ignored --nocapture
//!
//! Single test on purpose: the statement-store client is process-global (one init).

use std::time::Duration;
use unstation_core::crypto;
use unstation_core::signaling::{Presence, Signaling, SignalMsg};
use unstation_core::topic::{discovery_topic, shard_for};
use unstation_core::types::{SegmentId, StreamId};

fn node_ws() -> String {
    std::env::var("NODE_WS").unwrap_or_else(|_| "ws://127.0.0.1:9944".to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local chain: needs a --dev node with pallet-statement + --features testnet-provisioning"]
async fn local_chain_round_trips_presence_signaling_and_edge() {
    use unstation_chain::ChainSignaling;

    // Point the SDK at the local node, then init a fresh identity. With
    // `testnet-provisioning`, init kicks off Alice's sudo grant of our allowance.
    unstation_chain::set_statement_store_endpoint(vec![node_ws()]);
    let kp = crypto::keypair_from_seed(&[11u8; 32]);
    unstation_chain::init_statement_store(kp);
    assert!(
        unstation_chain::wait_ready(Duration::from_secs(20)),
        "statement store should connect + subscribe to the local node at {}",
        node_ws(),
    );
    let me = unstation_chain::local_peer_id().expect("identity initialized");

    let stream = StreamId([7u8; 32]);
    let sig = ChainSignaling::new(stream, 1);

    // ---- presence (retry to absorb async allowance provisioning) ----
    let pres = Presence { peer_id: me, caps_upload_bps: 20_000_000, ttl_s: 30, manifest_cid: None, relay: true };
    let mut waited = 0u64;
    loop {
        match sig.publish_presence(pres.clone()).await {
            Ok(()) => break,
            Err(e) if waited < 90 => {
                eprintln!("[e2e] publish_presence not yet allowed ({e}); allowance still provisioning…");
                tokio::time::sleep(Duration::from_secs(3)).await;
                waited += 3;
            }
            Err(e) => panic!("publish_presence never succeeded (allowance not provisioned?): {e}"),
        }
    }
    tokio::time::sleep(Duration::from_secs(3)).await; // let the store settle/gossip
    let topic = discovery_topic(&stream, shard_for(&me, 1));
    let found = sig.read_presence(topic, 32).await.expect("read_presence");
    assert!(found.iter().any(|p| p.peer_id == me), "our presence must round-trip through the real store");

    // ---- signaling (send an offer to ourselves, read it back) ----
    sig.publish_signal(me, me, SignalMsg::Offer { sdp: b"v=0 local-e2e".to_vec() })
        .await
        .expect("publish_signal");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let sigs = sig.read_signals(me).await.expect("read_signals");
    assert!(
        sigs.iter().any(|(from, m)| *from == me && matches!(m, SignalMsg::Offer { .. })),
        "the SDP offer must round-trip on the signaling topic",
    );

    // ---- live-edge manifest ----
    let entries = vec![(0u64, SegmentId([1u8; 32])), (1u64, SegmentId([2u8; 32]))];
    sig.publish_edge(entries.clone()).await.expect("publish_edge");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let edge = sig.read_edge().await.expect("read_edge");
    for (seq, id) in entries {
        assert!(edge.iter().any(|(s, i)| *s == seq && *i == id), "edge entry seq {seq} must round-trip");
    }

    unstation_chain::shutdown();
}
