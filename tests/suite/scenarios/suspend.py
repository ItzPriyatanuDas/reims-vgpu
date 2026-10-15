"""Suspend, resume and the lifecycle (docs/testing.md, sections 4.7 and 6.24):
the blob rvg_device_finish_suspend writes, crafted blobs and every
RvgResumeError, the capability check, resume onto a fresh device that rebuilds
FIFOs, tasks, displays and EFI from guest memory, the display restore, pause
and unpause, stop, and the asserts a host can reach.

The blob is parsed by the normalizer (blob_pgsd) into the registers and the
device info it carries.
"""
import struct

from pgtest import abi
from pgtest.abi import PAGE
from pgtest.display_guest import DisplayGuest, display, p_glyph, p_present3, p_show, p_sleep, p_swap, pattern
from pgtest.efi_guest import EFI
from pgtest.fifo import ChildFIFO
from pgtest.iosfc_guest import CMD_MAP, IOSFC, desc_v2
from pgtest.registry import scenario
from pgtest.scenario import Scenario
from pgtest import suspend_guest as sg
from pgtest.suspend_guest import blob, state
from pgtest.task import GPUTask, rec, seg


def suspend(s, slot="s"):
    s.api("willSuspend")
    s.api("finishSuspend", slot=slot)


def resume_fresh(s, slot="s", label=None):
    """A migration: a fresh device over the same guest RAM, the blob, and
    rvg_device_did_resume."""
    s.api("recreate")
    s.api("willResume", slot=slot, label=label)
    s.api("didResume")


def will_resume(s, data, label):
    s.api("willResume", hex=data.hex(), label=label)


# ---- the blob ----

@scenario
def suspend_blob():
    """rvg_device_finish_suspend's blob for a fresh device, one with only a
    version, and one brought up with a FIFO, stamps, a fault and EFI registers:
    the registers (reported under device-state keys 1-19 and 21), then the
    device info: the last device-info query's max key (1 by default) and the
    pairs the host wrote for it (none by default). The length:
    108 + 8 x pairs."""
    out = []
    s = Scenario("suspend_blob_fresh", doc=suspend_blob.__doc__)
    suspend(s)
    out.append(s)
    s = Scenario("suspend_blob_version", doc=suspend_blob.__doc__)
    s.w(abi.R_VERSION, 110)
    suspend(s)
    out.append(s)
    s = Scenario("suspend_blob_running", displays=[display(0)], descriptor={"trace": True}, doc=suspend_blob.__doc__)
    f = s.bring_up()
    s.submit(abi.OP_NOP)
    s.submit(99, fault=True)
    e = EFI(s)
    e.setup(16, 8, 64)
    s.wait(cb="newFrame")
    s.events("before")
    suspend(s)
    s.events("suspended")
    s.sweep(label="registers after finishSuspend")
    out.append(s)
    return out


@scenario(host=True)
def suspend_blob_device_info():
    """After a device-info query (58) with max key 45 the blob carries the
    host's device-info pairs, up to their {0, 0}; a max key of 100 is
    carried as it is, with the pairs the host wrote for it (at most 45)."""
    out = []
    for key in (45, 100):
        s = Scenario("suspend_blob_device_info_%d" % key, host=True, doc=suspend_blob_device_info.__doc__)
        s.bring_up()
        s.submit(abi.OP_GET_DEVICE_INFO, struct.pack("<III", key, 64, s.pfn(s.alloc_page())))
        suspend(s)
        out.append(s)
    return out


# ---- rvg_device_will_resume's refusals ----

