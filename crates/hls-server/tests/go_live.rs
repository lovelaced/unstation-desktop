//! D5 Go Live, end-to-end: OBS → publisher → mesh → viewer → playable.
//!
//! A standard RTMP publisher (what OBS is) streams into the publisher's live
//! ingest; the segmenter produces real CMAF as it arrives; a feeder injects each
//! fragment into a live publisher `MeshNode` (`Produced`) and announces it to a
//! viewer (`LiveEdge`); the viewer pulls fragments over the mesh, hash-verifies
//! them, and feeds the localhost HLS server; `ffprobe` plays the result back and
//! confirms decodable H.264. Skips if ffmpeg/ffprobe are absent.

use bytes::Bytes;
use hls_server::HlsServer;
use segmenter::{ffmpeg_available, load_init, load_segments_from, rtmp_url, spawn as seg_spawn, Source};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use unstation_core::config::{MeshConfig, Mode, Role};
use unstation_core::media::MediaSink;
use unstation_core::node::MeshNode;
use unstation_core::transport::EngineEvent;
use unstation_core::transport_mem::wire;
use unstation_core::types::PeerId;

struct NullSink;
impl MediaSink for NullSink {
    fn push_init(&self, _: Bytes) {}
    fn push_segment(&self, _: u64, _: Bytes) {}
    fn on_play_head(&self) -> u64 {
        0
    }
}

fn cfg(role: Role) -> MeshConfig {
    MeshConfig {
        mode: Mode::Live,
        role,
        window: 64,
        tick: Duration::from_millis(8),
        seg_ms: 1000,
        upload_budget_bps: 80_000_000,
        weights: Default::default(),
    }
}

// Real-media e2e: ffmpeg + ffprobe subprocesses + localhost serving + timing.
// Inherently timing-sensitive, so it's not in the always-green set; the
// deterministic live path is covered by `unstation-core/tests/live_publish.rs`.
// Run on demand: `cargo test -p hls-server --test go_live -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real-media e2e (ffmpeg/ffprobe + timing); run with --ignored"]
async fn go_live_obs_to_viewer_over_mesh() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not found — skipping Go Live e2e");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dirp = dir.path().to_path_buf();
    let url = rtmp_url(21939, "stream");

    // 1) The publisher's live RTMP ingest — what OBS connects to.
    let seg = seg_spawn(&Source::RtmpListen { url: &url }, &dirp, 1).expect("ingest starts");
    tokio::time::sleep(Duration::from_millis(500)).await; // let the listener bind

    // 2) A standard RTMP publisher == OBS. Stream ~6 s in the background.
    let mut obs = Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-re",
            "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=6",
            "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
            "-g", "30", "-f", "flv", &url,
        ])
        .spawn()
        .expect("OBS-style publisher starts");

    // 3) The viewer's localhost HLS playback feed.
    let hls = HlsServer::start(1000).unwrap();
    let sink = hls.sink();
    let addr = hls.addr();

    // 4) Publisher + viewer mesh nodes over the in-memory transport.
    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
    let (lp, lv) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: lp }).unwrap();
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: lv }).unwrap();

    let publisher =
        MeshNode::new_live_publisher(pubid, cfg(Role::Publisher), 200_000, Arc::new(NullSink));
    tokio::spawn(publisher.run(prx, Duration::from_millis(8), None));

    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        200_000,
        Arc::new(sink.clone()),
        HashMap::new(),
        0,
    );

    // 5) Feeder: tail the live segmenter dir → init to the sink, fragments to the
    //    publisher (`Produced`) and to the viewer's live edge (`LiveEdge`).
    let ptx_f = ptx.clone();
    let vtx_f = vtx.clone();
    let sink_f = sink.clone();
    let dirf = dirp.clone();
    let feeder = tokio::spawn(async move {
        let mut seen = 0u64;
        let mut init_sent = false;
        loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if !init_sent {
                if let Some(init) = load_init(&dirf) {
                    sink_f.push_init(init);
                    init_sent = true;
                }
            }
            if let Ok(news) = load_segments_from(&dirf, seen) {
                for s in news {
                    let _ = ptx_f.send(EngineEvent::Produced { seq: s.seq, id: s.id, bytes: s.bytes });
                    let _ = vtx_f.send(EngineEvent::LiveEdge { seq: s.seq, id: s.id });
                    seen = s.seq + 1;
                }
            }
        }
    });

    // 6) Run the viewer until it has pulled several live fragments over the mesh.
    let target = 3usize;
    let stats = tokio::time::timeout(
        Duration::from_secs(25),
        viewer.run(vrx, Duration::from_millis(8), Some(target)),
    )
    .await
    .expect("viewer should pull live fragments");
    assert!(stats.delivered >= target, "pulled live fragments, got {}", stats.delivered);
    assert_eq!(stats.hash_failures, 0, "every live fragment hash-verified");

    // 7) Play the served live stream back — must decode as H.264.
    let probe = Command::new("ffprobe")
        .args([
            "-v", "error", "-rw_timeout", "5000000",
            "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0",
        ])
        .arg(format!("http://{addr}/live.m3u8"))
        .output()
        .expect("ffprobe runs");
    let codec = String::from_utf8_lossy(&probe.stdout);
    assert!(
        codec.contains("h264"),
        "served live HLS must decode as H.264 — stdout={codec:?} stderr={:?}",
        String::from_utf8_lossy(&probe.stderr)
    );

    let _ = ptx.send(EngineEvent::Stop);
    feeder.abort();
    let _ = obs.kill();
    let _ = obs.wait();
    seg.stop();
}
