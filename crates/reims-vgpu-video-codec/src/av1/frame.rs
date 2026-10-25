//! The frame header OBU's uncompressed header (5.9, 6.8), with the values
//! it inherits from the eight reference slots: segmentation features,
//! loop filter deltas, global motion and film grain (7.20, 7.21). Every
//! field here holds its effective value, as the decoding process uses it,
//! not merely what was coded.

use super::Error;
use super::bits::BitReader;
use super::obu::in_layers;
use super::sequence::{SELECT, SequenceHeader};
use crate::num::ToUsize;
use std::num::NonZeroU32;

/// `NUM_REF_FRAMES`.
pub const NUM_REF_FRAMES: usize = 8;
/// `REFS_PER_FRAME`.
pub(crate) const REFS_PER_FRAME: usize = 7;
/// `PRIMARY_REF_NONE`.
pub(crate) const PRIMARY_REF_NONE: u8 = 7;
/// `MAX_SEGMENTS`.
pub(crate) const MAX_SEGMENTS: usize = 8;
/// `SEG_LVL_MAX`.
pub(crate) const SEG_LVL_MAX: usize = 8;
/// `MAX_TILE_COLS`.
pub(crate) const MAX_TILE_COLS: u32 = 64;
/// `MAX_TILE_ROWS`.
pub(crate) const MAX_TILE_ROWS: u32 = 64;
/// The most tiles a frame may have here, of the 4,096 the syntax allows
/// (ours): Mesa's VA-API decoder and VCN hold 256 (`AV1_MAX_NUM_TILES`,
/// `ac_video_dec.h`, read in Mesa's source), and drop the rest while the
/// firmware is told of them all.
pub(crate) const MAX_TILES: usize = 256;
/// `WARPEDMODEL_PREC_BITS`.
const WARPEDMODEL_PREC_BITS: u32 = 16;
/// `IDENTITY`, `TRANSLATION`, `ROTZOOM` and `AFFINE`: the types of a
/// global motion model.
const IDENTITY: u8 = 0;
const TRANSLATION: u8 = 1;
const ROTZOOM: u8 = 2;
const AFFINE: u8 = 3;

/// `frame_type` (6.8.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameType {
    /// `KEY_FRAME`.
    #[default]
    Key = 0,
    /// `INTER_FRAME`.
    Inter = 1,
    /// `INTRA_ONLY_FRAME`.
    IntraOnly = 2,
    /// `SWITCH_FRAME`.
    Switch = 3,
}

impl FrameType {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::Key,
            1 => Self::Inter,
            2 => Self::IntraOnly,
            _ => Self::Switch,
        }
    }

    /// `FrameIsIntra`.
    pub fn is_intra(self) -> bool {
        matches!(self, Self::Key | Self::IntraOnly)
    }
}

/// A global motion model (5.9.24): its type, `IDENTITY` (0) to `AFFINE`
/// (3), and `gm_params`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalMotion {
    /// `GmType`.
    pub kind: u8,
    /// `gm_params`.
    pub params: [i32; 6],
    /// `warpValid` of `setup_shear()` for the parameters, as the parse
    /// works it out.
    shear_valid: bool,
}

impl Default for GlobalMotion {
    fn default() -> Self {
        Self {
            kind: 0,
            params: [
                0,
                0,
                1 << WARPEDMODEL_PREC_BITS,
                0,
                0,
                1 << WARPEDMODEL_PREC_BITS,
            ],
            // The identity's shear: alpha, beta, gamma and delta all 0.
            shear_valid: true,
        }
    }
}

impl GlobalMotion {
    /// Whether the model's shear parameters are valid (`warpValid` of
    /// `setup_shear()`, 7.11.3.6). An accelerator that takes the model's
    /// invalidity takes its negation.
    pub fn shear_is_valid(&self) -> bool {
        self.shear_valid
    }
}

/// `warpValid` of `setup_shear()` (7.11.3.6) for the parameters `params`.
/// The parameters are those of `read_global_param()`, within 2^23 of
/// zero, so nothing here comes near 64 bits.
fn shear_valid(params: &[i32; 6]) -> Result<bool, Error> {
    let overflow = || Error::Overflow("setup_shear()");
    let p = params.map(i64::from);
    let one = 1i64 << WARPEDMODEL_PREC_BITS;
    let Some(divisor) = u32::try_from(p[2]).ok().and_then(NonZeroU32::new) else {
        // A non-positive warpParams[2] is not valid.
        return Ok(false);
    };
    let clip = |v: i64| v.clamp(-32768, 32767);
    let alpha0 = clip(p[2].checked_sub(one).ok_or_else(overflow)?);
    let beta0 = clip(p[3]);
    let (div_shift, div_factor) = resolve_divisor(divisor)?;
    let v = p[4] << WARPEDMODEL_PREC_BITS;
    let gamma0 = clip(round2_signed(
        v.checked_mul(div_factor).ok_or_else(overflow)?,
        div_shift,
    )?);
    let w = p[3].checked_mul(p[4]).ok_or_else(overflow)?;
    let delta0 = clip(
        p[5].checked_sub(round2_signed(
            w.checked_mul(div_factor).ok_or_else(overflow)?,
            div_shift,
        )?)
        .and_then(|delta| delta.checked_sub(one))
        .ok_or_else(overflow)?,
    );
    let reduce = |v: i64| round2_signed(v, 6).map(|v| v << 6);
    let (alpha, beta, gamma, delta) = (
        reduce(alpha0)?,
        reduce(beta0)?,
        reduce(gamma0)?,
        reduce(delta0)?,
    );
    // Each is within 16 bits once clipped, so the sums are far from 64.
    let sum = |a: i64, times_a: i64, b: i64, times_b: i64| {
        (a.abs().checked_mul(times_a))
            .zip(b.abs().checked_mul(times_b))
            .and_then(|(a, b)| a.checked_add(b))
            .ok_or_else(overflow)
    };
    Ok(sum(alpha, 4, beta, 7)? < one && sum(gamma, 4, delta, 4)? < one)
}

/// `Round2Signed()`, for `n` below 63.
fn round2_signed(x: i64, n: u32) -> Result<i64, Error> {
    let overflow = || Error::Overflow("Round2Signed()");
    // Round2(): (x + 2^(n - 1)) >> n, and x itself where n is 0.
    let half = 1i64.checked_shl(n).ok_or_else(overflow)? >> 1;
    let round = |x: i64| {
        (x.checked_add(half))
            .and_then(|x| x.checked_shr(n))
            .ok_or_else(overflow)
    };
    if x >= 0 {
        round(x)
    } else {
        round(x.checked_neg().ok_or_else(overflow)?)?
            .checked_neg()
            .ok_or_else(overflow)
    }
}

/// `resolve_divisor()` (7.11.3.7) of a positive `d`: `divShift` and
/// `divFactor`, with `Div_Lut` computed: its entries are 2^22 / (256 + i),
/// rounded to the nearest.
fn resolve_divisor(d: NonZeroU32) -> Result<(u32, i64), Error> {
    let overflow = || Error::Overflow("resolve_divisor()");
    // n, FloorLog2(d), and e, d - (1 << n): d without its highest bit.
    let n = d.ilog2();
    let e = u64::from(d.get() ^ (1 << n));
    // f: Round2(e, n - 8) above 8 bits, else e << (8 - n). Round2(e, k)
    // is ceil((e >> (k - 1)) / 2) for a k of 1 or more.
    let f = if n > 8 {
        (e >> n.abs_diff(9)).div_ceil(2)
    } else {
        e << n.abs_diff(8)
    };
    // f is 0 to 256: the 257 entries of Div_Lut.
    let entry = f.checked_add(256).ok_or_else(overflow)?;
    let lut = (1u64 << 22)
        .checked_add(entry / 2)
        .and_then(|numerator| numerator.checked_div(entry))
        .ok_or_else(overflow)?;
    let factor = i64::try_from(lut).map_err(|_| overflow())?;
    let shift = n.checked_add(14).ok_or_else(overflow)?;
    Ok((shift, factor))
}

/// `film_grain_params()` (5.9.30): every field, zero when grain is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilmGrain {
    /// `apply_grain`.
    pub apply_grain: bool,
    /// `grain_seed`.
    pub grain_seed: u16,
    /// `update_grain`.
    pub(crate) update_grain: bool,
    /// `num_y_points`.
    pub num_y_points: u8,
    /// `point_y_value`.
    pub point_y_value: [u8; 14],
    /// `point_y_scaling`.
    pub point_y_scaling: [u8; 14],
    /// `chroma_scaling_from_luma`.
    pub chroma_scaling_from_luma: bool,
    /// `num_cb_points`.
    pub num_cb_points: u8,
    /// `point_cb_value`.
    pub point_cb_value: [u8; 10],
    /// `point_cb_scaling`.
    pub point_cb_scaling: [u8; 10],
    /// `num_cr_points`.
    pub num_cr_points: u8,
    /// `point_cr_value`.
    pub point_cr_value: [u8; 10],
    /// `point_cr_scaling`.
    pub point_cr_scaling: [u8; 10],
    /// `grain_scaling_minus_8`.
    pub grain_scaling_minus_8: u8,
    /// `ar_coeff_lag`.
    pub ar_coeff_lag: u8,
    /// `ar_coeffs_y_plus_128`; 128 where none is coded.
    pub ar_coeffs_y_plus_128: [u8; 24],
    /// `ar_coeffs_cb_plus_128`; 128 where none is coded.
    pub ar_coeffs_cb_plus_128: [u8; 25],
    /// `ar_coeffs_cr_plus_128`; 128 where none is coded.
    pub ar_coeffs_cr_plus_128: [u8; 25],
    /// `ar_coeff_shift_minus_6`.
    pub ar_coeff_shift_minus_6: u8,
    /// `grain_scale_shift`.
    pub grain_scale_shift: u8,
    /// `cb_mult`.
    pub cb_mult: u8,
    /// `cb_luma_mult`.
    pub cb_luma_mult: u8,
    /// `cb_offset`.
    pub cb_offset: u16,
    /// `cr_mult`.
    pub cr_mult: u8,
    /// `cr_luma_mult`.
    pub cr_luma_mult: u8,
    /// `cr_offset`.
    pub cr_offset: u16,
    /// `overlap_flag`.
    pub overlap_flag: bool,
    /// `clip_to_restricted_range`.
    pub clip_to_restricted_range: bool,
}

