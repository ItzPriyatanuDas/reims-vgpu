#!/bin/bash
#
# Boot a Virtualization.framework macOS VM under QEMU's vmapple machine, with
# apple-gfx on the reims-vgpu this checkout builds.
#
#   scripts/internal/run_vmapple.sh VM_DIR CLONE_DIR [extra QEMU args...]
#
# VM_DIR     a macosvm VM: vm.json (base64 machineId plist holding the ECID),
#            disk.img, aux.img. Never written to.
# CLONE_DIR  where the boot happens. On first use disk.img and aux.img are
#            APFS-cloned there (cp -c: instant, copy-on-write; CLONE_DIR must be
#            on VM_DIR's volume for that) and aux.img is trimmed of its first
#            16 KiB block into aux.img.trimmed, as docs/system/arm/vmapple.rst
#            in vendor/qemu says. FRESH=1 re-clones.
#
# QEMU runs on reims-vgpu. As `make qemu` links it (default
# build/qemu/qemu-system-aarch64), the library is linked statically from the
# dist tree's libreims_vgpu.a, so QEMU loads nothing of it; the library runs
# the guest's isolated GPU tasks in QEMU's process. A QEMU built earlier
# (QEMU=...) may link the library by name instead, ReimsVGPU.framework or
# libreims_vgpu.dylib, which dyld finds as that QEMU's build recorded it, with
# no DYLD variable set: two VMs on two builds can run at once.
# The script refuses a QEMU that links ParavirtualizedGraphics at all; one
# that links neither ReimsVGPU nor libreims_vgpu must define the C API itself
# (rvg_device_create).
#
# Environment:
#   QEMU      qemu-system-aarch64 (default build/qemu's, above). Its build
#             signs it ad hoc with com.apple.security.hypervisor and no
#             hardened runtime, which keeps DYLD variables.
#   OUT       log directory (default CLONE_DIR). It holds
#             serial.log (the guest's serial console), qemu.log (QEMU's
#             stderr, with the apple_gfx* trace), monitor.sock (the HMP
#             monitor) and qmp.sock (QMP, which can send the
#             absolute pointer events the guest's USB tablet takes; HMP's
#             mouse_move sends relative ones, which the tablet ignores).
#   PRINT_LIBS=1  sets DYLD_PRINT_LIBRARIES for QEMU, so qemu.log lists
#             what dyld loaded. Setting DYLD_PRINT_LIBRARIES in the caller's
#             environment does nothing: this script's /bin/bash is a platform
#             binary, which starts with DYLD_* variables removed.
#   MEM       guest RAM (default 16G).  CPUS  vCPUs (default 8).
#   DISPLAY_OPT  QEMU -display value (default none; cocoa opens a window).
#   WINDOW=1  window mode: under -display none, apple-gfx has the library
#             open a window of its own for each guest display, which draws
#             the guest's frames and takes its pointer and keys; closing it
#             powers the VM off. The script runs QEMU with
#             REIMS_VGPU_WINDOW=0 otherwise, so a run stays headless, as
#             vm_drive.py's tours need; screendump works either way. The
#             library's REIMS_VGPU_WINDOW_* variables (borderless, screens,
#             refresh; crates/reims-vgpu-window/features.md) pass through.
#   RESOLUTION  the guest's mode in window mode: WIDTHxHEIGHT at 1x, scaled by
#             the window on the GPU; WIDTHxHEIGHT@PPI told that density, so
#             a HiDPI desktop at the mode's pixels where the guest reads
#             146.667 ppi or more across the width in whole centimetres, the
#             millimetres rounded to nearest and then up (2560x1600@200 looks
#             like 1280x800; @148 reads 44 cm and runs HiDPI, @146 45 cm and
#             1x), which a windowed window on a Retina screen shows one guest
#             pixel per screen pixel where half the mode fits; apple-gfx
#             refuses a density that does not tell the guest's scale for the
#             mode (docs/qemu.md); or host for the host screen's own modes
#             (4K HiDPI on a Retina screen, much slower); unset is apple-gfx's
#             default, 1920x1080. It sets -global apple-gfx-mmio.resolution.
#   ACCEL     QEMU -accel value (default hvf,kernel-irqchip=on, which
#             vendor/qemu's 4a2e2b3967 boots Monterey with).
#
# Screenshots go through the monitor, while QEMU runs:
#   echo 'screendump /path/shot.png -f png' | nc -U "$OUT/monitor.sock"
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
die() { echo "error: $*" >&2; exit 1; }

