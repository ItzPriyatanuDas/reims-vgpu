# Native Windows validation

Updated 2026-10-18. The synchronization table below records the earlier
checkpoint; current global-path observations are recorded separately below.
Historical host: RTX 4070 SUPER (10de:2783), NVIDIA 616.56,
32 GiB RAM; native GNU Rust 1.98.1 and MSYS2 UCRT64 LLVM/Clang 22.1.8.
All explicitly generated artifacts stay inside the project.

## Earlier synchronization checks

| Check | Observed result |
|---|---|
| Initial synchronized runtime units | 845 passed, 1 existing ignored fixture |
| Runtime product clippy, four crates, all targets/features | Passed with warnings denied before calibration |
| Runtime rustdoc | Passed with warnings denied after Unix-only API link fixes |
| Native host sections/private pages/reversed 4 KiB views | Bidirectional GPU readback passed |
| Native texture coherence | 24 shared/managed CPU/GPU rounds passed, plus BC3 edge-block copy |
| Native private memory | 4/8/16 KiB, 1/64/4096/262144 threads passed; no validation errors |
| Host-created-image copy path | Not exercised: driver counter remained zero; staging path exercised |
| Initial synchronized Reims release | Passed; final calibrated dependency rebuild pending |
| Firmware | Rebuilt; ROM bytes identical to the preserved engine's ROM |
| Native C host harness against release | Passed (`harness: ok`) |
| Reims full check before calibration | 2884/2895 passed, 11 failed, 2 skipped; doctests passed; **not green** |
| Core 32 GiB boundary under the full check | Passed unfiltered with test-only sparse mapping |
| QEMU native build | Passed, 1770 build steps |
| QEMU AppleSMC / xHCI tests | 1 / 7 passed |
| Frame-trace parser | Quantiles, absent timestamps and backward-clock cases passed |
| Frame-trace live collection | Timestamped callbacks and NVIDIA samples collected |
| Baseline WHPX boot | Installed 26.4 (25E246), SSH/login/desktop/Safari reached |
| Baseline shutdown | QMP shutdown and clean QEMU exit; exceeded 50-second deadline |
| Baseline native Metal source/compute/readback | Passed; 64 uint results matched |
| Baseline Safari WebGL 1/2 | Failed: contexts already lost during initialization |
| Baseline Chrome WebGL 1/2 | Failed: no contexts, EGL/GPU initialization failed |
| Stock Safari 27 with the x86 bridge, WebGL 1/2 | 22/22 authored acceptance stages passed |
| Stock Chrome 155 with the x86 bridge, WebGL 1/2 | 22/22 authored acceptance stages passed |
| Native bridge probes | Resource reuse, managed uploads, IOSurfaces, shared events, batches and empty completions passed |
| Earlier synchronized Reims check | 2,895 passed, two existing skips |
| Runtime product checks | 4,543 passed, 22 existing ignored cases |
| Source snapshot tests | Three tests passed |
| Snapshot-based macOS kext build | Passed, including strict code-signature verification |
| Mid-build source mutation | Rejected; previous signed bundle and manifest unchanged |
| Sustained performance / 4K / newer macOS at that checkpoint | Pending |

The ten initial strict render failures shared an unmeasured
`FunctionStorageAssumed` notice on this GPU. A single test rerun reproduced
it, excluding suite load as its cause. The memory floor is now measured on
this exact driver; strict policy is retained. The remaining surface fixture
incorrectly used section-backed Windows pages for its no-backing case; it
now allocates genuinely private pages. These are not relaxed comparisons.

A subsequent `cargo test` of all Vulkan tests in one process exposed shared
ID/state interference. The repository's nextest harness isolates test
processes; its result is the gate, not the single-process run.

## Performance evidence and limits

