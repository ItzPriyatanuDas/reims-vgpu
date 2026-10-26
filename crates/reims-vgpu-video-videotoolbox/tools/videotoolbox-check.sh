#!/usr/bin/env bash
# The VideoToolbox backend's checks, on a Mac:
#
#   crates/reims-vgpu-video-videotoolbox/tools/videotoolbox-check.sh [stream dir...]
#
# - the crate's tests: the declarations against the SDK, and the encoders
#   and decoders against each other;
# - the conformance suite's decode check, through core's host session as a
#   guest drives it, synchronously and asynchronously, of every MP4 in each
#   stream dir that has a reference beside it: NAME.mac-sw.md5 (a software
#   VideoToolbox decode), NAME.md5 (a libdav1d decode), NAME.vp9.guest
#   (vp9-check.sh's: ffmpeg's decode packed into NV12 or P010 by the
#   conformance suite's planar-packed, exact at every size) or NAME.expect
#   (hevc-check.sh's, for x265's and kvazaar's streams: ffmpeg's decode or
#   the encoder's reconstruction); a stream with none is named in a NOTE
#   line.
#
# A stream hevc-check.sh crafted (NAME.craft beside it names its source and
# hevc-craft's edits) is crafted again here from its source, before the GPU
# lock, and goes to the media hardware only where that gives the same bytes
# as NAME.265 and the codec crate's --check passes on them: invalid streams
# panicked this Mac's media hardware twice. Its MP4 is ffmpeg's mux of that
# stream, as every other stream's is.
#
# A decode check passes where the suite exits 0 having printed a PASS line
# and no FAIL line. The stream dirs default to build/streams,
# build/av1-streams and build/vp9-guest, each named in a NOTE line where it
# is missing; a dir named on the command line that is missing is a FAIL.
# The streams are made by the VA-API backend's checks on a Linux host and
# are never committed. Everything a run writes stays under
# build/videotoolbox-check. The last line is `result: pass` or `result:
# fail (N checks failed)`, and the exit status is 0 or 1.
# RVGV_ZERO_COPY=off has the conformance suite's runs copy every decoded
# frame on the CPU (--no-zero-copy), to compare the two.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
out=${RVGV_VT_OUT:-$root/build/videotoolbox-check}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
mkdir -p "$out"

