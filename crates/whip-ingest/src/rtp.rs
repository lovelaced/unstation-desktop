//! H.264 RTP depacketization (RFC 6184) — RTP packets in, Annex-B access units out.
//!
//! Supports the payload shapes real encoders emit: single NAL units (types 1–23),
//! STAP-A aggregation (24), and FU-A fragmentation (28). Anything else — and any
//! sequence-number gap — poisons the current access unit and playback resumes at
//! the next keyframe (partial H.264 is worse than a skipped frame). Pure and
//! allocation-conscious; the WHIP track handler drives it packet by packet.

/// One decoded access unit, Annex-B framed (`00 00 00 01` before every NAL).
#[derive(Debug)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    /// Presentation time in µs from the first packet's RTP timestamp (90 kHz),
    /// wrap-safe (u32 timestamps unwrapped into a monotonic u64 tick count).
    pub pts_us: i64,
    pub keyframe: bool,
}

pub struct H264Depacketizer {
    /// NALs (each Annex-B prefixed) collected for the in-progress access unit.
    au: Vec<u8>,
    au_has_idr: bool,
    /// FU-A reassembly buffer (one fragmented NAL at a time).
    fu: Vec<u8>,
    last_seq: Option<u16>,
    /// Wrap-unwrapped RTP timestamp state.
    last_ts: Option<u32>,
    ts_unwrapped: u64,
    base_ts: Option<u64>,
    /// A gap or unsupported payload poisoned the stream — discard until an IDR.
    wait_keyframe: bool,
    /// Latest SPS (7) / PPS (8) seen, for the muxer's codec config.
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    sps_pps_updated: bool,
}

