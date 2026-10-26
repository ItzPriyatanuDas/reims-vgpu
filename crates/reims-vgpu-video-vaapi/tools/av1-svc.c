// av1-svc: AV1 streams of libaom's scalable coding, with several operating
// points: temporal layers, spatial layers or both, as a WebRTC sender makes
// them. No aomenc option reaches it (AV1E_SET_SVC_PARAMS, aomcx.h). A test
// oracle for streams of several operating points, which av1-check.sh builds
// against the host's libaom and runs on its CPU: never linked into anything,
// and nothing it writes is committed. What came out is for av1-info to say.
//
//   av1-svc SPATIAL TEMPORAL SCALE IN.y4m OUT.ivf
//
// SPATIAL is 1 to 3 spatial layers; TEMPORAL 1 to 3 temporal layers (for 3,
// the 0212 pattern; for 2, 0101). SCALE is 2 for each spatial layer half
// the next one's size on a side, or 1 for every layer the input's size
// (quality layers). IN is a 4:2:0 Y4M of 8 bits. Every temporal unit, all
// its spatial layers, is one IVF frame. Frames are encoded with
// AOM_USAGE_REALTIME, CBR, no lag, no frames dropped and no key frame
// after the first. The reference structure is the one libaom's
// svc_encoder_rtc example gives these layer counts: each temporal layer
// predicts from the layers below it, and each spatial layer from the layer
// below in its temporal unit (GOLDEN) and from its own last frame (LAST).
// It fails where no temporal unit comes out.
#define PROGRAM "av1-svc"
#include "stream-io.h"

#include <aom/aom_encoder.h>
#include <aom/aomcx.h>

static void codec_die(aom_codec_ctx_t *codec, const char *what) {
	const char *detail = aom_codec_error_detail(codec);
	fprintf(stderr, "av1-svc: %s: %s%s%s\n", what, aom_codec_error(codec), detail ? ": " : "",
		detail ? detail : "");
	exit(1);
}

// One temporal unit's bytes, all its spatial layers' packets.
struct unit {
	uint8_t *bytes;
	size_t len, cap;
};

static void take(aom_codec_ctx_t *codec, struct unit *unit) {
	aom_codec_iter_t iter = NULL;
	const aom_codec_cx_pkt_t *pkt;
	while ((pkt = aom_codec_get_cx_data(codec, &iter))) {
		if (pkt->kind != AOM_CODEC_CX_FRAME_PKT) continue;
		size_t need = unit->len + pkt->data.frame.sz;
		if (need > unit->cap) {
			uint8_t *bytes = realloc(unit->bytes, need * 2);
			if (!bytes) die("out of memory", NULL);
			unit->bytes = bytes;
			unit->cap = need * 2;
		}
		memcpy(unit->bytes + unit->len, pkt->data.frame.buf, pkt->data.frame.sz);
		unit->len += pkt->data.frame.sz;
	}
}

// The temporal layer of temporal unit `n`.
static int temporal_layer(int temporal, unsigned n) {
	if (temporal == 3) return n % 4 == 0 ? 0 : n % 2 ? 2 : 1;
	if (temporal == 2) return n % 2;
	return 0;
}

// The references and refreshes of spatial layer `sl` in temporal unit `n`.
// Slots: spatial layer s's base temporal layer keeps its last frame in slot
// s, its middle temporal layer in slot 3 + s; LAST, LAST2 and GOLDEN are
// references 0, 1 and 3.
static void references(aom_svc_ref_frame_config_t *c, int temporal, int sl, unsigned n) {
	memset(c, 0, sizeof *c);
	int tl = temporal_layer(temporal, n);
	for (int i = 0; i < 7; i++) c->ref_idx[i] = sl;
	c->reference[0] = 1;
	if (tl == 0) {
		c->refresh[sl] = 1;
	} else if (tl == 1 && temporal == 3) {
		// The middle layer: LAST is the base, and it keeps its own frame
		// in LAST2's slot.
		c->ref_idx[1] = 3 + sl;
		c->refresh[3 + sl] = 1;
	} else if (temporal == 3 && n % 4 == 3) {
		// The second top-layer frame of four predicts from the middle
		// layer's.
		c->ref_idx[0] = 3 + sl;
	}
	if (sl > 0) {
		// GOLDEN: the spatial layer below, in this temporal unit.
		c->ref_idx[3] = sl - 1;
		c->reference[3] = 1;
	}
}

