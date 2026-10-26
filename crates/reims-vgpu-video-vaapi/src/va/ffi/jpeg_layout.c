/*
 * The layout of the libva baseline JPEG decode structs ffi/jpeg.rs
 * declares, from the installed headers: sizeof and offsetof of each, then
 * the value of each constant. jpeg.rs's jpeg_layouts_match_libva_headers
 * test compiles this and compares its output with Rust's, line for line.
 */
#include <stddef.h>
#include <stdio.h>
#include <va/va.h>
#include <va/va_dec_jpeg.h>

#define S(T) printf(#T " size %zu\n", sizeof(T))
#define F(T, f) printf(#T "." #f " %zu\n", offsetof(T, f))
/* A member of an anonymous struct, through an element of the array m. */
#define MS(label, T, m) printf(label " size %zu\n", sizeof(((T *)0)->m[0]))
#define MF(label, T, m, f)                                             \
    printf(label "." #f " %zu\n",                                      \
           (size_t)((char *)&((T *)0)->m[0].f - (char *)&((T *)0)->m[0]))
#define C(c) printf(#c " %lld\n", (long long)(c))

int main(void)
{
    S(VAPictureParameterBufferJPEGBaseline);
    F(VAPictureParameterBufferJPEGBaseline, picture_width);
    F(VAPictureParameterBufferJPEGBaseline, picture_height);
    F(VAPictureParameterBufferJPEGBaseline, components);
    F(VAPictureParameterBufferJPEGBaseline, num_components);
    F(VAPictureParameterBufferJPEGBaseline, color_space);
    F(VAPictureParameterBufferJPEGBaseline, rotation);
    F(VAPictureParameterBufferJPEGBaseline, crop_rectangle);
    F(VAPictureParameterBufferJPEGBaseline, va_reserved);

    MS("picture.components", VAPictureParameterBufferJPEGBaseline, components);
    MF("picture.components", VAPictureParameterBufferJPEGBaseline, components, component_id);
    MF("picture.components", VAPictureParameterBufferJPEGBaseline, components, h_sampling_factor);
    MF("picture.components", VAPictureParameterBufferJPEGBaseline, components, v_sampling_factor);
    MF("picture.components", VAPictureParameterBufferJPEGBaseline, components,
       quantiser_table_selector);

    S(VAIQMatrixBufferJPEGBaseline);
    F(VAIQMatrixBufferJPEGBaseline, load_quantiser_table);
    F(VAIQMatrixBufferJPEGBaseline, quantiser_table);
    F(VAIQMatrixBufferJPEGBaseline, va_reserved);

    S(VAHuffmanTableBufferJPEGBaseline);
    F(VAHuffmanTableBufferJPEGBaseline, load_huffman_table);
    F(VAHuffmanTableBufferJPEGBaseline, huffman_table);
    F(VAHuffmanTableBufferJPEGBaseline, va_reserved);

    MS("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table);
    MF("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table, num_dc_codes);
    MF("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table, dc_values);
    MF("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table, num_ac_codes);
    MF("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table, ac_values);
    MF("huffman.huffman_table", VAHuffmanTableBufferJPEGBaseline, huffman_table, pad);

    S(VASliceParameterBufferJPEGBaseline);
    F(VASliceParameterBufferJPEGBaseline, slice_data_size);
    F(VASliceParameterBufferJPEGBaseline, slice_data_offset);
    F(VASliceParameterBufferJPEGBaseline, slice_data_flag);
    F(VASliceParameterBufferJPEGBaseline, slice_horizontal_position);
    F(VASliceParameterBufferJPEGBaseline, slice_vertical_position);
    F(VASliceParameterBufferJPEGBaseline, components);
    F(VASliceParameterBufferJPEGBaseline, num_components);
    F(VASliceParameterBufferJPEGBaseline, restart_interval);
    F(VASliceParameterBufferJPEGBaseline, num_mcus);
    F(VASliceParameterBufferJPEGBaseline, va_reserved);

    MS("slice.components", VASliceParameterBufferJPEGBaseline, components);
    MF("slice.components", VASliceParameterBufferJPEGBaseline, components, component_selector);
    MF("slice.components", VASliceParameterBufferJPEGBaseline, components, dc_table_selector);
    MF("slice.components", VASliceParameterBufferJPEGBaseline, components, ac_table_selector);

    C(VAProfileJPEGBaseline);
    C(VAHuffmanTableBufferType);
    C(VAConfigAttribDecJPEG);
    C(VA_RT_FORMAT_YUV422);
    C(VA_RT_FORMAT_YUV444);
    C(VA_RT_FORMAT_YUV411);
    C(VA_RT_FORMAT_YUV400);
    C(VA_FOURCC_YUY2);
    C(VA_FOURCC_Y800);
    C(VA_FOURCC_411P);
    C(VA_FOURCC_422H);
    C(VA_FOURCC_422V);
    C(VA_FOURCC_444P);
    C(VA_ROTATION_NONE);
    C(VA_SLICE_DATA_FLAG_ALL);
    return 0;
}
