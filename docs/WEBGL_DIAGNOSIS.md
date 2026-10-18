# WebGL in stock Safari and Chrome

Updated 2026-10-18. The current candidate uses one Metal transport for the
Darwin 25 ABI family used by Tahoe 26.x. It selects the stock IOGPU-family
plugin for every client of the Reims accelerator and translates ordinary
type-5 connections, without inspecting process or browser names.
The [ReimsMetalCompat guest bridge](../guest/metal-compat/README.md) and its
matching Windows runtime are both required; neither browser is modified.

Qualification of this common path is in progress. On Tahoe 26.7.1, the
global bridge and matching Windows engine each completed all 22 authored
WebGL stages in stock Safari 27, Chrome 155 and Edge 155. A later Safari
run timed out in both repeated-frame stages; timing and sustained stability
remain unresolved. Before the older validation disks were retired, the
CI-built global source also passed all 22 stages in Safari and Chrome on
kernel 25.4.0. Further point-release matrix testing is outside this task.

## Failure and repair

The original x86 Metal plugin exposes a legacy capability/connection path
that current browser ANGLE initialization rejects. Merely selecting Apple's
newer IOGPU-family plugin is insufficient: its kernel requests do not match
the installed IOAccelerator-family kernel driver's records and selectors.
The bridge translates those requests and keeps the real driver's resource
IDs, compiler information and completion notifications.

| Reproduced failure | Repair and check |
|---|---|
| Initialization and resource creation use incompatible records | Translate device information, compiler data, buffers, textures, surfaces and queue records; native source compilation and GPU readback pass. |
| Resource-list lengths differ from allocations | Validate rounded-up KiB lengths, then send the exact allocation length to the legacy driver; boundary and IOSurface readback checks pass. |
| Continuous drawing rejects reused resource chunks | Read the count at offset `0x3e`; bytes at `0x3c` are padding and may be nonzero. Synthetic padding/count tests retain malformed-input rejection. |
| Shared-event command lists do not match the old transport | Preserve render and kernel lists, translate signal/infinite-wait operations, and retain owned command memory until actual completion. Cross-queue future-event ordering passes. |
| Native batching reaches external method 29 | Validate and prepare the complete batch before one legacy submission; track rollback by batch identity. A 64-command burst passes exact readback. |
| Empty command buffers contain only a modern preamble | Produce the measured empty legacy segment and wait for its real asynchronous callback; 32 empty submissions complete. |
| Completion callbacks deadlock or reorder | Drain outside legacy driver locks using one nonconcurrent thread call; four queues over 100 frames pass ten runs. |
| Browser helper names and roles change | Remove process-name filtering and use the selected accelerator's Metal connection ABI for all clients. Browser branding and helper suffixes are no longer dispatch inputs. |
| Global activation reaches blended framebuffer fetch | Blend direct UNORM attachments inside the existing ordered pixel interlock, preserving channel masks. Authored RGBA/BGRA full/partial-mask GPU cases pass; the matching engine cold-boots Tahoe 26.7.1. |
| Post-login WindowServer compute initialization is rejected | Replace the sampled 56-byte gate with the measured record class: a 32-byte prefix followed by a nonempty, four-byte-aligned operation stream. Validate arguments and forward valid bytes unchanged. Boundary tests and the 26.7.1 desktop/browser boot pass. |

The previous bridge selected browser helpers by executable name. Chrome's
helper naming changed on a later guest, exposing why that approach did not
cover other browsers or OS updates. The candidate removes that selection
mechanism; WindowServer and ordinary authored Metal probes use the same path.

The bridge does not invent successful GPU completions, enable SwiftShader,
force browser GPU acceptance, disable visual effects, or disable SIP/AMFI.
Unsupported translations fail explicitly. A host capability experiment failed
the cold-boot check and was withdrawn. The replacement implements the reached
framebuffer-fetch operation instead of changing the device's answers. Global
activation remains a candidate until the OS-family, repeated-frame and
performance checks pass. Keep writable overlays and the previous firmware
and engine for rollback.

## Global-path observations

The tested combination uses guest bridge source `700379ce`, the engine
from Reims `b8b4486c`, QEMU `7100064d` and runtime `ea5d6b87`. Both the
stock desktop and browsers use the same Metal route. There is no executable
name or helper-role condition. Chrome and Edge reported the Metal ANGLE
renderer on the Apple paravirtual device; Safari reported Apple GPU.