# The dirs the caller named, which the run under the GPU lock is handed as
# they were, so that it takes the defaults as defaults.
named=("$@")
named_dirs=1
if (($# == 0)); then
  named_dirs=0
  set -- "$root/build/streams" "$root/build/av1-streams" "$root/build/vp9-guest"
fi

cargo build --offline --release -p reims-vgpu-video-videotoolbox --examples 2> "$out/build.log" || {
  tail -20 "$out/build.log"
  echo "result: fail (the examples did not build)"
  exit 1
}
cargo nextest run --offline --no-run -p reims-vgpu-video-videotoolbox 2>> "$out/build.log" || {
  tail -20 "$out/build.log"
  echo "result: fail (the tests did not build)"
  exit 1
}
cargo build --offline --release -p reims-vgpu-video-vaapi --example hevc-craft 2>> "$out/build.log" || {
  tail -20 "$out/build.log"
  echo "result: fail (hevc-craft did not build)"
  exit 1
}
conformance=$CARGO_TARGET_DIR/release/examples/videotoolbox-conformance
craft=$CARGO_TARGET_DIR/release/examples/hevc-craft

# The crafted streams that came out the same here with --check passing,
# each as DIR/NAME, a line each.
if [[ -z ${RVG_GPU_LOCKS_HELD:-} ]]; then
  : > "$out/crafted-checked"
  for dir in "$@"; do
    [[ -d $dir ]] || continue
    for recipe in "$dir"/*.craft; do
      [[ -e $recipe ]] || continue
      name=$(basename "$recipe" .craft)
      read -r source edits < "$recipe"
      # shellcheck disable=SC2086 # $edits is a list of options
      if "$craft" "$dir/$source.265" "$out/$name.265" $edits --check > "$out/$name.check" 2>&1 &&
          cmp -s "$out/$name.265" "$dir/$name.265"; then
        echo "$dir/$name" >> "$out/crafted-checked"
      fi
      rm -f "$out/$name.265"
    done
  done
fi

# The rest runs holding the Mac's GPU lock (scripts/internal/gpu-locks.sh): the tests and the checks
# make GPU transfers and drive the media hardware.
if [[ -z ${RVG_GPU_LOCKS_HELD:-} ]]; then
  exec bash "$root/scripts/internal/gpu-locks.sh" -- bash "$0" ${named[@]+"${named[@]}"}
fi
zero_copy=()
[[ ${RVGV_ZERO_COPY:-on} == off ]] && zero_copy=(--no-zero-copy)

failed=0
pass() { echo "PASS $*"; }
fail() {
  echo "FAIL $*"
  failed=$((failed + 1))
}

if cargo nextest run --offline --no-fail-fast -p reims-vgpu-video-videotoolbox > "$out/tests.log" 2>&1; then
  pass "tests: $(grep -h '^ *Summary' "$out/tests.log" | tail -n1 | sed 's/^ *//' || true)"
else
  fail "tests: see $out/tests.log"
fi

# A stream whose frames change size: VideoToolbox's VP9 decoder emits a
# frame not of the description's size with -12909 and no image, as Apple's
# host does. Every frame decoded must be the reference's, and the others
# must have come without an image.
resized() {
  local stream=$1 reference=$2 name=$3 mode flags
  for mode in sync async; do
    flags=()
    [[ $mode == async ]] && flags=(--async)
    if ! "$conformance" ${zero_copy[@]+"${zero_copy[@]}"} hardware framemd5 "$stream" ${flags[@]+"${flags[@]}"} \
        > "$out/$name.$mode.frames" 2> "$out/$name.$mode.log"; then
      fail "$name.$mode: $(tail -1 "$out/$name.$mode.log")"
      continue
    fi
    local decoded refused total foreign
    decoded=$(grep -c '^frame' "$out/$name.$mode.frames" || true)
    refused=$(grep -o '[0-9]* without an image' "$out/$name.$mode.log" | tail -1 | cut -d' ' -f1 || true)
    refused=${refused:-0}
    total=$(grep -c . "$reference" || true)
    foreign=$(awk '{ print $NF }' "$out/$name.$mode.frames" | sort | comm -23 - <(sort "$reference") | wc -l)
    if ((foreign == 0 && decoded > 0 && decoded + refused == total)); then
      pass "$name.$mode: $decoded frames match, $refused of other sizes refused"
    else
      fail "$name.$mode: $decoded decoded ($foreign not in the reference), $refused refused, reference $total"
    fi
  done
}

# decode_check NAME STREAM REFERENCE: the suite's decode check, its PASS
# and FAIL lines counted; a run that exits otherwise than 0, or prints no
# PASS line, is a FAIL.
decode_check() {
  local name=$1 stream=$2 reference=$3 code=0 passes=0 before=$failed verdict rest
  "$conformance" ${zero_copy[@]+"${zero_copy[@]}"} hardware decode-check "$stream" "$reference" \
    > "$out/$name.txt" 2> "$out/$name.log" || code=$?
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
}

for dir in "$@"; do
  if [[ ! -d $dir ]]; then
    if ((named_dirs)); then
      fail "$dir: no such stream dir"
    else
      echo "NOTE $dir: no such stream dir"
    fi
    continue
  fi
  for stream in "$dir"/*.mp4; do
    [[ -e $stream ]] || continue
    name=$(basename "$stream" .mp4)
    base=${stream%.mp4}
    reference=
    for kind in mac-sw.md5 md5 vp9.guest expect; do
      if [[ -s $base.$kind ]]; then
        reference=$base.$kind
        break
      fi
    done
    case $name in
      # hevc-check.sh's streams that only our parser may see: a picture
      # with 9 current references, past the 8 va.h allows, and a tile
      # column narrower than Main allows. Never sent to the Mac's
      # hardware.
      kvz-720p-ref15 | kvz-720p-tiles-narrow) continue ;;
      # 63x65: VideoToolbox's VP9 hardware decoder opens no session
      # (-12911) and there is no software one (-12906): the guest is told
      # as Apple's host would tell it.
      p0-63x65)
        "$conformance" ${zero_copy[@]+"${zero_copy[@]}"} hardware decode-check "$stream" "${reference:-/dev/null}" > "$out/$name.txt" 2> "$out/$name.log" || true
        if grep -q 'dsta: -12911' "$out/$name.txt"; then
          pass "$name: refused at dsta with -12911, as VideoToolbox refuses it"
        else
          fail "$name: $(head -1 "$out/$name.txt")"
        fi
        continue
        ;;
    esac
    if [[ -z $reference || ! -s $reference ]]; then
      echo "NOTE $name: no reference beside it"
      continue
    fi
    if [[ -e $base.craft ]] && ! grep -qxF "$base" "$out/crafted-checked"; then
      fail "$name: hevc-craft's --check did not pass on it here (see $out/$name.check), so it goes to no hardware"
      continue
    fi
    if [[ $name == *resize* ]]; then
      resized "$stream" "$reference" "$name"
      continue
    fi
    decode_check "$name" "$stream" "$reference"
  done
done

if ((failed == 0)); then
  echo "result: pass"
else
  echo "result: fail ($failed checks failed)"
  exit 1
fi
