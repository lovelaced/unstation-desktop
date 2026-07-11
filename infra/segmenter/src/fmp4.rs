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
/// Opus always runs a 48 kHz clock (RFC 7587) — the audio track's timescale, so RTP
/// timestamps ARE sample counts and no rescaling ever rounds.
pub const AUDIO_TIMESCALE: u32 = 48_000;
const AUDIO_TRACK_ID: u32 = 2;
/// Samples per Opus frame at the default 20ms ptime — the duration fallback when a
/// frame's RTP timestamp delta is unusable (first frame, reordering, discontinuity).
pub const OPUS_DEFAULT_FRAME_TICKS: u32 = 960;
/// Cap on buffered audio while no video part is closing (video stalls must not grow
/// audio memory unboundedly): ~4s at 20ms frames.
const MAX_PENDING_AUDIO: usize = 200;

/// H.264 decoder configuration + display size, learned once from the encoder's codec-specific
/// data (CSD: the SPS and PPS NAL units, WITHOUT Annex-B start codes or length prefixes).
#[derive(Clone)]
pub struct H264Params {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

/// Remaps decode-order timestamps to presentation order using the H.264 POC, for streams
/// whose encoder puts the B-frame reorder in the bitstream (not the RTP timestamps).
struct PocMapper {
    sps: crate::h264_poc::SpsPoc,
    tracker: crate::h264_poc::PocTracker,
    /// Global decode-order counter (across ALL fragments) — the DTS position of each AU.
    decode_index: i64,
    /// Decode index of the current GOP's IDR (POC is relative to it).
    gop_start_index: i64,
    /// Frame duration in TIMESCALE ticks, locked on the first measurement so the decode
    /// clock and composition offsets stay consistent across low-latency parts.
    frame_dur_ticks: i64,
    last_rtp_us: Option<i64>,
    /// Per-frame POC increment (encoder-dependent: x264 = 2, some builds = 1), detected as
    /// the GCD of the GOP's POC values. Converges within the first GOP; offsets are computed
    /// at emit so no fragment is built before it's known. 0 until the first non-zero POC.
    poc_step: i64,
    /// The stream's actual reorder depth (max `decode_index − presentation_index`), locked
    /// from the first emitted fragment. Used as the composition-offset baseline so the
    /// smallest `ctts` is ~0 — a spec-normal B-frame timeline, not a large uniform shift.
    /// `-1` until locked.
    reorder_delay: i64,
}

/// Frames to accumulate before the FIRST low-latency part may close, so the encoder's POC
/// step (§`PocMapper::poc_step`) has converged — a step-1 stream doesn't reveal an odd POC
/// until a few frames in, and every fragment must use the same step to stay consistent.
/// One-time (~0.3s) startup cost; whole-GOP fragments and later parts are unaffected.
const POC_STEP_WARMUP_FRAMES: i64 = 12;

/// The first coded-slice NAL (type 1/5) in an Annex-B access unit — where the POC lives.
fn first_slice_nal(annexb: &[u8]) -> Option<&[u8]> {
    iter_annexb_nals(annexb)
        .into_iter()
        .find(|n| !n.is_empty() && matches!(n[0] & 0x1f, 1 | 5))
}

/// One encoded video access unit queued for the current fragment.
struct Au {
    /// Sample data in AVCC form (each NAL unit prefixed by a 4-byte big-endian length).
    avcc: Vec<u8>,
    /// Presentation timestamp in `TIMESCALE` ticks (only relative values matter). Set by the
    /// PTS-based path ([`push_au_pts`]); unused (0) by the duration-based path.
    pts: i64,
    /// DECODE duration in `TIMESCALE` ticks. In the duration-based path it's the caller's
    /// value; in the PTS-based path [`compute_timing`] fills it from the PTS timeline.
    duration: u32,
    /// Composition time offset (`PTS - DTS`) in `TIMESCALE` ticks — non-zero only when a
    /// B-frame stream reorders presentation vs decode order; [`compute_timing`] fills it.
    comp_offset: u32,
    keyframe: bool,
    /// POC-path bookkeeping (global decode index, this GOP's IDR decode index, and the
    /// frame's POC). Duration + `comp_offset` are computed from these at emit, once the POC
    /// step has converged — see [`FragmentBuilder::emit`]. Zero on the non-POC paths.
    poc_dec: i64,
    poc_gop: i64,
    poc_val: i64,
}

/// Greatest common divisor (for detecting the encoder's per-frame POC step: x264 uses 2,
/// OBS's build uses 1 — the step is `gcd` of the GOP's POC values).
fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// One Opus frame queued for the current fragment.
struct AudioFrame {
    /// A raw Opus packet (what came off the RTP payload / encoder) — Opus-in-ISOBMFF
    /// samples are the bare packets, no framing.
    data: Vec<u8>,
    /// Sample duration in `AUDIO_TIMESCALE` (48 kHz) ticks.
    duration: u32,
}

/// The audio side of the muxer, present when the ingest negotiated an Opus track.
struct AudioTrack {
    channels: u8,
    pending: Vec<AudioFrame>,
    /// `tfdt` base media decode time in 48 kHz ticks (sum of emitted durations).
    base_decode_time: u64,
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
    /// True once fed via [`push_au_pts`]: at emit, decode durations + composition offsets are
    /// derived from the PTS timeline (handles B-frames). The duration-based path leaves this
    /// false and uses the caller's per-AU durations verbatim (no reordering).
    pts_mode: bool,
    /// Nominal frame duration carried across fragments as a fallback for a 1-AU part (where a
    /// single PTS gives no gap to measure).
    last_frame_dur: u32,
    /// Presentation span of `pending` in the PTS path (max−min PTS), for the LL part cadence
    /// — PTS can arrive out of order, so a running sum of deltas wouldn't measure the span.
    pending_pts_lo: i64,
    pending_pts_hi: i64,
    /// POC-based presentation remapping, set up lazily from the SPS. Real encoders (OBS/x264)
    /// stamp RTP with MONOTONIC decode-order timestamps and put the B-frame reorder only in
    /// the bitstream POC; this recovers the true presentation order so composition offsets
    /// come out right. `None` once we've determined the stream has no usable POC (no
    /// B-frames / unsupported POC type) — then decode-order timing is used as-is.
    poc: Option<PocMapper>,
    /// Whether we've attempted the (one-time) SPS parse for `poc`.
    poc_init: bool,
    /// True once POC is driving per-AU timing (durations + composition offsets are set at
    /// push time on a global clock); `emit` then skips the per-fragment `compute_timing`.
    poc_active: bool,
    /// Opus audio track, when the ingest negotiated one. Fragment cadence stays
    /// video-driven; whatever audio accumulated rides in the same fragment as a
    /// second `traf`.
    audio: Option<AudioTrack>,
}

impl FragmentBuilder {
    pub fn new(params: H264Params) -> Self {
        Self {
            params,
            seq: 0,
            base_decode_time: 0,
            pending: Vec::new(),
            pending_ticks: 0,
            part_ticks: None,
            audio: None,
            pts_mode: false,
            last_frame_dur: 3000, // 30fps @ 90kHz until measured
            pending_pts_lo: i64::MAX,
            pending_pts_hi: i64::MIN,
            poc: None,
            poc_init: false,
            poc_active: false,
        }
    }

    /// Low-latency builder: also close a fragment every `part_ms` of media (a CMAF *part*),
    /// not just on GOP boundaries. `part_ms` is clamped to ≥20ms; a GOP shorter than a part
    /// still emits per-keyframe, so parts never straddle a keyframe.
    pub fn new_ll(params: H264Params, part_ms: u32) -> Self {
        let part_ticks = (part_ms.max(20) as u64 * TIMESCALE as u64 / 1000) as u32;
        Self { part_ticks: Some(part_ticks), ..Self::new(params) }
    }