impl Default for FilmGrain {
    /// `reset_grain_params()`.
    fn default() -> Self {
        Self {
            apply_grain: false,
            grain_seed: 0,
            update_grain: false,
            num_y_points: 0,
            point_y_value: [0; 14],
            point_y_scaling: [0; 14],
            chroma_scaling_from_luma: false,
            num_cb_points: 0,
            point_cb_value: [0; 10],
            point_cb_scaling: [0; 10],
            num_cr_points: 0,
            point_cr_value: [0; 10],
            point_cr_scaling: [0; 10],
            grain_scaling_minus_8: 0,
            ar_coeff_lag: 0,
            ar_coeffs_y_plus_128: [128; 24],
            ar_coeffs_cb_plus_128: [128; 25],
            ar_coeffs_cr_plus_128: [128; 25],
            ar_coeff_shift_minus_6: 0,
            grain_scale_shift: 0,
            cb_mult: 0,
            cb_luma_mult: 0,
            cb_offset: 0,
            cr_mult: 0,
            cr_luma_mult: 0,
            cr_offset: 0,
            overlap_flag: false,
            clip_to_restricted_range: false,
        }
    }
}

/// `tile_info()` (5.9.15), with the tiles' positions worked out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileInfo {
    /// `TileCols`.
    pub cols: u32,
    /// `TileRows`.
    pub rows: u32,
    /// `TileColsLog2`.
    pub cols_log2: u32,
    /// `TileRowsLog2`.
    pub rows_log2: u32,
    /// `uniform_tile_spacing_flag`.
    pub uniform: bool,
    /// Each tile column's width in superblocks.
    pub width_in_sbs: Vec<u32>,
    /// Each tile row's height in superblocks.
    pub height_in_sbs: Vec<u32>,
    /// `context_update_tile_id`.
    pub context_update_tile_id: u32,
    /// `TileSizeBytes`: the bytes of each tile's size in a tile group, 4
    /// when there is one tile and no size is coded.
    pub(crate) tile_size_bytes: u8,
    /// `NumTiles`, `TileCols * TileRows`, as the parse works it out.
    pub(crate) num_tiles: u32,
}

impl TileInfo {
    /// `NumTiles`.
    pub fn num_tiles(&self) -> u32 {
        self.num_tiles
    }
}

/// `quantization_params()` (5.9.12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quantization {
    /// `base_q_idx`.
    pub base_q_idx: u8,
    /// `DeltaQYDc`.
    pub delta_q_y_dc: i8,
    /// `DeltaQUDc`.
    pub delta_q_u_dc: i8,
    /// `DeltaQUAc`.
    pub delta_q_u_ac: i8,
    /// `DeltaQVDc`.
    pub delta_q_v_dc: i8,
    /// `DeltaQVAc`.
    pub delta_q_v_ac: i8,
    /// `using_qmatrix`.
    pub using_qmatrix: bool,
    /// `qm_y`.
    pub qm_y: u8,
    /// `qm_u`.
    pub qm_u: u8,
    /// `qm_v`.
    pub qm_v: u8,
}

/// `segmentation_params()` (5.9.14), the features with their inherited
/// values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segmentation {
    /// `segmentation_enabled`.
    pub enabled: bool,
    /// `segmentation_update_map`.
    pub update_map: bool,
    /// `segmentation_temporal_update`.
    pub temporal_update: bool,
    /// `segmentation_update_data`.
    pub update_data: bool,
    /// `FeatureEnabled`, a bit per feature for each segment.
    pub feature_mask: [u8; MAX_SEGMENTS],
    /// `FeatureData`, clipped.
    pub feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
}

/// `loop_filter_params()` (5.9.11), the deltas with their inherited values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopFilter {
    /// `loop_filter_level[0..4]`: luma vertical and horizontal, Cb, Cr.
    pub level: [u8; 4],
    /// `loop_filter_sharpness`.
    pub sharpness: u8,
    /// `loop_filter_delta_enabled`.
    pub delta_enabled: bool,
    /// `loop_filter_delta_update`.
    pub delta_update: bool,
    /// `loop_filter_ref_deltas`, by reference frame from `INTRA_FRAME`.
    pub ref_deltas: [i8; NUM_REF_FRAMES],
    /// `loop_filter_mode_deltas`.
    pub mode_deltas: [i8; 2],
}

/// The deltas `setup_past_independence()` gives (7.20).
pub(crate) const DEFAULT_REF_DELTAS: [i8; NUM_REF_FRAMES] = [1, 0, 0, 0, -1, 0, -1, -1];

impl Default for LoopFilter {
    fn default() -> Self {
        Self {
            level: [0; 4],
            sharpness: 0,
            delta_enabled: false,
            delta_update: false,
            ref_deltas: DEFAULT_REF_DELTAS,
            mode_deltas: [0; 2],
        }
    }
}

/// `cdef_params()` (5.9.19), the strengths as coded: a secondary strength
/// of 3 stays 3 here (the decoding process makes it 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cdef {
    /// `cdef_damping_minus_3`.
    pub damping_minus_3: u8,
    /// `cdef_bits`.
    pub bits: u8,
    /// `cdef_y_pri_strength`.
    pub y_pri_strength: [u8; 8],
    /// `cdef_y_sec_strength`, as coded.
    pub y_sec_strength: [u8; 8],
    /// `cdef_uv_pri_strength`.
    pub uv_pri_strength: [u8; 8],
    /// `cdef_uv_sec_strength`, as coded.
    pub uv_sec_strength: [u8; 8],
}

/// `lr_params()` (5.9.20).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopRestoration {
    /// `FrameRestorationType` per plane: `RESTORE_NONE` (0),
    /// `RESTORE_WIENER` (1), `RESTORE_SGRPROJ` (2), `RESTORE_SWITCHABLE`
    /// (3).
    pub frame_restoration_type: [u8; 3],
    /// `lr_unit_shift`, extra shift included: 0 to 2.
    pub lr_unit_shift: u8,
    /// `lr_uv_shift`.
    pub lr_uv_shift: u8,
}

/// What a reference slot keeps of the frame last stored in it (7.20): what
/// later frame headers read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReferenceState {
    /// `RefValid`.
    pub(crate) valid: bool,
    /// `RefFrameId`.
    pub(crate) frame_id: u32,
    /// `RefUpscaledWidth`.
    pub(crate) upscaled_width: u32,
    /// `RefFrameWidth`.
    pub(crate) frame_width: u32,
    /// `RefFrameHeight`.
    pub(crate) frame_height: u32,
    /// `RefRenderWidth`.
    pub(crate) render_width: u32,
    /// `RefRenderHeight`.
    pub(crate) render_height: u32,
    /// `RefFrameType`.
    pub(crate) frame_type: FrameType,
    /// `RefOrderHint`.
    pub(crate) order_hint: u32,
    /// `RefShowableFrame`: the frame's `showable_frame`.
    pub(crate) showable: bool,
    /// Whether the slot's key frame has been shown by `show_existing_frame`
    /// already. Ours, not the specification's: it bounds a key frame to one
    /// such show.
    pub(crate) key_shown: bool,
    /// `SavedGmParams`, by reference frame from `INTRA_FRAME`.
    pub(crate) global_motion: [GlobalMotion; NUM_REF_FRAMES],
    /// The saved loop filter deltas.
    pub(crate) loop_filter: LoopFilter,
    /// The saved segmentation features.
    pub(crate) segmentation: Segmentation,
    /// The saved film grain parameters.
    pub(crate) film_grain: FilmGrain,
}