const ANNEX_B: &[u8] = &[0, 0, 0, 1];
/// Reject absurd packets before allocating (jumbo frames top out well below this).
const MAX_RTP_LEN: usize = 64 * 1024;
/// Cap a runaway access unit (a poisoned stream must not grow memory unboundedly).
const MAX_AU_BYTES: usize = 4 * 1024 * 1024;

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            au: Vec::new(),
            au_has_idr: false,
            fu: Vec::new(),
            last_seq: None,
            last_ts: None,
            ts_unwrapped: 0,
            base_ts: None,
            wait_keyframe: true, // never start mid-GOP
            sps: None,
            pps: None,
            sps_pps_updated: false,
        }
    }

    /// The latest codec config, once both SPS and PPS have been seen. Returns it
    /// only when new/changed since the last call (the muxer configures once).
    pub fn take_config(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        if !self.sps_pps_updated {
            return None;
        }
        match (&self.sps, &self.pps) {
            (Some(s), Some(p)) => {
                self.sps_pps_updated = false;
                Some((s.clone(), p.clone()))
            }
            _ => None,
        }
    }

    /// Feed one RTP packet; returns a completed access unit when the packet closes
    /// one (RTP marker bit). Malformed input is dropped, never fatal.
    pub fn push(&mut self, pkt: &[u8]) -> Option<AccessUnit> {
        if pkt.len() < 12 || pkt.len() > MAX_RTP_LEN || (pkt[0] >> 6) != 2 {
            return None; // not RTP v2 / absurd size
        }
        let has_padding = pkt[0] & 0x20 != 0;
        let has_ext = pkt[0] & 0x10 != 0;
        let csrc_count = (pkt[0] & 0x0F) as usize;
        let marker = pkt[1] & 0x80 != 0;
        let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
        let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);

        let mut off = 12 + csrc_count * 4;
        if has_ext {
            if pkt.len() < off + 4 {
                return None;
            }
            let ext_words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
            off += 4 + ext_words * 4;
        }
        let mut end = pkt.len();
        if has_padding {
            let pad = *pkt.last()? as usize;
            if pad == 0 || pad > end.saturating_sub(off) {
                return None;
            }
            end -= pad;
        }
        if off >= end {
            return None;
        }
        let payload = &pkt[off..end];

        // Sequence continuity: a lost packet invalidates the in-progress AU (and any
        // FU-A in flight) — resume at the next keyframe.
        if let Some(last) = self.last_seq {
            if seq != last.wrapping_add(1) {
                self.poison();
            }
        }
        self.last_seq = Some(seq);

        // Unwrap the 90 kHz timestamp into a monotonic tick count.
        if let Some(last) = self.last_ts {
            let delta = ts.wrapping_sub(last);
            // Deltas ≥ 2^31 mean the clock went "backwards" (reordering across a
            // wrap); treat as zero advance rather than a huge jump.
            if delta < 1 << 31 {
                self.ts_unwrapped += delta as u64;
            }
        }
        self.last_ts = Some(ts);
        if self.base_ts.is_none() {
            self.base_ts = Some(self.ts_unwrapped);
        }

        let nal_type = payload[0] & 0x1F;
        match nal_type {
            1..=23 => self.take_nal(payload),
            // STAP-A: [u16 len ‖ NAL]* aggregated in one packet.
            24 => {
                let mut p = &payload[1..];
                while p.len() >= 2 {
                    let len = u16::from_be_bytes([p[0], p[1]]) as usize;
                    p = &p[2..];
                    if len == 0 || p.len() < len {
                        self.poison();
                        break;
                    }
                    self.take_nal(&p[..len]);
                    p = &p[len..];
                }
            }
            // FU-A: one NAL fragmented across packets.
            28 => {
                if payload.len() < 2 {
                    self.poison();
                } else {
                    let indicator = payload[0];
                    let fu = payload[1];
                    let start = fu & 0x80 != 0;
                    let fend = fu & 0x40 != 0;
                    if start {
                        self.fu.clear();
                        // Reconstruct the original NAL header from the FU pieces.
                        self.fu.push((indicator & 0xE0) | (fu & 0x1F));
                    } else if self.fu.is_empty() {
                        self.poison(); // middle/end with no start — a gap ate it
                    }
                    if !self.fu.is_empty() {
                        if self.fu.len() + payload.len() > MAX_AU_BYTES {
                            self.poison();
                        } else {
                            self.fu.extend_from_slice(&payload[2..]);
                            if fend {
                                let nal = std::mem::take(&mut self.fu);
                                self.take_nal(&nal);
                            }
                        }
                    }
                }
            }
            _ => self.poison(), // FU-B / MTAP / reserved — not emitted by real encoders
        }

        if !marker {
            return None;
        }
        // Marker: the access unit is complete.
        let data = std::mem::take(&mut self.au);
        let keyframe = self.au_has_idr;
        self.au_has_idr = false;
        if data.is_empty() {
            return None;
        }
        if self.wait_keyframe {
            if !keyframe {
                return None; // still discarding up to the next IDR
            }
            self.wait_keyframe = false;
        }
        let pts_ticks = self.ts_unwrapped - self.base_ts.unwrap_or(0);
        Some(AccessUnit {
            data,
            // 90 kHz ticks → µs (multiply first: sub-ms precision, no overflow at
            // any realistic stream length inside i64).
            pts_us: (pts_ticks as i64) * 1000 / 90,
            keyframe,
        })
    }

    fn take_nal(&mut self, nal: &[u8]) {
        if nal.is_empty() || self.au.len() + nal.len() + 4 > MAX_AU_BYTES {
            self.poison();
            return;
        }
        match nal[0] & 0x1F {
            5 => self.au_has_idr = true,
            7 => {
                if self.sps.as_deref() != Some(nal) {
                    self.sps = Some(nal.to_vec());
                    self.sps_pps_updated = true;
                }
            }
            8 => {
                if self.pps.as_deref() != Some(nal) {
                    self.pps = Some(nal.to_vec());
                    self.sps_pps_updated = true;
                }
            }
            _ => {}
        }
        self.au.extend_from_slice(ANNEX_B);
        self.au.extend_from_slice(nal);
    }

    fn poison(&mut self) {
        self.au.clear();
        self.fu.clear();
        self.au_has_idr = false;
        self.wait_keyframe = true;
    }
}

/// H.264 RTP packetization (RFC 6184) — the inverse of [`H264Depacketizer`]. Turns one
/// Annex-B access unit into RTP packets for the WebRTC media fast tier (W3): the publisher
/// writes these onto a sendonly track that browser viewers hardware-decode directly. Emits
/// single-NAL packets, FU-A fragmenting any NAL that won't fit the MTU. The marker bit is set
/// on the last packet of the access unit (the depacketizer keys AU completion off it).
pub struct H264Packetizer {
    ssrc: u32,
    payload_type: u8,
    seq: u16,
}

