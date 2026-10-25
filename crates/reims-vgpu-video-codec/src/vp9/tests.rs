//! The decoding process above the compressed header, on frames written
//! here from the specification's syntax and an accelerator that records
//! what it is asked to decode. The compressed header and the tiles are
//! placeholders: nothing here decodes pixels. What libvpx rarely writes
//! (intra-only frames, shown existing frames, segment features beyond the
//! quantizer, loop filter deltas, sizes taken from a reference) is covered
//! here; the VA-API crate's `tools/vp9-check.sh` covers real streams on
//! hardware.

use super::header::{
    ColorConfig, FrameHeader, FrameType, HeaderState, InterpolationFilter, NUM_REF_FRAMES,
    ReferenceInfo, SEG_LVL_ALT_L, SEG_LVL_ALT_Q, SEG_LVL_REF_FRAME, SEG_LVL_SKIP,
};
use super::quant::{AC_QLOOKUP_8, AC_QLOOKUP_10, DC_QLOOKUP_8, DC_QLOOKUP_10};
use super::vpcc::{ChromaSubsampling, ConfigurationRecord};
use super::*;

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

    /// `su(n)`: the magnitude, then the sign.
    fn su(&mut self, n: u32, value: i32) -> &mut Self {
        self.f(n, value.unsigned_abs()).flag(value < 0)
    }

    /// Zero bits to the next byte (`trailing_bits()`), then the bytes.
    fn aligned(&mut self) -> Vec<u8> {
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits
            .chunks(8)
            .map(|byte| byte.iter().fold(0u8, |v, &b| (v << 1) | u8::from(b)))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Key,
    IntraOnly,
    Inter,
}

/// A segment map's probabilities as coded: the tree's, then the
/// prediction's when the map is coded against the last one. None is 255.
type MapProbabilities = ([Option<u8>; 7], Option<[Option<u8>; 3]>);

/// The loop filter's deltas as coded, references then modes; None is not
/// updated.
type DeltaUpdates = ([Option<i32>; 4], [Option<i32>; 2]);

/// What a test segmentation codes.
#[derive(Clone, Default)]
struct SegWrite {
    update_map: Option<MapProbabilities>,
    /// `abs_or_delta_update`, then each segment's features as coded.
    update_data: Option<(bool, [[Option<i32>; 4]; 8])>,
}

/// A frame header to write: what the tests vary, the rest typical.
#[derive(Clone)]
struct Frame {
    profile: u8,
    kind: Kind,
    show_frame: bool,
    error_resilient: bool,
    reset_frame_context: u8,
    ten_or_twelve: bool,
    color_space: u8,
    full_range: bool,
    /// For Profiles 1 and 3: subsampling x and y.
    subsampling: (bool, bool),
    refresh: u8,
    refs: [u8; 3],
    sign_bias: [bool; 3],
    size: (u32, u32),
    size_from_ref: Option<usize>,
    render: Option<(u32, u32)>,
    high_precision_mv: bool,
    /// None for switchable, else the literal.
    filter: Option<u8>,
    refresh_context: bool,
    parallel: bool,
    context_idx: u8,
    lf_level: u8,
    sharpness: u8,
    lf_delta_enabled: bool,
    lf_update: Option<DeltaUpdates>,
    base_q: u8,
    delta_q: [i32; 3],
    seg: Option<SegWrite>,
    tile_cols_log2: u8,
    tile_rows_log2: u8,
    header_size: u16,
}

impl Frame {
    fn key(width: u32, height: u32) -> Self {
        Self {
            profile: 0,
            kind: Kind::Key,
            show_frame: true,
            error_resilient: false,
            reset_frame_context: 0,
            ten_or_twelve: false,
            color_space: 2,
            full_range: false,
            subsampling: (true, true),
            refresh: 0xff,
            refs: [0, 1, 2],
            sign_bias: [false; 3],
            size: (width, height),
            size_from_ref: None,
            render: None,
            high_precision_mv: false,
            filter: None,
            refresh_context: true,
            parallel: false,
            context_idx: 0,
            lf_level: 10,
            sharpness: 0,
            lf_delta_enabled: false,
            lf_update: None,
            base_q: 60,
            delta_q: [0; 3],
            seg: None,
            tile_cols_log2: 0,
            tile_rows_log2: 0,
            header_size: 3,
        }
    }

    fn inter(width: u32, height: u32) -> Self {
        Self {
            kind: Kind::Inter,
            refresh: 0x01,
            high_precision_mv: true,
            size_from_ref: Some(0),
            context_idx: 1,
            ..Self::key(width, height)
        }
    }

    fn intra_only(width: u32, height: u32) -> Self {
        Self {
            kind: Kind::IntraOnly,
            show_frame: false,
            refresh: 0x04,
            reset_frame_context: 3,
            ..Self::key(width, height)
        }
    }

    fn color_config(&self, b: &mut Bits) {
        if self.profile >= 2 {
            b.flag(self.ten_or_twelve);
        }
        b.f(3, u32::from(self.color_space));
        if self.color_space != 7 {
            b.flag(self.full_range);
            if self.profile == 1 || self.profile == 3 {
                b.flag(self.subsampling.0)
                    .flag(self.subsampling.1)
                    .flag(false);
            }
        } else if self.profile == 1 || self.profile == 3 {
            b.flag(false);
        }
    }

    fn sizes(&self, b: &mut Bits) {
        b.f(16, self.size.0 - 1).f(16, self.size.1 - 1);
    }

    fn render_size(&self, b: &mut Bits) {
        match self.render {
            Some((w, h)) => b.flag(true).f(16, w - 1).f(16, h - 1),
            None => b.flag(false),
        };
    }