int main(int argc, char **argv) {
	if (argc != 6) die("usage: av1-svc 1|2|3 1|2|3 1|2 IN.y4m OUT.ivf", NULL);
	int spatial = atoi(argv[1]), temporal = atoi(argv[2]), scale_step = atoi(argv[3]);
	if (spatial < 1 || spatial > 3 || temporal < 1 || temporal > 3 || (scale_step != 1 && scale_step != 2))
		die("layers are 1 to 3, temporal 1 to 3, scale 1 or 2", NULL);

	struct y4m y4m = y4m_open(argv[4], 8);
	unsigned width = y4m.width, height = y4m.height, rate = y4m.rate, scale = y4m.scale;

	aom_codec_iface_t *iface = aom_codec_av1_cx();
	aom_codec_enc_cfg_t cfg;
	if (aom_codec_enc_config_default(iface, &cfg, AOM_USAGE_REALTIME)) die("no default configuration", NULL);
	cfg.g_w = width;
	cfg.g_h = height;
	cfg.g_timebase.num = (int)scale;
	cfg.g_timebase.den = (int)rate;
	cfg.g_threads = 4;
	cfg.g_lag_in_frames = 0;
	cfg.g_error_resilient = 0;
	cfg.rc_end_usage = AOM_CBR;
	cfg.rc_dropframe_thresh = 0;
	cfg.rc_min_quantizer = 2;
	cfg.rc_max_quantizer = 56;
	cfg.kf_mode = AOM_KF_DISABLED;

	// Each spatial layer's rate by its area, a tenth of a bit a pixel at
	// the top; within one, the temporal layers' rates are cumulative.
	aom_svc_params_t svc;
	memset(&svc, 0, sizeof svc);
	svc.number_spatial_layers = spatial;
	svc.number_temporal_layers = temporal;
	unsigned total = 0;
	for (int s = 0; s < spatial; s++) {
		int den = scale_step == 2 ? 1 << (spatial - 1 - s) : 1;
		svc.scaling_factor_num[s] = 1;
		svc.scaling_factor_den[s] = den;
		unsigned layer = (unsigned)((uint64_t)(width / den) * (height / den) * rate / scale / 10000);
		if (layer < 20) layer = 20;
		for (int t = 0; t < temporal; t++) {
			int i = s * temporal + t;
			svc.max_quantizers[i] = 56;
			svc.min_quantizers[i] = 2;
			svc.layer_target_bitrate[i] =
				(int)(t == temporal - 1 ? layer : layer * (t == 0 ? (temporal == 3 ? 50 : 60) : 75) / 100);
		}
		total += layer;
	}
	for (int t = 0; t < temporal; t++) svc.framerate_factor[t] = 1 << (temporal - 1 - t);
	cfg.rc_target_bitrate = total;

	aom_codec_ctx_t codec;
	if (aom_codec_enc_init(&codec, iface, &cfg, 0)) codec_die(&codec, "aom_codec_enc_init");
	if (aom_codec_control(&codec, AOME_SET_CPUUSED, 8) || aom_codec_control(&codec, AV1E_SET_SVC_PARAMS, &svc) ||
	    aom_codec_control(&codec, AV1E_SET_AQ_MODE, 3) || aom_codec_control(&codec, AV1E_SET_ROW_MT, 1))
		codec_die(&codec, "aom_codec_control");

	aom_image_t img;
	if (!aom_img_alloc(&img, AOM_IMG_FMT_I420, width, height, 32)) die("aom_img_alloc", NULL);

	FILE *out = fopen(argv[5], "wb");
	if (!out) die("cannot write", argv[5]);
	ivf_header(out, "AV01", width, height, rate, scale, 0);
	struct unit unit = {0};
	uint32_t frames = 0, written = 0;
	while (y4m_frame(&y4m, img.planes, img.stride)) {
		unit.len = 0;
		for (int sl = 0; sl < spatial; sl++) {
			aom_svc_layer_id_t id = {sl, temporal_layer(temporal, frames)};
			aom_svc_ref_frame_config_t refs;
			references(&refs, temporal, sl, frames);
			if (aom_codec_control(&codec, AV1E_SET_SVC_LAYER_ID, &id) ||
			    aom_codec_control(&codec, AV1E_SET_SVC_REF_FRAME_CONFIG, &refs))
				codec_die(&codec, "aom_codec_control (layer)");
			if (aom_codec_encode(&codec, &img, frames, 1, 0)) codec_die(&codec, "aom_codec_encode");
			take(&codec, &unit);
		}
		if (!unit.len) die("a temporal unit with no packet", NULL);
		ivf_frame(out, unit.bytes, unit.len, frames);
		written++;
		frames++;
	}
	if (!written) die("no temporal unit out", argv[4]);
	ivf_header(out, "AV01", width, height, rate, scale, written);
	if (fclose(out)) die("closing", argv[5]);
	fclose(y4m.in);
	fprintf(stderr, "av1-svc: %d spatial, %d temporal, scale %d, %ux%u, %u frames in, %u temporal units out\n",
		spatial, temporal, scale_step, width, height, frames, written);
	free(unit.bytes);
	aom_img_free(&img);
	return aom_codec_destroy(&codec) ? 1 : 0;
}
