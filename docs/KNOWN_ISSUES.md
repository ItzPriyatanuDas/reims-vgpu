# Known issues

Updated 2026-10-18. The global Metal candidate completes the authored GPU
WebGL 1/2 suite in stock Safari 27, Chrome 155 and Edge 155 on Tahoe 26.7.1.
A subsequent Safari run timed out in its two repeated-frame stages. This
does not establish sustained stability or production readiness.

## Experimental driver scope

The candidate targets the Darwin 25 driver ABI family. Process-name
selection has been removed: WindowServer and ordinary accelerator clients
use the same route. Its 26.7.1 boot and first browser suites pass; the earlier
26.4 guest also completed Safari and Chrome suites through the global source
before its validation disks were retired. Sustained timing remains unresolved.
OpenCore bounds alone are not evidence for every 26.x build. Finite
shared-event waits and other untranslated commands fail explicitly.
Resource cleanup under failure and mip/slice preservation need further
stress coverage. The manager's guest package remains explicitly marked
as a candidate rather than a qualified family-wide release.

## Display width and stalls

Upstream documents an x86 Tahoe WindowServer/plugin tile-texture failure
above 2048 pixels wide. Initial qualification uses 1920x1080. Higher display
choices remain available with a warning; availability is not proof of stable
1440p or 4K operation.

Restored applications and Simulator services can delay normal shutdown.
Distinguish guest application responsiveness, window/frame progress and QMP
power state before diagnosing a GPU deadlock. A prior shutdown exceeded the
50-second ACPI deadline but subsequently reached QMP shutdown and exited.

## Performance and validation limits

- The reported stable 60 FPS has not yet been reproduced in a qualified,
  matched TestUFO comparison. Safari's initial nominal 60 FPS label also
  displayed a synchronization failure.
- On the 26.7.1 global path, first completed Safari/Chrome/Edge runs pass
  their pixels and resource checks. A later Safari run timed out after
  139/180 and 179/180 visible frames. The original 30-second checks remain
  unchanged; the regression is not hidden by a longer timeout.
- A Chrome TestUFO interval averaged about 38 guest callbacks/s and retained
  long frame gaps. The earlier Safari/26.4 baseline is not a matched
  comparison. See the exact observations in [WEBGL_DIAGNOSIS.md](WEBGL_DIAGNOSIS.md).
- Frame traces measure guest-frame callbacks. Window statistics measure host
  submission/completion latency; adapter samples include other applications.
- The full macOS behavior-lock suite requires its macOS runner. The Windows
  C harness and runtime tests do not replace it.
- Long-run device loss and all-resolution visual qualification remain open.
  The tested VM was updated to 26.7.1 with a 300 GiB disk and expanded APFS
  container. Old validation disks were retired after the independent base
  passed a full content comparison; logs and metadata were retained.

The installed manager exposed a real launch-chain failure: QEMU started but
the manager waited for redirected output EOF after its launcher exited.
This also prevented automatic console embedding. A native detached-child
fixture reproduces it; the fix and actual-VM acceptance are in progress.

## Working-file integrity

A verified bridge source file was observed reverting after a commit while
its known-good build remained intact. The writer is not yet attributed.
Source snapshot export now rejects dirty input, builds use a verified copy,
and the signed bundle records the exact source manifest. This prevents a
mismatched rebuild from inheriting earlier test results; it does not identify
or repair an external writer. Keep protected backups while investigating.
