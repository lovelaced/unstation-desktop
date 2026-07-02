//! Localhost HLS re-server (IMPLEMENTATION_SPEC §6).
//!
//! The mesh engine produces an ordered byte stream; this serves it as a
//! dynamically-generated HLS media playlist + init/media segments over
//! `http://127.0.0.1:<port>/live.m3u8`, so the platform player stays ignorant of
//! P2P (the AceStream "local proxy → any player" trick). The player's segment
//! GETs double as the `play_head` signal the picker needs.
//!
//! ## Two modes
//!
//! * **Standard** ([`HlsServer::start`]): each mesh segment is a full ~1s GOP → one
//!   `#EXTINF`. Compatible with everything; ~3–5s glass-to-glass.
//! * **Low-latency** ([`HlsServer::start_ll`]): the publisher's [`FragmentBuilder`] emits
//!   ~200ms CMAF *parts*, each still one mesh segment. This server rolls parts into parent
//!   segments at keyframe boundaries — recovering independence + duration straight from the
//!   fragment bytes, so nothing extra crosses the mesh — and emits an LL-HLS playlist
//!   (`#EXT-X-PART`, `#EXT-X-PRELOAD-HINT`, `CAN-BLOCK-RELOAD`). The player fetches parts as
//!   they're produced instead of waiting for whole segments, which is the dominant
//!   player-buffer win (~1.5s glass-to-glass). Blocking playlist reload holds the
//!   `_HLS_msn`/`_HLS_part` request until that part exists.
//!
//! [`FragmentBuilder`]: segmenter::FragmentBuilder

use bytes::Bytes;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use unstation_core::media::MediaSink;

/// Max full-GOP segments retained in the standard live window — enough for hls.js's
/// back-buffer + `liveSyncDurationCount` with margin, small enough to keep playback near the
/// live edge and memory bounded.
const MAX_LIVE_SEGMENTS: usize = 12;
/// Max *parts* retained in the low-latency window. Parts are ~200ms, so ~60 ≈ 12s — the same
/// order of back-buffer as the standard window, still bounded.
const MAX_LIVE_PARTS: usize = 60;

/// One delivered fragment. In standard mode it's a whole GOP (always `independent`); in LL
/// mode it's a part, and consecutive parts sharing a `parent_msn` form one HLS segment.
struct Part {
    bytes: Bytes,
    /// Starts with a keyframe → a valid parent-segment boundary (`INDEPENDENT=YES`).
    independent: bool,
    /// Presentation duration in 90kHz ticks (from the `trun`); 0 if unknown.
    dur_ticks: u64,
    /// HLS media-sequence number of the parent segment this part belongs to.
    parent_msn: u64,
}

struct Shared {
    init: Option<Bytes>,
    /// seq → part, in delivery order. `seq` matches the mesh/`Segment` seq (and part URIs).
    parts: BTreeMap<u64, Part>,
    /// Parent-segment target duration (seconds are ceil'd for `#EXT-X-TARGETDURATION`).
    target_ms: u32,
    /// Low-latency: emit `#EXT-X-PART` + support blocking reload.
    ll: bool,
    /// Nominal part duration (LL only) — used for `PART-TARGET` and as a duration fallback.
    part_ms: u32,
    /// Next parent media-sequence number to assign (monotonic; survives eviction).
    next_parent_msn: u64,
    /// Parent msn currently being filled.
    cur_parent_msn: u64,
    /// Whether any part has been pushed since the last reset (seeds the first parent).
    have_any: bool,
}

/// A [`MediaSink`] that feeds the localhost HLS server. Cloneable handle.
#[derive(Clone)]
pub struct HlsSink {
    shared: Arc<Mutex<Shared>>,
    /// Signalled on every push so blocking-reload requests wake promptly.
    cv: Arc<Condvar>,
    play_head: Arc<AtomicU64>,
}

