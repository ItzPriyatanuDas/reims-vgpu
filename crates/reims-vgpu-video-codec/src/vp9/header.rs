//! The uncompressed frame header (6.2, with its semantics in 7.2), and what
//! a frame inherits from those before it: the colour configuration of the
//! last intra frame, the loop filter's deltas and the segmentation
//! features, which only the frames that code them change, and which
//! `setup_past_independence` (8.4.1's process, 7.2's semantics) resets.

use super::Error;
use super::bits::BitReader;

/// The reference slots (`NUM_REF_FRAMES`).
pub const NUM_REF_FRAMES: usize = 8;
/// The references an inter frame names (`REFS_PER_FRAME`): LAST, GOLDEN and
/// ALTREF, in that order.
pub(crate) const REFS_PER_FRAME: usize = 3;
/// The segments (`MAX_SEGMENTS`).
pub const MAX_SEGMENTS: usize = 8;
/// The segment features (`SEG_LVL_MAX`): `SEG_LVL_ALT_Q`, `SEG_LVL_ALT_L`,
/// `SEG_LVL_REF_FRAME` and `SEG_LVL_SKIP`, in that order.
pub const SEG_LVL_MAX: usize = 4;
/// `SEG_LVL_ALT_Q`: a segment's quantizer index, absolute or a delta.
pub const SEG_LVL_ALT_Q: usize = 0;
/// `SEG_LVL_ALT_L`: a segment's loop filter level, absolute or a delta.
pub const SEG_LVL_ALT_L: usize = 1;
/// `SEG_LVL_REF_FRAME`: the reference a segment's blocks use.
pub const SEG_LVL_REF_FRAME: usize = 2;
/// `SEG_LVL_SKIP`: a segment's blocks have no residual.
pub const SEG_LVL_SKIP: usize = 3;
/// `MAX_LOOP_FILTER`.
pub(crate) const MAX_LOOP_FILTER: i32 = 63;

/// `frame_sync_code` (6.2.1).
const SYNC_CODE: [u32; 3] = [0x49, 0x83, 0x42];
/// `segmentation_feature_bits` and `segmentation_feature_signed` (7.2.10's
/// tables).
const FEATURE_BITS: [u32; SEG_LVL_MAX] = [8, 6, 2, 0];
const FEATURE_SIGNED: [bool; SEG_LVL_MAX] = [true, true, false, false];
/// The loop filter's reference deltas after `setup_past_independence`:
/// INTRA_FRAME 1, LAST_FRAME 0, GOLDEN_FRAME and ALTREF_FRAME -1.
pub(crate) const DEFAULT_REF_DELTAS: [i8; 4] = [1, 0, -1, -1];
/// `MIN_TILE_WIDTH_B64` and `MAX_TILE_WIDTH_B64`.
const MIN_TILE_WIDTH_B64: u32 = 4;
const MAX_TILE_WIDTH_B64: u32 = 64;

/// `frame_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    /// `KEY_FRAME`, 0.
    Key,
    /// `NON_KEY_FRAME`, 1: an inter frame, or an intra-only one.
    NonKey,
}

/// `CS_RGB`: sRGB, which is 4:4:4. `color_space` (7.2.2) is kept as
/// coded: 0 unknown, 1 BT.601, 2 BT.709, 3 SMPTE 170, 4 SMPTE 240, 5
/// BT.2020, 6 reserved, 7 sRGB.
pub const CS_RGB: u8 = 7;
/// `CS_BT_601`, which an intra-only frame of Profile 0 takes.
pub(crate) const CS_BT_601: u8 = 1;

/// A frame's colour configuration (6.2.2): coded by key frames, and by
/// intra-only frames above Profile 0; inherited by the frames after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorConfig {
    /// `BitDepth`: 8, 10 or 12.
    pub bit_depth: u8,
    /// `color_space`.
    pub color_space: u8,
    /// `color_range`: full range (0 to 255 at 8 bits) when set, the
    /// studio range when clear.
    pub full_range: bool,
    /// `subsampling_x`.
    pub subsampling_x: bool,
    /// `subsampling_y`.
    pub subsampling_y: bool,
}

