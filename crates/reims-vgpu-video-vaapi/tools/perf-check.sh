#!/usr/bin/env bash
# How fast VA-API decodes 4K and 1080p streams, and where each frame's time
# goes, every frame checked against a software reference.
#
#   crates/reims-vgpu-video-vaapi/tools/perf-check.sh [render node] [stream...]
#
# Runs on Linux with libva, x265, x264, SvtAv1EncApp and ffmpeg.
# The encoders and ffmpeg are test oracles only. It makes these streams
# once, in build/perf-check/streams, each 150 frames of testsrc2 with noise
# (mandelbrot for AV1), 30 fps:
#   hevc-2160p-main, hevc-2160p-main10  x265, 3840x2160, 20 Mbit/s, 3 B-frames
#   av1-2160p, av1-2160p-10bit          SvtAv1EncApp, 3840x2160, preset 10
#   h264-1080p-high                     x264, 1920x1080 High, 3 B-frames
# Their references are x265's reconstruction for HEVC, libdav1d's decode
# for AV1 and ffmpeg's for H.264, in NV12 or P010.
#
# ffmpeg muxes the raw streams' B-frames into the .mp4s with each PTS equal to
# its DTS, so a guest that sorts frames by PTS sees them in sample order. Once
# a direct run passes, the stream's <name>.guest.ref is that reference in
# sample order, the one a guest's check takes.
#
# Each stream is decoded three times:
#   direct  the codec's framemd5 example, display order: its fps, and the
#           readback's wait, transfer and copy a frame;
#   sync    the conformance suite's framemd5 (vaapi-conformance), the
#           stand-in guest through core's host session, each frame
#           synchronous: its fps, and core's own count of where a frame's
#           time went (RVGV_LOG=info);
#   async   the same with the asynchronous flag.
# The direct run's MD5s must equal the reference in order, the session's as
# a set. Each session run writes the host's messages in the order the guest
# got them to build/perf-check/<node>-<stream>.<mode>.sequence, which two
# builds of the host session must give alike where a change must not show
# to the guest. RVGV_VAAPI_READBACK (staged, getimage, derive, mapped) passes
# through to the examples, to compare the ways back, and RVGV_ZERO_COPY=off
# runs the session runs with --no-zero-copy, to compare the host session's
# frames written by the GPU with those the CPU copies.
# Lines go to stdout and build/perf-check/summary-<node>.txt; RVGV_PERF_OUT
# names another directory for a run's logs, and RVGV_PERF_STREAMS one of
# streams already made. Nothing here is committed.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

node=${1:-/dev/dri/renderD128}
shift || true
only=" $* "
out=${RVGV_PERF_OUT:-$root/build/perf-check}
streams=${RVGV_PERF_STREAMS:-$out/streams}
mkdir -p "$out" "$streams"

build reims-vgpu-video-vaapi hevc-framemd5 av1-framemd5 h264-framemd5 vaapi-conformance
examples=$CARGO_TARGET_DIR/release/examples
copy_flags=()
[[ ${RVGV_ZERO_COPY:-on} == off ]] && copy_flags=(--no-zero-copy)
noise="noise=alls=14:allf=t+u"

# Each make_ function makes its stream's MP4 and reference, or returns 1,
# leaving neither: it runs as a condition, where `set -e` does not, so each
# step returns its own failure. A stream made again drops its guest's
# reference, which the direct run writes.
make_hevc() {
  local name=$1 bits=$2
  [[ -s $streams/$name.mp4 && -s $streams/$name.ref ]] && return
  rm -f "$streams/$name".{mp4,ref,guest.ref}
  local pix=yuv420p depth=() packed=nv12
  if (( bits > 8 )); then
    pix=yuv420p10le depth=(--input-depth 10 --output-depth 10) packed=p010le
  fi
  ffmpeg -nostdin -hide_banner -loglevel error -f lavfi -i "testsrc2=size=3840x2160:rate=30,$noise" \
      -frames:v 150 -pix_fmt "$pix" -strict -1 -f yuv4mpegpipe - |
    nice -n 10 x265 --input - --y4m "${depth[@]}" --preset faster --bframes 3 --bitrate 20000 \
      --recon "$streams/$name.recon.y4m" -o "$streams/$name.265" 2> "$streams/$name.x265.log" ||
    return 1
  hashes "$streams/$name.ref" -i "$streams/$name.recon.y4m" -pix_fmt "$packed" || return 1
  rm -f "$streams/$name.recon.y4m"
  mp4 "$name" 265 -r 30 -tag:v hvc1
}

make_av1() {
  local name=$1 bits=$2
  [[ -s $streams/$name.mp4 && -s $streams/$name.ref ]] && return
  rm -f "$streams/$name".{mp4,ref,guest.ref}
  local pix=yuv420p depth=() packed=nv12
  if (( bits > 8 )); then
    pix=yuv420p10le depth=(--input-depth 10) packed=p010le
  fi
  ffmpeg -nostdin -hide_banner -loglevel error -f lavfi -i "mandelbrot=size=3840x2160:rate=30" \
      -frames:v 150 -pix_fmt "$pix" -strict -1 -f yuv4mpegpipe -y "$streams/$name.y4m" || return 1
  nice -n 10 SvtAv1EncApp -i "$streams/$name.y4m" -b "$streams/$name.ivf" -n 150 --preset 10 "${depth[@]}" \
    > "$streams/$name.svt.log" 2>&1 < /dev/null || return 1
  rm -f "$streams/$name.y4m"
  hashes "$streams/$name.ref" -c:v libdav1d -i "$streams/$name.ivf" \
    -fps_mode passthrough -pix_fmt "$packed" || return 1
  mp4 "$name" ivf
}