/// A parsed uncompressed header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameHeader {
    /// `temporal_id` of the OBU it came in.
    pub temporal_id: u8,
    /// `spatial_id` of the OBU it came in.
    pub spatial_id: u8,
    /// `show_existing_frame`.
    pub show_existing_frame: bool,
    /// `frame_to_show_map_idx`.
    pub frame_to_show_map_idx: u8,
    /// `frame_type`; for a shown existing frame, the frame's.
    pub frame_type: FrameType,
    /// `show_frame`.
    pub show_frame: bool,
    /// `showable_frame`.
    pub showable_frame: bool,
    /// `error_resilient_mode`.
    pub error_resilient_mode: bool,
    /// `disable_cdf_update`.
    pub disable_cdf_update: bool,
    /// `allow_screen_content_tools`.
    pub allow_screen_content_tools: bool,
    /// `force_integer_mv`, 1 for intra frames.
    pub force_integer_mv: bool,
    /// `current_frame_id`.
    pub(crate) current_frame_id: u32,
    /// `frame_size_override_flag`.
    pub(crate) frame_size_override_flag: bool,
    /// `OrderHint`.
    pub order_hint: u32,
    /// `primary_ref_frame`.
    pub primary_ref_frame: u8,
    /// `refresh_frame_flags`.
    pub refresh_frame_flags: u8,
    /// `FrameWidth`: the coded width, after superres's downscaling.
    pub frame_width: u32,
    /// `FrameHeight`.
    pub frame_height: u32,
    /// `UpscaledWidth`: the width the frame is output at.
    pub upscaled_width: u32,
    /// `RenderWidth`.
    pub render_width: u32,
    /// `RenderHeight`.
    pub render_height: u32,
    /// `use_superres`.
    pub use_superres: bool,
    /// `SuperresDenom`: 8 without superres.
    pub superres_denom: u8,
    /// `MiCols`.
    pub(crate) mi_cols: u32,
    /// `MiRows`.
    pub(crate) mi_rows: u32,
    /// `allow_intrabc`.
    pub allow_intrabc: bool,
    /// `frame_refs_short_signaling`.
    pub(crate) frame_refs_short_signaling: bool,
    /// `ref_frame_idx`: the slot of each of `LAST_FRAME` to `ALTREF_FRAME`.
    pub ref_frame_idx: [u8; REFS_PER_FRAME],
    /// `allow_high_precision_mv`.
    pub allow_high_precision_mv: bool,
    /// `interpolation_filter`: 4 is `SWITCHABLE`.
    pub interpolation_filter: u8,
    /// `is_motion_mode_switchable`.
    pub is_motion_mode_switchable: bool,
    /// `use_ref_frame_mvs`.
    pub use_ref_frame_mvs: bool,
    /// `disable_frame_end_update_cdf`.
    pub disable_frame_end_update_cdf: bool,
    /// `tile_info()`.
    pub tile_info: TileInfo,
    /// `quantization_params()`.
    pub quantization: Quantization,
    /// `segmentation_params()`.
    pub segmentation: Segmentation,
    /// `delta_q_present`.
    pub delta_q_present: bool,
    /// `delta_q_res`.
    pub delta_q_res: u8,
    /// `delta_lf_present`.
    pub delta_lf_present: bool,
    /// `delta_lf_res`.
    pub delta_lf_res: u8,
    /// `delta_lf_multi`.
    pub delta_lf_multi: bool,
    /// `CodedLossless`.
    pub coded_lossless: bool,
    /// `AllLossless`.
    pub(crate) all_lossless: bool,
    /// `loop_filter_params()`.
    pub loop_filter: LoopFilter,
    /// `cdef_params()`.
    pub cdef: Cdef,
    /// `lr_params()`.
    pub loop_restoration: LoopRestoration,
    /// `TxMode`: `ONLY_4X4` (0), `TX_MODE_LARGEST` (1), `TX_MODE_SELECT`
    /// (2).
    pub tx_mode: u8,
    /// `reference_select`.
    pub reference_select: bool,
    /// `skip_mode_present`.
    pub skip_mode_present: bool,
    /// `SkipModeFrame[]` (5.9.22): the two references, `LAST_FRAME` (1)
    /// to `ALTREF_FRAME` (7), a skip mode block predicts from, where
    /// `skip_mode_present`; 0 otherwise.
    pub skip_mode_frame: [u8; 2],
    /// `allow_warped_motion`.
    pub allow_warped_motion: bool,
    /// `reduced_tx_set`.
    pub reduced_tx_set: bool,
    /// `gm_params` and `GmType`, by reference frame from `INTRA_FRAME`.
    pub global_motion: [GlobalMotion; NUM_REF_FRAMES],
    /// `film_grain_params()`, after `load_grain_params()`.
    pub film_grain: FilmGrain,
}

impl FrameHeader {
    /// Parses `uncompressed_header()` from a frame header or frame OBU's
    /// payload, and returns it with the bytes it took, its last one
    /// counted whole: where a frame OBU's `byte_alignment()` ends and its
    /// tile group starts.
    ///
    /// `refs` is the reference slots' state before this frame. What the
    /// header itself changes there (a shown key frame's reset of every
    /// slot, frame id and order hint checks) is done in it, as 5.9.2 does,
    /// so a caller that keeps `refs` only on success stays consistent.
    pub(crate) fn parse(
        payload: &[u8],
        sequence: &SequenceHeader,
        refs: &mut [ReferenceState; NUM_REF_FRAMES],
        temporal_id: u8,
        spatial_id: u8,
    ) -> Result<(Self, usize), Error> {
        let mut r = BitReader::new(payload);
        let mut h = Self {
            temporal_id,
            spatial_id,
            ..Self::default()
        };
        h.uncompressed_header(&mut r, sequence, refs)?;
        Ok((h, r.bytes_used()))
    }

    /// `FrameIsIntra`.
    pub fn is_intra(&self) -> bool {
        self.frame_type.is_intra()
    }

    fn uncompressed_header(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        refs: &mut [ReferenceState; NUM_REF_FRAMES],
    ) -> Result<(), Error> {
        let id_len = seq.frame_id_length();
        let all_frames = 0xff;
        if seq.reduced_still_picture_header {
            self.frame_type = FrameType::Key;
            self.show_frame = true;
            // A key frame shown at once, as below.
            self.error_resilient_mode = true;
        } else {
            self.show_existing_frame = r.bit()?;
            if self.show_existing_frame {
                self.frame_to_show_map_idx = r.u8(3)?;
                if seq.has_frame_presentation_time() {
                    self.temporal_point_info(r, seq)?;
                }
                if seq.frame_id_numbers_present_flag {
                    // display_frame_id
                    r.bits(id_len)?;
                }
                let shown = *reference(refs, self.frame_to_show_map_idx)?;
                if !shown.valid {
                    return Err(Error::MissingReference);
                }
                // 6.8.2 has a frame shown again coded showable_frame 1, but
                // Apple's host shows one coded 0, as libaom writes its
                // filtered forward key frames, and so does this decoder.
                // A key frame is shown so once.
                if shown.frame_type == FrameType::Key && shown.key_shown {
                    return Err(Error::KeyFrameShownAgain);
                }
                self.frame_type = shown.frame_type;
                self.show_frame = true;
                if self.frame_type == FrameType::Key {
                    self.refresh_frame_flags = all_frames;
                }
                if seq.film_grain_params_present {
                    self.film_grain = shown.film_grain;
                }
                // What the frame loading process (7.21) brings back.
                self.upscaled_width = shown.upscaled_width;
                self.frame_width = shown.frame_width;
                self.frame_height = shown.frame_height;
                self.render_width = shown.render_width;
                self.render_height = shown.render_height;
                self.order_hint = shown.order_hint;
                self.current_frame_id = shown.frame_id;
                self.global_motion = shown.global_motion;
                self.loop_filter = shown.loop_filter;
                self.segmentation = shown.segmentation;
                return Ok(());
            }
            self.frame_type = FrameType::from_raw(r.u8(2)?);
            self.show_frame = r.bit()?;
            if self.show_frame && seq.has_frame_presentation_time() {
                self.temporal_point_info(r, seq)?;
            }
            self.showable_frame = if self.show_frame {
                self.frame_type != FrameType::Key
            } else {
                r.bit()?
            };
            self.error_resilient_mode = self.frame_type == FrameType::Switch
                || (self.frame_type == FrameType::Key && self.show_frame)
                || r.bit()?;
        }
        if self.frame_type == FrameType::Key && self.show_frame {
            for slot in refs.iter_mut() {
                slot.valid = false;
                slot.order_hint = 0;
            }
        }
        self.disable_cdf_update = r.bit()?;
        self.allow_screen_content_tools = if seq.seq_force_screen_content_tools == SELECT {
            r.bit()?
        } else {
            seq.seq_force_screen_content_tools == 1
        };
        if self.allow_screen_content_tools {
            self.force_integer_mv = if seq.seq_force_integer_mv == SELECT {
                r.bit()?
            } else {
                seq.seq_force_integer_mv == 1
            };
        }
        if self.is_intra() {
            self.force_integer_mv = true;
        }
        if seq.frame_id_numbers_present_flag {
            self.current_frame_id = r.bits(id_len)?;
            mark_ref_frames(seq, self.current_frame_id, refs)?;
        }
        self.frame_size_override_flag = if self.frame_type == FrameType::Switch {
            true
        } else if seq.reduced_still_picture_header {
            false
        } else {
            r.bit()?
        };
        self.order_hint = r.bits(u32::from(seq.order_hint_bits))?;
        self.primary_ref_frame = if self.is_intra() || self.error_resilient_mode {
            PRIMARY_REF_NONE
        } else {
            r.u8(3)?
        };
        if let Some(model) = seq.decoder_model_info
            && r.bit()?
        {
            // buffer_removal_time_present_flag: the removal times of the
            // operating points this frame is in.
            for point in &seq.operating_points {
                if point.decoder_model_present
                    && in_layers(point.idc, self.temporal_id, self.spatial_id)
                {
                    r.skip(usize::from(model.buffer_removal_time_length_minus_1) + 1)?;
                }
            }
        }
        self.refresh_frame_flags = if self.frame_type == FrameType::Switch
            || (self.frame_type == FrameType::Key && self.show_frame)
        {
            all_frames
        } else {
            r.u8(8)?
        };
        if self.frame_type == FrameType::IntraOnly && self.refresh_frame_flags == all_frames {
            return Err(Error::OutOfRange(
                "refresh_frame_flags of an intra-only frame",
            ));
        }
        if (!self.is_intra() || self.refresh_frame_flags != all_frames)
            && self.error_resilient_mode
            && seq.enable_order_hint
        {
            for slot in refs.iter_mut() {
                let ref_order_hint = r.bits(u32::from(seq.order_hint_bits))?;
                if ref_order_hint != slot.order_hint {
                    // The slot holds another frame than the encoder's:
                    // nothing may predict from it.
                    slot.valid = false;
                    slot.order_hint = ref_order_hint;
                }
            }
        }
        if self.is_intra() {
            self.frame_size(r, seq)?;
            self.render_size(r)?;
            if self.allow_screen_content_tools && self.upscaled_width == self.frame_width {
                self.allow_intrabc = r.bit()?;
            }
        } else {
            self.inter_frame_references(r, seq, refs)?;
        }
        self.disable_frame_end_update_cdf =
            seq.reduced_still_picture_header || self.disable_cdf_update || r.bit()?;
        // setup_past_independence() or load_previous().
        let mut previous_gm = [GlobalMotion::default(); NUM_REF_FRAMES];
        if self.primary_ref_frame == PRIMARY_REF_NONE {
            self.loop_filter = LoopFilter::default();
            self.segmentation = Segmentation::default();
        } else {
            let index = self
                .ref_frame_idx
                .get(usize::from(self.primary_ref_frame))
                .ok_or(Error::OutOfRange("primary_ref_frame"))?;
            let slot = reference(refs, *index)?;
            if !slot.valid {
                return Err(Error::MissingReference);
            }
            previous_gm = slot.global_motion;
            self.loop_filter.ref_deltas = slot.loop_filter.ref_deltas;
            self.loop_filter.mode_deltas = slot.loop_filter.mode_deltas;
            self.segmentation.feature_mask = slot.segmentation.feature_mask;
            self.segmentation.feature_data = slot.segmentation.feature_data;
        }
        self.tile_info(r, seq)?;
        self.quantization_params(r, seq)?;
        self.segmentation_params(r)?;
        self.delta_params(r)?;
        let q = &self.quantization;
        let segmentation = &self.segmentation;
        self.coded_lossless = q.delta_q_y_dc == 0
            && q.delta_q_u_ac == 0
            && q.delta_q_u_dc == 0
            && q.delta_q_v_ac == 0
            && q.delta_q_v_dc == 0
            && segmentation
                .feature_mask
                .iter()
                .zip(&segmentation.feature_data)
                .all(|(&mask, data)| self.qindex(mask, data) == 0);
        self.all_lossless = self.coded_lossless && self.frame_width == self.upscaled_width;
        self.loop_filter_params(r, seq)?;
        self.cdef_params(r, seq)?;
        self.lr_params(r, seq)?;
        self.tx_mode = if self.coded_lossless {
            0
        } else if r.bit()? {
            2
        } else {
            1
        };
        self.reference_select = !self.is_intra() && r.bit()?;
        let skip_mode_frame = self.skip_mode_frames(seq, refs);
        self.skip_mode_present = skip_mode_frame.is_some() && r.bit()?;
        self.skip_mode_frame = match skip_mode_frame {
            Some(frames) if self.skip_mode_present => frames,
            _ => [0; 2],
        };
        self.allow_warped_motion = !self.is_intra()
            && !self.error_resilient_mode
            && seq.enable_warped_motion
            && r.bit()?;
        self.reduced_tx_set = r.bit()?;
        self.global_motion_params(r, &previous_gm)?;
        self.film_grain_params(r, seq, refs)?;
        Ok(())
    }