impl ColorConfig {
    /// What an intra-only frame of Profile 0 takes, coding none (6.2):
    /// BT.601, 4:2:0, 8 bits. Its range is the studio range, as libvpx's
    /// decoder sets it.
    pub(crate) const PROFILE_0_INTRA_ONLY: Self = Self {
        bit_depth: 8,
        color_space: CS_BT_601,
        full_range: false,
        subsampling_x: true,
        subsampling_y: true,
    };
}

/// `interp_filter` (7.2.7), in the specification's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpolationFilter {
    /// `EIGHTTAP_SMOOTH`, 0.
    EightTapSmooth,
    /// `EIGHTTAP`, 1: the regular filter.
    EightTap,
    /// `EIGHTTAP_SHARP`, 2.
    EightTapSharp,
    /// `BILINEAR`, 3.
    Bilinear,
    /// `SWITCHABLE`, 4: chosen block by block.
    Switchable,
}

impl InterpolationFilter {
    /// `literal_to_type` (7.2.7): the filter a two-bit literal codes.
    fn from_literal(literal: u8) -> Self {
        match literal {
            0 => Self::EightTapSmooth,
            1 => Self::EightTap,
            2 => Self::EightTapSharp,
            _ => Self::Bilinear,
        }
    }
}

/// The loop filter's parameters (6.2.8): the level and sharpness are
/// coded by every frame, the deltas only when updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopFilter {
    /// `loop_filter_level`, 0 to 63.
    pub level: u8,
    /// `loop_filter_sharpness`, 0 to 7.
    pub sharpness: u8,
    /// `loop_filter_delta_enabled`.
    pub delta_enabled: bool,
    /// `loop_filter_delta_update`: this frame codes deltas.
    pub delta_update: bool,
    /// `loop_filter_ref_deltas`, in force: INTRA_FRAME, LAST_FRAME,
    /// GOLDEN_FRAME, ALTREF_FRAME.
    pub ref_deltas: [i8; 4],
    /// `loop_filter_mode_deltas`, in force.
    pub mode_deltas: [i8; 2],
}

/// The quantizer's parameters (6.2.9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quantization {
    /// `base_q_idx`.
    pub base_q_idx: u8,
    /// `delta_q_y_dc`.
    pub delta_q_y_dc: i8,
    /// `delta_q_uv_dc`.
    pub delta_q_uv_dc: i8,
    /// `delta_q_uv_ac`.
    pub delta_q_uv_ac: i8,
}

impl Quantization {
    /// `Lossless` (7.2.9): every index and delta zero, so that the frame
    /// uses the Walsh-Hadamard transform.
    pub fn lossless(&self) -> bool {
        self.base_q_idx == 0
            && self.delta_q_y_dc == 0
            && self.delta_q_uv_dc == 0
            && self.delta_q_uv_ac == 0
    }
}

/// The segmentation parameters (6.2.11), with the features and
/// probabilities in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segmentation {
    /// `segmentation_enabled`.
    pub enabled: bool,
    /// `segmentation_update_map`: this frame codes a segment map.
    pub update_map: bool,
    /// `segmentation_temporal_update`: the map is coded against the
    /// previous frame's. Clear when the map is not updated.
    pub temporal_update: bool,
    /// `segmentation_update_data`: this frame codes the features.
    pub update_data: bool,
    /// `segmentation_abs_or_delta_update`, in force: the features' values
    /// replace the frame's rather than adjust them.
    pub abs_or_delta_update: bool,
    /// `segmentation_tree_probs`, in force.
    pub tree_probs: [u8; 7],
    /// `segmentation_pred_prob`: 255 each when the map is not coded
    /// against the previous one.
    pub pred_probs: [u8; 3],
    /// `FeatureEnabled`, in force, by segment and feature.
    pub feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    /// `FeatureData`, in force.
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
}