impl HlsSink {
    fn new(target_ms: u32, ll: bool, part_ms: u32) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                init: None,
                parts: BTreeMap::new(),
                target_ms,
                ll,
                part_ms,
                next_parent_msn: 0,
                cur_parent_msn: 0,
                have_any: false,
            })),
            cv: Arc::new(Condvar::new()),
            play_head: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Switch the live window into (or out of) low-latency mode. The viewer learns `ll_mode`
    /// only after it verifies the publisher's manifest — which happens before any media
    /// fragment is delivered — so it starts the server in standard mode and calls this once
    /// the manifest is in hand. `part_ms`/`target_ms` of 0 leave the current value unchanged.
    pub fn configure(&self, ll: bool, part_ms: u32, target_ms: u32) {
        let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        g.ll = ll;
        if part_ms > 0 {
            g.part_ms = part_ms;
        }
        if target_ms > 0 {
            g.target_ms = target_ms;
        }
    }

    /// Clear the current init + segments — used when the ingest restarts a session,
    /// so the player loads the fresh feed cleanly instead of stale fragments.
    pub fn reset(&self) {
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.init = None;
            g.parts.clear();
            g.next_parent_msn = 0;
            g.cur_parent_msn = 0;
            g.have_any = false;
        }
        self.play_head.store(0, Ordering::SeqCst);
        self.cv.notify_all();
    }
}

impl MediaSink for HlsSink {
    fn push_init(&self, bytes: Bytes) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).init = Some(bytes);
    }
    fn push_segment(&self, seq: u64, bytes: Bytes) {
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            // In LL mode, learn independence + duration from the fragment bytes; a keyframe
            // (or the very first part) opens a new parent segment. Standard mode treats every
            // GOP as its own independent parent.
            let (independent, dur_ticks) = if g.ll {
                match segmenter::fragment_info(&bytes) {
                    Some(i) => (i.independent, i.duration_ticks),
                    None => (false, 0),
                }
            } else {
                (true, 0)
            };
            if !g.ll || !g.have_any || independent {
                g.cur_parent_msn = g.next_parent_msn;
                g.next_parent_msn += 1;
            }
            g.have_any = true;
            let parent_msn = g.cur_parent_msn;
            g.parts.insert(seq, Part { bytes, independent, dur_ticks, parent_msn });

            let cap = if g.ll { MAX_LIVE_PARTS } else { MAX_LIVE_SEGMENTS };
            while g.parts.len() > cap {
                let Some(&oldest) = g.parts.keys().next() else { break };
                g.parts.remove(&oldest);
            }
        }
        // Wake any blocking-reload requests waiting for this part.
        self.cv.notify_all();
    }
    fn on_play_head(&self) -> u64 {
        self.play_head.load(Ordering::SeqCst)
    }
}

/// Highest `(parent_msn, part_index_within_parent)` currently available — the comparison
/// point for a blocking-reload `_HLS_msn`/`_HLS_part` request.
fn live_edge(parts: &BTreeMap<u64, Part>) -> Option<(u64, u64)> {
    let (_, last) = parts.iter().next_back()?;
    let idx = parts.values().filter(|p| p.parent_msn == last.parent_msn).count() as u64 - 1;
    Some((last.parent_msn, idx))
}

/// Standard HLS media playlist — one `#EXTINF` per full-GOP segment.
fn standard_playlist(target_ms: u32, parts: &BTreeMap<u64, Part>) -> String {
    let target_s = ((target_ms as f64) / 1000.0).ceil().max(1.0) as u32;
    let media_seq = parts.keys().next().copied().unwrap_or(0);
    let dur = target_ms as f64 / 1000.0;
    let mut s = String::new();
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    s.push_str(&format!("#EXT-X-TARGETDURATION:{target_s}\n"));
    s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_seq}\n"));
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for seq in parts.keys() {
        s.push_str(&format!("#EXTINF:{dur:.3},\nseg/{seq}.m4s\n"));
    }
    s
}

