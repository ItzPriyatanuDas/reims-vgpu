#!/bin/bash
# The GPU locks a run of ours on this host takes for the GPUs it uses, which the GPU work of
# metal2vulkan's runs on the same hosts takes too, so that no two runs share a GPU (AGENTS.md,
# "GPU and VM discipline").
#
#   scripts/internal/gpu-locks.sh [--node PATH]... [--device-id 0xNNNN]... [--all]
#       print, one a line, the lock files the run takes, in the order it takes them
#   scripts/internal/gpu-locks.sh [--node PATH]... [--device-id 0xNNNN]... [--all] -- CMD [ARG]...
#       run CMD holding them, with RVG_GPU_LOCKS_HELD=1 in its environment
#
# --node names a GPU by its render node, --device-id by its PCI device ID, as Vulkan's loader
# filters it (VK_LOADER_DEVICE_ID_FILTER), and --all names every GPU on the host. A node no GPU
# is behind, or a run that names none, takes every GPU's locks. A caller that takes the printed
# files itself takes each in the order printed (`exec {fd}<"$lock"; flock -x "$fd"`), the one
# order of every run, so that two runs never wait on each other crosswise.
#
# On Linux a GPU is told by its PCI vendor and device IDs, not its node's number, which follows
# the PCI order and may change with a reboot or a new card. The files, each taken only where it
# exists:
# - metal2vulkan's, one a GPU: `~/.cache/rvg-vkr2-gpu-7900xtx.lock` for an RX 7900 XTX
#   (1002:744c), `~/.cache/rvg-vkr2-gpu-780m.lock` for a Radeon 780M (1002:15bf), and
#   `~/.cache/rvg-vkr2-gpu.lock` for an NVIDIA GPU (10de);
# - ours for an NVIDIA GPU, `~/rvgv/gpu.lock`;
# - the older shared ones for an AMD GPU, `~/.cache/m2v-orch/gpu-radv.lock` and
#   `~/.cache/m2v-gpu/run-gpu-radeon_icd.json.lock`.
# They are opened read-only, and none is made.
#
# Inside a run that holds them (RVG_GPU_LOCKS_HELD=1, which the run form sets), the print form
# prints nothing and the run form runs CMD at once: a second lock of a file this run holds, through
# a file of its own, would wait on the run itself.
#
# On macOS the Mac's one GPU lock is taken by metal2vulkan's `~/.cache/m2v-orch/maclock`, which
# queues its callers in their order of arrival: the run form runs CMD under it where it exists,
# and the print form prints nothing, as no file is the caller's to take.
set -euo pipefail

usage() {
  echo "usage: $0 [--node PATH]... [--device-id 0xNNNN]... [--all] [-- CMD [ARG]...]" >&2
  exit 2
}

# Every GPU on the host, as "vendor:device" of its render node's PCI device.
every() {
  local n
  for n in /sys/class/drm/renderD*; do
    [[ -r $n/device/vendor && -r $n/device/device ]] || continue
    echo "$(cat "$n/device/vendor"):$(cat "$n/device/device")"
  done
}

ids=()
all=0
named=0
run=()
while (($# > 0)); do
  case $1 in
    --node)
      (($# >= 2)) || usage
      named=1
      n=/sys/class/drm/$(basename "$2")
      if [[ -r $n/device/vendor && -r $n/device/device ]]; then
        ids+=("$(cat "$n/device/vendor"):$(cat "$n/device/device")")
      else
        all=1
      fi
      shift 2
      ;;
    --device-id)
      (($# >= 2)) || usage
      named=1
      found=0
      for id in $(every); do
        if [[ ${id#*:} == "$(printf '0x%04x' "$2")" ]]; then
          ids+=("$id")
          found=1
        fi
      done
      ((found)) || all=1
      shift 2
      ;;
    --all)
      all=1
      named=1
      shift
      ;;
    --)
      shift
      (($# > 0)) || usage
      run=("$@")
      break
      ;;
    *) usage ;;
  esac
done

if [[ ${RVG_GPU_LOCKS_HELD:-} == 1 ]]; then
  ((${#run[@]} > 0)) || exit 0
  exec "${run[@]}"
fi

if [[ $(uname) == Darwin ]]; then
  ((${#run[@]} > 0)) || exit 0
  export RVG_GPU_LOCKS_HELD=1
  maclock=$HOME/.cache/m2v-orch/maclock
  if [[ -x $maclock ]]; then
    exec "$maclock" "${run[@]}"
  fi
  exec "${run[@]}"
fi

((named)) || all=1
if ((all)); then
  for id in $(every); do ids+=("$id"); done
fi

xtx=0 m780=0 nvidia=0 amd=0
for id in ${ids[@]+"${ids[@]}"}; do
  case $id in
    0x1002:0x744c) xtx=1 amd=1 ;;
    0x1002:0x15bf) m780=1 amd=1 ;;
    0x1002:*) amd=1 ;;
    0x10de:*) nvidia=1 ;;
  esac
done

# The one order: metal2vulkan's per-GPU files, ours, then the older ones.
want=()
((xtx)) && want+=("$HOME/.cache/rvg-vkr2-gpu-7900xtx.lock")
((m780)) && want+=("$HOME/.cache/rvg-vkr2-gpu-780m.lock")
((nvidia)) && want+=("$HOME/.cache/rvg-vkr2-gpu.lock" "$HOME/rvgv/gpu.lock")
((amd)) && want+=("$HOME/.cache/m2v-orch/gpu-radv.lock" "$HOME/.cache/m2v-gpu/run-gpu-radeon_icd.json.lock")
locks=()
for f in ${want[@]+"${want[@]}"}; do
  [[ -e $f ]] && locks+=("$f")
done

if ((${#run[@]} == 0)); then
  for f in ${locks[@]+"${locks[@]}"}; do echo "$f"; done
  exit 0
fi
for f in ${locks[@]+"${locks[@]}"}; do
  exec {fd}<"$f"
  flock -x "$fd"
done
export RVG_GPU_LOCKS_HELD=1
exec "${run[@]}"