    /// The frame: its uncompressed header, `header_size` bytes of
    /// compressed header and a few of tile data.
    fn write(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.f(2, 2)
            .f(1, u32::from(self.profile & 1))
            .f(1, u32::from(self.profile >> 1));
        if self.profile == 3 {
            b.flag(false);
        }
        b.flag(false); // show_existing_frame
        b.flag(self.kind != Kind::Key)
            .flag(self.show_frame)
            .flag(self.error_resilient);
        let sync = |b: &mut Bits| {
            b.f(8, 0x49).f(8, 0x83).f(8, 0x42);
        };
        match self.kind {
            Kind::Key => {
                sync(&mut b);
                self.color_config(&mut b);
                self.sizes(&mut b);
                self.render_size(&mut b);
            }
            Kind::IntraOnly | Kind::Inter => {
                if !self.show_frame {
                    b.flag(self.kind == Kind::IntraOnly);
                }
                if !self.error_resilient {
                    b.f(2, u32::from(self.reset_frame_context));
                }
                if self.kind == Kind::IntraOnly {
                    sync(&mut b);
                    if self.profile > 0 {
                        self.color_config(&mut b);
                    }
                    b.f(8, u32::from(self.refresh));
                    self.sizes(&mut b);
                    self.render_size(&mut b);
                } else {
                    b.f(8, u32::from(self.refresh));
                    for i in 0..3 {
                        b.f(3, u32::from(self.refs[i])).flag(self.sign_bias[i]);
                    }
                    match self.size_from_ref {
                        Some(found) => {
                            for _ in 0..found {
                                b.flag(false);
                            }
                            b.flag(true);
                        }
                        None => {
                            b.flag(false).flag(false).flag(false);
                            self.sizes(&mut b);
                        }
                    }
                    self.render_size(&mut b);
                    b.flag(self.high_precision_mv);
                    match self.filter {
                        None => b.flag(true),
                        Some(literal) => b.flag(false).f(2, u32::from(literal)),
                    };
                }
            }
        }
        if !self.error_resilient {
            b.flag(self.refresh_context).flag(self.parallel);
        }
        b.f(2, u32::from(self.context_idx));
        // loop_filter_params()
        b.f(6, u32::from(self.lf_level))
            .f(3, u32::from(self.sharpness))
            .flag(self.lf_delta_enabled);
        if self.lf_delta_enabled {
            b.flag(self.lf_update.is_some());
            if let Some((refs, modes)) = self.lf_update {
                for delta in refs.iter().chain(&modes) {
                    b.flag(delta.is_some());
                    if let Some(d) = delta {
                        b.su(6, *d);
                    }
                }
            }
        }
        // quantization_params()
        b.f(8, u32::from(self.base_q));
        for delta in self.delta_q {
            b.flag(delta != 0);
            if delta != 0 {
                b.su(4, delta);
            }
        }
        // segmentation_params()
        b.flag(self.seg.is_some());
        if let Some(seg) = &self.seg {
            b.flag(seg.update_map.is_some());
            if let Some((tree, pred)) = &seg.update_map {
                for p in tree {
                    b.flag(p.is_some());
                    if let Some(p) = p {
                        b.f(8, u32::from(*p));
                    }
                }
                b.flag(pred.is_some());
                if let Some(pred) = pred {
                    for p in pred {
                        b.flag(p.is_some());
                        if let Some(p) = p {
                            b.f(8, u32::from(*p));
                        }
                    }
                }
            }
            b.flag(seg.update_data.is_some());
            if let Some((abs, features)) = &seg.update_data {
                b.flag(*abs);
                for segment in features {
                    for (j, feature) in segment.iter().enumerate() {
                        b.flag(feature.is_some());
                        if let Some(value) = feature {
                            let bits = [8, 6, 2, 0][j];
                            b.f(bits, value.unsigned_abs());
                            if j < 2 {
                                b.flag(*value < 0);
                            }
                        }
                    }
                }
            }
        }
        // tile_info()
        let sb64_cols = self.size.0.div_ceil(8).div_ceil(8);
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
            let more = cols < self.tile_cols_log2;
            b.flag(more);
            if !more {
                break;
            }
            cols += 1;
        }
        match self.tile_rows_log2 {
            0 => b.flag(false),
            1 => b.flag(true).flag(false),
            _ => b.flag(true).flag(true),
        };
        b.f(16, u32::from(self.header_size));
        let mut out = b.aligned();
        out.extend(std::iter::repeat_n(0xa5, usize::from(self.header_size) + 4));
        out
    }
}

/// A shown existing frame of `slot`.
fn show_existing(profile: u8, slot: u8) -> Vec<u8> {
    let mut b = Bits::default();
    b.f(2, 2)
        .f(1, u32::from(profile & 1))
        .f(1, u32::from(profile >> 1));
    if profile == 3 {
        b.flag(false);
    }
    b.flag(true).f(3, u32::from(slot));
    b.aligned()
}

/// Frames joined into a superframe, with an index of `size_bytes`-byte
/// sizes.
fn superframe(frames: &[Vec<u8>], size_bytes: usize) -> Vec<u8> {
    let marker = 0xc0 | ((size_bytes as u8 - 1) << 3) | (frames.len() as u8 - 1);
    let mut out: Vec<u8> = frames.concat();
    out.push(marker);
    for frame in frames {
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes()[..size_bytes]);
    }
    out.push(marker);
    out
}

fn parse(frame: &[u8], state: &mut HeaderState) -> Result<FrameHeader, Error> {
    FrameHeader::parse(frame, state, &[None; NUM_REF_FRAMES])
}

fn slots_of(width: u32, height: u32, color: ColorConfig) -> [Option<ReferenceInfo>; 8] {
    [Some(ReferenceInfo {
        width,
        height,
        color,
    }); 8]
}

const BT709_8: ColorConfig = ColorConfig {
    bit_depth: 8,
    color_space: 2,
    full_range: false,
    subsampling_x: true,
    subsampling_y: true,
};

#[test]
fn a_key_frame_reads_its_whole_header() -> Result<(), Error> {
    let mut frame = Frame::key(1920, 1080);
    frame.render = Some((1280, 720));
    frame.lf_level = 36;
    frame.sharpness = 5;
    frame.base_q = 120;
    frame.delta_q = [-3, 2, -8];
    frame.tile_cols_log2 = 2;
    frame.tile_rows_log2 = 1;
    frame.full_range = true;
    frame.context_idx = 2;
    frame.header_size = 300;
    let bytes = frame.write();
    let mut state = HeaderState::default();
    let h = parse(&bytes, &mut state)?;
    assert_eq!(h.profile, 0);
    assert_eq!(h.frame_type, FrameType::Key);
    assert!(h.show_frame && !h.intra_only && !h.error_resilient_mode);
    assert_eq!((h.width, h.height), (1920, 1080));
    assert_eq!((h.render_width, h.render_height), (1280, 720));
    assert_eq!(h.refresh_frame_flags, 0xff);
    assert_eq!(
        h.color,
        ColorConfig {
            full_range: true,
            ..BT709_8
        }
    );
    assert_eq!((h.loop_filter.level, h.loop_filter.sharpness), (36, 5));
    assert_eq!(h.quantization.base_q_idx, 120);
    assert_eq!(
        (
            h.quantization.delta_q_y_dc,
            h.quantization.delta_q_uv_dc,
            h.quantization.delta_q_uv_ac
        ),
        (-3, 2, -8)
    );
    assert!(!h.quantization.lossless());
    assert_eq!((h.tile_info.cols_log2, h.tile_info.rows_log2), (2, 1));
    assert_eq!(h.header_size_in_bytes, 300);
    // A key frame's context index is coded, and decoding uses 0.
    assert_eq!((h.coded_frame_context_idx, h.frame_context_idx), (2, 0));
    assert!(h.refresh_frame_context && !h.frame_parallel_decoding_mode);
    assert_eq!(state.color, Some(h.color));
    // The compressed header starts after the uncompressed one's last byte.
    assert_eq!(bytes[h.uncompressed_header_size], 0xa5);
    assert_ne!(bytes[h.uncompressed_header_size - 1], 0xa5);
    Ok(())
}

