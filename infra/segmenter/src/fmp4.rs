//! In-memory CMAF/fMP4 muxer — the non-ffmpeg source (Android camera publish, M4).
//!
//! Android can't spawn ffmpeg, and `MediaMuxer` can't emit *fragmented* MP4, so we box the
//! encoded access units ourselves: a CMAF init segment (`ftyp` + `moov`) built once from the
//! H.264 SPS/PPS, then one `moof` + `mdat` fragment per GOP. The output is ordinary CMAF —
//! the same shape ffmpeg produces — so it flows through the exact same mesh path
//! ([`crate::Segment`] → `segment_id` → publish/verify) and plays in the same players.
//!
//! Fragments need only be *valid* CMAF, not byte-identical to ffmpeg's: the content id is
//! `blake2b256(bytes)` of whatever we emit, the publisher signs that id, and viewers verify
//! received bytes against it — so the muxer is self-consistent by construction.
//!
//! Video-only (H.264) to start; AAC audio is an additive second track (a second `trak` in
//! `moov` + a second `traf` per fragment) once the capture side produces audio AUs.

use crate::Segment;
use bytes::Bytes;
use unstation_core::crypto::segment_id;
use unstation_core::types::Seq;

/// Video media timescale (ticks per second) used in the init + fragment timing.
pub const TIMESCALE: u32 = 90_000;
const VIDEO_TRACK_ID: u32 = 1;

/// H.264 decoder configuration + display size, learned once from the encoder's codec-specific
/// data (CSD: the SPS and PPS NAL units, WITHOUT Annex-B start codes or length prefixes).
#[derive(Clone)]
pub struct H264Params {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

/// One encoded video access unit queued for the current fragment.
struct Au {
    /// Sample data in AVCC form (each NAL unit prefixed by a 4-byte big-endian length).
    avcc: Vec<u8>,
    /// Sample duration in `TIMESCALE` ticks.
    duration: u32,
    keyframe: bool,
}

/// Accumulates encoded H.264 access units and emits one CMAF fragment per GOP.
///
/// Feed AUs with [`push_au`](Self::push_au); each new keyframe closes the previous GOP and
/// returns it as a [`Segment`]. Call [`flush`](Self::flush) to emit a trailing partial GOP.
///
/// In **low-latency mode** ([`new_ll`](Self::new_ll)) the builder additionally closes a
/// fragment once the pending AUs reach a target duration (a *part*, ~200ms), even mid-GOP —
/// so the mesh and player see fine-grained CMAF chunks instead of whole ~1s GOPs. Each part
/// is still an ordinary independently-parseable fragment (its own `moof`+`mdat`); the part
/// that *starts* with a keyframe is decode-independent, which the hls-server recovers from
/// the bytes with [`fragment_is_independent`] — no side-channel needed.
pub struct FragmentBuilder {
    params: H264Params,
    /// Next fragment sequence number (also the `Segment.seq` and `moof` `mfhd` sequence).
    seq: Seq,
    /// `tfdt` base media decode time: total sample duration emitted in prior fragments.
    base_decode_time: u64,
    pending: Vec<Au>,
    /// Sum of `pending` AU durations (ticks) — the length of the part being accumulated.
    pending_ticks: u64,
    /// LL mode: close a part once `pending_ticks` reaches this. `None` = one fragment per GOP.
    part_ticks: Option<u32>,
}

impl FragmentBuilder {
    pub fn new(params: H264Params) -> Self {
        Self { params, seq: 0, base_decode_time: 0, pending: Vec::new(), pending_ticks: 0, part_ticks: None }
    }

    /// Low-latency builder: also close a fragment every `part_ms` of media (a CMAF *part*),
    /// not just on GOP boundaries. `part_ms` is clamped to ≥20ms; a GOP shorter than a part
    /// still emits per-keyframe, so parts never straddle a keyframe.
    pub fn new_ll(params: H264Params, part_ms: u32) -> Self {
        let part_ticks = (part_ms.max(20) as u64 * TIMESCALE as u64 / 1000) as u32;
        Self { part_ticks: Some(part_ticks), ..Self::new(params) }
    }