impl Segmentation {
    /// `seg_feature_active( feature )` for `segment` (9.3's helper): the
    /// feature applies when segmentation is enabled and the segment has
    /// it.
    pub fn feature_active(&self, segment: usize, feature: usize) -> bool {
        self.active_feature(segment, feature).is_some()
    }

    /// `FeatureData[ segment ][ feature ]`, when the feature is active for
    /// the segment.
    pub(crate) fn active_feature(&self, segment: usize, feature: usize) -> Option<i16> {
        let enabled = *self.feature_enabled.get(segment)?.get(feature)?;
        (self.enabled && enabled)
            .then(|| self.feature_data.get(segment)?.get(feature).copied())
            .flatten()
    }
}

impl Default for Segmentation {
    fn default() -> Self {
        Self {
            enabled: false,
            update_map: false,
            temporal_update: false,
            update_data: false,
            abs_or_delta_update: false,
            tree_probs: [255; 7],
            pred_probs: [255; 3],
            feature_enabled: [[false; SEG_LVL_MAX]; MAX_SEGMENTS],
            feature_data: [[0; SEG_LVL_MAX]; MAX_SEGMENTS],
        }
    }
}

/// The tiles (6.2.12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileInfo {
    /// `tile_cols_log2`.
    pub cols_log2: u8,
    /// `tile_rows_log2`, 0 to 2.
    pub rows_log2: u8,
}

/// What a reference slot holds, as the header needs it: the size and
/// format of the frame stored there (`RefFrameWidth` and the others,
/// 8.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceInfo {
    /// `RefFrameWidth`.
    pub width: u32,
    /// `RefFrameHeight`.
    pub height: u32,
    /// The colour configuration the frame was decoded with.
    pub color: ColorConfig,
}

/// What frames inherit: the state the header's parsing reads and changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderState {
    /// The colour configuration of the last key or intra-only frame.
    pub(crate) color: Option<ColorConfig>,
    /// The loop filter deltas in force.
    pub(crate) ref_deltas: [i8; 4],
    /// The loop filter mode deltas in force.
    pub(crate) mode_deltas: [i8; 2],
    /// The segmentation features, probabilities and their mode in force.
    pub(crate) segmentation: Segmentation,
}

impl Default for HeaderState {
    fn default() -> Self {
        Self {
            color: None,
            ref_deltas: DEFAULT_REF_DELTAS,
            mode_deltas: [0; 2],
            segmentation: Segmentation::default(),
        }
    }
}

impl HeaderState {
    /// `setup_past_independence` (7.2's semantics of `reset_frame_context`
    /// and 8.4.1), as far as the header's state goes: the segmentation
    /// features cleared, the loop filter deltas at their defaults.
    fn setup_past_independence(&mut self) {
        self.segmentation.feature_enabled = [[false; SEG_LVL_MAX]; MAX_SEGMENTS];
        self.segmentation.feature_data = [[0; SEG_LVL_MAX]; MAX_SEGMENTS];
        self.segmentation.abs_or_delta_update = false;
        self.ref_deltas = DEFAULT_REF_DELTAS;
        self.mode_deltas = [0; 2];
    }
}

