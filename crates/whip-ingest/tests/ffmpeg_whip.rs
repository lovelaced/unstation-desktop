//! On-the-wire WHIP ingest validation, mirroring `segmenter::rtmp_ingest_from_a_standard_publisher`.
//!
//! ffmpeg 8.1+ ships a `whip` muxer — a faithful stand-in for OBS 30's WHIP output
//! (SDP offer over HTTP, DTLS, H.264 RTP). This test starts the WHIP server, points
//! ffmpeg at it, and asserts real access units — with codec config and a keyframe —
//! arrive through the depacketizer. `#[ignore]`d: needs ffmpeg + the media
//! libdatachannel build, so it runs on demand, not in the fast suite:
//!
//!   cargo test -p whip-ingest --features server -- --ignored --nocapture

#![cfg(feature = "server")]

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_has_whip() -> bool {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-muxers"]).output();
    matches!(out, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("whip"))
}

#[test]
#[ignore = "needs ffmpeg (whip muxer) + the media libdatachannel build"]
fn ffmpeg_whip_publisher_delivers_verified_access_units() {
    if !ffmpeg_available() || !ffmpeg_has_whip() {
        eprintln!("ffmpeg with a whip muxer not found — skipping");
        return;
    }
    // Localhost ICE: libjuice needs an explicit bind to offer 127.0.0.1 candidates.
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");

    let (tx, rx) = mpsc::channel();
    let server = whip_ingest::server::start(tx, vec![]).expect("start WHIP endpoint");
    let url = server.url();
    eprintln!("[test] WHIP endpoint at {url}");
    // Let the HTTP listener bind.
    std::thread::sleep(Duration::from_millis(300));

    // A standard WHIP publisher — exactly ffmpeg's OBS-equivalent output. Push ~6s of a
    // moving test pattern with an IDR every second (so config + a keyframe arrive fast).
    let mut pubr = Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-re",
            "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=6",
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
            "-bf", "0", "-g", "30", "-pix_fmt", "yuv420p",
            "-f", "whip", &url,
        ])
        .spawn()
        .expect("spawn ffmpeg whip publisher");

    // Collect access units for up to 12s; success = codec config + a keyframe + several units.
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut units = 0usize;
    let mut got_config = false;
    let mut got_keyframe = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ingest) => {
                units += 1;
                got_config |= ingest.config.is_some();
                got_keyframe |= ingest.au.keyframe;
                if got_config && got_keyframe && units >= 10 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pubr.try_wait().ok().flatten().is_some() {
                    break; // publisher exited (finished its 6s or errored)
                }
            }
            Err(_) => break,
        }
    }
    let _ = pubr.kill();
    let _ = pubr.wait();
    drop(server);

    assert!(got_config, "SPS/PPS codec config must arrive from the WHIP stream");
    assert!(got_keyframe, "at least one IDR access unit must arrive");
    assert!(units >= 10, "expected a run of access units, got {units}");
    eprintln!("[test] OK — {units} access units, config + keyframe received");
}
