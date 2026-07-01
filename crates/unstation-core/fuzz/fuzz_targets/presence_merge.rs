//! Fuzz the presence book against hostile gossip batches.
//!
//! `PresenceGossip` records are attacker-controlled. Invariants: never panics,
//! the book never exceeds its cap, own entry never merged, oversized manifest
//! CIDs never admitted. Run: `cargo +nightly fuzz run presence_merge`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use unstation_core::signaling::{PresenceBook, PresenceRecord};
use unstation_core::types::PeerId;

fuzz_target!(|batches: Vec<Vec<([u8; 32], u32, u64, u16, bool)>>| {
    let book = PresenceBook::new();
    let me = PeerId([0xEE; 32]);
    for batch in batches {
        let recs: Vec<PresenceRecord> = batch
            .into_iter()
            .map(|(id, ttl, caps, cid_len, relay)| PresenceRecord {
                peer_id: id,
                publisher: id,
                caps_upload_bps: caps,
                ttl_s: ttl,
                manifest_cid: if cid_len == 0 {
                    None
                } else {
                    Some("c".repeat((cid_len as usize).min(4096)))
                },
                relay,
            })
            .collect();
        book.merge(recs, &me);
    }
    assert!(book.len() <= 1024, "presence book grew past its cap");
    for rec in book.snapshot() {
        assert_ne!(PeerId(rec.peer_id), me, "own entry merged");
        assert!(rec.manifest_cid.map_or(0, |c| c.len()) <= 128, "oversized CID admitted");
    }
});
