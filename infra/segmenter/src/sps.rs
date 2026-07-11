//! Parse coded dimensions from an H.264 SPS (ISO/IEC 14496-10 §7.3.2.1.1).
//!
//! The WHIP ingest path receives only SPS/PPS from the RTP stream — no explicit
//! width/height like the Android camera config carries — but the CMAF init segment's
//! `tkhd`/`avc1` boxes need real dimensions. This reads them straight from the SPS
//! (an Exp-Golomb bitstream over the RBSP, emulation-prevention bytes removed).

/// Decode `(width, height)` in luma samples from an SPS NAL. `sps` may include or omit
/// the 1-byte NAL header (type 7); we detect and skip it. Returns `None` on a truncated
/// or unparseable SPS so the caller can fall back to a default.
pub fn dimensions(sps: &[u8]) -> Option<(u16, u16)> {
    if sps.is_empty() {
        return None;
    }
    // Skip the NAL header byte if present (forbidden-zero clear, type == 7).
    let body = if sps[0] & 0x1F == 7 && sps[0] & 0x80 == 0 { &sps[1..] } else { sps };
    let rbsp = unescape(body);
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.u(8)?;
    r.skip(8)?; // constraint flags + reserved
    r.skip(8)?; // level_idc
    r.ue()?; // seq_parameter_set_id

    // High-profile family carries extra chroma fields before the frame geometry.
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            // seq_scaling_matrix_present — skip the (rarely used) scaling lists.
            let count = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..count {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        r.skip(1)?; // delta_pic_order_always_zero_flag
        r.se()?; // offset_for_non_ref_pic
        r.se()?; // offset_for_top_to_bottom_field
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?; // offset_for_ref_frame[i]
        }
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs = r.ue()?.checked_add(1)?;
    let pic_height_in_map_units = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.u(1)?;
    if frame_mbs_only == 0 {
        r.skip(1)?; // mb_adaptive_frame_field_flag
    }
    r.skip(1)?; // direct_8x8_inference_flag

    // frame_cropping: subtract the cropped border (chroma-scaled).
    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0u64, 0u64, 0u64, 0u64);
    if r.u(1)? == 1 {
        crop_l = r.ue()?;
        crop_r = r.ue()?;
        crop_t = r.ue()?;
        crop_b = r.ue()?;
    }

    let width = pic_width_in_mbs.checked_mul(16)?;
    let height = pic_height_in_map_units.checked_mul(16)?.checked_mul(2 - frame_mbs_only as u64)?;
    // 4:2:0 crop units: horizontal 2, vertical 2*(2-frame_mbs_only). Good enough for the
    // common case (chroma_format_idc 1); an exact SubWidthC/SubHeightC table isn't worth it.
    let crop_x = (crop_l + crop_r) * 2;
    let crop_y = (crop_t + crop_b) * 2 * (2 - frame_mbs_only as u64);
    let w = width.checked_sub(crop_x)?;
    let h = height.checked_sub(crop_y)?;
    if w == 0 || h == 0 || w > u16::MAX as u64 || h > u16::MAX as u64 {
        return None;
    }
    Some((w as u16, h as u16))
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Strip H.264 emulation-prevention bytes (`00 00 03` → `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue; // drop the emulation-prevention 0x03
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }
    fn u(&mut self, n: usize) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.data.get(self.bit / 8)?;
            let b = (byte >> (7 - (self.bit % 8))) & 1;
            v = (v << 1) | b as u64;
            self.bit += 1;
        }
        Some(v)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.u(n).map(|_| ())
    }
    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 63 {
                return None; // malformed
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.u(zeros)?;
        Some((1u64 << zeros) - 1 + rest)
    }
    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i64> {
        let k = self.ue()?;
        Some(if k % 2 == 0 { -((k / 2) as i64) } else { ((k + 1) / 2) as i64 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_640x360_baseline() {
        // A real 640x360 baseline SPS (profile 66) from x264.
        let sps = [0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0xa0, 0x2f, 0xf9, 0x50];
        // 640x360: pic_width_in_mbs=40 (640/16), height 360 crops from 368.
        let (w, h) = dimensions(&sps).expect("parse baseline SPS");
        assert_eq!((w, h), (640, 360), "got {w}x{h}");
    }

    #[test]
    fn parses_1280x720_high() {
        // A real 1280x720 High-profile SPS (profile 100) — exercises the chroma branch.
        let sps = [
            0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x6c,
            0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8c, 0x18, 0xcb,
        ];
        let (w, h) = dimensions(&sps).expect("parse high SPS");
        assert_eq!((w, h), (1280, 720), "got {w}x{h}");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(dimensions(&[]), None);
        assert_eq!(dimensions(&[0x67]), None);
        assert_eq!(dimensions(&[0x67, 0xFF, 0xFF, 0xFF]), None);
    }

    // ---- deterministic SPS synthesis (no ffmpeg) --------------------------------------
    //
    // A tiny H.264 SPS *encoder* — the exact inverse of `dimensions`/`unescape`/`BitReader`
    // above — so the high-profile / scaling-matrix / POC-type-1 / interlaced / cropping /
    // overflow branches can each be hit by a purpose-built fixture without an encoder at test
    // time. It is NOT a decoder (it never reads the parser's fields); it only lays down the
    // bits the parser consumes, then round-trips through emulation-prevention so `unescape`
    // reconstructs exactly these bits.

    struct SpsWriter {
        out: Vec<u8>,
        cur: u8,
        n: u8,
    }
    impl SpsWriter {
        fn new() -> Self {
            Self { out: Vec::new(), cur: 0, n: 0 }
        }
        fn bit(&mut self, b: u64) {
            self.cur = (self.cur << 1) | (b as u8 & 1);
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.n = 0;
            }
        }
        fn put(&mut self, v: u64, bits: u32) {
            for i in (0..bits).rev() {
                self.bit((v >> i) & 1);
            }
        }
        /// Unsigned Exp-Golomb (inverse of `BitReader::ue`).
        fn ue(&mut self, v: u64) {
            let code = v + 1;
            let m = 63 - code.leading_zeros();
            for _ in 0..m {
                self.bit(0);
            }
            self.put(code, m + 1);
        }
        /// Signed Exp-Golomb (inverse of `BitReader::se`).
        fn se(&mut self, v: i64) {
            let k = if v > 0 { (2 * v - 1) as u64 } else { (-2 * v) as u64 };
            self.ue(k);
        }
        /// Byte-align, insert emulation-prevention bytes, prepend the `0x67` SPS NAL header.
        fn nal(mut self) -> Vec<u8> {
            if self.n > 0 {
                self.cur <<= 8 - self.n;
                self.out.push(self.cur);
            }
            let mut nal = vec![0x67u8];
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

    /// Configurable SPS. Defaults describe a 640x360 progressive baseline stream; each test
    /// flips exactly the fields it needs so the intended parser branch is the only variable.
    #[derive(Clone)]
    struct Sps {
        profile: u64,
        chroma_format_idc: u64,
        separate_colour_plane: bool,
        scaling_present: bool,
        scaling_lists: Vec<bool>,
        poc_type: u64,
        poc_cycle: Vec<i64>,
        width_mbs_minus1: u64,
        height_map_minus1: u64,
        frame_mbs_only: bool,
        crop: Option<(u64, u64, u64, u64)>,
    }
    impl Default for Sps {
        fn default() -> Self {
            Self {
                profile: 66,
                chroma_format_idc: 1,
                separate_colour_plane: false,
                scaling_present: false,
                scaling_lists: Vec::new(),
                poc_type: 0,
                poc_cycle: Vec::new(),
                width_mbs_minus1: 39, // 40 mbs → 640
                height_map_minus1: 22, // 23 map units → 368
                frame_mbs_only: true,
                crop: Some((0, 0, 0, 4)), // 368 → 360
            }
        }
    }
    impl Sps {
        fn encode(&self) -> Vec<u8> {
            let mut w = SpsWriter::new();
            let high = matches!(
                self.profile,
                100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
            );
            w.put(self.profile, 8);
            w.put(0, 8); // constraint flags
            w.put(30, 8); // level_idc
            w.ue(0); // seq_parameter_set_id
            if high {
                w.ue(self.chroma_format_idc);
                if self.chroma_format_idc == 3 {
                    w.bit(self.separate_colour_plane as u64);
                }
                w.ue(0); // bit_depth_luma_minus8
                w.ue(0); // bit_depth_chroma_minus8
                w.bit(0); // qpprime_y_zero_transform_bypass_flag
                w.bit(self.scaling_present as u64);
                if self.scaling_present {
                    for (i, &present) in self.scaling_lists.iter().enumerate() {
                        w.bit(present as u64);
                        if present {
                            let size = if i < 6 { 16 } else { 64 };
                            for _ in 0..size {
                                w.se(0); // flat (identity) scaling list — deltas all 0
                            }
                        }
                    }
                }
            }
            w.ue(4); // log2_max_frame_num_minus4
            w.ue(self.poc_type);
            if self.poc_type == 0 {
                w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
            } else if self.poc_type == 1 {
                w.bit(0); // delta_pic_order_always_zero_flag
                w.se(0); // offset_for_non_ref_pic
                w.se(0); // offset_for_top_to_bottom_field
                w.ue(self.poc_cycle.len() as u64);
                for &o in &self.poc_cycle {
                    w.se(o);
                }
            }
            w.ue(1); // max_num_ref_frames
            w.bit(0); // gaps_in_frame_num_value_allowed_flag
            w.ue(self.width_mbs_minus1);
            w.ue(self.height_map_minus1);
            w.bit(self.frame_mbs_only as u64);
            if !self.frame_mbs_only {
                w.bit(0); // mb_adaptive_frame_field_flag
            }
            w.bit(1); // direct_8x8_inference_flag
            match self.crop {
                Some((l, r, t, b)) => {
                    w.bit(1);
                    w.ue(l);
                    w.ue(r);
                    w.ue(t);
                    w.ue(b);
                }
                None => w.bit(0),
            }
            w.nal()
        }
    }

    #[test]
    fn synth_baseline_matches_640x360() {
        // Proves the encoder is the parser's inverse: the default fixture must read back as the
        // same 640x360 the hand-crafted real baseline SPS above yields.
        assert_eq!(dimensions(&Sps::default().encode()), Some((640, 360)));
    }

    #[test]
    fn parses_high444_with_separate_colour_plane() {
        // profile 244 (High 4:4:4) + chroma_format_idc 3 exercises the separate_colour_plane
        // skip inside the chroma branch.
        let sps = Sps {
            profile: 244,
            chroma_format_idc: 3,
            separate_colour_plane: true,
            crop: None,
            width_mbs_minus1: 79, // 1280
            height_map_minus1: 44, // 720
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), Some((1280, 720)));
    }

    #[test]
    fn parses_high_with_seq_scaling_matrix() {
        // seq_scaling_matrix_present with a mix of present/absent lists drives the scaling-list
        // skip (both the "present → consume a list" and "absent" arms).
        let sps = Sps {
            profile: 100,
            scaling_present: true,
            scaling_lists: vec![true, false, true, false, false, false, false, true],
            crop: None,
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), Some((640, 368)));
    }

    #[test]
    fn parses_poc_type_1() {
        // pic_order_cnt_type 1 carries a ref-frame offset cycle (each a signed Exp-Golomb) the
        // parser must skip to reach the frame geometry.
        let sps = Sps {
            poc_type: 1,
            poc_cycle: vec![1, -1, 2],
            crop: None,
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), Some((640, 368)));
    }

    #[test]
    fn interlaced_doubles_the_height() {
        // frame_mbs_only=0 → an extra mb_adaptive flag AND field-height doubling (map units are
        // half-frames). 22 map units → (22+1)*16*2 = 736.
        let sps = Sps {
            frame_mbs_only: false,
            crop: None,
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), Some((640, 736)));
    }

    #[test]
    fn interlaced_crop_scales_vertically() {
        // Vertical crop units are 2*(2-frame_mbs_only)=4 lines each when interlaced: crop_b=2
        // removes 8 lines from 736 → 728.
        let sps = Sps {
            frame_mbs_only: false,
            crop: Some((0, 0, 0, 2)),
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), Some((640, 728)));
    }

    #[test]
    fn rejects_zero_dimension_after_crop() {
        // A crop that consumes the whole width yields w==0 → None (not a panic / underflow).
        let sps = Sps {
            width_mbs_minus1: 0, // 16 px wide
            crop: Some((4, 4, 0, 0)), // crop_x = (4+4)*2 = 16 → 0
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), None);
    }

    #[test]
    fn rejects_oversized_dimension() {
        // pic_width_in_mbs large enough to exceed u16 → None (the >u16::MAX guard).
        let sps = Sps {
            width_mbs_minus1: 5000, // 5001*16 = 80016 > 65535
            crop: None,
            ..Sps::default()
        };
        assert_eq!(dimensions(&sps.encode()), None);
    }

    #[test]
    fn parses_body_without_nal_header() {
        // `dimensions` accepts an SPS with the 1-byte NAL header already stripped (first byte is
        // the profile_idc, not a type-7 header). Strip the 0x67 from a known-good fixture.
        let full = Sps::default().encode();
        assert_eq!(dimensions(&full[1..]), Some((640, 360)));
    }

    #[test]
    fn malformed_exp_golomb_runaway_is_none() {
        // A long run of zero bits makes an Exp-Golomb code exceed 63 leading zeros → None,
        // rather than looping or over-reading.
        let mut sps = vec![0x67, 0, 0, 0]; // header + profile/constraint/level = 0
        sps.extend_from_slice(&[0u8; 16]); // >64 zero bits for the next ue()
        assert_eq!(dimensions(&sps), None);
    }
}
