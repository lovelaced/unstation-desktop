//! Fuzz segment reassembly against hostile chunk sequences.
//!
//! A peer controls (total_len, offset, bytes) completely. Invariants: never
//! panics; buffered bytes never exceed total_len (the byte-budget contract the
//! node's global reassembly cap depends on); a completed assembly is exactly
//! total_len bytes. Run: `cargo +nightly fuzz run reassembler`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use unstation_core::reassembly::Reassembler;

fuzz_target!(|input: (u16, Vec<(u32, Vec<u8>)>)| {
    let (total, chunks) = input;
    let total = total as u32;
    let mut r = Reassembler::new(total);
    let mut accounted: u64 = 0;
    for (offset, bytes) in chunks {
        accounted += r.add(offset, &bytes) as u64;
        assert!(accounted <= total as u64, "buffered past total_len");
        assert_eq!(r.buffered_bytes(), accounted, "byte accounting drifted");
    }
    if r.is_complete() {
        let out = r.assemble().expect("complete implies assemblable");
        assert_eq!(out.len(), total as usize);
    }
});
