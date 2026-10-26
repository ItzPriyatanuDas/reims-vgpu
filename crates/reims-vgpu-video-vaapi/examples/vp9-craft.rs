//! Rewrites a VP9 IVF stream's uncompressed headers into a stream that
//! uses what libvpx's encoder does not write, for tools/vp9-check.sh. The
//! compressed headers and tiles are kept byte for byte; only what the
//! uncompressed header alone decides is changed, so that the result is a
//! valid stream, which ffmpeg's decoder then decodes as the reference.
//! Runs on any host.
//!
//! ```sh
//! vp9-craft MODE in.ivf out.ivf
//! ```
//!
//! Modes:
//! - `intra-only:N`: chunk `N`, a key frame, becomes a superframe of an
//!   intra-only frame (hidden, as intra-only frames are, refreshing every
//!   slot and resetting every probability context, as the key frame did)
//!   and a shown existing frame of slot 0. An intra-only frame's tiles are
//!   coded as a key frame's, so the frame decodes alike; the frames after
//!   it predict from it as from the key frame. The next frame must not
//!   adapt its probabilities backward (`frame-parallel` or
//!   `error-resilient` encodes), as that adaptation is faster after a key
//!   frame.
//! - `show-existing`: after each chunk with a hidden frame, a chunk that
//!   shows that frame (the lowest slot it refreshes) again.
//! - `features`: from frame 1 on, every frame without segmentation gets
//!   it, with no map (every block keeps its segment, 0 in a stream without
//!   segmentation) and features of segment 0 that vary by frame: a quantizer delta or absolute index, and a loop
//!   filter delta or absolute level. Every frame's loop filter deltas are
//!   coded, varying by frame, and its sharpness and level vary too. None of
//!   these change how the compressed header or the tiles parse. Lossless
//!   frames are kept as they are.
//! - `render-size:WxH`: every frame that codes a frame size (each but a
//!   shown existing frame) gets the render size WxH, coded as
//!   `render_and_frame_size_different` where it is not the frame's own
//!   size. The render size is a hint to the application (6.2.6, 7.2): it
//!   changes no decoded sample, so the stream decodes to the original's
//!   frames, at their frame size.
//!
//! The rewritten header codes what the original's means: segmentation
//! data in force is coded whole, and probabilities of 255 are left
//! uncoded.

use std::process::ExitCode;

use reims_vgpu_video_conformance::ivf;
use reims_vgpu_video_vaapi::vp9::header::{
    CS_RGB, FrameHeader, FrameType, HeaderState, InterpolationFilter, MAX_SEGMENTS, NUM_REF_FRAMES,
    ReferenceInfo, SEG_LVL_ALT_L, SEG_LVL_ALT_Q, SEG_LVL_MAX,
};
use reims_vgpu_video_vaapi::vp9::superframe;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vp9-craft: {e}");
            ExitCode::FAILURE
        }
    }
}