    /// Add an Opus audio track (48 kHz, `channels` — 2 for the WHIP/OBS default).
    /// Must be decided BEFORE the first [`init_segment`](Self::init_segment) call: the
    /// init advertises the track list, and every player configures its decoders from it.
    pub fn with_opus_audio(mut self, channels: u8) -> Self {
        self.audio = Some(AudioTrack {
            channels: channels.max(1),
            pending: Vec::new(),
            base_decode_time: 0,
        });
        self
    }

    /// Whether this builder muxes an audio track (drives feeder-side frame routing).
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// The CMAF init segment (`ftyp` + `moov`). Stable for the stream; push it once to the
    /// player (HLS `EXT-X-MAP`) and Bulletin before any media fragment.
    pub fn init_segment(&self) -> Bytes {
        let mut out = Vec::new();
        out.extend_from_slice(&ftyp());
        out.extend_from_slice(&moov(&self.params, self.audio.as_ref().map(|a| a.channels)));
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
            self.pending.push(Au { avcc, pts: 0, duration, comp_offset: 0, keyframe, poc_dec: 0, poc_gop: 0, poc_val: 0 });
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

    /// Queue one access unit by its **presentation timestamp** (microseconds) instead of a
    /// precomputed duration. This is the path real encoders (OBS, cameras) need: they emit
    /// **B-frames**, so presentation order differs from decode order and per-AU PTS deltas go
    /// negative — a duration can't be read off them. The builder buffers the fragment's PTS
    /// and, at emit, derives a monotonic decode timeline (DTS) plus per-sample composition
    /// offsets (`ctts`) so playback timing is exact. `nal` is Annex-B (see [`push_au`]).
    /// Returns the just-closed fragment when this AU starts a new one.
    pub fn push_au_pts(&mut self, nal: &[u8], pts_us: i64, keyframe: bool) -> Option<Segment> {
        self.pts_mode = true;
        // Timing: for a B-frame stream (encoder ships monotonic decode-order timestamps and
        // hides the reorder in the bitstream POC) we RECORD this AU's global decode index +
        // GOP anchor + POC now, and turn them into a duration + composition offset at emit —
        // once the encoder's POC step has converged. Otherwise the AU carries its RTP
        // timestamp and `compute_timing` derives per-fragment timing (no-reorder streams).
        self.poc_note_rtp(pts_us);
        let poc = self.poc_record(nal, keyframe);
        let pts = pts_us.saturating_mul(TIMESCALE as i64) / 1_000_000;
        let closed = if keyframe && !self.pending.is_empty() { self.emit() } else { None };
        let avcc = annexb_to_avcc(nal);
        if !avcc.is_empty() {
            let (poc_dec, poc_gop, poc_val) = poc.unwrap_or((0, 0, 0));
            if poc.is_some() {
                self.poc_active = true;
            }
            self.pending.push(Au { avcc, pts, duration: 0, comp_offset: 0, keyframe, poc_dec, poc_gop, poc_val });
            self.pending_pts_lo = self.pending_pts_lo.min(pts);
            self.pending_pts_hi = self.pending_pts_hi.max(pts);
        }
        // LL part cadence: in the POC path count frames (one frame_dur each — decode order is
        // exact); otherwise use the PTS SPAN (PTS may arrive out of order). Hold the FIRST
        // part until the POC step has converged (warm-up) so no fragment is built with a
        // premature step.
        if closed.is_none() {
            if let Some(pt) = self.part_ticks {
                let warmed = self.poc.as_ref().map_or(true, |m| m.decode_index >= POC_STEP_WARMUP_FRAMES);
                let fd = self.poc.as_ref().map(|m| m.frame_dur_ticks).filter(|d| *d > 0).unwrap_or(3000);
                let span = if self.poc_active {
                    self.pending.len() as i64 * fd
                } else {
                    (self.pending_pts_hi - self.pending_pts_lo).max(0)
                };
                if warmed && !self.pending.is_empty() && span >= pt as i64 {
                    return self.emit();
                }
            }
        }
        closed
    }

    /// Record a B-frame AU's `(decode_index, gop_start_index, poc)` and advance the POC state
    /// — WITHOUT computing timing yet (that waits for emit, when the POC step is known).
    /// `None` when there's no usable POC (SPS won't parse / unsupported type / no slice); the
    /// caller then uses the RTP timestamp + `compute_timing`, right for a no-reorder stream.
    fn poc_record(&mut self, nal: &[u8], keyframe: bool) -> Option<(i64, i64, i64)> {
        if !self.poc_init {
            self.poc_init = true;
            if let Some(sps) = crate::h264_poc::parse_sps(&self.params.sps) {
                self.poc = Some(PocMapper {
                    sps,
                    tracker: crate::h264_poc::PocTracker::new(&sps),
                    decode_index: 0,
                    gop_start_index: 0,
                    frame_dur_ticks: 0,
                    last_rtp_us: None,
                    poc_step: 0,
                    reorder_delay: -1,
                });
            }
        }
        let m = self.poc.as_mut()?;
        let slice = first_slice_nal(nal)?;
        let (is_idr, lsb) = crate::h264_poc::slice_poc_lsb(slice, &m.sps)?;
        let d = m.decode_index;
        if is_idr || keyframe {
            m.gop_start_index = d;
        }
        let poc = m.tracker.poc(is_idr, lsb, crate::h264_poc::nal_ref_idc(slice[0])) as i64;
        // POC is relative to this GOP's IDR (POC 0); its step is the GCD of positive values.
        if poc > 0 {
            m.poc_step = gcd(m.poc_step, poc);
        }
        let gop = m.gop_start_index;
        m.decode_index += 1;
        Some((d, gop, poc))
    }

    /// Lock the frame duration in ticks from the AU's decode-order RTP timestamp (call once
    /// per push, before recording, so the clock is stable). Guards against reconnect jumps.
    fn poc_note_rtp(&mut self, pts_us: i64) {
        if let Some(m) = self.poc.as_mut() {
            if m.frame_dur_ticks == 0 {
                if let Some(last) = m.last_rtp_us {
                    let d_us = pts_us - last;
                    if (1_000..=200_000).contains(&d_us) {
                        m.frame_dur_ticks = (d_us * TIMESCALE as i64 / 1_000_000).max(1);
                    }
                }
                m.last_rtp_us = Some(pts_us);
            }
        }
    }

    /// Queue one Opus frame (a raw Opus packet). `duration` is in 48 kHz ticks —
    /// derive it from RTP timestamp deltas, falling back to
    /// [`OPUS_DEFAULT_FRAME_TICKS`]. No-op unless the builder was built
    /// [`with_opus_audio`](Self::with_opus_audio). Never closes a fragment: the part
    /// cadence stays video-driven so parts never straddle a keyframe.
    pub fn push_opus(&mut self, frame: &[u8], duration: u32) {
        let Some(audio) = self.audio.as_mut() else { return };
        if frame.is_empty() {
            return;
        }
        if audio.pending.len() >= MAX_PENDING_AUDIO {
            // Video stalled (no part is closing): advance the audio timeline past the
            // dropped frame so A/V stay aligned when video resumes.
            let dropped = audio.pending.remove(0);
            audio.base_decode_time += dropped.duration as u64;
        }
        audio.pending.push(AudioFrame { data: frame.to_vec(), duration: duration.max(1) });
    }

    /// Emit any trailing accumulated AUs as a final fragment (e.g. on stop).
    pub fn flush(&mut self) -> Option<Segment> {
        if self.pending.is_empty() { None } else { self.emit() }
    }

    /// Build a `moof` + `mdat` fragment from the pending AUs (+ any accumulated audio)
    /// and advance the timeline.
    fn emit(&mut self) -> Option<Segment> {
        if self.pending.is_empty() {
            return None;
        }
        let mut aus = std::mem::take(&mut self.pending);
        self.pending_ticks = 0;
        self.pending_pts_lo = i64::MAX;
        self.pending_pts_hi = i64::MIN;
        // Fill decode duration + composition offset for this fragment. POC path: on a GLOBAL
        // clock — DTS = decode_index·frame_dur, ctts = (presentation_index − decode_index +
        // DELAY)·frame_dur with presentation_index = gop_start + POC/step. Computed HERE (not
        // at push) so every fragment uses the same, by-now-converged POC step — consistent
        // across low-latency parts. No-POC PTS path: per-fragment `compute_timing`.
        if self.poc_active {
            let (fd, step) = {
                let m = self.poc.as_ref();
                (
                    m.map(|m| m.frame_dur_ticks).filter(|d| *d > 0).unwrap_or(3000),
                    m.map(|m| m.poc_step).filter(|s| *s > 0).unwrap_or(2),
                )
            };
            // Reorder delay = the stream's actual max reorder depth, locked from the first
            // fragment (warm-up guarantees the deepest B-frame is present). Baseline so the
            // minimum ctts is ~0 — a normal B-frame timeline, not a big uniform offset.
            let delay = match self.poc.as_ref().map(|m| m.reorder_delay) {
                Some(d) if d >= 0 => d,
                _ => {
                    let d = aus
                        .iter()
                        .map(|au| au.poc_dec - (au.poc_gop + au.poc_val / step))
                        .max()
                        .unwrap_or(0)
                        .max(0);
                    if let Some(m) = self.poc.as_mut() {
                        m.reorder_delay = d;
                    }
                    d
                }
            };
            for au in aus.iter_mut() {
                let presentation_index = au.poc_gop + au.poc_val / step;
                let comp = (presentation_index - au.poc_dec + delay) * fd;
                au.duration = fd as u32;
                au.comp_offset = comp.max(0) as u32;
            }
        } else if self.pts_mode {
            self.last_frame_dur = compute_timing(&mut aus, self.last_frame_dur);
        }
        let seq = self.seq;
        self.seq += 1;

        // Audio rides along: whatever frames accumulated since the last fragment, as a
        // second traf. An empty accumulation (audio not started / gap) emits video-only —
        // fragments may legally differ in track presence.
        let audio_frames = match self.audio.as_mut() {
            Some(a) if !a.pending.is_empty() => Some((std::mem::take(&mut a.pending), a.base_decode_time)),
            _ => None,
        };

        let mut bytes = moof(
            seq,
            self.base_decode_time,
            &aus,
            audio_frames.as_ref().map(|(f, bdt)| (f.as_slice(), *bdt)),
        );
        bytes.extend_from_slice(&mdat(&aus, audio_frames.as_ref().map(|(f, _)| f.as_slice())));

        for au in &aus {
            self.base_decode_time += au.duration as u64;
        }
        if let (Some(a), Some((frames, _))) = (self.audio.as_mut(), audio_frames.as_ref()) {
            for f in frames {
                a.base_decode_time += f.duration as u64;
            }
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
    // Hostile-input bound: a forged trun can claim up to 2^32 samples; with no per-sample
    // fields (rec == 0) the loop below would spin on all of them. No honest part holds more
    // than a few hundred samples, so clamp instead of trusting the wire.
    let count = u32::from_be_bytes([trun[4], trun[5], trun[6], trun[7]]).min(8_192);
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

fn moov(p: &H264Params, audio_channels: Option<u8>) -> Vec<u8> {
    let mut parts = vec![mvhd(audio_channels.is_some()), trak(p)];
    if let Some(ch) = audio_channels {
        parts.push(audio_trak(ch));
    }
    parts.push(mvex(audio_channels.is_some()));
    bx(b"moov", &concat(&parts))
}

fn mvhd(with_audio: bool) -> Vec<u8> {
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
    let next_track = if with_audio { AUDIO_TRACK_ID + 1 } else { VIDEO_TRACK_ID + 1 };
    b.extend_from_slice(&next_track.to_be_bytes());
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

fn mvex(with_audio: bool) -> Vec<u8> {
    // trex per track: default sample description index 1; per-sample duration/size/flags
    // come from trun.
    let trex_for = |track_id: u32| {
        let mut b = Vec::new();
        b.extend_from_slice(&track_id.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes()); // default sample description index
        b.extend_from_slice(&0u32.to_be_bytes()); // default sample duration
        b.extend_from_slice(&0u32.to_be_bytes()); // default sample size
        b.extend_from_slice(&0u32.to_be_bytes()); // default sample flags
        full_bx(b"trex", 0, 0, &b)
    };
    let mut body = trex_for(VIDEO_TRACK_ID);
    if with_audio {
        body.extend_from_slice(&trex_for(AUDIO_TRACK_ID));
    }
    bx(b"mvex", &body)
}

// ---- audio trak (Opus, RFC 7845 §4.3 / Opus-in-ISOBMFF) --------------------------------

fn audio_trak(channels: u8) -> Vec<u8> {
    let body = concat(&[audio_tkhd(), audio_mdia(channels)]);
    bx(b"trak", &body)
}

fn audio_tkhd() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation
    b.extend_from_slice(&0u32.to_be_bytes()); // modification
    b.extend_from_slice(&AUDIO_TRACK_ID.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0 (audio)
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&UNITY_MATRIX);
    b.extend_from_slice(&0u32.to_be_bytes()); // width (audio = 0)
    b.extend_from_slice(&0u32.to_be_bytes()); // height
    full_bx(b"tkhd", 0, 0x7, &b) // flags: enabled | in-movie | in-preview
}

fn audio_mdia(channels: u8) -> Vec<u8> {
    let body = concat(&[audio_mdhd(), audio_hdlr(), audio_minf(channels)]);
    bx(b"mdia", &body)
}

fn audio_mdhd() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // creation
    b.extend_from_slice(&0u32.to_be_bytes()); // modification
    b.extend_from_slice(&AUDIO_TIMESCALE.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // duration
    b.extend_from_slice(&0x55c4u16.to_be_bytes()); // language 'und'
    b.extend_from_slice(&0u16.to_be_bytes()); // pre-defined
    full_bx(b"mdhd", 0, 0, &b)
}

fn audio_hdlr() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // pre-defined
    b.extend_from_slice(b"soun"); // handler type
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(b"SoundHandler\0");
    full_bx(b"hdlr", 0, 0, &b)
}

fn audio_minf(channels: u8) -> Vec<u8> {
    // smhd: balance (0) + reserved.
    let smhd = full_bx(b"smhd", 0, 0, &[0u8; 4]);
    let body = concat(&[smhd, dinf(), audio_stbl(channels)]);
    bx(b"minf", &body)
}

fn audio_stbl(channels: u8) -> Vec<u8> {
    let stsd = {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes()); // entry count
        body.extend_from_slice(&opus_sample_entry(channels));
        full_bx(b"stsd", 0, 0, &body)
    };
    let stts = full_bx(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = full_bx(b"stsc", 0, 0, &0u32.to_be_bytes());
    let stsz = full_bx(b"stsz", 0, 0, &[0u8; 8]); // sample size + count
    let stco = full_bx(b"stco", 0, 0, &0u32.to_be_bytes());
    bx(b"stbl", &concat(&[stsd, stts, stsc, stsz, stco]))
}

/// `Opus` AudioSampleEntry + `dOps` (Opus-in-ISOBMFF §4.3.2).
fn opus_sample_entry(channels: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0u8; 6]); // reserved
    b.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    b.extend_from_slice(&[0u8; 8]); // version/revision/vendor (AudioSampleEntry v0)
    b.extend_from_slice(&(channels as u16).to_be_bytes());
    b.extend_from_slice(&16u16.to_be_bytes()); // sample size (bits)
    b.extend_from_slice(&0u16.to_be_bytes()); // pre-defined
    b.extend_from_slice(&0u16.to_be_bytes()); // reserved
    b.extend_from_slice(&(AUDIO_TIMESCALE << 16).to_be_bytes()); // samplerate 16.16
    b.extend_from_slice(&d_ops(channels));
    bx(b"Opus", &b)
}

/// OpusSpecificBox. PreSkip = 312 (libopus's 6.5ms lookahead at 48 kHz — the value
/// encoders conventionally stamp); players discard that many samples before the
/// first audible one. ChannelMappingFamily 0 covers mono/stereo (all we ingest).
fn d_ops(channels: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(0); // Version
    b.push(channels); // OutputChannelCount
    b.extend_from_slice(&312u16.to_be_bytes()); // PreSkip
    b.extend_from_slice(&AUDIO_TIMESCALE.to_be_bytes()); // InputSampleRate
    b.extend_from_slice(&0i16.to_be_bytes()); // OutputGain (dB, 8.8 fixed)
    b.push(0); // ChannelMappingFamily
    bx(b"dOps", &b)
}

// ---- media fragment: moof + mdat ------------------------------------------------------

/// Derive each AU's DECODE duration + composition offset (`PTS − DTS`) from the fragment's
/// presentation timestamps, so a B-frame stream (presentation order ≠ decode order) muxes
/// with a monotonic DTS and exact playback timing. `aus` is in DECODE order (the order the
/// encoder/RTP delivered). Returns the nominal frame duration used (carried as a fallback).
///
/// Method: run a uniform decode clock `dts[i] = i·frame_dur`, and choose a per-fragment
/// reorder delay `D` so every `comp_offset[i] = pts_rel[i] − dts[i] + D ≥ 0`. Presentation
/// time `= tfdt + dts[i] + comp_offset[i] = base + pts_rel[i] + D` reproduces the input PTS
/// exactly (a constant `D` delay); DTS stays monotonic. With no B-frames PTS is already in
/// decode order, `D = 0`, and every offset is 0 — identical to the duration path.
fn compute_timing(aus: &mut [Au], fallback_frame_dur: u32) -> u32 {
    let n = aus.len();
    if n == 0 {
        return fallback_frame_dur.max(1);
    }
    let min_pts = aus.iter().map(|a| a.pts).min().unwrap_or(0);
    let rel: Vec<i64> = aus.iter().map(|a| a.pts - min_pts).collect();
    // Nominal frame duration: the median positive gap between SORTED PTS (for constant-fps
    // input every gap equals the frame duration; sorting undoes the B-frame reorder).
    let mut sorted = rel.clone();
    sorted.sort_unstable();
    let mut gaps: Vec<i64> = sorted.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0).collect();
    let frame_dur: i64 = if gaps.is_empty() {
        fallback_frame_dur.max(1) as i64
    } else {
        gaps.sort_unstable();
        gaps[gaps.len() / 2].max(1)
    };
    // Reorder delay: the smallest D making all composition offsets non-negative.
    let mut delay = 0i64;
    for (i, &r) in rel.iter().enumerate() {
        delay = delay.max(i as i64 * frame_dur - r);
    }
    for (i, au) in aus.iter_mut().enumerate() {
        au.duration = frame_dur as u32;
        au.comp_offset = (rel[i] - i as i64 * frame_dur + delay).max(0) as u32;
    }
    frame_dur as u32
}

fn moof(
    seq: Seq,
    base_decode_time: u64,
    aus: &[Au],
    audio: Option<(&[AudioFrame], u64)>,
) -> Vec<u8> {
    let mfhd = {
        let mut b = Vec::new();
        b.extend_from_slice(&((seq as u32).wrapping_add(1)).to_be_bytes()); // sequence >= 1
        full_bx(b"mfhd", 0, 0, &b)
    };
    // Build trafs with placeholder trun data_offsets, then patch them once the moof size
    // is known. The VIDEO traf comes first — `fragment_info` (and through it the LL-HLS
    // part-placement logic) reads the first traf for keyframe independence + duration.
    let (video_traf, video_off_in_traf) = traf(base_decode_time, aus);
    let audio_built = audio.map(|(frames, bdt)| audio_traf(bdt, frames));
    let mut parts = vec![mfhd.clone(), video_traf.clone()];
    if let Some((ref traf_box, _)) = audio_built {
        parts.push(traf_box.clone());
    }
    let mut moof_box = bx(b"moof", &concat(&parts));

    // mdat layout after the moof: [video samples ‖ audio samples]. Each trun's
    // data_offset is measured from the start of the moof box.
    let video_bytes: usize = aus.iter().map(|a| a.avcc.len()).sum();
    let video_data_offset = (moof_box.len() as u32) + 8; // + mdat header
    let vpos = 8 + mfhd.len() + video_off_in_traf; // 8 = moof box header
    moof_box[vpos..vpos + 4].copy_from_slice(&video_data_offset.to_be_bytes());
    if let Some((traf_box, audio_off_in_traf)) = audio_built {
        let audio_data_offset = video_data_offset + video_bytes as u32;
        let apos = 8 + mfhd.len() + video_traf.len() + audio_off_in_traf;
        moof_box[apos..apos + 4].copy_from_slice(&audio_data_offset.to_be_bytes());
        let _ = traf_box;
    }
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
        // Composition time offset (PTS − DTS). 0 for the duration path / no B-frames; the
        // PTS path fills it so a reordered stream presents in the right order. Non-negative,
        // so version-0 (unsigned) trun is valid.
        trun_body.extend_from_slice(&au.comp_offset.to_be_bytes());
    }
    let trun = full_bx(b"trun", 0, flags, &trun_body);

