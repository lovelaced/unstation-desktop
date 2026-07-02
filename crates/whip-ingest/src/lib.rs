//! WHIP ingest (RFC 9725): OBS 30+ → WebRTC RTP → H.264 access units, the
//! sub-2-second contribution path (TECH_SPEC D7). The publisher's alternative to
//! the RTMP→ffmpeg→CMAF ingest: OBS POSTs an SDP offer, sends media as RTP, and
//! this crate depacketizes it and feeds the same `FragmentBuilder` muxer.
//!
//! - [`rtp`] — pure H.264 RTP depacketization (RFC 6184). Always builds; unit-tested.
//! - [`server`] — the WHIP HTTP endpoint + libdatachannel media transport. Behind the
//!   `server` feature (a media-enabled libdatachannel rebuild), so the fast engine
//!   test path never pays for it.

pub mod rtp;

#[cfg(feature = "server")]
pub mod server;

pub use rtp::{AccessUnit, H264Depacketizer, H264Packetizer, DEFAULT_MTU};
