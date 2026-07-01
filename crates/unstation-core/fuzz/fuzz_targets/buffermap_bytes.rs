//! Fuzz the buffer-map wire decoding + operations against hostile bitfields.
//!
//! `Hello`/`BufferMap` carry an attacker-controlled (base_seq, bitfield). The node
//! caps bitfield size before this runs, but the type itself must hold up for any
//! input: no panics on decode, query, set, count, highest, prune, or re-encode.
//! Run: `cargo +nightly fuzz run buffermap_bytes`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use unstation_core::buffermap::BufferMap;

fuzz_target!(|input: (u64, Vec<u8>, u64, u64)| {
    let (base, bytes, probe, floor) = input;
    let mut b = BufferMap::from_bytes(base, &bytes[..bytes.len().min(8192)]);
    let _ = b.has(probe);
    let _ = b.count();
    let _ = b.highest();
    b.set(probe.min(base.saturating_add(1 << 20))); // bounded growth like the node's window
    b.prune_below(floor);
    let _ = b.to_bytes();
    let _ = b.has(probe);
});