#[test]
fn a_frame_shorter_than_its_compressed_header_is_truncated() {
    let mut bytes = Frame::key(64, 64).write();
    bytes.truncate(bytes.len() - 6);
    assert_eq!(
        parse(&bytes, &mut HeaderState::default()),
        Err(Error::Truncated)
    );
}

#[test]
fn profile_2_codes_its_bit_depth() -> Result<(), Error> {
    let mut frame = Frame::key(640, 360);
    frame.profile = 2;
    let h = parse(&frame.write(), &mut HeaderState::default())?;
    assert_eq!((h.profile, h.color.bit_depth), (2, 10));
    frame.ten_or_twelve = true;
    let h = parse(&frame.write(), &mut HeaderState::default())?;
    assert_eq!(h.color.bit_depth, 12);
    assert_eq!(
        decoder::check_supported(&h),
        Err(Error::Unsupported("VP9 at 12 bits"))
    );
    Ok(())
}

#[test]
fn profiles_1_and_3_parse_and_are_refused() -> Result<(), Error> {
    for profile in [1, 3] {
        let mut frame = Frame::key(640, 360);
        frame.profile = profile;
        frame.subsampling = (false, false);
        let h = parse(&frame.write(), &mut HeaderState::default())?;
        assert_eq!(h.profile, profile);
        assert!(!h.color.subsampling_x && !h.color.subsampling_y);
        assert!(matches!(
            decoder::check_supported(&h),
            Err(Error::Unsupported(_))
        ));
    }
    // sRGB is 4:4:4, which Profile 0 cannot have.
    let mut frame = Frame::key(64, 64);
    frame.color_space = 7;
    assert_eq!(
        parse(&frame.write(), &mut HeaderState::default()),
        Err(Error::OutOfRange("color_space"))
    );
    Ok(())
}

#[test]
fn an_inter_frame_reads_its_references_and_filter() -> Result<(), Error> {
    let mut state = HeaderState::default();
    parse(&Frame::key(352, 288).write(), &mut state)?;
    let refs = slots_of(352, 288, BT709_8);
    for (literal, filter) in [
        (Some(0), InterpolationFilter::EightTapSmooth),
        (Some(1), InterpolationFilter::EightTap),
        (Some(2), InterpolationFilter::EightTapSharp),
        (Some(3), InterpolationFilter::Bilinear),
        (None, InterpolationFilter::Switchable),
    ] {
        let mut frame = Frame::inter(352, 288);
        frame.refs = [3, 5, 7];
        frame.sign_bias = [false, true, true];
        frame.refresh = 0x22;
        frame.filter = literal;
        frame.size_from_ref = Some(2);
        let h = FrameHeader::parse(&frame.write(), &mut state.clone(), &refs)?;
        assert_eq!(h.frame_type, FrameType::NonKey);
        assert_eq!(h.ref_frame_idx, [3, 5, 7]);
        assert_eq!(h.ref_frame_sign_bias, [false, true, true]);
        assert_eq!(h.refresh_frame_flags, 0x22);
        assert_eq!(h.size_from_ref, Some(2));
        assert_eq!((h.width, h.height), (352, 288));
        assert!(h.allow_high_precision_mv);
        assert_eq!(h.interpolation_filter, filter);
        assert_eq!(h.color, BT709_8);
        // An inter frame decodes with its coded context.
        assert_eq!(h.frame_context_idx, 1);
    }
    Ok(())
}

#[test]
fn an_inter_frame_takes_a_references_size_or_codes_its_own() -> Result<(), Error> {
    let mut state = HeaderState::default();
    parse(&Frame::key(640, 480).write(), &mut state)?;
    let mut refs = slots_of(640, 480, BT709_8);
    refs[4] = Some(ReferenceInfo {
        width: 320,
        height: 240,
        color: BT709_8,
    });
    let mut frame = Frame::inter(320, 240);
    frame.refs = [0, 4, 1];
    frame.size_from_ref = Some(1);
    let h = FrameHeader::parse(&frame.write(), &mut state.clone(), &refs)?;
    assert_eq!((h.width, h.height), (320, 240));
    // Coded, three quarters of the references' size: within the scaling.
    frame.size_from_ref = None;
    frame.size = (480, 360);
    let h = FrameHeader::parse(&frame.write(), &mut state.clone(), &refs)?;
    assert_eq!((h.width, h.height, h.size_from_ref), (480, 360, None));
    assert_eq!((h.render_width, h.render_height), (480, 360));
    // Over twice as small as every reference: none can be predicted from.
    frame.size = (100, 100);
    frame.refs = [0, 1, 2];
    assert_eq!(
        FrameHeader::parse(&frame.write(), &mut state.clone(), &refs),
        Err(Error::NoReferenceInScale)
    );
    // A reference of another bit depth is malformed.
    refs[2] = Some(ReferenceInfo {
        color: ColorConfig {
            bit_depth: 10,
            ..BT709_8
        },
        ..refs[2].ok_or(Error::MissingReference)?
    });
    frame.size = (640, 480);
    assert!(FrameHeader::parse(&frame.write(), &mut state.clone(), &refs).is_err());
    // A slot with no frame is a missing reference.
    let mut empty = slots_of(640, 480, BT709_8);
    empty[1] = None;
    assert_eq!(
        FrameHeader::parse(&frame.write(), &mut state.clone(), &empty),
        Err(Error::MissingReference)
    );
    Ok(())
}

#[test]
fn an_inter_frame_needs_an_intra_frame_before_it() {
    let refs = slots_of(64, 64, BT709_8);
    assert_eq!(
        FrameHeader::parse(
            &Frame::inter(64, 64).write(),
            &mut HeaderState::default(),
            &refs
        ),
        Err(Error::MissingReference)
    );
}

#[test]
fn an_intra_only_frame_of_profile_0_takes_the_defaults() -> Result<(), Error> {
    let mut state = HeaderState::default();
    let mut key = Frame::key(640, 360);
    key.full_range = true;
    parse(&key.write(), &mut state)?;
    let mut frame = Frame::intra_only(320, 180);
    frame.context_idx = 3;
    frame.reset_frame_context = 2;
    let h = parse(&frame.write(), &mut state)?;
    assert!(h.intra_only && !h.show_frame);
    assert_eq!(h.color, ColorConfig::PROFILE_0_INTRA_ONLY);
    assert_eq!(state.color, Some(ColorConfig::PROFILE_0_INTRA_ONLY));
    assert_eq!(h.refresh_frame_flags, 0x04);
    assert_eq!((h.width, h.height), (320, 180));
    assert_eq!(h.reset_frame_context, 2);
    assert_eq!((h.coded_frame_context_idx, h.frame_context_idx), (3, 0));
    // Profile 2's intra-only frames code their colour.
    frame.profile = 2;
    frame.full_range = true;
    let h = parse(&frame.write(), &mut state)?;
    assert_eq!((h.color.bit_depth, h.color.full_range), (10, true));
    Ok(())
}

