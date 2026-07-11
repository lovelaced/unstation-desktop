//! Minimal H.264 Picture Order Count (POC) reconstruction.
//!
//! Real encoders (OBS/x264) emit **B-frames**, whose presentation order differs from the
//! decode order they're transmitted in — but they stamp WHIP RTP with MONOTONIC
//! decode-order timestamps, so the reorder isn't in the timestamps at all. It lives in the
//! bitstream: each slice carries a `pic_order_cnt_lsb`, and the POC derived from it IS the
//! presentation order. We parse the SPS once for the POC parameters, then each slice's LSB,
//! and track the POC MSB across the stream. The muxer turns POC into a presentation
//! timestamp so it can write correct composition offsets (`ctts`); without this, strict
//! players (AVFoundation) fail to decode a B-frame stream.
//!
//! Scope: **POC type 0** (what x264 / virtually all live encoders use). Any other type,
//! or a malformed header, yields `None` and the muxer falls back to decode-order timing
//! (which is what non-B-frame streams want anyway).

/// Bit reader over an RBSP (emulation-prevention bytes already removed), MSB-first.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> u32 {
        let byte = self.pos / 8;
        if byte >= self.data.len() {
            self.pos += 1;
            return 0; // past the end reads as 0 (defensive; callers bound their reads)
        }
        let b = (self.data[byte] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        b as u32
    }

    /// Read `n` bits (n ≤ 32) as an unsigned integer.
    fn u(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> u32 {
        let mut zeros = 0u32;
        while self.pos / 8 < self.data.len() && self.bit() == 0 {
            zeros += 1;
            if zeros > 31 {
                return 0; // malformed / runaway
            }
        }
        if zeros == 0 {
            return 0;
        }
        (1u32 << zeros) - 1 + self.u(zeros)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> i32 {
        let k = self.ue();
        let sign = if k & 1 == 1 { 1 } else { -1 };
        sign * ((k + 1) / 2) as i32
    }
}

/// Strip H.264 emulation-prevention bytes: any `00 00 03` becomes `00 00` (the `03` is
/// removed). Input is the NAL payload AFTER the 1-byte NAL header.
fn unescape_rbsp(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    let mut i = 0;
    while i < payload.len() {
        let b = payload[i];
        if zeros >= 2 && b == 0x03 && i + 1 < payload.len() && payload[i + 1] <= 0x03 {
            // Drop the emulation-prevention byte.
            zeros = 0;
            i += 1;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    out
}

/// The SPS fields needed to read POC out of slice headers.
#[derive(Clone, Copy, Debug)]
pub struct SpsPoc {
    pub log2_max_frame_num: u32,
    pub log2_max_poc_lsb: u32,
    pub frame_mbs_only: bool,
    pub separate_colour_plane: bool,
}

fn skip_scaling_list(r: &mut BitReader, size: u32) {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se();
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
}

/// Parse an SPS NAL (INCLUDING its 1-byte header, e.g. `0x67…`) for POC parameters.
/// `None` if it isn't an SPS, uses POC type ≠ 0, or is malformed.
pub fn parse_sps(sps_nal: &[u8]) -> Option<SpsPoc> {
    if sps_nal.is_empty() || sps_nal[0] & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(&sps_nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u(8);
    let _constraints = r.u(8);
    let _level_idc = r.u(8);
    let _sps_id = r.ue();

    let mut separate_colour_plane = false;
    // High and higher profiles carry chroma + scaling-list syntax before the POC fields.
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format_idc = r.ue();
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit() == 1;
        }
        let _bit_depth_luma = r.ue();
        let _bit_depth_chroma = r.ue();
        let _qpprime = r.bit();
        if r.bit() == 1 {
            // seq_scaling_matrix_present: 8 lists (12 for 4:4:4), sizes 16 then 64.
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..count {
                if r.bit() == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 });
                }
            }
        }
    }

    let log2_max_frame_num = r.ue() + 4;
    let poc_type = r.ue();
    let log2_max_poc_lsb = if poc_type == 0 {
        r.ue() + 4
    } else {
        // Type 1/2 aren't supported for reorder; parse enough to not matter (we bail).
        0
    };
    if poc_type != 0 {
        return None;
    }
    let _max_num_ref_frames = r.ue();
    let _gaps = r.bit();
    let _w = r.ue();
    let _h = r.ue();
    let frame_mbs_only = r.bit() == 1;

    Some(SpsPoc {
        log2_max_frame_num,
        log2_max_poc_lsb,
        frame_mbs_only,
        separate_colour_plane,
    })
}