@scenario
def suspend_resume_errors():
    """rvg_device_will_resume's answers for crafted blobs: data shorter
    than the header, a magic that is not ours (a PGSD blob's included) and
    malformed sections (one running past the end, an unknown tag, a section
    repeated or missing, a payload of the wrong length, more than 45
    device-info pairs, or a device-info key a resume does not read: 0, 43,
    45) are InvalidContent (2); a version other than 1 is
    InvalidSuspendStateVersion (1). The sections may come in either order,
    and the device-info max key is carried as it is, negative included. A
    binary version >= 0xf6, or one the version negotiation rejects (1,
    236), is InvalidGuestVersion (3); a port count other than the device's
    is InvalidDisplayPortCount (5); connection types that differ at version
    >= 238 are InternalFault (0). A blob setting every register is
    accepted. The device is never resumed."""
    s = Scenario("suspend_resume_errors", doc=suspend_resume_errors.__doc__)
    every = [(k, 0) for k in sg.REGISTER_KEYS]
    every[1] = (sg.K_VERSION, 0xF5)
    every[17] = (sg.K_PORT_COUNT, 1)
    regs = sg.section(sg.TAG_REGISTERS, sg.registers(state()))
    info = sg.section(sg.TAG_DEVICE_INFO, sg.device_info())
    cases = [
        ("empty", b""),
        ("1 byte", b"\x00"),
        ("magic only", struct.pack("<I", sg.MAGIC)),
        ("magic and 3 bytes", struct.pack("<I", sg.MAGIC) + b"\x01\x00\x00"),
        ("bad magic", sg.header(magic=0x52564753) + regs + info),
        ("a PGSD V2 header", struct.pack("<IIII", 0x50475344, 2, 22, 1)),
        ("version 2", sg.header(version=2) + regs + info),
        ("version 0", sg.header(version=0) + regs + info),
        ("version 0xffffffff", sg.header(version=0xFFFFFFFF)),
        ("header only", sg.header()),
        ("header and 4 bytes", sg.header() + struct.pack("<I", sg.TAG_REGISTERS)),
        ("registers only", sg.header() + regs),
        ("device info only", sg.header() + info),
        ("device info, then registers", sg.header() + info + regs),
        ("registers repeated", sg.header() + regs + info + regs),
        ("an unknown section 3", sg.header() + regs + info + sg.section(3, b"")),
        ("registers of 76 bytes", sg.header() + sg.section(sg.TAG_REGISTERS, sg.registers(state())[:76]) + info),
        ("registers of 84 bytes", sg.header() + sg.section(sg.TAG_REGISTERS, sg.registers(state()) + bytes(4)) + info),
        ("registers past the end",
         sg.header() + info + sg.section(sg.TAG_REGISTERS, sg.registers(state())[:40], length=80)),
        ("device info of 3 bytes", sg.header() + regs + sg.section(sg.TAG_DEVICE_INFO, bytes(3))),
        ("device info with half a pair",
         sg.header() + regs + sg.section(sg.TAG_DEVICE_INFO, sg.device_info([(1, 1)])[:8])),
        ("45 device info pairs", blob(state(), [(1, 1)] * 45)),
        ("46 device info pairs", blob(state(), [(1, 1)] * 46)),
        ("device info key 0", blob(state(), [(0, 0)])),
        ("device info key 43", blob(state(), [(43, 0)])),
        ("device info key 45", blob(state(), [(45, 0)])),
        ("device info max key -1", blob(state(), info_max=-1)),
        ("every register", blob(every)),
        ("binary version 0xf6", blob(state(version=0xF6))),
        ("binary version 0x80000000", blob(state(version=0x80000000))),
        ("binary version 1", blob(state(version=1))),
        ("binary version 236", blob(state(version=236))),
        ("binary version 0", blob(state(version=0))),
        ("binary version 244 steps down", blob(state(version=244))),
        ("port count 2", blob(state(ports=2))),
        ("no port count", blob([(sg.K_VERSION, 0xF5)])),
        ("connection types 1 at 245", blob(state(connection_types=1))),
        ("connection types 1 at 237", blob(state(version=237, connection_types=1))),
    ]
    for label, data in cases:
        will_resume(s, data, label)
    s.r(abi.R_VERSION, "after the last")
    return s


@scenario
def suspend_resume_nil():
    """rvg_device_will_resume with the empty blob of a slot never filled: empty
    data, not a NULL state (suspend_gap_nil_state passes NULL)."""
    s = Scenario("suspend_resume_nil", doc=suspend_resume_nil.__doc__)
    s.api("willResume", slot="never")
    return s


@scenario
def suspend_failed_resume_then_map():
    """A failed rvg_device_will_resume leaves the run state LeavingSuspendEarly
    (3); the next guest-memory mapping asserts: a root page write kills the
    host."""
    s = Scenario("suspend_failed_resume_then_map", doc=suspend_failed_resume_then_map.__doc__)
    s.w(abi.R_VERSION, abi.MAX_VERSION)
    will_resume(s, blob(state(ports=3)), "port count 3")
    s.w(abi.R_ROOT_PAGE, s.pfn(s.root_page))
    s.events("after")
    return s


