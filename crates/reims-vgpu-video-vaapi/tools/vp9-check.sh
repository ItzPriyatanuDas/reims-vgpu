#!/usr/bin/env bash
# The VA-API VP9 decoder against software references.
#
#   crates/reims-vgpu-video-vaapi/tools/vp9-check.sh [render node] [stream...]
#
# Runs on Linux with libva, vpxenc and ffmpeg. They are test
# oracles only: vpxenc writes the test streams from ffmpeg's synthetic
# sources, and ffmpeg decodes them in software, with its own vp9 decoder and
# with libvpx, for the reference MD5 of each frame. None is linked into
# anything, and nothing they write is committed: it all stays under build/.
#
# Each stream is decoded by examples/vp9-framemd5 from its IVF file and from
# its MP4 (the vpcC record and the samples, as VideoToolbox is handed them);
# every frame's MD5 must equal both references'. The MD5s are over the
# planar layout those decode into (yuv420p, yuv420p10le), so that ffmpeg
# converts nothing. The guest-side references (.guest) are ffmpeg's vp9
# decode packed into NV12 or P010 by the conformance suite's planar-packed
# (lib.sh's guest_frames), each frame at its own size, exact at every size,
# where ffmpeg's own P010 of an odd width is not; the decoder's NV12 or
# P010 (--packed) is compared with them on every stream. What libvpx's
# encoder never writes (intra-only frames, shown existing frames, every
# segment feature on its own, loop filter deltas that vary) is written into
# its streams by examples/vp9-craft; segments held to a reference or
# skipped, which libvpx writes only from an ROI map no vpxenc option sets,
# come from tools/vp9-segments.c, built here against the host's libvpx.
# Streams the decoder must refuse (Profiles 1 and 3, 12 bits) must fail
# naming what it cannot decode.
#
# Name streams to run only those, as the first column of the tables below.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

node=${1:-/dev/dri/renderD128}
shift || true
only=" $* "
out=${RVGV_VP9_OUT:-$root/build/vp9-check}
mkdir -p "$out"

build reims-vgpu-video-vaapi vp9-framemd5 vp9-info vp9-craft
build reims-vgpu-video-conformance planar-packed
tool=$CARGO_TARGET_DIR/release/examples/vp9-framemd5
info=$CARGO_TARGET_DIR/release/examples/vp9-info
craft=$CARGO_TARGET_DIR/release/examples/vp9-craft
segments=$out/vp9-segments
cc -O2 -Wall -Wextra -o "$segments" "$root/crates/reims-vgpu-video-vaapi/tools/vp9-segments.c" -lvpx
# The streams are made between decodes, so the locks are held from here.
lock_gpu --node "$node"

