#!/usr/bin/env bash
# The host session on VA-API against a software reference.
#
#   crates/reims-vgpu-video-vaapi/tools/host-session-check.sh [render node] [stream...]
#
# Runs on Linux with libva, on the streams and references tools/h264-check.sh
# and tools/hevc-check.sh leave in build/h264-check and build/hevc-check
# (run them first). Each stream goes through the conformance suite's
# framemd5 (examples/conformance.rs, vaapi-conformance): a stand-in for the
# guest's decoder drives a HostSession on VaapiBackend message by message,
# and hashes each frame out of the buffer it lent the host. Frames come out in decode order, as
# VideoToolbox's decoders emit them, so they are compared with ffmpeg's as
# a set; every one must match, synchronously and asynchronously decoded.
# AV1 streams come from tools/av1-check.sh's build/av1-check, against its
# libdav1d decode in NV12 or P010 (NAME.libdav1d.guest), and VP9 streams
# from tools/vp9-check.sh's build/vp9-check, against ffmpeg's vp9 decode in
# NV12 or P010 (NAME.vp9.guest): each packed by the conformance suite's
# planar-packed from the decoder's own planar frames, every frame at its
# own size (lib.sh's guest_frames). An AV1 or VP9 frame of another size
# than the session's is lent a buffer of its own size, so a resized
# stream's reference is every frame at its own size; Apple's decoder emits
# such a VP9 frame with -12909 and no image. Every frame must come
# out with an image: a frame the host emits with none is a FAIL, whatever
# its status. H.264 sessions Apple's decoder refuses must be refused at dsta
# with its status, and damaged H.264 streams answered as it answers them: a
# lost reference with every frame, a garbage NAL unit with -12909 for its
# frame and each after it until the I picture. Name streams, as
# build/host-session-check names them (vp9-check-p0-720p), to run only
# those. Each run writes the host's messages in the order the guest got them
# to <name>.<mode>.sequence, and its log, at RVGV_LOG's level (info by
# default), ends with the host's own count of each frame's time and of the
# frames its device wrote into the guest's buffers. So a run with
# RVGV_ZERO_COPY=off, which runs framemd5 with --no-zero-copy
# so the CPU copies every frame, in another RVGV_HOST_SESSION_OUT, must give
# every sequence alike by cmp. Nothing here is committed: it all stays
# under build/.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

node=${1:-/dev/dri/renderD128}
shift || true
only=" $* "
h264=${RVGV_H264_OUT:-$root/build/h264-check}
hevc=${RVGV_HEVC_OUT:-$root/build/hevc-check}
av1=${RVGV_AV1_OUT:-$root/build/av1-check}
vp9=${RVGV_VP9_OUT:-$root/build/vp9-check}
out=${RVGV_HOST_SESSION_OUT:-$root/build/host-session-check}
export RVGV_LOG=${RVGV_LOG:-info}
mkdir -p "$out"

build reims-vgpu-video-vaapi vaapi-conformance h264-craft
conformance=$CARGO_TARGET_DIR/release/examples/vaapi-conformance
craft=$CARGO_TARGET_DIR/release/examples/h264-craft
copy_flags=()
[[ ${RVGV_ZERO_COPY:-on} == off ]] && copy_flags=(--no-zero-copy)

# framemd5 STREAM [OPTION]...: the suite's framemd5 of STREAM on the node.
framemd5() {
  "$conformance" "$node" framemd5 "$@"
}

lock_gpu --node "$node"

echo "host-session-check on $node, $(date -Is), $(hostname), zero copy ${RVGV_ZERO_COPY:-on}"
# directory|stream|reference: hevc-check.sh's .expect is ffmpeg's decode or,
# where that differs from it, x265's reconstruction; for a stream
# hevc-craft wrote, its source's.
entries=()
for name in baseline-720p baseline-1080p main-720p main-1080p-opengop high-720p-cqm high-1080p high-1080p-deep \
    ltr-mmco-cabac ltr-mmco-cavlc gaps5-ref1 gaps2-ref4 gaps9-ref4 len2-baseline-360p; do
  entries+=("$h264|$name|ref")
done
for name in main-720p main-1080p-slices main-720p-cqm-file main-720p-fade main-1366x766-ctu16 \
    main-4k main10-720p main10-1080p-slices-cqm main10-720p-fade main10-4k \
    kvz-720p-tiles kvz-720p-tiles-explicit kvz-1080p-tiles-slices kvz-1366x766-tiles \
    kvz-4k-tiles-slices kvz-720p-dependent kvz-1080p-dependent-cra kvz-720p-lowdelay \
    kvz-720p-ref8 kvz-720p-fields kvz-720p-tools \
    main-720p-lists main-720p-sets-sps main-720p-sets-slice main-720p-closed-all \
    main-1080p-slices-lists-sps main-1080p-slices-all main10-720p-all main-720p-ref1-long-term \
    main-720p-ref1-all kvz-1080p-dependent-cra-lists kvz-1080p-dependent-cra-all \
    kvz-720p-lowdelay-lists-sps kvz-720p-lowdelay-all kvz-1080p-ref1-long-term; do
  entries+=("$hevc|$name|expect")