    /// `temporal_point_info()`: `frame_presentation_time`, unused here.
    fn temporal_point_info(
        &self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
    ) -> Result<(), Error> {
        let length = seq.decoder_model_info.map_or(0, |m| {
            usize::from(m.frame_presentation_time_length_minus_1) + 1
        });
        r.skip(length)
    }

    /// The references of an inter or switch frame, its size, and the
    /// motion vector tools (5.9.2, the `else` of `FrameIsIntra`).
    fn inter_frame_references(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        refs: &[ReferenceState; NUM_REF_FRAMES],
    ) -> Result<(), Error> {
        self.frame_refs_short_signaling = seq.enable_order_hint && r.bit()?;
        if self.frame_refs_short_signaling {
            let last = r.u8(3)?;
            let gold = r.u8(3)?;
            self.ref_frame_idx = set_frame_refs(seq, refs, self.order_hint, last, gold)?;
        }
        let short_signaling = self.frame_refs_short_signaling;
        for index in &mut self.ref_frame_idx {
            if !short_signaling {
                *index = r.u8(3)?;
            }
            if seq.frame_id_numbers_present_flag {
                // delta_frame_id_minus_1, of diffLen bits.
                r.skip(seq.diff_len.to_usize())?;
            }
        }
        for &index in &self.ref_frame_idx {
            if !reference(refs, index)?.valid {
                return Err(Error::MissingReference);
            }
        }
        if self.frame_size_override_flag && !self.error_resilient_mode {
            self.frame_size_with_refs(r, seq, refs)?;
        } else {
            self.frame_size(r, seq)?;
            self.render_size(r)?;
        }
        self.allow_high_precision_mv = !self.force_integer_mv && r.bit()?;
        self.interpolation_filter = if r.bit()? { 4 } else { r.u8(2)? };
        self.is_motion_mode_switchable = r.bit()?;
        self.use_ref_frame_mvs =
            !self.error_resilient_mode && seq.enable_ref_frame_mvs && r.bit()?;
        // 7.11.3.3: every reference within 2:1 smaller and 16:1 larger than
        // this frame.
        let times = |size: u32, by: u32| {
            size.checked_mul(by)
                .ok_or(Error::Overflow("a scaled FrameWidth or FrameHeight"))
        };
        for &index in &self.ref_frame_idx {
            let slot = reference(refs, index)?;
            let (width, height) = (self.frame_width, self.frame_height);
            if times(width, 2)? < slot.upscaled_width
                || times(height, 2)? < slot.frame_height
                || width > times(slot.upscaled_width, 16)?
                || height > times(slot.frame_height, 16)?
            {
                return Err(Error::ReferenceScale);
            }
        }
        Ok(())
    }

