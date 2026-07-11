//! Fuzz the H.264 SPS dimension parser against hostile NAL bytes.
//!
//! The WHIP ingest path feeds attacker-controlled SPS bytes straight into
//! `segmenter::sps::dimensions` to size the CMAF init segment. Decoding arbitrary
//! bytes must never panic — only ever return `Some((w, h))` or `None`.
//! Run: `cargo +nightly fuzz run sps_dimensions`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = segmenter::sps::dimensions(data);
});