make_h264() {
  local name=$1
  [[ -s $streams/$name.mp4 && -s $streams/$name.ref ]] && return
  rm -f "$streams/$name".{mp4,ref,guest.ref}
  ffmpeg -nostdin -hide_banner -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=30,$noise" \
      -frames:v 150 -pix_fmt yuv420p -f yuv4mpegpipe - |
    x264 --quiet --demuxer y4m --profile high --preset medium --bframes 3 --bitrate 8000 \
      -o "$streams/$name.264" - 2> /dev/null || return 1
  hashes "$streams/$name.ref" -hwaccel none -i "$streams/$name.264" \
    -fps_mode passthrough -pix_fmt nv12 || return 1
  mp4 "$name" 264 -r 30
}

# mp4 NAME EXTENSION [INPUT-OPTION]...: NAME.EXTENSION's stream as NAME.mp4,
# written last, so that its presence says the stream and reference are whole.
mp4() {
  local name=$1 extension=$2
  shift 2
  ffmpeg -nostdin -hide_banner -loglevel error -y "$@" -i "$streams/$name.$extension" \
    -c copy -f mp4 "$streams/$name.part.mp4" || return 1
  mv "$streams/$name.part.mp4" "$streams/$name.mp4"
}

# prepare NAME COMMAND...: runs COMMAND where NAME is wanted; a stream it
# does not make is a FAIL, and is not decoded.
prepare() {
  local name=$1
  shift
  wanted "$name" || return 0
  if ! "$@"; then
    echo "FAIL $name: its stream or reference was not made"
    failed=1
  fi
}
prepare hevc-2160p-main make_hevc hevc-2160p-main 8
prepare hevc-2160p-main10 make_hevc hevc-2160p-main10 10
prepare av1-2160p make_av1 av1-2160p 8
prepare av1-2160p-10bit make_av1 av1-2160p-10bit 10
prepare h264-1080p-high make_h264 h264-1080p-high

lock_gpu --node "$node"

label=$(basename "$node")
summary=$out/summary-$label.txt
{
  echo "perf-check on $node, $(date -Is), $(hostname), readback ${RVGV_VAAPI_READBACK:-default}," \
    "zero copy ${RVGV_ZERO_COPY:-on}"
} | tee "$summary"
for entry in hevc-2160p-main:hevc hevc-2160p-main10:hevc av1-2160p:av1 av1-2160p-10bit:av1 h264-1080p-high:h264; do
  name=${entry%%:*}
  codec=${entry##*:}
  wanted "$name" || continue
  [[ -s $streams/$name.mp4 ]] || continue
  logs=$out/$label-$name
  # direct, display order
  if "$examples/$codec-framemd5" "$node" "$streams/$name.mp4" > "$logs.direct.frames" 2> "$logs.direct.log"; then
    awk '{ print $NF }' "$logs.direct.frames" > "$logs.direct.md5"
    if [[ -s $streams/$name.ref ]] && cmp -s "$streams/$name.ref" "$logs.direct.md5"; then
      verdict=PASS
      # The guest's reference: the reference's MD5s placed at their samples, by the direct
      # run's `id`, which a display order equal to the reference has just checked.
      awk '$1 == "frame" && $3 == "id" { if ($4 in at) dup = 1; at[$4] = $NF; n++ }
        END { if (dup) exit 1; for (k = 0; k < n; k++) { if (!(k in at)) exit 1; print at[k] } }' \
        "$logs.direct.frames" > "$streams/$name.guest.ref" || rm -f "$streams/$name.guest.ref"
    else
      verdict=FAIL
      failed=1
    fi
    fps=$(grep -o '([0-9.]* fps)' "$logs.direct.log" | tr -d '()' || true)
    stages=$(grep '^readback a frame' "$logs.direct.log" | sed 's/^readback a frame: //' || true)
    echo "$verdict $name direct: $fps; readback $stages" | tee -a "$summary"
  else
    echo "FAIL $name direct: $(tail -n1 "$logs.direct.log")" | tee -a "$summary"
    failed=1
  fi
  sort "$streams/$name.ref" > "$logs.ref.sorted"
  for mode in sync async; do
    flags=("${copy_flags[@]}")
    [[ $mode == async ]] && flags+=(--async)
    if RVGV_LOG=info "$examples/vaapi-conformance" "$node" framemd5 "$streams/$name.mp4" ${flags[@]+"${flags[@]}"} \
        --sequence "$logs.$mode.sequence" > "$logs.$mode.frames" 2> "$logs.$mode.log"; then
      awk '{ print $NF }' "$logs.$mode.frames" | sort > "$logs.$mode.sorted"
      if [[ -s $logs.ref.sorted ]] && cmp -s "$logs.ref.sorted" "$logs.$mode.sorted"; then
        verdict=PASS
      else
        verdict=FAIL
        failed=1
      fi
      fps=$(grep -o '([0-9.]* fps)' "$logs.$mode.log" | tail -n1 | tr -d '()' || true)
      host=$(grep -o 'decoder session ends: [0-9].*' "$logs.$mode.log" | head -n1 |
        sed 's/decoder session ends: //' || true)
      echo "$verdict $name $mode: $fps; host: $host" | tee -a "$summary"
    else
      echo "FAIL $name $mode: $(tail -n1 "$logs.$mode.log")" | tee -a "$summary"
      failed=1
    fi
  done
done
exit "$failed"
