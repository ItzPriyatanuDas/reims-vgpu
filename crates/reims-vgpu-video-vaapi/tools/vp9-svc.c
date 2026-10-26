// vp9-svc: VP9 streams of libvpx's spatial scalability, whose frames of
// each layer are their own size and name slots that hold other layers'
// frames, as a WebRTC sender makes them. No vpxenc option reaches it
// (VP9E_SET_SVC, vp8cx.h). A test oracle for references of other layers'
// sizes, built against the host's libvpx and run on its CPU by hand
// (vaapi.md and nvidia.md say how): never linked into anything, and nothing
// it writes is committed. What came out is for vp9-info to say.
//
//   vp9-svc LAYERS TEMPORAL INTER-LAYER RESILIENT IN.y4m OUT.ivf
//
// LAYERS is 2 or 3 spatial layers, each half the next's size on a side;
// TEMPORAL 1 or 3 temporal layers (the 0212 pattern); INTER-LAYER is
// VP9E_SET_SVC_INTER_LAYER_PRED's mode: 0 on, 1 off, 2 on key frames alone
// (as K-SVC). RESILIENT is g_error_resilient. IN is a 4:2:0 Y4M of 8 bits.
// Every superframe, all its layers, is one IVF frame. Frames are encoded at
// VPX_DL_REALTIME, CBR, with no lag and no key frame after the first. It
// fails where no superframe comes out.
//
//   cc -O2 -Wall -Wextra -o vp9-svc vp9-svc.c -lvpx
#define PROGRAM "vp9-svc"
#include "stream-io.h"

#include <vpx/vp8cx.h>
#include <vpx/vpx_encoder.h>

static void codec_die(vpx_codec_ctx_t *codec, const char *what) {
	const char *detail = vpx_codec_error_detail(codec);
	fprintf(stderr, "vp9-svc: %s: %s%s%s\n", what, vpx_codec_error(codec), detail ? ": " : "",
		detail ? detail : "");
	exit(1);
}

// Writes the encoder's superframes out, and counts them.
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
	if (argc != 7) die("usage: vp9-svc 2|3 1|3 0|1|2 0|1 IN.y4m OUT.ivf", NULL);
	int spatial = atoi(argv[1]), temporal = atoi(argv[2]), inter_layer = atoi(argv[3]);
	int resilient = atoi(argv[4]);
	if ((spatial != 2 && spatial != 3) || (temporal != 1 && temporal != 3) || inter_layer < 0 ||
	    inter_layer > 2)
		die("layers are 2 or 3, temporal 1 or 3, inter-layer 0 to 2", NULL);

	struct y4m y4m = y4m_open(argv[5], 8);
	unsigned width = y4m.width, height = y4m.height, rate = y4m.rate, scale = y4m.scale;

	vpx_codec_iface_t *iface = vpx_codec_vp9_cx();
	vpx_codec_enc_cfg_t cfg;
	if (vpx_codec_enc_config_default(iface, &cfg, 0)) die("no default configuration", NULL);
	cfg.g_w = width;
	cfg.g_h = height;
	cfg.g_timebase.num = (int)scale;
	cfg.g_timebase.den = (int)rate;
	cfg.g_threads = 4;
	cfg.g_lag_in_frames = 0;
	cfg.g_error_resilient = resilient;
	cfg.rc_end_usage = VPX_CBR;
	cfg.rc_dropframe_thresh = 0;
	cfg.rc_min_quantizer = 2;
	cfg.rc_max_quantizer = 56;
	cfg.kf_mode = VPX_KF_DISABLED;
	cfg.ss_number_layers = (unsigned)spatial;
	cfg.ts_number_layers = (unsigned)temporal;
	cfg.temporal_layering_mode =
		temporal == 3 ? VP9E_TEMPORAL_LAYERING_MODE_0212 : VP9E_TEMPORAL_LAYERING_MODE_NOLAYERING;
	if (temporal == 3) {
		cfg.ts_rate_decimator[0] = 4;
		cfg.ts_rate_decimator[1] = 2;
		cfg.ts_rate_decimator[2] = 1;
	} else {
		cfg.ts_rate_decimator[0] = 1;
	}
	// Each spatial layer's rate by its area, a tenth of a bit a pixel at
	// the top; within one, the temporal layers' rates are cumulative.
	unsigned total = 0;
	for (int s = 0; s < spatial; s++) {
		unsigned shift = (unsigned)(spatial - 1 - s);
		unsigned layer = (unsigned)((uint64_t)(width >> shift) * (height >> shift) * rate / scale / 10000);
		if (layer < 20) layer = 20;
		for (int t = 0; t < temporal; t++)
			cfg.layer_target_bitrate[s * temporal + t] =
				temporal == 3 ? layer * (t == 0 ? 50 : t == 1 ? 75 : 100) / 100 : layer;
		total += layer;
	}
	cfg.rc_target_bitrate = total;

	vpx_codec_ctx_t codec;
	if (vpx_codec_enc_init(&codec, iface, &cfg, 0)) codec_die(&codec, "vpx_codec_enc_init");
	vpx_svc_extra_cfg_t svc;
	memset(&svc, 0, sizeof svc);
	for (int s = 0; s < spatial; s++) {
		svc.scaling_factor_num[s] = 1;
		svc.scaling_factor_den[s] = 1 << (spatial - 1 - s);
		svc.max_quantizers[s] = 56;
		svc.min_quantizers[s] = 2;
	}
	if (vpx_codec_control(&codec, VP8E_SET_CPUUSED, 7) || vpx_codec_control(&codec, VP9E_SET_SVC, 1) ||
	    vpx_codec_control(&codec, VP9E_SET_SVC_PARAMETERS, &svc) ||
	    vpx_codec_control(&codec, VP9E_SET_SVC_INTER_LAYER_PRED, inter_layer) ||
	    vpx_codec_control(&codec, VP9E_SET_AQ_MODE, 3))
		codec_die(&codec, "vpx_codec_control");

	vpx_image_t img;
	if (!vpx_img_alloc(&img, VPX_IMG_FMT_I420, width, height, 16)) die("vpx_img_alloc", NULL);

	FILE *out = fopen(argv[6], "wb");
	if (!out) die("cannot write", argv[6]);
	ivf_header(out, "VP90", width, height, rate, scale, 0);
	uint32_t frames = 0, written = 0;
	while (y4m_frame(&y4m, img.planes, img.stride)) {
		if (vpx_codec_encode(&codec, &img, frames, 1, 0, VPX_DL_REALTIME)) codec_die(&codec, "vpx_codec_encode");
		drain(&codec, out, &written);
		frames++;
	}
	if (vpx_codec_encode(&codec, NULL, -1, 1, 0, VPX_DL_REALTIME)) codec_die(&codec, "flushing");
	drain(&codec, out, &written);
	if (!written) die("no superframe out", argv[5]);
	ivf_header(out, "VP90", width, height, rate, scale, written);
	if (fclose(out)) die("closing", argv[6]);
	fclose(y4m.in);
	fprintf(stderr, "vp9-svc: %d spatial, %d temporal, inter-layer %d, resilient %d, %ux%u, %u frames in, %u out\n",
		spatial, temporal, inter_layer, resilient, width, height, frames, written);
	vpx_img_free(&img);
	return vpx_codec_destroy(&codec) ? 1 : 0;
}