/// One frame's uncompressed header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// `Profile`, 0 to 3.
    pub profile: u8,
    /// `show_existing_frame`: the frame is a slot's frame shown again, and
    /// nothing else of the header is coded.
    pub show_existing_frame: bool,
    /// `frame_to_show_map_idx`, for a shown existing frame.
    pub frame_to_show_map_idx: u8,
    /// `frame_type`.
    pub frame_type: FrameType,
    /// `show_frame`.
    pub show_frame: bool,
    /// `error_resilient_mode`.
    pub error_resilient_mode: bool,
    /// `intra_only`.
    pub intra_only: bool,
    /// `reset_frame_context`, 0 when not coded.
    pub reset_frame_context: u8,
    /// The colour configuration the frame is decoded with: its own for a
    /// key or intra-only frame, its intra frame's for an inter frame.
    pub color: ColorConfig,
    /// `refresh_frame_flags`: the slots the frame is stored in.
    pub refresh_frame_flags: u8,
    /// `ref_frame_idx`: the slots LAST, GOLDEN and ALTREF are taken from.
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    /// `ref_frame_sign_bias` of LAST, GOLDEN and ALTREF.
    pub ref_frame_sign_bias: [bool; REFS_PER_FRAME],
    /// `FrameWidth`.
    pub width: u32,
    /// `FrameHeight`.
    pub height: u32,
    /// For an inter frame whose size is a reference's (`found_ref`), which
    /// of LAST, GOLDEN and ALTREF.
    pub size_from_ref: Option<u8>,
    /// `RenderWidth`: the width the stream suggests showing the frame at.
    pub render_width: u32,
    /// `RenderHeight`.
    pub render_height: u32,
    /// `allow_high_precision_mv`, clear for an intra frame.
    pub allow_high_precision_mv: bool,
    /// `interp_filter`, `EightTap` for an intra frame, which codes none.
    pub interpolation_filter: InterpolationFilter,
    /// `refresh_frame_context`.
    pub refresh_frame_context: bool,
    /// `frame_parallel_decoding_mode`.
    pub frame_parallel_decoding_mode: bool,
    /// `frame_context_idx` as the frame's decoding uses it: 0 for an intra
    /// or error resilient frame, which `setup_past_independence` sets so
    /// (6.2).
    pub frame_context_idx: u8,
    /// `frame_context_idx` as coded.
    pub coded_frame_context_idx: u8,
    /// The loop filter, with the deltas in force.
    pub loop_filter: LoopFilter,
    /// The quantizer.
    pub quantization: Quantization,
    /// The segmentation, with the features in force.
    pub segmentation: Segmentation,
    /// The tiles.
    pub tile_info: TileInfo,
    /// `header_size_in_bytes`: the compressed header's size, 0 for a
    /// shown existing frame.
    pub header_size_in_bytes: u16,
    /// The bytes the uncompressed header takes, its trailing bits
    /// included: where the compressed header starts.
    pub uncompressed_header_size: usize,
}

impl FrameHeader {
    /// Whether the frame is intra coded (`FrameIsIntra`).
    pub fn is_intra(&self) -> bool {
        self.frame_type == FrameType::Key || self.intra_only
    }

    /// `MiCols`, the frame's width in 8x8 blocks.
    pub(crate) fn mi_cols(&self) -> u32 {
        self.width.div_ceil(8)
    }

    /// Where the compressed header ends in the frame: after the
    /// uncompressed header and `header_size_in_bytes`.
    pub(crate) fn compressed_header_end(&self) -> Result<usize, Error> {
        (self.uncompressed_header_size)
            .checked_add(usize::from(self.header_size_in_bytes))
            .ok_or(Error::Overflow("the compressed header's end"))
    }

    /// `Sb64Cols`, its width in 64x64 superblocks.
    pub fn sb64_cols(&self) -> u32 {
        self.mi_cols().div_ceil(8)
    }