User workload: [TestUFO](https://testufo.com/), Safari, 1920x1080 guest mode,
window bounds 1920x1015. No guest effects disabled and no host priority or
policy override. Restored apps/background activity must be recorded.

The initial 30.48-second baseline under restored application load averaged
38.45 guest/window callbacks per second and 825.58% QEMU CPU (100% is one
logical core). It did not establish the reported stable 60 FPS.

A 12.95-second instrumentation check collected 255 callbacks: median 19.07
ms, p95 82.50 ms, p99 1016.72 ms. It overlapped page loading and CPU tests,
so it validates instrumentation only and is excluded from regression claims.
TestUFO's visible nominal 60 FPS label coexisted with a sync-failure warning.

Private raw logs, timestamps, JSON and screenshots live beneath
`.work/diagnostics/upstream-sync-20261014` and the two validation VM log
folders. They are not published: guest logs and screenshots can contain
personal information. No Apple binaries or guest shader captures are
included in commits.

## Stock-browser acceptance

For the later global-path source `700379ce` on Tahoe 26.7.1, the first Safari
27, Chrome 155 and Edge 155 runs each passed all 22 authored stages. The
matching engine uses runtime `ea5d6b87`. Before the old disks were retired,
the CI-built global bridge also passed all 22 stages in Safari 27 and Chrome
155 on kernel 25.4.0. Those completed runs predate the instruction to stop
26.4/version-matrix tests; none will be repeated for this task.

A visible Safari rerun failed both unchanged 30-second repeated-frame
limits: 139/180 and 179/180 frames. The other pixel/resource stages passed;
these timeouts remain failures. A Chrome TestUFO 30.99-second trace recorded
1,162 guest-frame callbacks, with 17.49 ms median, 51.45 ms p95 and 180.39 ms
p99 intervals; TestUFO displayed a sync failure. This cannot establish
stable 60 FPS or a matched regression result against Safari on 26.4.

The 26.7.1 guest restarted to its login screen in the same QEMU process,
then reached QMP `shutdown` through a normal guest power-off. Explicit exit
released the retained process. Its 300 GiB base passed `qemu-img check` and
full `qemu-img compare` against the retained source. APFS expansion was
verified inside the guest before consolidation.

The standalone manager's combined CI build passed 57 rendered WPF pages,
native embedded-console fixture lifecycle, new-process profile persistence,
twice-stale multi-instance merging, backup recovery and import checks.
Two actual no-argument launches also retained the normal profile's selected
300 GiB VM, engine and other registrations. Real USB replug, audible output
and microphone signal are not established by mocked USB tests or endpoint
enumeration. Current clipboard GUI polling still needs real guest acceptance.
The real installed launch chain subsequently exposed an output-EOF hang that
the native-window fixture did not cover: its launcher exited while QEMU kept
a pipe open. Manager busy state blocked polling and automatic embedding.
The new detached-child regression and real-VM fix acceptance are in progress.

## Combined native merge gate

On the combined global-bridge tree, all 2,895 selected Reims tests passed
with two existing skips. Header checks, formatting, all-features and
no-default-features Clippy, and doctests passed. Initial invocations exposed
mixed MSYS/private Rust versions and an SDL CMake/GCC environment failure;
the remaining stages resumed with one explicit GNU toolchain and UCRT Clang.
These environment failures were retained, not counted as a clean first run.

Runtime CI at `ff0bbb05` completed the Ubuntu gate: 4,561 product/devtool
tests passed, 22 skipped; 1,293 validation tests passed, four skipped; 2,665
CPU-ICD validation tests passed, 43 skipped; docs, Clippy, packaging and
public-case native Vulkan qualification passed. The current runtime head
still requires its own complete Linux/macOS CI result. Its later changes
affect test code and actual-observation inventory rows, not the production
runtime pinned by the host.

The bridge source is preserved in `2d690d74`; snapshot-based builds were
introduced in `303f2356`. Both stock browsers passed the same 22 authored
stages: context/version, shader/link, triangle/VBO, textures, FBO/readPixels,
blend/depth, 180 frames, resource lifetime, deliberate recovery and unexpected
loss. Chrome's live GPU helper loaded the stock IOGPU-family Metal plugin.

Safari's 180-frame stages measured 19.20/30.76 rAF FPS and Chrome's measured
50.68/54.77 for WebGL 1/2. These runs establish functional acceptance on a
contended 1080p guest, not a matched 60-FPS performance result. They predate
any OS update and do not qualify a newer driver ABI. The source-integrity
checks alter packaging, not GPU behavior; their new signed artifact has not
been used as evidence for an additional browser or performance run.
