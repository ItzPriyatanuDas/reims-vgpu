#!/usr/bin/env python3
"""Checks apple-gfx-iosurface-pci through QEMU's qtest accelerator, with no guest.

usage: scripts/internal/qtest_iosurface_pci.py QEMU [--ram-mib N]

QEMU is an x86-64 qemu-system-x86_64 built from vendor/qemu on this library
(make qemu does aarch64 only; on Linux, scripts/install-ctl.sh's configure).
Each run makes the library's device, so it needs a Vulkan device the library
takes. No check needs a real GPU: lavapipe does (VK_ICD_FILENAMES naming its
ICD), with REIMS_VGPU_WINDOW=0 on a headless host. The script writes PCI configuration and registers itself, as
firmware and the guest's drivers would, and prints one line per check:

- realize: without a gpu link the device takes the machine's only
  apple-gfx-pci, and refuses when there is none or more than one; it refuses a
  second instance; apple-gfx-pci has no use-iosurface-mapper or
  process-isolation property;
- apple-gfx-pci's ID: 106b:eeee, in its configuration and in its option
  ROM's PCI data structure;
- identity: 106b:eeef, class 0x0880, no interrupt pin, an MSI capability, and
  BAR 0 alone, 64 KiB of 32-bit memory;
- registers: 0x1028 offers no second descriptor form on an x86-64 host;
  8-byte accesses pass whole (a base above 4 GiB, all ones off the
  registers); a 4-byte access reads the low half;
- the ring: commands written to a ring above 4 GiB complete, and each
  completion notifies the MSI vector while the guest has MSI and bus mastering
  on, and not otherwise. The MSI address points into RAM, where the message's
  write lands (qtest has no interrupt controller to take it);
- reset: a machine reset (0xcf9) returns the ring base, the descriptor table
  base, the descriptor enable and both counters to 0, and leaves the ring size,
  which the library keeps when written 0.

It exits 0 when every check passes.
"""

import argparse
import os
import select
import subprocess
import sys
import time

MAPPER_SLOT = 6
GPU_SLOT = 4
BAR = 0xC000_0000  # in q35's PCI hole, above its MMCONFIG window
RAM_HIGH = 0x1_0000_0000  # q35's RAM above 4 GiB starts here
RING = RAM_HIGH + 0x10000
TABLE = RAM_HIGH + 0x20000
MSI_WORD = 0x1000_0000  # RAM below 4 GiB, where the MSI's write lands
GPU_BAR0 = 0xC010_0000  # apple-gfx-pci's 16 KiB of registers
GPU_ROM = 0xC020_0000  # its option ROM
GPU_BAR2 = 0xC400_0000  # its 64 MiB EFI framebuffer window

REG_RING_BASE = 0x1000
REG_RING_SIZE = 0x1008
REG_DESCRIPTOR_BASE = 0x1010
REG_WR_COUNTER = 0x1018
REG_RD_COUNTER = 0x1020
REG_FEATURES = 0x1028
REG_DESC_ENABLE = 0x1030
UNSUPPORTED = 0xE00002C7  # kIOReturnUnsupported, a command the ring does not know

failures = []


