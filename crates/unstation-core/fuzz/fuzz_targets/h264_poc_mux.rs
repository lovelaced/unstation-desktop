//! Fuzz the H.264 POC slice/SPS parsers against hostile bitstream bytes.
//!
//! Real encoders stamp WHIP RTP with decode-order timestamps and hide the B-frame reorder in
//! the bitstream POC; the muxer parses an untrusted SPS (`parse_sps`) and each untrusted coded
//! slice header (`slice_poc_lsb`, `PocTracker`) to recover presentation order. Those parsers
//! live in a private module, reached from outside the crate only through the public muxer —
//! so we split the input into an SPS + one access unit and drive `push_au_pts`, which routes
//! both through exactly those parsers. Any input must yield a fragment or nothing, never a
//! panic. Run: `cargo +nightly fuzz run h264_poc_mux`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use segmenter::{FragmentBuilder, H264Params};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte chooses the SPS/access-unit split so the corpus explores both parsers.
    let split = (data[0] as usize) % data.len();
    let (sps, nal) = data.split_at(split);
    let params = H264Params { sps: sps.to_vec(), pps: vec![0x68, 0xee], width: 320, height: 240 };
    let mut fb = FragmentBuilder::new_ll(params, 100);
    let _ = fb.init_segment();
    // Two AUs (a keyframe then a non-keyframe) so the POC tracker + composition-offset emit run.
    let _ = fb.push_au_pts(nal, 0, true);
    let _ = fb.push_au_pts(nal, 33_000, false);
    let _ = fb.flush();
});