    /// Parses the uncompressed header at the start of `frame`, one frame of
    /// a chunk (`uncompressed_header()`, then `trailing_bits()`).
    ///
    /// `state` is what earlier frames left, and is updated with what this
    /// one changes; `refs` is what each slot holds, for the size of an
    /// inter frame that takes a reference's. The frame must hold its
    /// compressed header too. A shown existing frame reads only its first
    /// byte.
    pub fn parse(
        frame: &[u8],
        state: &mut HeaderState,
        refs: &[Option<ReferenceInfo>; NUM_REF_FRAMES],
    ) -> Result<Self, Error> {
        let mut r = BitReader::new(frame);
        if r.bits(2)? != 2 {
            return Err(Error::OutOfRange("frame_marker"));
        }
        let profile_low_bit = r.u8(1)?;
        let profile_high_bit = r.u8(1)?;
        let profile = (profile_high_bit << 1) | profile_low_bit;
        if profile == 3 && r.bit()? {
            return Err(Error::OutOfRange("reserved_zero"));
        }
        let mut header = Self {
            profile,
            show_existing_frame: false,
            frame_to_show_map_idx: 0,
            frame_type: FrameType::NonKey,
            show_frame: true,
            error_resilient_mode: false,
            intra_only: false,
            reset_frame_context: 0,
            color: state.color.unwrap_or(ColorConfig::PROFILE_0_INTRA_ONLY),
            refresh_frame_flags: 0,
            ref_frame_idx: [0; REFS_PER_FRAME],
            ref_frame_sign_bias: [false; REFS_PER_FRAME],
            width: 0,
            height: 0,
            size_from_ref: None,
            render_width: 0,
            render_height: 0,
            allow_high_precision_mv: false,
            interpolation_filter: InterpolationFilter::EightTap,
            refresh_frame_context: false,
            frame_parallel_decoding_mode: false,
            frame_context_idx: 0,
            coded_frame_context_idx: 0,
            loop_filter: LoopFilter {
                level: 0,
                sharpness: 0,
                delta_enabled: false,
                delta_update: false,
                ref_deltas: state.ref_deltas,
                mode_deltas: state.mode_deltas,
            },
            quantization: Quantization::default(),
            segmentation: state.segmentation,
            tile_info: TileInfo::default(),
            header_size_in_bytes: 0,
            uncompressed_header_size: 0,
        };
        header.show_existing_frame = r.bit()?;
        if header.show_existing_frame {
            header.frame_to_show_map_idx = r.u8(3)?;
            header.uncompressed_header_size = r.bytes_used();
            return Ok(header);
        }
        header.frame_type = if r.bit()? {
            FrameType::NonKey
        } else {
            FrameType::Key
        };
        header.show_frame = r.bit()?;
        header.error_resilient_mode = r.bit()?;
        if header.frame_type == FrameType::Key {
            frame_sync_code(&mut r)?;
            header.color = color_config(&mut r, profile)?;
            header.refresh_frame_flags = 0xff;
            frame_size(&mut r, &mut header)?;
            render_size(&mut r, &mut header)?;
        } else {
            header.intra_only = if header.show_frame { false } else { r.bit()? };
            header.reset_frame_context = if header.error_resilient_mode {
                0
            } else {
                r.u8(2)?
            };
            if header.intra_only {
                frame_sync_code(&mut r)?;
                header.color = if profile > 0 {
                    color_config(&mut r, profile)?
                } else {
                    ColorConfig::PROFILE_0_INTRA_ONLY
                };
                header.refresh_frame_flags = r.u8(8)?;
                frame_size(&mut r, &mut header)?;
                render_size(&mut r, &mut header)?;
            } else {
                header.color = state.color.ok_or(Error::MissingReference)?;
                header.refresh_frame_flags = r.u8(8)?;
                for (index, sign_bias) in header
                    .ref_frame_idx
                    .iter_mut()
                    .zip(&mut header.ref_frame_sign_bias)
                {
                    *index = r.u8(3)?;
                    *sign_bias = r.bit()?;
                }
                frame_size_with_refs(&mut r, &mut header, refs)?;
                header.allow_high_precision_mv = r.bit()?;
                header.interpolation_filter = if r.bit()? {
                    InterpolationFilter::Switchable
                } else {
                    InterpolationFilter::from_literal(r.u8(2)?)
                };
                check_references(&header, refs)?;
            }
        }
        if header.error_resilient_mode {
            header.refresh_frame_context = false;
            header.frame_parallel_decoding_mode = true;
        } else {
            header.refresh_frame_context = r.bit()?;
            header.frame_parallel_decoding_mode = r.bit()?;
        }
        header.coded_frame_context_idx = r.u8(2)?;
        header.frame_context_idx = header.coded_frame_context_idx;
        let mut next = *state;
        if header.is_intra() {
            next.color = Some(header.color);
        }
        if header.is_intra() || header.error_resilient_mode {
            // The probability contexts' resets are the accelerator's: it is
            // given reset_frame_context and the frame's type.
            next.setup_past_independence();
            header.frame_context_idx = 0;
        }
        loop_filter_params(&mut r, &mut header, &mut next)?;
        quantization_params(&mut r, &mut header)?;
        segmentation_params(&mut r, &mut header, &mut next)?;
        tile_info(&mut r, &mut header)?;
        header.header_size_in_bytes = r.u16(16)?;
        if header.header_size_in_bytes == 0 {
            return Err(Error::OutOfRange("header_size_in_bytes"));
        }
        header.uncompressed_header_size = r.bytes_used();
        if frame.len() < header.compressed_header_end()? {
            return Err(Error::Truncated);
        }
        *state = next;
        Ok(header)
    }
}