def check(name, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'} {name}{': ' + detail if detail else ''}", flush=True)
    if not ok:
        failures.append(name)


def base_args(qemu, ram_mib):
    return [
        qemu,
        "-machine", "q35,memory-backend=ram",
        "-object", f"memory-backend-memfd,id=ram,size={ram_mib}M,share=on",
        "-m", f"{ram_mib}M",
        "-accel", "qtest",
        "-display", "none",
        "-nodefaults",
    ]


def gpu(extra="", n=0):
    return ["-device", f"apple-gfx-pci,id=gpu{n},bus=pcie.0,addr={GPU_SLOT + n:#x}{extra}"]


def mapper(extra=",gpu=gpu0", slot=MAPPER_SLOT):
    return ["-device", f"apple-gfx-iosurface-pci,bus=pcie.0,addr={slot:#x}{extra}"]


def refused(qemu, ram_mib, devices, needle):
    """The realize fails, saying `needle`."""
    p = subprocess.run(
        base_args(qemu, ram_mib) + ["-qtest", "null"] + devices,
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120,
    )
    return p.returncode != 0 and needle in p.stderr, p.stderr.strip().splitlines()[-1:]


class Qtest:
    """QEMU under -qtest stdio: one command a line, one reply a line."""

    def __init__(self, args):
        self.p = subprocess.Popen(
            args + ["-qtest", "stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            text=True, bufsize=1,
        )

    def cmd(self, line):
        self.p.stdin.write(line + "\n")
        self.p.stdin.flush()
        while True:
            ready, _, _ = select.select([self.p.stdout], [], [], 60)
            if not ready:
                raise RuntimeError(f"no reply to {line!r}")
            reply = self.p.stdout.readline()
            if not reply:
                raise RuntimeError(f"QEMU ended at {line!r} (exit {self.p.wait()})")
            if reply.startswith("OK") or reply.startswith("FAIL") or reply.startswith("ERR"):
                break
        if not reply.startswith("OK"):
            raise RuntimeError(f"{line!r}: {reply.strip()}")
        parts = reply.split()
        return int(parts[1], 0) if len(parts) > 1 else None

    def readq(self, a):
        return self.cmd(f"readq {a:#x}")

    def readl(self, a):
        return self.cmd(f"readl {a:#x}")

    def writeq(self, a, v):
        self.cmd(f"writeq {a:#x} {v:#x}")

    def writel(self, a, v):
        self.cmd(f"writel {a:#x} {v:#x}")

    def cfg_addr(self, slot, reg):
        return 0x8000_0000 | (slot << 11) | (reg & 0xFC)

    def cfg_read(self, slot, reg, size=4):
        self.cmd(f"outl 0xcf8 {self.cfg_addr(slot, reg):#x}")
        port = 0xCFC + (reg & 3)
        op = {1: "inb", 2: "inw", 4: "inl"}[size]
        return self.cmd(f"{op} {port:#x}")

    def readw(self, a):
        return self.cmd(f"readw {a:#x}")

    def cfg_write(self, slot, reg, value, size=4):
        self.cmd(f"outl 0xcf8 {self.cfg_addr(slot, reg):#x}")
        port = 0xCFC + (reg & 3)
        op = {1: "outb", 2: "outw", 4: "outl"}[size]
        self.cmd(f"{op} {port:#x} {value:#x}")

    def poll(self, read, want, seconds=5.0):
        end = time.monotonic() + seconds
        while True:
            v = read()
            if v == want or time.monotonic() > end:
                return v
            time.sleep(0.02)

    def quit(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()


def gpu_identity(q):
    """apple-gfx-pci's ID, 106b:eeee, in its configuration and in its ROM's PCI data structure."""
    g = GPU_SLOT
    want = 0xEEEE
    ids = q.cfg_read(g, 0)
    check(f"apple-gfx-pci is 106b:{want:04x}", ids == (want << 16 | 0x106B), f"{ids:#010x}")
    q.cfg_write(g, 0x10, GPU_BAR0)
    q.cfg_write(g, 0x18, GPU_BAR2)
    q.cfg_write(g, 0x30, GPU_ROM | 1)  # the ROM's address and its enable bit
    q.cfg_write(g, 0x04, 0x2, 2)  # memory space
    magic = q.readw(GPU_ROM)
    pcir = q.readw(GPU_ROM + 0x18)
    sig = q.readl(GPU_ROM + pcir)
    rom_ids = q.readl(GPU_ROM + pcir + 4)
    check(f"its option ROM names 106b:{want:04x}",
          magic == 0xAA55 and sig == 0x52494350 and rom_ids == (want << 16 | 0x106B),
          f"magic {magic:#x}, PCIR at {pcir:#x}: {sig:#x} {rom_ids:#010x}")
    q.cfg_write(g, 0x04, 0x0, 2)
    q.cfg_write(g, 0x30, 0)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("qemu")
    ap.add_argument("--ram-mib", type=int, default=5120,
                    help="the machine's RAM; above 2 GiB q35 puts the rest above 4 GiB")
    a = ap.parse_args()
    qemu, ram = a.qemu, a.ram_mib

    # ---- realize
    ok, why = refused(qemu, ram, mapper(extra=""), "has no apple-gfx-pci")
    check("realize refused without a gpu link and no apple-gfx-pci", ok, " ".join(why))
    ok, why = refused(qemu, ram, gpu() + gpu(n=1) + mapper(extra=""),
                      "more than one apple-gfx-pci")
    check("realize refused without a gpu link beside two apple-gfx-pci", ok, " ".join(why))
    ok, why = refused(qemu, ram, gpu() + mapper() + mapper(slot=MAPPER_SLOT + 1),
                      "only one per machine")
    check("realize refused for a second instance", ok, " ".join(why))
    h = subprocess.run([qemu, "-device", "apple-gfx-pci,help"], capture_output=True,
                       text=True, timeout=60).stdout
    gone = [p for p in ("use-iosurface-mapper", "process-isolation")
            if any(l.strip().startswith(p + "=") for l in h.splitlines())]
    check("apple-gfx-pci has no mapper or isolation property", not gone, " ".join(gone))

    # ---- the device, with no gpu link: the machine's only apple-gfx-pci
    q = Qtest(base_args(qemu, ram) + gpu() + mapper(extra=""))
    try:
        gpu_identity(q)
        s = MAPPER_SLOT
        check("without a gpu link it realizes beside the only apple-gfx-pci",
              q.cfg_read(s, 0) == 0xEEEF106B, f"{q.cfg_read(s, 0):#010x}")
        check("vendor:device 106b:eeef", q.cfg_read(s, 0) == 0xEEEF106B, f"{q.cfg_read(s, 0):#010x}")
        cls = q.cfg_read(s, 8) >> 8
        check("class 0x0880", cls == 0x088000, f"{cls:#08x}")
        pin = q.cfg_read(s, 0x3D, 1)
        check("no interrupt pin", pin == 0, f"pin {pin}")
        caps, ptr = [], q.cfg_read(s, 0x34, 1)
        while ptr and len(caps) < 16:
            caps.append((q.cfg_read(s, ptr, 1), ptr))
            ptr = q.cfg_read(s, ptr + 1, 1)
        msi = next((at for cid, at in caps if cid == 0x05), None)
        check("an MSI capability", msi is not None, f"capabilities {[hex(c) for c, _ in caps]}")
        q.cfg_write(s, 0x10, 0xFFFF_FFFF)
        sized = q.cfg_read(s, 0x10)
        check("BAR 0: 64 KiB of 32-bit memory", sized == 0xFFFF_0000, f"{sized:#010x}")
        others = []
        for reg in (0x14, 0x18, 0x1C, 0x20, 0x24, 0x30):
            q.cfg_write(s, reg, 0xFFFF_FFFF)
            others.append(q.cfg_read(s, reg))
        check("no other BAR, no ROM", all(v == 0 for v in others), str([hex(v) for v in others]))
        q.cfg_write(s, 0x10, BAR)
        q.cfg_write(s, 0x04, 0x6, 2)  # memory space and bus master
        cmdreg = q.cfg_read(s, 0x04, 2)
        check("the command register takes memory space and bus master", cmdreg & 0x6 == 0x6,
              f"{cmdreg:#06x}")

        # ---- registers
        feats = q.readq(BAR + REG_FEATURES)
        check("0x1028 offers no second descriptor form", feats == 0, f"{feats:#x}")
        off = q.readq(BAR + 0x1038)
        check("an 8-byte read off the registers is all ones", off == 2**64 - 1, f"{off:#x}")
        q.writeq(BAR + REG_RING_BASE, RING)
        v = q.readq(BAR + REG_RING_BASE)
        check("an 8-byte ring base above 4 GiB reads back whole", v == RING, f"{v:#x}")
        lo, hi = q.readl(BAR + REG_RING_BASE), q.readl(BAR + REG_RING_BASE + 4)
        check("a 4-byte read takes the low half", lo == RING & 0xFFFF_FFFF and hi == 0xFFFF_FFFF,
              f"{lo:#x} {hi:#x}")
        q.writeq(BAR + REG_DESCRIPTOR_BASE, TABLE)
        v = q.readq(BAR + REG_DESCRIPTOR_BASE)
        check("an 8-byte table base above 4 GiB reads back whole", v == TABLE, f"{v:#x}")
        q.writeq(BAR + REG_RING_SIZE, 256)  # one 4 KiB page of 16-byte commands
        check("a ring of 256 commands", q.readq(BAR + REG_RING_SIZE) == 256)

        # ---- the ring and its MSI
        ctl = q.cfg_read(s, msi + 2, 2)
        data_at = msi + (0xC if ctl & 0x80 else 0x8)
        q.cfg_write(s, msi + 4, MSI_WORD)
        if ctl & 0x80:
            q.cfg_write(s, msi + 8, 0)
        expected = 0

        def command(index, data, msi_on, master):
            nonlocal expected
            q.cfg_write(s, data_at, data, 2)
            q.cfg_write(s, msi + 2, (ctl | 1) if msi_on else (ctl & ~1), 2)
            q.cfg_write(s, 0x04, 0x6 if master else 0x2, 2)
            q.writel(MSI_WORD, 0)
            entry = RING + 16 * index
            q.writel(entry, 99)  # no such command
            q.writel(entry + 4, 0)
            q.writel(entry + 8, 0x5555)
            expected += 1
            q.writeq(BAR + REG_WR_COUNTER, expected)
            done = q.poll(lambda: q.readq(BAR + REG_RD_COUNTER), expected)
            status = q.readl(entry + 8)
            word = q.poll(lambda: q.readl(MSI_WORD), data if msi_on and master else -1, 1.0)
            return done, status, word

        done, status, word = command(0, 0xABCD, True, True)
        check("a command completes", done == 1 and status == UNSUPPORTED,
              f"read counter {done}, status {status:#x}")
        check("its completion notifies the MSI", word == 0xABCD, f"{word:#x}")
        done, status, word = command(1, 0xABCE, True, True)
        check("a second command notifies it again", done == 2 and word == 0xABCE,
              f"read counter {done}, {word:#x}")
        done, status, word = command(2, 0xABCF, False, True)
        check("no MSI while MSI is off", done == 3 and word == 0, f"{word:#x}")
        done, status, word = command(3, 0xABD0, True, False)
        check("no MSI while bus mastering is off", done == 4 and word == 0, f"{word:#x}")

        # ---- reset
        q.writeq(BAR + REG_DESC_ENABLE, 1)
        q.writeq(BAR + REG_DESCRIPTOR_BASE, TABLE)
        q.cmd("outb 0xcf9 0x6")  # ICH9's reset control: a full reset
        q.poll(lambda: q.cfg_read(s, 0x10), 0)  # the reset clears BAR 0
        q.cfg_write(s, 0x10, BAR)
        q.cfg_write(s, 0x04, 0x2, 2)  # memory space
        regs = {name: q.readq(BAR + reg) for name, reg in (
            ("ring base", REG_RING_BASE), ("table base", REG_DESCRIPTOR_BASE),
            ("enable", REG_DESC_ENABLE), ("read counter", REG_RD_COUNTER),
            ("write counter", REG_WR_COUNTER))}
        check("a reset returns the registers to 0", all(v == 0 for v in regs.values()),
              ", ".join(f"{k} {v:#x}" for k, v in regs.items()))
        size = q.readq(BAR + REG_RING_SIZE)
        check("a reset keeps the ring size", size == 256, f"{size}")
    finally:
        q.quit()

    print(f"{len(failures)} failed" if failures else "all passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
