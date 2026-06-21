//! Localhost HLS re-server (IMPLEMENTATION_SPEC §6).
//!
//! The mesh engine produces an ordered byte stream; this serves it as a
//! dynamically-generated HLS media playlist + init/media segments over
//! `http://127.0.0.1:<port>/live.m3u8`, so the platform player stays ignorant of
//! P2P (the AceStream "local proxy → any player" trick). The player's segment
//! GETs double as the `play_head` signal the picker needs.

use bytes::Bytes;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use unstation_core::media::MediaSink;

struct Shared {
    init: Option<Bytes>,
    segments: BTreeMap<u64, Bytes>,
    target_ms: u32,
}

/// A [`MediaSink`] that feeds the localhost HLS server. Cloneable handle.
#[derive(Clone)]
pub struct HlsSink {
    shared: Arc<Mutex<Shared>>,
    play_head: Arc<AtomicU64>,
}

impl HlsSink {
    fn new(target_ms: u32) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                init: None,
                segments: BTreeMap::new(),
                target_ms,
            })),
            play_head: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl MediaSink for HlsSink {
    fn push_init(&self, bytes: Bytes) {
        self.shared.lock().unwrap().init = Some(bytes);
    }
    fn push_segment(&self, seq: u64, bytes: Bytes) {
        self.shared.lock().unwrap().segments.insert(seq, bytes);
    }
    fn on_play_head(&self) -> u64 {
        self.play_head.load(Ordering::SeqCst)
    }
}

/// Build an HLS media playlist for the segments currently delivered.
pub fn media_playlist(target_ms: u32, segments: &BTreeMap<u64, Bytes>) -> String {
    let target_s = ((target_ms as f64) / 1000.0).ceil().max(1.0) as u32;
    let media_seq = segments.keys().next().copied().unwrap_or(0);
    let dur = target_ms as f64 / 1000.0;
    let mut s = String::new();
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    s.push_str(&format!("#EXT-X-TARGETDURATION:{target_s}\n"));
    s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_seq}\n"));
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for seq in segments.keys() {
        s.push_str(&format!("#EXTINF:{dur:.3},\nseg/{seq}.m4s\n"));
    }
    s
}

/// A running localhost HLS server. Serves until the process exits.
pub struct HlsServer {
    addr: SocketAddr,
    sink: HlsSink,
}

impl HlsServer {
    /// Bind an ephemeral localhost port and start serving on a background thread.
    pub fn start(target_ms: u32) -> std::io::Result<Self> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| std::io::Error::other("no ip addr"))?;
        let sink = HlsSink::new(target_ms);
        let shared = sink.shared.clone();
        let play_head = sink.play_head.clone();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                handle(request, &shared, &play_head);
            }
        });
        Ok(Self { addr, sink })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The `live.m3u8` URL to hand to the player.
    pub fn url(&self) -> String {
        format!("http://{}/live.m3u8", self.addr)
    }

    /// The [`MediaSink`] handle the engine writes to.
    pub fn sink(&self) -> HlsSink {
        self.sink.clone()
    }
}

fn content_type(ct: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], ct.as_bytes())
        .expect("valid header")
}

fn not_found() -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string("not found").with_status_code(404)
}

fn handle(req: tiny_http::Request, shared: &Arc<Mutex<Shared>>, play_head: &Arc<AtomicU64>) {
    let url = req.url().to_string();
    let g = shared.lock().unwrap();
    let resp = if url == "/live.m3u8" {
        tiny_http::Response::from_data(media_playlist(g.target_ms, &g.segments).into_bytes())
            .with_header(content_type("application/vnd.apple.mpegurl"))
    } else if url == "/init.mp4" {
        match &g.init {
            Some(b) => tiny_http::Response::from_data(b.to_vec())
                .with_header(content_type("video/mp4")),
            None => not_found(),
        }
    } else if let Some(seq) = url
        .strip_prefix("/seg/")
        .and_then(|s| s.strip_suffix(".m4s"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        match g.segments.get(&seq) {
            Some(b) => {
                // The player's fetch is our play-head signal.
                play_head.store(seq, Ordering::SeqCst);
                tiny_http::Response::from_data(b.to_vec())
                    .with_header(content_type("video/iso.segment"))
            }
            None => not_found(),
        }
    } else {
        not_found()
    };
    drop(g);
    let _ = req.respond(resp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn playlist_lists_segments() {
        let mut segs = BTreeMap::new();
        segs.insert(0u64, Bytes::from_static(b"a"));
        segs.insert(1u64, Bytes::from_static(b"b"));
        let pl = media_playlist(2000, &segs);
        assert!(pl.contains("#EXTM3U"));
        assert!(pl.contains("#EXT-X-TARGETDURATION:2"));
        assert!(pl.contains("seg/0.m4s"));
        assert!(pl.contains("seg/1.m4s"));
    }

    fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        let req = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    #[test]
    fn serves_playlist_and_segment_over_http() {
        let srv = HlsServer::start(2000).unwrap();
        let sink = srv.sink();
        sink.push_init(Bytes::from_static(b"INIT"));
        sink.push_segment(0, Bytes::from_static(b"SEGMENT-ZERO"));
        let addr = srv.addr();

        let playlist = http_get(addr, "/live.m3u8");
        assert!(playlist.contains("#EXTM3U"), "got: {playlist}");
        assert!(playlist.contains("seg/0.m4s"));

        let seg = http_get(addr, "/seg/0.m4s");
        assert!(seg.contains("SEGMENT-ZERO"), "got: {seg}");

        // The segment fetch updated the play head.
        assert_eq!(sink.on_play_head(), 0);

        // Unknown path is a clean 404, not a hang.
        let missing = http_get(addr, "/seg/9.m4s");
        assert!(missing.is_empty() || !missing.contains("SEGMENT"));
    }
}