/// Low-latency HLS playlist: parts grouped into parent segments, `#EXT-X-PART` for each part,
/// the growing parent left open with a `#EXT-X-PRELOAD-HINT` for its next part.
fn ll_playlist(sh: &Shared) -> String {
    let ticks_to_s = |t: u64| t as f64 / segmenter::TIMESCALE as f64;
    let part_nominal = ticks_to_s((sh.part_ms as u64 * segmenter::TIMESCALE as u64) / 1000);
    let dur_of = |p: &Part| if p.dur_ticks > 0 { ticks_to_s(p.dur_ticks) } else { part_nominal };

    // Group parts by parent, preserving seq order; also find the max part/parent durations so
    // PART-TARGET/TARGETDURATION are never exceeded by an announced part (hls.js rejects that).
    let mut parents: Vec<(u64, Vec<(&u64, &Part)>)> = Vec::new();
    let (mut max_part, mut max_parent) = (part_nominal, 0.0f64);
    for (seq, p) in &sh.parts {
        max_part = max_part.max(dur_of(p));
        match parents.last_mut() {
            Some((msn, list)) if *msn == p.parent_msn => list.push((seq, p)),
            _ => parents.push((p.parent_msn, vec![(seq, p)])),
        }
    }
    for (_, list) in &parents {
        max_parent = max_parent.max(list.iter().map(|(_, p)| dur_of(p)).sum());
    }

    let media_seq = sh.parts.values().next().map(|p| p.parent_msn).unwrap_or(0);
    let next_seq = sh.parts.keys().next_back().map(|s| s + 1).unwrap_or(0);
    let part_hold = (max_part * 3.0).max(part_nominal * 3.0);
    let target_s = max_parent.max(sh.target_ms as f64 / 1000.0).ceil().max(1.0);

    let mut s = String::new();
    s.push_str("#EXTM3U\n#EXT-X-VERSION:9\n");
    s.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_s as u32));
    s.push_str(&format!(
        "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={part_hold:.3},HOLD-BACK={:.3}\n",
        (target_s * 3.0).max(3.0)
    ));
    s.push_str(&format!("#EXT-X-PART-INF:PART-TARGET={max_part:.3}\n"));
    s.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_seq}\n"));
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");

    let last_parent = parents.len().saturating_sub(1);
    for (i, (msn, list)) in parents.iter().enumerate() {
        for (seq, p) in list {
            let indep = if p.independent { ",INDEPENDENT=YES" } else { "" };
            s.push_str(&format!(
                "#EXT-X-PART:DURATION={:.3},URI=\"part/{seq}.m4s\"{indep}\n",
                dur_of(p)
            ));
        }
        // Every parent except the one still being filled is complete → publish its `#EXTINF`.
        if i != last_parent {
            let pd: f64 = list.iter().map(|(_, p)| dur_of(p)).sum();
            s.push_str(&format!("#EXTINF:{pd:.3},\nseg/{msn}.m4s\n"));
        }
    }
    if !sh.parts.is_empty() {
        s.push_str(&format!("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/{next_seq}.m4s\"\n"));
    }
    s
}

/// A running localhost HLS server. Drop it to stop serving (each watch/publish session
/// owns one; without the drop hook every re-watch would leak a thread + port).
pub struct HlsServer {
    addr: SocketAddr,
    sink: HlsSink,
    /// Kept to `unblock()` the accept loop on drop so the reactor thread + port are freed.
    server: Arc<tiny_http::Server>,
}

impl Drop for HlsServer {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

impl HlsServer {
    /// Standard mode: bind an ephemeral localhost port and serve full-GOP segments.
    pub fn start(target_ms: u32) -> std::io::Result<Self> {
        Self::start_inner(target_ms, false, 0)
    }

    /// Low-latency mode: serve LL-HLS with `part_ms` parts rolled into ~`target_ms` parents.
    pub fn start_ll(target_ms: u32, part_ms: u32) -> std::io::Result<Self> {
        Self::start_inner(target_ms, true, part_ms.max(20))
    }