# ---- the capability check ----

# Keys checked for alignment: the host's must not exceed the guest's
# (checkCapabilities, PG_CHECK_ALIGNMENT and key 40).
ALIGN_KEYS = (13, 15, 32, 40)


@scenario(host=True)
def suspend_capabilities():
    """The capability check for crafted device-info pairs, one key a blob:
    a guest value of 0xffffffff fails the key's defined, value, flags or
    rounding-mode check (IncompatibleDevice, 4), and an alignment key fails
    at 0; keys 38 and 39, which the host never writes, fail as undefined. At
    binary version 2 the host writes fewer keys, and a guest's key 14 is
    undefined there. A list of every key at 0, except the alignments at
    0xffffffff, passes. Which checks pass depends on the host's values."""
    s = Scenario("suspend_capabilities", host=True, doc=suspend_capabilities.__doc__)
    keys = [k for k in range(1, 45) if k != 43]
    for k in keys:
        will_resume(s, blob(state(), [(k, 0xFFFFFFFF)]), "key %d = 0xffffffff" % k)
    for k in ALIGN_KEYS:
        will_resume(s, blob(state(), [(k, 0)]), "key %d = 0" % k)
    for k in (14, 17, 19, 21, 23, 36):
        will_resume(s, blob(state(version=2), [(k, 0)]), "key %d at version 2" % k)
    for k in (19, 29, 33, 37, 42, 44):
        will_resume(s, blob(state(version=62), [(k, 0)]), "key %d at version 62" % k)
    ok = [(k, 0xFFFFFFFF if k in ALIGN_KEYS else 0) for k in keys if k not in (38, 39)]
    will_resume(s, blob(state(), ok), "every key but 38 and 39, passing")
    will_resume(s, blob(state(), [(18, 0x20007)]), "shader version 2.7")
    will_resume(s, blob(state(), [(18, 0x20008)]), "shader version 2.8")
    will_resume(s, blob(state(), [(41, 1)]), "rounding modes 1")
    will_resume(s, blob(state(), [(41, 8)]), "rounding modes 8")
    return s


@scenario
def suspend_capabilities_key_43():
    """Device-info key 43 is no key a resume reads: the blob is refused
    (InvalidContent, 2) before the capability check, whose reader has no
    case for it. The host lives on; the name is kept from when it died
    there."""
    s = Scenario("suspend_capabilities_key_43", doc=suspend_capabilities_key_43.__doc__)
    will_resume(s, blob(state(), [(43, 0)]), "key 43")
    return s


# ---- round trips ----

def _roundtrip(name, doc, do_suspend, **kw):
    """A guest with a root FIFO, a child FIFO, a GPU task and a connected
    display, suspended and resumed on a fresh device (or not, with marks
    in the same places so that later op indices agree), then driven on."""
    s = Scenario(name, displays=[display(0)], doc=doc, **kw)
    g = DisplayGuest(s, pages=16)
    t0 = g.t0
    c = ChildFIFO(s, 1)
    c.setup_node()
    c.define()
    t = GPUTask(s, tid=1)
    a, a_va = t.buffer(contents=bytes(range(256)))
    b, b_va = t.buffer()
    t.define()
    t.install()
    glyph = struct.pack("<I", 0x80402010) * 4
    t0.write(8 * PAGE, glyph)
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.submit(abi.OP_DISPLAY_CURSOR_GLYPH, p_glyph(0, 8 * PAGE, 16, 8, 2, 2, 1, 1, 0))
    s.submit(abi.OP_DISPLAY_CURSOR_SHOW, p_show(0, 1))
    s.wait(cb="cursorShow")
    s.events("before")
    s.diff("before")
    if do_suspend:
        suspend(s)
        s.diff("suspended")
        resume_fresh(s)
        s.wait(cb="cursorShow", cb_count=2)
        s.events("resumed")
    else:
        for i in range(8):
            s.mark("no suspend %d" % i)
    s.sweep(0x1000, 0x1040, label="registers")
    s.submit(abi.OP_NOP)
    c.submit(abi.OP_NOP)
    fill = struct.pack("<IQQB3x", b, 0, 0x40, 0xCD)
    copy = struct.pack("<IIQQQ", a, b, 0, 0x40, 0x40)
    t.exec3([t.cmdbuf(seg(abi.ENC_BLIT, rec(306, fill) + rec(301, copy)))], resources=[(a, 5), (b, 5)])
    t.dump(b_va, 0x80, "buffer B")
    s.api("cursorPosition", display=0)
    s.submit(abi.OP_DISPLAY_ACK, struct.pack("<II", 0, 1), fault=True, label="ack with nothing pending")
    s.resume_root()
    s.submit(abi.OP_NOP)
    s.events("after")
    s.diff("after", gpa=s.pa(0), length=s._next_page * PAGE)
    return s