# name|source|size|frames|bits|vpxenc options, or segments:MODE for
# vp9-segments
# The sources are ffmpeg's: noise is testsrc2 with temporal noise, which
# keeps every tool busy; noisy-start is testsrc2 under heavy noise for its
# first two seconds alone; mandel is mandelbrot; plain is testsrc2 alone,
# screen-like. Each is scaled to the size asked, since testsrc2 rounds an
# odd size down to even. vpxenc's frames are frame-parallel unless told
# otherwise, adapting no probabilities backward, so most streams turn that
# off; the -parallel and -resilient ones keep it.
good="--good --cpu-used=4 --threads=8 --frame-parallel=0"
rt="--rt --cpu-used=8 --threads=8 --frame-parallel=0 --lag-in-frames=0"
p2="--profile=2 --bit-depth=10 --input-bit-depth=10"
arf="--auto-alt-ref=1 --lag-in-frames=25"
streams=(
  "p0-720p|noise|1280x720|60|8|$good $arf --kf-max-dist=30"
  "p2-720p|noise|1280x720|60|10|$good $p2 $arf --kf-max-dist=30"
  "p0-720p-arf6|noise|1280x720|60|8|$good --auto-alt-ref=6 --lag-in-frames=25"
  # Tile rows need one thread (vpxenc --help).
  "p0-1080p-tiles|mandel|1920x1080|30|8|--good --cpu-used=4 --threads=1 --frame-parallel=0 --tile-columns=2 --tile-rows=2"
  "p2-1080p-tiles|noise|1920x1080|20|10|--good --cpu-used=4 --threads=1 --frame-parallel=0 $p2 --tile-columns=1 --tile-rows=1"
  "p0-720p-resilient|noise|1280x720|30|8|$good $arf --error-resilient=1"
  "p2-720p-resilient|noise|1280x720|30|10|$good $p2 --error-resilient=1"
  # Key frames every 20 frames, with no backward adaptation, for
  # vp9-craft's intra-only frames.
  "p0-720p-parallel|noise|1280x720|60|8|$good $arf --frame-parallel=1 --kf-max-dist=20 --kf-min-dist=20"
  "p2-720p-parallel|noise|1280x720|40|10|$good $p2 $arf --frame-parallel=1 --kf-max-dist=20 --kf-min-dist=20"
  "p0-720p-aq1|noise|1280x720|30|8|$good --aq-mode=1"
  "p0-720p-aq2|noise|1280x720|30|8|$good --aq-mode=2 --sharpness=4"
  "p0-720p-aq3|noise|1280x720|60|8|$rt --aq-mode=3 --end-usage=cbr --target-bitrate=1500"
  "p2-720p-aq3|noise|1280x720|30|10|$rt $p2 --aq-mode=3 --end-usage=cbr --target-bitrate=2000"
  "p0-360p-lossless|noise|640x360|10|8|$good --lossless=1"
  "p2-360p-lossless|noise|640x360|10|10|$good $p2 --lossless=1"
  # Frame sizes that change between inter frames, which predict from
  # references of another size, each frame rendered at the source's size:
  # libvpx's dynamic resize, starved of bits. vpxenc's fixed resize
  # (--resize-width) changed no size (libvpx 1.17.0's vpxenc, 2026-10-24).
  "p0-720p-resize|noisy-start|1280x720|240|8|$rt --end-usage=cbr --target-bitrate=500 --resize-allowed=1 --drop-frame=0"
  "p2-720p-resize|noisy-start|1280x720|240|10|$rt $p2 --end-usage=cbr --target-bitrate=700 --resize-allowed=1 --drop-frame=0"
  "p0-720p-resize-early|noise|1280x720|150|8|$rt --end-usage=cbr --target-bitrate=150 --resize-allowed=1 --drop-frame=0"
  "p0-360p-screen|plain|640x360|20|8|$rt --tune-content=screen"
  "p0-641x361|noise|641x361|30|8|$good $arf"
  "p2-643x363|noise|643x363|30|10|$good $p2 $arf"
  "p0-63x65|noise|63x65|10|8|$good"
  "p0-2160p|mandel|3840x2160|20|8|$rt --tile-columns=3"
  "p2-2160p|noise|3840x2160|20|10|$rt $p2 --tile-columns=2"
  # Segments held to a reference, skipped, or both, beside quantizer and
  # loop filter deltas (vp9-segments.c says what each mode's segments do).
  "p0-720p-seg-ref|noise|1280x720|60|8|segments:ref"
  "p0-720p-seg-skip|noise|1280x720|60|8|segments:skip"
  "p0-720p-seg-mixed|noise|1280x720|60|8|segments:mixed"
  "p2-720p-seg-mixed|noise|1280x720|60|10|segments:mixed"
  "p0-720p-seg-mixed-skip|noise|1280x720|60|8|segments:mixed-skip"
  "p2-720p-seg-mixed-skip|noise|1280x720|60|10|segments:mixed-skip"
)
# name|stream it is made from|vp9-craft mode
crafted=(
  "p0-720p-intra-only|p0-720p-parallel|intra-only:20"
  "p2-720p-intra-only|p2-720p-parallel|intra-only:20"
  "p0-720p-show-existing|p0-720p|show-existing"
  "p2-720p-show-existing|p2-720p|show-existing"
  "p0-720p-features|p0-720p|features"
  "p2-720p-features|p2-720p|features"
  "p0-720p-resilient-features|p0-720p-resilient|features"
  "p0-641x361-features|p0-641x361|features"
)
# What must be refused, and what the refusal names: the IVF reader's from
# the frame header, the MP4 reader's from the vpcC record.
refused=(
  "p1-444|noise|640x360|5|8|$good --profile=1"
  "p3-444-10bit|noise|640x360|5|10|$good --profile=3 --bit-depth=10 --input-bit-depth=10"
  "p2-12bit|noise|640x360|5|12|$good --profile=2 --bit-depth=12 --input-bit-depth=12"
)
declare -A refusal=(
  [p1-444]='(unsupported|the driver cannot): VP9 Profile 1 '
  [p3-444-10bit]='(unsupported|the driver cannot): VP9 Profile 3 '
  [p2-12bit]='(unsupported|the driver cannot): VP9 (Profile 2 )?at 12 bits'
)

