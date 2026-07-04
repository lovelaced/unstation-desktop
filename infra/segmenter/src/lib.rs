//! Real CMAF/fMP4 segmenter (TECH_SPEC §3.1, IMPLEMENTATION_SPEC §13.1).
//!
//! Drives `ffmpeg` to produce low-latency H.264 CMAF — a shared `init.mp4` plus
//! numbered `seg_NNNNN.m4s` fragments — then content-addresses each with
//! `blake2b256`. These are exactly the bytes that flow through the mesh; once a
//! viewer reassembles + verifies them and the HLS re-server serves them, the
//! platform player decodes **real video**.
//!
//! Two ways to feed it, both standard:
//!   - [`produce`] / [`demo_stream`] — batch (test pattern or a file) for the
//!     local demo + tests.
//!   - [`spawn`] with [`Source::RtmpListen`] — **the publisher's live ingest**:
//!     ffmpeg listens for an incoming RTMP publish and segments it as it arrives.
//!     This is the contribution path real encoders use — point **OBS** (Settings →
//!     Stream → Service: *Custom*, Server: `rtmp://127.0.0.1:<port>/live`, Stream
//!     Key: `<key>`) straight at it; no plugins, no special workflow.

use bytes::Bytes;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use unstation_core::crypto::segment_id;
use unstation_core::types::{SegmentId, Seq};

/// In-memory CMAF muxer for the non-ffmpeg (Android camera) publish path — see [`fmp4`].
mod fmp4;
pub mod sps;
pub use fmp4::{
    fragment_info, fragment_is_independent, FragmentBuilder, FragmentInfo, H264Params,
    AUDIO_TIMESCALE, OPUS_DEFAULT_FRAME_TICKS, TIMESCALE,
};

/// A content-addressed CMAF segment.
pub struct Segment {
    pub seq: Seq,
    pub id: SegmentId,
    pub bytes: Bytes,
}

/// A loaded CMAF stream: the init segment + ordered media fragments.
pub struct Cmaf {
    pub init: Bytes,
    pub segments: Vec<Segment>,
}

/// What to encode.
pub enum Source<'a> {
    /// Built-in test pattern (bars + tone) of `secs` seconds — demo + tests.
    TestPattern { secs: u32 },
    /// Transcode an existing media file.
    File(&'a Path),
    /// **Live ingest:** listen for an incoming RTMP publisher (OBS / any encoder)
    /// at `url` (e.g. from [`rtmp_url`]) and segment it as it arrives.
    RtmpListen { url: &'a str },
}

/// Resolve the `ffmpeg` binary to a path we can actually spawn.
///
/// macOS GUI apps (a launched `.app`) do NOT inherit the shell `PATH`, so a bare
/// `ffmpeg` lookup that works under `tauri dev` (terminal) fails in the bundled
/// app — Homebrew's `/opt/homebrew/bin` simply isn't on the GUI `PATH`. So we try,
/// in order: an explicit `UNSTATION_FFMPEG` override, then `PATH`, then the usual
/// install locations.
pub fn ffmpeg_path() -> Option<PathBuf> {
    let runs = |bin: &std::ffi::OsStr| {
        Command::new(bin)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if let Ok(p) = std::env::var("UNSTATION_FFMPEG") {
        let pb = PathBuf::from(p);
        if runs(pb.as_os_str()) {
            return Some(pb);
        }
    }
    if runs(std::ffi::OsStr::new("ffmpeg")) {
        return Some(PathBuf::from("ffmpeg"));
    }
    for cand in [
        "/opt/homebrew/bin/ffmpeg", // Apple Silicon Homebrew
        "/usr/local/bin/ffmpeg",    // Intel Homebrew
        "/opt/local/bin/ffmpeg",    // MacPorts
        "/usr/bin/ffmpeg",
    ] {
        let pb = PathBuf::from(cand);
        if pb.is_file() && runs(pb.as_os_str()) {
            return Some(pb);
        }
    }
    None
}

/// Is ffmpeg available anywhere we know to look (PATH or a common install dir)?
pub fn ffmpeg_available() -> bool {
    ffmpeg_path().is_some()
}

/// The publish URL to hand an encoder for a local ingest `port` + stream `key`.
/// In OBS this splits into Server `rtmp://127.0.0.1:<port>/live` + Stream Key `<key>`.
pub fn rtmp_url(port: u16, key: &str) -> String {
    format!("rtmp://127.0.0.1:{port}/live/{key}")
}

/// Build the ffmpeg command for `source` → CMAF in `out_dir`.
fn ffmpeg_command(source: &Source, out_dir: &Path, seg_secs: u32) -> Command {
    // Resolve an absolute path so this works in a bundled macOS app (no shell PATH).
    let bin = ffmpeg_path().unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let mut cmd = Command::new(bin);
    cmd.args(["-y", "-hide_banner"]);

    let live = match source {
        Source::TestPattern { secs } => {
            cmd.args(["-f", "lavfi", "-i"]);
            cmd.arg(format!("testsrc=size=640x360:rate=30:duration={secs}"));
            cmd.args(["-f", "lavfi", "-i"]);
            cmd.arg(format!("sine=frequency=440:duration={secs}"));
            false
        }
        Source::File(p) => {
            cmd.arg("-re").arg("-i").arg(p);
            false
        }
        Source::RtmpListen { url } => {
            // ffmpeg acts as the RTMP server; accepts a standard publisher (OBS).
            cmd.args(["-listen", "1", "-i", url]);
            true
        }
    };

    // Quiet for batch; verbose for the live ingest so ffmpeg.log shows the encoder
    // connecting (or why it didn't).
    cmd.args(["-loglevel", if live { "info" } else { "error" }]);

    // Low-latency encode. Keyframes are forced at EXACTLY the segment cadence
    // (`expr:gte(t,n_forced*seg)`) rather than via a fixed `-g <frames>`: a fixed GOP
    // only aligns with `-hls_time` at one assumed frame rate (30fps → -g 30), so a
    // 60fps or 24fps OBS source would drift, and the muxer would emit longer,
    // uneven segments — extra glass-to-glass latency and jerky live-edge tracking.
    // Forcing on the segment boundary keeps every segment ~`seg_secs` at any fps.
    let seg = seg_secs.max(1);
    cmd.args([
        "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
        "-force_key_frames", &format!("expr:gte(t,n_forced*{seg})"),
        "-bf", "0", "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", "96k",
        "-f", "hls",
        "-hls_time", &seg.to_string(),
        "-hls_flags", "independent_segments",
        "-hls_segment_type", "fmp4",
        "-hls_fmp4_init_filename", "init.mp4",
        "-hls_list_size", "0",
        "-hls_segment_filename",
    ]);
    cmd.arg(out_dir.join("seg_%05d.m4s"));
    if !live {
        // Finite sources get a closed VOD playlist; live ingest stays open-ended.
        cmd.args(["-hls_playlist_type", "vod"]);
    }
    cmd.arg(out_dir.join("live.m3u8"));
    cmd
}

/// Produce CMAF into `out_dir` and block until the (finite) source completes.
/// Use [`spawn`] for live ingest. Low-latency encode (no B-frames, 1 s GOP).
pub fn produce(source: &Source, out_dir: &Path, seg_secs: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let status = ffmpeg_command(source, out_dir, seg_secs).status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("ffmpeg exited with {status}")));
    }
    Ok(())
}