    /// The CMAF init segment (`ftyp` + `moov`). Stable for the stream; push it once to the
    /// player (HLS `EXT-X-MAP`) and Bulletin before any media fragment.
    pub fn init_segment(&self) -> Bytes {
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp());
        out.extend_from_slice(&moov(&self.params));
        Bytes::from(out)
    }

    /// Queue one access unit. `nal` is a single frame's NAL units in **Annex-B** form (the
    /// `00 00 00 01` / `00 00 01` start-code framing `MediaCodec` emits); SPS/PPS NALs in it
    /// are dropped (they live in the init's `avcC`). `duration` is in `TIMESCALE` ticks.
    /// Returns the just-closed GOP as a fragment when this AU starts a new one.
    pub fn push_au(&mut self, nal: &[u8], duration: u32, keyframe: bool) -> Option<Segment> {
        // A keyframe closes the prior fragment (GOP or part) and starts a fresh, decode-
        // independent one. This has priority over the part-cadence close below.
        let closed = if keyframe && !self.pending.is_empty() { self.emit() } else { None };
        let avcc = annexb_to_avcc(nal);
        if !avcc.is_empty() {
            self.pending.push(Au { avcc, duration, keyframe });
            self.pending_ticks += duration as u64;
        }
        // LL mode: once the pending part reaches its target duration, close it mid-GOP. Skip
        // if a keyframe already closed a fragment this call, so we return at most one segment.
        if closed.is_none() {
            if let Some(pt) = self.part_ticks {
                if !self.pending.is_empty() && self.pending_ticks >= pt as u64 {
                    return self.emit();
                }
            }
        }
        closed
    }

    /// Emit any trailing accumulated AUs as a final fragment (e.g. on stop).
    pub fn flush(&mut self) -> Option<Segment> {
        if self.pending.is_empty() { None } else { self.emit() }
    }

    /// Build a `moof` + `mdat` fragment from the pending AUs and advance the timeline.
    fn emit(&mut self) -> Option<Segment> {
        if self.pending.is_empty() {
            return None;
        }
        let aus = std::mem::take(&mut self.pending);
        self.pending_ticks = 0;
        let seq = self.seq;
        self.seq += 1;

        let mut bytes = moof(seq, self.base_decode_time, &aus);
        bytes.extend_from_slice(&mdat(&aus));

        for au in &aus {
            self.base_decode_time += au.duration as u64;
        }

        let bytes = Bytes::from(bytes);
        let id = segment_id(&bytes);
        Some(Segment { seq, id, bytes })
    }
}

/// Convert Annex-B (start-code framed) NAL units to AVCC (4-byte length-prefixed), dropping
/// SPS(7)/PPS(8)/AUD(9) NALs — SPS/PPS belong in `avcC`, AUDs aren't carried in MP4 samples.
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in iter_annexb_nals(data) {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1f {
            7 | 8 | 9 => continue, // SPS / PPS / access-unit-delimiter
            _ => {}
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Split an Annex-B buffer into NAL units (payload between start codes, start code removed).
fn iter_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    // Positions of each start code (00 00 01, possibly preceded by an extra 00).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (idx, &(sc_start, payload_start)) in starts.iter().enumerate() {
        // A 4-byte start code is `00 00 00 01`; trim a trailing 0 of the previous NAL.
        let end = if idx + 1 < starts.len() {
            let next_sc = starts[idx + 1].0;
            if next_sc > 0 && data[next_sc - 1] == 0 { next_sc - 1 } else { next_sc }
        } else {
            data.len()
        };
        let _ = sc_start;
        if payload_start < end {
            nals.push(&data[payload_start..end]);
        }
    }
    nals
}

