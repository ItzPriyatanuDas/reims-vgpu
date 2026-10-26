#!/usr/bin/env python3
"""Boot the vmapple guest fresh and drive it through a fixed tour.

    scripts/internal/vm_drive.py run --out DIR [--label L] [--vm VM_DIR]
                                     [--clone DIR] [--qemu PATH] [--no-tour] [--perf]
    scripts/internal/vm_drive.py pair --out-base DIR --index I [--vm VM_DIR]
                                      [--perf] LABEL[=QEMU] LABEL[=QEMU]
    scripts/internal/vm_drive.py compare DIR_A DIR_B
    scripts/internal/vm_drive.py summary DIR...
    scripts/internal/vm_drive.py perf DIR...
    scripts/internal/vm_drive.py logdiff DIR...

QEMU runs on the reims-vgpu this repository builds
(scripts/internal/run_vmapple.sh). A run's label (default: vm) names it in
result.json; `summary`, `perf` and `logdiff` tell runs apart by it. `pair`
boots two QEMU builds at once, each into DIR/LABEL-I (the two VMs do not
disturb each other): a change's QEMU against its predecessor's, given as
LABEL=<its qemu-system-aarch64>. QEMU defaults to make qemu's build. Runs
recorded before the label replaced the mode ("native", and qemu-native.log)
are still read.

`run` starts scripts/internal/run_vmapple.sh with FRESH=1 (so VM_DIR is only
cloned, never written) and waits for the desktop. The guest it is written
for (docs/qemu.md, "The guest") is macOS 13.6 set up with an account that
logs in automatically. A login window or Setup Assistant ends the boot: the
driver types no password and creates no account. Then a fixed tour: Keyboard
Setup Assistant quit, Launchpad, Maps zoomed out to the globe with a fixed
pan and zoom, Photos, Safari, System Settings > Displays with a mode switch
to 1920x1080 and back, a Finder window dragged, and Mission Control. It
quits QEMU through HMP at the end, or as soon as a step hangs.

Input goes the way docs/qemu.md measured it works: clicks as QMP
input-send-event absolute pointer events (the guest's USB tablet ignores
relative ones), keys as HMP sendkey, screendumps as QMP screendump. The
screen is read with Vision's text recognition
(scripts/internal/vm_ocr.swift, compiled once into
~/Library/Caches/vm_drive), and most clicks go to the text of the button
they press, so they do not depend on window placement. The Dock's icons are
clicked by position (DOCK).

Every step records how long it took, how many frames (apple_gfx_new_frame),
command doorbells (apple_gfx_write offset=0x1020 val=0x1) and interrupts
QEMU's trace counted in it, and a screendump under DIR/shots. A step that
waits for a transition and sees neither a screen change nor a new frame for
--hang-secs (default 180) is a hang: the driver saves `sample` of QEMU, the
last trace lines, the trace counts and a screendump under DIR/hang, and ends
the boot. DIR/result.json holds everything, including the crash reports of
QEMU's process, found by the PID in the report. QEMU's os_log lines, by PID,
go to DIR/qemu-oslog.txt and DIR/qemu-oslog.ndjson. They are those of the
system frameworks QEMU loads: the library logs through the `log` crate, and
writes none there. The library runs isolated tasks on threads of its own in
QEMU's process, so their work and their CPU are QEMU's.

`compare` diffs the screendumps of two runs step by step (pixels differing by
more than 16 in any channel below the menu bar) and reports Maps' grey
placeholder grid in the settled Maps frames (*-loaded) of each. `summary`
prints one line per run.

QEMU runs with -msg timestamp=on, so every trace line carries its time, and
-name ...,debug-threads=on, so its threads are named. `--perf` adds, after the
tour, bursts of interactive input (perf_tour: window drags and a resize,
Mission Control, Launchpad, Safari scrolling a long page served from this
host, Maps pans and zooms), each with its wall-clock times in result.json.
The run also records QEMU's rusage at exit, the CPU of its threads just
before quit (the library's among them, named reims-vgpu.*), and QEMU's `ps`
CPU every 2 s. `perf` reads each run's trace (analyse: doorbell-to-interrupt
latency, frame intervals in the bursts, host-silent stalls, MMIO round trips,
CPU) into DIR/perf.json and, given runs of two labels, compares them
(per-boot values with an exact Mann-Whitney test, pooled latencies with a
bootstrap over boots) into perf-compare.json next to the runs. `logdiff`
lists every distinct os_log line of QEMU that differs between the labels,
numbers, addresses and UUIDs masked, with its level and subsystem:category.
docs/qemu.md, "How the figures are defined", says what each figure means.
"""
import argparse
import collections
import ctypes
import datetime
import http.server
import itertools
import json
import math
import os
import re
import socket
import sys
import subprocess
import threading
import time
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
GUEST = Path('/Users/aneesiqbal/Projects/macos-gpu-re/guest')
CACHE = Path.home() / 'Library/Caches/vm_drive'
MENU_BAR = 26          # rows skipped by change detection and compare: the clock
CHANGE_PX = 150        # pixels differing by more than 24 that make a change

# Maps' placeholder for a tile it has no data for is a grid. On macOS 13.6
# its fill is cream (249, 245, 237) with fine lines of (236, 232, 224) and
# black meridians; on macOS 12 the fill is grey (223, 219, 212). grid_count
# counts pixels of MAP_BOX, the map area of the Maps window where it opens
# at 1440x1080, within 2 of any of them. At 85023009 the settled Maps frames
# whose map drew held 1 001 or fewer, and a globe drawn all in placeholder
# 284 097 (docs/qemu.md, "What the guest does on its own"). A gap is
# GRID_MIN or more.
MAP_BOX = (491, 116, 1231, 845)
GRID_RGB = ((249, 245, 237), (236, 232, 224), (223, 219, 212))
GRID_MIN = 5000

# The Dock at 1440x1080: the icons' centres, the same on every boot.
DOCK_Y = 1037
# Finder's window title, which Vision also reads as "<> Recents" or
# "›Recents" (with the back and forward chevrons), and the sidebar's as
# "• Recents", so a pattern anchored at both ends ('^Recents$') misses the
# window and stops the tour's Finder step.
RECENTS = r'Recents$'
DOCK = {'finder': 85, 'launchpad': 146, 'safari': 267, 'maps': 449,
        'photos': 511, 'settings': 1118}


# QEMU's -msg timestamp=on puts an ISO 8601 UTC time before every trace line
# (util/log.c, qemu_log_trylock_with_context), with microseconds unless
# they are 0.
TRACE_TS = re.compile(r'^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z ')


def log(msg):
    print(f'[{datetime.datetime.now():%H:%M:%S}] {msg}', flush=True)


def ocr_tool():
    exe = CACHE / 'vm_ocr'
    src = ROOT / 'scripts/internal/vm_ocr.swift'
    if not exe.exists() or exe.stat().st_mtime < src.stat().st_mtime:
        CACHE.mkdir(parents=True, exist_ok=True)
        subprocess.run(['swiftc', '-O', '-o', str(exe), str(src)], check=True)
    return exe


class _ThreadInfo(ctypes.Structure):
    """<sys/proc_info.h> struct proc_threadinfo: times in nanoseconds."""
    _fields_ = ([('user', ctypes.c_uint64), ('system', ctypes.c_uint64)]
                + [(n, ctypes.c_int32) for n in ('cpu_usage', 'policy', 'run_state', 'flags',
                                                 'sleep_time', 'curpri', 'priority', 'maxpriority')]
                + [('name', ctypes.c_char * 64)])


