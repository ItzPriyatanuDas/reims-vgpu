"""Decode walks (pgtest/decodewalk_util.py): each seed is one scenario,
interplay_decodewalk_<seed>, whose program the walker builds from the seed
alone: batches of execs whose segments are seeded random streams of valid
blit, compute, render and info records, several segments an exec and
several execs a batch, continued segments sharing an encoder, the valid
edges of section 6.13 (a blit decoder carried into a segment of another
type, an info segment continued by nothing, a protection envelope, short
ranges, trailing bytes, records longer than their commands, records past a
segment's length), and at most one malformed record or segment a batch,
on the batch's last exec, which faults at its ring offset and commits
nothing (docs/testing.md, sections 6.12-6.19). Every span of guest memory
the GPU wrote (two output buffers, the indirect command buffers' target,
two 8x8 RGBA8Unorm textures over buffers, read back directly and through
blits) is compared with the walker's model after each batch.

The all-setup walks (host-independent) run in blocks, map and isolate, and
meet the info decoder only where it throws before a reply (in-process a
reply aborts the host, section 7.4); some carry a second task on child
FIFO 1, whose execs interleave with the first's through barriers. The
isolate walks add info replies (host values, so keyed by host): the first
reply of each kind is dumped, every later one compared with it, and a reply
after a malformed record is compared with the 0xEE the guest wrote there.

Death-last seeds end in one of the decoder deaths of section 7.4 on task
1: an encoder left open at commit, a second compute encoder over a carried
one, DispatchThreadgroupsIndirect from buffer ref 0, a render pass with a
size and no attachment, an 0x87 naming a nil ref in a concurrent encoder, an
indirect dispatch in a run of compute encoders of mixed dispatch types, or
(all-setup walks) an info reply. In-process the host dies in the wait
labelled [dies]. Every ending but the info reply and the mixed dispatch
types is a death inside Metal, so under isolation too the host dies in that
wait, and the steps after it (the task's next exec, the task deleted,
defined again and walked on) are never reached; the info reply is one the
service writes, so under isolation its exec completes.

The indirect dispatch in a run of mixed dispatch types is the host Metal's
to end or run: on the M3 Max, Metal at macOS 26.7 ends the host there and
27.0.1 runs it (measured). That seed is keyed by host, and its walk ends at
that exec: its stamp, the fault state and every window compared with the
model, which the exec's writes update. Where the host dies, none of these
is reached.

python3 -m pgtest.decodewalk_util checks the expected files: every cmp
equal, every fault offset the one its label names.
"""
from pgtest.decodewalk_util import build
from pgtest.registry import scenario

ISO = ("isolate",)

# (seed, batches, isolate walk, second task, ending, fault probability)
SEEDS = [
    # all setups: medium walks
    (1, 40, False, False, None, 0.3),
    (2, 40, False, False, None, 0.3),
    (3, 40, False, True, None, 0.3),
    (4, 60, False, False, None, 0.3),
    (5, 60, False, True, None, 0.3),
    (6, 30, False, False, None, 0.8),
    (7, 30, False, True, None, 0.8),
    # all setups: long
    (11, 250, False, False, None, 0.25),
    (12, 250, False, True, None, 0.25),
    # all setups: ending in a death
    (21, 30, False, False, "open_encoder", 0.3),
    (22, 30, False, True, "second_encoder", 0.3),
    (23, 30, False, False, "indirect_nil", 0.3),
    (24, 30, False, True, "no_target", 0.3),
    (25, 30, False, False, "info_reply", 0.3),
    (26, 30, False, True, "concurrent_use_nil", 0.3),
    (27, 30, False, False, "mixed_dispatch_indirect", 0.3),
    # isolate: info replies among the records
    (41, 40, True, False, None, 0.3),
    (42, 40, True, True, None, 0.3),
    (43, 30, True, False, None, 0.8),
    (44, 250, True, True, None, 0.25),
    (51, 30, True, False, "open_encoder", 0.3),
    (52, 30, True, True, "indirect_nil", 0.3),
]


@scenario
def interplay_decodewalk():
    out = []
    for seed, steps, iso, two, ending, fault_p in SEEDS:
        kw = dict(setups=ISO, host=True) if iso else {}
        if ending == "mixed_dispatch_indirect":
            # Where it ends is the host Metal's (the module's docstring).
            kw["host"] = True
        out.append(build("interplay_decodewalk_%d" % seed, __doc__, seed, steps, two=two, ending=ending,
                         fault_p=fault_p, **kw))
    return out