fn frame_sync_code(r: &mut BitReader<'_>) -> Result<(), Error> {
    for byte in SYNC_CODE {
        if r.bits(8)? != byte {
            return Err(Error::OutOfRange("frame_sync_code"));
        }
    }
    Ok(())
}

/// `color_config()` (6.2.2).
fn color_config(r: &mut BitReader<'_>, profile: u8) -> Result<ColorConfig, Error> {
    let bit_depth = if profile >= 2 {
        if r.bit()? { 12 } else { 10 }
    } else {
        8
    };
    let color_space = r.u8(3)?;
    let four_four_four_allowed = profile == 1 || profile == 3;
    let (full_range, subsampling_x, subsampling_y) = if color_space == CS_RGB {
        if four_four_four_allowed && r.bit()? {
            return Err(Error::OutOfRange("reserved_zero"));
        }
        // sRGB is 4:4:4; Profiles 0 and 2 have no 4:4:4, so their sRGB
        // frames are malformed.
        if !four_four_four_allowed {
            return Err(Error::OutOfRange("color_space"));
        }
        (true, false, false)
    } else {
        let full_range = r.bit()?;
        if four_four_four_allowed {
            let x = r.bit()?;
            let y = r.bit()?;
            if r.bit()? {
                return Err(Error::OutOfRange("reserved_zero"));
            }
            (full_range, x, y)
        } else {
            (full_range, true, true)
        }
    };
    Ok(ColorConfig {
        bit_depth,
        color_space,
        full_range,
        subsampling_x,
        subsampling_y,
    })
}

/// `frame_size()` (6.2.3).
fn frame_size(r: &mut BitReader<'_>, header: &mut FrameHeader) -> Result<(), Error> {
    header.width = u32::from(r.u16(16)?) + 1;
    header.height = u32::from(r.u16(16)?) + 1;
    Ok(())
}

/// `render_size()` (6.2.4).
fn render_size(r: &mut BitReader<'_>, header: &mut FrameHeader) -> Result<(), Error> {
    if r.bit()? {
        header.render_width = u32::from(r.u16(16)?) + 1;
        header.render_height = u32::from(r.u16(16)?) + 1;
    } else {
        header.render_width = header.width;
        header.render_height = header.height;
    }
    Ok(())
}

/// `frame_size_with_refs()` (6.2.5).
fn frame_size_with_refs(
    r: &mut BitReader<'_>,
    header: &mut FrameHeader,
    refs: &[Option<ReferenceInfo>; NUM_REF_FRAMES],
) -> Result<(), Error> {
    for (i, &index) in (0u8..).zip(&header.ref_frame_idx) {
        // found_ref
        if r.bit()? {
            let slot = reference(refs, index)?;
            header.width = slot.width;
            header.height = slot.height;
            header.size_from_ref = Some(i);
            break;
        }
    }
    if header.size_from_ref.is_none() {
        frame_size(r, header)?;
    }
    render_size(r, header)
}

