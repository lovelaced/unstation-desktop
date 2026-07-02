//! The WHIP HTTP endpoint + libdatachannel media transport (RFC 9725).
//!
//! Flow: OBS POSTs an SDP offer to `/whip` → we add a **recvonly H.264 track whose
//! `mid` matches the offer's video m-line** (datachannel-rs 0.16 has no incoming-track
//! callback; the answerer declares the track it will receive), set the remote offer,
//! wait for ICE gathering to complete, and answer `201 Created` with our SDP (the WHIP
//! resource is named by the `Location` header) → OBS streams RTP → the track handler
//! depacketizes each packet ([`crate::rtp`]) and forwards access units. DELETE (or
//! dropping [`WhipServer`]) tears the session down.
//!
//! Localhost-only, one publish session at a time (the app opens it on Go Live).
//!
//! ## Status (TECH_SPEC D7 foundation)
//! The depacketizer is production-grade + unit-tested; this negotiation path compiles
//! against the media-enabled libdatachannel and implements the standard answerer
//! handshake, but the exact `mid`/payload-type matching and gathering-timeout have
//! only been validated at the type level — they need a pass against real OBS WHIP
//! output (no headless WHIP client here). See `docs/WHIP.md`.

use crate::rtp::{AccessUnit, H264Depacketizer};
use datachannel::{
    ConnectionState, DataChannelHandler, DataChannelInfo, GatheringState, PeerConnectionHandler,
    RtcConfig, RtcPeerConnection, SessionDescription, TrackHandler, TrackInit,
};
use std::ffi::CString;
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// An access unit plus the codec config current when it was produced. The first unit
/// after an SPS/PPS change carries `Some(config)`; the muxer configures once.
pub struct IngestAu {
    pub au: AccessUnit,
    pub config: Option<(Vec<u8>, Vec<u8>)>,
}

/// A running WHIP endpoint. Drop it to stop the HTTP server + tear down the session.
pub struct WhipServer {
    addr: std::net::SocketAddr,
    _reactor: thread::JoinHandle<()>,
}

impl WhipServer {
    /// The WHIP endpoint URL to paste into OBS (`Service: WHIP`, no bearer token —
    /// it's loopback and this app's own ingest).
    pub fn url(&self) -> String {
        format!("http://{}/whip", self.addr)
    }
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }
}

/// The per-track handler: depacketizes RTP and forwards access units. Lives on the
/// libdatachannel callback thread; the depacketizer is single-threaded so it's owned
/// here directly.
struct Track {
    depack: H264Depacketizer,
    out: Sender<IngestAu>,
}
impl TrackHandler for Track {
    fn on_message(&mut self, msg: &[u8]) {
        if let Some(au) = self.depack.push(msg) {
            let config = self.depack.take_config();
            let _ = self.out.send(IngestAu { au, config });
        }
    }
}

/// Per-connection handler. WHIP is media-only (no data channels). The answer SDP is
/// captured when ICE gathering completes and handed to the waiting HTTP responder.
struct Conn {
    /// Fires once with the connection id so the responder can read the final
    /// (candidate-complete) local description; `None` after it has fired.
    gathered: SyncSender<()>,
}
struct NullDc;
impl DataChannelHandler for NullDc {}
impl PeerConnectionHandler for Conn {
    type DCH = NullDc;
    fn data_channel_handler(&mut self, _: DataChannelInfo) -> NullDc {
        NullDc
    }
    fn on_gathering_state_change(&mut self, state: GatheringState) {
        if matches!(state, GatheringState::Complete) {
            let _ = self.gathered.try_send(());
        }
    }
    fn on_connection_state_change(&mut self, state: ConnectionState) {
        log::info!("[whip] connection state → {state:?}");
    }
}

