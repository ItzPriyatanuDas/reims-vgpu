/*
 * A test oracle for the JPEG engine's decoder: libjpeg's decode of a JPEG,
 * written as its components' samples, with no upsampling and no colour
 * conversion (raw data output), each plane at its own size and tightly
 * packed, Y then Cb then Cr. That is what a hardware decoder writes, and what
 * jpeg-check compares with.
 *
 *   cc -O2 -o jpeg-planes jpeg-planes.c -ljpeg
 *   jpeg-planes [islow|ifast|float] IN.jpg OUT.yuv
 *
 * It exits 1, having written no complete OUT, where libjpeg warns of the
 * stream (corrupt data, a scan cut short) or gives fewer rows than a plane
 * has: a reference made of such a decode is no reference. jpeg-check.sh
 * builds and runs it; it is never linked into the library.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <jpeglib.h>

static void *checked_malloc(size_t size)
{
    void *p = malloc(size ? size : 1);
    if (!p) {
        perror("malloc");
        exit(1);
    }
    return p;
}

int main(int argc, char **argv)
{
    if (argc != 4) {
        fprintf(stderr, "usage: %s islow|ifast|float IN.jpg OUT.yuv\n", argv[0]);
        return 2;
    }
    J_DCT_METHOD method = JDCT_ISLOW;
    if (strcmp(argv[1], "ifast") == 0)
        method = JDCT_IFAST;
    else if (strcmp(argv[1], "float") == 0)
        method = JDCT_FLOAT;
    else if (strcmp(argv[1], "islow") != 0) {
        fprintf(stderr, "%s: not a DCT method\n", argv[1]);
        return 2;
    }
    FILE *in = fopen(argv[2], "rb");
    if (!in) {
        perror(argv[2]);
        return 1;
    }
    /* libjpeg's default error handler ends the process on an error. */
    struct jpeg_decompress_struct cinfo;
    struct jpeg_error_mgr jerr;
    cinfo.err = jpeg_std_error(&jerr);
    jpeg_create_decompress(&cinfo);
    jpeg_stdio_src(&cinfo, in);
    jpeg_read_header(&cinfo, TRUE);
    cinfo.raw_data_out = TRUE;
    cinfo.dct_method = method;
    /* Keep the components as they are: no YCbCr to RGB. */
    cinfo.out_color_space = cinfo.jpeg_color_space;
    jpeg_start_decompress(&cinfo);

    int n = cinfo.num_components;
    int max_v = cinfo.max_v_samp_factor;
    /* The rows of each component that one jpeg_read_raw_data call gives. */
    JSAMPARRAY rows[MAX_COMPONENTS];
    int counts[MAX_COMPONENTS];
    unsigned char *planes[MAX_COMPONENTS];
    JDIMENSION widths[MAX_COMPONENTS], heights[MAX_COMPONENTS];
    for (int c = 0; c < n; c++) {
        jpeg_component_info *comp = &cinfo.comp_info[c];
        JDIMENSION padded = comp->width_in_blocks * DCTSIZE;
        int count = comp->v_samp_factor * DCTSIZE;
        counts[c] = count;
        rows[c] = checked_malloc(sizeof(JSAMPROW) * count);
        for (int r = 0; r < count; r++)
            rows[c][r] = checked_malloc(padded);
        widths[c] = comp->downsampled_width;
        heights[c] = comp->downsampled_height;
        planes[c] = checked_malloc((size_t)widths[c] * heights[c]);
    }
    JDIMENSION done[MAX_COMPONENTS] = {0};
    while (cinfo.output_scanline < cinfo.output_height) {
        JDIMENSION read = jpeg_read_raw_data(&cinfo, rows, max_v * DCTSIZE);
        if (read == 0)
            break;
        for (int c = 0; c < n; c++) {
            for (int r = 0; r < counts[c] && done[c] < heights[c]; r++, done[c]++)
                memcpy(planes[c] + (size_t)done[c] * widths[c], rows[c][r], widths[c]);
        }
    }
    int failed = 0;
    JDIMENSION image_width = cinfo.image_width, image_height = cinfo.image_height;
    for (int c = 0; c < n; c++) {
        if (done[c] < heights[c]) {
            fprintf(stderr, "%s: component %d: %u of %u rows\n", argv[2], c, done[c], heights[c]);
            failed = 1;
        }
    }
    jpeg_finish_decompress(&cinfo);
    if (jerr.num_warnings > 0) {
        fprintf(stderr, "%s: %ld warnings from libjpeg\n", argv[2], jerr.num_warnings);
        failed = 1;
    }
    jpeg_destroy_decompress(&cinfo);
    fclose(in);

    if (!failed) {
        FILE *out = fopen(argv[3], "wb");
        if (!out) {
            perror(argv[3]);
            failed = 1;
        } else {
            for (int c = 0; c < n; c++)
                fwrite(planes[c], 1, (size_t)widths[c] * heights[c], out);
            if (ferror(out) | (fclose(out) != 0)) {
                perror(argv[3]);
                failed = 1;
            }
        }
    }
    if (failed)
        remove(argv[3]);
    fprintf(stderr, "%ux%u, %d components:", image_width, image_height, n);
    for (int c = 0; c < n; c++)
        fprintf(stderr, " %ux%u", widths[c], heights[c]);
    fprintf(stderr, "\n");
    for (int c = 0; c < n; c++) {
        for (int r = 0; r < counts[c]; r++)
            free(rows[c][r]);
        free(rows[c]);
        free(planes[c]);
    }
    return failed;
}