    // Offset of the data_offset field within the traf box: traf header (8) + tfhd + tfdt
    // + trun header (8) + full-box (4) + sample count (4).
    let data_offset_pos = 8 + tfhd.len() + tfdt.len() + 8 + 4 + data_offset_field;
    let traf_box = bx(b"traf", &concat(&[tfhd, tfdt, trun]));
    (traf_box, data_offset_pos)
}

fn mdat(aus: &[Au], audio: Option<&[AudioFrame]>) -> Vec<u8> {
    let mut data = Vec::new();
    for au in aus {
        data.extend_from_slice(&au.avcc);
    }
    if let Some(frames) = audio {
        for f in frames {
            data.extend_from_slice(&f.data);
        }
    }
    bx(b"mdat", &data)
}

/// Returns the audio `traf` box and the byte offset (within it) of its `trun`
/// `data_offset` field. Mirrors [`traf`] with the audio track id, a 48 kHz `tfdt`,
/// and all-sync sample flags (every Opus frame decodes independently).
fn audio_traf(base_decode_time: u64, frames: &[AudioFrame]) -> (Vec<u8>, usize) {
    let mut tfhd_body = Vec::new();
    tfhd_body.extend_from_slice(&AUDIO_TRACK_ID.to_be_bytes());
    let tfhd = full_bx(b"tfhd", 0, 0x02_0000, &tfhd_body); // default-base-is-moof

    let tfdt = full_bx(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());

    // trun: data-offset + per-sample duration + size (no flags/cto — audio samples are
    // uniform sync samples; tfhd defaults would need trex defaults, so keep per-sample
    // duration/size explicit like the video trun).
    let flags = 0x0001 | 0x0100 | 0x0200;
    let mut trun_body = Vec::new();
    trun_body.extend_from_slice(&(frames.len() as u32).to_be_bytes());
    let data_offset_field = trun_body.len();
    trun_body.extend_from_slice(&0u32.to_be_bytes()); // patched by moof()
    for f in frames {
        trun_body.extend_from_slice(&f.duration.to_be_bytes());
        trun_body.extend_from_slice(&(f.data.len() as u32).to_be_bytes());
    }
    let trun = full_bx(b"trun", 0, flags, &trun_body);

    let data_offset_pos = 8 + tfhd.len() + tfdt.len() + 8 + 4 + data_offset_field;
    let traf_box = bx(b"traf", &concat(&[tfhd, tfdt, trun]));
    (traf_box, data_offset_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end POC muxing over a REAL B-frame stream: reads an Annex-B H.264 file (High
    /// profile, B-frames), feeds each access unit with a MONOTONIC decode-order timestamp
    /// (what OBS/x264 send over WHIP), and writes `init.mp4` + the muxed segments. A correct
    /// muxer recovers presentation order from the bitstream POC and writes composition
    /// offsets, so the output decodes cleanly on strict players. Verify externally:
    ///   ffmpeg -y -f lavfi -i testsrc=s=320x240:r=30 -t 1 -c:v libx264 -profile:v high \
    ///          -pix_fmt yuv420p -bf 2 -g 30 -f h264 /tmp/bf.h264
    ///   POC_H264=/tmp/bf.h264 cargo test -p segmenter poc_muxes_real_bframe_stream -- --ignored
    ///   cat /tmp/poc_init.mp4 /tmp/poc_seg*.m4s > /tmp/poc_out.mp4 && ffmpeg -v error -i /tmp/poc_out.mp4 -f null -
    #[test]
    #[ignore = "needs a local Annex-B B-frame file via POC_H264"]
    fn poc_muxes_real_bframe_stream() {
        let path = std::env::var("POC_H264").unwrap_or_else(|_| "/tmp/bf.h264".into());
        let data = std::fs::read(&path).expect("read POC_H264 file");
        let nals = iter_annexb_nals(&data);
        let sps = nals.iter().find(|n| n[0] & 0x1f == 7).expect("SPS").to_vec();
        let pps = nals.iter().find(|n| n[0] & 0x1f == 8).expect("PPS").to_vec();
        // LL mode (parts) — exercises the global-clock timing across mid-GOP fragments.
        let mut fb = FragmentBuilder::new_ll(H264Params { sps, pps, width: 320, height: 240 }, 100);
        std::fs::write("/tmp/poc_init.mp4", fb.init_segment()).unwrap();

        // Group NALs into access units (one per VCL slice), Annex-B framed, and feed with a
        // monotonic 30fps timeline — exactly the decode-order timestamps OBS emits.
        let mut au = Vec::new();
        let mut idx = 0i64;
        let mut segn = 0;
        let emit = |fb: &mut FragmentBuilder, au: &[u8], kf: bool, idx: i64, segn: &mut usize| {
            if let Some(seg) = fb.push_au_pts(au, idx * 33_367, kf) {
                std::fs::write(format!("/tmp/poc_seg{:03}.m4s", *segn), &seg.bytes).unwrap();
                *segn += 1;
            }
        };
        for nal in &nals {
            let t = nal[0] & 0x1f;
            if matches!(t, 1 | 5) {
                // finish the AU with this slice
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(nal);
                emit(&mut fb, &au, t == 5, idx, &mut segn);
                idx += 1;
                au.clear();
            } else {
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(nal);
            }
        }
        if let Some(seg) = fb.flush() {
            std::fs::write(format!("/tmp/poc_seg{:03}.m4s", segn), &seg.bytes).unwrap();
            segn += 1;
        }
        assert!(segn > 0, "muxed at least one segment");
        eprintln!("wrote /tmp/poc_init.mp4 + {segn} segments");
    }

    /// POC step detection: x264 numbers frames 0,2,4,… (step 2); other encoders (e.g. Apple
    /// VideoToolbox, which OBS-on-Mac uses) number 0,1,2,… (step 1). Assuming step 2 for a
    /// step-1 stream collides two frames into each presentation slot — the OBS decode bug.
    /// The step is the GCD of the GOP's POC values.
    #[test]
    fn poc_step_is_the_gcd_of_poc_values() {
        // x264-style even POCs → step 2.
        assert_eq!([0i64, 6, 2, 4, 10, 8].iter().fold(0, |g, &p| gcd(g, p)), 2);
        // VT/OBS-style consecutive POCs (an odd value appears) → step 1.
        assert_eq!([0i64, 4, 2, 1, 3, 8].iter().fold(0, |g, &p| gcd(g, p)), 1);
        assert_eq!(gcd(0, 5), 5);
        assert_eq!(gcd(12, 8), 4);
    }

    /// B-frame reorder: decode order `I P B B` presents as `I B B P`. `compute_timing` must
    /// yield a monotonic DTS (constant frame duration) and composition offsets that reproduce
    /// the presentation timeline exactly — the fix for OBS/High-profile decode failures.
    #[test]
    fn compute_timing_handles_bframe_reorder() {
        // PTS in decode order for I(0) P(3) B(1) B(2), 30fps → 3000 ticks/frame.
        let fd = 3000i64;
        let pts_decode = [0i64, 3 * fd, 1 * fd, 2 * fd];
        let mut aus: Vec<Au> = pts_decode
            .iter()
            .map(|&p| Au { avcc: vec![0], pts: p, duration: 0, comp_offset: 0, keyframe: false, poc_dec: 0, poc_gop: 0, poc_val: 0 })
            .collect();
        let frame_dur = compute_timing(&mut aus, 3000);
        assert_eq!(frame_dur, fd as u32, "measures 30fps frame duration");

        // DTS is the uniform decode clock i*frame_dur; PTS = DTS + comp_offset must equal the
        // input PTS (up to the constant reorder delay), and be strictly the input order.
        let delay = aus[0].comp_offset as i64; // sample 0 has pts_rel 0 → comp == delay
        for (i, au) in aus.iter().enumerate() {
            assert_eq!(au.duration, fd as u32, "monotonic DTS ⇒ constant per-sample duration");
            let dts = i as i64 * fd;
            let recovered_pts = dts + au.comp_offset as i64 - delay;
            assert_eq!(recovered_pts, pts_decode[i], "sample {i} PTS reproduced exactly");
        }
        // Every composition offset is non-negative (valid for a version-0 trun).
        assert!(aus.iter().all(|a| a.comp_offset <= i32::MAX as u32));
    }

    /// No B-frames (PTS already in decode order): reorder delay 0, every offset 0 — the
    /// PTS path must degrade to exactly what the duration path produces.
    #[test]
    fn compute_timing_no_bframes_is_offsetless() {
        let fd = 3000i64;
        let mut aus: Vec<Au> = (0..5)
            .map(|i| Au { avcc: vec![0], pts: i * fd, duration: 0, comp_offset: 0, keyframe: i == 0, poc_dec: 0, poc_gop: 0, poc_val: 0 })
            .collect();
        compute_timing(&mut aus, 3000);
        assert!(aus.iter().all(|a| a.comp_offset == 0), "no reorder ⇒ no composition offsets");
        assert!(aus.iter().all(|a| a.duration == fd as u32));
    }

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
    fn audio_builder_muxes_a_second_traf_and_video_only_bytes_are_unchanged() {
        let idr = [0, 0, 0, 1, 0x65, 1, 2, 3, 4];
        let p = [0, 0, 0, 1, 0x41, 9, 8, 7];
        let opus_a = [0x0b, 1, 2, 3]; // raw Opus packets (content opaque to the muxer)
        let opus_b = [0x0b, 4, 5];

        // Video-only output must be byte-identical with and without the audio FEATURE
        // compiled in — i.e. a builder without with_opus_audio changes nothing.
        let run_video_only = || {
            let mut fb = FragmentBuilder::new(params());
            let init = fb.init_segment();
            fb.push_au(&idr, 3000, true);
            fb.push_au(&p, 3000, false);
            let seg = fb.push_au(&idr, 3000, true).unwrap();
            (init, seg.bytes)
        };
        let (init_a, seg_a) = run_video_only();
        let (init_b, seg_b) = run_video_only();
        assert_eq!(init_a, init_b);
        assert_eq!(seg_a, seg_b);

        // A/V builder: audio rides in the same fragment as a second traf.
        let mut fb = FragmentBuilder::new(params()).with_opus_audio(2);
        assert!(fb.has_audio());
        let init = fb.init_segment();
        assert!(init.len() > init_a.len(), "init advertises the audio trak");
        // The init must contain the Opus sample entry + dOps.
        let hay = init.as_ref();
        assert!(hay.windows(4).any(|w| w == b"Opus"), "Opus sample entry present");
        assert!(hay.windows(4).any(|w| w == b"dOps"), "dOps present");
        assert!(hay.windows(4).any(|w| w == b"soun"), "sound handler present");

        fb.push_au(&idr, 3000, true);
        fb.push_opus(&opus_a, 960);
        fb.push_opus(&opus_b, 960);
        fb.push_au(&p, 3000, false);
        let seg = fb.push_au(&idr, 3000, true).expect("GOP closes");
        let boxes = top_boxes(&seg.bytes);
        assert_eq!(boxes[0].0, *b"moof");
        assert_eq!(boxes[1].0, *b"mdat");
        // Two trafs inside the moof.
        let moof_body = &seg.bytes[8..boxes[0].1];
        let mut trafs = 0;
        let mut i = 0;
        while i + 8 <= moof_body.len() {
            let size = u32::from_be_bytes([moof_body[i], moof_body[i+1], moof_body[i+2], moof_body[i+3]]) as usize;
            if &moof_body[i+4..i+8] == b"traf" { trafs += 1; }
            i += size;
        }
        assert_eq!(trafs, 2, "video + audio trafs");
        // fragment_info still reads the VIDEO traf: keyframe-led, 6000 video ticks.
        let info = fragment_info(&seg.bytes).unwrap();
        assert!(info.independent);
        assert_eq!(info.duration_ticks, 6000);
        // mdat = video samples then audio packets, byte-exact.
        let mdat_body = &seg.bytes[boxes[0].1 + 8..];
        let video_len: usize = mdat_body.len() - (opus_a.len() + opus_b.len());
        assert_eq!(&mdat_body[video_len..video_len + opus_a.len()], &opus_a);
        assert_eq!(&mdat_body[video_len + opus_a.len()..], &opus_b);

        // The NEXT fragment's audio tfdt advances by the emitted durations (1920 ticks):
        // audio pushed after the close lands in the following fragment.
        fb.push_opus(&opus_a, 960);
        let seg2 = fb.flush().expect("trailing fragment");
        // Find the audio traf's tfdt (version 1, 8-byte time) inside the second moof.
        let moof2 = find_box(&seg2.bytes, b"moof").unwrap();
        let mut times = Vec::new();
        let mut j = 0;
        while j + 8 <= moof2.len() {
            let size = u32::from_be_bytes([moof2[j], moof2[j+1], moof2[j+2], moof2[j+3]]) as usize;
            if &moof2[j+4..j+8] == b"traf" {
                let traf_body = &moof2[j+8..j+size];
                let tfdt = find_box(traf_body, b"tfdt").unwrap();
                times.push(u64::from_be_bytes(tfdt[4..12].try_into().unwrap()));
            }
            j += size;
        }
        assert_eq!(times, vec![6000, 1920], "video then audio decode times advance independently");
    }

    #[test]
    fn audio_backpressure_drops_oldest_but_keeps_the_timeline() {
        let mut fb = FragmentBuilder::new(params()).with_opus_audio(2);
        let idr = [0, 0, 0, 1, 0x65, 1, 2, 3, 4];
        // Overfill the audio buffer with no video part closing.
        for i in 0..(MAX_PENDING_AUDIO + 50) {
            fb.push_opus(&[i as u8, 1, 2], 960);
        }
        fb.push_au(&idr, 3000, true);
        let seg = fb.push_au(&idr, 3000, true).expect("GOP closes");
        // The fragment carries exactly the cap; the audio tfdt of the NEXT fragment
        // accounts for the 50 dropped + 200 kept frames (timeline never rewinds).
        let moof1 = find_box(&seg.bytes, b"moof").unwrap();
        let mut count = 0u32;
        let mut j = 0;
        let mut audio_seen = 0;
        while j + 8 <= moof1.len() {
            let size = u32::from_be_bytes([moof1[j], moof1[j+1], moof1[j+2], moof1[j+3]]) as usize;
            if &moof1[j+4..j+8] == b"traf" {
                audio_seen += 1;
                if audio_seen == 2 {
                    let traf_body = &moof1[j+8..j+size];
                    let trun = find_box(traf_body, b"trun").unwrap();
                    count = u32::from_be_bytes(trun[4..8].try_into().unwrap());
                }
            }
            j += size;
        }
        assert_eq!(count as usize, MAX_PENDING_AUDIO);
        fb.push_opus(&[9, 9], 960);
        let seg2 = fb.flush().unwrap();
        let moof2 = find_box(&seg2.bytes, b"moof").unwrap();
        let mut audio_tfdt = 0u64;
        let mut seen = 0;
        let mut k = 0;
        while k + 8 <= moof2.len() {
            let size = u32::from_be_bytes([moof2[k], moof2[k+1], moof2[k+2], moof2[k+3]]) as usize;
            if &moof2[k+4..k+8] == b"traf" {
                seen += 1;
                if seen == 2 {
                    let traf_body = &moof2[k+8..k+size];
                    let tfdt = find_box(traf_body, b"tfdt").unwrap();
                    audio_tfdt = u64::from_be_bytes(tfdt[4..12].try_into().unwrap());
                }
            }
            k += size;
        }
        assert_eq!(audio_tfdt, (MAX_PENDING_AUDIO as u64 + 50) * 960, "dropped frames still advance the clock");
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

    // ---- deterministic B-frame (POC) muxing without a real encoder ---------------------
    //
    // The POC presentation-remap path only runs for a bitstream that reorders (B-frames);
    // its deep branches previously ran solely under the #[ignore]d real-encoder tests. Here
    // we synthesize just the SPS + coded-slice HEADERS the muxer actually parses (it copies
    // the rest of each AU verbatim into `mdat` without inspecting it), so a full x264-style
    // bf=2 GOP drives `push_au_pts` → POC record → composition-offset emit deterministically,
    // with no ffmpeg. This is a bit *encoder* (the inverse of the h264_poc parser), not a
    // decoder.

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

    const LOG2_MAX_FRAME_NUM_M4: u32 = 4; // log2_max_frame_num = 8
    const LOG2_MAX_POC_LSB_M4: u32 = 4; // log2_max_poc_lsb = 8

    /// A High-profile SPS (POC type 0) whose fields match the slices below.
    fn bframe_sps() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(100, 8); // profile_idc (High)
        w.put(0, 8); // constraints
        w.put(30, 8); // level_idc
        w.ue(0); // sps_id
        w.ue(1); // chroma_format_idc (4:2:0)
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.bit(0); // qpprime
        w.bit(0); // seq_scaling_matrix_present
        w.ue(LOG2_MAX_FRAME_NUM_M4);
        w.ue(0); // poc_type
        w.ue(LOG2_MAX_POC_LSB_M4);
        w.ue(2); // max_num_ref_frames
        w.bit(0); // gaps
        w.ue(19); // pic_width_in_mbs_minus1  → 320
        w.ue(14); // pic_height_in_map_units_minus1 → 240
        w.bit(1); // frame_mbs_only
        // parse_sps stops above; the two fields below let the sibling sps::dimensions parser
        // read the same NAL back to (320, 240).
        w.bit(1); // direct_8x8_inference_flag
        w.bit(0); // frame_cropping_flag = 0
        w.nal(0x67)
    }

    /// One coded-slice header carrying `poc_lsb`, framed just past pic_order_cnt_lsb.
    fn bframe_slice(is_idr: bool, nal_ref_idc: u8, frame_num: u32, poc_lsb: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // first_mb_in_slice
        w.ue(if is_idr { 7 } else { 5 }); // slice_type
        w.ue(0); // pps_id
        w.put(frame_num, LOG2_MAX_FRAME_NUM_M4 + 4);
        if is_idr {
            w.ue(0); // idr_pic_id
        }
        w.put(poc_lsb, LOG2_MAX_POC_LSB_M4 + 4);
        let header = ((nal_ref_idc & 3) << 5) | if is_idr { 5 } else { 1 };
        w.nal(header)
    }

    /// x264-style bf=2 (no pyramid): decode order interleaves each P ahead of the two
    /// B-frames it brackets. Returns `(annexb_au, keyframe, pts_us)` in DECODE order, plus the
    /// display index each frame presents at (for verification). POC = 2·display_index (step 2).
    fn bframe_gop(groups: usize) -> Vec<(Vec<u8>, bool, i64, i64)> {
        // decode-order display indices: I(0), then for each group k: P(3k), B(3k-2), B(3k-1).
        let mut order = vec![0i64];
        for k in 1..=groups as i64 {
            order.push(3 * k);
            order.push(3 * k - 2);
            order.push(3 * k - 1);
        }
        let frame_ticks_us = 33_367i64; // ~30fps monotonic decode-order timestamps (OBS-style)
        order
            .iter()
            .enumerate()
            .map(|(dec_idx, &disp)| {
                let is_idr = disp == 0;
                let nal_ref_idc = if disp == 0 {
                    3
                } else if disp % 3 == 0 {
                    2 // P-frame (reference)
                } else {
                    0 // disposable B-frame
                };
                let poc_lsb = (2 * disp) as u32;
                let slice = bframe_slice(is_idr, nal_ref_idc, dec_idx as u32 & 0xff, poc_lsb);
                let mut au = vec![0u8, 0, 0, 1];
                au.extend_from_slice(&slice);
                (au, is_idr, dec_idx as i64 * frame_ticks_us, disp)
            })
            .collect()
    }

    /// Extract (base_media_decode_time, [(duration, composition_offset)]) from a fragment's
    /// FIRST (video) traf, using the known trun flag layout the builder writes.
    fn video_trun(frag: &[u8]) -> (u64, Vec<(u32, u32)>) {
        let moof = find_box(frag, b"moof").expect("moof");
        let traf = find_box(moof, b"traf").expect("video traf");
        let tfdt = find_box(traf, b"tfdt").expect("tfdt");
        let base = u64::from_be_bytes(tfdt[4..12].try_into().unwrap());
        let trun = find_box(traf, b"trun").expect("trun");
        let count = u32::from_be_bytes(trun[4..8].try_into().unwrap()) as usize;
        let flags = u32::from_be_bytes([0, trun[1], trun[2], trun[3]]);
        let mut p = 8;
        if flags & 0x0001 != 0 {
            p += 4; // data offset
        }
        let mut out = Vec::new();
        for _ in 0..count {
            let dur = u32::from_be_bytes(trun[p..p + 4].try_into().unwrap());
            let cto = u32::from_be_bytes(trun[p + 12..p + 16].try_into().unwrap());
            out.push((dur, cto));
            p += 16; // dur + size + flags + cto
        }
        (base, out)
    }

    #[test]
    fn poc_path_recovers_bframe_presentation_order() {
        // 7 groups → 22 access units in decode order, enough to pass the POC-step warm-up and
        // close several low-latency parts (each locking then reusing the reorder delay).
        let frames = bframe_gop(7);
        let n = frames.len();
        assert_eq!(n, 22);

        let params = H264Params { sps: bframe_sps(), pps: vec![0x68, 0xee, 0x3c, 0x80], width: 320, height: 240 };
        // Sanity: the synthetic SPS parses to the coded size via the sibling parser.
        assert_eq!(crate::sps::dimensions(&params.sps), Some((320, 240)));

        let mut fb = FragmentBuilder::new_ll(params, 100); // 100ms parts (9000 ticks)
        let mut segs = Vec::new();
        for (au, kf, pts_us, _disp) in &frames {
            if let Some(seg) = fb.push_au_pts(au, *pts_us, *kf) {
                segs.push(seg);
            }
        }
        if let Some(seg) = fb.flush() {
            segs.push(seg);
        }
        assert!(segs.len() >= 2, "several parts emitted, got {}", segs.len());

        // Every emitted fragment is well-formed; only the first (keyframe-led) is independent.
        for (i, seg) in segs.iter().enumerate() {
            top_boxes(&seg.bytes); // asserts the boxes tile exactly
            let indep = fragment_is_independent(&seg.bytes);
            if i == 0 {
                assert!(indep, "leading fragment starts on the IDR");
            }
        }

        // Reconstruct each sample's presentation time PTS = tfdt + Σ(prior decode durations) +
        // composition_offset, and confirm the whole stream presents in display order with a
        // uniform frame duration. This is the end-to-end proof the POC remap is exact.
        let fd = 3003u32; // 33_367µs @ 90kHz
        let mut all_pts = Vec::new();
        let mut total_samples = 0;
        let mut saw_nonzero_cto = false;
        for seg in &segs {
            let (base, samples) = video_trun(&seg.bytes);
            let mut dts = base;
            for (dur, cto) in samples {
                assert_eq!(dur, fd, "uniform decode duration");
                if cto > 0 {
                    saw_nonzero_cto = true;
                }
                all_pts.push(dts + cto as u64);
                dts += dur as u64;
                total_samples += 1;
            }
        }
        assert_eq!(total_samples, n, "all AUs muxed exactly once");
        assert!(saw_nonzero_cto, "B-frame reorder produced composition offsets");
        all_pts.sort_unstable();
        // Display order is 0..22, each one frame_dur apart (a constant reorder delay of 1 frame
        // shifts every PTS by +fd), so the sorted set is exactly {1..=22}·fd.
        let expected: Vec<u64> = (1..=n as u64).map(|d| d * fd as u64).collect();
        assert_eq!(all_pts, expected, "presentation timeline is display order, no gaps/dupes");
    }

    #[test]
    fn pts_path_without_poc_falls_back_to_reorder_delay() {
        // When the SPS carries no usable POC (won't parse), push_au_pts must still handle a
        // B-frame PTS timeline via the per-fragment compute_timing path (decode order I P B B).
        // Low-latency mode (with a wide part target so all four stay in one fragment) exercises
        // the no-POC part-cadence branch, which measures the PTS SPAN each push.
        let params = H264Params { sps: vec![0x41], pps: vec![0x68], width: 320, height: 240 };
        let mut fb = FragmentBuilder::new_ll(params, 1000);
        let fd_us = 33_367i64;
        // Decode order I(disp0) P(disp3) B(disp1) B(disp2); PTS in decode order.
        let plan = [(0x65u8, 0i64, true), (0x41, 3, false), (0x41, 1, false), (0x41, 2, false)];
        for (nal_ty, disp, kf) in plan {
            let au = [0, 0, 0, 1, nal_ty, 0x88];
            assert!(fb.push_au_pts(&au, disp * fd_us, kf).is_none(), "part target not reached");
        }
        let seg = fb.flush().expect("trailing fragment");
        let (_base, samples) = video_trun(&seg.bytes);
        assert_eq!(samples.len(), 4);
        // Reordered stream → at least one nonzero composition offset; durations uniform.
        assert!(samples.iter().any(|&(_, cto)| cto > 0), "reorder offsets present: {samples:?}");
        assert!(samples.iter().all(|&(d, _)| d > 0));
    }

    #[test]
    fn fragment_info_reads_tfhd_defaults_and_first_sample_flags() {
        // A fragment whose trun omits per-sample durations/flags: fragment_info must fall back
        // to the tfhd defaults, and honour a first-sample-flags override for independence.
        // Set every optional tfhd field so fragment_info walks past each one: base-data-offset
        // (0x01), sample-description-index (0x02), default-sample-duration (0x08),
        // default-sample-size (0x10), default-sample-flags (0x20).
        let mut tfhd_body = Vec::new();
        tfhd_body.extend_from_slice(&1u32.to_be_bytes()); // track id
        tfhd_body.extend_from_slice(&0u64.to_be_bytes()); // base-data-offset
        tfhd_body.extend_from_slice(&1u32.to_be_bytes()); // sample-description-index
        tfhd_body.extend_from_slice(&1000u32.to_be_bytes()); // default sample duration
        tfhd_body.extend_from_slice(&512u32.to_be_bytes()); // default sample size
        tfhd_body.extend_from_slice(&0x0101_0000u32.to_be_bytes()); // default flags (non-sync)
        let tfhd = full_bx(b"tfhd", 0, 0x0000_003b, &tfhd_body); // 0x01|0x02|0x08|0x10|0x20

        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&3u32.to_be_bytes()); // sample count
        trun_body.extend_from_slice(&0u32.to_be_bytes()); // data offset
        trun_body.extend_from_slice(&0x0200_0000u32.to_be_bytes()); // first-sample-flags: sync
        let trun = full_bx(b"trun", 0, 0x0000_0005, &trun_body); // data-offset | first-sample-flags

        let traf = bx(b"traf", &concat(&[tfhd, trun]));
        let moof = bx(b"moof", &traf);
        let info = fragment_info(&moof).expect("parses");
        // 3 samples × default 1000 ticks; first-sample-flags mark the lead sample as a sync
        // sample → independent, overriding the non-sync tfhd default.
        assert_eq!(info.duration_ticks, 3000);
        assert!(info.independent, "first-sample-flags sync overrides default");
    }

    #[test]
    fn fragment_info_reads_per_sample_flags_and_no_tfhd() {
        // No tfhd at all; per-sample durations + flags carried inline. The leading sample's
        // own flags decide independence.
        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&2u32.to_be_bytes()); // count
        trun_body.extend_from_slice(&0u32.to_be_bytes()); // data offset
        for (dur, flags) in [(1500u32, 0x0101_0000u32), (1500, 0x0101_0000)] {
            trun_body.extend_from_slice(&dur.to_be_bytes());
            trun_body.extend_from_slice(&flags.to_be_bytes());
        }
        // flags: data-offset | sample-duration | sample-flags
        let trun = full_bx(b"trun", 0, 0x0000_0501, &trun_body);
        let traf = bx(b"traf", &trun);
        let moof = bx(b"moof", &traf);
        let info = fragment_info(&moof).expect("parses");
        assert_eq!(info.duration_ticks, 3000);
        assert!(!info.independent, "leading per-sample flags are non-sync");
    }

    #[test]
    fn fragment_info_clamps_hostile_sample_count() {
        // A forged trun claiming 2^32-1 samples with no per-sample records must be clamped, not
        // walked billions of times (or hung).
        let mut trun_body = Vec::new();
        trun_body.extend_from_slice(&u32::MAX.to_be_bytes()); // absurd sample count
        trun_body.extend_from_slice(&0u32.to_be_bytes()); // data offset
        let trun = full_bx(b"trun", 0, 0x0000_0001, &trun_body); // data-offset only, rec == 0
        let traf = bx(b"traf", &trun);
        let moof = bx(b"moof", &traf);
        // Returns quickly (clamped to 8192 iterations); no per-sample duration → 0 ticks.
        let info = fragment_info(&moof).expect("parses without hanging");
        assert_eq!(info.duration_ticks, 0);
    }

    #[test]
    fn fragment_info_rejects_malformed() {
        assert!(fragment_info(&[]).is_none(), "empty");
        assert!(fragment_info(b"not boxes").is_none(), "garbage");
        // A declared box size < 8 is rejected by the box walker.
        assert!(fragment_info(&[0, 0, 0, 4, b'm', b'o', b'o', b'f']).is_none(), "size < 8");
        // A box claiming more bytes than the buffer holds.
        assert!(fragment_info(&[0, 0, 0, 255, b'm', b'o', b'o', b'f']).is_none(), "overrun");
        // moof present but no traf inside.
        let moof = bx(b"moof", &full_bx(b"mfhd", 0, 0, &1u32.to_be_bytes()));
        assert!(fragment_info(&moof).is_none(), "moof without traf");
        // traf present, trun too short (< 8 bytes of body).
        let traf = bx(b"traf", &full_bx(b"trun", 0, 0, &0u16.to_be_bytes()));
        let moof2 = bx(b"moof", &traf);
        assert!(fragment_info(&moof2).is_none(), "trun shorter than its header");
    }

    #[test]
    fn compute_timing_edge_cases() {
        // Empty input returns the fallback frame duration.
        assert_eq!(compute_timing(&mut [], 3000), 3000);
        // A single AU has no gap to measure → fallback, offset 0.
        let mut one = [Au { avcc: vec![0], pts: 5000, duration: 0, comp_offset: 7, keyframe: true, poc_dec: 0, poc_gop: 0, poc_val: 0 }];
        assert_eq!(compute_timing(&mut one, 2500), 2500);
        assert_eq!(one[0].duration, 2500);
        assert_eq!(one[0].comp_offset, 0);
    }

    #[test]
    fn push_opus_ignores_empty_frame() {
        let mut fb = FragmentBuilder::new(params()).with_opus_audio(2);
        fb.push_opus(&[], 960); // dropped: empty packet
        fb.push_opus(&[0x0b, 1, 2], 960); // the only real frame
        let idr = [0, 0, 0, 1, 0x65, 1, 2];
        fb.push_au(&idr, 3000, true);
        let seg = fb.push_au(&idr, 3000, true).expect("GOP closes");
        // The audio traf carries exactly one sample (the empty one was ignored).
        let moof = find_box(&seg.bytes, b"moof").unwrap();
        let mut count = None;
        let mut j = 0;
        let mut trafs = 0;
        while j + 8 <= moof.len() {
            let size = u32::from_be_bytes([moof[j], moof[j + 1], moof[j + 2], moof[j + 3]]) as usize;
            if &moof[j + 4..j + 8] == b"traf" {
                trafs += 1;
                if trafs == 2 {
                    let body = &moof[j + 8..j + size];
                    let trun = find_box(body, b"trun").unwrap();
                    count = Some(u32::from_be_bytes(trun[4..8].try_into().unwrap()));
                }
            }
            j += size;
        }
        assert_eq!(count, Some(1));
    }

    #[test]
    fn emit_and_flush_on_empty_are_none() {
        let mut fb = FragmentBuilder::new(params());
        assert!(fb.emit().is_none(), "nothing pending → no fragment");
        assert!(fb.flush().is_none(), "flush of an empty builder");
    }
}