@scenario
def suspend_roundtrip():
    """Suspend and resume on a fresh device mid-scenario, and the same
    scenario unsuspended: after the resume checkpoint, every observation of
    the two (their op indices agree) is the same, but for the colour matrix
    force-read byte (+0xa71) the restore sets. Resume rebuilds the root FIFO,
    child FIFO 1, tasks 0 and 1 (from the root words and each task root's
    length at +0x14) and the display (from root word 0x900), replays the
    cursor glyph and show, and raises one interrupt. In the blocks setup the
    host's own task numbers keep counting across the recreated device, so its
    block calls name other tasks."""
    return [_roundtrip("suspend_roundtrip", suspend_roundtrip.__doc__, True),
            _roundtrip("suspend_roundtrip_control", suspend_roundtrip.__doc__, False)]


@scenario
def suspend_writeback():
    """rvg_device_finish_suspend flushes a task's dirty GPU resources into
    guest memory before tearing tasks down (W14): a private buffer filled by a
    blit and never synchronized reaches guest memory only at
    rvg_device_finish_suspend."""
    s = Scenario("suspend_writeback", doc=suspend_writeback.__doc__)
    s.bring_up()
    t = GPUTask(s, tid=1)
    b, b_va = t.buffer(flags=1)
    t.define()
    t.install()
    fill = struct.pack("<IQQB3x", b, 0, 0x100, 0xAB)
    t.exec3([t.cmdbuf(seg(abi.ENC_BLIT, rec(306, fill)))], resources=[(b, abi.RES_INVALIDATE_GUEST)])
    t.dump(b_va, 0x100, "private buffer before suspend")
    suspend(s)
    t.dump(b_va, 0x100, "private buffer after finishSuspend")
    s.dump(s.pa(s.root_page, 4), 4, "root word of task 1")
    s.events("end")
    return s


@scenario
def suspend_in_place():
    """rvg_device_will_suspend, rvg_device_finish_suspend,
    rvg_device_will_resume and rvg_device_did_resume on the same device:
    rvg_device_did_resume re-writes the root page, whose teardown finds the
    FIFO still marked enabled, tears it down and clears that mark; the root
    FIFO is then not recreated (0x1000 reads 0) and a later doorbell is never
    answered. The guest display rvg_device_finish_suspend paused is torn down
    and the page restored."""
    s = Scenario("suspend_in_place", displays=[display(0)], doc=suspend_in_place.__doc__)
    g = DisplayGuest(s)
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.events("before")
    suspend(s)
    s.api("willResume")
    s.api("didResume")
    s.events("resumed")
    g.diff_page(0, "page")
    s.sweep(0x1000, 0x1040, label="registers")
    s.submit(abi.OP_NOP, timeout_ms=500, label="a NOP after the in-place resume")
    s.events("end")
    return s


@scenario
def suspend_resume_faults():
    """A root FIFO faulted at suspend resumes faulted: key 19 carries its
    fault offset and 0x102c reads 1 after rvg_device_did_resume; a child FIFO
    faulted at suspend also counts in the fault mask. 0x1024 and 0x1028 resume
    them."""
    s = Scenario("suspend_resume_faults", doc=suspend_resume_faults.__doc__)
    f = s.bring_up()
    c = ChildFIFO(s, 1)
    c.setup_node()
    c.define()
    s.submit(99, fault=True)
    c.submit(99, fault=True)
    s.fault_state("faulted")
    s.dump(c.node_gpa(), 0x14, "child node")
    s.events("faulted")
    suspend(s)
    resume_fresh(s)
    s.events("resumed")
    s.fault_state("resumed")
    s.resume_root()
    s.w(abi.R_RESUME_CHILD, 1)
    s.submit(abi.OP_NOP)
    c.submit(abi.OP_NOP)
    s.fault_state("after resuming both")
    s.events("end")
    return s


