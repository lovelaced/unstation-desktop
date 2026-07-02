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
}
