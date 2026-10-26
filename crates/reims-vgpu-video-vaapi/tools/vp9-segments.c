// vp9-segments: VP9 test streams whose segments use the reference and skip
// features, which libvpx sets only from an ROI map (VP9E_SET_ROI_MAP,
// vp8cx.h) that no vpxenc option reaches. A test oracle for
// tools/vp9-check.sh, which builds it against the host's libvpx: never linked
// into anything, and nothing it writes is committed. It fails where no frame
// comes out.
//
//   vp9-segments MODE IN.y4m OUT.ivf
//
// IN is a 4:2:0 Y4M of 8 bits (Profile 0 out) or 10 (C420p10, Profile 2 out),
// as ffmpeg writes it. Each frame gets an ROI map of 8x8 blocks in diagonal
// bands 32 pixels wide that move a block a frame, so the segment map changes
// every frame; MODE picks what each band's segment does:
// - ref: no feature; the reference held to LAST; to INTRA; to GOLDEN;
// - skip: no feature; skip; skip with LAST; a quantizer delta;
// - mixed: eight segments, each reference alone and with a quantizer or
//   loop filter delta;
// - mixed-skip: eight segments, skip alone and with LAST, quantizer and
//   loop filter deltas both ways, and LAST alone.
// The tables are those libvpx applied to every inter frame (libvpx 1.17.0,
// 2026-10-24): it applied none to a key frame, and some tables (one
// skipped segment with no reference held anywhere, say) to the frames
// between the second golden refresh and the next key frame alone. A skipped segment
// held to INTRA or to GOLDEN made streams that libvpx's own decoder and
// ffmpeg's refuse ("Failed to decode tile data" from frame 2 or 3, vpxdec,
// 1280x720), so no mode asks for one. Frames are encoded
// at VPX_DL_REALTIME, CBR, with no lag, no frame-parallel decoding mode (so
// the decoder adapts probabilities backward), and a key frame every 30.
// What came out is for vp9-info to say, not this tool.
#define PROGRAM "vp9-segments"
#include "stream-io.h"

#include <vpx/vp8cx.h>
#include <vpx/vpx_encoder.h>

enum { NONE = -1, INTRA = 0, LAST = 1, GOLDEN = 2 };

struct segment {
	int delta_q, delta_lf, skip, ref_frame;
};

struct mode {
	const char *name;
	int count;
	struct segment segments[8];
};

static const struct mode modes[] = {
	{"ref", 4, {{0, 0, 0, NONE}, {0, 0, 0, LAST}, {0, 0, 0, INTRA}, {0, 0, 0, GOLDEN}}},
	{"skip", 4, {{0, 0, 0, NONE}, {0, 0, 1, NONE}, {0, 0, 1, LAST}, {20, 0, 0, NONE}}},
	{"mixed",
	 8,
	 {{0, 0, 0, NONE},
	  {0, 0, 0, LAST},
	  {0, 0, 0, INTRA},
	  {0, 0, 0, GOLDEN},
	  {-15, 0, 0, LAST},
	  {0, 10, 0, INTRA},
	  {10, -8, 0, NONE},
	  {0, -8, 0, GOLDEN}}},
	{"mixed-skip",
	 8,
	 {{0, 0, 0, NONE},
	  {0, 0, 1, NONE},
	  {0, 0, 1, LAST},
	  {20, 0, 0, NONE},
	  {-15, 0, 0, NONE},
	  {0, 10, 0, NONE},
	  {0, -8, 0, NONE},
	  {0, 0, 0, LAST}}},
};

static void codec_die(vpx_codec_ctx_t *codec, const char *what) {
	const char *detail = vpx_codec_error_detail(codec);
	fprintf(stderr, "vp9-segments: %s: %s%s%s\n", what, vpx_codec_error(codec), detail ? ": " : "",
		detail ? detail : "");
	exit(1);
}

// Writes the encoder's frames out, and counts them.
static void drain(vpx_codec_ctx_t *codec, FILE *out, uint32_t *written) {
	vpx_codec_iter_t iter = NULL;
	const vpx_codec_cx_pkt_t *pkt;
	while ((pkt = vpx_codec_get_cx_data(codec, &iter))) {
		if (pkt->kind != VPX_CODEC_CX_FRAME_PKT) continue;
		ivf_frame(out, pkt->data.frame.buf, pkt->data.frame.sz, (uint64_t)pkt->data.frame.pts);
		(*written)++;
	}
}

