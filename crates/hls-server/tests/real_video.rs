//! D5 end-to-end: real video → real mesh → real HLS → decodable by a real player.
//!
//! ffmpeg produces real H.264 CMAF; the fragments flow through a real
//! publisher→viewer mesh (picker, 16 KiB chunking, reassembly, `blake2b256`
//! verification); the viewer feeds them to the localhost HLS re-server; and
//! `ffprobe` plays the served playlist back and confirms a decodable H.264 stream.
//! This is the proof that the whole path carries genuine, playable media.
//!
//! Skips cleanly if ffmpeg/ffprobe aren't installed.

use bytes::Bytes;
use hls_server::HlsServer;
use segmenter::{demo_stream, ffmpeg_available};
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
use unstation_core::types::{PeerId, SegmentId, Seq};

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
        mode: Mode::Vod,
        role,
        window: 64,
        tick: Duration::from_millis(8),
        seg_ms: 1000,
        upload_budget_bps: 80_000_000,
        weights: Default::default(),
    }
}

#[tokio::test]
async fn real_video_flows_through_mesh_and_is_playable() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not found — skipping real-video e2e");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cmaf = demo_stream(dir.path(), 4, 1).expect("produce CMAF");
    let n = cmaf.segments.len();
    assert!(n >= 3, "expected several fragments");

    // Real localhost HLS re-server, seeded with the real init segment.
    let hls = HlsServer::start(1000).unwrap();
    let sink = hls.sink();
    sink.push_init(cmaf.init.clone());
    let addr = hls.addr();

    let segments: Vec<Bytes> = cmaf.segments.iter().map(|s| s.bytes.clone()).collect();
    let segment_ids: HashMap<Seq, SegmentId> =
        cmaf.segments.iter().map(|s| (s.seq, s.id)).collect();

    let pubid = PeerId::from_u64(1);
    let viewid = PeerId::from_u64(2);
    let (ptx, prx) = mpsc::unbounded_channel::<EngineEvent>();
    let (vtx, vrx) = mpsc::unbounded_channel::<EngineEvent>();
    let (lp, lv) = wire(pubid, ptx.clone(), viewid, vtx.clone());
    ptx.send(EngineEvent::PeerConnected { peer: viewid, link: lp }).unwrap();
    vtx.send(EngineEvent::PeerConnected { peer: pubid, link: lv }).unwrap();

    let publisher =
        MeshNode::new_publisher(pubid, cfg(Role::Publisher), 200_000, Arc::new(NullSink), segments);
    tokio::spawn(publisher.run(prx, Duration::from_millis(8), None));

    let viewer = MeshNode::new_viewer(
        viewid,
        cfg(Role::Viewer),
        200_000,
        Arc::new(sink),
        segment_ids,
        (n - 1) as Seq,
    );
    let stats = tokio::time::timeout(
        Duration::from_secs(15),
        viewer.run(vrx, Duration::from_millis(8), Some(n)),
    )
    .await
    .expect("viewer should finish");
    ptx.send(EngineEvent::Stop).ok();

    assert_eq!(stats.delivered, n, "all fragments delivered through the mesh");
    assert_eq!(stats.hash_failures, 0, "every fragment hash-verified");

    // ffprobe plays the served playlist back — must decode as H.264.
    let url = format!("http://{addr}/live.m3u8");
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-rw_timeout", "5000000",
            "-select_streams", "v:0",
            "-show_entries", "stream=codec_name",
            "-of", "csv=p=0",
        ])
        .arg(&url)
        .output()
        .expect("ffprobe runs");
    let codec = String::from_utf8_lossy(&out.stdout);
    assert!(
        codec.contains("h264"),
        "served HLS must decode as H.264 — got stdout={codec:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