# The stream NAME as IVF and as an MP4; 1 where either is not made, which
# leaves neither for a later run to take. A stream made again drops the
# references of the one before.
encode() {
  local name=$1 source=$2 size=$3 frames=$4 bits=$5 opts=$6
  local ivf=$out/$name.ivf
  if [[ -s $ivf && -s $out/$name.mp4 ]]; then
    return
  fi
  rm -f "$ivf" "$out/$name.mp4" "$out/$name".*.ref "$out/$name".*.guest*
  local graph pix y4m=$out/$name.src.y4m part=$out/$name.part.ivf
  case $source in
    noise) graph="testsrc2=size=$size:rate=30,noise=alls=14:allf=t+u" ;;
    noisy-start) graph="testsrc2=size=$size:rate=30,noise=alls=40:allf=t+u:enable='lt(t,2)'" ;;
    mandel) graph="mandelbrot=size=$size:rate=30" ;;
    plain) graph="testsrc2=size=$size:rate=30" ;;
  esac
  graph="$graph,scale=${size/x/:}"
  pix=yuv420p
  [[ $opts != *--profile=[13]* ]] || pix=yuv444p
  case $bits in
    10) pix=${pix}10le ;;
    12) pix=${pix}12le ;;
  esac
  ffmpeg -nostdin -hide_banner -loglevel error -y -f lavfi -i "$graph" -frames:v "$frames" \
      -pix_fmt "$pix" -strict -1 "$y4m" || return 1
  if [[ $opts == segments:* ]]; then
    "$segments" "${opts#segments:}" "$y4m" "$part" > "$out/$name.encode.log" 2>&1 ||
      { rm -f "$y4m" "$part"; return 1; }
  else
    # shellcheck disable=SC2086 # $opts is a list of options
    vpxenc --quiet --disable-warning-prompt --ivf --limit="$frames" $opts -o "$part" "$y4m" \
      > "$out/$name.encode.log" 2>&1 < /dev/null || { rm -f "$y4m" "$part"; return 1; }
  fi
  rm -f "$y4m"
  [[ -s $part ]] || return 1
  mv "$part" "$ivf"
  mp4 "$name"
}

# What a vp9-segments stream must carry, from vp9-info's counts: the
# features its mode asks for, on every inter frame.
covers() {
  local name=$1 mode=$2 want summary inter key count
  case $mode in
    ref) want="seg-ref-last seg-ref-intra seg-ref-golden" ;;
    skip) want="seg-skip seg-ref-last-skip seg-alt-q" ;;
    mixed) want="seg-ref-last seg-ref-intra seg-ref-golden seg-alt-q seg-alt-lf" ;;
    mixed-skip) want="seg-skip seg-ref-last-skip seg-alt-q seg-alt-lf" ;;
  esac
  summary=$("$info" "$out/$name.ivf" | tail -n1)
  inter=$(tr ' ' '\n' <<<"$summary" | awk -F= '$1 == "inter" { print $2 }')
  for key in seg-enabled $want; do
    count=$(tr ' ' '\n' <<<"$summary" | awk -F= -v k="$key" '$1 == k { print $2 }')
    # GOLDEN is held only where libvpx can use it: not on the inter frames
    # right after a golden refresh.
    if [[ -z $count || ( $key != *golden* && $count != "$inter" ) ]]; then
      echo "FAIL $name: $key on ${count:-0} of $inter inter frames"
      failed=1
    fi
  done
}

# NAME.ivf as an MP4, written whole or not at all; 1 where ffmpeg fails.
mp4() {
  ffmpeg -nostdin -hide_banner -loglevel error -y -i "$out/$1.ivf" -c copy "$out/$1.part.mp4" ||
    { rm -f "$out/$1.part.mp4"; return 1; }
  mv "$out/$1.part.mp4" "$out/$1.mp4"
}

# A software decode's MD5 of each frame, in the layout it decodes into.
reference() {
  local name=$1 bits=$2 decoder=$3
  local pix=yuv420p
  [[ $bits == 8 ]] || pix=yuv420p10le
  # -noautoscale keeps each frame at its own size, where frames differ.
  if [[ ! -s $out/$name.$decoder.ref ]]; then
    hashes "$out/$name.$decoder.ref" -c:v "$decoder" -i "$out/$name.ivf" \
      -noautoscale -fps_mode passthrough -pix_fmt "$pix"
  fi
}

verify() {
  local name=$1 bits=$2 guest=$out/$1.vp9.guest packed=1
  reference "$name" "$bits" vp9
  reference "$name" "$bits" libvpx-vp9
  # ffmpeg's vp9 decode as the guest's buffers hold it, in NV12 or P010.
  if [[ ! -s $guest ]] &&
      ! guest_frames "$guest" vp9 "$bits" "$out/$name.ivf" "$out/$name.vp9.ref"; then
    echo "FAIL $name: no NV12 or P010 reference: $(tail -n1 "$guest.log")"
    failed=1
    packed=0
  fi
  echo "---- $name: $("$info" "$out/$name.ivf" | tail -n1)"
  if ! cmp -s "$out/$name.vp9.ref" "$out/$name.libvpx-vp9.ref"; then
    echo "     NOTE ffmpeg's vp9 and libvpx differ on $name"
  fi
  for form in ivf mp4; do
    if run "$out/$name.$form" "$name.$form"; then
      check "$name ($form, vp9)" "$out/$name.vp9.ref" "$out/$name.$form.md5"
      check "$name ($form, libvpx)" "$out/$name.libvpx-vp9.ref" "$out/$name.$form.md5"
    fi
  done
  if ((packed)) && run "$out/$name.mp4" "$name.packed" --packed; then
    check "$name (mp4, NV12 or P010)" "$guest" "$out/$name.packed.md5"
  fi
}

