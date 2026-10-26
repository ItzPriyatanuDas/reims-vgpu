/*
 * The layout of the VP9 decoding structs vp9/ffi.rs declares, from the
 * installed va_dec_vp9.h: sizeof and offsetof of each, then the mask of
 * each bit-field. vp9/ffi.rs's vp9_layouts_match_libva_headers test
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
#define PIC(f) BIT(VADecPictureParameterBufferVP9, pic_fields, bits, f)
#define SEG(f) BIT(VASegmentParameterVP9, segment_flags, fields, f)

int main(void)
{
    S(VADecPictureParameterBufferVP9);
    F(VADecPictureParameterBufferVP9, frame_width);
    F(VADecPictureParameterBufferVP9, frame_height);
    F(VADecPictureParameterBufferVP9, reference_frames);
    F(VADecPictureParameterBufferVP9, pic_fields);
    F(VADecPictureParameterBufferVP9, filter_level);
    F(VADecPictureParameterBufferVP9, sharpness_level);
    F(VADecPictureParameterBufferVP9, log2_tile_rows);
    F(VADecPictureParameterBufferVP9, log2_tile_columns);
    F(VADecPictureParameterBufferVP9, frame_header_length_in_bytes);
    F(VADecPictureParameterBufferVP9, first_partition_size);
    F(VADecPictureParameterBufferVP9, mb_segment_tree_probs);
    F(VADecPictureParameterBufferVP9, segment_pred_probs);
    F(VADecPictureParameterBufferVP9, profile);
    F(VADecPictureParameterBufferVP9, bit_depth);
    F(VADecPictureParameterBufferVP9, va_reserved);

    S(VASegmentParameterVP9);
    F(VASegmentParameterVP9, segment_flags);
    F(VASegmentParameterVP9, filter_level);
    F(VASegmentParameterVP9, luma_ac_quant_scale);
    F(VASegmentParameterVP9, luma_dc_quant_scale);
    F(VASegmentParameterVP9, chroma_ac_quant_scale);
    F(VASegmentParameterVP9, chroma_dc_quant_scale);
    F(VASegmentParameterVP9, va_reserved);

    S(VASliceParameterBufferVP9);
    F(VASliceParameterBufferVP9, slice_data_size);
    F(VASliceParameterBufferVP9, slice_data_offset);
    F(VASliceParameterBufferVP9, slice_data_flag);
    F(VASliceParameterBufferVP9, seg_param);
    F(VASliceParameterBufferVP9, va_reserved);

    PIC(subsampling_x);
    PIC(subsampling_y);
    PIC(frame_type);
    PIC(show_frame);
    PIC(error_resilient_mode);
    PIC(intra_only);
    PIC(allow_high_precision_mv);
    PIC(mcomp_filter_type);
    PIC(frame_parallel_decoding_mode);
    PIC(reset_frame_context);
    PIC(refresh_frame_context);
    PIC(frame_context_idx);
    PIC(segmentation_enabled);
    PIC(segmentation_temporal_update);
    PIC(segmentation_update_map);
    PIC(last_ref_frame);
    PIC(last_ref_frame_sign_bias);
    PIC(golden_ref_frame);
    PIC(golden_ref_frame_sign_bias);
    PIC(alt_ref_frame);
    PIC(alt_ref_frame_sign_bias);
    PIC(lossless_flag);

    SEG(segment_reference_enabled);
    SEG(segment_reference);
    SEG(segment_reference_skipped);

    printf("VAProfileVP9Profile0 %d\n", (int)VAProfileVP9Profile0);
    printf("VAProfileVP9Profile2 %d\n", (int)VAProfileVP9Profile2);
    return 0;
}
