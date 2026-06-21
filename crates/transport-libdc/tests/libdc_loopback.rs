//! Real libdatachannel loopback (D2 probe): two in-process `RtcPeerConnection`s
//! complete an offer/answer + ICE handshake over the loopback interface, open a
//! `DataChannel`, and carry a real `MeshMsg` from one peer to the other.
//!
//! This proves the native transport works and carries our wire protocol. It needs
//! to bind loopback UDP sockets; if a sandbox forbids that the handshake can't
//! complete, so the test fails closed with a clear timeout message rather than
//! hanging. The deterministic mesh logic is covered separately by the in-memory
//! transport (`unstation-core/tests/two_peer.rs`).

use datachannel::{RtcConfig, RtcPeerConnection};
use parity_scale_codec::{Decode, Encode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport_libdc::{Conn, DcSink};
use unstation_core::protocol::MeshMsg;

// Requires binding loopback UDP for ICE, which the default test sandbox blocks.
// Verified passing (~1s) on a networked host: `cargo test -p transport-libdc -- --ignored`.
#[test]
#[ignore = "requires loopback UDP (ICE); run with --ignored on a networked host"]
fn libdc_loopback_roundtrips_protocol_bytes() {
    // No ICE servers — host candidates over loopback suffice.
    let conf = RtcConfig::new::<&str>(&[]);

    let (a_desc_tx, a_desc_rx) = channel();
    let (a_cand_tx, a_cand_rx) = channel();
    let (b_desc_tx, b_desc_rx) = channel();
    let (b_cand_tx, b_cand_rx) = channel();
    let (a_in_tx, _a_in_rx) = channel::<Vec<u8>>();
    let (b_in_tx, b_in_rx) = channel::<Vec<u8>>();

    let a_opened = Arc::new(AtomicBool::new(false));
    let b_opened = Arc::new(AtomicBool::new(false));

    let mut pc_b = RtcPeerConnection::new(
        &conf,
        Conn {
            local_desc: b_desc_tx,
            local_cand: b_cand_tx,
            incoming: b_in_tx,
            opened: b_opened,
            recv_dc: Arc::new(Mutex::new(None)),
        },
    )
    .expect("create pc_b");

    let mut pc_a = RtcPeerConnection::new(
        &conf,
        Conn {
            local_desc: a_desc_tx,
            local_cand: a_cand_tx,
            incoming: a_in_tx.clone(),
            opened: a_opened.clone(),
            recv_dc: Arc::new(Mutex::new(None)),
        },
    )
    .expect("create pc_a");

    // A initiates: creating the channel triggers offer/answer auto-negotiation.
    let mut dc_a = pc_a
        .create_data_channel(
            "mesh",
            DcSink { incoming: a_in_tx, opened: a_opened.clone() },
        )
        .expect("create data channel");

    let payload = MeshMsg::Want { segment_seqs: vec![1, 2, 3], deadline_hint_ms: 0 }.encode();

    let start = Instant::now();
    let mut sent = false;
    let mut received: Option<Vec<u8>> = None;
    loop {
        // Ferry signaling A -> B.
        while let Ok(d) = a_desc_rx.try_recv() {
            let _ = pc_b.set_remote_description(&d);
        }
        while let Ok(c) = a_cand_rx.try_recv() {
            let _ = pc_b.add_remote_candidate(&c);
        }
        // Ferry signaling B -> A.
        while let Ok(d) = b_desc_rx.try_recv() {
            let _ = pc_a.set_remote_description(&d);
        }
        while let Ok(c) = b_cand_rx.try_recv() {
            let _ = pc_a.add_remote_candidate(&c);
        }
        // Once A's channel is open, push the payload.
        if !sent && a_opened.load(Ordering::SeqCst) && dc_a.send(&payload).is_ok() {
            sent = true;
        }
        if let Ok(msg) = b_in_rx.try_recv() {
            received = Some(msg);
            break;
        }
        if start.elapsed() > Duration::from_secs(20) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let received = received.expect("B should receive A's message over a real DataChannel");
    assert_eq!(received, payload, "bytes must arrive intact");
    let decoded = MeshMsg::decode(&mut &received[..]).expect("decodes as MeshMsg");
    assert!(matches!(decoded, MeshMsg::Want { .. }), "got {decoded:?}");
}