/// Is this CMAF fragment decode-independent — i.e. does it start with a keyframe?
///
/// The LL-HLS server rolls parts into parent segments at keyframe boundaries and marks the
/// leading part `INDEPENDENT=YES`; it recovers that purely from the fragment bytes (no
/// side-channel over the mesh). A sync sample has the `sample_is_non_sync_sample` bit
/// (0x0001_0000) clear — matching the flags [`FragmentBuilder`] writes (`0x0200_0000`
/// keyframe vs `0x0101_0000` other). Returns `false` if the box structure isn't the shape we
/// emit (be conservative — never claim independence we can't see).
pub fn fragment_is_independent(fragment: &[u8]) -> bool {
    fragment_info(fragment).map_or(false, |i| i.independent)
}

/// What the LL-HLS server needs to place a fragment (part) in a playlist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentInfo {
    /// Starts with a keyframe → a valid parent-segment boundary / `INDEPENDENT=YES` part.
    pub independent: bool,
    /// Total presentation duration in [`TIMESCALE`] (90kHz) ticks, summed from the `trun`.
    pub duration_ticks: u64,
}

/// Parse a CMAF fragment's `moof → traf → {tfhd, trun}` for its independence + duration.
/// `None` if the bytes aren't the fragment shape [`FragmentBuilder`] emits (e.g. an init
/// segment, or garbage) — callers treat that as "not a placeable part".
pub fn fragment_info(fragment: &[u8]) -> Option<FragmentInfo> {
    let moof = find_box(fragment, b"moof")?;
    let traf = find_box(moof, b"traf")?;
    // tfhd (full box): its flags decide which optional fields are present; the ones we may
    // fall back on are default-sample-duration (0x000008) and default-sample-flags (0x000020).
    let (mut default_dur, mut default_flags) = (None, None);
    if let Some(tfhd) = find_box(traf, b"tfhd") {
        if tfhd.len() >= 4 {
            let f = u32::from_be_bytes([0, tfhd[1], tfhd[2], tfhd[3]]);
            let mut p = 4 + 4; // full-box header + track_id
            if f & 0x000001 != 0 { p += 8; } // base-data-offset
            if f & 0x000002 != 0 { p += 4; } // sample-description-index
            if f & 0x000008 != 0 { if p + 4 <= tfhd.len() { default_dur = Some(u32::from_be_bytes([tfhd[p], tfhd[p+1], tfhd[p+2], tfhd[p+3]])); } p += 4; }
            if f & 0x000010 != 0 { p += 4; } // default-sample-size
            if f & 0x000020 != 0 && p + 4 <= tfhd.len() { default_flags = Some(u32::from_be_bytes([tfhd[p], tfhd[p+1], tfhd[p+2], tfhd[p+3]])); }
        }
    }
    let trun = find_box(traf, b"trun")?;
    if trun.len() < 8 { return None; }
    let f = u32::from_be_bytes([0, trun[1], trun[2], trun[3]]);
    let count = u32::from_be_bytes([trun[4], trun[5], trun[6], trun[7]]);
    let mut p = 8;
    if f & 0x000001 != 0 { p += 4; } // data-offset
    let first_sample_flags = if f & 0x000004 != 0 { // first-sample-flags overrides sample 1
        let v = (p + 4 <= trun.len()).then(|| u32::from_be_bytes([trun[p], trun[p+1], trun[p+2], trun[p+3]]));
        p += 4;
        v
    } else { None };
    // Per-sample record: the present fields, in this fixed order.
    let (has_dur, has_size, has_flags, has_cto) =
        (f & 0x000100 != 0, f & 0x000200 != 0, f & 0x000400 != 0, f & 0x000800 != 0);
    let rec = 4 * (has_dur as usize + has_size as usize + has_flags as usize + has_cto as usize);
    let mut duration_ticks: u64 = 0;
    let mut leading_flags = first_sample_flags.or(default_flags);
    for i in 0..count as usize {
        let base = p + i * rec;
        if base + rec > trun.len() { break; }
        let mut q = base;
        let dur = if has_dur { let d = u32::from_be_bytes([trun[q], trun[q+1], trun[q+2], trun[q+3]]); q += 4; d } else { default_dur.unwrap_or(0) };
        duration_ticks += dur as u64;
        if has_size { q += 4; }
        if has_flags {
            let sf = u32::from_be_bytes([trun[q], trun[q+1], trun[q+2], trun[q+3]]);
            if i == 0 && first_sample_flags.is_none() { leading_flags = Some(sf); }
        }
    }
    let independent = leading_flags.map_or(false, |fl| fl & 0x0001_0000 == 0);
    Some(FragmentInfo { independent, duration_ticks })
}

