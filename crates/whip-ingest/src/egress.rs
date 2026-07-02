//! WebRTC media egress — the opt-in, unverified, sub-second fast tier (W3).
//!
//! A fast-tier viewer's browser creates an `RTCPeerConnection` (recvonly video) and sends
//! an SDP **offer** over the mesh's session signaling. The publisher answers here: a
//! libdatachannel PC with a **sendonly H.264 track** whose payload type matches the offer,
//! then writes the SAME access units the mesh muxer gets — packetized to RTP
//! ([`crate::rtp::H264Packetizer`]) — straight onto the track. The browser hardware-decodes
//! them with no HLS, no segmentation, no mesh buffering: sub-second, publisher-direct.
//!
//! This is a genuinely different transport from the verified mesh. It carries no hashes and
//! no signatures — a fast-tier viewer accepts unverified bytes the way it accepts a video
//! call. The mesh tier is untouched and stays warm underneath as the fallback.
//!
//! WHEP-shaped (the viewer offers, the publisher answers) but the offer/answer + ICE ride
//! the statement-store signaling, not HTTP. Non-trickle: we wait (bounded) for ICE gathering
//! so the answer carries all candidates, matching the WHIP ingest path.

use crate::rtp::{H264Packetizer, DEFAULT_MTU};
use crate::server::{rtc_config, video_mline};
use datachannel::{
    ConnectionState, DataChannelHandler, DataChannelInfo, GatheringState, PeerConnectionHandler,
    RtcPeerConnection, RtcTrack, SessionDescription, TrackHandler, TrackInit,
};
use std::ffi::CString;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A fixed SSRC for the publisher's fast-tier video track. Publisher-direct, one track per
/// viewer connection, so a constant is fine; it's stamped into both the track and the RTP.
const EGRESS_SSRC: u32 = 0xF00D_CAFE;

/// The publisher side of one fast-tier connection: a sendonly H.264 track to a single viewer.
/// Hold it for the connection's life; drop it to tear the connection down.
pub struct MediaEgress {
    // Field order matters for drop: the track is torn down before the PC that owns it.
    track: Box<RtcTrack<SendTrack>>,
    _pc: Box<RtcPeerConnection<EgressConn>>,
    packetizer: H264Packetizer,
    answer_sdp: String,
    connected: Arc<Mutex<bool>>,
}

impl MediaEgress {
    /// Answer a viewer's SDP `offer` with a sendonly H.264 track. `stun` mirrors the mesh
    /// transport's ICE config. Returns once ICE gathering finishes (bounded), so
    /// [`answer_sdp`](Self::answer_sdp) carries the publisher's candidates.
    pub fn answer(offer: &str, stun: &[String]) -> Result<Self, String> {
        // Match the offer's H.264 video m-line — the answer track's mid + payload type must
        // line up with it or libdatachannel won't bind RTP to the section.
        let (mid, pt) = video_mline(offer)
            .ok_or_else(|| "fast-tier offer has no H.264 video m-line".to_string())?;
        let offer_desc: SessionDescription =
            serde_json::from_value(serde_json::json!({ "type": "offer", "sdp": offer }))
                .map_err(|e| format!("parse offer: {e}"))?;

        let (gathered_tx, gathered_rx) = sync_channel::<()>(1);
        let connected = Arc::new(Mutex::new(false));
        let conf = rtc_config(stun);
        let mut pc = RtcPeerConnection::new(
            &conf,
            EgressConn { gathered: gathered_tx, connected: connected.clone() },
        )
        .map_err(|e| format!("pc create: {e}"))?;

        // cname/msid/track-id are REQUIRED for browser interop: without them libdatachannel
        // emits a bare `a=ssrc:<id>` line (no `cname:`), which Chrome's SDP parser rejects —
        // setRemoteDescription throws and the viewer falls back to the mesh. (RFC 5576 §4.1:
        // the ssrc attribute takes `attribute:value`.) ffmpeg and libdatachannel itself are
        // lenient, which is why the WHIP ingest and the loopback test never caught it.
        let track_init = TrackInit {
            direction: datachannel::Direction::SendOnly,
            codec: datachannel::Codec::H264,
            payload_type: pt,
            ssrc: EGRESS_SSRC,
            mid: CString::new(mid).map_err(|e| format!("mid: {e}"))?,
            name: Some(CString::new("unstation-video").expect("static cname")),
            msid: Some(CString::new("unstation").expect("static msid")),
            track_id: Some(CString::new("unstation-video-0").expect("static track id")),
            profile: None,
        };
        let track = pc
            .add_track_ex(&track_init, SendTrack)
            .map_err(|e| format!("add_track: {e}"))?;

        log::debug!("[egress] OFFER from viewer:\n{offer}");
        // Setting the remote offer makes libdatachannel generate the answer + start ICE.
        pc.set_remote_description(&offer_desc).map_err(|e| format!("set offer: {e}"))?;
        let _ = gathered_rx.recv_timeout(Duration::from_secs(3));
        let answer = pc
            .local_description()
            .ok_or_else(|| "no local answer generated".to_string())?;
        log::debug!("[egress] ANSWER generated:\n{}", answer.sdp);

        Ok(Self {
            track,
            _pc: pc,
            // The RTP the packetizer stamps must carry the negotiated PT + the track SSRC.
            packetizer: H264Packetizer::new(EGRESS_SSRC, pt as u8),
            answer_sdp: answer.sdp.to_string(),
            connected,
        })
    }