| Tahoe 26.7.1 first completed run | WebGL 1 rAF FPS | WebGL 2 rAF FPS | Authored stages |
|---|---:|---:|---|
| Safari 27 | 41.80 | 8.83 | 22/22 passed |
| Chrome 155 | 40.15 | 60.00 | 22/22 passed |
| Edge 155 | 49.49 | 55.95 | 22/22 passed |

These short callback measurements do not establish stable 60 FPS. A later
visible Safari run completed only 139/180 and 179/180 frames within each
30-second limit, with maximum JavaScript draw callback times of 1 and 8 ms.
The timeouts are retained as failures. No context loss or pixel failure was
reported in the remaining stages. The source of the long frame intervals
has not yet been isolated.

A 30.99-second Chrome TestUFO trace recorded 1,162 guest-frame callbacks,
17.49 ms median, 51.45 ms p95 and 180.39 ms p99 intervals, with a 622.55 ms
maximum. Its average callback rate was about 38/s. TestUFO displayed a sync
failure. This is not a matched before/after result: the earlier baseline
used Safari on a different OS build. Frame callbacks are not physical
monitor scanout.

Guest-initiated restart returned to the login screen using the same QEMU
process. A subsequent normal guest shutdown reached QMP `shutdown` while
retaining that process; explicit exit then released it. These checks do not
replace repeated desktop/browser stress after login.

## Historical browser-scoped acceptance

Both APIs pass context/version checks, GLSL compilation/linking,
triangle/VBO pixels, texture upload, FBO/readPixels, blending, depth,
180 rendered frames, resource lifetime, deliberate loss/recovery and
unexpected context-loss checks. Chrome's live GPU helper was also observed
loading the stock IOGPU-family Metal plugin. Native authored probes verify
compute, managed uploads, memoryless MSAA resolve, IOSurfaces, shared events,
resource reuse, batches and empty-command completion.

The acceptance runs used 1920x1080, 8 vCPUs and 4 GiB guest RAM on a contended
Windows host. Browser callback rates from the 180-frame stage were:

| Browser | WebGL 1 rAF FPS | WebGL 2 rAF FPS |
|---|---:|---:|
| Safari 27 | 19.20 | 30.76 |
| Chrome 155 | 50.68 | 54.77 |

These are requestAnimationFrame intervals, not physical display presents or
a matched performance comparison. They do **not** establish stable 60 FPS.
[TestUFO](https://testufo.com/) remains the requested performance workload.
4K, extended stress and full Khronos conformance remain unqualified.

Context recovery must wait until after dispatch of the loss event, when
WebKit records `preventDefault()`. The repeated-frame fixture reuses a VBO,
keeps the canvas visible and cancels rAF on timeout; independent upload and
lifetime cases remain. Expected pixels and error checks were not relaxed.

## OS-family qualification, build and rollback

| Guest | Driver / Metal plugin | Evidence |
|---|---|---|
| Tahoe 26.4, build 25E246, kernel 25.4.0 | AppleParavirtGPU 15.0.0 / 64.4.7 | Historical CI-built global-source cold boot and 22/22 Safari and Chrome stages passed before retirement; no further tests planned. |
| Tahoe 26.7.1, build 25G241, kernel 25.6.0 | AppleParavirtGPU 15.0.0 / 64.5 | OS update and 300 GiB APFS expansion verified on a copy; global-path cold boot, desktop and first Safari/Chrome/Edge suites pass. A Safari frame-timeout rerun keeps stability qualification open. |

The compatibility boundary is the driver ABI, rather than a browser name or
one OS point release. Family-wide OpenCore bounds are an experimental target,
not proof that every 26.x build has been tested. A release records its ABI
bounds, exact source revision, required engine and actually qualified kernels.
Unknown record shapes still fail explicitly. Future observed ABI changes
need a class-level translation fix and its focused proof; adding browser
names or point-release exceptions is not the compatibility mechanism.

Export from the committed source with `source_snapshot.py`, following the
[build instructions](../guest/metal-compat/README.md). Dirty input is refused;
the build uses a private copy, verifies hashes before publication and includes
its source manifest in the signed kext. Pair results with that manifest.
A commit alone does not prevent an external process from rewriting a live
working file after the build. Unexpected writers need separate attribution.

The separately adapted WebKit used during diagnosis is not required for this
stock-browser fix and is not the supported delivery path. Private captures,
raw reports, screenshots and proprietary guest binaries stay outside Git.
Only authored bridge source, probes, synthetic tests and documentation are
published. Preserve the previous engine, disk, firmware and NVRAM for rollback.