echo "vp9-check on $node, $(date -Is), $(hostname)"
for entry in "${streams[@]}"; do
  IFS='|' read -r name source size frames bits opts <<<"$entry"
  wanted "$name" || continue
  if ! encode "$name" "$source" "$size" "$frames" "$bits" "$opts"; then
    echo "FAIL $name: the encoder failed: $(tail -n2 "$out/$name.encode.log" | tr '\n' ' ')"
    failed=1
    continue
  fi
  [[ $opts != segments:* ]] || covers "$name" "${opts#segments:}"
  verify "$name" "$bits"
done

for entry in "${crafted[@]}"; do
  IFS='|' read -r name from mode <<<"$entry"
  wanted "$name" || continue
  if [[ ! -s $out/$from.ivf ]]; then
    echo "FAIL $name: $from.ivf is missing: run $from first"
    failed=1
    continue
  fi
  if [[ ! -s $out/$name.ivf ]]; then
    rm -f "$out/$name.mp4" "$out/$name".*.ref "$out/$name".*.guest*
    if ! "$craft" "$mode" "$out/$from.ivf" "$out/$name.part.ivf" 2> "$out/$name.craft.log"; then
      echo "FAIL $name: vp9-craft failed: $(tail -n1 "$out/$name.craft.log")"
      rm -f "$out/$name.part.ivf"
      failed=1
      continue
    fi
    mv "$out/$name.part.ivf" "$out/$name.ivf"
  fi
  if [[ ! -s $out/$name.mp4 ]] && ! mp4 "$name"; then
    echo "FAIL $name: ffmpeg cannot read vp9-craft's stream"
    failed=1
    continue
  fi
  bits=8
  [[ $name != p2-* ]] || bits=10
  verify "$name" "$bits"
done

# The caller-facing options, on one stream each.
if wanted options; then
  if run "$out/p0-1080p-tiles.mp4" p0-1080p-tiles.stride --stride-pad 192; then
    check "p0-1080p-tiles (padded strides)" "$out/p0-1080p-tiles.vp9.ref" "$out/p0-1080p-tiles.stride.md5"
  fi
  if run "$out/p2-720p.mp4" p2-720p.hold --hold 8; then
    check "p2-720p (8 frames held by the caller)" "$out/p2-720p.vp9.ref" "$out/p2-720p.hold.md5"
  fi
  # A seek: every reference forgotten before the key frame at sample 30.
  if run "$out/p0-720p.mp4" p0-720p.reset --reset-at 30; then
    check "p0-720p (references reset at the key frame of sample 30)" "$out/p0-720p.vp9.ref" "$out/p0-720p.reset.md5"
  fi
fi

# Another size or profile at a key frame, mid-stream: the decoder makes a
# new session and goes on. The reference is the two halves'.
concat() {
  local name=$1 first=$2 second=$3
  wanted "$name" || return 0
  { cat "$out/$first.ivf"; tail -c +33 "$out/$second.ivf"; } > "$out/$name.ivf"
  cat "$out/$first.vp9.ref" "$out/$second.vp9.ref" > "$out/$name.vp9.ref"
  if run "$out/$name.ivf" "$name.ivf"; then
    check "$name (ivf, the halves' references)" "$out/$name.vp9.ref" "$out/$name.ivf.md5"
  fi
}
concat switch-720p-1080p p0-720p p0-1080p-tiles
concat switch-8bit-10bit p0-720p p2-720p
concat switch-10bit-8bit p2-720p p0-641x361

for entry in "${refused[@]}"; do
  IFS='|' read -r name source size frames bits opts <<<"$entry"
  wanted "$name" || continue
  if ! encode "$name" "$source" "$size" "$frames" "$bits" "$opts"; then
    echo "FAIL $name: the encoder failed: $(tail -n2 "$out/$name.encode.log" | tr '\n' ' ')"
    failed=1
    continue
  fi
  for form in ivf mp4; do
    refused "$name ($form)" "$out/$name.$form" "${refusal[$name]}"
  done
done

exit "$failed"
