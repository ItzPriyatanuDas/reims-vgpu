#!/usr/bin/env bash
# The NVIDIA backend's decoders through core's host session, on a Linux host
# with an NVIDIA GPU:
#
#   crates/reims-vgpu-video-nvidia/tools/decode-check.sh [stream dir...]
#
# The conformance suite's decode check (nvidia-conformance GPU decode-check),
# synchronously and asynchronously, of every MP4 in each stream dir that has a
# reference beside it: NAME.expect (hevc-check.sh's, for x265's, kvazaar's and
# hevc-craft's streams: ffmpeg's decode, or the encoder's reconstruction, or
# for a crafted stream its source's), NAME.ref (h264-check.sh's ffmpeg
# decode), NAME.vp9.guest or NAME.libdav1d.guest (vp9-check.sh's and
# av1-check.sh's, packed by the conformance suite's planar-packed). A stream
# with none is named in a NOTE line. Each run's frames must be the
# reference's, every one with an image.
#
# The streams come from the VA-API backend's checks
# (crates/reims-vgpu-video-vaapi/tools/*-check.sh), run on any Linux host and
# copied here; they are never committed. kvz-720p-ref15, whose tenth picture
# names 9 current references, past NVDEC's 8, must give its first 9 frames as
# its reference's, its tenth -12910 and no image, and each of the 20 after
# it -12909, both ways, as host-session-check.sh has it on VA-API.
# p0-63x65 and aom-63x65, under the 128x128 NVDEC's caps give VP9 and AV1,
# must be refused at the session's start, `dsta`, with -12911 both ways, and
# give no other answer (docs/video/nvidia.md, "Decode"). Left to the runs
# docs/video/nvidia.md names, and named in NOTE lines:
# kvz-720p-tiles-narrow, which the codec crate refuses, and the streams
# whose frames change size, which go through the session whole
# (conformance GPU framemd5). The stream dirs default to
# build/hevc-check under the repository root; a dir named that is missing is
# a FAIL. RVGV_NVIDIA_GPU picks the GPU (0). Everything a run writes stays
# under build/nvidia-decode-check. The last line is `result: pass` or
# `result: fail (N checks failed)`, and the exit status is 0 or 1.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
out=${RVGV_NVIDIA_OUT:-$root/build/nvidia-decode-check}
gpu=${RVGV_NVIDIA_GPU:-0}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
export PATH=$HOME/.cargo/bin:$PATH
mkdir -p "$out"

if (($# == 0)); then
  set -- "$root/build/hevc-check"
fi

if [[ -z ${RVG_GPU_LOCKS_HELD:-} ]]; then
  (cd "$root" && cargo build --quiet --release -p reims-vgpu-video-nvidia --example nvidia-conformance) \
    2> "$out/build.log" || {
    tail -20 "$out/build.log"
    echo "result: fail (the example did not build)"
    exit 1
  }
  # The rest runs holding the GPU's locks (scripts/gpu-locks.sh).
  exec bash "$root/scripts/gpu-locks.sh" --all -- bash "$0" "$@"
fi
conformance=$CARGO_TARGET_DIR/release/examples/nvidia-conformance

failed=0
pass() { echo "PASS $*"; }
fail() {
  echo "FAIL $*"
  failed=$((failed + 1))
}

echo "nvidia decode-check on GPU $gpu, $(date -Is), $(hostname)"
for dir in "$@"; do
  if [[ ! -d $dir ]]; then
    fail "$dir: no such stream dir"
    continue
  fi
  for stream in "$dir"/*.mp4; do
    [[ -e $stream ]] || continue
    name=$(basename "$stream" .mp4)
    base=${stream%.mp4}
    case $name in
      kvz-720p-tiles-narrow | *resize*)
        echo "NOTE $name: not run here (docs/video/nvidia.md, \"Decode\")"
        continue
        ;;
      kvz-720p-ref15)
        if [[ ! -s $base.ref ]]; then
          fail "$name: no reference beside it"
          continue
        fi
        head -n 9 "$base.ref" | sort > "$out/$name.ref.sorted"
        for mode in sync async; do
          flags=()
          [[ $mode == async ]] && flags=(--async)
          if ! "$conformance" "$gpu" framemd5 "$stream" ${flags[@]+"${flags[@]}"} \
              > "$out/$name.$mode.frames" 2> "$out/$name.$mode.log"; then
            fail "$name.$mode: $(tail -n1 "$out/$name.$mode.log")"
            continue
          fi
          awk '{ print $NF }' "$out/$name.$mode.frames" | sort > "$out/$name.$mode.sorted"
          unsupported=$(grep -c '^frame 9: status -12910,' "$out/$name.$mode.log" || true)
          unrecovered=$(grep -c '^frame [0-9]*: status -12909,' "$out/$name.$mode.log" || true)
          no_image=$(grep -c '^frame -\?[0-9]*: status ' "$out/$name.$mode.log" || true)
          if cmp -s "$out/$name.ref.sorted" "$out/$name.$mode.sorted" &&
              [[ $unsupported == 1 && $unrecovered == 20 && $no_image == 21 ]]; then
            pass "$name.$mode: 9 frames match, frame 9 refused with -12910 and the 20 after it with -12909"
          else
            fail "$name.$mode: $(wc -l < "$out/$name.$mode.sorted") frames, frame 9 -12910" \
              "$unsupported times, $unrecovered with -12909, $no_image with no image"
          fi
        done
        continue
        ;;
      p0-63x65 | aom-63x65)
        # The refusal comes before any frame, so the reference is not read
        # for it; /dev/null stands in where there is none.
        reference=/dev/null
        for kind in vp9.guest libdav1d.guest; do
          if [[ -s $base.$kind ]]; then
            reference=$base.$kind
          fi
        done
        "$conformance" "$gpu" decode-check "$stream" "$reference" > "$out/$name.txt" 2> "$out/$name.log" || true
        for mode in sync async; do
          if grep -qxF "FAIL $name.$mode: dsta: -12911" "$out/$name.txt"; then
            pass "$name.$mode: refused at dsta with -12911, as too small for NVDEC"
          else
            fail "$name.$mode: not refused at dsta with -12911:" \
              "$(grep -F " $name.$mode:" "$out/$name.txt" || tail -n1 "$out/$name.log")"
          fi
        done
        continue
        ;;
    esac
    reference=
    for kind in expect ref vp9.guest libdav1d.guest; do
      if [[ -s $base.$kind ]]; then
        reference=$base.$kind
        break
      fi
    done
    if [[ -z $reference ]]; then
      echo "NOTE $name: no reference beside it"
      continue
    fi
    code=0
    passes=0
    before=$failed
    "$conformance" "$gpu" decode-check "$stream" "$reference" > "$out/$name.txt" 2> "$out/$name.log" || code=$?
    while read -r verdict rest; do
      case $verdict in
        PASS)
          pass "$rest"
          passes=$((passes + 1))
          ;;
        FAIL) fail "$rest" ;;
      esac
    done < "$out/$name.txt"
    if ((code != 0 && failed == before)); then
      fail "$name: exit $code: $(tail -n1 "$out/$name.log")"
    elif ((passes == 0 && failed == before)); then
      fail "$name: no verdict: $(tail -n1 "$out/$name.log")"
    fi
  done
done

if ((failed == 0)); then
  echo "result: pass"
else
  echo "result: fail ($failed checks failed)"
  exit 1
fi
