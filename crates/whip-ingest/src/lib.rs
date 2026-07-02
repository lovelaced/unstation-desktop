//! WHIP ingest (RFC 9725): OBS 30+ → WebRTC RTP → H.264 access units, the
//! sub-2-second contribution path (TECH_SPEC D7). The publisher's alternative to
//! the RTMP→ffmpeg→CMAF ingest: OBS POSTs an SDP offer, sends media as RTP, and
//! this crate depacketizes it and feeds the same `FragmentBuilder` muxer.
//!
//! - [`rtp`] — pure H.264 RTP depacketization + packetization (RFC 6184). Always builds;
//!   unit-tested (the packetizer round-trips through the depacketizer).
//! - [`server`] — the WHIP HTTP endpoint (ingest) + libdatachannel media transport.
//! - [`egress`] — the WebRTC media fast tier (W3): a sendonly H.264 track straight to a
//!   browser viewer, sub-second and publisher-direct.
//!
//! `server` + `egress` are behind the `server` feature (a media-enabled libdatachannel
//! rebuild), so the fast engine test path never pays for it.

pub mod rtp;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "server")]
pub mod egress;

pub use rtp::{AccessUnit, H264Depacketizer, H264Packetizer, DEFAULT_MTU};

#[cfg(feature = "server")]
pub use egress::MediaEgress;