/// Start a WHIP endpoint on an ephemeral localhost port. Access units (with codec
/// config on change) arrive on `out`. `stun` mirrors the mesh transport's ICE config.
pub fn start(out: Sender<IngestAu>, stun: Vec<String>) -> std::io::Result<WhipServer> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| std::io::Error::other("no ip addr"))?;

    // The active PeerConnection is held for the endpoint's lifetime; dropping it (a new
    // offer, DELETE, or the endpoint closing) tears the session down. One at a time.
    let pc_slot: Arc<Mutex<Option<Box<RtcPeerConnection<Conn>>>>> = Arc::new(Mutex::new(None));

    let reactor = thread::Builder::new().name("whip-http".into()).spawn(move || {
        for req in server.incoming_requests() {
            match (req.method(), req.url()) {
                (tiny_http::Method::Post, "/whip") => handle_offer(req, &out, &stun, &pc_slot),
                (tiny_http::Method::Delete, _) => {
                    *pc_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let _ = req.respond(tiny_http::Response::empty(200));
                }
                (tiny_http::Method::Options, _) => {
                    let _ = req.respond(tiny_http::Response::empty(204));
                }
                _ => {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    })?;
    Ok(WhipServer { addr, _reactor: reactor })
}

fn handle_offer(
    mut req: tiny_http::Request,
    out: &Sender<IngestAu>,
    stun: &[String],
    pc_slot: &Arc<Mutex<Option<Box<RtcPeerConnection<Conn>>>>>,
) {
    let mut offer_sdp = String::new();
    if req.as_reader().read_to_string(&mut offer_sdp).is_err() || offer_sdp.is_empty() {
        let _ = req.respond(tiny_http::Response::from_string("bad offer").with_status_code(400));
        return;
    }
    match negotiate(&offer_sdp, out.clone(), stun) {
        Ok((pc, answer_sdp)) => {
            *pc_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(pc);
            let resp = tiny_http::Response::from_string(answer_sdp)
                .with_status_code(201)
                .with_header(header("Content-Type", "application/sdp"))
                .with_header(header("Location", "/whip/session"));
            let _ = req.respond(resp);
        }
        Err(e) => {
            log::warn!("[whip] negotiation failed: {e}");
            let _ = req.respond(
                tiny_http::Response::from_string(format!("whip error: {e}")).with_status_code(500),
            );
        }
    }
}

/// Build a recvonly H.264 transport matching the offer's video m-line, apply the
/// offer, wait for ICE gathering, and return our answer SDP.
fn negotiate(
    offer_sdp: &str,
    out: Sender<IngestAu>,
    stun: &[String],
) -> Result<(Box<RtcPeerConnection<Conn>>, String), String> {
    let (mid, pt) = video_mline(offer_sdp)
        .ok_or_else(|| "offer has no H.264 video m-line".to_string())?;

    // The wire form the safe wrapper (de)serializes is JSON `{type, sdp}`.
    let offer: SessionDescription =
        serde_json::from_value(serde_json::json!({ "type": "offer", "sdp": offer_sdp }))
            .map_err(|e| format!("parse offer: {e}"))?;

    let (gathered_tx, gathered_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let conf = RtcConfig::new(stun);
    let mut pc = RtcPeerConnection::new(&conf, Conn { gathered: gathered_tx })
        .map_err(|e| format!("pc create: {e}"))?;

    // Declare the recvonly track we'll receive — its mid MUST match the offer's video
    // section, or libdatachannel won't bind the incoming RTP to it.
    let track_init = TrackInit {
        direction: datachannel::Direction::RecvOnly,
        codec: datachannel::Codec::H264,
        payload_type: pt,
        ssrc: 1,
        mid: CString::new(mid).map_err(|e| format!("mid: {e}"))?,
        name: None,
        msid: None,
        track_id: None,
        profile: None,
    };
    let track = pc
        .add_track_ex(&track_init, Track { depack: H264Depacketizer::new(), out })
        .map_err(|e| format!("add_track: {e}"))?;
    // The track lives as long as the PC (which we return + hold); leak the box so the
    // handler keeps receiving without us threading it through the endpoint state.
    std::mem::forget(track);

    // Setting the remote offer makes libdatachannel generate the answer + start ICE.
    pc.set_remote_description(&offer).map_err(|e| format!("set offer: {e}"))?;

    // WHIP is non-trickle: wait (bounded) for gathering to finish so the answer carries
    // its host candidates. On localhost/LAN this is near-instant.
    let _ = gathered_rx.recv_timeout(Duration::from_secs(3));
    let answer = pc
        .local_description()
        .ok_or_else(|| "no local answer generated".to_string())?;
    Ok((pc, answer.sdp.to_string()))
}

/// Extract `(mid, payload_type)` of the offer's H.264 video m-line by scanning the raw
/// SDP: find the `m=video` section, its `a=mid:`, and the payload type of the
/// `a=rtpmap:<pt> H264/90000` line. Text-scanning keeps this independent of the SDP
/// object model.
fn video_mline(sdp: &str) -> Option<(String, i32)> {
    let mut in_video = false;
    let mut mid: Option<String> = None;
    let mut pt: Option<i32> = None;
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            in_video = rest.starts_with("video");
            if in_video {
                mid = None;
                pt = None;
            }
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(m) = line.strip_prefix("a=mid:") {
            mid = Some(m.trim().to_string());
        } else if let Some(r) = line.strip_prefix("a=rtpmap:") {
            // "<pt> H264/90000"
            if let Some((p, codec)) = r.split_once(' ') {
                if codec.to_ascii_uppercase().starts_with("H264") {
                    if let Ok(n) = p.trim().parse::<i32>() {
                        pt = Some(n);
                    }
                }
            }
        }
        if let (Some(m), Some(p)) = (&mid, pt) {
            return Some((m.clone(), p));
        }
    }
    None
}

fn header(k: &str, v: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

#[cfg(test)]
mod tests {
    use super::video_mline;

    #[test]
    fn parses_the_h264_video_mline() {
        let sdp = "v=0\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=mid:0\r\n\
                   a=rtpmap:111 opus/48000/2\r\n\
                   m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
                   a=mid:1\r\n\
                   a=rtpmap:96 H264/90000\r\n\
                   a=rtpmap:97 rtx/90000\r\n";
        assert_eq!(video_mline(sdp), Some(("1".to_string(), 96)));
    }

    #[test]
    fn none_without_an_h264_video_section() {
        let sdp = "v=0\r\nm=audio 9 RTP 111\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\n";
        assert_eq!(video_mline(sdp), None);
    }
}