    fn start_inner(target_ms: u32, ll: bool, part_ms: u32) -> std::io::Result<Self> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| std::io::Error::other("no ip addr"))?;
        let sink = HlsSink::new(target_ms, ll, part_ms);
        let server = Arc::new(server);
        let server_cl = server.clone();
        let shared = sink.shared.clone();
        let cv = sink.cv.clone();
        let play_head = sink.play_head.clone();
        thread::spawn(move || {
            for request in server_cl.incoming_requests() {
                // One thread per request: a blocking-reload playlist GET parks on the condvar
                // until its part is ready, and must not stall concurrent part/segment fetches.
                let (shared, cv, play_head) = (shared.clone(), cv.clone(), play_head.clone());
                thread::spawn(move || handle(request, &shared, &cv, &play_head));
            }
        });
        Ok(Self { addr, sink, server })
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

/// `Access-Control-Allow-Origin: *`. hls.js (Android WebView) fetches the playlist +
/// segments via XHR from the app's origin (`http://tauri.localhost`), which is cross-origin
/// to this loopback server — so Chromium enforces CORS and drops responses lacking this
/// header (hls.js reports `manifestLoadError`). Native `<video src>` (desktop) isn't subject
/// to XHR CORS, so this only bites the mobile hls.js path. Allowing any origin is safe: the
/// server binds 127.0.0.1 and serves only this app's own stream — no cross-site data to leak.
fn cors() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..])
        .expect("valid header")
}

fn not_found() -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_string("not found").with_status_code(404)
}