    /// `frame_size()` (5.9.5), with `superres_params()` and
    /// `compute_image_size()`.
    fn frame_size(&mut self, r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<(), Error> {
        if self.frame_size_override_flag {
            // frame_width_minus_1 + 1, of frame_width_bits_minus_1 + 1
            // bits; the height likewise.
            let bits = |minus_1: u8, name| {
                u32::from(minus_1)
                    .checked_add(1)
                    .ok_or(Error::Overflow(name))
            };
            let n = bits(seq.frame_width_bits_minus_1, "frame_width_bits_minus_1 + 1")?;
            self.frame_width = r.plus_one(n, "FrameWidth")?;
            let n = bits(
                seq.frame_height_bits_minus_1,
                "frame_height_bits_minus_1 + 1",
            )?;
            self.frame_height = r.plus_one(n, "FrameHeight")?;
        } else {
            (self.frame_width, self.frame_height) = seq.max_frame_size();
        }
        self.superres_params(r, seq)
    }

    /// `superres_params()` (5.9.8) and `compute_image_size()` (5.9.9).
    fn superres_params(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
    ) -> Result<(), Error> {
        self.use_superres = seq.enable_superres && r.bit()?;
        // SUPERRES_DENOM_MIN + coded_denom, or SUPERRES_NUM.
        self.superres_denom = if self.use_superres {
            (r.u8(3)?.checked_add(9)).ok_or(Error::Overflow("SuperresDenom"))?
        } else {
            8
        };
        self.upscaled_width = self.frame_width;
        let denom = u32::from(self.superres_denom);
        // (UpscaledWidth * SUPERRES_NUM + SuperresDenom / 2) / SuperresDenom.
        self.frame_width = (self.upscaled_width.checked_mul(8))
            .and_then(|scaled| scaled.checked_add(denom / 2))
            .and_then(|scaled| scaled.checked_div(denom))
            .ok_or(Error::Overflow("FrameWidth"))?;
        // compute_image_size(): MiCols = 2 * ((FrameWidth + 7) >> 3).
        self.mi_cols =
            (self.frame_width.div_ceil(8).checked_mul(2)).ok_or(Error::Overflow("MiCols"))?;
        self.mi_rows =
            (self.frame_height.div_ceil(8).checked_mul(2)).ok_or(Error::Overflow("MiRows"))?;
        Ok(())
    }

    /// `render_size()` (5.9.6).
    fn render_size(&mut self, r: &mut BitReader<'_>) -> Result<(), Error> {
        if r.bit()? {
            self.render_width = r.plus_one(16, "RenderWidth")?;
            self.render_height = r.plus_one(16, "RenderHeight")?;
        } else {
            self.render_width = self.upscaled_width;
            self.render_height = self.frame_height;
        }
        Ok(())
    }

    /// `frame_size_with_refs()` (5.9.7).
    fn frame_size_with_refs(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        refs: &[ReferenceState; NUM_REF_FRAMES],
    ) -> Result<(), Error> {
        for index in self.ref_frame_idx {
            // found_ref
            if r.bit()? {
                let slot = reference(refs, index)?;
                self.frame_width = slot.upscaled_width;
                self.frame_height = slot.frame_height;
                self.render_width = slot.render_width;
                self.render_height = slot.render_height;
                return self.superres_params(r, seq);
            }
        }
        self.frame_size(r, seq)?;
        self.render_size(r)
    }

    /// `tile_info()` (5.9.15).
    fn tile_info(&mut self, r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<(), Error> {
        // sbCols and sbRows, in superblocks of 2^sbShift mode info units,
        // and maxTileWidthSb and maxTileAreaSb, in superblocks of 2^sbSize
        // samples: MAX_TILE_WIDTH (4096) and MAX_TILE_AREA (4096 * 2304)
        // over the superblock's side and area.
        let (superblock_mi, max_tile_width_sb, max_tile_area_sb) = if seq.use_128x128_superblock {
            (32, 4096 >> 7, (4096 * 2304) >> 14)
        } else {
            (16, 4096 >> 6, (4096 * 2304) >> 12)
        };
        let sb_cols = self.mi_cols.div_ceil(superblock_mi);
        let sb_rows = self.mi_rows.div_ceil(superblock_mi);
        let sb_area = (sb_rows.checked_mul(sb_cols)).ok_or(Error::Overflow("sbRows * sbCols"))?;
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols)?;
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS))?;
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS))?;
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_area)?);
        let t = &mut self.tile_info;
        t.uniform = r.bit()?;
        // Each column's and row's first superblock, then the frame's end.
        let mut col_starts = Vec::new();
        let mut row_starts = Vec::new();
        // ((sb + (1 << log2) - 1) >> log2): sb over 2^log2, rounded up.
        let tile_size = |sb: u32, log2: u32, name| {
            1u32.checked_shl(log2)
                .map(|tiles| sb.div_ceil(tiles))
                .ok_or(Error::Overflow(name))
        };
        // increment_tile_cols_log2 and increment_tile_rows_log2.
        let increment = |log2: u32, name| log2.checked_add(1).ok_or(Error::Overflow(name));
        if t.uniform {
            t.cols_log2 = min_log2_tile_cols;
            while t.cols_log2 < max_log2_tile_cols && r.bit()? {
                t.cols_log2 = increment(t.cols_log2, "TileColsLog2")?;
            }
            let tile_width_sb = tile_size(sb_cols, t.cols_log2, "tileWidthSb")?;
            col_starts.extend((0..sb_cols).step_by(tile_width_sb.max(1).to_usize()));
            // Max(minLog2Tiles - TileColsLog2, 0): the standard clips it.
            let min_log2_tile_rows = min_log2_tiles.saturating_sub(t.cols_log2);
            t.rows_log2 = min_log2_tile_rows;
            while t.rows_log2 < max_log2_tile_rows && r.bit()? {
                t.rows_log2 = increment(t.rows_log2, "TileRowsLog2")?;
            }
            let tile_height_sb = tile_size(sb_rows, t.rows_log2, "tileHeightSb")?;
            row_starts.extend((0..sb_rows).step_by(tile_height_sb.max(1).to_usize()));
        } else {
            let mut widest_tile_sb = 0;
            let mut start = 0u32;
            // While startSb < sbCols: width_in_sbs_minus_1 + 1 superblocks
            // at most maxTileWidthSb, and at most what is left.
            while let Some(left) = sb_cols.checked_sub(start).filter(|&left| left > 0) {
                col_starts.push(start);
                let size = (r.ns(left.min(max_tile_width_sb))?.checked_add(1))
                    .ok_or(Error::Overflow("sizeSb"))?;
                widest_tile_sb = widest_tile_sb.max(size);
                start = start.checked_add(size).ok_or(Error::Overflow("startSb"))?;
            }
            let max_tile_area_sb = if min_log2_tiles > 0 {
                (min_log2_tiles.checked_add(1))
                    .and_then(|shift| sb_area.checked_shr(shift))
                    .ok_or(Error::Overflow("maxTileAreaSb"))?
            } else {
                sb_area
            };
            // Max(maxTileAreaSb / widestTileSb, 1).
            let max_tile_height_sb = (max_tile_area_sb.checked_div(widest_tile_sb.max(1)))
                .ok_or(Error::Overflow("maxTileHeightSb"))?
                .max(1);
            let mut start = 0u32;
            while let Some(left) = sb_rows.checked_sub(start).filter(|&left| left > 0) {
                row_starts.push(start);
                let size = (r.ns(left.min(max_tile_height_sb))?.checked_add(1))
                    .ok_or(Error::Overflow("sizeSb"))?;
                start = start.checked_add(size).ok_or(Error::Overflow("startSb"))?;
            }
        }
        // TileCols and TileRows: at most MAX_TILE_COLS and MAX_TILE_ROWS.
        let (Ok(cols), Ok(rows)) = (
            u32::try_from(col_starts.len()),
            u32::try_from(row_starts.len()),
        ) else {
            return Err(Error::OutOfRange("tile_info()"));
        };
        if cols > MAX_TILE_COLS || rows > MAX_TILE_ROWS {
            return Err(Error::OutOfRange("tile_info()"));
        }
        let num_tiles = cols.checked_mul(rows).ok_or(Error::Overflow("NumTiles"))?;
        if num_tiles.to_usize() > MAX_TILES {
            return Err(Error::Unsupported("more than 256 tiles"));
        }
        if !t.uniform {
            t.cols_log2 = tile_log2(1, cols)?;
            t.rows_log2 = tile_log2(1, rows)?;
        }
        t.cols = cols;
        t.rows = rows;
        t.num_tiles = num_tiles;
        t.width_in_sbs = sizes(&col_starts, sb_cols)?;
        t.height_in_sbs = sizes(&row_starts, sb_rows)?;
        if t.cols_log2 > 0 || t.rows_log2 > 0 {
            let bits = (t.rows_log2.checked_add(t.cols_log2))
                .ok_or(Error::Overflow("TileRowsLog2 + TileColsLog2"))?;
            t.context_update_tile_id = r.bits(bits)?;
            if t.context_update_tile_id >= t.num_tiles {
                return Err(Error::OutOfRange("context_update_tile_id"));
            }
            t.tile_size_bytes =
                (r.u8(2)?.checked_add(1)).ok_or(Error::Overflow("TileSizeBytes"))?;
        } else {
            t.context_update_tile_id = 0;
            t.tile_size_bytes = 4;
        }
        Ok(())
    }

    /// `quantization_params()` (5.9.12).
    fn quantization_params(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
    ) -> Result<(), Error> {
        let q = &mut self.quantization;
        q.base_q_idx = r.u8(8)?;
        q.delta_q_y_dc = read_delta_q(r)?;
        if seq.color.num_planes() > 1 {
            let diff_uv_delta = seq.color.separate_uv_delta_q && r.bit()?;
            q.delta_q_u_dc = read_delta_q(r)?;
            q.delta_q_u_ac = read_delta_q(r)?;
            if diff_uv_delta {
                q.delta_q_v_dc = read_delta_q(r)?;
                q.delta_q_v_ac = read_delta_q(r)?;
            } else {
                q.delta_q_v_dc = q.delta_q_u_dc;
                q.delta_q_v_ac = q.delta_q_u_ac;
            }
        }
        q.using_qmatrix = r.bit()?;
        if q.using_qmatrix {
            q.qm_y = r.u8(4)?;
            q.qm_u = r.u8(4)?;
            q.qm_v = if seq.color.separate_uv_delta_q {
                r.u8(4)?
            } else {
                q.qm_u
            };
        }
        Ok(())
    }

    /// `segmentation_params()` (5.9.14). Without `segmentation_update_data`
    /// the features stay as `load_previous()` left them.
    fn segmentation_params(&mut self, r: &mut BitReader<'_>) -> Result<(), Error> {
        // Segmentation_Feature_Bits, Segmentation_Feature_Signed and
        // Segmentation_Feature_Max, by feature.
        const FEATURES: [(u32, bool, i16); SEG_LVL_MAX] = [
            (8, true, 255),
            (6, true, 63),
            (6, true, 63),
            (6, true, 63),
            (6, true, 63),
            (3, false, 7),
            (0, false, 0),
            (0, false, 0),
        ];
        let primary_none = self.primary_ref_frame == PRIMARY_REF_NONE;
        let s = &mut self.segmentation;
        s.enabled = r.bit()?;
        if !s.enabled {
            *s = Segmentation::default();
            return Ok(());
        }
        if primary_none {
            s.update_map = true;
            s.temporal_update = false;
            s.update_data = true;
        } else {
            s.update_map = r.bit()?;
            s.temporal_update = s.update_map && r.bit()?;
            s.update_data = r.bit()?;
        }
        if s.update_data {
            for (mask, data) in s.feature_mask.iter_mut().zip(&mut s.feature_data) {
                *mask = 0;
                for (feature, (value, &(bits, signed, limit))) in
                    (0u8..).zip(data.iter_mut().zip(&FEATURES))
                {
                    *value = 0;
                    if r.bit()? {
                        *mask |= 1 << feature;
                        *value = if signed {
                            // su(1 + bitsToRead), clipped to the limit.
                            let n = bits
                                .checked_add(1)
                                .ok_or(Error::Overflow("1 + bitsToRead"))?;
                            let coded = r.su(n)?;
                            let limit = i32::from(limit);
                            let low = limit.checked_neg().ok_or(Error::Overflow("-limit"))?;
                            i16::try_from(coded.clamp(low, limit)).unwrap_or(0)
                        } else {
                            let coded = r.bits(bits)?;
                            i16::try_from(coded.min(limit.unsigned_abs().into())).unwrap_or(0)
                        };
                    }
                }
            }
        }
        Ok(())
    }

    /// `delta_q_params()` (5.9.17) and `delta_lf_params()` (5.9.18).
    fn delta_params(&mut self, r: &mut BitReader<'_>) -> Result<(), Error> {
        self.delta_q_present = self.quantization.base_q_idx > 0 && r.bit()?;
        if self.delta_q_present {
            self.delta_q_res = r.u8(2)?;
            self.delta_lf_present = !self.allow_intrabc && r.bit()?;
            if self.delta_lf_present {
                self.delta_lf_res = r.u8(2)?;
                self.delta_lf_multi = r.bit()?;
            }
        }
        Ok(())
    }

    /// `get_qindex(1, segmentId)` (7.12.2), for the segment whose
    /// `FeatureEnabled` and `FeatureData` are `feature_mask` and
    /// `feature_data`.
    fn qindex(&self, feature_mask: u8, feature_data: &[i16; SEG_LVL_MAX]) -> i32 {
        let base = i32::from(self.quantization.base_q_idx);
        // SEG_LVL_ALT_Q is feature 0.
        if self.segmentation.enabled && feature_mask & 1 != 0 {
            // Clip3(0, 255, base_q_idx + FeatureData[segmentId][0]).
            base.saturating_add(i32::from(feature_data[0]))
                .clamp(0, 255)
        } else {
            base
        }
    }

    /// `loop_filter_params()` (5.9.11).
    fn loop_filter_params(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
    ) -> Result<(), Error> {
        let lf = &mut self.loop_filter;
        if self.coded_lossless || self.allow_intrabc {
            *lf = LoopFilter::default();
            return Ok(());
        }
        lf.level[0] = r.u8(6)?;
        lf.level[1] = r.u8(6)?;
        if seq.color.num_planes() > 1 && (lf.level[0] != 0 || lf.level[1] != 0) {
            lf.level[2] = r.u8(6)?;
            lf.level[3] = r.u8(6)?;
        }
        lf.sharpness = r.u8(3)?;
        lf.delta_enabled = r.bit()?;
        if lf.delta_enabled {
            lf.delta_update = r.bit()?;
            if lf.delta_update {
                for delta in &mut lf.ref_deltas {
                    if r.bit()? {
                        *delta = i8::try_from(r.su(7)?).unwrap_or(0);
                    }
                }
                for delta in &mut lf.mode_deltas {
                    if r.bit()? {
                        *delta = i8::try_from(r.su(7)?).unwrap_or(0);
                    }
                }
            }
        }
        Ok(())
    }

    /// `cdef_params()` (5.9.19).
    fn cdef_params(&mut self, r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<(), Error> {
        let c = &mut self.cdef;
        if self.coded_lossless || self.allow_intrabc || !seq.enable_cdef {
            *c = Cdef::default();
            return Ok(());
        }
        c.damping_minus_3 = r.u8(2)?;
        c.bits = r.u8(2)?;
        let strengths = c
            .y_pri_strength
            .iter_mut()
            .zip(&mut c.y_sec_strength)
            .zip(c.uv_pri_strength.iter_mut().zip(&mut c.uv_sec_strength));
        // 1 << cdef_bits: up to the eight of each array.
        for ((y_pri, y_sec), (uv_pri, uv_sec)) in strengths.take(1 << c.bits) {
            *y_pri = r.u8(4)?;
            *y_sec = r.u8(2)?;
            if seq.color.num_planes() > 1 {
                *uv_pri = r.u8(4)?;
                *uv_sec = r.u8(2)?;
            }
        }
        Ok(())
    }

    /// `lr_params()` (5.9.20).
    fn lr_params(&mut self, r: &mut BitReader<'_>, seq: &SequenceHeader) -> Result<(), Error> {
        let lr = &mut self.loop_restoration;
        *lr = LoopRestoration::default();
        if self.all_lossless || self.allow_intrabc || !seq.enable_restoration {
            return Ok(());
        }
        let mut uses_lr = false;
        let mut uses_chroma_lr = false;
        let planes = usize::from(seq.color.num_planes());
        for (plane, restoration) in lr
            .frame_restoration_type
            .iter_mut()
            .enumerate()
            .take(planes)
        {
            // lr_type through Remap_Lr_Type.
            *restoration = match r.u8(2)? {
                0 => 0,
                1 => 3,
                2 => 1,
                _ => 2,
            };
            if *restoration != 0 {
                uses_lr = true;
                uses_chroma_lr |= plane > 0;
            }
        }
        if uses_lr {
            let overflow = || Error::Overflow("lr_unit_shift");
            if seq.use_128x128_superblock {
                lr.lr_unit_shift = r.u8(1)?.checked_add(1).ok_or_else(overflow)?;
            } else {
                lr.lr_unit_shift = r.u8(1)?;
                if lr.lr_unit_shift != 0 {
                    // lr_unit_extra_shift.
                    lr.lr_unit_shift =
                        (lr.lr_unit_shift.checked_add(r.u8(1)?)).ok_or_else(overflow)?;
                }
            }
            if seq.color.subsampling_x && seq.color.subsampling_y && uses_chroma_lr {
                lr.lr_uv_shift = r.u8(1)?;
            }
        }
        Ok(())
    }

    /// `skipModeAllowed` and `SkipModeFrame[]` (5.9.22): the two
    /// references a skip mode block would predict from, when skip mode is
    /// allowed.
    fn skip_mode_frames(
        &self,
        seq: &SequenceHeader,
        refs: &[ReferenceState; NUM_REF_FRAMES],
    ) -> Option<[u8; 2]> {
        if self.is_intra() || !self.reference_select || !seq.enable_order_hint {
            return None;
        }
        let dist = |a: u32, b: u32| relative_dist(seq, a, b);
        let mut hints = [0u32; REFS_PER_FRAME];
        for (hint, &index) in hints.iter_mut().zip(&self.ref_frame_idx) {
            *hint = refs.get(usize::from(index))?.order_hint;
        }
        // Each as (forwardIdx, forwardHint) and the like.
        let mut forward: Option<(usize, u32)> = None;
        let mut backward: Option<(usize, u32)> = None;
        for (i, &ref_hint) in hints.iter().enumerate() {
            if dist(ref_hint, self.order_hint) < 0 {
                if forward.is_none_or(|(_, f)| dist(ref_hint, f) > 0) {
                    forward = Some((i, ref_hint));
                }
            } else if dist(ref_hint, self.order_hint) > 0
                && backward.is_none_or(|(_, b)| dist(ref_hint, b) < 0)
            {
                backward = Some((i, ref_hint));
            }
        }
        let (forward_idx, forward_hint) = forward?;
        let second = match backward {
            Some((backward_idx, _)) => backward_idx,
            None => {
                let mut second_forward: Option<(usize, u32)> = None;
                for (i, &ref_hint) in hints.iter().enumerate() {
                    if dist(ref_hint, forward_hint) < 0
                        && second_forward.is_none_or(|(_, s)| dist(ref_hint, s) > 0)
                    {
                        second_forward = Some((i, ref_hint));
                    }
                }
                second_forward?.0
            }
        };
        // LAST_FRAME + i: LAST_FRAME is 1, and REFS_PER_FRAME is 7.
        let frame = |i: usize| u8::try_from(i).ok()?.checked_add(1);
        Some([
            frame(forward_idx.min(second))?,
            frame(forward_idx.max(second))?,
        ])
    }

    /// `global_motion_params()` (5.9.24), the deltas coded against
    /// `previous`, `PrevGmParams`.
    fn global_motion_params(
        &mut self,
        r: &mut BitReader<'_>,
        previous: &[GlobalMotion; NUM_REF_FRAMES],
    ) -> Result<(), Error> {
        self.global_motion = [GlobalMotion::default(); NUM_REF_FRAMES];
        if self.is_intra() {
            return Ok(());
        }
        let high_precision = self.allow_high_precision_mv;
        // LAST_FRAME to ALTREF_FRAME.
        for (model, previous) in self.global_motion.iter_mut().zip(previous).skip(1) {
            let kind = if !r.bit()? {
                IDENTITY
            } else if r.bit()? {
                ROTZOOM
            } else if r.bit()? {
                TRANSLATION
            } else {
                AFFINE
            };
            model.kind = kind;
            // The parameters in the order they are coded.
            let order: &[usize] = match kind {
                IDENTITY => &[],
                TRANSLATION => &[0, 1],
                ROTZOOM => &[2, 3, 0, 1],
                _ => &[2, 3, 4, 5, 0, 1],
            };
            for &idx in order {
                let (Some(param), Some(&previous)) =
                    (model.params.get_mut(idx), previous.params.get(idx))
                else {
                    continue;
                };
                *param = read_global_param(r, kind, idx, previous, high_precision)?;
            }
            if kind == ROTZOOM {
                model.params[4] =
                    (model.params[3].checked_neg()).ok_or(Error::Overflow("gm_params[ref][4]"))?;
                model.params[5] = model.params[2];
            }
            model.shear_valid = shear_valid(&model.params)?;
        }
        Ok(())
    }

    /// `film_grain_params()` (5.9.30), with `load_grain_params()` from
    /// `refs`.
    fn film_grain_params(
        &mut self,
        r: &mut BitReader<'_>,
        seq: &SequenceHeader,
        refs: &[ReferenceState; NUM_REF_FRAMES],
    ) -> Result<(), Error> {
        self.film_grain = FilmGrain::default();
        if !seq.film_grain_params_present || (!self.show_frame && !self.showable_frame) {
            return Ok(());
        }
        if !r.bit()? {
            return Ok(());
        }
        let g = &mut self.film_grain;
        g.apply_grain = true;
        g.grain_seed = r.u16(16)?;
        g.update_grain = self.frame_type != FrameType::Inter || r.bit()?;
        if !g.update_grain {
            let index = r.u8(3)?;
            if !self.ref_frame_idx.contains(&index) {
                return Err(Error::OutOfRange("film_grain_params_ref_idx"));
            }
            let seed = g.grain_seed;
            *g = reference(refs, index)?.film_grain;
            g.grain_seed = seed;
            return Ok(());
        }
        g.num_y_points = r.u8(4)?;
        if g.num_y_points > 14 {
            return Err(Error::OutOfRange("num_y_points"));
        }
        read_points(
            r,
            &mut g.point_y_value,
            &mut g.point_y_scaling,
            g.num_y_points,
        )?;
        let c = &seq.color;
        g.chroma_scaling_from_luma = !c.mono_chrome && r.bit()?;
        if !(c.mono_chrome
            || g.chroma_scaling_from_luma
            || (c.subsampling_x && c.subsampling_y && g.num_y_points == 0))
        {
            g.num_cb_points = r.u8(4)?;
            if g.num_cb_points > 10 {
                return Err(Error::OutOfRange("num_cb_points"));
            }
            read_points(
                r,
                &mut g.point_cb_value,
                &mut g.point_cb_scaling,
                g.num_cb_points,
            )?;
            g.num_cr_points = r.u8(4)?;
            if g.num_cr_points > 10 {
                return Err(Error::OutOfRange("num_cr_points"));
            }
            read_points(
                r,
                &mut g.point_cr_value,
                &mut g.point_cr_scaling,
                g.num_cr_points,
            )?;
        }
        // 6.8.20: each component's point values increase, and 4:2:0 codes
        // points for both chroma components or neither. A driver divides
        // by the step from one point to the next.
        let increasing = |values: &[u8], count: u8| {
            values
                .get(..usize::from(count))
                .is_some_and(|values| values.is_sorted_by(|a, b| a < b))
        };
        if !increasing(&g.point_y_value, g.num_y_points)
            || !increasing(&g.point_cb_value, g.num_cb_points)
            || !increasing(&g.point_cr_value, g.num_cr_points)
        {
            return Err(Error::FilmGrainPointsUnordered);
        }
        if c.subsampling_x && c.subsampling_y && (g.num_cb_points == 0) != (g.num_cr_points == 0) {
            return Err(Error::OutOfRange("num_cr_points"));
        }
        g.grain_scaling_minus_8 = r.u8(2)?;
        g.ar_coeff_lag = r.u8(2)?;
        // numPosLuma, 2 * ar_coeff_lag * (ar_coeff_lag + 1): at an
        // ar_coeff_lag of 3, the 24 and 25 of the arrays.
        let lag = usize::from(g.ar_coeff_lag);
        let num_pos_luma = (lag.checked_add(1))
            .and_then(|next| next.checked_mul(lag))
            .and_then(|product| product.checked_mul(2))
            .ok_or(Error::Overflow("numPosLuma"))?;
        let num_pos_chroma = if g.num_y_points > 0 {
            for coeff in g.ar_coeffs_y_plus_128.iter_mut().take(num_pos_luma) {
                *coeff = r.u8(8)?;
            }
            (num_pos_luma.checked_add(1)).ok_or(Error::Overflow("numPosChroma"))?
        } else {
            num_pos_luma
        };
        if g.chroma_scaling_from_luma || g.num_cb_points > 0 {
            for coeff in g.ar_coeffs_cb_plus_128.iter_mut().take(num_pos_chroma) {
                *coeff = r.u8(8)?;
            }
        }
        if g.chroma_scaling_from_luma || g.num_cr_points > 0 {
            for coeff in g.ar_coeffs_cr_plus_128.iter_mut().take(num_pos_chroma) {
                *coeff = r.u8(8)?;
            }
        }
        g.ar_coeff_shift_minus_6 = r.u8(2)?;
        g.grain_scale_shift = r.u8(2)?;
        if g.num_cb_points > 0 {
            g.cb_mult = r.u8(8)?;
            g.cb_luma_mult = r.u8(8)?;
            g.cb_offset = r.u16(9)?;
        }
        if g.num_cr_points > 0 {
            g.cr_mult = r.u8(8)?;
            g.cr_luma_mult = r.u8(8)?;
            g.cr_offset = r.u16(9)?;
        }
        g.overlap_flag = r.bit()?;
        g.clip_to_restricted_range = r.bit()?;
        Ok(())
    }

    /// What a slot keeps of this frame once it is decoded (7.20).
    pub(crate) fn reference_state(&self) -> ReferenceState {
        ReferenceState {
            valid: true,
            frame_id: self.current_frame_id,
            upscaled_width: self.upscaled_width,
            frame_width: self.frame_width,
            frame_height: self.frame_height,
            render_width: self.render_width,
            render_height: self.render_height,
            frame_type: self.frame_type,
            order_hint: self.order_hint,
            showable: self.showable_frame,
            key_shown: false,
            global_motion: self.global_motion,
            loop_filter: self.loop_filter,
            segmentation: self.segmentation,
            film_grain: self.film_grain,
        }
    }
}