int main(int argc, char **argv) {
	if (argc != 4) die("usage: vp9-segments ref|skip|mixed|mixed-skip IN.y4m OUT.ivf", NULL);
	const struct mode *mode = NULL;
	for (size_t i = 0; i < sizeof modes / sizeof modes[0]; i++)
		if (!strcmp(argv[1], modes[i].name)) mode = &modes[i];
	if (!mode) die("no such mode", argv[1]);

	struct y4m y4m = y4m_open(argv[2], 10);
	unsigned width = y4m.width, height = y4m.height, rate = y4m.rate, scale = y4m.scale, depth = y4m.depth;

	vpx_codec_iface_t *iface = vpx_codec_vp9_cx();
	vpx_codec_enc_cfg_t cfg;
	if (vpx_codec_enc_config_default(iface, &cfg, 0)) die("no default configuration", NULL);
	cfg.g_w = width;
	cfg.g_h = height;
	cfg.g_timebase.num = (int)scale;
	cfg.g_timebase.den = (int)rate;
	cfg.g_profile = depth > 8 ? 2 : 0;
	cfg.g_bit_depth = depth > 8 ? VPX_BITS_10 : VPX_BITS_8;
	cfg.g_input_bit_depth = depth;
	cfg.g_threads = 4;
	cfg.g_lag_in_frames = 0;
	cfg.g_error_resilient = 0;
	cfg.rc_end_usage = VPX_CBR;
	cfg.rc_target_bitrate = (unsigned)((uint64_t)width * height * 8 / 1000);
	cfg.rc_dropframe_thresh = 0;
	cfg.kf_mode = VPX_KF_AUTO;
	cfg.kf_min_dist = 30;
	cfg.kf_max_dist = 30;
	vpx_codec_ctx_t codec;
	if (vpx_codec_enc_init(&codec, iface, &cfg, depth > 8 ? VPX_CODEC_USE_HIGHBITDEPTH : 0))
		codec_die(&codec, "vpx_codec_enc_init");
	if (vpx_codec_control(&codec, VP8E_SET_CPUUSED, 7) || vpx_codec_control(&codec, VP9E_SET_AQ_MODE, 0) ||
	    vpx_codec_control(&codec, VP9E_SET_FRAME_PARALLEL_DECODING, 0))
		codec_die(&codec, "vpx_codec_control");

	vpx_image_t img;
	if (!vpx_img_alloc(&img, depth > 8 ? VPX_IMG_FMT_I42016 : VPX_IMG_FMT_I420, width, height, 16))
		die("vpx_img_alloc", NULL);
	img.bit_depth = depth;

	unsigned rows = (height + 7) / 8, cols = (width + 7) / 8;
	unsigned char *map = malloc((size_t)rows * cols);
	if (!map) die("out of memory", NULL);
	vpx_roi_map_t roi;
	memset(&roi, 0, sizeof roi);
	roi.enabled = 1;
	roi.roi_map = map;
	roi.rows = rows;
	roi.cols = cols;

	FILE *out = fopen(argv[3], "wb");
	if (!out) die("cannot write", argv[3]);
	ivf_header(out, "VP90", width, height, rate, scale, 0);
	uint32_t frames = 0, written = 0;
	while (y4m_frame(&y4m, img.planes, img.stride)) {
		for (unsigned r = 0; r < rows; r++)
			for (unsigned c = 0; c < cols; c++)
				map[r * cols + c] = (unsigned char)((r / 4 + c / 4 + frames) % (unsigned)mode->count);
		for (int s = 0; s < 8; s++) {
			const struct segment *seg = s < mode->count ? &mode->segments[s] : &mode->segments[0];
			roi.delta_q[s] = seg->delta_q;
			roi.delta_lf[s] = seg->delta_lf;
			roi.skip[s] = seg->skip;
			roi.ref_frame[s] = seg->ref_frame;
		}
		if (vpx_codec_control(&codec, VP9E_SET_ROI_MAP, &roi)) codec_die(&codec, "VP9E_SET_ROI_MAP");
		if (vpx_codec_encode(&codec, &img, frames, 1, 0, VPX_DL_REALTIME)) codec_die(&codec, "vpx_codec_encode");
		drain(&codec, out, &written);
		frames++;
	}
	if (vpx_codec_encode(&codec, NULL, -1, 1, 0, VPX_DL_REALTIME)) codec_die(&codec, "flushing");
	drain(&codec, out, &written);
	if (!written) die("no frame out", argv[2]);
	ivf_header(out, "VP90", width, height, rate, scale, written);
	if (fclose(out)) die("closing", argv[3]);
	fclose(y4m.in);
	fprintf(stderr, "vp9-segments: %s, %ux%u at %u bits, %u frames in, %u out\n", mode->name, width, height,
		depth, frames, written);
	vpx_img_free(&img);
	free(map);
	return vpx_codec_destroy(&codec) ? 1 : 0;
}