@scenario(host=True)
def suspend_crafted_resume():
    """A crafted blob resumed on a fresh device: no device-info pairs (max
    key 45), and the root FIFO healthy (fault offset 0xffffffff); the FIFO
    continues from the blob's read and written counts. The blob the suspend
    after it writes holds the host's device info, so the case is keyed by
    host: key 26, supportsMemoryOrderAtomics, is 0 on the M3 Max at macOS
    26.7 and 1 at 27.0.1 (measured)."""
    s = Scenario("suspend_crafted_resume", host=True, doc=suspend_crafted_resume.__doc__)
    f = s.bring_up()
    s.submit(abi.OP_NOP)
    s.submit(abi.OP_NOP)
    s.events("before")
    data = blob([(1, s.pfn(s.root_page)), (2, 0xF5), (3, s.pfn(f.page)), (4, 1), (5, f.length), (6, f.written),
                 (7, f.written), (8, f.start), (18, 1), (19, 0xFFFFFFFF)])
    s.api("recreate")
    will_resume(s, data, "crafted")
    s.api("didResume")
    s.events("resumed")
    s.fault_state("resumed")
    s.submit(abi.OP_NOP)
    s.events("a NOP")
    suspend(s)
    return s


@scenario
def suspend_resume_bad_root_words():
    """rvg_device_did_resume with root words that do not describe valid
    objects: a task word whose task root has a page-table root of 0 fails to
    create a task (logged, skipped), a display word naming a page outside guest
    RAM fails the guest display (logged, skipped), and a root page that does
    not map leaves no root; a FIFO enabled in the blob is then not recreated.
    None of these stops the resume."""
    out = []
    s = Scenario("suspend_resume_bad_root_words", descriptor={"port_count": 2}, displays=[display(0)],
                 doc=suspend_resume_bad_root_words.__doc__)
    root = s.root_page
    troot = s.alloc_page()
    s.mem(s.pa(troot), abi.TASK_ROOT.pack(0, 1, 0, 0, PAGE, 0, 0))
    s.u32(s.pa(root, 4 * 3), s.pfn(troot))
    s.u32(s.pa(root, abi.ROOT_DISPLAY_WORD), 0x10)
    s.u32(s.pa(root, abi.ROOT_DISPLAY_WORD + 4), s.pfn(s.alloc_page()))
    will_resume(s, blob(state(ports=2, root=s.pfn(root))), "resume")
    s.api("didResume")
    s.events("resumed")
    s.r(abi.R_ROOT_PAGE)
    s.diff("guest memory")
    out.append(s)
    s = Scenario("suspend_resume_unmapped_root", doc=suspend_resume_bad_root_words.__doc__)
    will_resume(s, blob(state(root=0x10, fifo_enabled=1, fifo_base=0x11, fifo_length=PAGE, fifo_start=0x400)),
                "resume")
    s.api("didResume")
    s.events("resumed")
    s.sweep(0x1000, 0x1040, label="registers")
    out.append(s)
    return out


@scenario
def suspend_resume_bad_fifos():
    """rvg_device_did_resume restores only child FIFO nodes whose running word
    (+0xe) is nonzero. Nodes 1 and 2 hold what would fail (node 1 a stamp index
    of 100, past the device's 64 FIFOs; node 2 a page list page outside guest
    RAM) but have running 0, so the resume never tries them and waking them
    does nothing. suspend_gap_resume_child_nodes locks the same nodes with
    running 1."""
    s = Scenario("suspend_resume_bad_fifos", doc=suspend_resume_bad_fifos.__doc__)
    root = s.root_page
    node = lambda fid, field=0: s.pa(root, abi.ROOT_CHILD_NODE + abi.CHILD_NODE_SIZE * (fid - 1) + field)
    s.mem(node(1), struct.pack("<IIIHHI", 0, 0, 0xFFFFFFFF, 100, 0, s.pfn(s.alloc_page())))
    s.mem(node(2), struct.pack("<IIIHHI", 0, 0, 0xFFFFFFFF, 2, 0, 0x3FFFFF))
    will_resume(s, blob(state(root=s.pfn(root))), "resume")
    s.api("didResume")
    s.events("resumed")
    s.fault_state("resumed")
    s.dump(node(1), 2 * abi.CHILD_NODE_SIZE, "nodes")
    s.w(abi.R_WAKE_CHILD, 1)
    s.w(abi.R_WAKE_CHILD, 2)
    s.quiesce()
    s.events("woken")
    return s