/// Read `pic_order_cnt_lsb` from a slice NAL (INCLUDING its header). Returns
/// `(is_idr, poc_lsb)`, or `None` if it isn't a coded slice / can't be parsed.
pub fn slice_poc_lsb(slice_nal: &[u8], sps: &SpsPoc) -> Option<(bool, u32)> {
    if slice_nal.is_empty() {
        return None;
    }
    let nal_type = slice_nal[0] & 0x1f;
    if nal_type != 1 && nal_type != 5 {
        return None; // not a coded slice (of a non-IDR / IDR picture)
    }
    let is_idr = nal_type == 5;
    let rbsp = unescape_rbsp(&slice_nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let _first_mb = r.ue();
    let _slice_type = r.ue();
    let _pps_id = r.ue();
    if sps.separate_colour_plane {
        let _colour_plane_id = r.u(2);
    }
    let _frame_num = r.u(sps.log2_max_frame_num);
    if !sps.frame_mbs_only {
        let field_pic_flag = r.bit();
        if field_pic_flag == 1 {
            let _bottom = r.bit();
        }
    }
    if is_idr {
        let _idr_pic_id = r.ue();
    }
    // POC type 0: pic_order_cnt_lsb is next.
    let poc_lsb = r.u(sps.log2_max_poc_lsb);
    Some((is_idr, poc_lsb))
}

/// Tracks the POC MSB across frames (H.264 §8.2.1.1, POC type 0) to reconstruct the full,
/// monotonic-per-GOP presentation order from the wrapping `pic_order_cnt_lsb`.
pub struct PocTracker {
    max_poc_lsb: i32,
    prev_msb: i32,
    prev_lsb: i32,
}

impl PocTracker {
    pub fn new(sps: &SpsPoc) -> Self {
        Self {
            max_poc_lsb: 1 << sps.log2_max_poc_lsb,
            prev_msb: 0,
            prev_lsb: 0,
        }
    }

    /// Full POC (TopFieldOrderCnt) for a frame. `is_idr` resets the reference; `nal_ref_idc`
    /// is the NAL header's ref bits (0 for a disposable/non-reference frame — those must NOT
    /// advance the MSB reference, per spec).
    pub fn poc(&mut self, is_idr: bool, poc_lsb: u32, nal_ref_idc: u8) -> i32 {
        if is_idr {
            self.prev_msb = 0;
            self.prev_lsb = 0;
        }
        let poc_lsb = poc_lsb as i32;
        let half = self.max_poc_lsb / 2;
        let msb = if poc_lsb < self.prev_lsb && (self.prev_lsb - poc_lsb) >= half {
            self.prev_msb + self.max_poc_lsb
        } else if poc_lsb > self.prev_lsb && (poc_lsb - self.prev_lsb) > half {
            self.prev_msb - self.max_poc_lsb
        } else {
            self.prev_msb
        };
        let poc = msb + poc_lsb;
        // Only reference pictures update the MSB reference state.
        if nal_ref_idc != 0 {
            self.prev_msb = msb;
            self.prev_lsb = poc_lsb;
        }
        poc
    }
}

/// `nal_ref_idc` from a NAL header byte (bits 5–6).
pub fn nal_ref_idc(nal_header: u8) -> u8 {
    (nal_header >> 5) & 0x03
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real x264 High-profile SPS captured from an OBS-style encode (bf=2).
    const REAL_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];

    #[test]
    fn parses_real_high_profile_sps() {
        let sps = parse_sps(REAL_SPS).expect("High-profile SPS parses");
        assert!(sps.frame_mbs_only, "progressive");
        assert!(!sps.separate_colour_plane);
        // log2_max_poc_lsb must be sane (4..=16); a wrong scaling-list skip corrupts it.
        assert!(
            (4..=16).contains(&sps.log2_max_poc_lsb),
            "log2_max_poc_lsb={} — High-profile scaling-list skip likely wrong",
            sps.log2_max_poc_lsb
        );
        assert!((4..=16).contains(&sps.log2_max_frame_num));
    }

    #[test]
    fn real_slices_reconstruct_display_order() {
        // First 10 slice-NAL heads from the same bf=2 stream, in DECODE (bitstream) order.
        // Only the header bytes are needed to reach pic_order_cnt_lsb.
        let slices: &[&[u8]] = &[
            &[0x65, 0x88, 0x84, 0x00, 0x37, 0xff], // IDR (nal_ref_idc=3)
            &[0x41, 0x9a, 0x23, 0x6c, 0x43, 0x7f], // P (ref)
            &[0x41, 0x9e, 0x41, 0x78, 0x85, 0x7f], // ref B (b-pyramid) or P
            &[0x01, 0x9e, 0x62, 0x6a, 0x42, 0x7f], // disposable B (nal_ref_idc=0)
            &[0x41, 0x9a, 0x65, 0x49, 0xa8, 0x41],
            &[0x01, 0x9e, 0x84, 0x6a, 0x42, 0x7f],
            &[0x41, 0x9a, 0x88, 0x49, 0xe1, 0x0a],
            &[0x41, 0x9e, 0xa6, 0x45, 0x34, 0x4c],
            &[0x01, 0x9e, 0xc7, 0x6a, 0x42, 0x7f],
            &[0x41, 0x9a, 0xcb, 0x49, 0xa8, 0x41],
        ];
        let sps = parse_sps(REAL_SPS).unwrap();
        let mut tracker = PocTracker::new(&sps);
        let mut pocs = Vec::new();
        for s in slices {
            let (is_idr, lsb) = slice_poc_lsb(s, &sps).expect("slice parses");
            let poc = tracker.poc(is_idr, lsb, nal_ref_idc(s[0]));
            pocs.push(poc);
        }
        // The IDR is POC 0 and presents first.
        assert_eq!(pocs[0], 0, "IDR POC is 0");
        // POCs are all distinct and non-negative within the GOP (valid presentation order).
        assert!(pocs.iter().all(|&p| p >= 0), "GOP POCs non-negative: {pocs:?}");
        let mut sorted = pocs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pocs.len(), "POCs are distinct: {pocs:?}");
        // Decode order is NOT presentation order here (there are B-frames): some later-decoded
        // frame must present before an earlier-decoded one.
        assert!(
            pocs.windows(2).any(|w| w[1] < w[0]),
            "B-frame reorder present: {pocs:?}"
        );
    }

    #[test]
    fn non_slice_and_wrong_type_return_none() {
        let sps = parse_sps(REAL_SPS).unwrap();
        assert!(slice_poc_lsb(&[0x67, 0x00], &sps).is_none(), "SPS NAL is not a slice");
        assert!(parse_sps(&[0x41, 0x00]).is_none(), "not an SPS");
        assert!(slice_poc_lsb(&[], &sps).is_none(), "empty slice NAL");
    }

    // ---- deterministic SPS / slice synthesis (no ffmpeg) ------------------------------
    //
    // A minimal H.264 SPS + slice-header *encoder* — the inverse of `parse_sps` /
    // `slice_poc_lsb` — so the 4:4:4, scaling-matrix, POC-type≠0, interlaced and truncated
    // paths each get a purpose-built fixture without a real encoder. It writes the bits the
    // parser reads (round-tripped through emulation-prevention); it never parses.

    struct BitWriter {
        out: Vec<u8>,
        cur: u8,
        n: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            Self { out: Vec::new(), cur: 0, n: 0 }
        }
        fn bit(&mut self, b: u32) {
            self.cur = (self.cur << 1) | (b as u8 & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.n = 0;
            }
        }
        fn put(&mut self, v: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.bit((v >> i) & 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let code = v as u64 + 1;
            let m = 63 - code.leading_zeros();
            for _ in 0..m {
                self.bit(0);
            }
            for i in (0..=m).rev() {
                self.bit(((code >> i) & 1) as u32);
            }
        }
        fn se(&mut self, v: i32) {
            let k = if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 };
            self.ue(k);
        }
        /// Byte-align, emulation-prevent, and prepend the given 1-byte NAL header.
        fn nal(mut self, header: u8) -> Vec<u8> {
            if self.n > 0 {
                self.cur <<= 8 - self.n;
                self.out.push(self.cur);
            }
            let mut nal = vec![header];
            let mut zeros = 0;
            for &b in &self.out {
                if zeros >= 2 && b <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                nal.push(b);
                zeros = if b == 0 { zeros + 1 } else { 0 };
            }
            nal
        }
    }

    #[derive(Clone)]
    struct SpsCfg {
        profile: u32,
        chroma_format_idc: u32,
        separate_colour_plane: bool,
        scaling_present: bool,
        scaling_lists: Vec<bool>,
        poc_type: u32,
        log2_max_frame_num_minus4: u32,
        log2_max_poc_lsb_minus4: u32,
        frame_mbs_only: bool,
    }
    impl Default for SpsCfg {
        fn default() -> Self {
            Self {
                profile: 100,
                chroma_format_idc: 1,
                separate_colour_plane: false,
                scaling_present: false,
                scaling_lists: Vec::new(),
                poc_type: 0,
                log2_max_frame_num_minus4: 0,
                log2_max_poc_lsb_minus4: 0,
                frame_mbs_only: true,
            }
        }
    }
    impl SpsCfg {
        fn encode(&self) -> Vec<u8> {
            let mut w = BitWriter::new();
            let high = matches!(
                self.profile,
                100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
            );
            w.put(self.profile, 8);
            w.put(0, 8); // constraints
            w.put(30, 8); // level_idc
            w.ue(0); // sps_id
            if high {
                w.ue(self.chroma_format_idc);
                if self.chroma_format_idc == 3 {
                    w.bit(self.separate_colour_plane as u32);
                }
                w.ue(0); // bit_depth_luma_minus8
                w.ue(0); // bit_depth_chroma_minus8
                w.bit(0); // qpprime
                w.bit(self.scaling_present as u32);
                if self.scaling_present {
                    for (i, &present) in self.scaling_lists.iter().enumerate() {
                        w.bit(present as u32);
                        if present {
                            let size = if i < 6 { 16 } else { 64 };
                            for _ in 0..size {
                                w.se(0);
                            }
                        }
                    }
                }
            }
            w.ue(self.log2_max_frame_num_minus4);
            w.ue(self.poc_type);
            if self.poc_type == 0 {
                w.ue(self.log2_max_poc_lsb_minus4);
            }
            w.ue(1); // max_num_ref_frames
            w.bit(0); // gaps
            w.ue(9); // pic_width_in_mbs_minus1 (irrelevant to POC)
            w.ue(9); // pic_height_in_map_units_minus1
            w.bit(self.frame_mbs_only as u32);
            // parse_sps stops after frame_mbs_only; nothing more is read.
            w.nal(0x67)
        }
    }

    /// A coded-slice header carrying just enough to reach pic_order_cnt_lsb.
    struct SliceCfg {
        is_idr: bool,
        nal_ref_idc: u8,
        frame_num: u32,
        field_pic: Option<bool>, // Some(bottom) sets field_pic_flag=1
        poc_lsb: u32,
    }
    impl SliceCfg {
        fn encode(&self, sps: &SpsCfg) -> Vec<u8> {
            let mut w = BitWriter::new();
            w.ue(0); // first_mb_in_slice
            w.ue(if self.is_idr { 7 } else { 5 }); // slice_type (value unused by parser)
            w.ue(0); // pps_id
            if sps.chroma_format_idc == 3 && sps.separate_colour_plane {
                w.put(0, 2); // colour_plane_id
            }
            w.put(self.frame_num, sps.log2_max_frame_num_minus4 + 4);
            if !sps.frame_mbs_only {
                match self.field_pic {
                    Some(bottom) => {
                        w.bit(1);
                        w.bit(bottom as u32);
                    }
                    None => w.bit(0),
                }
            }
            if self.is_idr {
                w.ue(0); // idr_pic_id
            }
            w.put(self.poc_lsb, sps.log2_max_poc_lsb_minus4 + 4);
            let header = ((self.nal_ref_idc & 3) << 5) | if self.is_idr { 5 } else { 1 };
            w.nal(header)
        }
    }

    #[test]
    fn parses_high444_sps() {
        // chroma_format_idc 3 → separate_colour_plane bit is present in the SPS, and its slices
        // carry a 2-bit colour_plane_id the POC read must skip.
        let cfg = SpsCfg {
            profile: 244,
            chroma_format_idc: 3,
            separate_colour_plane: true,
            log2_max_poc_lsb_minus4: 2, // log2=6
            ..SpsCfg::default()
        };
        let sps = parse_sps(&cfg.encode()).expect("4:4:4 SPS parses");
        assert!(sps.separate_colour_plane);
        assert_eq!(sps.log2_max_poc_lsb, 6);
        let slice = SliceCfg { is_idr: true, nal_ref_idc: 3, frame_num: 0, field_pic: None, poc_lsb: 0 }
            .encode(&cfg);
        assert_eq!(slice_poc_lsb(&slice, &sps), Some((true, 0)));
        let slice2 = SliceCfg { is_idr: false, nal_ref_idc: 2, frame_num: 1, field_pic: None, poc_lsb: 12 }
            .encode(&cfg);
        assert_eq!(slice_poc_lsb(&slice2, &sps), Some((false, 12)));
    }

    #[test]
    fn parses_sps_with_scaling_matrix() {
        // seq_scaling_matrix_present forces the scaling-list skip; a wrong skip corrupts the
        // POC fields that follow, so a sane log2_max_poc_lsb proves the skip is exact.
        let cfg = SpsCfg {
            profile: 100,
            scaling_present: true,
            scaling_lists: vec![true, false, false, true, false, false, true, false],
            log2_max_poc_lsb_minus4: 4, // log2=8
            log2_max_frame_num_minus4: 2, // log2=6
            ..SpsCfg::default()
        };
        let sps = parse_sps(&cfg.encode()).expect("scaling-matrix SPS parses");
        assert_eq!(sps.log2_max_poc_lsb, 8);
        assert_eq!(sps.log2_max_frame_num, 6);
    }

    #[test]
    fn rejects_unsupported_poc_type() {
        // POC type ≠ 0 is unsupported for reorder → None (the muxer then uses decode-order
        // timing). Covers both the type-1 and type-2 paths.
        for t in [1u32, 2] {
            let cfg = SpsCfg { poc_type: t, ..SpsCfg::default() };
            // Note: for type 1 the encoder omits the extra cycle syntax, but parse_sps bails on
            // the type before reading it — exactly the branch under test.
            assert!(parse_sps(&cfg.encode()).is_none(), "poc_type {t} unsupported");
        }
    }

    #[test]
    fn interlaced_slice_reads_field_flags() {
        // frame_mbs_only=0 → each slice header carries field_pic_flag (+ bottom_field_flag) the
        // POC read must consume before pic_order_cnt_lsb.
        let cfg = SpsCfg { frame_mbs_only: false, log2_max_poc_lsb_minus4: 2, ..SpsCfg::default() };
        let sps = parse_sps(&cfg.encode()).expect("interlaced SPS parses");
        assert!(!sps.frame_mbs_only);
        // A field-coded slice (field_pic_flag=1, bottom_field_flag=1).
        let field = SliceCfg { is_idr: false, nal_ref_idc: 2, frame_num: 3, field_pic: Some(true), poc_lsb: 7 }
            .encode(&cfg);
        assert_eq!(slice_poc_lsb(&field, &sps), Some((false, 7)));
        // A frame-coded slice in the same interlaced stream (field_pic_flag=0).
        let frame = SliceCfg { is_idr: false, nal_ref_idc: 2, frame_num: 4, field_pic: None, poc_lsb: 9 }
            .encode(&cfg);
        assert_eq!(slice_poc_lsb(&frame, &sps), Some((false, 9)));
    }

    #[test]
    fn truncated_slice_reads_zeros_past_end() {
        // A slice header cut off before pic_order_cnt_lsb must not panic: the reader treats
        // past-the-end bits as 0 and returns a (defensive) value.
        let sps = parse_sps(REAL_SPS).unwrap();
        // Just the NAL header + one payload byte — nowhere near pic_order_cnt_lsb.
        let (is_idr, _lsb) = slice_poc_lsb(&[0x41, 0x80], &sps).expect("no panic on short slice");
        assert!(!is_idr);
    }

    #[test]
    fn exp_golomb_runaway_is_bounded() {
        // A slice whose first Exp-Golomb code is an unterminated run of zeros must not loop —
        // ue() bails after 31 leading zeros. This is the hostile-input guard.
        let sps = parse_sps(REAL_SPS).unwrap();
        let mut zeros = vec![0x41u8];
        zeros.extend_from_slice(&[0u8; 8]); // 64 zero bits → runaway
        assert!(slice_poc_lsb(&zeros, &sps).is_some(), "bounded, no hang");
    }

    #[test]
    fn poc_tracker_handles_lsb_wraparound_both_directions() {
        // log2_max_poc_lsb=4 → max=16, half=8. Exercise both MSB-adjust arms plus the
        // non-reference and IDR-reset rules.
        let sps = SpsPoc {
            log2_max_frame_num: 4,
            log2_max_poc_lsb: 4,
            frame_mbs_only: true,
            separate_colour_plane: false,
        };
        let mut t = PocTracker::new(&sps);
        assert_eq!(t.poc(true, 0, 3), 0, "IDR anchors at 0");
        assert_eq!(t.poc(false, 2, 3), 2, "no wrap");
        // lsb jumps far forward (11 vs prev 2, diff 9 > 8) → MSB wraps DOWN.
        assert_eq!(t.poc(false, 11, 3), -5, "forward jump wraps msb down");
        // lsb drops far back (1 vs prev 11, diff 10 ≥ 8) → MSB wraps UP.
        assert_eq!(t.poc(false, 1, 3), 1, "backward jump wraps msb up");
        // A non-reference frame (nal_ref_idc=0) must NOT advance the MSB reference state.
        let before = t.poc(false, 4, 0);
        let after = t.poc(false, 4, 0);
        assert_eq!(before, after, "non-ref frames don't move the reference");
        // IDR resets the reference regardless of history.
        assert_eq!(t.poc(true, 0, 3), 0, "IDR resets");
    }

    #[test]
    fn nal_ref_idc_extracts_ref_bits() {
        assert_eq!(nal_ref_idc(0x65), 3); // IDR, ref 3
        assert_eq!(nal_ref_idc(0x41), 2); // P, ref 2
        assert_eq!(nal_ref_idc(0x01), 0); // disposable B, ref 0
    }
}