/// What reference slot `index` holds: a header's `ref_frame_idx` is three
/// bits, so that it names one of the eight, which may be empty.
fn reference(
    refs: &[Option<ReferenceInfo>; NUM_REF_FRAMES],
    index: u8,
) -> Result<ReferenceInfo, Error> {
    refs.get(usize::from(index))
        .copied()
        .flatten()
        .ok_or(Error::MissingReference)
}

/// What an inter frame's references must be (7.2.5's conformance
/// requirements, as libvpx's decoder checks them): each holds a frame of
/// the frame's own colour format, and at least one is within the scaling
/// the motion vector prediction takes, at most twice as large and at
/// least a sixteenth as large in each dimension. A reference outside it
/// may still be named, and is the accelerator's to refuse if a block uses
/// it.
fn check_references(
    header: &FrameHeader,
    refs: &[Option<ReferenceInfo>; NUM_REF_FRAMES],
) -> Result<(), Error> {
    let mut any_in_range = false;
    for &index in &header.ref_frame_idx {
        let slot = reference(refs, index)?;
        if slot.color.bit_depth != header.color.bit_depth
            || slot.color.subsampling_x != header.color.subsampling_x
            || slot.color.subsampling_y != header.color.subsampling_y
        {
            return Err(Error::ReferenceFormat);
        }
        let times = |size: u32, by: u32| {
            size.checked_mul(by)
                .ok_or(Error::Overflow("a scaled FrameWidth or FrameHeight"))
        };
        any_in_range |= times(header.width, 2)? >= slot.width
            && times(header.height, 2)? >= slot.height
            && header.width <= times(slot.width, 16)?
            && header.height <= times(slot.height, 16)?;
    }
    if any_in_range {
        Ok(())
    } else {
        Err(Error::NoReferenceInScale)
    }
}

/// `loop_filter_params()` (6.2.8). The deltas a frame does not code are
/// those in force.
fn loop_filter_params(
    r: &mut BitReader<'_>,
    header: &mut FrameHeader,
    state: &mut HeaderState,
) -> Result<(), Error> {
    let lf = &mut header.loop_filter;
    lf.level = r.u8(6)?;
    lf.sharpness = r.u8(3)?;
    lf.delta_enabled = r.bit()?;
    if lf.delta_enabled {
        lf.delta_update = r.bit()?;
        if lf.delta_update {
            for delta in &mut state.ref_deltas {
                if r.bit()? {
                    *delta = narrow(r.signed(6)?);
                }
            }
            for delta in &mut state.mode_deltas {
                if r.bit()? {
                    *delta = narrow(r.signed(6)?);
                }
            }
        }
    }
    lf.ref_deltas = state.ref_deltas;
    lf.mode_deltas = state.mode_deltas;
    Ok(())
}

/// A delta of at most six or four bits and a sign, as an `i8`.
fn narrow(value: i16) -> i8 {
    i8::try_from(value).unwrap_or(0)
}

/// `quantization_params()` (6.2.9).
fn quantization_params(r: &mut BitReader<'_>, header: &mut FrameHeader) -> Result<(), Error> {
    let delta_q = |r: &mut BitReader<'_>| -> Result<i8, Error> {
        Ok(if r.bit()? { narrow(r.signed(4)?) } else { 0 })
    };
    let q = &mut header.quantization;
    q.base_q_idx = r.u8(8)?;
    q.delta_q_y_dc = delta_q(r)?;
    q.delta_q_uv_dc = delta_q(r)?;
    q.delta_q_uv_ac = delta_q(r)?;
    Ok(())
}