#[test]
fn a_shown_frame_is_never_intra_only() -> Result<(), Error> {
    let mut state = HeaderState::default();
    parse(&Frame::key(64, 64).write(), &mut state)?;
    let refs = slots_of(64, 64, BT709_8);
    // With show_frame set, intra_only is not coded: an inter frame.
    let frame = Frame::inter(64, 64);
    let h = FrameHeader::parse(&frame.write(), &mut state, &refs)?;
    assert!(h.show_frame && !h.intra_only);
    Ok(())
}

#[test]
fn error_resilient_frames_infer_their_context_flags() -> Result<(), Error> {
    let mut state = HeaderState::default();
    parse(&Frame::key(64, 64).write(), &mut state)?;
    let refs = slots_of(64, 64, BT709_8);
    let mut frame = Frame::inter(64, 64);
    frame.error_resilient = true;
    frame.context_idx = 2;
    let h = FrameHeader::parse(&frame.write(), &mut state, &refs)?;
    assert!(h.error_resilient_mode);
    assert_eq!(h.reset_frame_context, 0);
    assert!(!h.refresh_frame_context && h.frame_parallel_decoding_mode);
    assert_eq!((h.coded_frame_context_idx, h.frame_context_idx), (2, 0));
    Ok(())
}

#[test]
fn loop_filter_deltas_persist_until_an_intra_or_resilient_frame() -> Result<(), Error> {
    let mut state = HeaderState::default();
    let mut key = Frame::key(64, 64);
    key.lf_delta_enabled = true;
    key.lf_update = Some(([None, Some(5), None, Some(-7)], [Some(-2), None]));
    let h = parse(&key.write(), &mut state)?;
    assert!(h.loop_filter.delta_enabled && h.loop_filter.delta_update);
    assert_eq!(h.loop_filter.ref_deltas, [1, 5, -1, -7]);
    assert_eq!(h.loop_filter.mode_deltas, [-2, 0]);
    let refs = slots_of(64, 64, BT709_8);
    // Not updated: the deltas in force.
    let mut inter = Frame::inter(64, 64);
    inter.lf_delta_enabled = true;
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    assert!(!h.loop_filter.delta_update);
    assert_eq!(h.loop_filter.ref_deltas, [1, 5, -1, -7]);
    assert_eq!(h.loop_filter.mode_deltas, [-2, 0]);
    // Disabled for a frame, and still in force after.
    inter.lf_delta_enabled = false;
    FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    inter.lf_delta_enabled = true;
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    assert_eq!(h.loop_filter.ref_deltas, [1, 5, -1, -7]);
    // An error resilient frame resets them to the defaults.
    inter.error_resilient = true;
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    assert_eq!(h.loop_filter.ref_deltas, [1, 0, -1, -1]);
    assert_eq!(h.loop_filter.mode_deltas, [0, 0]);
    Ok(())
}

fn features(f: impl Fn(usize) -> [Option<i32>; 4]) -> [[Option<i32>; 4]; 8] {
    core::array::from_fn(f)
}

#[test]
fn segmentation_reads_its_map_probabilities_and_features() -> Result<(), Error> {
    let mut state = HeaderState::default();
    let mut key = Frame::key(64, 64);
    key.seg = Some(SegWrite {
        update_map: Some((
            [Some(10), None, Some(30), None, None, Some(200), Some(1)],
            None,
        )),
        update_data: Some((
            false,
            features(|s| match s {
                0 => [Some(-20), Some(9), None, None],
                3 => [None, Some(-63), Some(2), Some(0)],
                7 => [Some(255), None, Some(3), None],
                _ => [None; 4],
            }),
        )),
    });
    let h = parse(&key.write(), &mut state)?;
    let s = &h.segmentation;
    assert!(s.enabled && s.update_map && s.update_data);
    assert!(!s.temporal_update && !s.abs_or_delta_update);
    assert_eq!(s.tree_probs, [10, 255, 30, 255, 255, 200, 1]);
    assert_eq!(s.pred_probs, [255; 3]);
    assert_eq!(s.feature_data[0][SEG_LVL_ALT_Q], -20);
    assert_eq!(s.feature_data[0][SEG_LVL_ALT_L], 9);
    assert_eq!(s.feature_data[3][SEG_LVL_ALT_L], -63);
    assert_eq!(s.feature_data[3][SEG_LVL_REF_FRAME], 2);
    assert!(s.feature_enabled[3][SEG_LVL_SKIP]);
    assert_eq!(s.feature_data[7][SEG_LVL_ALT_Q], 255);
    assert!(!s.feature_enabled[1][SEG_LVL_ALT_Q]);
    // An inter frame that updates the map against the last one, and keeps
    // the features.
    let refs = slots_of(64, 64, BT709_8);
    let mut inter = Frame::inter(64, 64);
    inter.seg = Some(SegWrite {
        update_map: Some(([None; 7], Some([Some(7), None, Some(9)]))),
        update_data: None,
    });
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    let s = &h.segmentation;
    assert!(s.temporal_update && s.update_map && !s.update_data);
    assert_eq!(s.pred_probs, [7, 255, 9]);
    assert_eq!(s.tree_probs, [255; 7]);
    assert_eq!(s.feature_data[3][SEG_LVL_ALT_L], -63);
    // Segmentation off for a frame, then on with neither update: the
    // features are still those in force.
    inter.seg = None;
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    assert!(!h.segmentation.enabled);
    assert!(!h.segmentation.feature_active(3, SEG_LVL_ALT_L));
    inter.seg = Some(SegWrite::default());
    let h = FrameHeader::parse(&inter.write(), &mut state, &refs)?;
    assert!(h.segmentation.feature_active(3, SEG_LVL_ALT_L));
    assert!(!h.segmentation.update_map && !h.segmentation.temporal_update);
    // An intra-only frame clears them.
    let mut intra = Frame::intra_only(64, 64);
    intra.seg = Some(SegWrite::default());
    let h = parse(&intra.write(), &mut state)?;
    assert!(!h.segmentation.feature_active(3, SEG_LVL_ALT_L));
    assert_eq!(h.segmentation.feature_data, [[0; 4]; 8]);
    Ok(())
}

#[test]
fn a_header_with_an_error_changes_no_state() -> Result<(), Error> {
    let mut state = HeaderState::default();
    let mut key = Frame::key(64, 64);
    key.lf_delta_enabled = true;
    key.lf_update = Some(([Some(3); 4], [Some(1); 2]));
    let mut bytes = key.write();
    bytes.truncate(bytes.len() - 6);
    assert_eq!(parse(&bytes, &mut state), Err(Error::Truncated));
    assert_eq!(state, HeaderState::default());
    Ok(())
}