done
for name in aom-720p aom-720p-10bit aom-1080p-tiles aom-720p-superres aom-720p-sframe \
    svt-720p-grain svt-720p-grain-10bit svt-720p-bigmax svt-2160p-10bit rav1e-720p-sframe \
    aom-720p-pad-under aom-720p-pad-over aom-720p-fwdkf aom-360p-fwdkf-filtered aom-720p-kfflt2 aom-720p-gm-zoom \
    aom-720p-gm-pan-10bit svc-720p-l1t3 svc-720p-l2t1 svc-720p-l3t3 \
    aom-360p-resize-569 aom-360p-resize-465-10bit; do
  entries+=("$av1|$name|libdav1d.guest")
done
for name in p0-720p p2-720p p0-720p-arf6 p0-1080p-tiles p2-1080p-tiles p0-720p-resilient \
    p0-720p-intra-only p2-720p-show-existing p0-720p-features p0-720p-aq3 p2-2160p \
    p0-720p-seg-mixed p2-720p-seg-mixed-skip p0-720p-resize p2-720p-resize p0-720p-resize-early; do
  entries+=("$vp9|$name|vp9.guest")
done
for entry in "${entries[@]}"; do
  IFS='|' read -r streams stream ref <<<"$entry"
  name=$(basename "$streams")-$stream
  wanted "$name" || continue
  if [[ ! -s $streams/$stream.mp4 || ! -s $streams/$stream.$ref ]]; then
    echo "FAIL $name: no stream or reference in $streams; run its check first"
    failed=1
    continue
  fi
  sort "$streams/$stream.$ref" > "$out/$name.ref.sorted"
  for mode in sync async; do
    label=$name.$mode
    flags=("${copy_flags[@]}")
    [[ $mode == async ]] && flags+=(--async)
    if ! framemd5 "$streams/$stream.mp4" ${flags[@]+"${flags[@]}"} --sequence "$out/$label.sequence" \
        > "$out/$label.frames" 2> "$out/$label.log"; then
      echo "FAIL $label: $(tail -n1 "$out/$label.log")"
      failed=1
      continue
    fi
    awk '{ print $NF }' "$out/$label.frames" | sort > "$out/$label.sorted"
    n_got=$(wc -l < "$out/$label.sorted")
    no_image=$(grep -c '^frame -\?[0-9]*: status ' "$out/$label.log" || true)
    if ((no_image > 0)); then
      echo "FAIL $label: $no_image frames emitted with no image, the first" \
        "$(grep -m1 '^frame -\?[0-9]*: status ' "$out/$label.log")"
      failed=1
    elif cmp -s "$out/$name.ref.sorted" "$out/$label.sorted"; then
      echo "PASS $label: $n_got frames match"
    else
      echo "FAIL $label: $n_got frames, reference $(wc -l < "$out/$name.ref.sorted")"
      failed=1
    fi
    echo "     $(grep -v '^\[' "$out/$label.log" | tail -n1)"
    host=$(grep -o 'decoder session ends: [0-9].*' "$out/$label.log" | head -n1 || true)
    [[ -z $host ]] || echo "     host: ${host#decoder session ends: }"
  done
done