/// The reference slot `index` names, for a header's `ref_frame_idx`,
/// `frame_to_show_map_idx` and `film_grain_params_ref_idx`: three bits each,
/// so that each names one of the eight.
fn reference(refs: &[ReferenceState; NUM_REF_FRAMES], index: u8) -> Result<&ReferenceState, Error> {
    refs.get(usize::from(index))
        .ok_or(Error::NoSuchReferenceSlot)
}

/// `point_*_value` and `point_*_scaling` of one component's film grain,
/// `count` of each, which the caller has held to the arrays' length.
fn read_points(
    r: &mut BitReader<'_>,
    values: &mut [u8],
    scalings: &mut [u8],
    count: u8,
) -> Result<(), Error> {
    for (value, scaling) in values.iter_mut().zip(scalings).take(usize::from(count)) {
        *value = r.u8(8)?;
        *scaling = r.u8(8)?;
    }
    Ok(())
}

/// `read_delta_q()` (5.9.13).
fn read_delta_q(r: &mut BitReader<'_>) -> Result<i8, Error> {
    if r.bit()? {
        i8::try_from(r.su(7)?).map_err(|_| Error::OutOfRange("delta_q"))
    } else {
        Ok(0)
    }
}

/// `tile_log2()` (5.9.16): the least `k` for which `block_size << k`
/// reaches `target`. In 64 bits, where a 32-bit block size shifted by up
/// to 32 keeps its bits; a block size of 0 reaches no target.
fn tile_log2(block_size: u32, target: u32) -> Result<u32, Error> {
    (0..=u32::BITS)
        .find(|&k| u64::from(block_size) << k >= u64::from(target))
        .ok_or(Error::Overflow("tile_log2()"))
}