    /// The SDP answer to hand back to the viewer over signaling.
    pub fn answer_sdp(&self) -> &str {
        &self.answer_sdp
    }

    /// True once the DTLS/ICE connection to the viewer is up (media can flow).
    pub fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Packetize one Annex-B access unit at 90kHz `ts` and send it on the track. Best-effort:
    /// a send error (viewer gone, congestion) is dropped — the mesh tier is the safety net.
    pub fn write_au(&mut self, au: &[u8], ts_90k: u32) {
        for pkt in self.packetizer.packetize(au, ts_90k, DEFAULT_MTU) {
            let _ = self.track.send(&pkt);
        }
    }
}

/// Sendonly track: it never receives, so the handler is a no-op.
struct SendTrack;
impl TrackHandler for SendTrack {}

/// Per-connection handler. Fires `gathered` when ICE gathering completes (so the answer is
/// final) and tracks the connected state for `is_connected`.
struct EgressConn {
    gathered: std::sync::mpsc::SyncSender<()>,
    connected: Arc<Mutex<bool>>,
}
struct NullDc;
impl DataChannelHandler for NullDc {}
impl PeerConnectionHandler for EgressConn {
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
        *self.connected.lock().unwrap_or_else(|e| e.into_inner()) =
            matches!(state, ConnectionState::Connected);
        log::info!("[egress] connection state → {state:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{AccessUnit, H264Depacketizer};
    use datachannel::{Direction, RtcPeerConnection, SdpType};
    use std::sync::mpsc::{sync_channel, SyncSender};

    /// Loopback interop spike: a libdatachannel subscriber (offerer, recvonly — standing in
    /// for the browser) negotiates with [`MediaEgress`] (answerer, sendonly) purely in-process,
    /// and we assert access units the publisher writes arrive + depacketize on the subscriber.
    ///
    /// This validates the media-track API end to end — sendonly negotiation, the reversed
    /// offer/answer roles, RTP over SRTP, and packetize→send→receive→depacketize — short of a
    /// real browser (the remaining SDP/codec-param interop unknown). Ignored: it needs the
    /// media-enabled libdatachannel and runs a full ICE/DTLS/SRTP session.
    /// A canned keyframe access unit (SPS + PPS + IDR).
    fn idr_au() -> Vec<u8> {
        [
            &[0u8, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a][..], // SPS
            &[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80],       // PPS
            &[0, 0, 0, 1, 0x65, 1, 2, 3, 4, 5, 6, 7, 8], // IDR slice
        ]
        .concat()
    }

    /// Set up the loopback pair: a libdatachannel subscriber (offerer, recvonly — standing in
    /// for the browser) negotiated against [`MediaEgress`] (answerer, sendonly), connected over
    /// real ICE/DTLS/SRTP on loopback. Returns the connected egress, the subscriber's decoded
    /// AU stream, and the subscriber PC (keep it alive for the session's duration).
    fn connected_loopback() -> (
        MediaEgress,
        std::sync::mpsc::Receiver<AccessUnit>,
        Box<RtcPeerConnection<SubConn>>,
    ) {
        // libjuice drops 127.0.0.1 host candidates unless we bind explicitly (RFC 8445).
        std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");
        let (au_tx, au_rx) = sync_channel::<AccessUnit>(64);
        let (sub_gathered_tx, sub_gathered_rx) = sync_channel::<()>(1);
        let sub_connected = Arc::new(Mutex::new(false));
        let mut sub = RtcPeerConnection::new(
            &rtc_config(&[]),
            SubConn { gathered: sub_gathered_tx, connected: sub_connected.clone() },
        )
        .expect("sub pc");
        let sub_track_init = TrackInit {
            direction: Direction::RecvOnly,
            codec: datachannel::Codec::H264,
            payload_type: 96,
            ssrc: 1,
            mid: CString::new("0").unwrap(),
            name: None,
            msid: None,
            track_id: None,
            profile: None,
        };
        let sub_track = sub
            .add_track_ex(&sub_track_init, RecvTrack { depack: H264Depacketizer::new(), out: au_tx })
            .expect("sub add_track");
        std::mem::forget(sub_track);

        sub.set_local_description(SdpType::Offer).expect("sub set_local offer");
        let _ = sub_gathered_rx.recv_timeout(Duration::from_secs(3));
        let offer = sub.local_description().expect("sub offer").sdp.to_string();

        let egress = MediaEgress::answer(&offer, &[]).expect("egress answer");
        let answer: SessionDescription =
            serde_json::from_value(serde_json::json!({ "type": "answer", "sdp": egress.answer_sdp() }))
                .unwrap();
        sub.set_remote_description(&answer).expect("sub set answer");

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if egress.is_connected() && *sub_connected.lock().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(egress.is_connected(), "publisher side never connected");
        (egress, au_rx, sub)
    }

    #[test]
    #[ignore = "media libdatachannel + live ICE/DTLS/SRTP loopback; run explicitly"]
    fn libdatachannel_media_loopback_delivers_access_units() {
        let (mut egress, au_rx, _sub) = connected_loopback();
        // Publisher writes a keyframe AU repeatedly (a real feed keeps sending); the
        // subscriber should depacketize at least one.
        let idr = idr_au();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let got = loop {
            if std::time::Instant::now() >= deadline {
                break None;
            }
            egress.write_au(&idr, 90_000);
            if let Ok(au) = au_rx.recv_timeout(Duration::from_millis(200)) {
                break Some(au);
            }
        };
        let au = got.expect("subscriber received no access unit within the deadline");
        assert!(au.keyframe, "the delivered AU is the keyframe we sent");
        assert_eq!(au.data, idr, "delivered bytes match what the publisher packetized");
    }

    /// A consent-answering peer must hold the session PAST libjuice's 30s consent window
    /// (RFC 7675). This is the compliant-peer counterpart to the ffmpeg soak in
    /// tests/ffmpeg_whip.rs (known-fail: ffmpeg ≤ 8.1 never answers mid-session consent
    /// checks, so juice expires it at exactly 30s). Media sessions with real WebRTC peers
    /// — OBS-WHIP, browsers — behave like this test.
    #[test]
    #[ignore = "40s media soak across the ICE consent window; run explicitly"]
    fn media_session_with_a_compliant_peer_survives_the_consent_window() {
        let (mut egress, au_rx, _sub) = connected_loopback();
        let idr = idr_au();
        let start = std::time::Instant::now();
        let mut ts: u32 = 0;
        let mut last_rx = Duration::ZERO;
        while start.elapsed() < Duration::from_secs(40) {
            egress.write_au(&idr, ts);
            ts = ts.wrapping_add(90_000); // 1s cadence
            if au_rx.recv_timeout(Duration::from_millis(1000)).is_ok() {
                last_rx = start.elapsed();
            }
        }
        assert!(egress.is_connected(), "session dropped inside 40s (consent should be honored)");
        assert!(
            last_rx >= Duration::from_secs(35),
            "media stopped flowing at {last_rx:?} — consent/keepalive regression"
        );
    }

    struct RecvTrack {
        depack: H264Depacketizer,
        out: SyncSender<AccessUnit>,
    }
    impl TrackHandler for RecvTrack {
        fn on_message(&mut self, msg: &[u8]) {
            if let Some(au) = self.depack.push(msg) {
                let _ = self.out.try_send(au);
            }
        }
    }
    struct SubConn {
        gathered: SyncSender<()>,
        connected: Arc<Mutex<bool>>,
    }
    struct SubNullDc;
    impl DataChannelHandler for SubNullDc {}
    impl PeerConnectionHandler for SubConn {
        type DCH = SubNullDc;
        fn data_channel_handler(&mut self, _: DataChannelInfo) -> SubNullDc {
            SubNullDc
        }
        fn on_gathering_state_change(&mut self, state: GatheringState) {
            if matches!(state, GatheringState::Complete) {
                let _ = self.gathered.try_send(());
            }
        }
        fn on_connection_state_change(&mut self, state: ConnectionState) {
            *self.connected.lock().unwrap_or_else(|e| e.into_inner()) =
                matches!(state, ConnectionState::Connected);
        }
    }
}
