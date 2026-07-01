//! Real two-channel libdatachannel loopback (D2 probe): two `LibDcTransport`
//! reactors complete an offer/answer + trickle-ICE handshake over the loopback
//! interface, open the `ctrl`+`bulk` data channels, and carry a real `MeshMsg`
//! across the `bulk` channel — exercising the production transport path end to end.
//!
//! Needs to bind loopback UDP sockets for ICE; if a sandbox forbids that the
//! handshake can't complete, so the test fails closed with a clear timeout rather
//! than hanging. The deterministic mesh logic is covered separately by the
//! in-memory transport (`unstation-core/tests/two_peer.rs`).

use parity_scale_codec::{Decode, Encode};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use transport_libdc::{LibDcTransport, SignalOut};
use unstation_core::protocol::MeshMsg;
use unstation_core::transport::{Channel, EngineEvent};
use unstation_core::types::PeerId;

// Requires binding loopback UDP for ICE, which the default test sandbox blocks.
// Run on a networked host: `cargo test -p transport-libdc -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
async fn two_channel_link_roundtrips_protocol_bytes() {
    let a_id = PeerId::from_u64(1);
    let b_id = PeerId::from_u64(2);

    let (a_inbox_tx, mut a_inbox_rx) = unbounded_channel::<EngineEvent>();
    let (b_inbox_tx, mut b_inbox_rx) = unbounded_channel::<EngineEvent>();
    let (a_sig_tx, mut a_sig_rx) = unbounded_channel::<SignalOut>();
    let (b_sig_tx, mut b_sig_rx) = unbounded_channel::<SignalOut>();

    // No STUN — host candidates over loopback suffice.
    let a = LibDcTransport::new(vec![], a_inbox_tx, a_sig_tx).expect("spawn reactor");
    let b = LibDcTransport::new(vec![], b_inbox_tx, b_sig_tx).expect("spawn reactor");

    // Relay A's local signaling to B (the first description is A's offer → accept).
    {
        let b = b.clone();
        tokio::spawn(async move {
            let mut accepted = false;
            while let Some(sig) = a_sig_rx.recv().await {
                match sig {
                    SignalOut::LocalDescription { sdp, .. } => {
                        if !accepted {
                            b.accept(a_id, sdp);
                            accepted = true;
                        } else {
                            b.remote_description(a_id, sdp);
                        }
                    }
                    SignalOut::LocalCandidate { cand, .. } => b.remote_candidate(a_id, cand),
                }
            }
        });
    }
    // Relay B's local signaling to A (B's answer + candidates).
    {
        let a = a.clone();
        tokio::spawn(async move {
            while let Some(sig) = b_sig_rx.recv().await {
                match sig {
                    SignalOut::LocalDescription { sdp, .. } => a.remote_description(b_id, sdp),
                    SignalOut::LocalCandidate { cand, .. } => a.remote_candidate(b_id, cand),
                }
            }
        });
    }

    a.dial(b_id);

    // A should see the link come up within 20s.
    let link = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match a_inbox_rx.recv().await {
                Some(EngineEvent::PeerConnected { link, .. }) => return link,
                Some(_) => continue,
                None => panic!("A inbox closed before connect"),
            }
        }
    })
    .await
    .expect("A should connect to B within 20s");

    let payload = MeshMsg::Want { segment_seqs: vec![1, 2, 3], deadline_hint_ms: 0 }.encode();
    link.send(Channel::Bulk, payload.clone());

    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match b_inbox_rx.recv().await {
                Some(EngineEvent::Inbound { channel: Channel::Bulk, bytes, .. }) => return bytes,
                Some(_) => continue,
                None => panic!("B inbox closed before message"),
            }
        }
    })
    .await
    .expect("B should receive the bulk message within 10s");

    assert_eq!(got, payload, "bytes must arrive intact");
    let decoded = MeshMsg::decode(&mut &got[..]).expect("decodes as MeshMsg");
    assert!(matches!(decoded, MeshMsg::Want { .. }), "got {decoded:?}");
}