/// Return the body (after the 8-byte header) of the first top-level box of type `typ` in `buf`.
fn find_box<'a>(buf: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= buf.len() {
        let size = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        if size < 8 || i + size > buf.len() { return None; }
        if &buf[i + 4..i + 8] == typ {
            return Some(&buf[i + 8..i + size]);
        }
        i += size;
    }
    None
}

// ---- ISO-BMFF box helpers -------------------------------------------------------------

fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    out
}

fn full_bx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + body.len());
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..]); // low 3 bytes
    b.extend_from_slice(body);
    bx(typ, &b)
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

// ---- init segment: ftyp + moov --------------------------------------------------------

fn ftyp() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"iso5"); // major brand
    b.extend_from_slice(&0u32.to_be_bytes()); // minor version
    for brand in [b"iso5", b"iso6", b"mp41", b"avc1", b"cmfc"] {
        b.extend_from_slice(brand);
    }
    bx(b"ftyp", &b)
}

fn moov(p: &H264Params) -> Vec<u8> {
    let body = concat(&[mvhd(), trak(p), mvex()]);
    bx(b"moov", &body)
}

fn mvhd() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification time
    b.extend_from_slice(&TIMESCALE.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration (0 = fragmented)
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&UNITY_MATRIX);
    b.extend_from_slice(&[0u8; 24]); // pre-defined
    b.extend_from_slice(&2u32.to_be_bytes()); // next track id
    full_bx(b"mvhd", 0, 0, &b)
}

const UNITY_MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00,
];

fn trak(p: &H264Params) -> Vec<u8> {
    let body = concat(&[tkhd(p), mdia(p)]);
    bx(b"trak", &body)
}

fn tkhd(p: &H264Params) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation
    b.extend_from_slice(&0u32.to_be_bytes()); // modification
    b.extend_from_slice(&VIDEO_TRACK_ID.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    b.extend_from_slice(&0u16.to_be_bytes()); // volume (video = 0)
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&UNITY_MATRIX);
    b.extend_from_slice(&((p.width as u32) << 16).to_be_bytes()); // 16.16 fixed
    b.extend_from_slice(&((p.height as u32) << 16).to_be_bytes());
    full_bx(b"tkhd", 0, 0x7, &b) // flags: enabled | in-movie | in-preview
}

fn mdia(p: &H264Params) -> Vec<u8> {
    let body = concat(&[mdhd(), hdlr(), minf(p)]);
    bx(b"mdia", &body)
}

fn mdhd() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation
    b.extend_from_slice(&0u32.to_be_bytes()); // modification
    b.extend_from_slice(&TIMESCALE.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&0x55c4u16.to_be_bytes()); // language 'und'
    b.extend_from_slice(&0u16.to_be_bytes()); // pre-defined
    full_bx(b"mdhd", 0, 0, &b)
}

fn hdlr() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // pre-defined
    b.extend_from_slice(b"vide"); // handler type
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(b"VideoHandler\0");
    full_bx(b"hdlr", 0, 0, &b)
}

fn minf(p: &H264Params) -> Vec<u8> {
    let vmhd = full_bx(b"vmhd", 0, 1, &[0u8; 8]); // graphicsmode + opcolor
    let body = concat(&[vmhd, dinf(), stbl(p)]);
    bx(b"minf", &body)
}

fn dinf() -> Vec<u8> {
    // dref with one self-contained ('url ' flags=1) entry.
    let url = full_bx(b"url ", 0, 1, &[]);
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&1u32.to_be_bytes()); // entry count
    dref_body.extend_from_slice(&url);
    let dref = full_bx(b"dref", 0, 0, &dref_body);
    bx(b"dinf", &dref)
}

