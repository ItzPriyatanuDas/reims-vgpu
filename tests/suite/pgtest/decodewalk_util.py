"""Decode walks (scenarios/interplay_decodewalk.py): seeded random command
streams for the deserializer's four decoders, blit (segment type 2),
compute (1), render (0) and info (4) (docs/testing.md, sections 6.12-6.19),
built from the record builders the per-area modules use, and checked
against a model of every guest byte the GPU writes.

The walker composes:
  - pgtest/diff_util.py (Lab, exec_payload, fault_state, resume): a task
    whose execs any FIFO carries;
  - pgtest/compute_objects.py: the blit (Blit, 300-323) and compute
    (Compute, 0x86-0xe6) records, compute pipelines over functions, fences,
    samplers, textures over buffers (type 8, commandType 9), heaps and
    indirect command buffers with icb_compute_command's layout;
  - pgtest/render_lib.py: the render pass (0x1a) and its prefix, the
    render records, the "basic" render pipeline over vert/frag.metallib;
  - pgtest/exec_objects.py: the info records (info_ref, info_heap_size),
    rate maps and depth-stencil states;
  - pgtest/servicewalk_util.py (emit_bytes): the model's bytes written
    into a guest page of their own, then compared (cmp);
  - pgtest/fifo_util.py: a child FIFO carrying a second task.

Every walk is one scenario, interplay_decodewalk_<seed>, whose program the
walker derives from the seed alone. A batch is one to four execs (67, or 55
or 69 at random), each of one to six segments, rung in one doorbell; every
exec after the first waits on the one before it with a barrier (on its own
FIFO's stamp index, or the other FIFO's when the second task's execs on
child FIFO 1 interleave with the first's), so each is decoded after the one
before it completed, and every stamp is published alone. A segment is a
random stream of VALID records for its decoder, tracking the encoder's
state (a compute pipeline and its bindings before a dispatch, a render
pipeline before a draw, a pass before the render records), across
continued segments (continuedByNext / continuesPrevious) that share one
encoder. Edges that are valid ride along: a blit decoder carried into a
segment marked with another type (it decodes blit records, section 6.13
step 4), an info segment continued by nothing, a protection envelope with
options 0 around a blit segment, a range of 1-7 bytes (skipped), trailing
bytes after the last record, a record longer than its command, a segment
whose header length cuts the records after it (never decoded), a range
longer than its segment holding a second segment that must not run, and
nullable lookups (314, 0x87 and 0xd6 on refs that name nothing).

At most one exec a batch, the last, carries one malformed record or
segment (a decode error: the exec faults at its ring offset, nothing of it
is committed, section 6.12), chosen from those section 7.4 does not list as
deaths: unknown command ids, record lengths under 8 or past the segment,
records one byte short, refs that are 0, past the object list or of the
wrong kind, out-of-range buffer and ICB ranges, binding counts past the
limits, a dispatch or draw without a pipeline, primitive type 10, a render
segment without a pass, a pass command in the main loop, 0x16, colour
indices past the attachments, encoder types 3 and 6, a continuation that
does not match, a segment header longer than its range or under 8 bytes.
The fault's register or node offset is read and checked against the ring
offset the walker wrote the exec at (the label says "@0x..").

The model: the bytes of two output buffers A and B, of ICBOUT (the
indirect command buffers' target), and of two 8x8 RGBA8Unorm textures over
buffers (TBA, TBB) that blits copy, fill and read back and render passes
clear and draw (the "basic" pipeline covers the target in ff8040ff, clipped
by viewport, scissor and culling); which of ICB2's two commands hold a copy
of ICB1's dispatch (305 copies, 313 resets); and a source buffer SRC the
guest rewrites between batches. FillTextureWithBytes (320) reads its bytes
at decode time, so the model gives it the bytes as they were before the
exec's GPU work. After each batch every
span a batch wrote is compared with the model's bytes.

Info replies (isolate walks only: in-process a reply aborts the host,
section 7.4) go to 0x20-byte slots of a reply buffer the guest fills with
0xEE first; the first reply of each kind (command and object) is dumped
(host values: the walks are keyed by host), every later one compared with
it; a reply after the malformed record in decode order is compared with
0xEE: it was never written. The all-setup walks meet the info decoder as
empty segments, an info segment continued by nothing, and info records that
throw before the reply cursor is fetched (a bad object ref, an unknown id).

Endings (last, a death-last seed): the last exec opens an encoder it never
ends (a blit segment continued by nothing), opens a second compute encoder
over a carried one, dispatches indirect from buffer ref 0, begins a render
pass with a size and no attachment, names a nil ref in a concurrent
encoder's 0x87, dispatches indirect in a run of compute encoders of mixed
dispatch types, or asks an info reply (in-process a host death; the
service writes it). The wait the host dies in is labelled [dies]. Every
ending but the info reply is a death inside Metal, which ends the host
under isolation too, in the same wait. So the steps the walker writes
after it, for a task whose own code died (the task's next exec, which
faults, then the task deleted, defined again with a new service, whose
indirect command buffers and fences start empty, and run), are never
reached.

python3 -m pgtest.decodewalk_util checks the expected files: every cmp
equal, every wait ok but the [dies] ones and the "completed (0) or faulted
(1)" ones, every fault offset the one its label names.

Record layouts follow reims-vgpu-core's deserializer::{blit, compute,
render, info}. Nothing here is imported from the library's code.
"""
import json
import os
import struct

from . import abi
from . import render_lib as R
from .abi import PAGE
from .compute_objects import (Blit as B, Compute as C, compute_blob, compute_pipeline_record, fence_record,
                              heap_record, icb_compute_command, icb_record, sampler_record, texture_descriptor,
                              texture_on_buffer_record)
from .diff_util import K_ADD, K_DOUBLE, K_INC, Lab, exec_payload
from .exec_objects import depth_stencil_record, info_heap_size, info_ref, rate_map_record
from .fifo_util import HEALTHY, NODE_FAULT, child
from .randgen import Rng
from .scenario import SETUPS, Scenario
from .servicewalk_util import emit_bytes
from .task import rec, seg

BLIT, COMPUTE, RENDER, INFO, PROT = abi.ENC_BLIT, abi.ENC_COMPUTE, abi.ENC_RENDER, abi.ENC_INFO, abi.ENC_PROTECTION
E3, E2, E9 = abi.OP_EXEC_INDIRECT3, abi.OP_EXEC_INDIRECT2, abi.OP_INFO_INDIRECT
ISO = ("isolate",)
DIES = " [dies]"
EITHER = "completed (0) or faulted (1)"
EE = 0xEE
WAIT_MS = 30000
LAST_SLOT = 2 * PAGE - 0x20
IN456 = struct.pack("<4f", 16.0, 8.0, 40.0, 20.0)

# The spans the walker writes: A and B below WIN, ICBOUT below 0x200.
WIN = 0x900
SPAN = 0x800          # offsets into A and B stay below this
TEXW = TEXH = 8       # the textures: 8x8 RGBA8Unorm, bytesPerRow 32
BPR = 32
TEXLEN = BPR * TEXH
SRC_LEN = 0x1000
ICB_THREADS = 16      # ICB1's one command: one threadgroup of 16 double_gid threads
DRAWN = bytes([0xFF, 0x80, 0x40, 0xFF])   # frag.metallib's float4(1, .5, .25, 1)

# Colours whose RGBA8Unorm bytes are exact: 0, 0.25, 0.5, 1 give 00, 40,
# 80, ff (blit_fill_texture: 0.5 -> 0x80, 0.25 -> 0x40).
LEVELS = (0.0, 0.25, 0.5, 1.0)
LEVEL_BYTE = {0.0: 0x00, 0.25: 0x40, 0.5: 0x80, 1.0: 0xFF}

# The triangle vert.metallib draws, (-1,-1) (3,-1) (-1,3), is
# counter-clockwise in normalized device coordinates. With the default
# winding (0x73 0, clockwise front) it is back-facing
# (render_state_raster locks 0x6b and 0x73 over both windings).
TRI_FRONT_WHEN_WINDING = 1


def u32s(*v):
    return struct.pack("<%dI" % len(v), *v)


def colour_bytes(c):
    return bytes(LEVEL_BYTE[v] for v in c)


# ---- one task: its objects and its model ----