@scenario
def suspend_resume_bad_root_fifo():
    """A blob whose root FIFO is enabled but cannot be created (a length that
    is not a page multiple, "FIFO length not multiple of page size") kills
    the host: rvg_device_did_resume asserts that the root FIFO exists."""
    s = Scenario("suspend_resume_bad_root_fifo", doc=suspend_resume_bad_root_fifo.__doc__)
    will_resume(s, blob(state(root=s.pfn(s.root_page), fifo_enabled=1, fifo_base=s.pfn(s.alloc_page()),
                            fifo_length=0x1000, fifo_start=0x400)), "resume")
    s.api("didResume")
    s.events("after")
    return s


@scenario(host=True)
def suspend_resume_efi():
    """EFI enabled at suspend is replayed first by rvg_device_did_resume (keys
    12-17): the fresh device's EFI display binds the display parked at port 0
    and presents the guest's framebuffer again."""
    s = Scenario("suspend_resume_efi", host=True, displays=[display(0)], descriptor={"trace": True},
                 doc=suspend_resume_efi.__doc__)
    s.w(abi.R_VERSION, abi.MAX_VERSION)
    e = EFI(s)
    e.write_frame(16, 8, 64, seed=9)
    e.setup(16, 8, 64)
    s.wait(cb="newFrame")
    s.events("presented")
    suspend(s)
    resume_fresh(s)
    s.wait(cb="newFrame", cb_count=2)
    s.events("resumed")
    e.regs("resumed")
    s.encode(0, 16, 8, label="after resume")
    return s


# ---- the display restore ----

@scenario
def suspend_restore_present7():
    """A display last presented with opcode 7 restores through the
    mapped-surface path only: presentedMappingID is 0, "Invalid mappingID" is
    thrown and caught, which skips the state machine call after it. The frame
    is not presented again; the guest's page keeps its state."""
    s = Scenario("suspend_restore_present7", displays=[display(0)], doc=suspend_restore_present7.__doc__)
    g = DisplayGuest(s)
    r, va = g.backing(16, 8, pixels=pattern(16, 8))
    g.install()
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.submit(abi.OP_DISPLAY_TRANSACTION3, p_present3(0, r))
    s.wait(cb="newFrame")
    s.events("presented")
    suspend(s)
    g.diff_page(0, "suspended")
    resume_fresh(s)
    s.quiesce(100)
    s.events("resumed")
    g.diff_page(0, "resumed")
    s.api("present_counts", display=0)
    s.api("set_modes", display=0, modes=[[32, 16, 60]])
    s.events("a new mode list")
    g.diff_page(0, "new modes")
    return s


@scenario
def suspend_restore_asleep():
    """A display asleep at suspend restores asleep (no call a hypervisor
    sees), and the state machine runs: a mode list that differs from the
    page's raises 12 at once."""
    s = Scenario("suspend_restore_asleep", displays=[display(0)], doc=suspend_restore_asleep.__doc__)
    g = DisplayGuest(s, task0=False)
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.submit(abi.OP_DISPLAY_SLEEP_STATE, p_sleep(0, 1))
    s.events("asleep")
    suspend(s)
    s.api("recreate", config={"op": "config", "displays": [display(0, modes=[[128, 64, 30]])]})
    s.api("willResume")
    s.api("didResume")
    s.events("resumed")
    g.diff_page(0, "resumed")
    return s


@scenario(setups=("map", "isolate"), host=True)
def suspend_restore_mapped():
    """A display last presented with opcode 8 is re-presented by the restore
    from its mapping ID: the IOSurface host device (process-wide, not part of
    the device) still holds the surface, so the new display gets mode_change
    and a frame. Map and isolate only: the mapper needs a memory map."""
    s = Scenario("suspend_restore_mapped", setups=("map", "isolate"), host=True, iosfc=True,
                 displays=[display(0)], doc=suspend_restore_mapped.__doc__)
    g = DisplayGuest(s)
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    d = IOSFC(s, v2=True)
    d.start()
    root, data = d.surface_pages(1)
    s.mem(s.pa(data), pattern(16, 8, 5))
    d.put_desc(1, desc_v2(root, 16, 8))
    d.submit((CMD_MAP, 1))
    s.submit(abi.OP_DISPLAY_SWAP_MAPPING, p_swap(0, 1))
    s.wait(cb="newFrame")
    s.events("presented")
    suspend(s)
    resume_fresh(s)
    s.wait(cb="newFrame", cb_count=2)
    s.events("resumed")
    g.diff_page(0, "resumed")
    s.encode(0, 16, 8, label="restored frame")
    return s