#[test]
fn tile_columns_are_bounded_by_the_width() -> Result<(), Error> {
    // 4096 wide: 64 superblocks, so 0 to 4 for the log2 of the columns.
    for cols in 0..=4 {
        let mut frame = Frame::key(4096, 2176);
        frame.tile_cols_log2 = cols;
        frame.tile_rows_log2 = 2;
        let h = parse(&frame.write(), &mut HeaderState::default())?;
        assert_eq!((h.tile_info.cols_log2, h.tile_info.rows_log2), (cols, 2));
    }
    // 8192 wide: 128 superblocks, at least two columns, at most 32.
    let mut frame = Frame::key(8192, 64);
    frame.tile_cols_log2 = 1;
    let h = parse(&frame.write(), &mut HeaderState::default())?;
    assert_eq!(h.tile_info.cols_log2, 1);
    frame.tile_cols_log2 = 5;
    let h = parse(&frame.write(), &mut HeaderState::default())?;
    assert_eq!(h.tile_info.cols_log2, 5);
    // Under 256 wide: one column, and no bit for it.
    let frame = Frame::key(200, 64);
    let h = parse(&frame.write(), &mut HeaderState::default())?;
    assert_eq!(h.tile_info.cols_log2, 0);
    Ok(())
}

#[test]
fn superframes_split_by_their_index() -> Result<(), Error> {
    let a = Frame::key(64, 64).write();
    let b = vec![0x55; 300];
    for size_bytes in 2..=4 {
        let chunk = superframe(&[a.clone(), b.clone()], size_bytes);
        assert_eq!(superframe::frames(&chunk)?, vec![&a[..], &b[..]]);
    }
    let small = superframe(&[a.clone(), vec![0x55; 10]], 1);
    assert_eq!(superframe::frames(&small)?.len(), 2);
    // No index: one frame.
    assert_eq!(superframe::frames(&a)?, vec![&a[..]]);
    // A marker byte at the end whose index start does not match it: one
    // frame, as the end of a frame's tile data may look like a marker.
    let mut odd = a.clone();
    odd.push(0xc1);
    assert_eq!(superframe::frames(&odd)?, vec![&odd[..]]);
    // Sizes past the chunk.
    let mut bad = superframe(&[a.clone(), b.clone()], 2);
    let at = bad.len() - 3;
    bad[at] = 0xff;
    bad[at + 1] = 0x7f;
    assert!(superframe::frames(&bad).is_err());
    assert!(superframe::frames(&[])?.is_empty());
    // A frame of size 0 is no frame.
    let with_empty = superframe(&[a.clone(), Vec::new(), b.clone()], 2);
    assert_eq!(superframe::frames(&with_empty)?, vec![&a[..], &b[..]]);
    // An index longer than the chunk: one frame.
    let lone_index = [0xc7, 0xc7];
    assert_eq!(superframe::frames(&lone_index)?, vec![&lone_index[..]]);
    Ok(())
}

#[test]
fn quantizers_follow_the_segment_and_the_deltas() -> Result<(), Error> {
    let mut key = Frame::key(64, 64);
    key.base_q = 100;
    key.delta_q = [-5, 3, 7];
    key.seg = Some(SegWrite {
        update_map: None,
        update_data: Some((
            false,
            features(|s| match s {
                1 => [Some(-120), None, None, None],
                2 => [Some(200), None, None, None],
                _ => [None; 4],
            }),
        )),
    });
    let h = parse(&key.write(), &mut HeaderState::default())?;
    let q = quant::segment_quantizers(&h, 0).ok_or(Error::Truncated)?;
    assert_eq!(q.qindex, 100);
    assert_eq!(q.luma_dc, DC_QLOOKUP_8[95]);
    assert_eq!(q.luma_ac, AC_QLOOKUP_8[100]);
    assert_eq!(q.chroma_dc, DC_QLOOKUP_8[103]);
    assert_eq!(q.chroma_ac, AC_QLOOKUP_8[107]);
    // A delta below 0 clips to 0, and its DC delta then to 0 too.
    let q = quant::segment_quantizers(&h, 1).ok_or(Error::Truncated)?;
    assert_eq!(q.qindex, 0);
    assert_eq!(q.luma_dc, DC_QLOOKUP_8[0]);
    assert_eq!(q.chroma_ac, AC_QLOOKUP_8[7]);
    // Above 255 clips to 255.
    let q = quant::segment_quantizers(&h, 2).ok_or(Error::Truncated)?;
    assert_eq!((q.qindex, q.chroma_ac), (255, AC_QLOOKUP_8[255]));
    // Absolute values replace the base, and 10 bits use their tables.
    key.profile = 2;
    key.seg = Some(SegWrite {
        update_map: None,
        update_data: Some((true, features(|_| [Some(40), None, None, None]))),
    });
    let h = parse(&key.write(), &mut HeaderState::default())?;
    let q = quant::segment_quantizers(&h, 5).ok_or(Error::Truncated)?;
    assert_eq!(q.qindex, 40);
    assert_eq!(q.luma_dc, DC_QLOOKUP_10[35]);
    assert_eq!(q.luma_ac, AC_QLOOKUP_10[40]);
    // A segment past the eight has no feature: the frame's own values.
    assert_eq!(quant::qindex(&h, 8), 100);
    assert!(!h.segmentation.feature_active(8, SEG_LVL_ALT_Q));
    assert_eq!(quant::filter_levels(&h, 8), quant::filter_levels(&h, 0));
    // Lossless is the frame's: every index and delta zero.
    let mut lossless = Frame::key(64, 64);
    lossless.base_q = 0;
    let h = parse(&lossless.write(), &mut HeaderState::default())?;
    assert!(h.quantization.lossless());
    Ok(())
}