/// Default RTP packet ceiling: keeps the whole packet (incl. IP/UDP/SRTP overhead) under a
/// 1500-byte Ethernet MTU, so DTLS/SRTP never has to IP-fragment.
pub const DEFAULT_MTU: usize = 1200;
/// RTP header we emit: 12 bytes, no CSRC/extension/padding.
const RTP_HEADER: usize = 12;

impl H264Packetizer {
    /// `payload_type` must be the dynamic PT negotiated for H.264 in the SDP answer;
    /// `ssrc` identifies this track's stream. `seq` starts at 0 and rolls over naturally.
    pub fn new(ssrc: u32, payload_type: u8) -> Self {
        Self { ssrc, payload_type, seq: 0 }
    }

    /// Packetize one Annex-B access unit at 90kHz `ts`, bounding each packet to `mtu` bytes.
    /// Returns the packets in send order; the last carries the marker bit. An access unit
    /// with no NALs yields no packets.
    pub fn packetize(&mut self, au: &[u8], ts: u32, mtu: usize) -> Vec<Vec<u8>> {
        let nals = iter_annexb_nals(au);
        if nals.is_empty() {
            return Vec::new();
        }
        // Room for a NAL (or FU-A payload chunk) after the RTP header.
        let max_payload = mtu.saturating_sub(RTP_HEADER).max(2);
        let mut out: Vec<Vec<u8>> = Vec::new();
        for nal in &nals {
            if nal.is_empty() {
                continue;
            }
            if nal.len() <= max_payload {
                out.push(self.rtp_packet(false, ts, nal)); // single NAL unit
            } else {
                // FU-A: fragment the NAL body (after its 1-byte header) into chunks that
                // leave 2 bytes for the FU indicator + FU header.
                let hdr = nal[0];
                let indicator = (hdr & 0xE0) | 28; // keep F/NRI, type 28
                let body = &nal[1..];
                let chunk = max_payload.saturating_sub(2).max(1);
                let mut i = 0;
                while i < body.len() {
                    let end = (i + chunk).min(body.len());
                    let start_bit = (i == 0) as u8;
                    let end_bit = (end == body.len()) as u8;
                    let fu_header = (start_bit << 7) | (end_bit << 6) | (hdr & 0x1F);
                    let mut payload = Vec::with_capacity(2 + (end - i));
                    payload.push(indicator);
                    payload.push(fu_header);
                    payload.extend_from_slice(&body[i..end]);
                    out.push(self.rtp_packet(false, ts, &payload));
                    i = end;
                }
            }
        }
        // Mark the final packet as the access unit's end.
        if let Some(last) = out.last_mut() {
            last[1] |= 0x80;
        }
        out
    }

    fn rtp_packet(&mut self, marker: bool, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(RTP_HEADER + payload.len());
        p.push(0x80); // v2, no padding/extension/CSRC
        p.push((self.payload_type & 0x7F) | if marker { 0x80 } else { 0 });
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        p
    }
}