class DTask(Lab):
    """One walker task (Lab's layout: object list at VA 0, records at pages
    1-3, command buffers at pages 4-7, data from page 8), holding one of
    every object the walker's records name, each over its own pages:

      A, B      shared buffers (type 1), the output spans;
      SRC       a shared buffer the guest writes (u32 arithmetic runs);
      ARGS      the indirect dispatch arguments (0xc9, 0xe6);
      TBA, TBB  buffers under TA and TB, 8x8 RGBA8Unorm textures (type 8,
                commandType 9, bytesPerRow 32), usage read, write, render
                target;
      ICBOUT    the buffer ICB1's command writes;
      REPLY     (isolate walks) two pages of info reply slots;
      DBL, ADD, INC  compute pipelines over double_gid, add_one, inc_k;
      DBLI      double_gid with supportIndirectCommandBuffers;
      ICB1      an ICB of one concurrentDispatch command: DBLI over
                ICBOUT, one threadgroup of 16;
      ICB2      an ICB of two empty commands (305 copies into it);
      F1, F2    fences; SMP a sampler; RP the basic render pipeline;
      HEAP      a private heap of one page;
      RM, DSS   (isolate walks) a rate map and a depth-stencil state.

    Every exec lists every buffer, texture buffer, ICB and the heap with
    flags 0 (docs/testing.md 6.12: the service needs every guest buffer
    the GPU touches listed)."""

    def __init__(self, s, tid, iso):
        super().__init__(s, tid=tid, pages=40)
        self.iso = iso
        self.A, self.A_va = self.shared()
        self.B, self.B_va = self.shared()
        self.SRC, self.SRC_va = self.shared()
        self.ARGS, self.ARGS_va = self.shared()
        self.TBA, self.TBA_va = self.shared()
        self.TBB, self.TBB_va = self.shared()
        self.ICBOUT, self.ICBOUT_va = self.shared()
        if iso:
            self.REPLY, self.REPLY_va = self.shared(length=2 * PAGE)
        desc = texture_descriptor(TEXW, TEXH)
        self.TA = self.add(abi.T_SERIALIZER_TEXTURE, texture_on_buffer_record(self.TBA, 0, BPR, desc))
        self.TB = self.add(abi.T_SERIALIZER_TEXTURE, texture_on_buffer_record(self.TBB, 0, BPR, desc))
        self.fn_dbl = self.function(K_DOUBLE)
        self.DBL = self.add(abi.T_SERIALIZED, compute_pipeline_record(compute_blob(self.fn_dbl)))
        self.ADD = self.kernel(K_ADD)
        self.INC = self.kernel(K_INC)
        self.DBLI = self.add(abi.T_SERIALIZED, compute_pipeline_record(compute_blob(self.fn_dbl, icb=True)))
        va = self.reserve(PAGE)
        self.write(va, icb_compute_command(pipeline=self.DBLI, buffers=[(self.ICBOUT, 0)], grid=(1, 1, 1),
                                           tptg=(ICB_THREADS, 1, 1)))
        self.ICB1 = self.add(abi.T_ICB, icb_record(va // PAGE, 0x100, 1))
        va = self.reserve(PAGE)
        self.write(va, icb_compute_command(ctype=0) * 2)
        self.ICB2 = self.add(abi.T_ICB, icb_record(va // PAGE, 0x200, 2))
        self.F1 = self.add(abi.T_SERIALIZED, fence_record())
        self.F2 = self.add(abi.T_SERIALIZED, fence_record())
        self.SMP = self.add(abi.T_SERIALIZED, sampler_record())
        v = self.function("shaders/vert.metallib")
        f = self.function("shaders/frag.metallib")
        b = R.pipeline_blob("basic", v, f)
        self.RP = self.add(abi.T_SERIALIZED, struct.pack("<IIII", 14, 0, 0, len(b)) + b)
        hva = self.reserve(PAGE)
        self.HEAP = self.add(abi.T_HEAP, heap_record(hva // PAGE, PAGE, 1))
        if iso:
            self.RM = self.add(abi.T_SERIALIZED, rate_map_record(w=256, h=256, samples=[([0.25, 1.0], [0.25, 1.0])]))
            self.DSS = self.add(abi.T_SERIALIZED, depth_stencil_record())
        self.listed += [self.ICB1, self.ICB2, self.HEAP]
        self.past = len(self.ol.entries) + 7
        # The guest's bytes: SRC u32 runs, ARGS the indirect arguments.
        self.src_base, self.src_step = 0x01000000 * tid + 0x10, 0x01010103
        self.m = Model(self)
        self.write_src(self.src_base, self.src_step)
        self.args = {}
        for off, v in ((0x00, (2, 1, 1)), (0x10, (4, 1, 1)), (0x20, (1, 1, 1)), (0x30, (3, 1, 1))):
            self.args[off] = v
            self.write(self.ARGS_va + off, u32s(*v))
        for off, v in ((0x40, (16, 1, 1, 16, 1, 1)), (0x60, (40, 1, 1, 8, 1, 1)), (0x80, (64, 1, 1, 64, 1, 1)),
                       (0xa0, (9, 1, 1, 4, 1, 1))):
            self.args[off] = v
            self.write(self.ARGS_va + off, u32s(*v))
        self.ol.write()
        self.fifo = None
        self.dead = False
        self.reply_next = 0
        self.first_reply = {}       # kind -> guest address of its first reply
        self.anchors = set()        # reply slots that hold a first reply

    def write_src(self, base, step):
        """The guest rewrites SRC: u32 k = base + k * step."""
        self.src_base, self.src_step = base, step
        self.s.mem_seq(self.gpa(self.SRC_va), SRC_LEN // 4, 4, bytes(4), fields=[(0, 4, base, step)])
        self.m.buf["SRC"][:] = b"".join(struct.pack("<I", (base + k * step) & 0xFFFFFFFF)
                                        for k in range(SRC_LEN // 4))

    def name_of(self, ref):
        for n in ("A", "B", "SRC", "ARGS", "TBA", "TBB", "ICBOUT"):
            if getattr(self, n) == ref:
                return n
        return None

    def va_of(self, name):
        return getattr(self, name + "_va")

    def reply_slot(self):
        """The next 0x20-byte reply slot that holds no first reply."""
        while True:
            k = self.reply_next % (2 * PAGE // 0x20 - 1)   # the last slot is the refused records' own
            self.reply_next += 1
            if k not in self.anchors:
                return k * 0x20


class Model:
    """The bytes the walker predicts: each buffer's first window, ICB2's two
    commands (True: a copy of ICB1's dispatch), and which fences were ever
    updated by committed work."""

    def __init__(self, t):
        self.buf = {n: bytearray(PAGE) for n in ("A", "B", "ICBOUT", "TBA", "TBB")}
        self.buf["SRC"] = bytearray(SRC_LEN)
        self.icb2 = [False, False]

    def copy(self):
        m = Model.__new__(Model)
        m.buf = {k: bytearray(v) for k, v in self.buf.items()}
        m.icb2 = list(self.icb2)
        return m


# ---- one exec as the walker builds it ----

class Seg:
    """A segment: its header's type and flags, its records (each with the
    replies it writes), and the edges it carries."""

    def __init__(self, enc, dec, cont_prev=0, cont_next=0):
        self.enc = enc          # the header's encoder type
        self.dec = dec          # the decoder that decodes it
        self.cont_prev = cont_prev
        self.cont_next = cont_next
        self.recs = []          # [(bytes, reply or None)]
        self.first_is_pass = False
        self.tail = b""         # 1-7 bytes after the last record
        self.cut = b""          # records after the header's length: never decoded
        self.ghost = b""        # a second segment in the same range: never decoded
        self.envelope = False   # a protection envelope, options 0
        self.hdr_len = None     # a header length that is not the segment's
        self.pre_ranges = []    # ranges of 1-7 bytes placed before this segment's

    def body(self):
        return b"".join(r for r, _ in self.recs) + self.tail

    def data(self):
        body = self.body()
        n = 8 + len(body) if self.hdr_len is None else self.hdr_len
        out = abi.SEG_HDR.pack(n & 0xFFFFFFFF, self.enc, self.cont_prev, self.cont_next) + body + self.cut
        if self.envelope:
            out = seg(PROT, struct.pack("<Q", 0) + out)
        return out + self.ghost


class CEnc:
    """A compute encoder's state as the model needs it."""

    def __init__(self, concurrent=False):
        self.pso = None
        self.bufs = {}          # index -> (name or None for nil, offset)
        self.resident = set()
        self.concurrent = concurrent
        self.dispatches = 0
        self.ran_icb = False


class REnc:
    """A render encoder's state: its target, pipeline, viewport, scissor,
    cull mode and winding."""

    def __init__(self, target):
        self.target = target
        self.pso = False
        self.vp = (0, 0, TEXW, TEXH)
        self.sc = (0, 0, TEXW, TEXH)
        self.cull = 0
        self.wind = 0


class ExecBuilder:
    """Builds one exec's segments over a scratch copy of the task's model,
    applying each record's effect in decode (= execution) order."""

    def __init__(self, walk, t, allow_info):
        self.w = walk
        self.r = walk.r
        self.t = t
        self.pre = t.m
        self.m = t.m.copy()
        self.segs = []
        self.allow_info = allow_info
        self.dirty = {}
        # Whether anything in this command buffer names ICBOUT: an ICB's
        # command writes it only then. Residency is the command buffer's:
        # an 0x87 in a later encoder, or a blit of ICBOUT, is enough, and
        # without any the command writes nothing (found by this walker;
        # compute_indirect_command_buffer is the one-encoder case).
        self.icbout_ok = self.r.chance(0.6)
        # Only an exec whose ICBOUT is named runs an ICB command: without
        # a reference in the command buffer whether the command's write
        # lands depends on what earlier command buffers left mapped (it
        # landed or not across runs of one program), so the walker never
        # does that.
        self.icbout_refd = False
        # An ICB's command writes ICBOUT with no order against the command
        # buffer's other work on it (the command's write landed before or
        # after a later blit fill across runs), so a command buffer writes
        # ICBOUT either through ICB commands or through anything else.
        self.icbout_by_icb = False
        self.icbout_by_other = False
        # The run of compute encoders since the last blit or render
        # encoder: their dispatch types, and whether one dispatched
        # indirect. Mixed types and an indirect dispatch in one run kill
        # the process (the ending "mixed_dispatch_indirect").
        self.run = None

    # ---- the model ----

    def touch(self, name, lo, hi):
        a, b = self.dirty.get(name, (lo, hi))
        self.dirty[name] = (min(a, lo), max(b, hi))

    def wr(self, name, off, data, ref=True):
        self.m.buf[name][off:off + len(data)] = data
        self.touch(name, off, off + len(data))
        if name == "ICBOUT":
            if ref:
                self.icbout_by_other = True
            else:
                self.icbout_by_icb = True
        if name == "ICBOUT" and ref:
            # A blit or a dispatch writing ICBOUT makes it resident for
            # the whole command buffer.
            self.icbout_refd = True

    def rd(self, name, off, n):
        return bytes(self.m.buf[name][off:off + n])

    def tex_buf(self, tex):
        return "TBA" if tex == self.t.TA else "TBB"

    def tex_write(self, tex, x, y, px):
        self.wr(self.tex_buf(tex), y * BPR + x * 4, px)

    def tex_read(self, tex, x, y):
        return self.rd(self.tex_buf(tex), y * BPR + x * 4, 4)

    # ---- helpers ----

    def buf_out(self):
        t = self.t
        return self.r.choice([t.A, t.A, t.B, t.B] + ([t.ICBOUT] if self.icbout_ok and not self.icbout_by_icb else []))

    def lim(self, name):
        return 0x100 if name == "ICBOUT" else SPAN

    def region(self):
        r = self.r
        w, h = 1 + r.below(TEXW), 1 + r.below(TEXH)
        x, y = r.below(TEXW - w + 1), r.below(TEXH - h + 1)
        return x, y, w, h

    def colour(self):
        return tuple(self.r.choice(LEVELS) for _ in range(4))

    def fence(self):
        return self.r.choice([self.t.F1, self.t.F2])

    # ---- blit records (deserializer::blit) ----

    def blit_rec(self):
        r, t = self.r, self.t
        k = r.weighted([("fill", 10), ("pattern", 6), ("copy", 10), ("b2t", 6), ("t2b", 6), ("t2t", 4),
                        ("slices", 2), ("fillc", 5), ("fillb", 4), ("noop", 6), ("fence", 3), ("icb", 4)])
        if k == "fill":
            ref = self.buf_out()
            n = t.name_of(ref)
            off = 4 * r.below(self.lim(n) // 4)
            ln = 4 * (1 + r.below(0x40))
            v = r.below(256)
            self.wr(n, off, bytes([v]) * ln)
            return B.fill(ref, off, ln, v)
        if k == "pattern":
            ref = self.buf_out()
            n = t.name_of(ref)
            off = 4 * r.below(self.lim(n) // 4)
            ln = 4 * (1 + r.below(0x40))
            p = r.next() & 0xFFFFFFFF
            self.wr(n, off, struct.pack("<I", p) * (ln // 4))
            return B.fill_pattern(ref, off, ln, p)
        if k == "copy":
            src = r.choice([t.A, t.B, t.SRC, t.SRC])
            dst = self.buf_out()
            sn, dn = t.name_of(src), t.name_of(dst)
            ln = 4 * (1 + r.below(0x40))
            so = 4 * r.below((0xE00 if sn == "SRC" else SPAN) // 4)
            do = 4 * r.below(self.lim(dn) // 4)
            if src == dst and so < do + ln and do < so + ln:
                # Overlapping copies are undefined: the other buffer.
                dst = t.B if src == t.A else t.A
                dn = t.name_of(dst)
            data = self.rd(sn, so, ln)
            self.wr(dn, do, data)
            return B.copy_buffer_to_buffer(src, dst, so, do, ln)
        if k == "b2t":
            src = r.choice([t.A, t.B, t.SRC])
            sn = t.name_of(src)
            tex = r.choice([t.TA, t.TB])
            x, y, w, h = self.region()
            bpr = r.choice([4 * w, 32, 64, 4 * w + 16])
            so = 4 * r.below(0x600 // 4)
            for row in range(h):
                for col in range(w):
                    self.tex_write(tex, x + col, y + row, self.rd(sn, so + row * bpr + 4 * col, 4))
            return B.copy_buffer_to_texture(src, tex, so, bpr, bpr * h, (w, h, 1), (x, y, 0))
        if k == "t2b":
            tex = r.choice([t.TA, t.TB])
            dst = r.choice([t.A, t.B])
            dn = t.name_of(dst)
            x, y, w, h = self.region()
            bpr = r.choice([4 * w, 32, 64])
            do = 4 * r.below(0x500 // 4)
            for row in range(h):
                line = b"".join(self.tex_read(tex, x + col, y + row) for col in range(w))
                self.wr(dn, do + row * bpr, line)
            return B.copy_texture_to_buffer(tex, dst, (x, y, 0), (w, h, 1), do, bpr, bpr * h)
        if k == "t2t":
            src = r.choice([t.TA, t.TB])
            dst = t.TB if src == t.TA else t.TA
            x, y, w, h = self.region()
            dx, dy = r.below(TEXW - w + 1), r.below(TEXH - h + 1)
            px = [[self.tex_read(src, x + c, y + rr) for c in range(w)] for rr in range(h)]
            for rr in range(h):
                for c in range(w):
                    self.tex_write(dst, dx + c, dy + rr, px[rr][c])
            if r.chance(0.5):
                return B.copy_texture_to_texture(src, dst, (x, y, 0), (w, h, 1), (dx, dy, 0))
            return B.copy_texture_to_texture_options(src, dst, (x, y, 0), (w, h, 1), (dx, dy, 0))
        if k == "slices":
            src = r.choice([t.TA, t.TB])
            dst = t.TB if src == t.TA else t.TA
            self.wr(self.tex_buf(dst), 0, self.rd(self.tex_buf(src), 0, TEXLEN))
            return B.copy_slices(src, dst)
        if k == "fillc":
            tex = r.choice([t.TA, t.TB])
            x, y, w, h = self.region()
            c = self.colour()
            for rr in range(h):
                for cc in range(w):
                    self.tex_write(tex, x + cc, y + rr, colour_bytes(c))
            return B.fill_texture_color(tex, 0, 0, (w, h, 1), (x, y, 0), c, R.RGBA8)
        if k == "fillb":
            # The bytes are read out of the buffer at decode time, before any
            # of this exec's GPU work.
            tex = r.choice([t.TA, t.TB])
            src = r.choice([t.SRC, t.A, t.B])
            sn = t.name_of(src)
            off = 4 * r.below(0x400 // 4)
            px = bytes(self.pre.buf[sn][off:off + 4])
            x, y, w, h = self.region()
            for rr in range(h):
                for cc in range(w):
                    self.tex_write(tex, x + cc, y + rr, px)
            return B.fill_texture_bytes(tex, 0, 0, (w, h, 1), (x, y, 0), src, off, 4)
        if k == "noop":
            tex = r.choice([t.TA, t.TB])
            j = r.below(9)
            if j == 0:
                return B.mipmaps(tex)
            if j in (1, 2):
                return B.optimize(308 + j - 1, tex)
            if j in (3, 4):
                return B.optimize_image(310 + j - 3, tex)
            if j == 5:
                # Nullable: 0 and a ref past the list are nil (blit_texture_maintenance).
                return B.synchronize(r.choice([0, t.past, t.A, tex]))
            if j == 6:
                return B.synchronize_image(tex)
            if j == 7:
                return B.invalidate(tex)
            return B.invalidate_image(tex)
        if k == "fence":
            return B.fence(r.choice([316, 317]), self.fence())
        # ICBs: 305 copies ICB1's command into ICB2, 313 resets ICB2's
        # commands, 312 optimizes ICB1 (blit_indirect_command_buffers).
        j = r.below(3)
        if j == 0:
            k2 = r.below(2)
            self.m.icb2[k2] = True
            return B.copy_icb(t.ICB1, t.ICB2, 0, 1, k2)
        if j == 1:
            a = r.below(3)
            n = r.below(3 - a)
            for i in range(a, a + n):
                self.m.icb2[i] = False
            return B.icb_range(313, t.ICB2, a, n)
        return B.icb_range(312, t.ICB1, 0, 1)

    # ---- compute records (deserializer::compute) ----

    def c_ready(self, e, need):
        return all(e.bufs.get(i, (None,))[0] is not None and i in e.bufs for i in need)

    def compute_rec(self, e):
        r, t = self.r, self.t
        opts = [("pso", 6), ("bind", 8), ("offset", 3), ("dispatch", 14), ("use", 3), ("barrier", 3),
                ("bindmisc", 3), ("fence", 2), ("flush", 1), ("icb", 4), ("heap", 1)]
        k = r.weighted(opts)
        if k in ("dispatch", "icb") and e.concurrent and e.dispatches:
            k = "barrier"
        if k == "pso":
            e.pso = r.choice(["DBL", "DBL", "ADD", "INC"])
            return C.set_pipeline(getattr(t, e.pso))
        if k == "bind":
            idx = r.choice([0, 0, 1])
            if idx == 0:
                ref = self.buf_out()
            else:
                ref = r.choice([t.A, t.B, t.SRC])
            n = t.name_of(ref)
            off = 4 * r.below(self.lim(n) // 4) if n != "SRC" else 4 * r.below(0xE00 // 4)
            if r.chance(0.3) and idx == 0:
                # Both indices in one record.
                ref1 = r.choice([t.A, t.B, t.SRC])
                off1 = 4 * r.below(0x700 // 4)
                e.bufs[0] = (n, off)
                e.bufs[1] = (t.name_of(ref1), off1)
                return C.set_buffers(0, [(ref, off), (ref1, off1)])
            if r.chance(0.1):
                # Ref 0 binds nil (compute_err_buffer_refs); a dispatch then
                # waits for a real binding.
                e.bufs[idx] = (None, 0)
                return C.set_buffers(idx, [(0, 0)])
            e.bufs[idx] = (n, off)
            return C.set_buffers(idx, [(ref, off)])
        if k == "offset":
            idx = r.choice([0, 1])
            if e.bufs.get(idx, (None,))[0] is None:
                return C.barrier_scope(1)
            n = e.bufs[idx][0]
            off = 4 * r.below((self.lim(n) if n != "SRC" else 0xE00) // 4)
            e.bufs[idx] = (n, off)
            return C.set_buffer_offset(idx, off)
        if k == "use":
            # Refs that name no resource are nil to 0x87 (the nullable
            # getter): a serial encoder takes them, a concurrent one kills
            # the process (the ending "concurrent_use_nil").
            nils = [] if e.concurrent else [t.DBL, t.past, 0]
            refs = r.sample([t.A, t.B, t.SRC, t.TA] + nils + ([t.ICBOUT] * 2 if self.icbout_ok else []),
                            1 + r.below(3))
            self.icbout_refd |= t.ICBOUT in refs
            for ref in refs:
                e.resident.add(ref)
            return C.use_resources(refs, usage=r.choice([1, 2, 3]))
        if k == "barrier":
            if r.chance(0.5):
                return C.barrier_scope(r.choice([1, 2, 3]))
            # A barrier naming ICBOUT does not make it resident.
            refs = r.sample([t.A, t.B, t.ICBOUT, t.DBL, t.past, 0], 1 + r.below(3))
            return C.barrier_resources(refs)
        if k == "bindmisc":
            j = r.below(3)
            if j == 0:
                return C.set_textures(r.below(4), [r.choice([t.TA, t.TB, 0])])
            if j == 1:
                return C.set_samplers(r.below(4), [r.choice([t.SMP, 0])])
            return C.set_samplers_lod(r.below(4), [(t.SMP, 0.0, 4.0)])
        if k == "fence":
            return C.fence(r.choice([0xD4, 0xD5]), self.fence())
        if k == "flush":
            return C.flush()
        if k == "heap":
            return C.use_heaps([t.HEAP])
        if k == "icb":
            # ICB1 [0, 1) or ICB2 in [0, 2): a command writes ICBOUT when
            # the command buffer names it (an 0x87 in any encoder, or a
            # blit or dispatch writing it), and the walker runs one only
            # then (see icbout_refd).
            e.dispatches += 1
            e.ran_icb = True
            cmds_ok = self.icbout_ok and not self.icbout_by_other
            if r.chance(0.5) and cmds_ok:
                icb, a, n, cmds = t.ICB1, 0, 1, [True]
            else:
                a = r.below(3)
                n = r.below(3 - a)
                if not cmds_ok:
                    # only ICB2's empty commands
                    while any(self.m.icb2[a:a + n]):
                        n -= 1
                icb, cmds = t.ICB2, self.m.icb2[a:a + n]
            if any(cmds):
                # the chain's end names ICBOUT if nothing else does
                self.wr("ICBOUT", 0, struct.pack("<%dI" % ICB_THREADS, *[2 * i for i in range(ICB_THREADS)]),
                        ref=False)
            return C.execute_icb(icb, a, n)
        # a dispatch, when the state allows one
        need = {"DBL": [0], "ADD": [0, 1], "INC": [0]}.get(e.pso)
        if need is None or any(e.bufs.get(i, (None,))[0] is None for i in need):
            return C.barrier_scope(1)
        e.dispatches += 1
        on, oo = e.bufs[0]
        if on not in ("A", "B", "ICBOUT") or (on == "ICBOUT" and self.icbout_by_icb):
            return C.barrier_scope(2)
        if e.pso == "INC":
            c = list(struct.unpack("<16I", self.rd(on, oo, 64)))
            n = (c[0] + 1) & 0xFFFFFFFF
            self.wr(on, oo, struct.pack("<I", n))
            if n < 16:
                self.wr(on, oo + 4 * n, struct.pack("<I", n * 0x11))
            return C.dispatch_threads((1, 1, 1), (1, 1, 1)) if r.chance(0.7) else \
                C.dispatch_threadgroups((1, 1, 1), (1, 1, 1))
        form = r.weighted([("threads", 5), ("groups", 3), ("gind", 2), ("tind", 2)])
        if form == "threads":
            n = 1 + r.below(64)
            tg = 1 + r.below(n)
            rec_ = C.dispatch_threads((n, 1, 1), (tg, 1, 1))
        elif form == "groups":
            tg = r.choice([1, 2, 4, 8, 16])
            g = 1 + r.below(64 // tg)
            n = g * tg
            rec_ = C.dispatch_threadgroups((g, 1, 1), (tg, 1, 1))
        elif form in ("gind", "tind") and len(e.run["types"]) > 1:
            # After a compute encoder of the other dispatch type with no
            # blit or render encoder between, an indirect dispatch kills
            # the process (the ending "mixed_dispatch_indirect").
            return C.barrier_scope(1)
        elif form == "gind":
            e.run["indirect"] = True
            off = r.choice([0x00, 0x10, 0x20, 0x30])
            tg = r.choice([1, 4, 16])
            n = t.args[off][0] * tg
            rec_ = C.dispatch_threadgroups_indirect((tg, 1, 1), off, t.ARGS)
        else:
            e.run["indirect"] = True
            off = r.choice([0x40, 0x60, 0x80, 0xa0])
            n = t.args[off][0]
            rec_ = C.dispatch_threads_indirect(off, t.ARGS)
        if e.pso == "DBL":
            self.wr(on, oo, struct.pack("<%dI" % n, *[2 * i for i in range(n)]))
        else:
            inn, io = e.bufs[1]
            if inn == on and io != oo and abs(io - oo) < 4 * n:
                # Overlapping in and out at different offsets would race:
                # write out a disjoint range instead.
                return C.barrier_scope(1)
            vals = struct.unpack("<%dI" % n, self.rd(inn, io, 4 * n))
            self.wr(on, oo, struct.pack("<%dI" % n, *[(v + 1) & 0xFFFFFFFF for v in vals]))
        return rec_

    # ---- render records (deserializer::render) ----

    def render_pass(self, e):
        """The pass prefix and 0x1a: colour attachment 0 is the target, load
        Clear or Load, store Store; the prefix is 0x1e or 0x1f of one
        sample at random."""
        r = self.r
        pre = b""
        if r.chance(0.2):
            pre += R.default_raster_samples(1)
        if r.chance(0.2):
            pre += R.default_color_samples(1)
        load = r.choice([R.LOAD_CLEAR, R.LOAD_CLEAR, R.LOAD_LOAD])
        c = self.colour()
        if load == R.LOAD_CLEAR:
            for y in range(TEXH):
                for x in range(TEXW):
                    self.tex_write(e.target, x, y, colour_bytes(c))
        return pre + R.describe_pass({0: dict(tex=e.target, load=load, store=R.STORE_STORE, clear=c)},
                                     width=TEXW, height=TEXH)

    def draw(self, e):
        """Draws with the basic pipeline cover the viewport inside the
        scissor unless culled."""
        r = self.r
        k = r.below(7)
        cnt = r.choice([3, 3, 3, 0, 2])
        inst = r.choice([1, 1, 2, 0])
        prim = r.choice([3, 3, 4])
        if k == 0:
            rec_, inst = R.draw16(prim, 0, cnt), 1
        elif k == 1:
            rec_, inst = R.draw64(prim, 0, cnt), 1
        elif k == 2:
            rec_ = R.draw_inst16(prim, 0, cnt, inst)
        elif k == 3:
            rec_ = R.draw_inst64(prim, 0, cnt, inst)
        elif k == 4:
            rec_ = R.draw_inst_base64(prim, 0, cnt, inst, r.below(3))
        elif k == 5:
            rec_ = R.draw_inst_base16(prim, 0, cnt, inst, r.below(3))
        else:
            rec_, inst = R.draw16(prim, 0, cnt), 1
        front = e.wind == TRI_FRONT_WHEN_WINDING
        culled = (e.cull == 1 and front) or (e.cull == 2 and not front)
        if cnt >= 3 and inst >= 1 and not culled:
            vx, vy, vw, vh = e.vp
            sx, sy, sw, sh = e.sc
            for y in range(max(vy, sy), min(vy + vh, sy + sh)):
                for x in range(max(vx, sx), min(vx + vw, sx + sw)):
                    self.tex_write(e.target, x, y, DRAWN)
        return rec_

    def render_rec(self, e):
        r, t = self.r, self.t
        k = r.weighted([("pso", 4), ("draw", 8), ("scissor", 3), ("viewport", 3), ("cull", 3), ("state", 5),
                        ("bind", 3), ("fence", 2), ("use", 2), ("barrier", 2), ("store", 1)])
        if k == "draw" and not e.pso:
            k = "pso"
        if k == "pso":
            e.pso = True
            return R.pipeline_state(t.RP)
        if k == "draw":
            return self.draw(e)
        if k == "scissor":
            x, y = r.below(TEXW), r.below(TEXH)
            w, h = 1 + r.below(TEXW - x), 1 + r.below(TEXH - y)
            e.sc = (x, y, w, h)
            if r.chance(0.5):
                return R.scissor(x, y, w, h)
            return R.scissors([(x, y, w, h)])
        if k == "viewport":
            x, y = r.below(TEXW), r.below(TEXH)
            w, h = 1 + r.below(TEXW - x), 1 + r.below(TEXH - y)
            e.vp = (x, y, w, h)
            if r.chance(0.5):
                return R.viewport(float(x), float(y), float(w), float(h))
            return R.viewports([(float(x), float(y), float(w), float(h), 0.0, 1.0)])
        if k == "cull":
            if r.chance(0.5):
                e.cull = r.below(3)
                return R.value(0x6B, e.cull)
            e.wind = r.below(2)
            return R.value(0x73, e.wind)
        if k == "state":
            j = r.below(10)
            return [R.blend_color(0.5, 0.25, 1.0, 0.5), R.depth_bias(0.0, 0.0, 0.0), R.value(0x6D, r.below(2)),
                    R.value(0x7C, 0), R.stencil_ref(1, 2), R.f32(0x88, 1.0), R.f32(0x8A, 0.5), R.f32(0x8B, 2.0),
                    R.u32(0x90, r.below(2)), rec(0x85)][j]
        if k == "bind":
            other = t.TB if e.target == t.TA else t.TA
            j = r.below(5)
            if j == 0:
                return R.buffers(0x7D, r.below(4), [(r.choice([t.A, t.SRC, 0]), 4 * r.below(16))])
            if j == 1:
                return R.buffers(0x6E, r.below(4), [(r.choice([t.B, t.SRC]), 0)])
            if j == 2:
                return R.refs_range(0x72, 0, [other])
            if j == 3:
                return R.refs_range(r.choice([0x70, 0x7F]), 0, [t.SMP])
            return R.samplers_lod(0x71, 0, [(t.SMP, 0.0, 2.0)])
        if k == "fence":
            if r.chance(0.5):
                return R.update_fence(self.fence(), 2)
            return R.wait_fence(self.fence(), 1)
        if k == "use":
            j = r.below(4)
            if j == 0:
                return R.use_resources([r.choice([t.A, t.SRC, t.RP])], 1)
            if j == 1:
                return R.use_resources_stages([t.A, t.B][:1 + r.below(2)], 1, 3)
            if j == 2:
                return R.use_heaps([t.HEAP])
            return R.use_heaps_stages([t.HEAP], 3)
        if k == "barrier":
            return R.barrier_scope(1, 1, 2)
        return R.color_store_action(R.STORE_STORE, 0)

    # ---- info records (deserializer::info), isolate ----

    def info_rec(self):
        r, t = self.r, self.t
        off = t.reply_slot()
        R_ = t.REPLY
        k = r.below(16)
        if k == 0:
            obj = r.choice([t.DBL, t.ADD, t.INC])
            return info_ref(450, obj, R_, off), (off, 0x19, ("450", obj))
        if k == 1:
            fmt, wd = r.choice([(70, 256), (70, 64), (10, 128)])
            return info_heap_size(R_, off, fmt=fmt, w=wd, h=wd, usage=1, options=0x20), (off, 16, ("451", fmt, wd))
        if k == 2:
            return rec(452, struct.pack("<IIQQ", t.RM, R_, off, 0)), (off, 0x18, ("452",))
        if k == 3:
            return rec(454, struct.pack("<IIQIff", t.RM, R_, off, 0, 10.0, 20.0)), (off, 8, ("454",))
        if k == 4:
            return rec(455, struct.pack("<IIQIff", t.RM, R_, off, 0, 5.0, 6.0)), (off, 8, ("455",))
        if k == 5:
            # 456 maps count x {f32, f32} of the reply buffer in place: the
            # guest writes them.
            self.w.pre_writes.append((t, off, IN456))
            return rec(456, struct.pack("<IIQQII", t.RM, R_, off, 0, 0, 2)), (off, 16, ("456",))
        if k == 6:
            return info_ref(457, t.RP, R_, off), (off, 10, ("457",))
        if k == 7:
            return rec(458, struct.pack("<IQQQIQ", t.RP, 8, 8, 1, R_, off)), (off, 4, ("458",))
        if k == 8:
            obj = r.choice([t.DBL, t.INC])
            return rec(459, struct.pack("<IQQQIQ", obj, 8, 8, 1, R_, off)), (off, 4, ("459", obj))
        if k == 9:
            obj, typ = r.choice([(t.TA, 1), (t.TB, 1), (t.ICB1, 5), (t.ICB2, 5)])
            return rec(460, struct.pack("<IIIQ", obj, typ, R_, off)), (off, 8, ("460", obj))
        if k == 10:
            obj = r.choice([t.A, t.B, t.SRC, t.ICBOUT])
            return info_ref(461, obj, R_, off), (off, 8, ("461", obj))
        if k == 11:
            obj = r.choice([t.TA, t.TB])
            return info_ref(462, obj, R_, off), (off, 16, ("462", obj))
        if k == 12:
            return info_ref(463, t.HEAP, R_, off), (off, 8, ("463",))
        if k == 13:
            cmd, obj = r.choice([(464, t.SMP), (465, t.ICB1), (465, t.ICB2), (466, t.RP), (467, t.DBL),
                                 (467, t.ADD)])
            return info_ref(cmd, obj, R_, off), (off, 16, (str(cmd), obj))
        if k == 14:
            return info_heap_size(R_, off, v2=True, fmt=70, w=128, h=128, usage=1, options=0x20), \
                (off, 16, ("469",))
        return info_ref(461, t.ICBOUT, R_, off), (off, 8, ("461", t.ICBOUT))

    # ---- segments ----

    def open_seg(self, dec, enc=None, cont_prev=0):
        sg = Seg(dec if enc is None else enc, dec, cont_prev=cont_prev)
        self.segs.append(sg)
        return sg

    def fill_seg(self, sg, e, n):
        """n valid records of sg's decoder."""
        for _ in range(n):
            if sg.dec == BLIT:
                b = self.blit_rec()
                if b:
                    sg.recs.append((b, None))
            elif sg.dec == COMPUTE:
                sg.recs.append((self.compute_rec(e), None))
            elif sg.dec == RENDER:
                sg.recs.append((self.render_rec(e), None))
            elif self.allow_info:
                b, rep = self.info_rec()
                sg.recs.append((b, rep))

    def build(self, nsegs, edges=True):
        """nsegs segments of random records, in chains of continued
        segments. The last segment never says continuedByNext, but for an
        info segment (section 6.13: an info decoder continued by nothing
        completes)."""
        r = self.r
        i = 0
        while i < nsegs:
            types = [(BLIT, 5), (COMPUTE, 5), (RENDER, 4), (INFO, 2 if self.allow_info else 1)]
            dec = r.weighted(types)
            chain = 1 + (r.below(3) if r.chance(0.3) else 0)
            chain = min(chain, nsegs - i)
            if dec == COMPUTE:
                if self.run is None:
                    self.run = {"types": set(), "indirect": False}
                conc = r.chance(0.15)
                if self.run["indirect"] and self.run["types"] and conc not in self.run["types"]:
                    conc = next(iter(self.run["types"]))
                e = CEnc(conc)
                e.run = self.run
                self.run["types"].add(conc)
            elif dec == RENDER:
                e = REnc(r.choice([self.t.TA, self.t.TB]))
                self.run = None
            else:
                e = None
                if dec == BLIT:
                    self.run = None
            for j in range(chain):
                sg = self.open_seg(dec, cont_prev=1 if j else 0)
                if j == 0 and dec == COMPUTE:
                    if e.concurrent:
                        sg.recs.append((C.dispatch_type(1), None))
                    elif r.chance(0.2):
                        sg.recs.append((C.dispatch_type(0), None))
                if j == 0 and dec == RENDER:
                    sg.recs.append((self.render_pass(e), None))
                    sg.first_is_pass = True
                nrec = r.below(4) if dec == INFO and not self.allow_info else 1 + r.below(7)
                if dec == INFO and not self.allow_info:
                    nrec = 0
                self.fill_seg(sg, e, nrec)
                if j < chain - 1:
                    sg.cont_next = 1
            if dec == COMPUTE and e.ran_icb and self.icbout_ok and not self.icbout_refd:
                sg.recs.append((C.use_resources([self.t.ICBOUT], usage=r.choice([2, 3])), None))
                self.icbout_refd = True
            # A blit decoder carried into a segment of another type
            # (section 6.13 step 4): it decodes blit records.
            if dec == BLIT and edges and r.chance(0.12) and i + chain < nsegs:
                self.segs[-1].cont_next = 1
                sg = self.open_seg(BLIT, enc=r.choice([COMPUTE, RENDER, INFO, 3]))
                self.fill_seg(sg, None, 1 + r.below(3))
                i += 1
            i += chain
        last = self.segs[-1]
        if last.dec == INFO and edges and r.chance(0.2):
            last.cont_next = 1
        if edges:
            self.edges()

    def edges(self):
        """The valid edges, each at random (see the module docstring)."""
        r = self.r
        for k, sg in enumerate(self.segs):
            if sg.dec != COMPUTE or sg.recs or sg.cont_prev:
                if r.chance(0.08):
                    sg.tail = bytes(1 + r.below(7))
            if r.chance(0.06) and sg.recs:
                # A record longer than its command: the extra bytes are skipped.
                j = r.below(len(sg.recs))
                b, rep = sg.recs[j]
                if not (j == 0 and (sg.first_is_pass or sg.dec == COMPUTE)):
                    extra = 4 * (1 + r.below(4))
                    sg.recs[j] = (struct.pack("<II", *struct.unpack_from("<II", b)[:1],
                                              len(b) + extra) + b[8:] + bytes(extra), rep)
            if r.chance(0.06) and not sg.tail:
                # Records past the header's length: never decoded.
                sg.cut = B.fill(self.t.A, 0, 0x40, 0xD1)
            if r.chance(0.05):
                sg.ghost = seg(BLIT, B.fill(self.t.A, 0x40, 0x40, 0xD2))
            if r.chance(0.06):
                sg.pre_ranges.append(bytes([0x5A] * (1 + r.below(7))))
            prev_open = k > 0 and self.segs[k - 1].cont_next
            if sg.enc == BLIT and sg.dec == BLIT and not sg.cont_prev and not prev_open and not sg.cut \
                    and r.chance(0.1):
                # The inner header is not truncated to its length (section
                # 6.13 step 1), so records past it would be decoded: no cut.
                sg.envelope = True

    # ---- the malformed record, segment or range ----

    def inject(self, iso):
        """Makes the exec fault: returns (kind, (segment index, record
        index)) of the first thing never decoded."""
        r, t = self.r, self.t
        kinds = [("rec", 10), ("seg", 3)]
        where = r.weighted(kinds)
        if where == "seg":
            k = r.weighted([("type", 2), ("mismatch", 2), ("nothing", 1), ("hdr_long", 1), ("hdr_short", 1),
                            ("nopass", 1), ("emptypass", 1), ("nopipe_c", 1), ("peek", 1), ("cut_mid", 1)])
            # A new segment after one that does not say continuedByNext.
            pos = [k2 for k2 in range(len(self.segs) + 1) if k2 == 0 or not self.segs[k2 - 1].cont_next]
            at = r.choice(pos)
            if k == "cut_mid":
                cands = [(k2, j) for k2, sg in enumerate(self.segs) if not sg.envelope
                         for j, (b, _) in enumerate(sg.recs) if len(b) > 8
                         and not (sg.dec == RENDER and sg.first_is_pass and j == 0)]
                if cands:
                    k2, j = r.choice(cands)
                    sg = self.segs[k2]
                    upto = sum(len(b) for b, _ in sg.recs[:j])
                    # Inside the record's payload: its header reads, and its
                    # sub-cursor fails ("Failed to read"). A cut inside a
                    # header would only end the loop.
                    sg.hdr_len = 8 + upto + 8 + r.below(len(sg.recs[j][0]) - 8)
                    sg.tail = b""
                    return "segment length cuts record %d of segment %d" % (j, k2), (k2, j)
                k = "type"
            if k == "type":
                sg = Seg(r.choice([3, 6]), None)
                sg.recs.append((B.fill(t.A, 0, 4, 0xF1), None))
                self.segs.insert(at, sg)
                return "encoder type %d" % sg.enc, (at, 0)
            if k == "mismatch":
                # A chain whose second segment says another type.
                cands = [k2 for k2, sg in enumerate(self.segs) if sg.cont_next and not
                         (k2 + 1 < len(self.segs) and self.segs[k2 + 1].cont_prev == 0)]
                cands = [k2 for k2 in cands if k2 + 1 < len(self.segs)]
                if cands:
                    k2 = r.choice(cands)
                    nx = self.segs[k2 + 1]
                    nx.enc = r.choice([x for x in (BLIT, COMPUTE, RENDER, INFO) if x != self.segs[k2].dec])
                    return "continuation of a type %d decoder as type %d" % (self.segs[k2].dec, nx.enc), (k2 + 1, 0)
                k = "nothing"
            if k == "nothing":
                enc = r.choice([BLIT, COMPUTE, RENDER, INFO])
                sg = Seg(enc, None, cont_prev=1)
                sg.recs.append((B.fill(t.A, 0, 4, 0xF2), None))
                self.segs.insert(at, sg)
                return "type %d continues nothing" % enc, (at, 0)
            if k == "hdr_long":
                sg = Seg(BLIT, BLIT)
                sg.recs.append((B.fill(t.A, 0, 4, 0xF3), None))
                sg.hdr_len = 0x40 + 8 * r.below(8)
                self.segs.insert(at, sg)
                return "segment header longer than its range", (at, 0)
            if k == "hdr_short":
                sg = Seg(r.choice([BLIT, COMPUTE, INFO]), None)
                sg.hdr_len = r.choice([0, 4, 7])
                self.segs.insert(at, sg)
                return "segment header length %d" % sg.hdr_len, (at, 0)
            if k == "emptypass":
                # No attachment, no size, no 0x1e: Metal gives no encoder,
                # "Failed to create render encoder" (:1726-1731).
                sg = Seg(RENDER, RENDER)
                sg.recs.append((R.describe_pass({}), None))
                sg.recs.append((R.pipeline_state(t.RP), None))
                self.segs.insert(at, sg)
                return "render pass with no attachment and no size", (at, 0)
            if k == "nopass":
                sg = Seg(RENDER, RENDER)
                sg.recs.append((r.choice([R.pipeline_state(t.RP), R.draw16(3, 0, 3), R.blend_color(0, 0, 0, 0)]),
                                None))
                self.segs.insert(at, sg)
                return "render segment without a pass", (at, 0)
            if k == "nopipe_c":
                sg = Seg(COMPUTE, COMPUTE)
                sg.recs.append((C.set_buffers(0, [(t.A, 0)]), None))
                sg.recs.append((r.choice([C.dispatch_threads((4, 1, 1), (4, 1, 1)),
                                          C.dispatch_threadgroups((1, 1, 1), (4, 1, 1)),
                                          C.dispatch_threadgroups_indirect((4, 1, 1), 0, t.ARGS),
                                          C.dispatch_threads_indirect(0x40, t.ARGS)]), None))
                self.segs.insert(at, sg)
                return "compute dispatch without a pipeline", (at, 1)
            sg = Seg(COMPUTE, COMPUTE)
            sg.tail = bytes([1, 2, 3, 4][:1 + r.below(4)])
            self.segs.insert(at, sg)
            return "compute segment of %d bytes: the first record's peek fails" % len(sg.tail), (at, 0)
        # a record, in a segment whose decoder faults on it
        cands = [k2 for k2, sg in enumerate(self.segs) if sg.dec in (BLIT, COMPUTE, RENDER)
                 or (sg.dec == INFO)]
        k2 = r.choice(cands)
        sg = self.segs[k2]
        lo = 1 if (sg.dec == RENDER and sg.first_is_pass) or (sg.dec == COMPUTE and not sg.cont_prev) else 0
        j = lo + r.below(len(sg.recs) - lo + 1)
        label, b = self.bad_record(sg.dec, iso)
        sg.recs.insert(j, (b, None))
        return "%s in segment %d at record %d" % (label, k2, j), (k2, j)

    def short(self, b):
        """A record one byte short of its command, the header saying so."""
        cmd, n = struct.unpack_from("<II", b)
        return struct.pack("<II", cmd, n - 1) + b[8:n - 1]

    def bad_record(self, dec, iso):
        """A record that faults its decoder (blit_errors, compute_errors,
        render_pass, render_draw, render_state, info)."""
        r, t = self.r, self.t
        if dec == BLIT:
            opts = [
                ("blit unknown id", lambda: rec(r.choice([299, 324, 0, 399]), u32s(t.A))),
                ("blit record length 4", lambda: rec(306, length=4)),
                ("blit 306 one byte short", lambda: self.short(B.fill(t.A, 0, 4, 1))),
                ("blit 301 source ref 0", lambda: B.copy_buffer_to_buffer(0, t.A, 0, 0, 4)),
                ("blit 306 ref past the list", lambda: B.fill(t.past, 0, 4, 1)),
                ("blit 306 on a texture", lambda: B.fill(t.TA, 0, 4, 1)),
                ("blit 306 past the buffer", lambda: B.fill(t.A, 0x3FF0, 0x11, 1)),
                ("blit 305 destination index 2", lambda: B.copy_icb(t.ICB1, t.ICB2, 0, 1, 2)),
                ("blit 320 length 3", lambda: B.fill_texture_bytes(t.TA, 0, 0, (1, 1, 1), (0, 0, 0), t.SRC, 0, 3)),
                ("blit 316 on a buffer", lambda: B.fence(316, t.A)),
                ("blit 302 texture a buffer", lambda: B.copy_texture_to_buffer(t.A, t.B, (0, 0, 0), (1, 1, 1),
                                                                               0, 16, 16)),
            ]
        elif dec == COMPUTE:
            opts = [
                ("compute unknown id", lambda: rec(r.choice([0x100, 0xE7, 0xC7]), u32s(0))),
                ("compute 0xdb not first", lambda: C.dispatch_type(0)),
                ("compute record length 4", lambda: rec(0xCA, length=4)),
                ("compute 0xd0 ref 0", lambda: C.set_pipeline(0)),
                ("compute 0xd0 a buffer", lambda: C.set_pipeline(t.A)),
                ("compute 0xcb location 0x10000", lambda: C.set_buffers(0x10000, [], count=1)),
                ("compute 0xcb a texture", lambda: C.set_buffers(0, [(t.TA, 0)])),
                ("compute 0x87 count 2 of 1", lambda: C.use_resources([t.A], count=2)),
                ("compute 0x86 a buffer", lambda: C.use_heaps([t.A])),
                ("compute 0xd4 ref 0", lambda: C.fence(0xD4, 0)),
                ("compute 0xe4 ICB1 [1, 3)", lambda: C.execute_icb(t.ICB1, 1, 2)),
                ("compute 0xca short", lambda: rec(0xCA, bytes(8))),
            ]
        elif dec == RENDER:
            opts = [
                ("render unknown id", lambda: rec(r.choice([0x30, 0x64, 0xA7]), u32s(0))),
                ("render 0x1e in the main loop", lambda: R.default_raster_samples(1)),
                ("render 0x1a in the main loop", lambda: R.describe_pass({0: dict(tex=t.TA)}, width=8, height=8)),
                ("render draw primitive type 10", lambda: R.pipeline_state(t.RP) + R.draw16(10, 0, 3)),
                ("render 0x74 ref 0", lambda: R.pipeline_state(0)),
                ("render 0x16", lambda: rec(0x16, u32s(0, 0))),
                ("render 0x66 index 1", lambda: R.color_store_action(R.STORE_STORE, 1)),
                ("render 0x96 index 0", lambda: R.color_resolve(t.TB, 0, 0, 0, 0, 0)),
                ("render record length 4", lambda: rec(0x65, b"", length=4)),
                ("render 0x18 ref 0", lambda: R.update_fence(0, 1)),
                ("render 0x01 short", lambda: rec(0x01, b"\x03\x00")),
            ]
        else:
            # Throws before the reply cursor is fetched, so safe in-process
            # too.
            opts = [
                ("info unknown id", lambda: rec(r.choice([449, 470, 0]), u32s(0, 0, 0, 0))),
                ("info 450 pipeline ref 0", lambda: info_ref(450, 0, t.A, 0)),
                ("info 457 a compute pipeline", lambda: info_ref(457, t.DBL, t.A, 0)),
                ("info 461 a texture", lambda: info_ref(461, t.TA, t.A, 0)),
                ("info record length 4", lambda: rec(461, length=4)),
            ]
            if iso:
                opts += [
                    ("info 461 reply past the buffer", lambda: info_ref(461, t.A, t.REPLY, 2 * PAGE)),
                    ("info 460 object type 2", lambda: rec(460, struct.pack("<IIIQ", t.TA, 2, t.REPLY, LAST_SLOT))),
                    ("info 468 a depth-stencil state", lambda: info_ref(468, t.DSS, t.REPLY, LAST_SLOT)),
                ]
        label, f = opts[r.below(len(opts))]
        return label, f()

    # ---- placing it ----

    def ranges(self):
        """The command-buffer ranges, one segment each, plus the 1-7 byte
        ranges the edges put before some."""
        t = self.t
        out = []
        for sg in self.segs:
            for pr in sg.pre_ranges:
                out.append(t.cmdbuf(pr))
            out.append(t.cmdbuf(sg.data()))
        return out

    def size(self):
        return sum(len(sg.data()) + 0x100 + 0x100 * len(sg.pre_ranges) for sg in self.segs)

    def replies(self):
        """[(slot, size, kind, landed)] in decode order, given the fault
        position (or None)."""
        out = []
        for k, sg in enumerate(self.segs):
            for j, (b, rep) in enumerate(sg.recs):
                if rep is not None:
                    out.append((k, j) + rep)
        return out


# ---- the walk ----

class Walk:
    def __init__(self, name, doc, seed, steps, setups, host, two, ending, fault_p):
        self.r = Rng(seed * 0x2545F4914F6CDD1D + 0x51)
        self.setups = tuple(setups)
        self.iso = self.setups == ISO
        s = self.s = Scenario(name, setups=setups, host=host, doc=doc, watchdog_s=90)
        s.bring_up()
        self.steps = steps
        self.ending = ending
        self.fault_p = fault_p
        self.batch_no = 0
        self.model_page = s.alloc_page()
        self.ee_page = s.alloc_page()
        s.fill(s.pa(self.ee_page), 0x40, EE)
        self.pre_writes = []
        t1 = DTask(s, 1, self.iso)
        t1.fifo = s.fifo
        self.tasks = [t1]
        t1.define(timeout_ms=WAIT_MS)
        t1.install()
        if two:
            c = child(s, 1)
            t2 = DTask(s, 2, self.iso)
            t2.fifo = c
            t2.define(fifo=c, timeout_ms=WAIT_MS)
            t2.install()
            self.tasks.append(t2)

    def label(self, text):
        return "b%d: %s" % (self.batch_no, text)

    # ---- checks ----

    def compare(self, t, name, lo, hi, label):
        s = self.s
        lo &= ~3
        hi = (hi + 3) & ~3
        emit_bytes(s, s.pa(self.model_page), bytes(t.m.buf[name][lo:hi]))
        s.cmp(t.gpa(t.va_of(name) + lo), s.pa(self.model_page), hi - lo,
              label=self.label("task %d %s [0x%x, 0x%x)" % (t.tid, name, lo, hi)))

    def check_replies(self, t, reps, fault_at):
        s = self.s
        for k, j, off, n, kind in reps:
            gpa = t.gpa(t.REPLY_va + off)
            if fault_at is not None and (k, j) >= fault_at:
                if kind[0] == "456":
                    # 456 rewrites the guest's own coordinates in place.
                    emit_bytes(s, s.pa(self.model_page), IN456)
                    s.cmp(gpa, s.pa(self.model_page), n, label=self.label(
                        "task %d reply 456 at 0x%x: never written, the guest's coordinates" % (t.tid, off)))
                    continue
                s.cmp(gpa, s.pa(self.ee_page), n, label=self.label("task %d reply %s at 0x%x: never written"
                                                                   % (t.tid, "/".join(map(str, kind)), off)))
                continue
            key = kind
            if key not in t.first_reply:
                t.first_reply[key] = gpa
                t.anchors.add(off // 0x20)
                s.dump(gpa, n, label=self.label("task %d reply %s: the first" % (t.tid, "/".join(map(str, kind)))))
            else:
                s.cmp(gpa, t.first_reply[key], n,
                      label=self.label("task %d reply %s == the first" % (t.tid, "/".join(map(str, kind)))))

    # ---- one batch ----

    def batch(self, allow_fault=True):
        r, s = self.r, self.s
        self.batch_no += 1
        live = [t for t in self.tasks if not t.dead]
        nexec = 1 + r.below(4)
        plan = [r.choice(live) for _ in range(nexec)]
        fault_last = allow_fault and r.chance(self.fault_p)
        builders = []
        self.pre_writes = []
        # Command buffers: each task's region restarts when this batch would
        # not fit (a held exec reads its ranges when it runs).
        need = {}
        for i, t in enumerate(plan):
            eb = ExecBuilder(self, t, allow_info=self.iso)
            eb.build(1 + r.below(6) if r.chance(0.8) else 1)
            fault = None
            if fault_last and i == nexec - 1:
                fault = eb.inject(self.iso)
            op = r.weighted([(E3, 7), (E2, 2), (E9, 1)])
            builders.append((t, eb, fault, op))
            if fault is None and op != E9:
                t.m = eb.m
            # else nothing of a faulted exec is committed, and InfoIndirect
            # never commits (docs/testing.md 6.12): the spans it would have
            # written are compared with the model as it was.
            need[t.tid] = need.get(t.tid, 0) + eb.size()
        for t in live:
            if t.tid in need and t._cmd + need[t.tid] > 8 * PAGE:
                t.reset_cmdbufs()
        # Replies' slots filled with 0xEE, 456's inputs written, by the guest.
        for t, eb, fault, op in builders:
            for k, j, off, n, kind in eb.replies():
                t.fill(t.REPLY_va + off, 0x20, EE)
        for t, off, data in self.pre_writes:
            t.write(t.REPLY_va + off, data)
        nodes = []
        prev = None
        for t, eb, fault, op in builders:
            f = t.fifo
            cbs = eb.ranges()
            bars = [(prev[0].stamp_index, prev[1])] if prev is not None else []
            ring_off = f.written % f.ring_size()
            st = f.cmd(op, exec_payload(t.tid, cbs, t.resources(), op), barriers=bars)
            nodes.append((t, eb, fault, op, f, st, ring_off))
            prev = (f, st)
        rung = []
        for t, eb, fault, op, f, st, ro in nodes:
            if f not in rung:
                rung.append(f)
        for f in rung:
            f.doorbell()
        t, eb, fault, op, f, st, ro = nodes[-1]
        desc = ", ".join("%s%d(%s)" % ({E3: "67", E2: "55", E9: "69"}[n[3]], n[0].tid,
                                        "".join({RENDER: "r", COMPUTE: "c", BLIT: "b", INFO: "i"}.get(sg.dec, "x")
                                                for sg in n[1].segs)) for n in nodes)
        if fault is not None:
            lab = self.label("%s: %s @0x%x" % (desc, fault[0], ro))
            if f.fid == 0:
                s.wait_fault(timeout_ms=WAIT_MS, label=lab)
                s.r(abi.R_FAULT_MASK, label=lab)
                s.r(abi.R_FAULT_OFFSET, label=lab)
                s.resume_root()
            else:
                s.wait(gpa=f.node_gpa(NODE_FAULT), ne=HEALTHY, timeout_ms=WAIT_MS, label=lab + ": node")
                s.wait(mmio=abi.R_FAULT_MASK, mmio_ne=0, timeout_ms=WAIT_MS, label=lab + ": 0x102c")
                s.r(abi.R_FAULT_MASK, label=lab)
                s.dump(f.node_gpa(NODE_FAULT), 4, label=lab)
                s.w(abi.R_RESUME_CHILD, f.fid)
            f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("resumed, stamp %d" % st))
        else:
            f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("%s: stamp %d" % (desc, st)))
        # The checks: every span written, every reply.
        dirty = {}
        for t, eb, fault_, op, f_, st_, ro_ in nodes:
            for name, (lo, hi) in eb.dirty.items():
                a, b = dirty.get((t.tid, name), (lo, hi))
                dirty[(t.tid, name)] = (min(a, lo), max(b, hi))
            if self.iso:
                self.check_replies(t, eb.replies(), fault_[1] if fault_ is not None else None)
        for (tid, name), (lo, hi) in sorted(dirty.items()):
            t = [x for x in self.tasks if x.tid == tid][0]
            self.compare(t, name, lo, hi, name)

    def guest_writes(self):
        """The guest rewrites a task's SRC between batches."""
        self.batch_no += 1
        t = self.r.choice([t for t in self.tasks if not t.dead])
        t.write_src(self.r.next() & 0xFFFFFFFF, (self.r.next() | 1) & 0xFFFFFFFF)

    def full_compare(self):
        """Every window of every task against the model."""
        self.batch_no += 1
        for t in self.tasks:
            if t.dead:
                continue
            for name, n in (("A", WIN), ("B", WIN), ("ICBOUT", 0x200), ("TBA", TEXLEN), ("TBB", TEXLEN)):
                self.compare(t, name, 0, n, name)

    # ---- the program ----

    def run(self):
        r, s = self.r, self.s
        s.events("tasks defined", sort_blocks=True, blocks_digest=True)
        for i in range(self.steps):
            if r.chance(0.06):
                self.guest_writes()
            self.batch()
            if (i + 1) % 25 == 0:
                s.events(self.label("events"), sort_blocks=True, blocks_digest=True)
        self.full_compare()
        s.fault_state(self.label("after the walk"))
        if self.ending:
            self.end()
        self.batch_no += 1
        for t in self.tasks:
            s.submit(abi.OP_DELETE_TASK, struct.pack("<I", t.tid), timeout_ms=WAIT_MS,
                     label=self.label("32 task %d" % t.tid))
        s.events(self.label("end"), sort_blocks=True, blocks_digest=True)
        return s

    # ---- the ending ----

    def death_segments(self, t, kind):
        """The last exec's segments (docs/testing.md 7.4)."""
        if kind == "open_encoder":
            return [seg(BLIT, B.fill(t.A, 0, 0x10, 0x11)), seg(BLIT, B.fill(t.B, 0, 0x10, 0x12), cont_next=1)]
        if kind == "second_encoder":
            return [seg(COMPUTE, C.set_pipeline(t.DBL) + C.set_buffers(0, [(t.A, 0)]), cont_next=1),
                    seg(COMPUTE, C.dispatch_threads((4, 1, 1), (4, 1, 1)))]
        if kind == "indirect_nil":
            return [seg(COMPUTE, C.set_pipeline(t.DBL) + C.set_buffers(0, [(t.A, 0)])
                        + C.dispatch_threadgroups_indirect((4, 1, 1), 0, 0))]
        if kind == "concurrent_use_nil":
            # Found by this walker: 0x87 with a ref that resolves to nil in
            # a concurrent compute encoder (0xdb 1). The same record in a
            # serial encoder runs (compute_err_resource_lists).
            return [seg(BLIT, B.fill(t.B, 0, 0x10, 0x15)),
                    seg(COMPUTE, C.dispatch_type(1) + C.set_pipeline(t.DBL) + C.set_buffers(0, [(t.A, 0)])
                        + C.use_resources([t.A, self.r.choice([0, t.DBL, t.past])], usage=1)
                        + C.dispatch_threads((4, 1, 1), (4, 1, 1)))]
        if kind == "mixed_dispatch_indirect":
            # Found by this walker: two compute encoders in a row whose
            # dispatch types differ (0xdb), then an indirect dispatch.
            ind = self.r.choice([C.dispatch_threadgroups_indirect((4, 1, 1), 0x10, t.ARGS),
                                 C.dispatch_threads_indirect(0x40, t.ARGS)])
            return [seg(BLIT, B.fill(t.B, 0, 0x10, 0x16)),
                    seg(COMPUTE, C.dispatch_type(1) + C.set_pipeline(t.DBL) + C.set_buffers(0, [(t.A, 0)])
                        + C.dispatch_threads((4, 1, 1), (4, 1, 1))),
                    seg(COMPUTE, C.set_pipeline(t.DBL) + C.set_buffers(0, [(t.A, 0x100)]) + ind)]
        if kind == "no_target":
            # render_pass_no_target: no attachment and no 0x1e, with a
            # target size (without one Metal returns no encoder: a fault).
            return [seg(RENDER, R.describe_pass({}, width=TEXW, height=TEXH) + R.pipeline_state(t.RP)
                        + R.draw16(3, 0, 3))]
        if kind == "info_reply":
            return [seg(BLIT, B.fill(t.B, 0, 0x10, 0x13)), seg(INFO, info_ref(461, t.A, t.A, 0xF00))]
        raise ValueError(kind)

    def end(self):
        s = self.s
        t = self.tasks[0]
        kind = self.ending
        self.batch_no += 1
        t.reset_cmdbufs()
        cbs = [t.cmdbuf(sg) for sg in self.death_segments(t, kind)]
        f = s.fifo
        # 256 NOPs keep the root FIFO busy past the doorbell's register
        # write, so a host death lands in the wait (exec_objects.submit_last).
        for _ in range(256):
            f.cmd(abi.OP_NOP)
        ro = f.written % f.ring_size()
        st = f.cmd(E3, exec_payload(t.tid, cbs, t.resources()))
        f.doorbell()
        if kind == "info_reply":
            s.wait_any([dict(gpa=f.stamp_gpa(), ge=st), dict(mmio=abi.R_FAULT_MASK, mmio_eq=1)],
                       timeout_ms=WAIT_MS, label=self.label("info reply: %s" % EITHER) + DIES)
            s.fault_state(self.label("info reply"))
            s.resume_root()
            f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("info reply: stamp %d" % st))
            t.m.buf["B"][0:0x10] = b"\x13" * 0x10
            self.compare(t, "B", 0, 0x10, "B after the info reply")
            return
        if kind == "mixed_dispatch_indirect":
            # The host Metal's to end or run (the scenario's docstring): the
            # walk ends at this exec, so what follows holds either way.
            f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("%s: stamp %d" % (kind, st)))
            s.fault_state(self.label(kind))
            # The exec's writes: B's fill, then DBL's 4 threads on A at 0 and
            # its 16 on A at 0x100 (4 groups of 4 from ARGS at 0x10, or 16
            # threads from ARGS at 0x40).
            t.m.buf["B"][0:0x10] = b"\x16" * 0x10
            t.m.buf["A"][0:0x10] = struct.pack("<4I", *[2 * i for i in range(4)])
            t.m.buf["A"][0x100:0x140] = struct.pack("<16I", *[2 * i for i in range(16)])
            self.full_compare()
            return
        lab = self.label("%s: the host dies or the service @0x%x" % (kind, ro))
        s.wait_fault(timeout_ms=WAIT_MS, label=lab + DIES)
        s.r(abi.R_FAULT_MASK, label=lab)
        s.r(abi.R_FAULT_OFFSET, label=lab)
        s.resume_root()
        f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("%s: resumed, stamp %d" % (kind, st)))
        # Never reached: every ending here dies inside Metal, which ends the
        # host in every setup. After a task's own death the next exec would
        # fault too.
        ro = f.written % f.ring_size()
        t.reset_cmdbufs()
        cbs = [t.cmdbuf(seg(BLIT, B.fill(t.A, 0, 0x10, 0x14)))]
        st = f.cmd(E3, exec_payload(t.tid, cbs, t.resources()))
        f.doorbell()
        lab = self.label("task 1's next exec, its service gone @0x%x" % ro)
        s.wait_fault(timeout_ms=WAIT_MS, label=lab)
        s.r(abi.R_FAULT_MASK, label=lab)
        s.r(abi.R_FAULT_OFFSET, label=lab)
        s.resume_root()
        f.wait_stamp(st, timeout_ms=WAIT_MS, label=self.label("resumed, stamp %d" % st))
        # Deleted, defined again: a new service, whose ICBs and fences start
        # from the guest's records again.
        s.submit(abi.OP_DELETE_TASK, struct.pack("<I", t.tid), timeout_ms=WAIT_MS, label=self.label("32 task 1"))
        t.define(timeout_ms=WAIT_MS, label=self.label("56 task 1 again"))
        t.install(timeout_ms=WAIT_MS, label=self.label("51 task 1 again"))
        t.m.icb2 = [False, False]
        t.first_reply, t.anchors = {}, set()
        for _ in range(3):
            self.batch(allow_fault=False)
        self.full_compare()


def build(name, doc, seed, steps, setups=SETUPS, host=False, two=False, ending=None, fault_p=0.3):
    return Walk(name, doc, seed, steps, setups, host, two, ending, fault_p).run()


# ---- reading the expected files back ----

def check(pattern_=None):
    """Every cmp equal; every wait ok but the either() and [dies] ones;
    every fault offset (0x103c, or a child's node) the one its label names;
    every scenario completed but those that end in a host death."""
    import glob
    import re
    base = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "expected",
                        "interplay_decodewalk")
    bad = 0
    n = 0
    for path in sorted(glob.glob(os.path.join(base, "*", "*.jsonl"))):
        if pattern_ and pattern_ not in path:
            continue
        n += 1
        for line in open(path):
            o = json.loads(line)
            ev = o.get("ev")
            lab = o.get("label", "")
            if ev == "cmp" and not o.get("equal"):
                print("%s: cmp %s differs" % (path, lab))
                bad += 1
            if ev == "wait" and not o.get("ok") and EITHER not in lab and DIES not in lab:
                print("%s: wait %s failed" % (path, lab))
                bad += 1
            m = re.search(r"@0x([0-9a-f]+)", lab)
            if m and "[dies]" not in lab:
                want = int(m.group(1), 16)
                if ev == "mmio_r" and o.get("off") == "0x103c" and int(o["val"], 16) != want:
                    print("%s: %s: 0x103c %s" % (path, lab, o["val"]))
                    bad += 1
                if ev == "dump" and struct.unpack("<I", bytes.fromhex(o["hex"]))[0] != want:
                    print("%s: %s: node offset %s" % (path, lab, o["hex"]))
                    bad += 1
            if ev == "outcome" and o["kind"] not in ("completed", "died"):
                print("%s: outcome %s" % (path, o))
                bad += 1
    print("%d files checked; %d problems" % (n, bad))
    return bad


if __name__ == "__main__":
    import sys
    sys.exit(1 if check(sys.argv[1] if len(sys.argv) > 1 else None) else 0)
