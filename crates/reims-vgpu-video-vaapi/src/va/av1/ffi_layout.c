/*
 * The layout of the AV1 decoding structs av1/ffi.rs declares, from the
 * installed va_dec_av1.h: sizeof and offsetof of each, then the mask of
 * each bit-field. av1/ffi.rs's av1_layouts_match_libva_headers test
 * compiles this and compares its output with Rust's, line for line.
 */
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <va/va.h>

#define S(T) printf(#T " size %zu\n", sizeof(T))
#define F(T, f) printf(#T "." #f " %zu\n", offsetof(T, f))
/* One bit-field of a union member `u` of struct `T`, its struct named `b`. */
#define BIT(T, u, b, f)                                                \
    do {                                                               \
        T p;                                                           \
        memset(&p, 0, sizeof p);                                       \
        p.u.b.f = ~0u;                                                 \
        printf(#u "." #f " %#x\n", (unsigned)p.u.value);               \
    } while (0)
#define SEG(f) BIT(VASegmentationStructAV1, segment_info_fields, bits, f)
#define FG(f) BIT(VAFilmGrainStructAV1, film_grain_info_fields, bits, f)
#define SEQ(f) BIT(VADecPictureParameterBufferAV1, seq_info_fields, fields, f)
#define PIC(f) BIT(VADecPictureParameterBufferAV1, pic_info_fields, bits, f)
#define LF(f) BIT(VADecPictureParameterBufferAV1, loop_filter_info_fields, bits, f)
#define QM(f) BIT(VADecPictureParameterBufferAV1, qmatrix_fields, bits, f)
#define MODE(f) BIT(VADecPictureParameterBufferAV1, mode_control_fields, bits, f)

static unsigned lr(int which)
{
    VADecPictureParameterBufferAV1 p;
    memset(&p, 0, sizeof p);
    switch (which) {
    case 0: p.loop_restoration_fields.bits.yframe_restoration_type = ~0u; break;
    case 1: p.loop_restoration_fields.bits.cbframe_restoration_type = ~0u; break;
    case 2: p.loop_restoration_fields.bits.crframe_restoration_type = ~0u; break;
    case 3: p.loop_restoration_fields.bits.lr_unit_shift = ~0u; break;
    default: p.loop_restoration_fields.bits.lr_uv_shift = ~0u; break;
    }
    return p.loop_restoration_fields.value;
}

int main(void)
{
    S(VASegmentationStructAV1);
    F(VASegmentationStructAV1, segment_info_fields);
    F(VASegmentationStructAV1, feature_data);
    F(VASegmentationStructAV1, feature_mask);
    F(VASegmentationStructAV1, va_reserved);

    S(VAFilmGrainStructAV1);
    F(VAFilmGrainStructAV1, film_grain_info_fields);
    F(VAFilmGrainStructAV1, grain_seed);
    F(VAFilmGrainStructAV1, num_y_points);
    F(VAFilmGrainStructAV1, point_y_value);
    F(VAFilmGrainStructAV1, point_y_scaling);
    F(VAFilmGrainStructAV1, num_cb_points);
    F(VAFilmGrainStructAV1, point_cb_value);
    F(VAFilmGrainStructAV1, point_cb_scaling);
    F(VAFilmGrainStructAV1, num_cr_points);
    F(VAFilmGrainStructAV1, point_cr_value);
    F(VAFilmGrainStructAV1, point_cr_scaling);
    F(VAFilmGrainStructAV1, ar_coeffs_y);
    F(VAFilmGrainStructAV1, ar_coeffs_cb);
    F(VAFilmGrainStructAV1, ar_coeffs_cr);
    F(VAFilmGrainStructAV1, cb_mult);
    F(VAFilmGrainStructAV1, cb_luma_mult);
    F(VAFilmGrainStructAV1, cb_offset);
    F(VAFilmGrainStructAV1, cr_mult);
    F(VAFilmGrainStructAV1, cr_luma_mult);
    F(VAFilmGrainStructAV1, cr_offset);
    F(VAFilmGrainStructAV1, va_reserved);

    S(VAWarpedMotionParamsAV1);
    F(VAWarpedMotionParamsAV1, wmtype);
    F(VAWarpedMotionParamsAV1, wmmat);
    F(VAWarpedMotionParamsAV1, invalid);
    F(VAWarpedMotionParamsAV1, va_reserved);

    S(VADecPictureParameterBufferAV1);
    F(VADecPictureParameterBufferAV1, profile);
    F(VADecPictureParameterBufferAV1, order_hint_bits_minus_1);
    F(VADecPictureParameterBufferAV1, bit_depth_idx);
    F(VADecPictureParameterBufferAV1, matrix_coefficients);
    F(VADecPictureParameterBufferAV1, seq_info_fields);
    F(VADecPictureParameterBufferAV1, current_frame);
    F(VADecPictureParameterBufferAV1, current_display_picture);
    F(VADecPictureParameterBufferAV1, anchor_frames_num);
    F(VADecPictureParameterBufferAV1, anchor_frames_list);
    F(VADecPictureParameterBufferAV1, frame_width_minus1);
    F(VADecPictureParameterBufferAV1, frame_height_minus1);
    F(VADecPictureParameterBufferAV1, output_frame_width_in_tiles_minus_1);
    F(VADecPictureParameterBufferAV1, output_frame_height_in_tiles_minus_1);
    F(VADecPictureParameterBufferAV1, ref_frame_map);
    F(VADecPictureParameterBufferAV1, ref_frame_idx);
    F(VADecPictureParameterBufferAV1, primary_ref_frame);
    F(VADecPictureParameterBufferAV1, order_hint);
    F(VADecPictureParameterBufferAV1, seg_info);
    F(VADecPictureParameterBufferAV1, film_grain_info);
    F(VADecPictureParameterBufferAV1, tile_cols);
    F(VADecPictureParameterBufferAV1, tile_rows);
    F(VADecPictureParameterBufferAV1, width_in_sbs_minus_1);
    F(VADecPictureParameterBufferAV1, height_in_sbs_minus_1);
    F(VADecPictureParameterBufferAV1, tile_count_minus_1);
    F(VADecPictureParameterBufferAV1, context_update_tile_id);
    F(VADecPictureParameterBufferAV1, pic_info_fields);
    F(VADecPictureParameterBufferAV1, superres_scale_denominator);
    F(VADecPictureParameterBufferAV1, interp_filter);
    F(VADecPictureParameterBufferAV1, filter_level);
    F(VADecPictureParameterBufferAV1, filter_level_u);
    F(VADecPictureParameterBufferAV1, filter_level_v);
    F(VADecPictureParameterBufferAV1, loop_filter_info_fields);
    F(VADecPictureParameterBufferAV1, ref_deltas);
    F(VADecPictureParameterBufferAV1, mode_deltas);
    F(VADecPictureParameterBufferAV1, base_qindex);
    F(VADecPictureParameterBufferAV1, y_dc_delta_q);
    F(VADecPictureParameterBufferAV1, u_dc_delta_q);
    F(VADecPictureParameterBufferAV1, u_ac_delta_q);
    F(VADecPictureParameterBufferAV1, v_dc_delta_q);
    F(VADecPictureParameterBufferAV1, v_ac_delta_q);
    F(VADecPictureParameterBufferAV1, qmatrix_fields);
    F(VADecPictureParameterBufferAV1, mode_control_fields);
    F(VADecPictureParameterBufferAV1, cdef_damping_minus_3);
    F(VADecPictureParameterBufferAV1, cdef_bits);
    F(VADecPictureParameterBufferAV1, cdef_y_strengths);
    F(VADecPictureParameterBufferAV1, cdef_uv_strengths);
    F(VADecPictureParameterBufferAV1, loop_restoration_fields);
    F(VADecPictureParameterBufferAV1, wm);
    F(VADecPictureParameterBufferAV1, va_reserved);

    S(VASliceParameterBufferAV1);
    F(VASliceParameterBufferAV1, slice_data_size);
    F(VASliceParameterBufferAV1, slice_data_offset);
    F(VASliceParameterBufferAV1, slice_data_flag);
    F(VASliceParameterBufferAV1, tile_row);
    F(VASliceParameterBufferAV1, tile_column);
    F(VASliceParameterBufferAV1, tg_start);
    F(VASliceParameterBufferAV1, tg_end);
    F(VASliceParameterBufferAV1, anchor_frame_idx);
    F(VASliceParameterBufferAV1, tile_idx_in_tile_list);
    F(VASliceParameterBufferAV1, va_reserved);

    SEG(enabled);
    SEG(update_map);
    SEG(temporal_update);
    SEG(update_data);

    FG(apply_grain);
    FG(chroma_scaling_from_luma);
    FG(grain_scaling_minus_8);
    FG(ar_coeff_lag);
    FG(ar_coeff_shift_minus_6);
    FG(grain_scale_shift);
    FG(overlap_flag);
    FG(clip_to_restricted_range);

    SEQ(still_picture);
    SEQ(use_128x128_superblock);
    SEQ(enable_filter_intra);
    SEQ(enable_intra_edge_filter);
    SEQ(enable_interintra_compound);
    SEQ(enable_masked_compound);
    SEQ(enable_dual_filter);
    SEQ(enable_order_hint);
    SEQ(enable_jnt_comp);
    SEQ(enable_cdef);
    SEQ(mono_chrome);
    SEQ(color_range);
    SEQ(subsampling_x);
    SEQ(subsampling_y);
    SEQ(chroma_sample_position);
    SEQ(film_grain_params_present);

    PIC(frame_type);
    PIC(show_frame);
    PIC(showable_frame);
    PIC(error_resilient_mode);
    PIC(disable_cdf_update);
    PIC(allow_screen_content_tools);
    PIC(force_integer_mv);
    PIC(allow_intrabc);
    PIC(use_superres);
    PIC(allow_high_precision_mv);
    PIC(is_motion_mode_switchable);
    PIC(use_ref_frame_mvs);
    PIC(disable_frame_end_update_cdf);
    PIC(uniform_tile_spacing_flag);
    PIC(allow_warped_motion);
    PIC(large_scale_tile);

    LF(sharpness_level);
    LF(mode_ref_delta_enabled);
    LF(mode_ref_delta_update);

    QM(using_qmatrix);
    QM(qm_y);
    QM(qm_u);
    QM(qm_v);

    MODE(delta_q_present_flag);
    MODE(log2_delta_q_res);
    MODE(delta_lf_present_flag);
    MODE(log2_delta_lf_res);
    MODE(delta_lf_multi);
    MODE(tx_mode);
    MODE(reference_select);
    MODE(reduced_tx_set_used);
    MODE(skip_mode_present);

    printf("loop_restoration_fields %#x %#x %#x %#x %#x\n", lr(0), lr(1), lr(2), lr(3), lr(4));
    printf("VAProfileAV1Profile0 %d\n", (int)VAProfileAV1Profile0);
    return 0;
}