/// A running ffmpeg segmenter (live ingest). Drop or [`stop`](SegmenterProcess::stop)
/// to end it.
pub struct SegmenterProcess {
    child: Child,
    pub out_dir: PathBuf,
}

impl SegmenterProcess {
    /// True while ffmpeg is still running (a publisher is connected / awaited).
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    /// Stop the segmenter.
    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SegmenterProcess {
    /// Kill ffmpeg when the handle goes away (a respawn, or the publish session
    /// ending) so the RTMP port is freed and no orphan encoder lingers.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn ffmpeg to segment `source` into `out_dir` live; returns immediately.
/// For [`Source::RtmpListen`] ffmpeg binds the port and waits for a publisher.
pub fn spawn(source: &Source, out_dir: &Path, seg_secs: u32) -> std::io::Result<SegmenterProcess> {
    std::fs::create_dir_all(out_dir)?;
    let mut cmd = ffmpeg_command(source, out_dir, seg_secs);
    cmd.stdin(Stdio::null());
    // Capture ffmpeg's own log so a stuck or failed ingest is diagnosable.
    if let Ok(log) = std::fs::File::create(out_dir.join("ffmpeg.log")) {
        cmd.stderr(Stdio::from(log));
    }
    let child = cmd.spawn()?;
    Ok(SegmenterProcess { child, out_dir: out_dir.to_path_buf() })
}

/// Read the init segment if present yet.
pub fn load_init(out_dir: &Path) -> Option<Bytes> {
    std::fs::read(out_dir.join("init.mp4")).ok().map(Bytes::from)
}

/// Load every `*.m4s` fragment in order, content-addressed. Works on a live dir
/// too (a snapshot of whatever's been written so far).
pub fn load(out_dir: &Path) -> std::io::Result<Cmaf> {
    let init = load_init(out_dir).ok_or_else(|| std::io::Error::other("no init.mp4 yet"))?;
    let segments = load_segments_from(out_dir, 0)?;
    Ok(Cmaf { init, segments })
}

/// Load fragments with sequence >= `from`, from a snapshot of the dir. Reads every
/// `.m4s` present — use [`load_live_segments_from`] for a live tail, where the
/// newest file may still be being written.
pub fn load_segments_from(out_dir: &Path, from: Seq) -> std::io::Result<Vec<Segment>> {
    let paths = sorted_segment_paths(out_dir)?;
    segments_from_paths(&paths, from)
}

/// Like [`load_segments_from`] but holds back the newest fragment — for the LIVE
/// tail, where the highest-numbered `.m4s` is the one ffmpeg is still writing.
/// Reading it would capture a truncated, undecodable segment (the player then shows
/// nothing). ffmpeg only opens segment N+1 after closing N, so any fragment that has
/// a higher-numbered sibling is guaranteed complete.
pub fn load_live_segments_from(out_dir: &Path, from: Seq) -> std::io::Result<Vec<Segment>> {
    let mut paths = sorted_segment_paths(out_dir)?;
    paths.pop(); // drop the in-progress (newest) fragment
    segments_from_paths(&paths, from)
}

fn sorted_segment_paths(out_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<_> = std::fs::read_dir(out_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("m4s"))
        .collect();
    paths.sort();
    Ok(paths)
}

fn segments_from_paths(paths: &[PathBuf], from: Seq) -> std::io::Result<Vec<Segment>> {
    let mut out = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let seq = i as Seq;
        if seq < from {
            continue;
        }
        let bytes = Bytes::from(std::fs::read(p)?);
        out.push(Segment { seq, id: segment_id(&bytes), bytes });
    }
    Ok(out)
}

/// Convenience: produce + load a test-pattern stream in one call (batch).
pub fn demo_stream(out_dir: &Path, secs: u32, seg_secs: u32) -> std::io::Result<Cmaf> {
    produce(&Source::TestPattern { secs }, out_dir, seg_secs)?;
    load(out_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn probe_is_h264(init: &[u8], seg: &[u8], dir: &Path) -> bool {
        let probe = dir.join("probe.mp4");
        let mut buf = init.to_vec();
        buf.extend_from_slice(seg);
        std::fs::write(&probe, &buf).unwrap();
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
            .arg(&probe)
            .output()
            .expect("ffprobe runs");
        String::from_utf8_lossy(&out.stdout).contains("h264")
    }

    #[test]
    fn produces_real_h264_cmaf() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not found — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cmaf = demo_stream(dir.path(), 4, 1).expect("produce CMAF");
        assert!(!cmaf.init.is_empty());
        assert!(cmaf.segments.len() >= 3, "got {}", cmaf.segments.len());
        let mut ids: Vec<_> = cmaf.segments.iter().map(|s| s.id.0).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), cmaf.segments.len(), "unique content ids");
        assert!(probe_is_h264(&cmaf.init, &cmaf.segments[0].bytes, dir.path()));
    }

    #[test]
    fn segment_cadence_tracks_seg_secs() {
        // Forced keyframes on the segment boundary mean the segment COUNT scales with
        // the requested duration: an 8 s source at 1 s ≈ 8 segments, at 2 s ≈ 4. A
        // fixed `-g` would have pinned the cadence regardless. (Boundaries can round,
        // so assert the ratio, not exact counts.)
        if !ffmpeg_available() {
            eprintln!("ffmpeg not found — skipping");
            return;
        }
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let one = demo_stream(d1.path(), 8, 1).expect("1s segments");
        let two = demo_stream(d2.path(), 8, 2).expect("2s segments");
        assert!(
            one.segments.len() > two.segments.len(),
            "1s cadence ({}) should yield more segments than 2s ({})",
            one.segments.len(),
            two.segments.len(),
        );
    }

    #[test]
    fn rtmp_ingest_from_a_standard_publisher() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not found — skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let url = rtmp_url(21937, "stream");

        // Publisher's live ingest: ffmpeg listens for an OBS-style RTMP publish.
        let seg = spawn(&Source::RtmpListen { url: &url }, dir.path(), 1).unwrap();
        std::thread::sleep(Duration::from_millis(500)); // let the listener bind

        // A standard RTMP client — exactly what OBS sends. Push ~5 s and exit.
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "error", "-re",
                "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=5",
                "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                "-g", "30", "-f", "flv", &url,
            ])
            .status()
            .expect("publisher runs");
        assert!(status.success(), "OBS-style RTMP publish should be accepted");

        std::thread::sleep(Duration::from_millis(800)); // flush the last fragment
        seg.stop();

        let cmaf = load(dir.path()).expect("CMAF written from the ingest");
        assert!(!cmaf.init.is_empty(), "init.mp4 written");
        assert!(cmaf.segments.len() >= 3, "live fragments written, got {}", cmaf.segments.len());
        assert!(
            probe_is_h264(&cmaf.init, &cmaf.segments[0].bytes, dir.path()),
            "ingested media decodes as H.264"
        );
    }
}