# ---- pause, unpause, stop ----

@scenario
def suspend_pause():
    """pause quiesces and drains the FIFOs, pauses the guest displays and
    suppresses interrupts: stamps still land in guest memory and set 0x1018
    bits, but raise nothing; unpause restarts the FIFOs and raises exactly
    one interrupt. A display page while paused: mode lists do not run the
    machine until unpause."""
    s = Scenario("suspend_pause", displays=[display(0)], doc=suspend_pause.__doc__)
    g = DisplayGuest(s, task0=False)
    f = g.s.fifo
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.events("before")
    s.api("pause")
    st = f.cmd(abi.OP_NOP)
    f.doorbell()
    f.wait_stamp(st, timeout_ms=500, label="a NOP while paused")
    s.api("set_modes", display=0, modes=[[32, 16, 60]])
    s.quiesce(100)
    s.events("paused")
    g.diff_page(0, "paused")
    s.api("unpause")
    f.wait_stamp(st)
    s.events("unpaused")
    s.r(abi.R_IRQ_STAMP)
    s.r(abi.R_IRQ_DISPLAY)
    g.diff_page(0, "unpaused")
    s.api("pause", label="again")
    s.api("unpause", label="again")
    s.events("second pause")
    return s


@scenario
def suspend_pause_no_display():
    """Pause and unpause with no display page: nothing asserts, and unpause
    without a pause, or two pauses, are harmless when no guest display
    exists."""
    s = Scenario("suspend_pause_no_display", doc=suspend_pause_no_display.__doc__)
    s.bring_up()
    s.api("unpause", label="without a pause")
    s.api("pause")
    s.api("pause", label="twice")
    s.api("unpause")
    s.wait(irqs=2)
    s.submit(abi.OP_NOP)
    s.events("end")
    return s


@scenario
def suspend_host_asserts():
    """Host-API misuse with a display page connected, each an assert and a
    host death at that call: a second pause, unpause without pause,
    rvg_device_finish_suspend without rvg_device_will_suspend, and while
    paused, rvg_display_cursor_position and the doorbell (both read the guest
    display's page, which asserts it is not paused)."""
    out = []
    for name, ops in (("pause_twice", [("api", "pause"), ("api", "pause")]),
                      ("unpause_unpaused", [("api", "unpause")]),
                      ("finish_without_will", [("api", "finishSuspend")]),
                      ("cursor_while_paused", [("api", "pause"), ("cursor", None)]),
                      ("doorbell_while_paused", [("api", "pause"), ("doorbell", None)])):
        s = Scenario("suspend_host_asserts_" + name, displays=[display(0)], doc=suspend_host_asserts.__doc__)
        g = DisplayGuest(s, task0=False)
        g.connect(0)
        s.events("connected")
        for kind, arg in ops:
            if kind == "api":
                s.api(arg)
            elif kind == "cursor":
                s.api("cursorPosition", display=0)
            else:
                s.w(abi.R_DISPLAY_DOORBELL, 0)
        s.events("after")
        out.append(s)
    return out


@scenario
def suspend_stop():
    """stop writes root page 0: FIFOs, tasks and display pages are torn down
    (root words cleared only while running), and the device takes a new root
    page and FIFO afterwards."""
    s = Scenario("suspend_stop", displays=[display(0)], doc=suspend_stop.__doc__)
    g = DisplayGuest(s)
    g.connect(0, enabled=0xF)
    g.ack(0, 1)
    s.events("before")
    s.api("stop")
    s.events("stopped")
    s.sweep(0x1000, 0x1040, label="registers")
    s.diff("guest memory")
    s.api("cursorPosition", display=0)
    s.w(abi.R_ROOT_PAGE, s.pfn(s.root_page))
    s.fifo.enable()
    s.fifo.written = 0
    s.submit(abi.OP_NOP)
    s.events("a new root")
    return s