/// Split an Annex-B buffer into NAL units (payload between start codes, start code removed).
/// Mirrors what the depacketizer emits: a `00 00 00 01` (or `00 00 01`) prefix per NAL.
fn iter_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (idx, &payload_start) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() {
            let next = starts[idx + 1] - 3; // back up over that start code
            if next > 0 && data[next - 1] == 0 { next - 1 } else { next } // trim 4-byte code's extra 0
        } else {
            data.len()
        };
        if payload_start < end {
            nals.push(&data[payload_start..end]);
        }
    }
    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, if marker { 0x80 } else { 0x00 }];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&0xAABBCCDDu32.to_be_bytes()); // SSRC
        p.extend_from_slice(payload);
        p
    }

    /// A tiny IDR AU: SPS + PPS + IDR slice, delivered as single-NAL packets.
    fn idr_packets(seq0: u16, ts: u32) -> Vec<Vec<u8>> {
        vec![
            rtp(seq0, ts, false, &[0x67, 1, 2, 3]),                 // SPS
            rtp(seq0.wrapping_add(1), ts, false, &[0x68, 9]),       // PPS
            rtp(seq0.wrapping_add(2), ts, true, &[0x65, 4, 5, 6]),  // IDR + marker
        ]
    }

    #[test]
    fn single_nal_au_reassembles_with_config() {
        let mut d = H264Depacketizer::new();
        let pkts = idr_packets(100, 90_000);
        let mut au = None;
        for p in &pkts {
            au = d.push(p).or(au);
        }
        let au = au.expect("marker closes the AU");
        assert!(au.keyframe);
        assert_eq!(
            au.data,
            [&[0, 0, 0, 1, 0x67, 1, 2, 3][..], &[0, 0, 0, 1, 0x68, 9], &[0, 0, 0, 1, 0x65, 4, 5, 6]].concat()
        );
        let (sps, pps) = d.take_config().expect("SPS+PPS captured");
        assert_eq!(sps, vec![0x67, 1, 2, 3]);
        assert_eq!(pps, vec![0x68, 9]);
        assert!(d.take_config().is_none(), "config emitted once until it changes");
    }

    #[test]
    fn fu_a_fragments_reassemble() {
        let mut d = H264Depacketizer::new();
        for p in idr_packets(1, 0) {
            d.push(&p);
        }
        // A P-slice (type 1) split across three FU-A packets: header 0x41 → FU
        // indicator 0x5C (NRI kept, type 28), FU header type 1 with S/E bits.
        let au = [
            d.push(&rtp(4, 3000, false, &[0x5C, 0x81, 10, 11])), // S
            d.push(&rtp(5, 3000, false, &[0x5C, 0x01, 12])),     // middle
            d.push(&rtp(6, 3000, true, &[0x5C, 0x41, 13])),      // E + marker
        ]
        .into_iter()
        .flatten()
        .next()
        .expect("FU-A end + marker closes the AU");
        assert!(!au.keyframe);
        assert_eq!(au.data, vec![0, 0, 0, 1, 0x41, 10, 11, 12, 13]);
        assert_eq!(au.pts_us, 3000 * 1000 / 90, "90 kHz ticks → µs");
    }

    #[test]
    fn stap_a_aggregation_splits() {
        let mut d = H264Depacketizer::new();
        // STAP-A carrying SPS + PPS + IDR in one packet, marker set.
        let mut pl = vec![0x78]; // STAP-A, NRI 3
        for nal in [&[0x67u8, 1][..], &[0x68, 2], &[0x65, 3]] {
            pl.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            pl.extend_from_slice(nal);
        }
        let au = d.push(&rtp(9, 0, true, &pl)).expect("one packet, one AU");
        assert!(au.keyframe);
        assert_eq!(
            au.data,
            [&[0, 0, 0, 1, 0x67, 1][..], &[0, 0, 0, 1, 0x68, 2], &[0, 0, 0, 1, 0x65, 3]].concat()
        );
    }

    #[test]
    fn a_gap_discards_until_the_next_keyframe() {
        let mut d = H264Depacketizer::new();
        for p in idr_packets(1, 0) {
            d.push(&p);
        }
        // seq 4 lost; 5 arrives → the P-frame AU must NOT be emitted…
        assert!(d.push(&rtp(5, 6000, true, &[0x41, 1])).is_none(), "gap poisons the AU");
        // …a following P-frame is still suppressed (no keyframe yet)…
        assert!(d.push(&rtp(6, 9000, true, &[0x41, 2])).is_none());
        // …and the next IDR resumes playback.
        let au = d.push(&rtp(7, 12_000, true, &[0x65, 3])).expect("IDR resumes");
        assert!(au.keyframe);
    }

    #[test]
    fn timestamp_wrap_is_monotonic() {
        let mut d = H264Depacketizer::new();
        let near_wrap = u32::MAX - 45_000; // half a second before the wrap
        for p in idr_packets(1, near_wrap) {
            d.push(&p);
        }
        let au = d
            .push(&rtp(4, near_wrap.wrapping_add(90_000), true, &[0x65, 1]))
            .expect("keyframe after the wrap");
        assert_eq!(au.pts_us, 1_000_000, "one second later despite the u32 wrap");
    }

    /// Annex-B helper: prefix each NAL with a 4-byte start code and concatenate.
    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    /// Feed a packet stream through the depacketizer and collect the AUs it emits.
    fn depacketize(pkts: &[Vec<u8>]) -> Vec<AccessUnit> {
        let mut d = H264Depacketizer::new();
        let mut aus = Vec::new();
        for p in pkts {
            if let Some(au) = d.push(p) {
                aus.push(au);
            }
        }
        aus
    }

    #[test]
    fn packetize_single_nal_round_trips() {
        // A keyframe AU (SPS+PPS+IDR), each NAL small enough for a single-NAL packet.
        let au = annexb(&[&[0x67, 1, 2, 3], &[0x68, 9], &[0x65, 4, 5, 6]]);
        let mut p = H264Packetizer::new(0xDEAD_BEEF, 96);
        let pkts = p.packetize(&au, 90_000, DEFAULT_MTU);
        assert_eq!(pkts.len(), 3, "one packet per NAL");
        assert!(pkts.last().unwrap()[1] & 0x80 != 0, "marker on the last packet");
        assert!(pkts[..2].iter().all(|p| p[1] & 0x80 == 0), "no marker before the end");

        let aus = depacketize(&pkts);
        assert_eq!(aus.len(), 1);
        assert!(aus[0].keyframe);
        assert_eq!(aus[0].data, au, "depacketized bytes match the original AU");
    }

    #[test]
    fn packetize_large_nal_fu_a_round_trips() {
        // A big IDR slice that must FU-A fragment across several packets at a small MTU.
        let mut slice = vec![0x65]; // IDR NAL header
        slice.extend((0..4000u32).map(|i| (i & 0xFF) as u8));
        let au = annexb(&[&[0x67, 1, 2, 3], &[0x68, 9], &slice]);
        let mut p = H264Packetizer::new(1, 96);
        let pkts = p.packetize(&au, 12_345, 300);
        // SPS + PPS single-NAL, then many FU-A fragments for the slice.
        assert!(pkts.len() > 4, "the slice fragmented: {} packets", pkts.len());
        assert!(pkts.last().unwrap()[1] & 0x80 != 0, "marker on the last fragment");
        assert!(pkts.iter().all(|p| p.len() <= 300), "every packet within MTU");

        let aus = depacketize(&pkts);
        assert_eq!(aus.len(), 1, "reassembles into exactly one AU");
        assert!(aus[0].keyframe);
        assert_eq!(aus[0].data, au, "FU-A reassembly is byte-exact");
    }

    #[test]
    fn packetize_multi_au_stream_round_trips() {
        let mut p = H264Packetizer::new(7, 102);
        let idr = annexb(&[&[0x67, 1, 2, 3], &[0x68, 9], &[0x65, 4, 5, 6]]);
        // A P-frame big enough to fragment, to mix single-NAL + FU-A across AUs.
        let mut pslice = vec![0x41];
        pslice.extend((0..900u32).map(|i| (i & 0xFF) as u8));
        let pframe = annexb(&[&pslice]);

        let mut pkts = Vec::new();
        pkts.extend(p.packetize(&idr, 0, 400));
        pkts.extend(p.packetize(&pframe, 3000, 400));

        let aus = depacketize(&pkts);
        assert_eq!(aus.len(), 2);
        assert!(aus[0].keyframe && !aus[1].keyframe);
        assert_eq!(aus[0].data, idr);
        assert_eq!(aus[1].data, pframe);
        assert_eq!(aus[1].pts_us, 3000 * 1000 / 90);
    }

    #[test]
    fn packetize_empty_au_yields_nothing() {
        let mut p = H264Packetizer::new(1, 96);
        assert!(p.packetize(&[], 0, DEFAULT_MTU).is_empty());
        assert!(p.packetize(&[0, 0, 0, 1], 0, DEFAULT_MTU).is_empty(), "start code, no NAL body");
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut d = H264Depacketizer::new();
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x80; 5],
            rtp(1, 0, true, &[]),                    // header only → no payload
            rtp(2, 0, true, &[0x78, 0xFF, 0xFF]),    // STAP-A length overrun
            rtp(3, 0, true, &[0x5C]),                // FU-A with no header
            rtp(4, 0, true, &[0xFD, 1, 2]),          // reserved NAL type
            {
                let mut p = rtp(5, 0, true, &[0x41, 1, 2]);
                p[0] |= 0x20; // padding flag with garbage padding length
                *p.last_mut().unwrap() = 200;
                p
            },
        ];
        for c in cases {
            let _ = d.push(&c);
        }
    }
}