#[test]
fn the_tables_end_where_the_specification_ends_them() {
    assert_eq!((DC_QLOOKUP_8[0], DC_QLOOKUP_8[255]), (4, 1336));
    assert_eq!((AC_QLOOKUP_8[0], AC_QLOOKUP_8[255]), (4, 1828));
    assert_eq!((DC_QLOOKUP_10[0], DC_QLOOKUP_10[255]), (4, 5347));
    assert_eq!((AC_QLOOKUP_10[0], AC_QLOOKUP_10[255]), (4, 7312));
    for table in [DC_QLOOKUP_8, AC_QLOOKUP_8, DC_QLOOKUP_10, AC_QLOOKUP_10] {
        assert!(table.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

#[test]
fn filter_levels_follow_the_segment_and_the_deltas() -> Result<(), Error> {
    let mut key = Frame::key(64, 64);
    key.lf_level = 20;
    key.lf_delta_enabled = true;
    key.lf_update = Some(([Some(2), Some(-3), Some(4), Some(-30)], [Some(5), Some(-1)]));
    key.seg = Some(SegWrite {
        update_map: None,
        update_data: Some((
            false,
            features(|s| match s {
                1 => [None, Some(30), None, None],
                2 => [None, Some(-25), None, None],
                _ => [None; 4],
            }),
        )),
    });
    let h = parse(&key.write(), &mut HeaderState::default())?;
    // Below 32 the deltas count once.
    assert_eq!(
        quant::filter_levels(&h, 0),
        [[22, 22], [22, 16], [29, 23], [0, 0]]
    );
    // A segment's delta, then the frame's scale: 50 with each delta once.
    assert_eq!(
        quant::filter_levels(&h, 1),
        [[52, 52], [52, 46], [59, 53], [25, 19]]
    );
    // Clipped at 0 before the deltas.
    assert_eq!(
        quant::filter_levels(&h, 2),
        [[2, 2], [2, 0], [9, 3], [0, 0]]
    );
    // From 32 up the frame's level doubles every delta, whatever the
    // segment's level.
    key.lf_level = 40;
    let h = parse(&key.write(), &mut HeaderState::default())?;
    assert_eq!(
        quant::filter_levels(&h, 2),
        [[19, 19], [19, 7], [33, 21], [0, 0]]
    );
    // With no deltas, the segment's level everywhere; absolute values
    // replace the frame's.
    key.lf_delta_enabled = false;
    key.lf_update = None;
    key.seg = Some(SegWrite {
        update_map: None,
        update_data: Some((true, features(|_| [None, Some(12), None, None]))),
    });
    let h = parse(&key.write(), &mut HeaderState::default())?;
    assert_eq!(quant::filter_levels(&h, 4), [[12, 12]; 4]);
    // A frame level of 0 turns the loop filter off, segments and all.
    key.lf_level = 0;
    let h = parse(&key.write(), &mut HeaderState::default())?;
    assert_eq!(quant::filter_levels(&h, 4), [[0, 0]; 4]);
    Ok(())
}

#[test]
fn vpcc_records_of_both_versions() -> Result<(), Error> {
    let v1 = [1, 0, 0, 0, 2, 31, (10 << 4) | (1 << 1) | 1, 9, 16, 9, 0, 0];
    let record = ConfigurationRecord::parse(&v1)?;
    assert_eq!(
        (record.profile, record.level, record.bit_depth),
        (2, 31, 10)
    );
    assert_eq!(
        record.chroma_subsampling,
        ChromaSubsampling::Yuv420Colocated
    );
    assert!(record.full_range && record.chroma_subsampling.is_420());
    assert_eq!(record.colour, (9, 16, 9));
    let v0 = [0, 0, 0, 0, 1, 10, (8 << 4) | 2, 3 << 4, 0x80, 0, 0];
    let record = ConfigurationRecord::parse(&v0)?;
    assert_eq!(
        (record.version, record.profile, record.bit_depth),
        (0, 1, 8)
    );
    assert_eq!(record.chroma_subsampling, ChromaSubsampling::Yuv444);
    assert!(record.full_range);
    assert!(ConfigurationRecord::parse(&v1[..9]).is_err());
    let mut with_data = v1.to_vec();
    with_data[11] = 4;
    assert!(ConfigurationRecord::parse(&with_data).is_err());
    with_data.extend([0; 4]);
    assert!(ConfigurationRecord::parse(&with_data).is_ok());
    assert!(ConfigurationRecord::parse(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    Ok(())
}

/// What the recording accelerator saw of one frame.
#[derive(Debug, Clone)]
struct Decoded {
    picture: u32,
    size: (u32, u32),
    references: [Option<u32>; 8],
    intra_only: bool,
    first_byte: u8,
    segments: [SegmentParams; 8],
}

#[derive(Default)]
struct Recorder {
    next: u32,
    configured: Vec<(StreamFormat, u32, u32)>,
    decoded: Vec<Decoded>,
}

impl Accelerator for Recorder {
    type Picture = u32;
    type Error = Error;

    fn configure(&mut self, format: &StreamFormat, width: u32, height: u32) -> Result<(), Error> {
        self.configured.push((*format, width, height));
        Ok(())
    }

    fn new_picture(&mut self, _width: u32, _height: u32) -> Result<u32, Error> {
        self.next += 1;
        Ok(self.next)
    }

    fn decode_frame(&mut self, p: &FrameParams<'_, u32>) -> Result<(), Error> {
        self.decoded.push(Decoded {
            picture: *p.picture,
            size: (p.header.width, p.header.height),
            references: p.references.map(|r| r.copied()),
            intra_only: p.header.intra_only,
            first_byte: p.data[0],
            segments: p.segments,
        });
        Ok(())
    }
}

fn shown(outputs: &[Output<u32>]) -> Vec<(u32, u64, bool)> {
    outputs
        .iter()
        .map(|o| (*o.picture, o.id, o.show_existing_frame))
        .collect()
}

#[test]
fn the_decoder_follows_superframes_slots_and_shown_existing_frames() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    assert_eq!(
        shown(&d.decode(&Frame::key(320, 240).write(), 0)?),
        [(1, 0, false)]
    );
    // A hidden ALTREF into slot 2, then a shown frame into slot 0.
    let mut altref = Frame::inter(320, 240);
    altref.show_frame = false;
    altref.refresh = 0x04;
    let mut next = Frame::inter(320, 240);
    next.refs = [0, 1, 2];
    next.refresh = 0x01;
    let chunk = superframe(&[altref.write(), next.write()], 2);
    assert_eq!(shown(&d.decode(&chunk, 1)?), [(3, 1, false)]);
    // The ALTREF decoded against the key frame, the next frame with it.
    let decoded = &d.accelerator().decoded;
    assert_eq!(
        decoded.iter().map(|f| f.picture).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(decoded[1].references, [Some(1); 8]);
    assert_eq!(
        decoded[2].references,
        [
            Some(1),
            Some(1),
            Some(2),
            Some(1),
            Some(1),
            Some(1),
            Some(1),
            Some(1)
        ]
    );
    // The ALTREF shown when its time comes: nothing decoded.
    let outputs = d.decode(&show_existing(0, 2), 2)?;
    assert_eq!(shown(&outputs), [(2, 2, true)]);
    assert_eq!(outputs[0].display_index, 2);
    assert_eq!(d.accelerator().decoded.len(), 3);
    // A chunk with only a hidden frame shows nothing.
    assert!(d.decode(&altref.write(), 3)?.is_empty());
    // An empty slot cannot be shown.
    let mut d = Decoder::new(Recorder::default());
    assert_eq!(
        d.decode(&show_existing(0, 5), 0).err(),
        Some(Error::MissingReference)
    );
    Ok(())
}

#[test]
fn the_decoder_configures_at_key_frames_not_at_inter_resizes() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    d.decode(&Frame::key(640, 480).write(), 0)?;
    // An inter frame at three quarters of its references' size.
    let mut smaller = Frame::inter(480, 360);
    smaller.size_from_ref = None;
    let outputs = d.decode(&smaller.write(), 1)?;
    assert_eq!((outputs[0].width, outputs[0].height), (480, 360));
    // The same key frame size again: nothing to configure.
    d.decode(&Frame::key(640, 480).write(), 2)?;
    // A key frame of another size, then of another bit depth.
    d.decode(&Frame::key(1280, 720).write(), 3)?;
    let mut ten = Frame::key(1280, 720);
    ten.profile = 2;
    d.decode(&ten.write(), 4)?;
    let eight = StreamFormat {
        profile: 0,
        bit_depth: 8,
        subsampling_x: true,
        subsampling_y: true,
    };
    assert_eq!(
        d.accelerator().configured,
        [
            (eight, 640, 480),
            (eight, 1280, 720),
            (
                StreamFormat {
                    profile: 2,
                    bit_depth: 10,
                    ..eight
                },
                1280,
                720
            )
        ]
    );
    assert_eq!(d.accelerator().decoded[1].size, (480, 360));
    Ok(())
}

#[test]
fn the_decoder_starts_at_key_and_intra_only_frames() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    assert_eq!(
        d.decode(&Frame::inter(64, 64).write(), 0).err(),
        Some(Error::MissingReference)
    );
    assert!(d.accelerator().decoded.is_empty());
    // An intra-only frame fills the slots it refreshes, and an inter frame
    // that names only those decodes.
    let intra = Frame::intra_only(64, 64);
    assert!(d.decode(&intra.write(), 1)?.is_empty());
    let mut inter = Frame::inter(64, 64);
    inter.refs = [2, 2, 2];
    let outputs = d.decode(&inter.write(), 2)?;
    assert_eq!(outputs.len(), 1);
    let decoded = &d.accelerator().decoded;
    assert!(decoded[0].intra_only);
    assert_eq!(decoded[1].references[2], Some(1));
    assert_eq!(decoded[1].references[0], None);
    // After a reset, inter frames are refused until the next key frame.
    d.reset();
    assert_eq!(
        d.decode(&inter.write(), 3).err(),
        Some(Error::MissingReference)
    );
    Ok(())
}

#[test]
fn the_decoder_refuses_profiles_1_and_3_before_decoding() {
    let mut d = Decoder::new(Recorder::default());
    let mut frame = Frame::key(64, 64);
    frame.profile = 1;
    frame.subsampling = (true, false);
    assert!(matches!(
        d.decode(&frame.write(), 0),
        Err(Error::Unsupported(_))
    ));
    assert!(d.accelerator().configured.is_empty());
}

#[test]
fn the_decoder_gives_each_segment_its_parameters() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    let mut key = Frame::key(64, 64);
    key.base_q = 50;
    d.decode(&key.write(), 0)?;
    // Without segmentation, the first segment alone.
    let segments = d.accelerator().decoded[0].segments;
    assert_eq!(segments[0].quantizers.luma_ac, AC_QLOOKUP_8[50]);
    assert_eq!(segments[0].filter_levels, [[10, 10]; 4]);
    assert_eq!(segments[1], SegmentParams::default());
    key.seg = Some(SegWrite {
        update_map: Some(([None; 7], None)),
        update_data: Some((
            false,
            features(|s| match s {
                6 => [Some(10), None, Some(3), Some(0)],
                _ => [None; 4],
            }),
        )),
    });
    d.decode(&key.write(), 1)?;
    let segments = d.accelerator().decoded[1].segments;
    assert_eq!(segments[1].quantizers.luma_ac, AC_QLOOKUP_8[50]);
    assert_eq!(segments[6].quantizers.qindex, 60);
    assert!(segments[6].reference_enabled && segments[6].skip);
    assert_eq!(segments[6].reference, 3);
    assert!(!segments[5].reference_enabled);
    assert_eq!(d.accelerator().decoded[1].first_byte, key.write()[0]);
    Ok(())
}

#[test]
fn an_inter_frame_of_another_profile_than_its_stream_is_refused() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    d.decode(&Frame::key(320, 240).write(), 0)?;
    let mut inter = Frame::inter(320, 240);
    inter.profile = 2;
    assert!(matches!(
        d.decode(&inter.write(), 1),
        Err(Error::InterFrameFormat)
    ));
    assert_eq!(d.accelerator().configured.len(), 1);
    Ok(())
}

#[test]
fn tiles_are_checked_against_their_size_markers() -> Result<(), Error> {
    let mut frame = Frame::key(1024, 64);
    frame.tile_cols_log2 = 2;
    let written = frame.write();
    // The builder's tile data, four bytes of 0xa5, reads as a first tile
    // far past the frame's end.
    let mut d = Decoder::new(Recorder::default());
    assert!(matches!(d.decode(&written, 0), Err(Error::Truncated)));
    let headers = &written[..written.len() - 4];
    let with_tiles = |tiles: &[u8]| [headers, tiles].concat();
    // Four tiles: three of one byte each behind their markers, the last
    // the rest.
    let good = with_tiles(&[0, 0, 0, 1, 7, 0, 0, 0, 1, 7, 0, 0, 0, 1, 7, 7]);
    assert_eq!(shown(&d.decode(&good, 1)?).len(), 1);
    // A tile of no bytes, behind its marker or as the last.
    let empty = with_tiles(&[0, 0, 0, 0, 0, 0, 0, 1, 7, 0, 0, 0, 1, 7, 7]);
    assert!(matches!(
        d.decode(&empty, 2),
        Err(Error::OutOfRange("tile_size"))
    ));
    let no_last = with_tiles(&[0, 0, 0, 1, 7, 0, 0, 0, 1, 7, 0, 0, 0, 1, 7]);
    assert!(matches!(d.decode(&no_last, 3), Err(Error::Truncated)));
    Ok(())
}

#[test]
fn after_a_frame_that_fails_only_a_key_frame_decodes() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    d.decode(&Frame::key(320, 240).write(), 0)?;
    d.decode(&Frame::inter(320, 240).write(), 1)?;
    // A frame of another profile fails, and nothing after it decodes:
    // inter frames, intra-only frames, shown existing frames, in their own
    // chunks or in one superframe.
    let mut other = Frame::inter(320, 240);
    other.profile = 2;
    assert_eq!(
        d.decode(&other.write(), 2).err(),
        Some(Error::InterFrameFormat)
    );
    let refused = [
        Frame::inter(320, 240).write(),
        Frame::intra_only(320, 240).write(),
        show_existing(0, 0),
        superframe(
            &[
                Frame::inter(320, 240).write(),
                Frame::inter(320, 240).write(),
            ],
            2,
        ),
    ];
    for (id, chunk) in (3..).zip(&refused) {
        assert_eq!(d.decode(chunk, id).err(), Some(Error::Unrecovered));
    }
    assert_eq!(d.accelerator().decoded.len(), 2);
    // A key frame that fails ends nothing; one that decodes ends it.
    let mut one = Frame::key(320, 240);
    one.profile = 1;
    one.subsampling = (true, false);
    assert!(matches!(
        d.decode(&one.write(), 7),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(
        d.decode(&Frame::inter(320, 240).write(), 8).err(),
        Some(Error::Unrecovered)
    );
    assert_eq!(
        shown(&d.decode(&Frame::key(320, 240).write(), 9)?),
        [(3, 9, false)]
    );
    assert_eq!(
        shown(&d.decode(&Frame::inter(320, 240).write(), 10)?),
        [(4, 10, false)]
    );
    // A frame that fails in a superframe fails those after it in the
    // chunk; those before it decode.
    let chunk = superframe(
        &[
            Frame::inter(320, 240).write(),
            other.write(),
            Frame::inter(320, 240).write(),
        ],
        2,
    );
    assert_eq!(d.decode(&chunk, 11).err(), Some(Error::InterFrameFormat));
    assert_eq!(d.accelerator().decoded.len(), 5);
    assert_eq!(
        d.decode(&Frame::inter(320, 240).write(), 12).err(),
        Some(Error::Unrecovered)
    );
    // A reset forgets it, as it forgets the slots.
    d.reset();
    assert_eq!(
        shown(&d.decode(&Frame::intra_only(320, 240).write(), 13)?),
        []
    );
    Ok(())
}

#[test]
fn a_frame_marked_failed_fails_the_frames_after_it() -> Result<(), Error> {
    let mut d = Decoder::new(Recorder::default());
    // Before any slot holds a frame there is nothing to fail: the start
    // waits for a key or an intra-only frame anyway.
    d.mark_failed();
    assert_eq!(
        d.decode(&Frame::inter(64, 64).write(), 0).err(),
        Some(Error::MissingReference)
    );
    assert!(d.decode(&Frame::intra_only(64, 64).write(), 1)?.is_empty());
    let mut inter = Frame::inter(64, 64);
    inter.refs = [2, 2, 2];
    assert_eq!(shown(&d.decode(&inter.write(), 2)?).len(), 1);
    // A decode the hardware fails, once it is done.
    d.mark_failed();
    assert_eq!(d.decode(&inter.write(), 3).err(), Some(Error::Unrecovered));
    assert_eq!(shown(&d.decode(&Frame::key(64, 64).write(), 4)?).len(), 1);
    assert_eq!(shown(&d.decode(&Frame::inter(64, 64).write(), 5)?).len(), 1);
    Ok(())
}

/// Each frame of a chunk is a step: decoded, shown again, or refused, as
/// Apple's VP9 decoder lends a guest buffer for each (`Decoder::steps`).
#[test]
fn each_frame_is_a_step_decoded_shown_again_or_refused() -> Result<(), Error> {
    let mut d = Decoder::new(FramesOnly);
    d.decode(&Frame::key(320, 240).write(), 0)?;
    let k = d.steps()[0].picture_id;
    assert_eq!(
        d.steps(),
        [Step {
            picture_id: k,
            shown_again: false,
            refused: false,
            width: 320,
            height: 240,
            references: [Some(k); 8],
            output: true,
        }]
    );
    // A hidden ALTREF into slot 2, then a shown frame into slot 0: a step
    // each, the hidden one not output.
    let mut altref = Frame::inter(320, 240);
    altref.show_frame = false;
    altref.refresh = 0x04;
    let mut next = Frame::inter(320, 240);
    next.refs = [0, 1, 2];
    next.refresh = 0x01;
    d.decode(&superframe(&[altref.write(), next.write()], 2), 1)?;
    let steps = d.take_steps();
    assert!(d.steps().is_empty(), "read once");
    let (a, n) = (steps[0].picture_id, steps[1].picture_id);
    let after_altref = [
        Some(k),
        Some(k),
        Some(a),
        Some(k),
        Some(k),
        Some(k),
        Some(k),
        Some(k),
    ];
    let mut after_next = after_altref;
    after_next[0] = Some(n);
    assert_eq!(
        steps
            .iter()
            .map(|s| (s.output, s.references))
            .collect::<Vec<_>>(),
        [(false, after_altref), (true, after_next)]
    );
    let decoded = reims_vgpu_video_core::backend::FrameDecoded::from(&steps[1]);
    assert_eq!(
        (decoded.picture, decoded.primary_reference, decoded.largest),
        (n, None, (320, 240))
    );
    // The ALTREF shown again: its own number, the slots as they were.
    d.decode(&show_existing(0, 2), 2)?;
    assert_eq!(
        d.steps(),
        [Step {
            picture_id: a,
            shown_again: true,
            refused: false,
            width: 320,
            height: 240,
            references: after_next,
            output: true,
        }]
    );
    // A superframe whose second frame fails: the first decoded, and none
    // output; the one that failed and the one after it refused, with no
    // slot holding a frame after them.
    let mut other = Frame::inter(320, 240);
    other.profile = 2;
    let chunk = superframe(
        &[
            Frame::inter(320, 240).write(),
            other.write(),
            Frame::inter(320, 240).write(),
        ],
        2,
    );
    assert_eq!(d.decode(&chunk, 3).err(), Some(Error::InterFrameFormat));
    let steps = d.take_steps();
    assert_eq!(
        steps
            .iter()
            .map(|s| (s.refused, s.output))
            .collect::<Vec<_>>(),
        [(false, false), (true, false), (true, false)]
    );
    assert!(steps[0].references.iter().all(Option::is_some));
    assert!(
        steps[1..]
            .iter()
            .all(|s| s.references == [None; 8] && !s.shown_again)
    );
    let numbers: std::collections::HashSet<u64> = steps.iter().map(|s| s.picture_id).collect();
    assert_eq!(
        numbers.len(),
        3,
        "every frame refused is a frame of its own"
    );
    // Each frame after it is refused until a key frame, a frame shown
    // again too; the key frame fills every slot again.
    assert_eq!(
        d.decode(&show_existing(0, 0), 4).err(),
        Some(Error::Unrecovered)
    );
    assert!(matches!(d.steps(), [step] if step.refused && !step.shown_again));
    d.decode(&Frame::key(320, 240).write(), 5)?;
    let key = d.take_steps();
    assert_eq!(key[0].references, [Some(key[0].picture_id); 8]);
    assert!(!key[0].refused);
    // A chunk whose superframe index runs past its data is one frame
    // refused.
    let mut chunk = superframe(
        &[
            Frame::inter(320, 240).write(),
            Frame::inter(320, 240).write(),
        ],
        2,
    );
    let index_start = chunk.len() - 6;
    chunk[index_start + 1] = 0xff;
    chunk[index_start + 2] = 0xff;
    assert!(d.decode(&chunk, 6).is_err());
    assert!(matches!(d.steps(), [step] if step.refused));
    Ok(())
}

mod fuzz;
