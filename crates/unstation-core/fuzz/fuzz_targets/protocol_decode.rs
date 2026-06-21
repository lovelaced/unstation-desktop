//! Fuzz the SCALE wire decoder against hostile peer input.
//!
//! Peers are untrusted by design (TECH_SPEC §11): decoding arbitrary bytes must
//! never panic — only ever return `Ok`/`Err`. Run: `cargo +nightly fuzz run protocol_decode`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use parity_scale_codec::Decode;
use unstation_core::protocol::MeshMsg;

fuzz_target!(|data: &[u8]| {
    let _ = MeshMsg::decode(&mut &data[..]);
});