/// The sizes between consecutive `starts`, which increase, the last
/// running to `end`, which is past them.
fn sizes(starts: &[u32], end: u32) -> Result<Vec<u32>, Error> {
    let nexts = starts.iter().skip(1).chain([&end]);
    starts
        .iter()
        .zip(nexts)
        .map(|(&start, &next)| next.checked_sub(start).ok_or(Error::Overflow("sizeSb")))
        .collect()
}

/// `get_relative_dist()` (7.12.3).
pub(crate) fn relative_dist(seq: &SequenceHeader, a: u32, b: u32) -> i32 {
    if !seq.enable_order_hint {
        return 0;
    }
    let bits = u32::from(seq.order_hint_bits);
    // The difference mod 2^OrderHintBits, sign extended from its top bit:
    // diff = a - b, m = 1 << (OrderHintBits - 1), (diff & (m - 1)) -
    // (diff & m). It wraps by definition, and OrderHintBits, 1 to 8 with
    // order hints, keeps both masks within an i32.
    let diff = a.wrapping_sub(b);
    let m = (1u32 << bits) >> 1;
    (diff & m.wrapping_sub(1))
        .cast_signed()
        .wrapping_sub((diff & m).cast_signed())
}

/// `mark_ref_frames()` (5.9.4): slots whose frame id is too far from this
/// frame's are no longer references.
fn mark_ref_frames(
    seq: &SequenceHeader,
    current: u32,
    refs: &mut [ReferenceState; NUM_REF_FRAMES],
) -> Result<(), Error> {
    let overflow = || Error::Overflow("mark_ref_frames()");
    // 1 << diffLen and 1 << idLen, below 2^26: idLen's two fields are of 3
    // and 4 bits.
    let diff_span = 1u32.checked_shl(seq.diff_len).ok_or_else(overflow)?;
    let id_span = 1u32
        .checked_shl(seq.frame_id_length())
        .ok_or_else(overflow)?;
    // The ids more than diffLen's span below the current one, mod idLen's.
    let (low, high) = match current.checked_sub(diff_span).filter(|&low| low > 0) {
        Some(low) => (low, None),
        None => (
            0,
            Some(
                (id_span.checked_add(current))
                    .and_then(|sum| sum.checked_sub(diff_span))
                    .ok_or_else(overflow)?,
            ),
        ),
    };
    for slot in refs.iter_mut() {
        let id = slot.frame_id;
        let stale = match high {
            None => id > current || id < low,
            Some(high) => id > current && id < high,
        };
        if stale {
            slot.valid = false;
        }
    }
    Ok(())
}