def thread_cpu(pid):
    """[(thread name, CPU seconds)] of pid's live threads (libproc
    PROC_PIDLISTTHREADS, PROC_PIDTHREADINFO). Threads that have exited are not
    listed: their time is only in the process's total."""
    lib = ctypes.CDLL('/usr/lib/libproc.dylib')
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p,
                                 ctypes.c_int]
    buf = (ctypes.c_uint64 * 8192)()
    n = lib.proc_pidinfo(pid, 6, 0, buf, ctypes.sizeof(buf))
    out = []
    for i in range(max(n, 0) // 8):
        ti = _ThreadInfo()
        if lib.proc_pidinfo(pid, 5, buf[i], ctypes.byref(ti), ctypes.sizeof(ti)) == ctypes.sizeof(ti):
            out.append((ti.name.decode(errors='replace'), (ti.user + ti.system) / 1e9))
    return out


def long_page():
    """The page Safari scrolls under --perf: 400 sections of text, each over a
    gradient block with a shadow, and an SVG of 40 circles every eighth one."""
    words = ('graphics fifo stamp doorbell interrupt texture buffer heap display frame '
             'present command queue fence shader pipeline surface metal guest host').split()
    parts = ['<!doctype html><html><head><meta charset="utf-8"><title>PERFPAGE</title><style>'
             'body{font:17px -apple-system,Helvetica,sans-serif;margin:0 48px;background:#f4f1ea}'
             '.b{height:140px;margin:18px 0;border-radius:22px;box-shadow:0 8px 24px rgba(0,0,0,.3)}'
             'h2{margin:30px 0 6px}</style></head><body><h1>PERFPAGE scroll test</h1>']
    for i in range(400):
        h = (i * 37) % 360
        text = ' '.join(words[(i * 7 + k * 3) % len(words)] for k in range(90))
        parts.append(f'<h2>Section {i}</h2><p>{text}</p><div class="b" style="background:'
                     f'linear-gradient(90deg,hsl({h},70%,55%),hsl({(h + 120) % 360},70%,45%))"></div>')
        if i % 8 == 0:
            circles = ''.join(f'<circle cx="{(k * 53) % 900 + 20}" cy="{(k * 29) % 160 + 20}" '
                              f'r="{10 + k % 25}" fill="hsl({(h + k * 9) % 360},80%,50%)" '
                              f'fill-opacity=".6"/>' for k in range(40))
            parts.append(f'<svg width="940" height="200">{circles}</svg>')
    parts.append('</body></html>')
    return '\n'.join(parts).encode()


def serve_page():
    """Serve long_page() on 127.0.0.1 at a free port, which the guest reaches
    as 10.0.2.2 through QEMU's user network. Returns the port."""
    page = long_page()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Content-Length', str(len(page)))
            self.end_headers()
            self.wfile.write(page)

        def log_message(self, *a):
            pass
    srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv.server_address[1]


def grid_count(img, box=MAP_BOX):
    """Pixels of `box` in the colours of Maps' placeholder grid."""
    a = np.asarray(img.convert('RGB')).astype(int)
    x0, y0, x1, y1 = box
    if a.shape[0] < y1 or a.shape[1] < x1:
        return 0
    a = a[y0:y1, x0:x1]
    return int(sum((np.abs(a - np.array(c)).max(axis=2) <= 2).sum() for c in GRID_RGB))


class QMP:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.settimeout(90)
        self.s.connect(path)
        self.f = self.s.makefile('rw')
        json.loads(self.f.readline())
        self.cmd('qmp_capabilities')

    def cmd(self, execute, **args):
        msg = {'execute': execute}
        if args:
            msg['arguments'] = args
        self.f.write(json.dumps(msg) + '\n')
        self.f.flush()
        while True:
            line = self.f.readline()
            if not line:
                raise EOFError('QMP closed')
            r = json.loads(line)
            if 'return' in r:
                return r['return']
            if 'error' in r:
                raise RuntimeError(f'{execute}: {r["error"]}')


class HMP:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.settimeout(30)
        self.s.connect(path)
        self._until_prompt()

    def _until_prompt(self):
        out = b''
        while not out.endswith(b'(qemu) '):
            d = self.s.recv(65536)
            if not d:
                break
            out += d
        return out.decode(errors='replace')

    def cmd(self, c):
        self.s.sendall((c + '\n').encode())
        return self._until_prompt()


class Trace:
    """Counts in qemu.log's apple_gfx trace, read as it grows."""

    def __init__(self, path):
        self.path, self.pos = path, 0
        self.n = collections.Counter()
        self.tail = collections.deque(maxlen=2000)
        self.partial = b''

    def poll(self):
        try:
            with open(self.path, 'rb') as f:
                f.seek(self.pos)
                data = f.read()
        except FileNotFoundError:
            return self.n
        self.pos += len(data)
        data = self.partial + data
        lines = data.split(b'\n')
        self.partial = lines.pop()
        for raw in lines:
            line = raw.decode(errors='replace')
            self.tail.append(line)
            if TRACE_TS.match(line):       # -msg timestamp=on: "<ISO 8601 UTC> name args"
                line = line.split(' ', 1)[1] if ' ' in line else ''
            name = line.split(' ', 1)[0]
            self.n[name] += 1
            if name == 'apple_gfx_write' and 'offset=0x1020 val=0x1' in line:
                self.n['doorbell'] += 1
        return self.n


class VM:
    def __init__(self, a):
        self.out = Path(a.out).resolve()
        self.label = a.label or 'vm'
        self.clone = Path(a.clone or f'{self.out}/clone').resolve()
        self.vm_dir = Path(a.vm)
        self.qemu = a.qemu
        self.hang_secs = a.hang_secs
        self.net_wait = a.net_wait
        self.shots = self.out / 'shots'
        self.shots.mkdir(parents=True, exist_ok=True)
        self.W, self.H = 1440, 1080
        self.frame = None
        self.last_change = time.time()
        self.nchanges = 0      # screen changes seen, for 'changed' predicates
        self.mark = 0          # nchanges when the current step's action ran
        self.steps = []
        self.retried = False
        self.nshot = 0
        self.ocr_exe = ocr_tool()
        self.bursts = []       # --perf: the interactive bursts, with their wall-clock times
        self.cpu = {}          # QEMU's pid -> [CPU seconds, first seen, last seen], from ps
        self.sampling = threading.Event()
        self.result = {'label': self.label, 'out': str(self.out), 'desktop': False,
                       'hang': None, 'steps': self.steps, 'bursts': self.bursts}

    # -- process ------------------------------------------------------------
    def start(self):
        """Boot, again from a fresh clone while the guest's network stays down.

        Some boots of a macOS 12 guest never sent a packet (no DHCP,
        nothing), whatever the build, and Maps then drew no map. A
        filter-dump of the netdev shows it within seconds: a boot whose
        capture holds no packet after net_wait seconds is quit and started
        again, and result.json counts it (net_restarts)."""
        self.result['net_restarts'] = 0
        for attempt in range(6):
            self._start()
            pcap = self.out / 'net.pcap'
            t = time.time()
            while time.time() - t < self.net_wait:
                if pcap.exists() and pcap.stat().st_size > 24:
                    self.result['net_up_secs'] = round(time.time() - self.t0)
                    log(f'{self.label}: network up after {self.result["net_up_secs"]}s')
                    return
                time.sleep(2)
            log(f'{self.label}: no packet in {self.net_wait}s: booting again')
            self.result['net_restarts'] += 1
            self.stop()
        raise RuntimeError('the guest network never came up')

    def _start(self):
        env = dict(os.environ, FRESH='1', OUT=str(self.out))
        if self.qemu:
            env['QEMU'] = self.qemu
        self.t0 = time.time()
        self.result['start'] = datetime.datetime.now().strftime('%Y-%m-%d %H:%M:%S')
        # -msg timestamp=on stamps every trace line (TRACE_TS); debug-threads
        # names QEMU's own threads ("CPU 0/HVF"), so thread_cpu can tell the
        # vCPUs from the rest.
        self.proc = subprocess.Popen(
            [str(ROOT / 'scripts/internal/run_vmapple.sh'), str(self.vm_dir), str(self.clone),
             '-object', f'filter-dump,id=netdump,netdev=net0,maxlen=128,file={self.out}/net.pcap',
             '-msg', 'timestamp=on', '-name', f'vm-{self.label},debug-threads=on'],
            env=env, stdout=open(self.out / 'run.log', 'w'), stderr=subprocess.STDOUT)
        self.pid = self.proc.pid       # the script execs QEMU in place
        self.result['qemu_pid'] = self.pid
        mon, qmp = self.out / 'monitor.sock', self.out / 'qmp.sock'
        for _ in range(600):
            if self.proc.poll() is not None:
                raise RuntimeError(f'QEMU exited {self.proc.returncode}: see {self.out}/run.log')
            if mon.exists() and qmp.exists():
                break
            time.sleep(0.2)
        time.sleep(0.5)
        self.qmp, self.hmp = QMP(str(qmp)), HMP(str(mon))
        self.trace = Trace(self.out / 'qemu.log')
        self.frame = None
        log(f'{self.label}: QEMU pid {self.pid}, out {self.out}')

    def stop(self):
        """Quit QEMU through HMP, and reap it with wait4 for its rusage."""
        try:
            self.result['qemu_threads'] = thread_cpu(self.pid)
            self.result['blockstats'] = self.hmp.cmd('info blockstats')
        except Exception:
            pass
        try:
            self.hmp.cmd('quit')
        except Exception:
            pass
        ru = None
        t = time.time()
        while True:
            try:
                pid, status, ru = os.wait4(self.pid, os.WNOHANG)
            except ChildProcessError:
                break
            if pid:
                self.proc.returncode = os.waitstatus_to_exitcode(status)
                break
            if time.time() - t > 30:
                self.proc.kill()
                t = float('inf')
            time.sleep(0.2)
        if ru is not None:
            self.result['qemu_rusage'] = {'user': ru.ru_utime, 'system': ru.ru_stime}
        self.result['qemu_exit'] = self.proc.returncode

    # -- CPU ----------------------------------------------------------------
    def sample_cpu(self):
        """Every 2 s, until sampling is set: `ps -o time` (user + system CPU)
        of QEMU. analyse() takes the last sample for QEMU's CPU when
        result.json has no rusage (stop() did not reap QEMU)."""
        while not self.sampling.is_set():
            try:
                out = subprocess.run(['ps', '-o', 'pid=,time=', '-p', str(self.pid)],
                                     capture_output=True, text=True).stdout
                now = time.time()
                for line in out.splitlines():
                    pid, cpu = line.split()
                    secs = 0.0
                    for part in cpu.split(':'):
                        secs = secs * 60 + float(part)
                    e = self.cpu.setdefault(int(pid), [0.0, now, now])
                    e[0], e[2] = secs, now
            except Exception:
                pass
            self.sampling.wait(2)

    # -- screen -------------------------------------------------------------
    def grab(self):
        p = self.out / 'cur.ppm'
        self.qmp.cmd('screendump', filename=str(p))
        img = Image.open(p)
        img.load()
        self.W, self.H = img.size
        a = np.asarray(img.convert('RGB'))
        if self.frame is None or self.frame.shape != a.shape or \
                (np.abs(a[MENU_BAR:].astype(int) - self.frame[MENU_BAR:]).max(axis=2) > 24).sum() > CHANGE_PX:
            self.last_change = time.time()
            self.nchanges += 1
        self.frame = a
        self.img = img
        return img

    def ocr(self, img=None):
        img = img or self.grab()
        p = self.out / 'cur.png'
        img.save(p)
        out = subprocess.run([str(self.ocr_exe), str(p)], capture_output=True, text=True).stdout
        items = []
        for line in out.splitlines():
            x, y, w, h, t = line.split(' ', 4)
            items.append((int(x), int(y), int(w), int(h), t))
        return items

    def shot(self, name, img=None):
        self.nshot += 1
        img = img or self.grab()
        p = self.shots / f'{self.nshot:02d}-{name}.png'
        img.save(p)
        return p

    # -- input --------------------------------------------------------------
    def _abs(self, x, y):
        return [{'type': 'abs', 'data': {'axis': 'x', 'value': int(x * 32767 / (self.W - 1))}},
                {'type': 'abs', 'data': {'axis': 'y', 'value': int(y * 32767 / (self.H - 1))}}]

    def move(self, x, y):
        self.qmp.cmd('input-send-event', events=self._abs(x, y))

    def button(self, down, b='left'):
        self.qmp.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': down, 'button': b}}])

    def click(self, x, y):
        self.move(x, y)
        time.sleep(0.2)
        self.button(True)
        time.sleep(0.08)
        self.button(False)
        time.sleep(0.1)

    def drag(self, x0, y0, x1, y1, steps=20):
        self.move(x0, y0)
        time.sleep(0.2)
        self.button(True)
        time.sleep(0.2)
        for i in range(1, steps + 1):
            self.move(x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps)
            time.sleep(0.04)
        time.sleep(0.5)          # stop before release: no momentum
        self.button(False)

    def key(self, k, gap=0.1):
        self.hmp.cmd('sendkey ' + k)
        time.sleep(gap)

    def type(self, text):
        m = {' ': 'spc', '.': 'dot', '-': 'minus', ':': 'shift-semicolon', '/': 'slash'}
        for ch in text:
            self.key(m.get(ch, ('shift-' + ch.lower()) if ch.isupper() else ch), 0.08)

    @staticmethod
    def find(items, pattern, pick='first', where=None):
        hits = [i for i in items if re.search(pattern, i[4]) and (where is None or where(i))]
        if not hits:
            return None
        if pick == 'top':
            hits.sort(key=lambda i: i[1])
        elif pick == 'bottom':
            hits.sort(key=lambda i: -i[1])
        x, y, w, h, _ = hits[0]
        return x + w // 2, y + h // 2

    def click_text(self, items, pattern, pick='first', where=None):
        pos = self.find(items, pattern, pick, where)
        if pos is None:
            return False
        self.click(*pos)
        return True

    # -- waiting ------------------------------------------------------------
    def settle(self, quiet=3.0, timeout=60):
        """Wait until the screen stops changing for `quiet` seconds."""
        start = time.time()
        self.grab()
        while time.time() - start < timeout:
            time.sleep(1)
            self.grab()
            if time.time() - self.last_change >= quiet:
                return True
        return False

    def wait(self, pred, timeout, retry=None, retry_after=25, ocr=True):
        """Poll until pred(items) holds. Returns 'ok', 'hang' or 'timeout'.

        A hang: no screen change and no new frame for hang_secs, or for the
        whole of a timeout (a still screen where a change was expected: the
        menu bar clock alone gives a frame a minute). `retry`, if given, is
        run once after `retry_after` quiet seconds (a click that landed during
        a transition)."""
        start = time.time()
        frames = self.trace.poll()['apple_gfx_new_frame']
        last_frame_t = start
        self.last_change = start
        retried = False
        while True:
            items = self.ocr() if ocr else self.grab()
            if pred(items):
                return 'ok'
            now = time.time()
            n = self.trace.poll()['apple_gfx_new_frame']
            if n != frames:
                frames, last_frame_t = n, now
            quiet = now - max(self.last_change, last_frame_t)
            if retry and not retried and now - self.last_change >= retry_after:
                retried = True
                self.retried = True
                log(f'{self.label}: retrying the action')
                retry(items)
                self.last_change = time.time()
                continue
            if quiet >= self.hang_secs:
                return 'hang'
            if now - start >= timeout:
                return 'hang' if quiet >= timeout - 5 else 'timeout'
            time.sleep(1.5)

    # -- steps --------------------------------------------------------------
    def step(self, name, action, pred, timeout=240, retry=None, shot=True, settle=True):
        c0 = dict(self.trace.poll())
        t = time.time()
        self.mark = self.nchanges
        self.retried = False
        if action:
            action()
        r = self.wait(pred, timeout, retry) if pred else 'ok'
        if r == 'ok' and settle:
            self.settle()
        c1 = self.trace.poll()
        rec = {'step': name, 'result': r, 'secs': round(time.time() - t, 1),
               't0': t, 't1': time.time(),
               'frames': c1['apple_gfx_new_frame'] - c0.get('apple_gfx_new_frame', 0),
               'doorbells': c1['doorbell'] - c0.get('doorbell', 0),
               'irqs': c1['apple_gfx_raise_irq'] - c0.get('apple_gfx_raise_irq', 0),
               'cursor_sets': c1['apple_gfx_cursor_set'] - c0.get('apple_gfx_cursor_set', 0),
               'retried': self.retried}
        if shot:
            p = self.shot(name)
            rec['shot'] = p.name
        self.steps.append(rec)
        log(f'{self.label}: {name}: {r} in {rec["secs"]}s frames={rec["frames"]} '
            f'doorbells={rec["doorbells"]}' + (f' grid={rec["grid"]}' if 'grid' in rec else ''))
        if r != 'ok':
            self.hung(name, r)
        return r == 'ok'

    def hung(self, name, why):
        self.result['hang'] = {'step': name, 'why': why,
                               'secs_since_boot': round(time.time() - self.t0)}
        d = self.out / 'hang'
        d.mkdir(exist_ok=True)
        log(f'{self.label}: {why} at {name}: sampling QEMU')
        subprocess.run(['sample', str(self.pid), '10', '-file', str(d / 'sample-qemu.txt')],
                       capture_output=True)
        self.trace.poll()
        (d / 'trace-tail.txt').write_text('\n'.join(self.trace.tail) + '\n')
        (d / 'trace-counts.json').write_text(json.dumps(self.trace.n, indent=1))
        self.grab().save(d / 'screen.png')

    # -- the walk -----------------------------------------------------------
    def boot(self):
        """Wait for the desktop. A login window or Setup Assistant ends the boot."""
        def state(items):
            if self.find(items, '^Finder$') or self.find(items, 'Keyboard Set-?up Assistant'):
                return 'desktop'
            if self.find(items, r'^Enter Password$|^Password$'):
                return 'login'
            if self.find(items, r'Select Your Country|^Language$|Hello'):
                return 'setup'
            return None
        c0 = dict(self.trace.poll())
        t = time.time()
        r = self.wait(lambda it: state(it) is not None, 600)
        c1 = self.trace.poll()
        rec = {'step': 'boot', 'result': r, 'secs': round(time.time() - t, 1),
               't0': self.t0, 't1': time.time(),
               'frames': c1['apple_gfx_new_frame'] - c0.get('apple_gfx_new_frame', 0),
               'doorbells': c1['doorbell'] - c0.get('doorbell', 0)}
        if r == 'ok':
            st = state(self.ocr())
            if st != 'desktop':
                rec['result'] = st
                self.steps.append(rec)
                self.shot(st)
                self.result['stopped'] = f'{st}: the driver types no password and makes no account'
                log(f'{self.label}: the guest shows {st}: stopping')
                return False
            self.result['desktop'] = True
            self.result['desktop_secs'] = round(time.time() - self.t0)
            rec['shot'] = self.shot('desktop').name
            self.steps.append(rec)
            (self.out / 'usernet-desktop.txt').write_text(self.hmp.cmd('info usernet'))
            log(f'{self.label}: desktop after {self.result["desktop_secs"]}s')
            return True
        self.steps.append(rec)
        self.hung('boot', r)
        return False

    def tour(self):
        s = self.step
        changed = lambda it: self.nchanges > self.mark
        dock = lambda app: (lambda: self.click(DOCK[app], DOCK_Y))
        items = self.ocr()
        if self.find(items, 'Keyboard Set-?up Assistant'):
            if not s('ksa-quit', lambda: self.click_text(items, '^Quit$'),
                     lambda it: not self.find(it, 'Keyboard Set-?up Assistant'),
                     retry=lambda it: self.click_text(it, '^Quit$')):
                return False

        def loaded(name):
            p = self.shot(name)
            g = grid_count(Image.open(p))
            self.steps.append({'step': name, 'result': 'ok', 'shot': p.name, 'grid': g,
                               'grid_gap': g >= GRID_MIN})
            log(f'{self.label}: {name}: grid={g}')

        if not s('launchpad', dock('launchpad'), changed, 120):
            return False
        self.key('esc')
        self.settle()
        # Maps: "What's New", then (on a first open) the notifications sheet
        if not s('maps-open', dock('maps'), lambda it: self.find(it, "What.s New in Maps"), 180,
                 retry=lambda it: dock('maps')()):
            return False
        if not s('maps', lambda: self.click_text(self.ocr(), '^Continue$'),
                 lambda it: not self.find(it, "What.s New in Maps"), 120,
                 retry=lambda it: self.click_text(it, '^Continue$')):
            return False
        def sheet():
            """Maps' notifications sheet, which can come seconds late."""
            items = self.ocr()
            if not self.find(items, 'Get Notified'):
                return True
            return s('maps-notnow', lambda: self.click_text(items, '^Not Now$'),
                     lambda it: not self.find(it, 'Get Notified'), 120,
                     retry=lambda it: self.click_text(it, '^Not Now$'))
        if not sheet():
            return False
        time.sleep(10)
        self.settle(quiet=5)
        if not sheet():
            return False
        loaded('maps-loaded')

        def zoom(k, n):
            def f():
                for _ in range(n):
                    self.key(k, 0.4)
            return f

        def tiles():
            time.sleep(15)       # tiles load over the guest's network
            self.settle(quiet=5, timeout=90)

        if not s('maps-globe', zoom('meta_l-minus', 10), changed, 120, settle=False):
            return False
        tiles()
        loaded('maps-globe-loaded')
        if not sheet():
            return False
        if not s('maps-globe-pan', lambda: self.drag(860, 500, 1060, 560), changed, 120,
                 settle=False):
            return False
        tiles()
        loaded('maps-globe-pan-loaded')
        if not s('maps-zoom-in', zoom('meta_l-equal', 2), changed, 120, settle=False):
            return False
        tiles()
        loaded('maps-zoom-in-loaded')
        if not s('maps-zoom-out', zoom('meta_l-minus', 2), changed, 120, settle=False):
            return False
        tiles()
        loaded('maps-zoom-out-loaded')
        self.key('meta_l-q')
        self.settle()
        # Photos: "What's New in Photos", Get Started
        if not s('photos', dock('photos'), lambda it: self.find(it, "What.s New in Photos|^Library$"),
                 180, retry=lambda it: dock('photos')()):
            return False
        items = self.ocr()
        if self.find(items, '^Get Started$'):
            if not s('photos-start', lambda: self.click_text(items, '^Get Started$'),
                     lambda it: not self.find(it, "What.s New in Photos"), 120,
                     retry=lambda it: self.click_text(it, '^Get Started$')):
                return False
        self.key('meta_l-q')
        self.settle()
        if not s('safari', dock('safari'), lambda it: self.find(it, 'Search or enter website'),
                 180, retry=lambda it: dock('safari')()):
            return False
        self.key('meta_l-q')
        self.settle()
        # System Settings > Displays, found through its search field
        sidebar = lambda i: i[0] < 560
        if not s('settings', dock('settings'), lambda it: self.find(it, '^Appearance$'), 180,
                 retry=lambda it: dock('settings')()):
            return False

        def search():
            items = self.ocr()
            if not self.click_text(items, '^Search$', where=sidebar):
                self.click(420, 198)
            time.sleep(1)
            self.type('Displays')
        if not s('settings-search', search,
                 lambda it: self.find(it, '^Display Settings$', where=sidebar), 60):
            return False
        pick = lambda i: sidebar(i) and i[1] > 220
        if not s('displays', lambda: self.click_text(self.ocr(), '^Displays$', 'top', pick),
                 lambda it: self.find(it, 'Show all resolutions'), 120,
                 retry=lambda it: self.click_text(it, '^Displays$', 'top', pick)):
            return False
        if not s('mode-1920', lambda: self.click_text(self.ocr(), r'^1920\s*[x×]\s*1080'),
                 lambda it: self.W == 1920, 120):
            return False
        time.sleep(3)
        self.settle()
        if not s('mode-1440', lambda: self.click_text(self.ocr(), r'^1440\s*[x×]\s*1080'),
                 lambda it: self.W == 1440, 120):
            return False
        self.key('meta_l-q')
        self.settle()
        # Finder: a window, dragged by its toolbar
        if not s('finder', dock('finder'), lambda it: self.find(it, RECENTS), 120,
                 retry=lambda it: dock('finder')()):
            return False
        pos = self.find(self.ocr(), RECENTS, 'top')

        def drag():
            x, y = pos
            self.drag(x + 90, y, x + 290, y + 120)
        if not s('finder-drag', drag, changed, 60):
            return False
        if not s('mission-control', lambda: self.key('ctrl-up'), changed, 60):
            return False
        self.key('esc')
        self.settle()
        s('end', None, None)
        return True

    # -- interactive load (--perf) ------------------------------------------
    def path(self, points, dt=1 / 60, press=True):
        """The pointer along points, one QMP event every dt, the button held
        from the first point to the last (a drag) if press."""
        self.move(*points[0])
        time.sleep(0.15)
        if press:
            self.button(True)
            time.sleep(0.1)
        nxt = time.time()
        for x, y in points[1:]:
            nxt += dt
            self.move(x, y)
            time.sleep(max(0.0, nxt - time.time()))
        if press:
            time.sleep(0.1)
            self.button(False)

    def wheel(self, x, y, n, down=True, dt=1 / 60):
        """n wheel clicks at (x, y), one every dt: the USB tablet's wheel."""
        self.move(x, y)
        time.sleep(0.15)
        b = 'wheel-down' if down else 'wheel-up'
        nxt = time.time()
        for _ in range(n):
            nxt += dt
            self.qmp.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': True, 'button': b}}])
            self.qmp.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': False, 'button': b}}])
            time.sleep(max(0.0, nxt - time.time()))

    def burst(self, name, kind, action):
        """One burst of interactive input. kind 'continuous' is pointer input
        at 60 Hz, where every event should give a frame; 'keyed' is keys with
        pauses, where the screen can rest between animations. t0 and t1 are
        the wall-clock times of its first and last input, the clock of the
        trace's timestamps."""
        c0 = dict(self.trace.poll())
        t0 = time.time()
        action()
        t1 = time.time()
        time.sleep(0.5)
        c1 = self.trace.poll()
        rec = {'burst': name, 'kind': kind, 't0': t0, 't1': t1,
               'frames': c1['apple_gfx_new_frame'] - c0.get('apple_gfx_new_frame', 0),
               'doorbells': c1['doorbell'] - c0.get('doorbell', 0),
               'shot': self.shot(f'burst-{name}').name}
        self.bursts.append(rec)
        log(f'{self.label}: burst {name}: {t1 - t0:.1f}s frames={rec["frames"]}')

    def perf_tour(self):
        """Interactive-style load after the tour: a Finder window dragged in
        circles and resized, Mission Control in and out, Launchpad paged,
        Safari scrolling a long page (long_page, served from this host),
        and Maps panned and zoomed in bursts. Each burst is timed (burst)."""
        s = self.step
        changed = lambda it: self.nchanges > self.mark
        dock = lambda app: (lambda: self.click(DOCK[app], DOCK_Y))
        port = serve_page()
        items = self.ocr()
        if self.find(items, 'Keyboard Set-?up Assistant'):      # --no-tour
            if not s('ksa-quit', lambda: self.click_text(items, '^Quit$'),
                     lambda it: not self.find(it, 'Keyboard Set-?up Assistant'),
                     retry=lambda it: self.click_text(it, '^Quit$')):
                return False
        # Finder: the window the tour left, or a new one
        items = self.ocr()
        if not self.find(items, RECENTS, 'top'):
            if not s('perf-finder', dock('finder'), lambda it: self.find(it, RECENTS), 120,
                     retry=lambda it: dock('finder')()):
                return False
            items = self.ocr()
        tx, ty = self.find(items, RECENTS, 'top')
        gx, gy = tx + 90, ty          # the toolbar, where the tour grabbed it

        def circles():
            pts = [(gx + 150 * math.sin(2 * math.pi * k / 90), gy + 90 - 90 * math.cos(2 * math.pi * k / 90))
                   for k in range(181)]
            self.path(pts)
        for i in range(3):
            self.burst(f'finder-drag-{i}', 'continuous', circles)
        self.settle(quiet=2)
        # The bottom right corner: the window's right and bottom edges, the
        # first columns and rows from the toolbar outwards where the screen
        # is the desktop again (Finder's window is light grey; the edge is a
        # dark line).
        items = self.ocr()
        tx, ty = self.find(items, RECENTS, 'top')
        a = self.frame.astype(int)
        row = a[min(ty + 40, a.shape[0] - 1)]
        light = lambda px: px.min() >= 200
        x = tx
        while x < a.shape[1] - 1 and light(row[x]):
            x += 1
        right = x
        col = a[:, max(right - 20, 0)]
        y = ty + 40
        while y < a.shape[0] - 1 and light(col[y]):
            y += 1
        bottom = y
        self.result['finder_corner'] = [right, bottom]
        cx, cy = right - 2, bottom - 2

        def resize():
            pts = []
            for k in range(3):
                pts += [(cx - 300 * i / 30, cy - 200 * i / 30) for i in range(31)]
                pts += [(cx - 300 + 300 * i / 30, cy - 200 + 200 * i / 30) for i in range(1, 31)]
            self.path(pts)
        self.burst('finder-resize', 'continuous', resize)
        self.settle(quiet=2)

        def mission():
            for _ in range(6):
                self.key('ctrl-up', 0.9)
                self.key('ctrl-up', 0.9)
        self.burst('mission-control', 'keyed', mission)
        self.settle(quiet=2)
        # Launchpad: paged by swipes of the pointer and by keys
        if not s('perf-launchpad', dock('launchpad'), changed, 120):
            return False
        time.sleep(1)

        def swipes():
            for _ in range(3):
                self.path([(1100 - 800 * i / 24, 540) for i in range(25)])
                time.sleep(0.6)
                self.path([(300 + 800 * i / 24, 540) for i in range(25)])
                time.sleep(0.6)
        self.burst('launchpad-swipe', 'keyed', swipes)

        # Launchpad has one page on this guest, so the swipes rubber-band.
        # Then a search typed and erased, and the Other folder opened.
        def keys():
            for ch in 'mail':
                self.key(ch, 0.4)
            for _ in range(4):
                self.key('backspace', 0.4)
            self.click(180, 805)
            time.sleep(1.2)
            self.key('esc', 1.0)
        self.burst('launchpad-keys', 'keyed', keys)
        self.key('esc')
        self.settle(quiet=2)
        # Safari: the long page, scrolled by the wheel and by the space bar
        if not s('perf-safari', dock('safari'), lambda it: self.find(it, 'Search or enter website'),
                 180, retry=lambda it: dock('safari')()):
            return False
        self.key('meta_l-l', 0.5)
        self.type(f'10.0.2.2:{port}/long.html')
        self.key('ret')
        if not s('perf-page', None, lambda it: self.find(it, 'PERFPAGE'), 60):
            return False
        px, py = self.W // 2, self.H // 2
        self.click(px, py)
        # The guest scrolls naturally: the wheel's up clicks move down the page.
        for i in range(2):
            self.burst(f'safari-scroll-down-{i}', 'continuous', lambda: self.wheel(px, py, 150, False))
            self.burst(f'safari-scroll-up-{i}', 'continuous', lambda: self.wheel(px, py, 150))

        def pages():
            for _ in range(10):
                self.key('spc', 0.3)
            for _ in range(10):
                self.key('shift-spc', 0.3)
        self.burst('safari-space', 'keyed', pages)
        self.key('meta_l-q')
        self.settle(quiet=2)
        # Maps: pans and zooms in bursts
        if not s('perf-maps', dock('maps'), changed, 120, retry=lambda it: dock('maps')()):
            return False
        time.sleep(5)
        self.settle(quiet=3)
        for pat, btn in (("What.s New in Maps", '^Continue$'), ('Get Notified', '^Not Now$')):
            items = self.ocr()
            if self.find(items, pat):
                self.click_text(items, btn)
                self.settle(quiet=2)
        mx, my = 860, 500

        def pans():
            for k in range(6):
                d = 1 if k % 2 == 0 else -1
                self.path([(mx - d * 150 + d * 300 * i / 20, my - d * 60 + d * 120 * i / 20)
                           for i in range(21)])
        self.burst('maps-pan', 'continuous', pans)
        time.sleep(5)
        # The wheel's up clicks zoom in. 60 of them reach the closest zoom.
        for i in range(2):
            self.burst(f'maps-wheel-in-{i}', 'continuous', lambda: self.wheel(mx, my, 20, False))
            time.sleep(3)
            self.burst(f'maps-wheel-out-{i}', 'continuous', lambda: self.wheel(mx, my, 20))
            time.sleep(3)

        # The map's zoom buttons, bottom right of the window where Maps
        # opens at 1440x1080 (Cmd-= and Cmd-minus stop reaching the map once
        # the wheel has zoomed it).
        def zooms():
            for _ in range(4):
                self.click(1149, 820)
                time.sleep(0.35)
            time.sleep(1.5)
            for _ in range(4):
                self.click(1125, 820)
                time.sleep(0.35)
        self.burst('maps-buttons', 'keyed', zooms)
        time.sleep(10)
        self.settle(quiet=3, timeout=60)
        self.shot('perf-maps-end')
        self.key('meta_l-q')
        self.settle()
        s('perf-end', None, None)
        return True

    # -- after --------------------------------------------------------------
    def logs(self):
        """QEMU's os_log lines since the boot started, by its PID, and the
        crash reports of its process."""
        start = self.result['start']
        out = subprocess.run(['/usr/bin/log', 'show', '--start', start, '--predicate',
                              f'processID == {self.pid}', '--info', '--debug'],
                             capture_output=True, text=True).stdout
        (self.out / 'qemu-oslog.txt').write_text(out)
        out = subprocess.run(['/usr/bin/log', 'show', '--start', start, '--predicate',
                              f'processID == {self.pid}', '--info', '--debug', '--style', 'ndjson'],
                             capture_output=True, text=True).stdout
        (self.out / 'qemu-oslog.ndjson').write_text(out)
        # Crash reports of this QEMU: by the pid in the report, since the
        # suite's processes (reims_vgpu-*, pgrun-*) crash on purpose.
        since = self.t0 - 5
        rep = Path.home() / 'Library/Logs/DiagnosticReports'
        crashes = []
        for p in rep.glob('*.ips'):
            try:
                if p.stat().st_mtime < since:
                    continue
                body = p.read_text(errors='replace')
            except OSError:
                continue
            m = re.search(r'"pid"\s*:\s*(\d+)', body)
            if m and int(m.group(1)) == self.pid:
                crashes.append(p.name)
        self.result['crashes'] = crashes

    def save(self):
        self.result['trace_counts'] = dict(self.trace.n) if hasattr(self, 'trace') else {}
        self.result['cpu'] = {str(p): v for p, v in self.cpu.items()}
        (self.out / 'result.json').write_text(json.dumps(self.result, indent=1, default=str))


def run(a):
    vm = VM(a)
    sampler = threading.Thread(target=vm.sample_cpu, daemon=True)
    try:
        vm.start()
        sampler.start()
        ok = vm.boot()
        if ok and not a.no_tour:
            ok = vm.tour()
        vm.result['tour_done'] = bool(ok)
        if ok and a.perf:
            ok = vm.perf_tour()
            vm.result['perf_done'] = bool(ok)
    except Exception as e:  # a closed socket is QEMU gone: record it
        vm.result['error'] = repr(e)
        log(f'{vm.label}: error {e!r}')
    finally:
        vm.result['secs'] = round(time.time() - vm.t0)
        vm.stop()
        vm.sampling.set()
        if sampler.is_alive():
            sampler.join(10)
        time.sleep(3)
        vm.logs()
        vm.save()
        for p in ('cur.ppm', 'cur.png'):
            (vm.out / p).unlink(missing_ok=True)
    log(f'{vm.label}: desktop={vm.result["desktop"]} hang={vm.result["hang"]} '
        f'crashes={vm.result["crashes"]}')


def pair(a):
    specs = []
    for spec in a.builds:
        m = re.fullmatch(r'([A-Za-z0-9_.-]+)(?:=(.+))?', spec)
        if not m:
            raise SystemExit(f'pair: {spec!r} is not LABEL[=QEMU]')
        specs.append(m.groups())
    if specs[0][0] == specs[1][0]:
        raise SystemExit('pair: the two builds need different labels')
    base = Path(a.out_base)
    base.mkdir(parents=True, exist_ok=True)
    procs = []
    for label, q in specs:
        cmd = [sys.executable, __file__, 'run', '--label', label, '--out',
               str(base / f'{label}-{a.index}'), '--vm', a.vm]
        if q:
            cmd += ['--qemu', q]
        if a.perf:
            cmd += ['--perf']
        procs.append(subprocess.Popen(cmd, stdout=open(base / f'{label}-{a.index}.log', 'w'),
                                      stderr=subprocess.STDOUT))
    for p in procs:
        p.wait()
    log(f'pair {a.index} done')


def compare(a):
    A, B = Path(a.a), Path(a.b)

    def shots(d):
        return {re.sub(r'^\d+-', '', p.stem): p for p in sorted((d / 'shots').glob('*.png'))}

    sa, sb = shots(A), shots(B)
    for name in [n for n in sa if n in sb]:
        x = np.asarray(Image.open(sa[name]).convert('RGB')).astype(int)
        y = np.asarray(Image.open(sb[name]).convert('RGB')).astype(int)
        g = (grid_count(Image.open(sa[name])), grid_count(Image.open(sb[name]))) \
            if name.endswith('-loaded') else None
        gs = f' grid={g[0]}/{g[1]}' if name.endswith('-loaded') else ''
        if x.shape != y.shape:
            print(f'{name}: sizes {x.shape[1]}x{x.shape[0]} {y.shape[1]}x{y.shape[0]}{gs}')
            continue
        d = np.abs(x[MENU_BAR:] - y[MENU_BAR:]).max(axis=2)
        n16 = int((d > 16).sum())
        box = ''
        if n16:
            ys, xs = np.nonzero(d > 16)
            box = f' box=({xs.min()},{ys.min() + MENU_BAR})-({xs.max()},{ys.max() + MENU_BAR})'
        print(f'{name}: max={d.max()} >16={n16}{box}{gs}')


def summary(a):
    for d in a.dirs:
        p = Path(d) / 'result.json'
        if not p.exists():
            print(f'{d}: no result.json')
            continue
        r = json.loads(p.read_text())
        steps = r['steps']
        grid = [s['step'] for s in steps if s.get('grid_gap') and s['step'].endswith('-loaded')]
        times = ' '.join(f'{s["step"]}={s.get("secs", "?")}' for s in steps if 'secs' in s)
        print(f'{d}: label={run_label(r)} '
              f'desktop={r["desktop"]}@{r.get("desktop_secs")}s '
              f'tour={r.get("tour_done")} hang={r["hang"]} grid={grid or "-"} '
              f'crashes={r.get("crashes")} '
              f'net_restarts={r.get("net_restarts")} total={r.get("secs")}s\n    {times}')


def run_label(r):
    """A run's label, from its result.json: runs recorded before the label
    replaced the mode have only the mode."""
    return r.get('label') or r.get('mode', 'vm')


def trace_path(d, r):
    """A run's qemu.log, or the qemu-MODE.log of a run recorded before."""
    path = Path(d) / 'qemu.log'
    if path.exists() or 'mode' not in r:
        return path
    return Path(d) / f'qemu-{r["mode"]}.log'


def read_trace(path):
    """[(t, name, offset, value)] of the timestamped lines of a qemu.log;
    t in seconds since the epoch, offset and value for the MMIO lines."""
    ev = []
    mmio = ('apple_gfx_write', 'apple_gfx_read', 'apple_gfx_mmio_iosfc_write',
            'apple_gfx_mmio_iosfc_read')
    with open(path, 'rb') as f:
        for raw in f:
            line = raw.decode(errors='replace').rstrip('\n')
            if not TRACE_TS.match(line):
                continue
            ts, rest = line.split(' ', 1)
            t = datetime.datetime.fromisoformat(ts.replace('Z', '+00:00')).timestamp()
            name, _, args = rest.partition(' ')
            off = val = None
            if name in mmio:
                a, b = args.split(' ')[:2]
                off, val = int(a.split('=')[1], 16), int(b.split('=')[1], 16)
            ev.append((t, name, off, val))
    return ev


def pcap_stats(path, per_second=None):
    """(packets, bytes on the wire, first SYN) of a libpcap file
    (filter-dump's net.pcap, whose records are cut to maxlen but keep the
    original length). per_second, a Counter, gets the bytes of each whole
    second of the file's clock. filter-dump stamps packets with QEMU's
    virtual clock plus a start time (net/dump.c), not the wall clock the
    trace uses; the first SYN the guest sends to 10.0.2.2 (the host, where
    perf_tour's page is served) is the anchor analyse() aligns them with."""
    import struct
    n = total = 0
    syn = None
    try:
        with open(path, 'rb') as f:
            head = f.read(24)
            if len(head) < 24:
                return 0, 0
            end = '<' if head[:4] in (b'\xd4\xc3\xb2\xa1', b'\x4d\x3c\xb2\xa1') else '>'
            while True:
                rec = f.read(16)
                if len(rec) < 16:
                    break
                ts, us, incl, orig = struct.unpack(end + 'IIII', rec)
                if syn is None and incl >= 48:
                    pkt = f.read(incl)
                    # IPv4, TCP, to 10.0.2.2, SYN without ACK
                    if pkt[12:14] == b'\x08\x00' and pkt[23] == 6 and pkt[30:34] == bytes([10, 0, 2, 2]):
                        l4 = 14 + (pkt[14] & 15) * 4
                        if len(pkt) > l4 + 13 and pkt[l4 + 13] & 0x12 == 0x02:
                            syn = ts + us / 1e6
                else:
                    f.seek(incl, 1)
                n += 1
                total += orig
                if per_second is not None:
                    per_second[ts] += orig
    except FileNotFoundError:
        pass
    return n, total, syn


def pct(xs, q):
    """The q-th percentile (0-100) of xs, linear between order statistics."""
    if not xs:
        return float('nan')
    xs = sorted(xs)
    k = (len(xs) - 1) * q / 100
    lo, hi = math.floor(k), math.ceil(k)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


# The library's threads in QEMU's process, by the prefix of their names: its
# FIFO threads (reims-vgpu.fifo), its timers' (reims-vgpu.timer), and its
# serial queues' workers, each named after its queue's label
# (reims-vgpu.fifo-control, reims-vgpu.reporting, reims-vgpu.display, and the
# others rt::queue::SerialQueue::new lists), isolated tasks' queues among them.
LIBRARY_THREAD_PREFIX = 'reims-vgpu.'

# A second in which a guest moved more than this many bytes on its network
# is busy (perf: the "quiet" comparison).
QUIET_BPS = 1_000_000

# Bursts whose input pauses: counted with the keyed ones. launchpad-swipe
# waits 0.6 s between its swipes (Launchpad has one page here, and each
# swipe rubber-bands, then rests); a result.json that records it as
# continuous is counted keyed too.
BURST_KIND = {'launchpad-swipe': 'keyed'}


def analyse(d):
    """The latency, frame and CPU figures of one boot (docs/qemu.md, "How
    the figures are defined"), from its trace, result.json, net.pcap and
    QEMU's os_log.

    Doorbell to interrupt. A FIFO doorbell is a write of n (1..63) to 0x1020,
    which wakes child FIFO n. The host publishes a FIFO's stamp by setting bit
    stampIndex in the read-to-clear mask at 0x1018 and raising the interrupt,
    and the guest's handler reads 0x1018 after each interrupt. The guest gives
    child n stampIndex n (every bit it reads is of a FIFO it rang), so a
    doorbell of FIFO n is matched to the first read of 0x1018 after its line
    with bit n set, and its latency ends at the last interrupt raised before
    that read. Doorbells rung again before the match share it. QEMU prints
    a write's line only once the library's rvg_device_mmio_write, which it
    calls on a thread of its pool, has returned and the vCPU thread has
    woken, so the interrupt can come before the doorbell's own line:
    - lenient (lat): such a doorbell counts 0. This is a lower bound: the
      bit may be the FIFO's earlier work, published before the doorbell.
    - strict (lat_strict): such a doorbell waits for the next read with bit n
      after an interrupt that follows its line. An upper bound: when the
      host finished before the line, it waits for the FIFO's next work,
      seconds later if the FIFO then idles.
    The trace carries no stamp values, so both are the time to a stamp the
    host publishes for that FIFO after the doorbell, not to the stamp of the
    doorbell's own commands."""
    import bisect
    d = Path(d)
    r = json.loads((d / 'result.json').read_text())
    ev = read_trace(trace_path(d, r))
    # The guest's network bytes per wall-clock second: the pcap's clock
    # moved onto the wall clock by the guest's first connection to the page
    # server, which perf_tour made at the start of step perf-page.
    raw_rate = collections.Counter()
    net_stats = pcap_stats(d / 'net.pcap', raw_rate)
    anchor = next((s['t0'] for s in r['steps'] if s.get('step') == 'perf-page'), None)
    net_rate = collections.Counter()
    if net_stats[2] is not None and anchor is not None:
        shift = anchor - net_stats[2]
        for sec, b in raw_rate.items():
            net_rate[int(sec + shift)] += b
    steps = [(s['t0'], s['t1'], s['step']) for s in r['steps'] if 't0' in s]
    steps += [(b['t0'], b['t1'], 'burst ' + b['burst']) for b in r.get('bursts', [])]
    steps.sort()
    desk = next((s['t1'] for s in r['steps'] if s.get('step') == 'boot' and 't1' in s), None)

    def where(t):
        name = 'before the desktop'
        for t0, t1, n in steps:
            if t0 <= t:
                name = n if t <= t1 else f'after {n}'
        return name

    pending = collections.defaultdict(list)       # lenient: fifo -> [(t, index)]
    pending_s = collections.defaultdict(list)     # strict
    last_irq = None            # (t, index)
    irqs, frames, doors = [], [], []
    lat, lat_strict, late_gap = [], [], []        # (fifo, t_door, t_irq) / seconds
    bit_reads = collections.defaultdict(list)     # fifo -> times of reads with its bit
    irq_to_read = []
    stray_bits = collections.Counter()
    ios_pending, ios_lat = [], []
    fifo_doors = collections.Counter()
    spacing = collections.defaultdict(list)       # adjacent MMIO lines, seconds apart
    prev = None
    for i, (t, name, off, val) in enumerate(ev):
        if name in ('apple_gfx_write', 'apple_gfx_read'):
            key = f'{"w" if name == "apple_gfx_write" else "r"}{off:#x}'
            if prev and prev[1] in ('apple_gfx_write', 'apple_gfx_read') and ev[i - 1] is prev:
                pk = f'{"w" if prev[1] == "apple_gfx_write" else "r"}{prev[2]:#x}'
                if (pk, key) in (('w0x1020', 'w0x1020'), ('r0x1018', 'r0x1014'),
                                 ('r0x1014', 'r0x102c')):
                    spacing[f'{pk}->{key}'].append(t - prev[0])
        prev = ev[i]
        if name == 'apple_gfx_write' and off == 0x1020 and 1 <= val < 64:
            pending[val].append((t, i))
            pending_s[val].append((t, i))
            doors.append((t, val))
            fifo_doors[val] += 1
        elif name == 'apple_gfx_raise_irq':
            last_irq = (t, i)
            irqs.append(t)
        elif name == 'apple_gfx_read' and off == 0x1018:
            if last_irq:
                irq_to_read.append(t - last_irq[0])
            for n in range(64):
                if not (val >> n) & 1:
                    continue
                bit_reads[n].append(t)
                if not pending[n]:
                    stray_bits[n] += 1
                ti = last_irq[0] if last_irq else t
                for td, idx in pending[n]:
                    lat.append((n, td, max(ti, td)))
                    if last_irq and last_irq[1] < idx:
                        late_gap.append(td - ti)
                pending[n] = []
                keep = []
                for td, idx in pending_s[n]:
                    if last_irq and last_irq[1] > idx:
                        lat_strict.append(ti - td)
                    else:
                        keep.append((td, idx))
                pending_s[n] = keep
        elif name == 'apple_gfx_new_frame':
            frames.append(t)
        elif name == 'apple_gfx_mmio_iosfc_write' and off == 0x1018:
            ios_pending.append(t)
        elif name == 'apple_gfx_iosfc_raise_irq':
            ios_lat += [t - x for x in ios_pending]
            ios_pending = []
    unmatched = sum(len(v) for v in pending.values())
    end = ev[-1][0]

    # Host silent: a doorbell, then for > 100 ms no interrupt at all and no
    # read of its FIFO's bit (which an interrupt printed just before the
    # doorbell's line can bring).
    silent = []
    for td, n in doors:
        k = bisect.bisect_right(irqs, td)
        nxt = irqs[k] if k < len(irqs) else end
        br = bit_reads[n]
        j = bisect.bisect_right(br, td)
        nb = br[j] if j < len(br) else end
        if min(nxt, nb) - td > 0.1:
            silent.append((td, min(nxt, nb), n))
    slow = sorted((td, ti, n) for n, td, ti in lat if ti - td > 0.1)

    def merge(xs):
        out = []
        for t0, t1, n in xs:
            if out and t0 <= out[-1][1]:
                out[-1][1] = max(out[-1][1], t1)
                out[-1][2].add(n)
            else:
                out.append([t0, t1, {n}])
        return out
    # QEMU's os_log events, by time: an interval lists those within 0.5 s of
    # it. Metal's shader compiles log there ("Metal Compiling Shader").
    log_lines = []
    ql = d / 'qemu-oslog.ndjson'
    if ql.exists():
        for line in ql.read_text().splitlines():
            try:
                e = json.loads(line)
                t = datetime.datetime.strptime(e['timestamp'][:26], '%Y-%m-%d %H:%M:%S.%f')
                tz = e['timestamp'][26:].strip()
                off = datetime.timedelta(hours=int(tz[:3]), minutes=int(tz[0] + tz[3:5]))
                log_lines.append(((t - off).replace(tzinfo=datetime.timezone.utc).timestamp(),
                                  e.get('eventMessage', '')))
            except (ValueError, KeyError):
                continue
    times = [e[0] for e in ev]

    def context(t0, t1):
        c = collections.Counter()
        for t, name, off, val in ev[bisect.bisect_left(times, t0):bisect.bisect_right(times, t1)]:
            c[name if off is None else f'{name} {off:#x}'] += 1
        logs = collections.Counter(m[:100] for t, m in log_lines if t0 - 0.5 <= t <= t1 + 0.5)
        return {'where': where(t0), 'events': dict(c.most_common(10)),
                'log': [f'{n}x {m}' for m, n in logs.most_common(6)]}
    stalls = [{'t0': a, 't1': b, 'ms': round((b - a) * 1000, 1), 'fifos': sorted(f), **context(a, b)}
              for a, b, f in merge(silent)]
    slow_fifo = [{'t0': a, 't1': b, 'ms': round((b - a) * 1000, 1), 'fifos': sorted(f), **context(a, b)}
                 for a, b, f in merge(slow)]

    # Frames while a burst runs: consecutive new frames within [t0, t1].
    fi = {'continuous': [], 'keyed': []}
    per_burst, burst_iv = {}, {}
    for b in r.get('bursts', []):
        kind = BURST_KIND.get(b['burst'], b['kind'])
        fr = [t for t in frames if b['t0'] <= t <= b['t1']]
        iv = [y - x for x, y in zip(fr, fr[1:])]
        fi[kind] += iv
        burst_iv[b['burst']] = (b['t0'], b['t1'], kind, iv)
        per_burst[b['burst']] = {'kind': kind, 'frames': len(fr), 'secs': round(b['t1'] - b['t0'], 2),
                                 'p50_ms': round(pct(iv, 50) * 1000, 1) if iv else None,
                                 'max_ms': round(max(iv) * 1000, 1) if iv else None,
                                 'gaps100': sum(x > 0.1 for x in iv)}
    frame_stats = {k: {'n': len(v), 'p50': pct(v, 50) * 1000, 'p99': pct(v, 99) * 1000,
                       'max': max(v) * 1000 if v else float('nan'),
                       'gt100': sum(x > 0.1 for x in v), 'gt250': sum(x > 0.25 for x in v),
                       'gt500': sum(x > 0.5 for x in v)} for k, v in fi.items()}

    def dist(xs, qs=(50, 90, 99, 99.9)):
        xs = [x * 1000 for x in xs]
        return {q: pct(xs, q) for q in qs} | {'max': max(xs) if xs else None, 'n': len(xs)}
    ms = [(ti - td) * 1000 for n, td, ti in lat]
    ms_desk = [(ti - td) * 1000 for n, td, ti in lat if desk and td >= desk]
    cpu = r.get('cpu', {})
    qpid = str(r.get('qemu_pid'))
    ru = r.get('qemu_rusage') or {}
    vcpu = sum(c for n, c in r.get('qemu_threads', []) if n.startswith('CPU '))
    qemu_cpu = ru.get('user', 0) + ru.get('system', 0) if ru else cpu.get(qpid, [float('nan')])[0]
    # QEMU's live threads just before quit, by name: vCPUs, QEMU's main loop
    # and its thread pool ("worker", block I/O), the library's threads, and
    # the unnamed ones (the main thread, which serves the main dispatch
    # queue, and libdispatch's workers). apple-gfx's MMIO reads and writes and
    # its frame encodes call the library on a GLib pool's threads ("pool-N"),
    # counted with the other named threads. Threads that had exited are only
    # in the total ("exited").
    groups = collections.Counter()
    for n, c in r.get('qemu_threads', []):
        g = ('vCPU' if n.startswith('CPU ') else n if n in ('qemu_main', 'worker') else
             'unnamed' if not n else 'library' if n.startswith(LIBRARY_THREAD_PREFIX) else
             'other')
        groups[g] += c
    if ru:
        groups['exited'] = qemu_cpu - sum(groups.values())
    return {
        'dir': str(d), 'label': run_label(r),
        'tour': r.get('tour_done'), 'perf': r.get('perf_done'),
        'hang': r.get('hang'), 'secs': r.get('secs'), 'desktop_secs': r.get('desktop_secs'),
        'net': dict(zip(('packets', 'bytes', 'syn'), net_stats)),
        'doorbells': len(doors), 'fifo_doorbells': dict(fifo_doors), 'matched': len(lat),
        'unmatched_at_end': unmatched, 'stray_bits': dict(stray_bits),
        'irqs': len(irqs), 'frames': len(frames),
        'door_irq_ms': dist([x / 1000 for x in ms]),
        'door_irq_desktop_ms': dist([x / 1000 for x in ms_desk]),
        'door_irq_strict_ms': dist(lat_strict),
        'door_irq_by_fifo_ms': {n: dist([(ti - td) for f, td, ti in lat if f == n and desk and td >= desk])
                                for n in sorted(fifo_doors)},
        'irq_before_line': {'n': len(late_gap), 'over_1ms': sum(g > 0.001 for g in late_gap)}
                           | {k: v for k, v in dist(late_gap).items() if k != 'n'},
        'irq_to_read_ms': dist(irq_to_read),
        'iosfc_ms': dist(ios_lat),
        'mmio_spacing_ms': {k: dist(v, (10, 50, 90, 99)) for k, v in spacing.items()},
        'frame_ms': frame_stats, 'bursts': per_burst,
        'stalls': stalls, 'slow_fifo': slow_fifo,
        'cpu': {'qemu': qemu_cpu, 'qemu_vcpu': vcpu, 'qemu_other': qemu_cpu - vcpu if vcpu else None,
                'qemu_threads': dict(groups)},
        'lat_ms': ms, 'lat_desk_ms': ms_desk, 'lat_strict_ms': [x * 1000 for x in lat_strict],
        'lat_raw': [(td, (ti - td) * 1000) for n, td, ti in lat if desk and td >= desk],
        'burst_iv': burst_iv, 'net_rate': net_rate,
    }


def mann_whitney(a, b):
    """U of a, and the exact two-sided p-value (no ties correction; small n)."""
    n, m = len(a), len(b)
    u = sum((x > y) + 0.5 * (x == y) for x in a for y in b)
    pool = list(a) + list(b)
    us = []
    for idx in itertools.combinations(range(n + m), n):
        s = set(idx)
        aa = [pool[i] for i in idx]
        bb = [pool[i] for i in range(n + m) if i not in s]
        us.append(sum((x > y) + 0.5 * (x == y) for x in aa for y in bb))
    mean = n * m / 2
    p = sum(abs(x - mean) >= abs(u - mean) - 1e-9 for x in us) / len(us)
    return u, p


def perf(a):
    """analyse() each run, print one line per run, write DIR/perf.json, and,
    given runs of two labels, compare them: per-boot values side by side
    with an exact Mann-Whitney test, and the pooled doorbell latencies with
    a bootstrap over boots."""
    runs = [analyse(d) for d in a.dirs if (Path(d) / 'result.json').exists()]
    by = collections.defaultdict(list)
    big = ('lat_ms', 'lat_desk_ms', 'lat_strict_ms', 'lat_raw', 'burst_iv', 'net_rate')
    for x in runs:
        by[x['label']].append(x)
        f = x['frame_ms']['continuous']
        dd = x['door_irq_desktop_ms']
        print(f"{Path(x['dir']).name}: tour={x['tour']} perf={x['perf']} doorbells={x['doorbells']} "
              f"irqs={x['irqs']} frames={x['frames']} | door->irq (desktop) ms p50={dd[50]:.3f} "
              f"p90={dd[90]:.3f} p99={dd[99]:.2f} p99.9={dd[99.9]:.2f} max={dd['max']:.1f} | "
              f"frames(cont) n={f['n']} p50={f['p50']:.1f} p99={f['p99']:.1f} max={f['max']:.1f} "
              f">100={f['gt100']} >250={f['gt250']} >500={f['gt500']} | stalls={len(x['stalls'])} "
              f"slow_fifo={len(x['slow_fifo'])} | cpu qemu={x['cpu']['qemu']:.1f} "
              f"non-vCPU={x['cpu']['qemu_other'] or float('nan'):.1f}")
        Path(x['dir'], 'perf.json').write_text(json.dumps({k: v for k, v in x.items() if k not in big},
                                                          indent=1, default=str))
    if len(by) != 2 or not all(len(v) >= 2 for v in by.values()):
        return
    (ma, ra), (mb, rb) = sorted(by.items())
    after = lambda s: s['where'] not in ('boot', 'before the desktop')
    rows = [
        ('door->irq p50', lambda x: x['door_irq_ms'][50]),
        ('door->irq p90', lambda x: x['door_irq_ms'][90]),
        ('door->irq p99', lambda x: x['door_irq_ms'][99]),
        ('door->irq p99.9', lambda x: x['door_irq_ms'][99.9]),
        ('door->irq max', lambda x: x['door_irq_ms']['max']),
        ('desk door->irq p50', lambda x: x['door_irq_desktop_ms'][50]),
        ('desk door->irq p90', lambda x: x['door_irq_desktop_ms'][90]),
        ('desk door->irq p99', lambda x: x['door_irq_desktop_ms'][99]),
        ('desk door->irq p99.9', lambda x: x['door_irq_desktop_ms'][99.9]),
        ('desk door->irq max', lambda x: x['door_irq_desktop_ms']['max']),
        ('strict door->irq p50', lambda x: x['door_irq_strict_ms'][50]),
        ('strict door->irq p99', lambda x: x['door_irq_strict_ms'][99]),
        ('strict door->irq p99.9', lambda x: x['door_irq_strict_ms'][99.9]),
        ('irq before line (count)', lambda x: x['irq_before_line']['n']),
        ('irq before line >1ms', lambda x: x['irq_before_line']['over_1ms']),
        ('irq before line p99', lambda x: x['irq_before_line'][99]),
        ('irq->read p50', lambda x: x['irq_to_read_ms'][50]),
        ('irq->read p99', lambda x: x['irq_to_read_ms'][99]),
        ('irq->read max', lambda x: x['irq_to_read_ms']['max']),
        ('iosfc p50', lambda x: x['iosfc_ms'][50]),
        ('iosfc p99', lambda x: x['iosfc_ms'][99]),
        ('doorbell spacing p10', lambda x: x['mmio_spacing_ms']['w0x1020->w0x1020'][10]),
        ('doorbell spacing p50', lambda x: x['mmio_spacing_ms']['w0x1020->w0x1020'][50]),
        ('doorbell spacing p99', lambda x: x['mmio_spacing_ms']['w0x1020->w0x1020'][99]),
        ('read 1018->1014 p50', lambda x: x['mmio_spacing_ms']['r0x1018->r0x1014'][50]),
        ('read 1018->1014 p99', lambda x: x['mmio_spacing_ms']['r0x1018->r0x1014'][99]),
        ('read 1014->102c p50', lambda x: x['mmio_spacing_ms']['r0x1014->r0x102c'][50]),
        ('frame p50 (cont)', lambda x: x['frame_ms']['continuous']['p50']),
        ('frame p99 (cont)', lambda x: x['frame_ms']['continuous']['p99']),
        ('frame max (cont)', lambda x: x['frame_ms']['continuous']['max']),
        ('frame gaps >100 (cont)', lambda x: x['frame_ms']['continuous']['gt100']),
        ('frames (cont)', lambda x: x['frame_ms']['continuous']['n']),
        ('frame p99 (keyed)', lambda x: x['frame_ms']['keyed']['p99']),
        ('frame gaps >250 (keyed)', lambda x: x['frame_ms']['keyed']['gt250']),
        ('frame gaps >500 (keyed)', lambda x: x['frame_ms']['keyed']['gt500']),
        ('stalls', lambda x: len(x['stalls'])),
        ('stalls after desktop', lambda x: sum(map(after, x['stalls']))),
        ('stall ms after desktop', lambda x: sum(s['ms'] for s in x['stalls'] if after(s))),
        ('slow fifo after desktop', lambda x: sum(map(after, x['slow_fifo']))),
        ('doorbells', lambda x: x['doorbells']),
        ('frames', lambda x: x['frames']),
        ('desktop secs', lambda x: x['desktop_secs'] or float('nan')),
        ('guest net MB', lambda x: x['net']['bytes'] / 1e6),
        ('cpu qemu', lambda x: x['cpu']['qemu']),
        ('cpu qemu vCPU', lambda x: x['cpu']['qemu_vcpu']),
        ('cpu qemu non-vCPU', lambda x: x['cpu']['qemu_other'] or float('nan')),
        ('cpu qemu_main', lambda x: x['cpu']['qemu_threads'].get('qemu_main', 0)),
        ('cpu qemu worker (block)', lambda x: x['cpu']['qemu_threads'].get('worker', 0)),
        ('cpu library threads', lambda x: x['cpu']['qemu_threads'].get('library', 0)),
        ('cpu unnamed threads', lambda x: x['cpu']['qemu_threads'].get('unnamed', 0)),
        ('cpu exited threads', lambda x: x['cpu']['qemu_threads'].get('exited', 0)),
    ]
    print(f'\n{"":26s} {ma} (per boot) | {mb} (per boot) | medians | exact Mann-Whitney')
    table = []
    for key, get in rows:
        va, vb = [get(x) for x in ra], [get(x) for x in rb]
        u, p = mann_whitney(va, vb)
        table.append({'row': key, ma: va, mb: vb, 'U': u, 'p': p})
        print(f'{key:26s} {" ".join(f"{v:.3g}" for v in va)} | {" ".join(f"{v:.3g}" for v in vb)} | '
              f'{pct(va, 50):.3g} vs {pct(vb, 50):.3g} | U={u:.1f} p={p:.3f}')
    # Pooled doorbell latencies: a bootstrap over boots (resampling whole
    # boots) of the difference of the pooled percentiles.
    import random
    rnd = random.Random(1)
    pooled = []
    # Both guests of a pair quiet on the network: the seconds in which
    # neither moved more than QUIET_BPS (a guest's own background download,
    # e.g. Software Update's, loads it independently of the build). The
    # doorbells after the desktop in those seconds, and the bursts wholly in
    # them.
    partner = {}
    for x in runs:
        m = re.search(r'-(\d+)$', Path(x['dir']).name)
        if m:
            partner.setdefault(m.group(1), {})[x['label']] = x
    for x in runs:
        m = re.search(r'-(\d+)$', Path(x['dir']).name)
        both = partner.get(m.group(1), {}) if m else {}
        busy = set()
        for y in both.values():
            for sec, b in y['net_rate'].items():
                if b > QUIET_BPS:
                    busy |= set(range(sec - 2, sec + 3))
        x['lat_quiet_ms'] = [v for td, v in x['lat_raw'] if int(td) not in busy]
        x['quiet_share'] = len(x['lat_quiet_ms']) / max(1, len(x['lat_raw']))
        iv = []
        nb = 0
        for t0, t1, kind, ivs in x['burst_iv'].values():
            if kind == 'continuous' and not any(sec in busy for sec in range(int(t0), int(t1) + 1)):
                iv += ivs
                nb += 1
        x['frame_quiet'] = {'bursts': nb, 'n': len(iv), 'p50': pct(iv, 50) * 1000,
                            'p99': pct(iv, 99) * 1000, 'max': max(iv) * 1000 if iv else float('nan'),
                            'gt100': sum(v > 0.1 for v in iv)}
    for key, get in (('quiet share of doorbells', lambda x: x['quiet_share']),
                     ('quiet door->irq p50', lambda x: pct(x['lat_quiet_ms'], 50)),
                     ('quiet door->irq p90', lambda x: pct(x['lat_quiet_ms'], 90)),
                     ('quiet door->irq p99', lambda x: pct(x['lat_quiet_ms'], 99)),
                     ('quiet door->irq p99.9', lambda x: pct(x['lat_quiet_ms'], 99.9)),
                     ('quiet door->irq max', lambda x: max(x['lat_quiet_ms'] or [float('nan')])),
                     ('quiet bursts (cont)', lambda x: x['frame_quiet']['bursts']),
                     ('quiet frame p99 (cont)', lambda x: x['frame_quiet']['p99']),
                     ('quiet frame max (cont)', lambda x: x['frame_quiet']['max']),
                     ('quiet frame >100 (cont)', lambda x: x['frame_quiet']['gt100'])):
        va, vb = [get(x) for x in ra], [get(x) for x in rb]
        u, p = mann_whitney(va, vb)
        table.append({'row': key, ma: va, mb: vb, 'U': u, 'p': p})
        print(f'{key:26s} {" ".join(f"{v:.3g}" for v in va)} | {" ".join(f"{v:.3g}" for v in vb)} | '
              f'{pct(va, 50):.3g} vs {pct(vb, 50):.3g} | U={u:.1f} p={p:.3f}')
    # 400 resamples of whole boots; each takes all four percentiles at once.
    qs = (50, 90, 99, 99.9)
    for field in ('lat_desk_ms', 'lat_strict_ms', 'lat_quiet_ms'):
        ra_ = [np.array(x[field]) for x in ra]
        rb_ = [np.array(x[field]) for x in rb]
        pa = np.percentile(np.concatenate(ra_), qs)
        pb = np.percentile(np.concatenate(rb_), qs)
        diffs = []
        for _ in range(400):
            sa = np.concatenate([ra_[i] for i in rnd.choices(range(len(ra_)), k=len(ra_))])
            sb = np.concatenate([rb_[i] for i in rnd.choices(range(len(rb_)), k=len(rb_))])
            diffs.append(np.percentile(sb, qs) - np.percentile(sa, qs))
        diffs = np.array(diffs)
        for k, q in enumerate(qs):
            lo, hi = float(np.percentile(diffs[:, k], 2.5)), float(np.percentile(diffs[:, k], 97.5))
            pooled.append({'field': field, 'q': q, ma: float(pa[k]), mb: float(pb[k]),
                           'diff': float(pb[k] - pa[k]), 'ci': [lo, hi]})
            print(f'pooled {field} p{q}: {ma} {pa[k]:.3f} ms, {mb} {pb[k]:.3f} ms, {mb}-{ma} '
                  f'{pb[k] - pa[k]:+.3f} ms, 95% CI over boots [{lo:+.3f}, {hi:+.3f}]')
    out = Path(a.dirs[0]).parent / 'perf-compare.json'
    out.write_text(json.dumps({'table': table, 'pooled': pooled}, indent=1, default=str))


# UUIDs, addresses and numbers are masked, in that order.
LOG_MASK = [(re.compile(r'[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}'), 'UUID'),
            (re.compile(r'(0x)+[0-9a-fA-F]+'), '0x#'), (re.compile(r'\d+(\.\d+)?'), '#')]


def logdiff(a):
    """Every distinct os_log line of QEMU (qemu-oslog.ndjson), masked, with
    its level and subsystem:category, counted per boot, per label (a pair's
    two builds): those whose presence differs between the labels, or every
    line of a single label's runs."""
    counts = collections.defaultdict(lambda: collections.defaultdict(list))
    boots = collections.Counter()
    for d in a.dirs:
        d = Path(d)
        rp = d / 'result.json'
        if not rp.exists():
            continue
        r = json.loads(rp.read_text())
        label = run_label(r)
        boots[label] += 1
        per = collections.Counter()
        p = d / 'qemu-oslog.ndjson'
        for line in (p.read_text().splitlines() if p.exists() else []):
            try:
                e = json.loads(line)
            except ValueError:
                continue
            if e.get('eventType') not in ('logEvent', None):
                continue
            msg = e.get('eventMessage', '')
            for rx, sub in LOG_MASK:
                msg = rx.sub(sub, msg)
            key = (e.get('subsystem', ''), e.get('category', ''), e.get('messageType', ''), msg[:200])
            per[key] += 1
        for k, n in per.items():
            counts[k][label].append(n)
    labels = sorted(boots)
    print(f'\n== os_log lines (boots: {dict(boots)}) ==')
    for k in sorted(counts, key=lambda k: (k[0], k[1], k[3])):
        c = counts[k]
        cells = []
        for m in labels:
            v = c.get(m, [])
            cells.append(f'{m}: {len(v)}/{boots[m]} boots, {min(v) if v else 0}-{max(v) if v else 0}')
        same = len({(len(c.get(m, [])) > 0) for m in labels}) == 1
        if same and len(labels) > 1:
            continue
        print(f'{"  " if same else "!!"} {k[0]}:{k[1]} {k[2]}: {k[3]}\n      ' + ' | '.join(cells))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='cmd', required=True)
    r = sub.add_parser('run')
    r.add_argument('--out', required=True)
    r.add_argument('--label', help='names the run in result.json (default: vm)')
    r.add_argument('--vm', default=str(GUEST))
    r.add_argument('--clone')
    r.add_argument('--qemu')
    r.add_argument('--hang-secs', type=float, default=180)
    r.add_argument('--net-wait', type=float, default=90,
                   help='seconds to wait for the guest\'s first packet before booting again')
    r.add_argument('--no-tour', action='store_true')
    r.add_argument('--perf', action='store_true',
                   help='after the tour, the interactive bursts (perf_tour)')
    c = sub.add_parser('compare')
    c.add_argument('a')
    c.add_argument('b')
    pr = sub.add_parser('pair')
    pr.add_argument('--out-base', required=True)
    pr.add_argument('--index', required=True)
    pr.add_argument('--vm', default=str(GUEST))
    pr.add_argument('--perf', action='store_true')
    pr.add_argument('builds', nargs=2, metavar='LABEL[=QEMU]',
                    help='a build to boot: its label, and its '
                         'qemu-system-aarch64 (default: make qemu\'s)')
    m = sub.add_parser('summary')
    m.add_argument('dirs', nargs='+')
    pf = sub.add_parser('perf')
    pf.add_argument('dirs', nargs='+')
    ld = sub.add_parser('logdiff')
    ld.add_argument('dirs', nargs='+')
    a = p.parse_args()
    {'run': run, 'pair': pair, 'compare': compare, 'summary': summary, 'perf': perf,
     'logdiff': logdiff}[a.cmd](a)


if __name__ == '__main__':
    main()
