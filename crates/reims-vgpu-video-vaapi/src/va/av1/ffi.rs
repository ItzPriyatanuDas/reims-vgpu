//! libva's AV1 decoding interface (`va_dec_av1.h`, libva 2.24.1), written
//! by hand as `super::super::ffi` is. The test at the end checks each
//! struct's size and offsets, and each bit-field's position, against the
//! installed header with a C compiler.

#![expect(
    non_upper_case_globals,
    reason = "libva's names, kept for grepping the headers"
)]

use super::super::ffi::{BitField, VA_PADDING_LOW, VA_PADDING_MEDIUM, VAProfile, VASurfaceID};

/// `VAProfileAV1Profile0`: Main, 8 and 10 bits, 4:2:0.
pub(crate) const VAProfileAV1Profile0: VAProfile = 32;

const fn bits(shift: u32, width: u32) -> BitField {
    BitField { shift, width }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VASegmentationStructAV1 {
    /// `segment_info_fields.value`: see [`segment_info_fields`].
    pub(crate) segment_info_fields: u32,
    pub(crate) feature_data: [[i16; 8]; 8],
    pub(crate) feature_mask: [u8; 8],
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VAFilmGrainStructAV1 {
    /// `film_grain_info_fields.value`: see [`film_grain_info_fields`].
    pub(crate) film_grain_info_fields: u32,
    pub(crate) grain_seed: u16,
    pub(crate) num_y_points: u8,
    pub(crate) point_y_value: [u8; 14],
    pub(crate) point_y_scaling: [u8; 14],
    pub(crate) num_cb_points: u8,
    pub(crate) point_cb_value: [u8; 10],
    pub(crate) point_cb_scaling: [u8; 10],
    pub(crate) num_cr_points: u8,
    pub(crate) point_cr_value: [u8; 10],
    pub(crate) point_cr_scaling: [u8; 10],
    pub(crate) ar_coeffs_y: [i8; 24],
    pub(crate) ar_coeffs_cb: [i8; 25],
    pub(crate) ar_coeffs_cr: [i8; 25],
    pub(crate) cb_mult: u8,
    pub(crate) cb_luma_mult: u8,
    pub(crate) cb_offset: u16,
    pub(crate) cr_mult: u8,
    pub(crate) cr_luma_mult: u8,
    pub(crate) cr_offset: u16,
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VAWarpedMotionParamsAV1 {
    /// `VAAV1TransformationType`, a C enum.
    pub(crate) wmtype: i32,
    pub(crate) wmmat: [i32; 8],
    pub(crate) invalid: u8,
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct VADecPictureParameterBufferAV1 {
    pub(crate) profile: u8,
    pub(crate) order_hint_bits_minus_1: u8,
    pub(crate) bit_depth_idx: u8,
    pub(crate) matrix_coefficients: u8,
    /// `seq_info_fields.value`: see [`seq_info_fields`].
    pub(crate) seq_info_fields: u32,
    pub(crate) current_frame: VASurfaceID,
    pub(crate) current_display_picture: VASurfaceID,
    pub(crate) anchor_frames_num: u8,
    pub(crate) anchor_frames_list: *mut VASurfaceID,
    pub(crate) frame_width_minus1: u16,
    pub(crate) frame_height_minus1: u16,
    pub(crate) output_frame_width_in_tiles_minus_1: u16,
    pub(crate) output_frame_height_in_tiles_minus_1: u16,
    pub(crate) ref_frame_map: [VASurfaceID; 8],
    pub(crate) ref_frame_idx: [u8; 7],
    pub(crate) primary_ref_frame: u8,
    pub(crate) order_hint: u8,
    pub(crate) seg_info: VASegmentationStructAV1,
    pub(crate) film_grain_info: VAFilmGrainStructAV1,
    pub(crate) tile_cols: u8,
    pub(crate) tile_rows: u8,
    pub(crate) width_in_sbs_minus_1: [u16; 63],
    pub(crate) height_in_sbs_minus_1: [u16; 63],
    pub(crate) tile_count_minus_1: u16,
    pub(crate) context_update_tile_id: u16,
    /// `pic_info_fields.value`: see [`pic_info_fields`].
    pub(crate) pic_info_fields: u32,
    pub(crate) superres_scale_denominator: u8,
    pub(crate) interp_filter: u8,
    pub(crate) filter_level: [u8; 2],
    pub(crate) filter_level_u: u8,
    pub(crate) filter_level_v: u8,
    /// `loop_filter_info_fields.value`: see [`loop_filter_info_fields`].
    pub(crate) loop_filter_info_fields: u8,
    pub(crate) ref_deltas: [i8; 8],
    pub(crate) mode_deltas: [i8; 2],
    pub(crate) base_qindex: u8,
    pub(crate) y_dc_delta_q: i8,
    pub(crate) u_dc_delta_q: i8,
    pub(crate) u_ac_delta_q: i8,
    pub(crate) v_dc_delta_q: i8,
    pub(crate) v_ac_delta_q: i8,
    /// `qmatrix_fields.value`: see [`qmatrix_fields`].
    pub(crate) qmatrix_fields: u16,
    /// `mode_control_fields.value`: see [`mode_control_fields`].
    pub(crate) mode_control_fields: u32,
    pub(crate) cdef_damping_minus_3: u8,
    pub(crate) cdef_bits: u8,
    pub(crate) cdef_y_strengths: [u8; 8],
    pub(crate) cdef_uv_strengths: [u8; 8],
    /// `loop_restoration_fields.value`: see [`loop_restoration_fields`].
    pub(crate) loop_restoration_fields: u16,
    pub(crate) wm: [VAWarpedMotionParamsAV1; 7],
    pub(crate) va_reserved: [u32; VA_PADDING_MEDIUM],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VASliceParameterBufferAV1 {
    pub(crate) slice_data_size: u32,
    pub(crate) slice_data_offset: u32,
    pub(crate) slice_data_flag: u32,
    pub(crate) tile_row: u16,
    pub(crate) tile_column: u16,
    pub(crate) tg_start: u16,
    pub(crate) tg_end: u16,
    pub(crate) anchor_frame_idx: u8,
    pub(crate) tile_idx_in_tile_list: u16,
    pub(crate) va_reserved: [u32; VA_PADDING_LOW],
}

/// `VASegmentationStructAV1::segment_info_fields.bits`.
pub(crate) mod segment_info_fields {
    use super::{BitField, bits};
    pub(crate) const ENABLED: BitField = bits(0, 1);
    pub(crate) const UPDATE_MAP: BitField = bits(1, 1);
    pub(crate) const TEMPORAL_UPDATE: BitField = bits(2, 1);
    pub(crate) const UPDATE_DATA: BitField = bits(3, 1);
}

/// `VAFilmGrainStructAV1::film_grain_info_fields.bits`.
pub(crate) mod film_grain_info_fields {
    use super::{BitField, bits};
    pub(crate) const APPLY_GRAIN: BitField = bits(0, 1);
    pub(crate) const CHROMA_SCALING_FROM_LUMA: BitField = bits(1, 1);
    pub(crate) const GRAIN_SCALING_MINUS_8: BitField = bits(2, 2);
    pub(crate) const AR_COEFF_LAG: BitField = bits(4, 2);
    pub(crate) const AR_COEFF_SHIFT_MINUS_6: BitField = bits(6, 2);
    pub(crate) const GRAIN_SCALE_SHIFT: BitField = bits(8, 2);
    pub(crate) const OVERLAP_FLAG: BitField = bits(10, 1);
    pub(crate) const CLIP_TO_RESTRICTED_RANGE: BitField = bits(11, 1);
}

/// `VADecPictureParameterBufferAV1::seq_info_fields.fields`.
pub(crate) mod seq_info_fields {
    use super::{BitField, bits};
    pub(crate) const STILL_PICTURE: BitField = bits(0, 1);
    pub(crate) const USE_128X128_SUPERBLOCK: BitField = bits(1, 1);
    pub(crate) const ENABLE_FILTER_INTRA: BitField = bits(2, 1);
    pub(crate) const ENABLE_INTRA_EDGE_FILTER: BitField = bits(3, 1);
    pub(crate) const ENABLE_INTERINTRA_COMPOUND: BitField = bits(4, 1);
    pub(crate) const ENABLE_MASKED_COMPOUND: BitField = bits(5, 1);
    pub(crate) const ENABLE_DUAL_FILTER: BitField = bits(6, 1);
    pub(crate) const ENABLE_ORDER_HINT: BitField = bits(7, 1);
    pub(crate) const ENABLE_JNT_COMP: BitField = bits(8, 1);
    pub(crate) const ENABLE_CDEF: BitField = bits(9, 1);
    pub(crate) const MONO_CHROME: BitField = bits(10, 1);
    pub(crate) const COLOR_RANGE: BitField = bits(11, 1);
    pub(crate) const SUBSAMPLING_X: BitField = bits(12, 1);
    pub(crate) const SUBSAMPLING_Y: BitField = bits(13, 1);
    pub(crate) const CHROMA_SAMPLE_POSITION: BitField = bits(14, 1);
    pub(crate) const FILM_GRAIN_PARAMS_PRESENT: BitField = bits(15, 1);
}

/// `VADecPictureParameterBufferAV1::pic_info_fields.bits`.
pub(crate) mod pic_info_fields {
    use super::{BitField, bits};
    pub(crate) const FRAME_TYPE: BitField = bits(0, 2);
    pub(crate) const SHOW_FRAME: BitField = bits(2, 1);
    pub(crate) const SHOWABLE_FRAME: BitField = bits(3, 1);
    pub(crate) const ERROR_RESILIENT_MODE: BitField = bits(4, 1);
    pub(crate) const DISABLE_CDF_UPDATE: BitField = bits(5, 1);
    pub(crate) const ALLOW_SCREEN_CONTENT_TOOLS: BitField = bits(6, 1);
    pub(crate) const FORCE_INTEGER_MV: BitField = bits(7, 1);
    pub(crate) const ALLOW_INTRABC: BitField = bits(8, 1);
    pub(crate) const USE_SUPERRES: BitField = bits(9, 1);
    pub(crate) const ALLOW_HIGH_PRECISION_MV: BitField = bits(10, 1);
    pub(crate) const IS_MOTION_MODE_SWITCHABLE: BitField = bits(11, 1);
    pub(crate) const USE_REF_FRAME_MVS: BitField = bits(12, 1);
    pub(crate) const DISABLE_FRAME_END_UPDATE_CDF: BitField = bits(13, 1);
    pub(crate) const UNIFORM_TILE_SPACING_FLAG: BitField = bits(14, 1);
    pub(crate) const ALLOW_WARPED_MOTION: BitField = bits(15, 1);
    pub(crate) const LARGE_SCALE_TILE: BitField = bits(16, 1);
}

/// `VADecPictureParameterBufferAV1::loop_filter_info_fields.bits`, a
/// `uint8_t`.
pub(crate) mod loop_filter_info_fields {
    use super::{BitField, bits};
    pub(crate) const SHARPNESS_LEVEL: BitField = bits(0, 3);
    pub(crate) const MODE_REF_DELTA_ENABLED: BitField = bits(3, 1);
    pub(crate) const MODE_REF_DELTA_UPDATE: BitField = bits(4, 1);
}

/// `VADecPictureParameterBufferAV1::qmatrix_fields.bits`, a `uint16_t`.
pub(crate) mod qmatrix_fields {
    use super::{BitField, bits};
    pub(crate) const USING_QMATRIX: BitField = bits(0, 1);
    pub(crate) const QM_Y: BitField = bits(1, 4);
    pub(crate) const QM_U: BitField = bits(5, 4);
    pub(crate) const QM_V: BitField = bits(9, 4);
}

/// `VADecPictureParameterBufferAV1::mode_control_fields.bits`.
pub(crate) mod mode_control_fields {
    use super::{BitField, bits};
    pub(crate) const DELTA_Q_PRESENT_FLAG: BitField = bits(0, 1);
    pub(crate) const LOG2_DELTA_Q_RES: BitField = bits(1, 2);
    pub(crate) const DELTA_LF_PRESENT_FLAG: BitField = bits(3, 1);
    pub(crate) const LOG2_DELTA_LF_RES: BitField = bits(4, 2);
    pub(crate) const DELTA_LF_MULTI: BitField = bits(6, 1);
    pub(crate) const TX_MODE: BitField = bits(7, 2);
    pub(crate) const REFERENCE_SELECT: BitField = bits(9, 1);
    pub(crate) const REDUCED_TX_SET_USED: BitField = bits(10, 1);
    pub(crate) const SKIP_MODE_PRESENT: BitField = bits(11, 1);
}

/// `VADecPictureParameterBufferAV1::loop_restoration_fields.bits`, a
/// `uint16_t`.
pub(crate) mod loop_restoration_fields {
    use super::{BitField, bits};
    pub(crate) const YFRAME_RESTORATION_TYPE: BitField = bits(0, 2);
    pub(crate) const CBFRAME_RESTORATION_TYPE: BitField = bits(2, 2);
    pub(crate) const CRFRAME_RESTORATION_TYPE: BitField = bits(4, 2);
    pub(crate) const LR_UNIT_SHIFT: BitField = bits(6, 2);
    pub(crate) const LR_UV_SHIFT: BitField = bits(8, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// What the C side prints, line for line.
    const LAYOUT_C: &str = include_str!("ffi_layout.c");

    macro_rules! layout {
        ($out:ident, $ty:ty, $c:literal, { $($field:ident),* $(,)? }) => {
            $out.push(format!("{} size {}", $c, size_of::<$ty>()));
            $( $out.push(format!("{}.{} {}", $c, stringify!($field), offset_of!($ty, $field))); )*
        };
    }

    fn rust_layout() -> Vec<String> {
        let mut out = Vec::new();
        layout!(out, VASegmentationStructAV1, "VASegmentationStructAV1", {
            segment_info_fields, feature_data, feature_mask, va_reserved,
        });
        layout!(out, VAFilmGrainStructAV1, "VAFilmGrainStructAV1", {
            film_grain_info_fields, grain_seed, num_y_points, point_y_value, point_y_scaling,
            num_cb_points, point_cb_value, point_cb_scaling, num_cr_points, point_cr_value,
            point_cr_scaling, ar_coeffs_y, ar_coeffs_cb, ar_coeffs_cr, cb_mult, cb_luma_mult,
            cb_offset, cr_mult, cr_luma_mult, cr_offset, va_reserved,
        });
        layout!(out, VAWarpedMotionParamsAV1, "VAWarpedMotionParamsAV1", {
            wmtype, wmmat, invalid, va_reserved,
        });
        layout!(out, VADecPictureParameterBufferAV1, "VADecPictureParameterBufferAV1", {
            profile, order_hint_bits_minus_1, bit_depth_idx, matrix_coefficients,
            seq_info_fields, current_frame, current_display_picture, anchor_frames_num,
            anchor_frames_list, frame_width_minus1, frame_height_minus1,
            output_frame_width_in_tiles_minus_1, output_frame_height_in_tiles_minus_1,
            ref_frame_map, ref_frame_idx, primary_ref_frame, order_hint, seg_info,
            film_grain_info, tile_cols, tile_rows, width_in_sbs_minus_1,
            height_in_sbs_minus_1, tile_count_minus_1, context_update_tile_id,
            pic_info_fields, superres_scale_denominator, interp_filter, filter_level,
            filter_level_u, filter_level_v, loop_filter_info_fields, ref_deltas, mode_deltas,
            base_qindex, y_dc_delta_q, u_dc_delta_q, u_ac_delta_q, v_dc_delta_q, v_ac_delta_q,
            qmatrix_fields, mode_control_fields, cdef_damping_minus_3, cdef_bits,
            cdef_y_strengths, cdef_uv_strengths, loop_restoration_fields, wm, va_reserved,
        });
        layout!(out, VASliceParameterBufferAV1, "VASliceParameterBufferAV1", {
            slice_data_size, slice_data_offset, slice_data_flag, tile_row, tile_column,
            tg_start, tg_end, anchor_frame_idx, tile_idx_in_tile_list, va_reserved,
        });
        let fields: [(&str, &[(&str, BitField)]); 7] = [
            (
                "segment_info_fields",
                &[
                    ("enabled", segment_info_fields::ENABLED),
                    ("update_map", segment_info_fields::UPDATE_MAP),
                    ("temporal_update", segment_info_fields::TEMPORAL_UPDATE),
                    ("update_data", segment_info_fields::UPDATE_DATA),
                ],
            ),
            (
                "film_grain_info_fields",
                &[
                    ("apply_grain", film_grain_info_fields::APPLY_GRAIN),
                    (
                        "chroma_scaling_from_luma",
                        film_grain_info_fields::CHROMA_SCALING_FROM_LUMA,
                    ),
                    (
                        "grain_scaling_minus_8",
                        film_grain_info_fields::GRAIN_SCALING_MINUS_8,
                    ),
                    ("ar_coeff_lag", film_grain_info_fields::AR_COEFF_LAG),
                    (
                        "ar_coeff_shift_minus_6",
                        film_grain_info_fields::AR_COEFF_SHIFT_MINUS_6,
                    ),
                    (
                        "grain_scale_shift",
                        film_grain_info_fields::GRAIN_SCALE_SHIFT,
                    ),
                    ("overlap_flag", film_grain_info_fields::OVERLAP_FLAG),
                    (
                        "clip_to_restricted_range",
                        film_grain_info_fields::CLIP_TO_RESTRICTED_RANGE,
                    ),
                ],
            ),
            (
                "seq_info_fields",
                &[
                    ("still_picture", seq_info_fields::STILL_PICTURE),
                    (
                        "use_128x128_superblock",
                        seq_info_fields::USE_128X128_SUPERBLOCK,
                    ),
                    ("enable_filter_intra", seq_info_fields::ENABLE_FILTER_INTRA),
                    (
                        "enable_intra_edge_filter",
                        seq_info_fields::ENABLE_INTRA_EDGE_FILTER,
                    ),
                    (
                        "enable_interintra_compound",
                        seq_info_fields::ENABLE_INTERINTRA_COMPOUND,
                    ),
                    (
                        "enable_masked_compound",
                        seq_info_fields::ENABLE_MASKED_COMPOUND,
                    ),
                    ("enable_dual_filter", seq_info_fields::ENABLE_DUAL_FILTER),
                    ("enable_order_hint", seq_info_fields::ENABLE_ORDER_HINT),
                    ("enable_jnt_comp", seq_info_fields::ENABLE_JNT_COMP),
                    ("enable_cdef", seq_info_fields::ENABLE_CDEF),
                    ("mono_chrome", seq_info_fields::MONO_CHROME),
                    ("color_range", seq_info_fields::COLOR_RANGE),
                    ("subsampling_x", seq_info_fields::SUBSAMPLING_X),
                    ("subsampling_y", seq_info_fields::SUBSAMPLING_Y),
                    (
                        "chroma_sample_position",
                        seq_info_fields::CHROMA_SAMPLE_POSITION,
                    ),
                    (
                        "film_grain_params_present",
                        seq_info_fields::FILM_GRAIN_PARAMS_PRESENT,
                    ),
                ],
            ),
            (
                "pic_info_fields",
                &[
                    ("frame_type", pic_info_fields::FRAME_TYPE),
                    ("show_frame", pic_info_fields::SHOW_FRAME),
                    ("showable_frame", pic_info_fields::SHOWABLE_FRAME),
                    (
                        "error_resilient_mode",
                        pic_info_fields::ERROR_RESILIENT_MODE,
                    ),
                    ("disable_cdf_update", pic_info_fields::DISABLE_CDF_UPDATE),
                    (
                        "allow_screen_content_tools",
                        pic_info_fields::ALLOW_SCREEN_CONTENT_TOOLS,
                    ),
                    ("force_integer_mv", pic_info_fields::FORCE_INTEGER_MV),
                    ("allow_intrabc", pic_info_fields::ALLOW_INTRABC),
                    ("use_superres", pic_info_fields::USE_SUPERRES),
                    (
                        "allow_high_precision_mv",
                        pic_info_fields::ALLOW_HIGH_PRECISION_MV,
                    ),
                    (
                        "is_motion_mode_switchable",
                        pic_info_fields::IS_MOTION_MODE_SWITCHABLE,
                    ),
                    ("use_ref_frame_mvs", pic_info_fields::USE_REF_FRAME_MVS),
                    (
                        "disable_frame_end_update_cdf",
                        pic_info_fields::DISABLE_FRAME_END_UPDATE_CDF,
                    ),
                    (
                        "uniform_tile_spacing_flag",
                        pic_info_fields::UNIFORM_TILE_SPACING_FLAG,
                    ),
                    ("allow_warped_motion", pic_info_fields::ALLOW_WARPED_MOTION),
                    ("large_scale_tile", pic_info_fields::LARGE_SCALE_TILE),
                ],
            ),
            (
                "loop_filter_info_fields",
                &[
                    ("sharpness_level", loop_filter_info_fields::SHARPNESS_LEVEL),
                    (
                        "mode_ref_delta_enabled",
                        loop_filter_info_fields::MODE_REF_DELTA_ENABLED,
                    ),
                    (
                        "mode_ref_delta_update",
                        loop_filter_info_fields::MODE_REF_DELTA_UPDATE,
                    ),
                ],
            ),
            (
                "qmatrix_fields",
                &[
                    ("using_qmatrix", qmatrix_fields::USING_QMATRIX),
                    ("qm_y", qmatrix_fields::QM_Y),
                    ("qm_u", qmatrix_fields::QM_U),
                    ("qm_v", qmatrix_fields::QM_V),
                ],
            ),
            (
                "mode_control_fields",
                &[
                    (
                        "delta_q_present_flag",
                        mode_control_fields::DELTA_Q_PRESENT_FLAG,
                    ),
                    ("log2_delta_q_res", mode_control_fields::LOG2_DELTA_Q_RES),
                    (
                        "delta_lf_present_flag",
                        mode_control_fields::DELTA_LF_PRESENT_FLAG,
                    ),
                    ("log2_delta_lf_res", mode_control_fields::LOG2_DELTA_LF_RES),
                    ("delta_lf_multi", mode_control_fields::DELTA_LF_MULTI),
                    ("tx_mode", mode_control_fields::TX_MODE),
                    ("reference_select", mode_control_fields::REFERENCE_SELECT),
                    (
                        "reduced_tx_set_used",
                        mode_control_fields::REDUCED_TX_SET_USED,
                    ),
                    ("skip_mode_present", mode_control_fields::SKIP_MODE_PRESENT),
                ],
            ),
        ];
        for (union, list) in fields {
            for (name, field) in list {
                out.push(format!("{union}.{name} {:#x}", field.put(u32::MAX)));
            }
        }
        out.push(format!(
            "loop_restoration_fields {:#x} {:#x} {:#x} {:#x} {:#x}",
            loop_restoration_fields::YFRAME_RESTORATION_TYPE.put(u32::MAX),
            loop_restoration_fields::CBFRAME_RESTORATION_TYPE.put(u32::MAX),
            loop_restoration_fields::CRFRAME_RESTORATION_TYPE.put(u32::MAX),
            loop_restoration_fields::LR_UNIT_SHIFT.put(u32::MAX),
            loop_restoration_fields::LR_UV_SHIFT.put(u32::MAX),
        ));
        out.push(format!("VAProfileAV1Profile0 {VAProfileAV1Profile0}"));
        out
    }

    /// Compiles `ffi_layout.c` against the installed `va_dec_av1.h` and
    /// compares its output with this module's layout. `CC` picks the
    /// compiler (`cc` by default).
    #[test]
    #[ignore = "needs libva's headers and a C compiler: run on the test host"]
    fn av1_layouts_match_libva_headers() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("rvgv-va-av1-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let binary = dir.join("layout");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let mut compile = Command::new(cc)
            .args(["-Wno-overflow", "-x", "c", "-", "-o"])
            .arg(&binary)
            .stdin(Stdio::piped())
            .spawn()?;
        compile
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(LAYOUT_C.as_bytes())?;
        if !compile.wait()?.success() {
            return Err("the C compiler failed".into());
        }
        let output = Command::new(&binary).output()?;
        std::fs::remove_dir_all(&dir)?;
        let c_lines: Vec<String> = String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(rust_layout(), c_lines);
        Ok(())
    }
}