fn stbl(p: &H264Params) -> Vec<u8> {
    // Fragmented: the sample tables are all empty; samples live in moof/trun.
    let stsd = {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes()); // entry count
        body.extend_from_slice(&avc1(p));
        full_bx(b"stsd", 0, 0, &body)
    };
    let stts = full_bx(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = full_bx(b"stsc", 0, 0, &0u32.to_be_bytes());
    let stsz = full_bx(b"stsz", 0, 0, &[0u8; 8]); // sample size + count
    let stco = full_bx(b"stco", 0, 0, &0u32.to_be_bytes());
    bx(b"stbl", &concat(&[stsd, stts, stsc, stsz, stco]))
}

fn avc1(p: &H264Params) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0u8; 6]); // reserved
    b.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    b.extend_from_slice(&[0u8; 16]); // pre-defined + reserved (VisualSampleEntry)
    b.extend_from_slice(&p.width.to_be_bytes());
    b.extend_from_slice(&p.height.to_be_bytes());
    b.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horiz resolution 72dpi
    b.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vert resolution 72dpi
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&1u16.to_be_bytes()); // frame count
    b.extend_from_slice(&[0u8; 32]); // compressor name (empty)
    b.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    b.extend_from_slice(&0xffffu16.to_be_bytes()); // pre-defined = -1
    b.extend_from_slice(&avcc(p));
    bx(b"avc1", &b)
}

/// AVCDecoderConfigurationRecord (`avcC`) from the SPS/PPS.
fn avcc(p: &H264Params) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(1); // configurationVersion
    b.push(*p.sps.get(1).unwrap_or(&0x64)); // AVCProfileIndication
    b.push(*p.sps.get(2).unwrap_or(&0x00)); // profile_compatibility
    b.push(*p.sps.get(3).unwrap_or(&0x28)); // AVCLevelIndication
    b.push(0xff); // 6 bits reserved + lengthSizeMinusOne (3 → 4-byte lengths)
    b.push(0xe1); // 3 bits reserved + numOfSequenceParameterSets (1)
    b.extend_from_slice(&(p.sps.len() as u16).to_be_bytes());
    b.extend_from_slice(&p.sps);
    b.push(1); // numOfPictureParameterSets
    b.extend_from_slice(&(p.pps.len() as u16).to_be_bytes());
    b.extend_from_slice(&p.pps);
    bx(b"avcC", &b)
}

fn mvex() -> Vec<u8> {
    // trex: default sample description index 1; per-sample duration/size/flags come from trun.
    let mut trex_body = Vec::new();
    trex_body.extend_from_slice(&VIDEO_TRACK_ID.to_be_bytes());
    trex_body.extend_from_slice(&1u32.to_be_bytes()); // default sample description index
    trex_body.extend_from_slice(&0u32.to_be_bytes()); // default sample duration
    trex_body.extend_from_slice(&0u32.to_be_bytes()); // default sample size
    trex_body.extend_from_slice(&0u32.to_be_bytes()); // default sample flags
    let trex = full_bx(b"trex", 0, 0, &trex_body);
    bx(b"mvex", &trex)
}

// ---- media fragment: moof + mdat ------------------------------------------------------

fn moof(seq: Seq, base_decode_time: u64, aus: &[Au]) -> Vec<u8> {
    let mfhd = {
        let mut b = Vec::new();
        b.extend_from_slice(&((seq as u32).wrapping_add(1)).to_be_bytes()); // sequence >= 1
        full_bx(b"mfhd", 0, 0, &b)
    };
    // Build traf with a placeholder trun data_offset, then patch it once the moof size is known.
    let (traf_box, data_offset_pos_in_traf) = traf(base_decode_time, aus);
    let mut moof_body = concat(&[mfhd.clone(), traf_box]);
    let mut moof_box = bx(b"moof", &moof_body);
    // data_offset (in trun) is measured from the start of the moof box; mdat data begins at
    // moof_size + 8 (mdat header). Patch the 4-byte field in place.
    let data_offset = (moof_box.len() as u32) + 8;
    let pos = 8 + mfhd.len() + data_offset_pos_in_traf; // 8 = moof box header
    moof_box[pos..pos + 4].copy_from_slice(&data_offset.to_be_bytes());
    let _ = &mut moof_body;
    moof_box
}

