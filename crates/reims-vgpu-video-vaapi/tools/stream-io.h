// What the stream helpers, av1-svc.c, vp9-segments.c and vp9-svc.c, share:
// their errors, their Y4M input and their IVF output. Each defines PROGRAM,
// its name in its messages, before it includes this file.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void die(const char *what, const char *detail) {
	fprintf(stderr, PROGRAM ": %s%s%s\n", what, detail ? ": " : "", detail ? detail : "");
	exit(1);
}

static void put16(uint8_t *p, unsigned v) {
	p[0] = v & 0xff;
	p[1] = (v >> 8) & 0xff;
}

static void put32(uint8_t *p, uint32_t v) {
	for (int i = 0; i < 4; i++) p[i] = (v >> (8 * i)) & 0xff;
}

// The IVF file header (32 bytes), at the file's start, with its frame count:
// written first with none, and again once every frame is.
static void ivf_header(FILE *out, const char fourcc[4], unsigned width, unsigned height, unsigned rate,
		       unsigned scale, uint32_t frames) {
	uint8_t h[32] = {'D', 'K', 'I', 'F'};
	put16(h + 4, 0);
	put16(h + 6, 32);
	memcpy(h + 8, fourcc, 4);
	put16(h + 12, width);
	put16(h + 14, height);
	put32(h + 16, rate);
	put32(h + 20, scale);
	put32(h + 24, frames);
	if (fseek(out, 0, SEEK_SET) || fwrite(h, 1, sizeof h, out) != sizeof h) die("writing the IVF header", NULL);
}

// One IVF frame: its 12-byte header, then its bytes.
static void ivf_frame(FILE *out, const void *bytes, size_t len, uint64_t pts) {
	uint8_t h[12];
	put32(h, (uint32_t)len);
	put32(h + 4, (uint32_t)pts);
	put32(h + 8, (uint32_t)(pts >> 32));
	if (fwrite(h, 1, sizeof h, out) != sizeof h || fwrite(bytes, 1, len, out) != len) die("writing a frame", NULL);
}

// A 4:2:0 Y4M stream's header, as ffmpeg writes it.
struct y4m {
	FILE *in;
	unsigned width, height, rate, scale, depth;
};

// Opens PATH and reads its header: 4:2:0 at 8 bits, or at 10 (C420p10)
// where MAX_DEPTH allows it.
static struct y4m y4m_open(const char *path, unsigned max_depth) {
	struct y4m y = {fopen(path, "rb"), 0, 0, 30, 1, 8};
	if (!y.in) die("cannot open", path);
	char line[512];
	if (!fgets(line, sizeof line, y.in) || strncmp(line, "YUV4MPEG2 ", 10)) die("not a Y4M file", path);
	for (char *tok = strtok(line + 10, " \n"); tok; tok = strtok(NULL, " \n")) {
		if (tok[0] == 'W') y.width = (unsigned)strtoul(tok + 1, NULL, 10);
		else if (tok[0] == 'H') y.height = (unsigned)strtoul(tok + 1, NULL, 10);
		else if (tok[0] == 'F') {
			if (sscanf(tok + 1, "%u:%u", &y.rate, &y.scale) != 2) die("a frame rate that does not parse", tok);
		} else if (tok[0] == 'C') {
			if (!strcmp(tok, "C420p10") && max_depth >= 10) y.depth = 10;
			else if (strcmp(tok, "C420") && strcmp(tok, "C420jpeg") && strcmp(tok, "C420paldv") &&
				 strcmp(tok, "C420mpeg2"))
				die(max_depth >= 10 ? "not 4:2:0 at 8 or 10 bits" : "not 4:2:0 at 8 bits", tok);
		}
	}
	if (!y.width || !y.height || !y.rate || !y.scale) die("no frame size or rate in the Y4M header", path);
	return y;
}

// The next frame's planes into PLANES, rows STRIDE bytes apart: 1, or 0 at
// the end of the stream. A frame cut short ends the process.
static int y4m_frame(struct y4m *y, unsigned char *const planes[3], const int stride[3]) {
	char line[512];
	if (!fgets(line, sizeof line, y->in)) return 0;
	if (strncmp(line, "FRAME", 5)) die("not a Y4M frame", NULL);
	size_t sample = y->depth > 8 ? 2 : 1;
	for (int plane = 0; plane < 3; plane++) {
		unsigned w = plane ? (y->width + 1) / 2 : y->width, h = plane ? (y->height + 1) / 2 : y->height;
		for (unsigned row = 0; row < h; row++)
			if (fread(planes[plane] + (size_t)row * stride[plane], sample, w, y->in) != w)
				die("a Y4M frame cut short", NULL);
	}
	return 1;
}