enum Mode {
    IntraOnly(usize),
    ShowExisting,
    Features,
    RenderSize(u32, u32),
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, input, output] = <[String; 3]>::try_from(args).map_err(
        |_| "usage: vp9-craft intra-only:N|show-existing|features|render-size:WxH in.ivf out.ivf",
    )?;
    let mode = match mode.split_once(':') {
        Some(("intra-only", n)) => Mode::IntraOnly(n.parse().map_err(|_| "intra-only:N")?),
        Some(("render-size", size)) => {
            let (w, h) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .filter(|&(w, h)| (1..=65536).contains(&w) && (1..=65536).contains(&h))
                .ok_or("render-size:WxH, each side 1 to 65536")?;
            Mode::RenderSize(w, h)
        }
        None if mode == "show-existing" => Mode::ShowExisting,
        None if mode == "features" => Mode::Features,
        _ => return Err(format!("unknown mode {mode}")),
    };
    let file = std::fs::read(&input).map_err(|e| format!("{input}: {e}"))?;
    let header_size = usize::from(u16::from_le_bytes([file[6], file[7]]));
    let mut state = HeaderState::default();
    let mut refs: [Option<ReferenceInfo>; NUM_REF_FRAMES] = [None; NUM_REF_FRAMES];
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut frame_index = 0usize;
    let mut changed = 0usize;
    let mut after_intra_only = false;
    for (index, (offset, size)) in ivf::vp9_frames(&file)?.into_iter().enumerate() {
        let chunk = &file[offset..offset + size];
        let frames = superframe::frames(chunk).map_err(|e| format!("chunk {index}: {e}"))?;
        let mut out_frames = Vec::with_capacity(frames.len());
        let mut hidden_slot = None;
        for frame in &frames {
            let mut h = FrameHeader::parse(frame, &mut state, &refs)
                .map_err(|e| format!("chunk {index}: {e}"))?;
            if !h.show_existing_frame && std::mem::take(&mut after_intra_only) && adapts(&h) {
                return Err(format!(
                    "chunk {index} adapts its probabilities backward after the new intra-only frame: \
                     encode with frame-parallel decoding or error resilience"
                ));
            }
            let body = &frame[h.uncompressed_header_size..];
            let rewritten = match mode {
                _ if h.show_existing_frame => frame.to_vec(),
                Mode::IntraOnly(n) if n == index => {
                    if h.frame_type != FrameType::Key || frames.len() != 1 {
                        return Err(format!("chunk {index} is not a lone key frame"));
                    }
                    changed += 1;
                    after_intra_only = true;
                    let mut out = write_header(&h, Kind::IntraOnly);
                    out.extend_from_slice(body);
                    out
                }
                Mode::RenderSize(width, height) => {
                    if (h.render_width, h.render_height) != (width, height) {
                        changed += 1;
                    }
                    h.render_width = width;
                    h.render_height = height;
                    let mut out = write_header(&h, Kind::AsCoded);
                    out.extend_from_slice(body);
                    out
                }
                Mode::Features if frame_index > 0 && !h.quantization.lossless() => {
                    changed += 1;
                    add_features(&mut h, frame_index);
                    let mut out = write_header(&h, Kind::AsCoded);
                    out.extend_from_slice(body);
                    out
                }
                _ => frame.to_vec(),
            };
            if !h.show_existing_frame {
                if !h.show_frame {
                    hidden_slot = (0..NUM_REF_FRAMES)
                        .find(|&slot| h.refresh_frame_flags & (1 << slot) != 0)
                        .map(|slot| slot as u8);
                }
                let info = ReferenceInfo {
                    width: h.width,
                    height: h.height,
                    color: h.color,
                };
                for (slot, entry) in refs.iter_mut().enumerate() {
                    if h.refresh_frame_flags & (1 << slot) != 0 {
                        *entry = Some(info);
                    }
                }
                frame_index += 1;
            }
            out_frames.push(rewritten);
        }
        match mode {
            Mode::IntraOnly(n) if n == index => {
                out_frames.push(show_existing(frames[0], 0));
                chunks.push(join(&out_frames));
            }
            Mode::ShowExisting => {
                chunks.push(join(&out_frames));
                if let Some(slot) = hidden_slot {
                    changed += 1;
                    chunks.push(show_existing(frames[0], slot));
                }
            }
            _ => chunks.push(join(&out_frames)),
        }
    }
    if changed == 0 {
        return Err("nothing to change".into());
    }
    let mut out = file[..header_size].to_vec();
    out[24..28].copy_from_slice(&u32::try_from(chunks.len()).unwrap_or(0).to_le_bytes());
    for (pts, chunk) in chunks.iter().enumerate() {
        out.extend_from_slice(&u32::try_from(chunk.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&(pts as u64).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    std::fs::write(&output, out).map_err(|e| format!("{output}: {e}"))?;
    eprintln!("{changed} changes, {} chunks", chunks.len());
    Ok(())
}

/// Whether a frame adapts its probabilities to what it decoded and keeps
/// them for later frames. The adaptation's rate differs after a key frame
/// (libvpx's `COEF_MAX_UPDATE_FACTOR_AFTER_KEY`, ffmpeg's `last_keyframe`),
/// so such a frame cannot follow a key frame made intra-only.
fn adapts(h: &FrameHeader) -> bool {
    h.refresh_frame_context && !h.error_resilient_mode && !h.frame_parallel_decoding_mode
}

/// Frames as one chunk: alone, or a superframe with a 4-byte size index.
fn join(frames: &[Vec<u8>]) -> Vec<u8> {
    if frames.len() == 1 {
        return frames[0].clone();
    }
    let marker = 0xc0 | (3 << 3) | (frames.len() as u8 - 1);
    let mut out = frames.concat();
    out.push(marker);
    for frame in frames {
        out.extend_from_slice(&u32::try_from(frame.len()).unwrap_or(0).to_le_bytes());
    }
    out.push(marker);
    out
}

/// A shown existing frame of `slot`, in the profile of `like`.
fn show_existing(like: &[u8], slot: u8) -> Vec<u8> {
    let profile = ((like[0] >> 5) & 1) | (((like[0] >> 4) & 1) << 1);
    let mut b = Bits::default();
    b.f(2, 2)
        .f(1, u32::from(profile & 1))
        .f(1, u32::from(profile >> 1));
    if profile == 3 {
        b.flag(false);
    }
    b.flag(true).f(3, u32::from(slot));
    b.bytes()
}

/// Varies segment 0's features, the loop filter's deltas, sharpness and
/// level by `frame`, in ways that leave the tiles' parsing alone.
fn add_features(h: &mut FrameHeader, frame: usize) {
    let lf = &mut h.loop_filter;
    let wave = |period: usize, range: i32| ((frame % period) as i32) - range;
    lf.level = (i32::from(lf.level) + wave(9, 4) * 4).clamp(1, 63) as u8;
    lf.sharpness = (frame % 8) as u8;
    lf.delta_enabled = !frame.is_multiple_of(5);
    lf.delta_update = lf.delta_enabled && !frame.is_multiple_of(3);
    if lf.delta_update {
        lf.ref_deltas = [
            (wave(7, 3) * 3) as i8,
            wave(5, 2) as i8,
            (wave(11, 5) * 2) as i8,
            (wave(13, 6) * 4) as i8,
        ];
        lf.mode_deltas = [wave(3, 1) as i8, (wave(4, 2) * 5) as i8];
    }
    let s = &mut h.segmentation;
    if s.enabled {
        return;
    }
    s.enabled = true;
    s.update_map = false;
    s.temporal_update = false;
    s.update_data = true;
    s.abs_or_delta_update = frame % 4 == 3;
    s.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
    s.feature_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
    let base = i32::from(h.quantization.base_q_idx);
    let (q, l) = if s.abs_or_delta_update {
        (
            (base + wave(6, 3) * 8).clamp(1, 255),
            (frame * 7 % 64) as i32,
        )
    } else {
        (wave(6, 3) * 8, wave(10, 5) * 6)
    };
    s.feature_enabled[0][SEG_LVL_ALT_Q] = frame.is_multiple_of(2);
    s.feature_data[0][SEG_LVL_ALT_Q] = q as i16;
    s.feature_enabled[0][SEG_LVL_ALT_L] = frame % 3 != 1;
    s.feature_data[0][SEG_LVL_ALT_L] = l as i16;
    // Another segment's features, which no block uses: coded, not applied.
    s.feature_enabled[5][SEG_LVL_ALT_Q] = true;
    s.feature_data[5][SEG_LVL_ALT_Q] = -40;
}

/// Writes syntax elements, most significant bit first.
#[derive(Default)]
struct Bits {
    bits: Vec<bool>,
}

impl Bits {
    fn f(&mut self, n: u32, value: u32) -> &mut Self {
        for i in (0..n).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    fn flag(&mut self, value: bool) -> &mut Self {
        self.bits.push(value);
        self
    }

    fn su(&mut self, n: u32, value: i32) -> &mut Self {
        self.f(n, value.unsigned_abs()).flag(value < 0)
    }

    /// The bits, then zero bits to the next byte (`trailing_bits()`).
    fn bytes(&mut self) -> Vec<u8> {
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits
            .chunks(8)
            .map(|byte| byte.iter().fold(0u8, |v, &b| (v << 1) | u8::from(b)))
            .collect()
    }
}

#[derive(PartialEq, Eq)]
enum Kind {
    /// The frame as it is.
    AsCoded,
    /// A key frame as an intra-only frame.
    IntraOnly,
}

/// The frame's uncompressed header (6.2), coded from what it means.
fn write_header(h: &FrameHeader, kind: Kind) -> Vec<u8> {
    let mut b = Bits::default();
    let profile = h.profile;
    b.f(2, 2)
        .f(1, u32::from(profile & 1))
        .f(1, u32::from(profile >> 1));
    if profile == 3 {
        b.flag(false);
    }
    b.flag(false);
    let intra_only = kind == Kind::IntraOnly || h.intra_only;
    let key = h.frame_type == FrameType::Key && kind == Kind::AsCoded;
    let show_frame = !intra_only && h.show_frame;
    b.flag(!key).flag(show_frame).flag(h.error_resilient_mode);
    let sync = |b: &mut Bits| {
        b.f(8, 0x49).f(8, 0x83).f(8, 0x42);
    };
    let color = |b: &mut Bits| {
        let c = &h.color;
        if profile >= 2 {
            b.flag(c.bit_depth == 12);
        }
        b.f(3, u32::from(c.color_space));
        if c.color_space != CS_RGB {
            b.flag(c.full_range);
            if profile == 1 || profile == 3 {
                b.flag(c.subsampling_x).flag(c.subsampling_y).flag(false);
            }
        } else if profile == 1 || profile == 3 {
            b.flag(false);
        }
    };
    let frame_size = |b: &mut Bits| {
        b.f(16, h.width - 1).f(16, h.height - 1);
    };
    let render_size = |b: &mut Bits| {
        if (h.render_width, h.render_height) == (h.width, h.height) {
            b.flag(false);
        } else {
            b.flag(true)
                .f(16, h.render_width - 1)
                .f(16, h.render_height - 1);
        }
    };
    if key {
        sync(&mut b);
        color(&mut b);
        frame_size(&mut b);
        render_size(&mut b);
    } else {
        if !show_frame {
            b.flag(intra_only);
        }
        if !h.error_resilient_mode {
            let reset = if kind == Kind::IntraOnly {
                3
            } else {
                h.reset_frame_context
            };
            b.f(2, u32::from(reset));
        }
        if intra_only {
            sync(&mut b);
            if profile > 0 {
                color(&mut b);
            }
            b.f(8, u32::from(h.refresh_frame_flags));
            frame_size(&mut b);
            render_size(&mut b);
        } else {
            b.f(8, u32::from(h.refresh_frame_flags));
            for i in 0..3 {
                b.f(3, u32::from(h.ref_frame_idx[i]))
                    .flag(h.ref_frame_sign_bias[i]);
            }
            match h.size_from_ref {
                Some(found) => {
                    for _ in 0..found {
                        b.flag(false);
                    }
                    b.flag(true);
                }
                None => {
                    b.flag(false).flag(false).flag(false);
                    frame_size(&mut b);
                }
            }
            render_size(&mut b);
            b.flag(h.allow_high_precision_mv);
            match h.interpolation_filter {
                InterpolationFilter::Switchable => b.flag(true),
                InterpolationFilter::EightTapSmooth => b.flag(false).f(2, 0),
                InterpolationFilter::EightTap => b.flag(false).f(2, 1),
                InterpolationFilter::EightTapSharp => b.flag(false).f(2, 2),
                InterpolationFilter::Bilinear => b.flag(false).f(2, 3),
            };
        }
    }
    if !h.error_resilient_mode {
        b.flag(h.refresh_frame_context)
            .flag(h.frame_parallel_decoding_mode);
    }
    b.f(2, u32::from(h.coded_frame_context_idx));
    let lf = &h.loop_filter;
    b.f(6, u32::from(lf.level))
        .f(3, u32::from(lf.sharpness))
        .flag(lf.delta_enabled);
    if lf.delta_enabled {
        b.flag(lf.delta_update);
        if lf.delta_update {
            for &delta in lf.ref_deltas.iter().chain(&lf.mode_deltas) {
                b.flag(true).su(6, i32::from(delta));
            }
        }
    }
    let q = &h.quantization;
    b.f(8, u32::from(q.base_q_idx));
    for delta in [q.delta_q_y_dc, q.delta_q_uv_dc, q.delta_q_uv_ac] {
        b.flag(delta != 0);
        if delta != 0 {
            b.su(4, i32::from(delta));
        }
    }
    let s = &h.segmentation;
    b.flag(s.enabled);
    if s.enabled {
        let prob = |b: &mut Bits, p: u8| {
            if p == 255 {
                b.flag(false);
            } else {
                b.flag(true).f(8, u32::from(p));
            }
        };
        b.flag(s.update_map);
        if s.update_map {
            for &p in &s.tree_probs {
                prob(&mut b, p);
            }
            b.flag(s.temporal_update);
            if s.temporal_update {
                for &p in &s.pred_probs {
                    prob(&mut b, p);
                }
            }
        }
        // The data in force, coded whole: the data this rewrite leaves in
        // the decoder's state is not the original stream's.
        b.flag(true);
        {
            b.flag(s.abs_or_delta_update);
            for segment in 0..MAX_SEGMENTS {
                for feature in 0..SEG_LVL_MAX {
                    let enabled = s.feature_enabled[segment][feature];
                    b.flag(enabled);
                    if enabled {
                        let value = i32::from(s.feature_data[segment][feature]);
                        b.f([8, 6, 2, 0][feature], value.unsigned_abs());
                        if feature < 2 {
                            b.flag(value < 0);
                        }
                    }
                }
            }
        }
    }
    // tile_info(), as 6.2.12 bounds it for the frame's width.
    let sb64_cols = h.sb64_cols();
    let mut min = 0;
    while (64 << min) < sb64_cols {
        min += 1;
    }
    let mut max = 1;
    while (sb64_cols >> max) >= 4 {
        max += 1;
    }
    max -= 1;
    let mut cols = min;
    while cols < max {
        let more = cols < h.tile_info.cols_log2;
        b.flag(more);
        if !more {
            break;
        }
        cols += 1;
    }
    match h.tile_info.rows_log2 {
        0 => b.flag(false),
        1 => b.flag(true).flag(false),
        _ => b.flag(true).flag(true),
    };
    b.f(16, u32::from(h.header_size_in_bytes));
    b.bytes()
}