/// Returns the `traf` box and the byte offset (within `traf`) of the `trun` `data_offset` field.
fn traf(base_decode_time: u64, aus: &[Au]) -> (Vec<u8>, usize) {
    // tfhd: default-base-is-moof (0x020000); no other defaults (per-sample values in trun).
    let mut tfhd_body = Vec::new();
    tfhd_body.extend_from_slice(&VIDEO_TRACK_ID.to_be_bytes());
    let tfhd = full_bx(b"tfhd", 0, 0x02_0000, &tfhd_body);

    // tfdt: 64-bit base media decode time.
    let tfdt = full_bx(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

    // trun: data-offset(0x1) + sample-duration(0x100) + sample-size(0x200) + sample-flags(0x400)
    //       + sample-composition-time-offset(0x800). Version 0 (all deltas non-negative).
    let flags = 0x0001 | 0x0100 | 0x0200 | 0x0400 | 0x0800;
    let mut trun_body = Vec::new();
    trun_body.extend_from_slice(&(aus.len() as u32).to_be_bytes()); // sample count
    let data_offset_field = trun_body.len(); // within trun body (after count)
    trun_body.extend_from_slice(&0u32.to_be_bytes()); // data offset (patched later)
    for au in aus {
        trun_body.extend_from_slice(&au.duration.to_be_bytes());
        trun_body.extend_from_slice(&(au.avcc.len() as u32).to_be_bytes());
        // sample flags: non-keyframes are non-sync + may depend on others.
        let sample_flags: u32 = if au.keyframe { 0x0200_0000 } else { 0x0101_0000 };
        trun_body.extend_from_slice(&sample_flags.to_be_bytes());
        trun_body.extend_from_slice(&0u32.to_be_bytes()); // composition time offset (0)
    }
    let trun = full_bx(b"trun", 0, flags, &trun_body);

    // Offset of the data_offset field within the traf box: traf header (8) + tfhd + tfdt
    // + trun header (8) + full-box (4) + sample count (4).
    let data_offset_pos = 8 + tfhd.len() + tfdt.len() + 8 + 4 + data_offset_field;
    let traf_box = bx(b"traf", &concat(&[tfhd, tfdt, trun]));
    (traf_box, data_offset_pos)
}

fn mdat(aus: &[Au]) -> Vec<u8> {
    let mut data = Vec::new();
    for au in aus {
        data.extend_from_slice(&au.avcc);
    }
    bx(b"mdat", &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> H264Params {
        // Minimal SPS/PPS payloads (byte 0 = NAL header 0x67/0x68). Enough to exercise the
        // box layout; real CSD comes from MediaCodec.
        H264Params { sps: vec![0x67, 0x64, 0x00, 0x28, 0xAC, 0xB4], pps: vec![0x68, 0xEE, 0x3C, 0x80], width: 1280, height: 720 }
    }

    /// Walk a box container and return (type, total_len) for each top-level box, asserting the
    /// declared sizes exactly tile the buffer — catches wrong lengths / offsets.
    fn top_boxes(buf: &[u8]) -> Vec<([u8; 4], usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
            let mut typ = [0u8; 4];
            typ.copy_from_slice(&buf[i + 4..i + 8]);
            assert!(size >= 8 && i + size <= buf.len(), "box {:?} size {size} overruns at {i}/{}", typ, buf.len());
            out.push((typ, size));
            i += size;
        }
        assert_eq!(i, buf.len(), "boxes do not tile the buffer exactly");
        out
    }

    #[test]
    fn init_segment_has_ftyp_and_moov() {
        let fb = FragmentBuilder::new(params());
        let init = fb.init_segment();
        let boxes = top_boxes(&init);
        assert_eq!(boxes[0].0, *b"ftyp");
        assert_eq!(boxes[1].0, *b"moov");
        assert_eq!(boxes.len(), 2);
    }

    #[test]
    fn annexb_converts_to_avcc_and_drops_parameter_sets() {
        // SPS(7) + PPS(8) + IDR(5): only the IDR should survive, length-prefixed.
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 1, 0x65, 0x01, 0x02, 0x03, // IDR
        ];
        let avcc = annexb_to_avcc(&stream);
        assert_eq!(&avcc[0..4], &4u32.to_be_bytes()); // one NAL of 4 bytes
        assert_eq!(avcc[4] & 0x1f, 5); // IDR
        assert_eq!(avcc.len(), 8);
    }

    #[test]
    fn fragment_emits_on_next_keyframe_and_boxes_are_valid() {
        let mut fb = FragmentBuilder::new(params());
        let idr = [0, 0, 0, 1, 0x65, 1, 2, 3, 4];
        let p = [0, 0, 0, 1, 0x41, 9, 8, 7];
        // First GOP: keyframe + a P-frame. No fragment closes yet.
        assert!(fb.push_au(&idr, 3000, true).is_none());
        assert!(fb.push_au(&p, 3000, false).is_none());
        // Second keyframe closes the first GOP.
        let seg = fb.push_au(&idr, 3000, true).expect("first GOP emitted");
        assert_eq!(seg.seq, 0);
        let boxes = top_boxes(&seg.bytes);
        assert_eq!(boxes[0].0, *b"moof");
        assert_eq!(boxes[1].0, *b"mdat");
        // Flush closes the second GOP as seq 1, with a monotonically advanced decode time.
        let seg2 = fb.flush().expect("second GOP emitted");
        assert_eq!(seg2.seq, 1);
    }

    #[test]
    fn ll_mode_closes_parts_within_a_gop_and_marks_independence() {
        // Part target 100ms = 9000 ticks; AUs are 4000 ticks each so parts don't fall on
        // AU boundaries — exercising both the duration-triggered close and a keyframe closing
        // a still-accumulating part.
        let mut fb = FragmentBuilder::new_ll(params(), 100);
        let idr = [0, 0, 0, 1, 0x65, 1, 2, 3, 4];
        let p = [0, 0, 0, 1, 0x41, 9, 8, 7];
        let dur = 4000;
        // GOP 0 opens on the keyframe; the part closes once ≥9000 ticks accumulate (3rd AU).
        assert!(fb.push_au(&idr, dur, true).is_none()); // 4000
        assert!(fb.push_au(&p, dur, false).is_none());  // 8000
        let part0 = fb.push_au(&p, dur, false).expect("part closes past 100ms"); // 12000 ≥ 9000
        assert_eq!(part0.seq, 0);
        assert!(fragment_is_independent(&part0.bytes), "leading part starts on the keyframe");
        // Duration is recovered from the trun: three 4000-tick AUs = 12000 ticks.
        assert_eq!(fragment_info(&part0.bytes).unwrap().duration_ticks, 12000);
        // Two more P-frames leave an 8000-tick part still open (below the 9000 target)…
        assert!(fb.push_au(&p, dur, false).is_none()); // 4000
        assert!(fb.push_au(&p, dur, false).is_none()); // 8000
        // …which the next keyframe closes as a P-only (non-independent) part before opening GOP 1.
        let part1 = fb.push_au(&idr, dur, true).expect("keyframe closes the pending part");
        assert_eq!(part1.seq, 1);
        assert!(!fragment_is_independent(&part1.bytes), "mid-GOP part holds only P-frames");
        // The trailing part begins with that keyframe → independent again.
        let part2 = fb.flush().expect("final part");
        assert_eq!(part2.seq, 2);
        assert!(fragment_is_independent(&part2.bytes), "part opening on the new keyframe is independent");
    }

    #[test]
    fn non_fragment_bytes_are_never_claimed_independent() {
        assert!(!fragment_is_independent(&[]));
        assert!(!fragment_is_independent(b"not a box at all, just text"));
        let fb = FragmentBuilder::new(params());
        // The init segment (ftyp+moov) has no moof → conservatively not independent.
        assert!(!fragment_is_independent(&fb.init_segment()));
    }

    /// End-to-end: generate a REAL H.264 stream with ffmpeg, mux it through
    /// `FragmentBuilder`, and assert ffmpeg decodes the result without error. This is the
    /// decode-level validation the structure tests can't give — a malformed box, avcC, or
    /// trun makes ffmpeg fail here. Ignored (needs ffmpeg); run in CI/dev where it's present.
    #[test]
    #[ignore = "needs ffmpeg: decode-validates the muxed CMAF output"]
    fn muxed_output_decodes_with_ffmpeg() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("fmp4-decode-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.h264");

        let gen = Command::new("ffmpeg")
            // `-bf 0`: no B-frames → decode order == presentation order, so composition-time
            // offsets are 0 (what the muxer assumes). Low-latency live encoders (Android
            // MediaCodec) run this way; B-frame reordering (nonzero cto) is a later addition.
            .args(["-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10", "-t", "2",
                   "-c:v", "libx264", "-g", "10", "-bf", "0", "-profile:v", "high", "-pix_fmt", "yuv420p",
                   "-f", "h264"])
            .arg(&src)
            .output();
        match gen {
            Ok(o) if o.status.success() => {}
            _ => { eprintln!("ffmpeg unavailable — skipping decode validation"); return; }
        }
        let raw = std::fs::read(&src).unwrap();

        // Split the elementary stream into access units: SPS/PPS feed the init; each VCL NAL
        // (slice types 1/5) is one frame's AU (x264 emits one slice per frame here).
        let mut params = H264Params { sps: Vec::new(), pps: Vec::new(), width: 320, height: 240 };
        let mut aus: Vec<(Vec<u8>, bool)> = Vec::new();
        for nal in iter_annexb_nals(&raw) {
            if nal.is_empty() { continue; }
            match nal[0] & 0x1f {
                7 => params.sps = nal.to_vec(),
                8 => params.pps = nal.to_vec(),
                t @ (1 | 5) => {
                    let mut au = vec![0, 0, 0, 1];
                    au.extend_from_slice(nal);
                    aus.push((au, t == 5));
                }
                _ => {}
            }
        }
        assert!(!params.sps.is_empty() && !params.pps.is_empty() && !aus.is_empty(), "no SPS/PPS/AUs parsed");

        let mut fb = FragmentBuilder::new(params);
        let mut out = fb.init_segment().to_vec();
        for (au, kf) in &aus {
            if let Some(seg) = fb.push_au(au, TIMESCALE / 10, *kf) {
                out.extend_from_slice(&seg.bytes);
            }
        }
        if let Some(seg) = fb.flush() {
            out.extend_from_slice(&seg.bytes);
        }
        let built = dir.join("built.mp4");
        std::fs::write(&built, &out).unwrap();

        let dec = Command::new("ffmpeg")
            .args(["-v", "error", "-i"]).arg(&built).args(["-f", "null", "-"])
            .output().unwrap();
        let stderr = String::from_utf8_lossy(&dec.stderr);
        assert!(dec.status.success() && stderr.trim().is_empty(),
            "ffmpeg could not decode the muxed CMAF:\n{stderr}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_decode_time_advances_across_fragments() {
        let mut fb = FragmentBuilder::new(params());
        let idr = [0, 0, 0, 1, 0x65, 1];
        fb.push_au(&idr, 3000, true);
        fb.push_au(&[0, 0, 0, 1, 0x41, 2], 3000, false);
        fb.push_au(&idr, 3000, true); // closes GOP 0 (two samples → 6000 ticks)
        assert_eq!(fb.base_decode_time, 6000);
    }
}