/// `segmentation_params()` (6.2.11). What a frame does not code is what is
/// in force: the probabilities when the map is not updated, and the
/// features when the data is not.
fn segmentation_params(
    r: &mut BitReader<'_>,
    header: &mut FrameHeader,
    state: &mut HeaderState,
) -> Result<(), Error> {
    let s = &mut state.segmentation;
    s.enabled = r.bit()?;
    s.update_map = false;
    s.update_data = false;
    s.temporal_update = false;
    if s.enabled {
        s.update_map = r.bit()?;
        if s.update_map {
            for prob in &mut s.tree_probs {
                *prob = read_prob(r)?;
            }
            s.temporal_update = r.bit()?;
            for prob in &mut s.pred_probs {
                *prob = if s.temporal_update {
                    read_prob(r)?
                } else {
                    255
                };
            }
        }
        s.update_data = r.bit()?;
        if s.update_data {
            s.abs_or_delta_update = r.bit()?;
            for (segment_enabled, segment_data) in
                s.feature_enabled.iter_mut().zip(&mut s.feature_data)
            {
                for ((enabled, value), (&bits, &signed)) in segment_enabled
                    .iter_mut()
                    .zip(segment_data.iter_mut())
                    .zip(FEATURE_BITS.iter().zip(&FEATURE_SIGNED))
                {
                    *value = 0;
                    *enabled = r.bit()?;
                    if *enabled {
                        // feature_value, of at most 8 bits, and its sign.
                        *value = i16::try_from(r.bits(bits)?).unwrap_or(0);
                        if signed && r.bit()? {
                            *value = value
                                .checked_neg()
                                .ok_or(Error::Overflow("feature_value"))?;
                        }
                    }
                }
            }
        }
    }
    header.segmentation = *s;
    Ok(())
}

/// `read_prob()` (6.2.11): a coded probability, or 255.
fn read_prob(r: &mut BitReader<'_>) -> Result<u8, Error> {
    if r.bit()? { r.u8(8) } else { Ok(255) }
}

/// `tile_info()` (6.2.12), with `calc_min_log2_tile_cols` and
/// `calc_max_log2_tile_cols`.
fn tile_info(r: &mut BitReader<'_>, header: &mut FrameHeader) -> Result<(), Error> {
    let sb64_cols = header.sb64_cols();
    // calc_min_log2_tile_cols(): the least minLog2 for which
    // MAX_TILE_WIDTH_B64 << minLog2 reaches Sb64Cols. In 64 bits, where a
    // shift of up to 32 keeps every bit, so 32 reaches any Sb64Cols.
    let min_log2 = (0..=u32::BITS)
        .find(|&k| u64::from(MAX_TILE_WIDTH_B64) << k >= u64::from(sb64_cols))
        .and_then(|k| u8::try_from(k).ok())
        .ok_or(Error::Overflow("minLog2"))?;
    // calc_max_log2_tile_cols(): one less than the least maxLog2 from 1 on
    // for which Sb64Cols >> maxLog2 is below MIN_TILE_WIDTH_B64, which is
    // the count of those before it where it is not.
    let max_log2 = (1..u32::BITS)
        .take_while(|&k| sb64_cols >> k >= MIN_TILE_WIDTH_B64)
        .count();
    let max_log2 = u8::try_from(max_log2).map_err(|_| Error::Overflow("maxLog2"))?;
    let mut cols_log2 = min_log2;
    while cols_log2 < max_log2 {
        if r.bit()? {
            // increment_tile_cols_log2, below maxLog2.
            cols_log2 = cols_log2
                .checked_add(1)
                .ok_or(Error::Overflow("tile_cols_log2"))?;
        } else {
            break;
        }
    }
    let mut rows_log2 = r.u8(1)?;
    if rows_log2 == 1 {
        // increment_tile_rows_log2.
        rows_log2 = rows_log2
            .checked_add(r.u8(1)?)
            .ok_or(Error::Overflow("tile_rows_log2"))?;
    }
    header.tile_info = TileInfo {
        cols_log2,
        rows_log2,
    };
    Ok(())
}