[[ $# -ge 2 ]] || die "usage: $0 VM_DIR CLONE_DIR [extra QEMU args...]"
VM_DIR="$1"; CLONE_DIR="$2"; shift 2

QEMU="${QEMU:-$ROOT/build/qemu/qemu-system-aarch64}"; MAKES="make qemu"
[[ -x "$QEMU" ]] || die "$QEMU not found: run $MAKES, or set QEMU"
for f in vm.json disk.img aux.img; do
  [[ -f "$VM_DIR/$f" ]] || die "$VM_DIR/$f not found"
done

# The install name QEMU links a framework by (the first line of otool -L is
# QEMU's own path), or nothing.
linked() { otool -L "$QEMU" | awk -v n="$1" 'NR > 1 && index($1, n) {print $1}'; }

# What QEMU links: reims-vgpu statically, or ReimsVGPU.framework or
# libreims_vgpu.dylib by name, and never ParavirtualizedGraphics. LIB says
# which library runs.
PVG="$(linked ParavirtualizedGraphics)"
[[ -z "$PVG" ]] || die "$QEMU links ParavirtualizedGraphics ('$PVG'): build it with $MAKES"
LIB="$(linked ReimsVGPU)"
[[ -n "$LIB" ]] || LIB="$(linked libreims_vgpu)"
if [[ -z "$LIB" ]]; then
  # Linked statically: the C API is defined in QEMU itself.
  # grep reads to the end: with -q, nm could die of SIGPIPE, failing the pipe.
  nm "$QEMU" 2>/dev/null | grep -E ' [Tt] _rvg_device_create$' >/dev/null \
    || die "$QEMU links no reims-vgpu and defines no rvg_device_create: build it with $MAKES"
  LIB="static, in $QEMU"
fi

mkdir -p "$CLONE_DIR"
CLONE_DIR="$(cd "$CLONE_DIR" && pwd)"
OUT="${OUT:-$CLONE_DIR}"
mkdir -p "$OUT"

if [[ "${FRESH:-0}" == 1 || ! -f "$CLONE_DIR/disk.img" || ! -f "$CLONE_DIR/aux.img" ]]; then
  rm -f "$CLONE_DIR/disk.img" "$CLONE_DIR/aux.img" "$CLONE_DIR/aux.img.trimmed"
  cp -c "$VM_DIR/disk.img" "$VM_DIR/aux.img" "$CLONE_DIR/" \
    || die "cp -c failed: CLONE_DIR must be on the same APFS volume as VM_DIR"
fi
if [[ ! -f "$CLONE_DIR/aux.img.trimmed" ]]; then
  dd if="$CLONE_DIR/aux.img" of="$CLONE_DIR/aux.img.trimmed" \
    bs=$((0x4000)) skip=1 status=none
fi

# vendor/qemu's contrib/vmapple/uuid.sh, inlined.
ECID="$(plutil -extract machineId raw "$VM_DIR/vm.json" | base64 -d \
        | plutil -extract ECID raw -)"
[[ "$ECID" =~ ^[0-9]+$ ]] || die "no ECID in $VM_DIR/vm.json's machineId"

AVPBOOTER=/System/Library/Frameworks/Virtualization.framework/Resources/AVPBooter.vmapple2.bin
AUX="$CLONE_DIR/aux.img.trimmed"
DISK="$CLONE_DIR/disk.img"
MON="$OUT/monitor.sock"
QMP="$OUT/qmp.sock"
rm -f "$MON" "$QMP"

ENV=()
[[ "${PRINT_LIBS:-0}" == 1 ]] && ENV+=("DYLD_PRINT_LIBRARIES=1")
# apple-gfx opens windows under -display none unless REIMS_VGPU_WINDOW is 0.
if [[ "${WINDOW:-0}" == 1 ]]; then
  ENV+=("REIMS_VGPU_WINDOW=1")
else
  ENV+=("REIMS_VGPU_WINDOW=0")
fi

ARGS=()
[[ -n "${RESOLUTION:-}" ]] &&
  ARGS+=(-global "apple-gfx-mmio.resolution=$RESOLUTION")

echo "library=$LIB ecid=$ECID clone=$CLONE_DIR out=$OUT" >&2
exec env ${ENV[@]+"${ENV[@]}"} "$QEMU" \
  -accel "${ACCEL:-hvf,kernel-irqchip=on}" \
  -M vmapple,uuid="$ECID" \
  -m "${MEM:-16G}" -smp "${CPUS:-8}" \
  -bios "$AVPBOOTER" \
  -drive file="$AUX",if=pflash,format=raw \
  -drive file="$DISK",if=pflash,format=raw \
  -drive file="$AUX",if=none,id=aux,format=raw \
  -drive file="$DISK",if=none,id=root,format=raw \
  -device vmapple-virtio-blk-pci,variant=aux,drive=aux \
  -device vmapple-virtio-blk-pci,variant=root,drive=root \
  -netdev user,id=net0,ipv6=off \
  -device virtio-net-pci,netdev=net0 \
  -display "${DISPLAY_OPT:-none}" \
  -serial file:"$OUT/serial.log" \
  -monitor unix:"$MON",server,nowait \
  -qmp unix:"$QMP",server,nowait \
  -trace 'apple_gfx*' \
  ${ARGS[@]+"${ARGS[@]}"} "$@" 2>"$OUT/qemu.log"