/// `set_frame_refs()` (7.8): the references `frame_refs_short_signaling`
/// leaves to the decoder.
fn set_frame_refs(
    seq: &SequenceHeader,
    refs: &[ReferenceState; NUM_REF_FRAMES],
    order_hint: u32,
    last_frame_idx: u8,
    gold_frame_idx: u8,
) -> Result<[u8; REFS_PER_FRAME], Error> {
    const LAST: usize = 0;
    const GOLDEN: usize = 3;
    const BWDREF: usize = 4;
    const ALTREF2: usize = 5;
    const ALTREF: usize = 6;
    let mut idx: [Option<u8>; REFS_PER_FRAME] = [None; REFS_PER_FRAME];
    idx[LAST] = Some(last_frame_idx);
    idx[GOLDEN] = Some(gold_frame_idx);
    // usedFrame, a bit per slot.
    let mut used = (1u8 << last_frame_idx) | (1u8 << gold_frame_idx);
    // 1 << (OrderHintBits - 1), and each slot's hint around it: order
    // hints are of 1 to 8 bits where frame refs are signalled short.
    let cur_frame_hint = (1i32 << seq.order_hint_bits) >> 1;
    let mut shifted = [0i32; NUM_REF_FRAMES];
    for (hint, slot) in shifted.iter_mut().zip(refs) {
        *hint = (cur_frame_hint.checked_add(relative_dist(seq, slot.order_hint, order_hint)))
            .ok_or(Error::Overflow("shiftedOrderHints"))?;
    }
    // find_latest_backward, find_earliest_backward, find_latest_forward:
    // the unused slot at or after the frame latest or earliest, or the
    // latest before it.
    let find = |used: u8, backward: bool, latest: bool| -> Option<u8> {
        let mut best: Option<(u8, i32)> = None;
        for (i, &hint) in (0u8..).zip(&shifted) {
            let side = if backward {
                hint >= cur_frame_hint
            } else {
                hint < cur_frame_hint
            };
            if used & (1 << i) != 0 || !side {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, b)) if latest => hint >= b,
                Some((_, b)) => hint < b,
            };
            if better {
                best = Some((i, hint));
            }
        }
        best.map(|(i, _)| i)
    };
    if let Some(slot) = find(used, true, true) {
        idx[ALTREF] = Some(slot);
        used |= 1 << slot;
    }
    if let Some(slot) = find(used, true, false) {
        idx[BWDREF] = Some(slot);
        used |= 1 << slot;
    }
    if let Some(slot) = find(used, true, false) {
        idx[ALTREF2] = Some(slot);
        used |= 1 << slot;
    }
    // LAST2_FRAME, LAST3_FRAME, BWDREF_FRAME, ALTREF2_FRAME and
    // ALTREF_FRAME, in that order: those of the references left unset.
    for entry in idx.iter_mut().filter(|entry| entry.is_none()) {
        if let Some(slot) = find(used, false, true) {
            *entry = Some(slot);
            used |= 1 << slot;
        }
    }
    let mut earliest: Option<(u8, i32)> = None;
    for (i, &hint) in (0u8..).zip(&shifted) {
        if earliest.is_none_or(|(_, e)| hint < e) {
            earliest = Some((i, hint));
        }
    }
    let fallback = earliest.map_or(0, |(i, _)| i);
    Ok(idx.map(|slot| slot.unwrap_or(fallback)))
}

/// `read_global_param()` (5.9.25).
fn read_global_param(
    r: &mut BitReader<'_>,
    kind: u8,
    idx: usize,
    previous: i32,
    allow_high_precision_mv: bool,
) -> Result<i32, Error> {
    let overflow = || Error::Overflow("gm_params");
    // absBits and precBits: GM_ABS_TRANS_ONLY_BITS and
    // GM_TRANS_ONLY_PREC_BITS, less one without high precision motion
    // vectors; GM_ABS_TRANS_BITS and GM_TRANS_PREC_BITS; or
    // GM_ABS_ALPHA_BITS and GM_ALPHA_PREC_BITS.
    let (abs_bits, prec_bits) = if idx < 2 {
        if kind == TRANSLATION {
            if allow_high_precision_mv {
                (9, 3)
            } else {
                (8, 2)
            }
        } else {
            (12, 6)
        }
    } else {
        (12, 15)
    };
    let prec_diff = WARPEDMODEL_PREC_BITS
        .checked_sub(prec_bits)
        .ok_or_else(overflow)?;
    let (round, sub) = if idx % 3 == 2 {
        (1i32 << WARPEDMODEL_PREC_BITS, 1i32 << prec_bits)
    } else {
        (0, 0)
    };
    let mx = 1i32 << abs_bits;
    // r: PrevGmParams[ref][idx] >> precDiff, less sub.
    let reference = (previous >> prec_diff)
        .checked_sub(sub)
        .ok_or_else(overflow)?;
    let low = mx.checked_neg().ok_or_else(overflow)?;
    let high = mx.checked_add(1).ok_or_else(overflow)?;
    let value = decode_signed_subexp_with_ref(r, low, high, reference)?;
    // Within mx of 0, so the shift by precDiff keeps every bit.
    (value << prec_diff).checked_add(round).ok_or_else(overflow)
}

/// `decode_signed_subexp_with_ref()` (5.9.26).
fn decode_signed_subexp_with_ref(
    r: &mut BitReader<'_>,
    low: i32,
    high: i32,
    reference: i32,
) -> Result<i32, Error> {
    let overflow = || Error::Overflow("decode_signed_subexp_with_ref()");
    let x = decode_unsigned_subexp_with_ref(
        r,
        high.checked_sub(low).ok_or_else(overflow)?,
        reference.checked_sub(low).ok_or_else(overflow)?,
    )?;
    x.checked_add(low).ok_or_else(overflow)
}

/// `decode_unsigned_subexp_with_ref()` (5.9.27).
fn decode_unsigned_subexp_with_ref(
    r: &mut BitReader<'_>,
    mx: i32,
    reference: i32,
) -> Result<i32, Error> {
    let overflow = || Error::Overflow("decode_unsigned_subexp_with_ref()");
    let v = decode_subexp(r, mx)?;
    let below_mx = |value: i32| (mx.checked_sub(1)).and_then(|top| top.checked_sub(value));
    if reference.checked_mul(2).ok_or_else(overflow)? <= mx {
        inverse_recenter(reference, v)
    } else {
        // mx - 1 - inverse_recenter(mx - 1 - r, v)
        let mirrored = below_mx(reference).ok_or_else(overflow)?;
        below_mx(inverse_recenter(mirrored, v)?).ok_or_else(overflow)
    }
}

/// `decode_subexp()` (5.9.28).
fn decode_subexp(r: &mut BitReader<'_>, num_syms: i32) -> Result<i32, Error> {
    let overflow = || Error::Overflow("decode_subexp()");
    let mut mk = 0i32;
    // b2: k (3) for the first, k + i - 1 for each after; 24 more at most
    // before b2 passes the bits a value of i32 holds.
    for b2 in [3u32].into_iter().chain(3..27) {
        let a = 1i32 << b2;
        let last = (a.checked_mul(3))
            .and_then(|three_a| mk.checked_add(three_a))
            .ok_or_else(overflow)?;
        if num_syms <= last {
            let n = (num_syms.checked_sub(mk))
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(Error::OutOfRange("subexp"))?;
            let bits = i32::try_from(r.ns(n)?).map_err(|_| Error::OutOfRange("subexp"))?;
            return bits.checked_add(mk).ok_or_else(overflow);
        }
        if r.bit()? {
            mk = mk.checked_add(a).ok_or_else(overflow)?;
        } else {
            let bits = i32::try_from(r.bits(b2)?).map_err(|_| Error::OutOfRange("subexp"))?;
            return bits.checked_add(mk).ok_or_else(overflow);
        }
    }
    Err(Error::OutOfRange("subexp"))
}

/// `inverse_recenter()` (5.9.29).
fn inverse_recenter(r: i32, v: i32) -> Result<i32, Error> {
    let overflow = || Error::Overflow("inverse_recenter()");
    if v > r.checked_mul(2).ok_or_else(overflow)? {
        Ok(v)
    } else if v & 1 == 1 {
        // r - ((v + 1) >> 1), v below 2r here.
        (v >> 1)
            .checked_add(1)
            .and_then(|half| r.checked_sub(half))
            .ok_or_else(overflow)
    } else {
        r.checked_add(v >> 1).ok_or_else(overflow)
    }
}