# H.264 that Apple's decoder refuses a session on: an SPS that allows
# fields (-12911), 3-byte NAL unit lengths (-12910).
for entry in high-1080p-interlaced:-12911 high-1080p-fakeint:-12911 len3-baseline-360p:-12910; do
  stream=${entry%%:*}
  status=${entry##*:}
  name=h264-check-$stream
  wanted "$name" || continue
  if [[ ! -s $h264/$stream.mp4 ]]; then
    echo "FAIL $name: no stream in $h264; run its check first"
    failed=1
    continue
  fi
  if framemd5 "$h264/$stream.mp4" > /dev/null 2> "$out/$name.log"; then
    echo "FAIL $name: decoded, should be refused at dsta"
    failed=1
  elif grep -q "dsta: $status" "$out/$name.log"; then
    echo "PASS $name: refused at dsta with $status"
  else
    echo "FAIL $name: $(tail -n1 "$out/$name.log")"
    failed=1
  fi
done

# HEVC whose tenth picture names 9 current references, past VA-API's 8
# (hevc-check.sh): its first 9 frames must come out, the tenth with -12910
# (kVTVideoDecoderUnsupportedDataFormatErr) and no image, and each after it,
# none an IRAP picture, with -12909, as after any frame that fails.
name=hevc-check-kvz-720p-ref15
if wanted "$name"; then
  if [[ ! -s $hevc/kvz-720p-ref15.mp4 || ! -s $hevc/kvz-720p-ref15.ref ]]; then
    echo "FAIL $name: no stream or reference in $hevc; run its check first"
    failed=1
  else
    head -n 9 "$hevc/kvz-720p-ref15.ref" | sort > "$out/$name.ref.sorted"
    for mode in sync async; do
      label=$name.$mode
      flags=("${copy_flags[@]}")
      [[ $mode == async ]] && flags+=(--async)
      if ! framemd5 "$hevc/kvz-720p-ref15.mp4" ${flags[@]+"${flags[@]}"} > "$out/$label.frames" 2> "$out/$label.log"; then
        echo "FAIL $label: $(tail -n1 "$out/$label.log")"
        failed=1
        continue
      fi
      awk '{ print $NF }' "$out/$label.frames" | sort > "$out/$label.sorted"
      unsupported=$(grep -c '^frame 9: status -12910,' "$out/$label.log" || true)
      unrecovered=$(grep -c '^frame [0-9]*: status -12909,' "$out/$label.log" || true)
      no_image=$(grep -c '^frame -\?[0-9]*: status ' "$out/$label.log" || true)
      if cmp -s "$out/$name.ref.sorted" "$out/$label.sorted" &&
          [[ $unsupported == 1 && $unrecovered == 20 && $no_image == 21 ]]; then
        echo "PASS $label: 9 frames match, frame 9 refused with -12910 and the 20 after it with -12909"
      else
        echo "FAIL $label: $(wc -l < "$out/$label.sorted") frames, frame 9 -12910" \
          "$unsupported times, $unrecovered with -12909, $no_image with no image"
        failed=1
      fi
    done
  fi
fi

# Damaged H.264, crafted from main-gapsrc-720p (an I picture at 30, no gap
# allowed): frame 20 lost, or followed by a garbage NAL unit. Each goes to
# the GPU only if h264-craft's stand-in shows nothing but valid slices
# reaching it. The source's frames 0 to 19 and 30 to 59 must all come out;
# frames 20 to 29 are refused after the garbage, and decoded from a copy
# after the loss.
source_ref=$h264/main-gapsrc-720p.ref
for entry in "drop20|--drop 20|0|59" "garbage20-slice|--garbage 20:slice|10|50" \
    "garbage20-forbidden|--garbage 20:forbidden|10|50"; do
  IFS='|' read -r stream edits refused frames <<<"$entry"
  name=h264-damage-$stream
  wanted "$name" || continue
  if [[ ! -s $h264/main-gapsrc-720p.264 || ! -s $source_ref ]]; then
    echo "FAIL $name: no main-gapsrc-720p in $h264; run its check first"
    failed=1
    continue
  fi
  # shellcheck disable=SC2086 # $edits is a list of options
  if ! "$craft" "$h264/main-gapsrc-720p.264" "$out/$stream" $edits --check > "$out/$name.check" 2>&1; then
    echo "FAIL $name: h264-craft failed: $(tail -n1 "$out/$name.check")"
    failed=1
    continue
  fi
  # Every picture h264-craft's stand-in sends must be one slice, and it
  # must send some.
  if ! awk '/pictures, the last/ { bad = 1 }
      /to the accelerator/ && !/to the accelerator: nothing/ { n++; if (!/ 1 slices/) bad = 1 }
      END { exit bad || !n }' "$out/$name.check"; then
    echo "FAIL $name: the stand-in shows a sample's garbage joining a picture, so it goes to no GPU"
    failed=1
    continue
  fi
  { sed -n '1,20p' "$source_ref"; sed -n '31,60p' "$source_ref"; } | sort > "$out/$name.kept.sorted"
  for mode in sync async; do
    label=$name.$mode
    flags=("${copy_flags[@]}")
    [[ $mode == async ]] && flags+=(--async)
    if ! framemd5 "$out/$stream.mp4" ${flags[@]+"${flags[@]}"} > "$out/$label.frames" 2> "$out/$label.log"; then
      echo "FAIL $label: $(tail -n1 "$out/$label.log")"
      failed=1
      continue
    fi
    awk '{ print $NF }' "$out/$label.frames" | sort > "$out/$label.sorted"
    n_got=$(wc -l < "$out/$label.sorted")
    n_refused=$(grep -c '^frame [0-9]*: status -12909,' "$out/$label.log" || true)
    no_image=$(grep -c '^frame -\?[0-9]*: status ' "$out/$label.log" || true)
    missing=$(comm -23 "$out/$name.kept.sorted" "$out/$label.sorted" | wc -l)
    if [[ $n_refused != "$refused" || $no_image != "$refused" || $n_got != "$frames" || $missing != 0 ]]; then
      echo "FAIL $label: $n_got frames ($frames expected), $n_refused refused ($refused), $missing of the source's 50 missing"
      failed=1
    else
      echo "PASS $label: $n_got frames, the source's 50 among them, and $n_refused refused"
    fi
  done
done
exit "$failed"
