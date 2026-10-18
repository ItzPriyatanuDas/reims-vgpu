# Windows native Tahoe: current implementation

Updated 2026-10-18. The development branch retains the working Windows-native
QEMU/WHPX desktop, Apple Account configuration, JIS input, USB selection,
OpenCore controls and VM management. Production rollout remains pending.

## Development branch

`windows-native-tahoe-upstream-sync-20261014` integrates upstream Reims,
its QEMU branch and metal2vulkan while retaining Windows section sharing,
reset/topology handling, SDL3/Vulkan and management scripts. See
[UPSTREAM_SYNC.md](UPSTREAM_SYNC.md) for exact integration decisions.
The preserved baseline is `68d0a4d7`, with QEMU `c1dbb187` and runtime
`8556e6e4`; candidate checks use separate disk, firmware and NVRAM copies.

The development tree adds a native Windows C harness, authored WebGL and
Metal probes, frame-callback instrumentation, Windows runtime calibration
and the experimental x86 Metal user-client bridge. The bridge selects the
stock IOGPU-family plugin for every Reims accelerator client, translating its
requests into the installed legacy kernel driver's ABI. Browser binaries,
SIP and AMFI remain unchanged.

The global candidate cold-boots Tahoe 26.7.1 and completes the first authored
Safari, Chrome and Edge WebGL suites. A variable-length compute request fix
retains post-login WindowServer operation. The VM's disk and APFS container
were expanded to 300 GiB; a consolidated base was checked and compared with
the retained source before a new writable overlay was created.

The standalone Fluent manager has a VM library, embedded native console,
hardware/settings pages, dependency setup and backed-up persistent profiles.
Actual normal launches from two different working directories retained the
selected VM, engine and existing registrations without command-line profile
overrides. The app, engine and active VM now use ordinary user-facing
directories; development caches remain separate.

## Verification

The combined global-path native gate passed. Runtime CI on its final
test/inventory follow-up remains in progress; its earlier Ubuntu head passed
the full validation and CPU-ICD qualification stages.

- Reims check stages: 2,895 selected tests passed, two existing skips; header,
  formatting, both Clippy configurations and doctests passed. Toolchain/PATH
  failures were repaired before resuming the remaining stages.
- Earlier runtime CI: 4,561 tests passed, 22 skipped; validation 1,293 passed,
  four skipped; Linux CPU-ICD validation 2,665 passed, 43 skipped.
- Native release, QEMU and C harness build/run checks passed.
- Stock Safari 27 and Chrome 155: each passed all 22 GPU WebGL 1/2 stages on
  Tahoe 26.4, including pixels, 180 frames, resource lifetime and loss/recovery.
- Source snapshot tests, actual macOS kext build and signature verification
  passed. A deliberately changed input during compilation was rejected while
  preserving the previous signed output.

Stable 60 FPS, 4K and extended resource/device-loss stress remain unqualified.
A Safari repeated-frame rerun timed out despite its first full-suite pass.
Before retirement of the old validation disks, the same global bridge source
also completed the stock Safari and Chrome suites on kernel 25.4.0. Further
26.4/version-matrix tests are outside the current task; compatibility is
implemented against the common ABI rather than a point-release allowlist.
The separate diagnostic
WebKit app is not required for the stock-browser fix. See
[TEST_RESULTS.md](TEST_RESULTS.md), [KNOWN_ISSUES.md](KNOWN_ISSUES.md) and
[WEBGL_DIAGNOSIS.md](WEBGL_DIAGNOSIS.md) for evidence and limits.
