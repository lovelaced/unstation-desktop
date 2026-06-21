//! D3: discovery + SDP-over-statement-store exchange, with write accounting.
//!
//! Peers announce presence on sharded discovery topics; a joiner reads its shards,
//! finds candidates, and exchanges a STREAM_MESH offer/answer over the statement
//! store. Asserts the §7.3 budget shape: writes are O(joins) — one presence per
//! peer plus the bootstrap offer/answer — not O(connections). The actual
//! mesh-gossip handoff (zero further store writes) is shown in `gossip.rs`.

use parity_scale_codec::Decode;
use std::sync::Arc;
use unstation_core::chat_codec::{ChatMessageContent, DataChannelPurpose};
use unstation_core::clock::VirtualClock;
use unstation_core::signaling::SignalMsg;
use unstation_core::statement_store_mem::{MemStatementStore, StatementSignaling};
use unstation_core::topic::signaling_topic;
use unstation_core::types::{PeerId, StreamId};

fn sig(store: &MemStatementStore, stream: StreamId, me: PeerId, clock: Arc<VirtualClock>) -> StatementSignaling {
    StatementSignaling::new(store.clone(), stream, me, 2, 30, clock)
}

#[test]
fn discovery_and_sdp_exchange_over_statement_store() {
    let store = MemStatementStore::new();
    let clock = Arc::new(VirtualClock::new()); // now = 0; ttl 30s ⇒ nothing expired
    let stream = StreamId([7u8; 32]);

    let (p, a, b, c) = (
        PeerId::from_u64(1),
        PeerId::from_u64(2),
        PeerId::from_u64(3),
        PeerId::from_u64(4),
    );
    let sp = sig(&store, stream, p, clock.clone());
    let sa = sig(&store, stream, a, clock.clone());
    let sb = sig(&store, stream, b, clock.clone());
    let sc = sig(&store, stream, c, clock.clone());

    // Each peer announces presence — one write each.
    for s in [&sp, &sa, &sb, &sc] {
        s.publish_presence_now(5_000_000);
    }
    assert_eq!(store.writes(), 4);

    // A reads its shards and finds everyone but itself.
    let cands: Vec<PeerId> = sa.read_candidates(10).into_iter().map(|x| x.peer_id).collect();
    assert!(cands.contains(&p) && cands.contains(&b) && cands.contains(&c));
    assert!(!cands.contains(&a));

    // A → P: a STREAM_MESH offer over the store.
    sa.send_signal_now(p, &SignalMsg::Offer { sdp: b"v=0 offer-from-A".to_vec() });

    // The on-wire content really is a DataChannelOffer with the StreamMesh purpose.
    let raw = store.read(signaling_topic(&stream, &p), 0);
    assert_eq!(raw.len(), 1);
    match ChatMessageContent::decode(&mut &raw[0].1[..]).unwrap() {
        ChatMessageContent::DataChannelOffer(o) => {
            assert_eq!(o.purpose, DataChannelPurpose::StreamMesh)
        }
        other => panic!("expected offer, got {other:?}"),
    }

    // P decodes the offer and answers.
    let pin = sp.read_signals();
    assert_eq!(pin.len(), 1);
    assert_eq!(pin[0].0, a);
    assert!(matches!(pin[0].1, SignalMsg::Offer { .. }));
    sp.send_signal_now(
        a,
        &SignalMsg::Answer { offer_id: "off-A-P".into(), sdp: b"v=0 answer".to_vec() },
    );

    // A receives the answer ⇒ the first link is brokered.
    let ain = sa.read_signals();
    assert_eq!(ain.len(), 1);
    assert!(matches!(ain[0].1, SignalMsg::Answer { .. }));

    // 4 presence + 1 offer + 1 answer = 6 writes for 4 joins + 1 link setup.
    assert_eq!(store.writes(), 6, "writes are O(joins), not O(connections)");
}