/// Parse `_HLS_msn` / `_HLS_part` from a `live.m3u8` query. `None` → not a blocking request.
fn blocking_target(raw: &str) -> Option<(u64, u64)> {
    let q = raw.split('?').nth(1)?;
    let mut msn = None;
    let mut part = 0u64;
    for kv in q.split('&') {
        match kv.split_once('=') {
            Some(("_HLS_msn", v)) => msn = v.parse().ok(),
            Some(("_HLS_part", v)) => part = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    msn.map(|m| (m, part))
}

fn handle(
    req: tiny_http::Request,
    shared: &Arc<Mutex<Shared>>,
    cv: &Arc<Condvar>,
    play_head: &Arc<AtomicU64>,
) {
    // CORS preflight: hls.js may OPTIONS-probe before a cross-origin segment fetch (e.g. with
    // a Range header). Answer permissively and return — loopback-only, this app's own stream.
    if req.method() == &tiny_http::Method::Options {
        let resp = tiny_http::Response::empty(204)
            .with_header(cors())
            .with_header(
                tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"GET, OPTIONS"[..])
                    .expect("valid header"),
            )
            .with_header(
                tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Range"[..])
                    .expect("valid header"),
            );
        let _ = req.respond(resp);
        return;
    }
    let raw = req.url().to_string();
    let url = raw.split('?').next().unwrap_or(&raw); // path without the query

    // Take only cheap refcounted clones under the lock; the actual body copies and response IO
    // happen after it drops. The node's delivery path contends on this same mutex, so holding
    // it across a `to_vec` of a full segment (or the socket write) would stall ingestion.
    enum Payload {
        Playlist(String),
        Media(Bytes),
        Missing,
    }
    let payload = if url == "/live.m3u8" {
        let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
        // Low-latency blocking reload: hold until the requested (msn, part) is produced, so
        // the player fetches the next part the instant it exists (bounded wait as a backstop).
        if g.ll {
            if let Some((want_msn, want_part)) = blocking_target(&raw) {
                let deadline = Duration::from_millis((g.part_ms as u64 * 6).clamp(1000, 4000));
                let ready = |sh: &Shared| {
                    live_edge(&sh.parts).is_some_and(|(m, p)| m > want_msn || (m == want_msn && p >= want_part))
                };
                while !ready(&g) {
                    let (ng, timeout) = cv
                        .wait_timeout(g, deadline)
                        .unwrap_or_else(|e| e.into_inner());
                    g = ng;
                    if timeout.timed_out() {
                        break;
                    }
                }
            }
        }
        let pl = if g.ll { ll_playlist(&g) } else { standard_playlist(g.target_ms, &g.parts) };
        Payload::Playlist(pl)
    } else if url == "/init.mp4" {
        shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .init
            .clone()
            .map(Payload::Media)
            .unwrap_or(Payload::Missing)
    } else if let Some(seq) = url
        .strip_prefix("/part/")
        .and_then(|s| s.strip_suffix(".m4s"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        // An individual LL part. Its fetch is the play-head signal (part seq == mesh seq).
        let g = shared.lock().unwrap_or_else(|e| e.into_inner());
        match g.parts.get(&seq) {
            Some(p) => {
                play_head.store(seq, Ordering::SeqCst);
                Payload::Media(p.bytes.clone())
            }
            None => Payload::Missing,
        }
    } else if let Some(n) = url
        .strip_prefix("/seg/")
        .and_then(|s| s.strip_suffix(".m4s"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        let g = shared.lock().unwrap_or_else(|e| e.into_inner());
        if g.ll {
            // A completed parent segment (`n` is its media-sequence number): the concatenation
            // of its parts, which is itself a valid CMAF segment. Play-head = its last part.
            let members: Vec<(u64, Bytes)> = g
                .parts
                .iter()
                .filter(|(_, p)| p.parent_msn == n)
                .map(|(s, p)| (*s, p.bytes.clone()))
                .collect();
            if members.is_empty() {
                Payload::Missing
            } else {
                if let Some(max) = members.iter().map(|(s, _)| *s).max() {
                    play_head.store(max, Ordering::SeqCst);
                }
                let mut buf = Vec::with_capacity(members.iter().map(|(_, b)| b.len()).sum());
                for (_, b) in &members {
                    buf.extend_from_slice(b);
                }
                Payload::Media(Bytes::from(buf))
            }
        } else {
            // Standard mode: `n` is the segment seq directly.
            match g.parts.get(&n) {
                Some(p) => {
                    play_head.store(n, Ordering::SeqCst);
                    Payload::Media(p.bytes.clone())
                }
                None => Payload::Missing,
            }
        }
    } else {
        Payload::Missing
    };

    let resp = match payload {
        Payload::Playlist(pl) => tiny_http::Response::from_data(pl.into_bytes())
            .with_header(content_type("application/vnd.apple.mpegurl")),
        Payload::Media(b) => {
            tiny_http::Response::from_data(b.to_vec()).with_header(content_type("video/mp4"))
        }
        Payload::Missing => not_found(),
    };
    let _ = req.respond(resp.with_header(cors()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Build a Shared directly (bypassing the server) for playlist-rendering unit tests.
    fn shared(target_ms: u32, ll: bool, part_ms: u32) -> HlsSink {
        HlsSink::new(target_ms, ll, part_ms)
    }

    #[test]
    fn standard_playlist_lists_segments() {
        let sink = shared(2000, false, 0);
        sink.push_segment(0, Bytes::from_static(b"a"));
        sink.push_segment(1, Bytes::from_static(b"b"));
        let g = sink.shared.lock().unwrap();
        let pl = standard_playlist(g.target_ms, &g.parts);
        assert!(pl.contains("#EXTM3U"));
        assert!(pl.contains("#EXT-X-TARGETDURATION:2"));
        assert!(pl.contains("seg/0.m4s"));
        assert!(pl.contains("seg/1.m4s"));
    }

    fn ll_params() -> segmenter::H264Params {
        segmenter::H264Params { sps: vec![0x67, 0x42, 0x00, 0x0a], pps: vec![0x68, 0xce], width: 320, height: 240 }
    }

    /// Feed one AU into `fb` and forward whatever part it closes into `sink`. Part target is
    /// 9000 ticks and AUs are 4000, so a 6-AU GOP (idr + 5 P) closes exactly two parts.
    fn feed(fb: &mut segmenter::FragmentBuilder, sink: &HlsSink, kf: bool) {
        let idr = [0u8, 0, 0, 1, 0x65, 1, 2, 3, 4];
        let p = [0u8, 0, 0, 1, 0x41, 9, 8, 7];
        let nal: &[u8] = if kf { &idr } else { &p };
        if let Some(s) = fb.push_au(nal, 4000, kf) {
            sink.push_segment(s.seq, s.bytes);
        }
    }

    /// Two full GOPs of two parts each: GOP0's parent is complete (has an `#EXTINF`), GOP1's
    /// parent is still open (parts + preload hint, no `#EXTINF`).
    #[test]
    fn ll_playlist_groups_parts_into_parents() {
        let mut fb = segmenter::FragmentBuilder::new_ll(ll_params(), 100);
        let sink = shared(1000, true, 100);
        // GOP0 (idr + 5 P → parts 0,1 under parent 0), then GOP1 (idr + 5 P → parts 2,3 parent 1).
        feed(&mut fb, &sink, true);
        for _ in 0..5 { feed(&mut fb, &sink, false); }
        feed(&mut fb, &sink, true);
        for _ in 0..5 { feed(&mut fb, &sink, false); }

        let g = sink.shared.lock().unwrap();
        let pl = ll_playlist(&g);
        assert!(pl.contains("#EXT-X-VERSION:9"), "{pl}");
        assert!(pl.contains("CAN-BLOCK-RELOAD=YES"), "{pl}");
        assert!(pl.contains("#EXT-X-PART-INF:PART-TARGET="), "{pl}");
        assert!(pl.contains("#EXT-X-PART:DURATION="), "{pl}");
        assert!(pl.contains("INDEPENDENT=YES"), "leading parts marked independent: {pl}");
        assert!(pl.contains("#EXTINF:"), "completed first parent has an EXTINF: {pl}");
        assert!(pl.contains("seg/0.m4s"), "parent 0's EXTINF URI: {pl}");
        assert!(pl.contains("#EXT-X-PRELOAD-HINT:TYPE=PART"), "{pl}");
        // Media sequence starts at the first parent (0); part URIs are part/<seq>.
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:0"), "{pl}");
        assert!(pl.contains("URI=\"part/0.m4s\""), "{pl}");
        // The last (open) parent must NOT have an EXTINF yet — count them: 1 complete parent.
        assert_eq!(pl.matches("#EXTINF:").count(), 1, "only the completed parent gets EXTINF: {pl}");
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

    /// A blocking-reload request for a not-yet-produced parent returns once it arrives.
    #[test]
    fn blocking_reload_wakes_on_new_part() {
        let mut fb = segmenter::FragmentBuilder::new_ll(ll_params(), 100);
        let srv = HlsServer::start_ll(1000, 100).unwrap();
        let sink = srv.sink();
        // Produce GOP0 fully (parts 0,1 under parent 0) → live edge (msn=0, part=1).
        feed(&mut fb, &sink, true);
        for _ in 0..5 { feed(&mut fb, &sink, false); }
        let addr = srv.addr();

        // Ask for parent 1 (not produced yet) on a background thread; it must park…
        let t = thread::spawn(move || http_get(addr, "/live.m3u8?_HLS_msn=1&_HLS_part=0"));
        // …then produce GOP1's first independent part (idr + 2 P closes part 2 → parent 1).
        feed(&mut fb, &sink, true);
        feed(&mut fb, &sink, false);
        feed(&mut fb, &sink, false);
        let pl = t.join().unwrap();
        // The response reflects the live edge having reached parent 1: parent 0 completed
        // (its EXTINF/URI present) and parent 1's independent part is announced.
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:0"), "playlist returned: {pl}");
        assert!(pl.contains("seg/0.m4s"), "parent 0 completed: {pl}");
        assert!(pl.contains("URI=\"part/2.m4s\""), "parent 1's first part announced: {pl}");
    }
}
