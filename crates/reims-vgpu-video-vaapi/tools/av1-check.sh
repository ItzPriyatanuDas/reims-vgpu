#!/usr/bin/env bash
# The VA-API AV1 decoder against software references.
#
#   crates/reims-vgpu-video-vaapi/tools/av1-check.sh [render node] [stream...]
#
# Runs on Linux with libva, aomenc, SvtAv1EncApp, rav1e, libaom's headers
# and ffmpeg. They are test oracles only: the encoders
# write the test streams from ffmpeg's synthetic sources, tools/av1-svc.c
# (built here against libaom) writes the scalable ones, and ffmpeg decodes
# them in software, with libdav1d and with libaom, for the reference MD5 of
# each frame. None is linked into anything, and nothing they write is
# committed: it all stays under build/. The streams and references are made
# first, on the CPU alone; the GPU's locks are taken for the decodes after.
#
# A stream in `covers` below must show, in `av1-info --frames`, what it is
# there for (a forward key frame shown, global motion, its layers), or it
# fails before any decode: an encoder that stops writing a feature should
# not pass for covering it.
#
# Each stream is decoded by examples/av1-framemd5 from its IVF file (each
# frame a temporal unit with its own sequence headers) and from its MP4
# (the av1C record and the samples, as VideoToolbox is handed them); every
# frame's MD5 must equal libdav1d's and libaom's. The MD5s are over the
# planar layout those decode into (av1-framemd5 --planar), so that ffmpeg
# converts nothing: its conversion to P010 gets an odd width's last chroma
# pair wrong. host-session-check.sh's references, libdav1d's frames as the
# guest's buffers hold them, in NV12 or P010, each at its own size, are
# packed by the conformance suite's planar-packed (lib.sh's guest_frames)
# into NAME.libdav1d.guest, for every colour stream. Streams the decoder
# must refuse (4:4:4, 12 bits) must fail naming their profile. aom-720p-pad-under and -over are aom-720p with one sample
# padded to just under and just over the guest's 0x200000 bytes a message
# (examples/av1-pad): the same frames, in one message and in two.
#
# Name streams to run only those, as the first column of the table below.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

node=${1:-/dev/dri/renderD128}
shift || true
only=" $* "
out=${RVGV_AV1_OUT:-$root/build/av1-check}
mkdir -p "$out"

build reims-vgpu-video-vaapi av1-framemd5 av1-info
build reims-vgpu-video-conformance av1-pad av1-mp4 planar-packed
tool=$CARGO_TARGET_DIR/release/examples/av1-framemd5
info=$CARGO_TARGET_DIR/release/examples/av1-info
pad=$CARGO_TARGET_DIR/release/examples/av1-pad
av1mp4=$CARGO_TARGET_DIR/release/examples/av1-mp4
svc=$out/av1-svc
svc_source=$root/crates/reims-vgpu-video-vaapi/tools/av1-svc.c
if [[ ! -x $svc || $svc -ot $svc_source ]]; then
  cc -O2 -Wall -Wextra -o "$svc" "$svc_source" -laom
fi

# name|encoder|source|size|frames|bits|options
# A stream named -mono is monochrome: its reference is the luma plane
# alone (av1-framemd5 --monochrome), as libdav1d decodes it.
# The sources are ffmpeg's: noise is testsrc2 with temporal noise, which
# keeps every tool busy; mandel is mandelbrot; plain is testsrc2 alone,
# screen-like; grainy is testsrc2 under heavy noise, for the encoders that
# estimate film grain; zoom and pan move a larger testsrc2 under the frame,
# in and sideways, for global motion. Each is scaled to the size asked,
# since testsrc2 rounds an odd size down to even.
# The svc encoder is tools/av1-svc.c, its options its spatial layers,
# temporal layers and scale step.
aom="--cpu-used=6 --threads=8"
streams=(
  "aom-720p|aom|noise|1280x720|60|8|$aom --kf-max-dist=30 --kf-min-dist=30"
  "aom-720p-10bit|aom|noise|1280x720|60|10|$aom --bit-depth=10"
  "aom-1080p-tiles|aom|mandel|1920x1080|30|8|$aom --tile-columns=2 --tile-rows=1 --num-tile-groups=3"
  "aom-1080p-tiles-10bit|aom|noise|1920x1080|20|10|$aom --bit-depth=10 --tile-columns=1 --tile-rows=2 --num-tile-groups=2"
  "aom-720p-nofilters|aom|noise|1280x720|30|8|$aom --enable-cdef=0 --enable-restoration=0"
  "aom-480p-lr|aom|noise|854x480|20|8|--cpu-used=2 --threads=8 --enable-restoration=1 --enable-cdef=1"
  "aom-480p-lr-10bit|aom|noise|854x480|20|10|--cpu-used=2 --threads=8 --bit-depth=10 --enable-restoration=1"
  "aom-720p-superres|aom|noise|1280x720|30|8|$aom --superres-mode=1 --superres-denominator=12 --superres-kf-denominator=10"
  "aom-720p-superres-random|aom|noise|1280x720|30|10|$aom --bit-depth=10 --superres-mode=2"
  "aom-641x361|aom|noise|641x361|30|8|$aom"
  "aom-641x361-10bit|aom|noise|641x361|30|10|$aom --bit-depth=10"
  "aom-63x65|aom|noise|63x65|10|8|$aom"
  "aom-720p-sframe|aom|noise|1280x720|60|8|$aom --sframe-dist=10 --sframe-mode=1"
  # Forward key frames: coded hidden, then shown from their slot, which
  # refreshes every slot. --kf-max-dist alone wrote none, and with key
  # frame filtering on, libaom's default, they were coded showable_frame 0
  # and shown anyway, which libaom's decoder refuses: so filtering is off
  # here, and on in aom-360p-fwdkf-filtered.
  "aom-720p-fwdkf|aom|noise|1280x720|60|8|$aom --enable-fwd-kf=1 --kf-max-dist=30 --fwd-kf-dist=16 --enable-keyframe-filtering=0"
  # The same with key frame filtering on: forward key frames coded
  # showable_frame 0 and shown, which Apple's host shows and libdav1d
  # decodes, and libaom refuses, so it is checked against libdav1d alone.
  "aom-360p-fwdkf-filtered|aom|noise|640x360|60|8|$aom --kf-max-dist=30 --enable-fwd-kf=1 --fwd-kf-dist=16"
  # Key frames coded hidden and shown through an inter overlay frame
  # (libaom's key frame filtering 2). ffmpeg's stream copy writes no sample
  # of it, so its MP4 is av1-mp4's.
  "aom-720p-kfflt2|aom|noise|1280x720|60|8|$aom --enable-keyframe-filtering=2 --kf-max-dist=24 --kf-min-dist=24 --lag-in-frames=48"
  # Global motion: libaom writes ROTZOOM models alone.
  "aom-720p-gm-zoom|aom|zoom|1280x720|30|8|--cpu-used=4 --threads=8 --enable-global-motion=1"
  "aom-720p-gm-pan-10bit|aom|pan|1280x720|30|10|--cpu-used=4 --threads=8 --bit-depth=10 --enable-global-motion=1"
  # Scalable streams of several operating points (tools/av1-svc.c):
  # temporal layers alone, then spatial layers of half the size, quality
  # layers of one size, and three of each. A unit of several spatial layers
  # shows the top one.
  "svc-720p-l1t2|svc|noise|1280x720|60|8|1 2 2"
  "svc-720p-l1t3|svc|noise|1280x720|60|8|1 3 2"
  "svc-720p-l2t1|svc|noise|1280x720|60|8|2 1 2"
  "svc-720p-l2t1q|svc|noise|1280x720|60|8|2 1 1"
  "svc-720p-l3t3|svc|noise|1280x720|60|8|3 3 2"
  "aom-720p-resilient|aom|noise|1280x720|30|8|$aom --error-resilient=1"
  "aom-360p-screen|aom|plain|640x360|10|8|--cpu-used=1 --threads=8 --tune-content=screen"
  "aom-360p-resize-random|aom|noise|640x360|30|8|$aom --resize-mode=2"
  # Key frames of 640x360 and inter frames of 569x320, or 465x262, between:
  # scaled references, and frames smaller than the session's, which the
  # host session lends buffers of their own size (host-session-check.sh).
  "aom-360p-resize-569|aom|noise|640x360|30|8|$aom --resize-mode=1 --resize-denominator=9 --resize-kf-denominator=8 --kf-max-dist=10"
  "aom-360p-resize-465-10bit|aom|noise|640x360|30|10|$aom --bit-depth=10 --resize-mode=1 --resize-denominator=11 --resize-kf-denominator=8 --kf-max-dist=10"
  "aom-720p-deltaq|aom|noise|1280x720|30|8|--cpu-used=4 --threads=8 --aq-mode=1 --deltaq-mode=1 --enable-tpl-model=1 --delta-lf-mode=1"
  "aom-360p-lossless|aom|noise|640x360|10|8|$aom --lossless=1"
  "aom-720p-mono|aom|noise|1280x720|30|8|$aom --monochrome"
  "aom-720p-mono-10bit|aom|noise|1280x720|30|10|$aom --bit-depth=10 --monochrome"
  "aom-720p-resize|aom|noise|1280x720|30|8|$aom --resize-mode=1 --resize-denominator=12 --resize-kf-denominator=12"
  "svt-1080p|svt|noise|1920x1080|60|8|--preset 8 --keyint 30 --enable-overlays 1"
  "svt-1080p-10bit|svt|noise|1920x1080|60|10|--preset 8 --input-depth 10 --tile-columns 1 --tile-rows 1"
  "svt-720p-opengop|svt|noise|1280x720|60|8|--preset 8 --keyint 30 --irefresh-type 1"
  "svt-720p-grain|svt|grainy|1280x720|30|8|--preset 8 --film-grain 10"
  "svt-720p-grain-10bit|svt|grainy|1280x720|30|10|--preset 8 --input-depth 10 --film-grain 20 --film-grain-denoise 1"
  "svt-720p-superres|svt|noise|1280x720|30|8|--preset 8 --superres-mode 1 --superres-denom 14 --superres-kf-denom 9"
  # SVT-AV1 writes no switch frame here, but allows 16384x8704 frames in
  # the sequence header of a 720p stream: above the driver's maximum.
  "svt-720p-bigmax|svt|noise|1280x720|60|8|--preset 8 --pred-struct 1 --sframe-dist 15 --sframe-mode 1"
  "svt-2160p-10bit|svt|noise|3840x2160|20|10|--preset 10 --input-depth 10"
  "svt-2160p|svt|mandel|3840x2160|20|8|--preset 10"
  "rav1e-720p|rav1e|noise|1280x720|60|8|--speed 8 --threads 8 --tiles 4 --keyint 30"
  "rav1e-720p-10bit|rav1e|noise|1280x720|60|10|--speed 8 --threads 8"
  "rav1e-720p-sframe|rav1e|noise|1280x720|60|8|--speed 8 --threads 8 --switch-frame-interval 10 --low-latency"
  "rav1e-720p-grain|rav1e|noise|1280x720|30|8|--speed 8 --threads 8 --photon-noise 8"
  "rav1e-643x363|rav1e|noise|643x363|30|10|--speed 8 --threads 8"
)
# Each of libaom's film grain test vectors, small, at 8 and 10 bits.
for vector in $(seq 1 16); do
  streams+=("aom-grain-$vector|aom|noise|352x288|10|8|$aom --film-grain-test=$vector")
done
for vector in 1 4 10 16; do
  streams+=("aom-grain-$vector-10bit|aom|noise|352x288|10|10|$aom --bit-depth=10 --film-grain-test=$vector")
done
# What each stream named here is for, as a pattern av1-info --frames must
# match (grep -E), its lines joined into one.
declare -A covers=(
  [aom-720p-fwdkf]='[1-9][0-9]* of them key frames'
  [aom-360p-fwdkf-filtered]='decode Key show 0 showable 0'
  [aom-720p-kfflt2]='decode Key show 0'
  [aom-720p-gm-zoom]='rotzoom [1-9]'
  [aom-720p-gm-pan-10bit]='rotzoom [1-9]'
  [svc-720p-l1t2]='idc@level\): 0x103@'
  [svc-720p-l1t3]='idc@level\): 0x107@'
  [svc-720p-l2t1]='idc@level\): 0x301@.*sizes 640x360 1280x720'
  [svc-720p-l2t1q]='idc@level\): 0x301@'
  [svc-720p-l3t3]='idc@level\): 0x707@.*sizes 320x180 640x360 1280x720'
  [aom-360p-resize-569]='sizes 569x320 640x360'
  [aom-360p-resize-465-10bit]='sizes 465x262 640x360'
)
# The streams libaom refuses, checked against libdav1d alone.
libdav1d_only="aom-360p-fwdkf-filtered"
# What must be refused, and what the refusal names.
refused=(
  "aom-444|aom|noise|640x360|5|8|$aom --profile=1"
  "aom-12bit|aom|noise|640x360|5|12|$aom --profile=2 --bit-depth=12"
)
declare -A refusal=(
  [aom-444]='the driver cannot: AV1 High profile'
  [aom-12bit]='the driver cannot: AV1 Professional profile'
)

# The stream NAME as IVF and as an MP4; 1 where either is not made, which
# leaves neither for a later run to take.
encode() {
  local name=$1 encoder=$2 source=$3 size=$4 frames=$5 bits=$6 opts=$7
  local ivf=$out/$name.ivf
  if [[ -s $ivf && -s $out/$name.mp4 ]]; then
    return
  fi
  rm -f "$ivf" "$out/$name.mp4"
  local graph pix y4m=$out/$name.src.y4m
  case $source in
    noise) graph="testsrc2=size=$size:rate=30,noise=alls=14:allf=t+u" ;;
    grainy) graph="testsrc2=size=$size:rate=30,noise=alls=40:allf=t" ;;
    mandel) graph="mandelbrot=size=$size:rate=30" ;;
    plain) graph="testsrc2=size=$size:rate=30" ;;
    zoom) graph="testsrc2=size=1920x1080:rate=30,zoompan=z='1+0.01*on':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s=$size:fps=30,noise=alls=6:allf=t+u" ;;
    pan) graph="testsrc2=size=2560x1440:rate=30,crop=${size/x/:}:x='40*n':y='20*n',noise=alls=6:allf=t+u" ;;
  esac
  graph="$graph,scale=${size/x/:}"
  case $bits in
    8) pix=yuv420p ;;
    10) pix=yuv420p10le ;;
    12) pix=yuv420p12le ;;
  esac
  if [[ $opts == *--profile=1* ]]; then
    pix=yuv444p
  fi
  local part=$out/$name.part.ivf
  ffmpeg -nostdin -hide_banner -loglevel error -y -f lavfi -i "$graph" -frames:v "$frames" \
      -pix_fmt "$pix" -strict -1 "$y4m" || return 1
  # shellcheck disable=SC2086 # $opts is a list of options
  case $encoder in
    aom) aomenc --quiet --ivf --limit="$frames" $opts -o "$part" "$y4m" ;;
    svt) SvtAv1EncApp -i "$y4m" -b "$part" -n "$frames" $opts ;;
    rav1e) rav1e "$y4m" -o "$part" -y --quiet --limit "$frames" $opts ;;
    svc) "$svc" $opts "$y4m" "$part" ;;
  esac > "$out/$name.encode.log" 2>&1 < /dev/null || { rm -f "$y4m" "$part"; return 1; }
  rm -f "$y4m"
  [[ -s $part ]] || return 1
  ffmpeg -nostdin -hide_banner -loglevel error -y -i "$part" -c copy -f mp4 \
    "$out/$name.part.mp4" || return 1
  # ffmpeg drops the samples before the first it flags a key frame, which
  # may be all of them: av1-mp4 writes each temporal unit then.
  if [[ $(packets "$out/$name.part.mp4") != "$(packets "$part")" ]]; then
    "$av1mp4" "$part" "$out/$name.part.mp4" >> "$out/$name.encode.log" || return 1
  fi
  mv "$out/$name.part.mp4" "$out/$name.mp4"
  mv "$part" "$ivf"
}

# How many packets ffmpeg reads from a file.
packets() {
  ffprobe -v error -select_streams v:0 -count_packets -show_entries stream=nb_read_packets \
    -of csv=p=0 "$1" < /dev/null || echo 0
}

# The software decode's MD5 of each frame, in the layout it decodes into,
# and for libdav1d's also packed as the guest's buffers hold them.
reference() {
  local name=$1 bits=$2 decoder=$3
  local pix=yuv420p
  if [[ $name == *-mono* ]]; then
    pix=gray
  fi
  if [[ $bits != 8 ]]; then
    pix=${pix}10le
  fi
  if [[ ! -s $out/$name.$decoder.ref ]]; then
    # -noautoscale keeps each frame at its own size, where frames differ.
    hashes "$out/$name.$decoder.ref" -c:v "$decoder" -i "$out/$name.ivf" \
      -noautoscale -fps_mode passthrough -pix_fmt "$pix"
  fi
  # NV12 or P010, as the guest's buffers hold them, each frame at its own
  # size, for host-session-check.sh.
  local guest=$out/$name.$decoder.guest
  if [[ $decoder == libdav1d && $name != *-mono* && ! -s $guest ]] &&
      ! guest_frames "$guest" "$decoder" "$bits" "$out/$name.ivf" "$out/$name.$decoder.ref"; then
    echo "FAIL $name: no NV12 or P010 reference: $(tail -n1 "$guest.log")"
    failed=1
  fi
}

echo "av1-check on $node, $(date -Is), $(hostname)"
# The streams and their references, on the CPU.
made=()
for entry in "${streams[@]}" "${refused[@]}"; do
  IFS='|' read -r name encoder source size frames bits opts <<<"$entry"
  wanted "$name" || continue
  if ! encode "$name" "$encoder" "$source" "$size" "$frames" "$bits" "$opts"; then
    echo "FAIL $name: the encoder failed: $(tail -n2 "$out/$name.encode.log" | tr '\n' ' ')"
    failed=1
    continue
  fi
  if [[ " ${refused[*]} " != *" $entry "* ]]; then
    reference "$name" "$bits" libdav1d
    [[ " $libdav1d_only " == *" $name "* ]] || reference "$name" "$bits" libaom-av1
  fi
  made+=("$name")
done

lock_gpu --node "$node"

for entry in "${streams[@]}"; do
  IFS='|' read -r name encoder source size frames bits opts <<<"$entry"
  [[ " ${made[*]} " == *" $name "* ]] || continue
  echo "---- $name: $("$info" "$out/$name.ivf" | paste -sd '|' - | sed 's/|/; /g')"
  if [[ -n ${covers[$name]:-} ]] &&
      ! "$info" --frames "$out/$name.ivf" | tr '\n' ' ' | grep -E "${covers[$name]}" > /dev/null; then
    echo "FAIL $name: av1-info --frames does not show ${covers[$name]}"
    failed=1
    continue
  fi
  libaom=1
  if [[ " $libdav1d_only " == *" $name "* ]]; then
    libaom=0
    echo "     NOTE libaom refuses $name: libdav1d's decode alone is its reference"
  elif ! cmp -s "$out/$name.libdav1d.ref" "$out/$name.libaom-av1.ref"; then
    echo "     NOTE libdav1d and libaom differ on $name"
  fi
  layout=(--planar)
  [[ $name != *-mono* ]] || layout=(--monochrome)
  for form in ivf mp4; do
    if run "$out/$name.$form" "$name.$form" "${layout[@]}"; then
      check "$name ($form, libdav1d)" "$out/$name.libdav1d.ref" "$out/$name.$form.md5"
      if ((libaom)); then
        check "$name ($form, libaom)" "$out/$name.libaom-av1.ref" "$out/$name.$form.md5"
      fi
    fi
  done
done

# The caller-facing options, on one stream each.
if wanted options; then
  if run "$out/aom-1080p-tiles.mp4" aom-1080p-tiles.stride --planar --stride-pad 192; then
    check "aom-1080p-tiles (padded strides)" "$out/aom-1080p-tiles.libdav1d.ref" "$out/aom-1080p-tiles.stride.md5"
  fi
  if run "$out/svt-1080p.mp4" svt-1080p.hold --planar --hold 8; then
    check "svt-1080p (8 frames held by the caller)" "$out/svt-1080p.libdav1d.ref" "$out/svt-1080p.hold.md5"
  fi
  # A seek: every reference forgotten before the key frame at sample 30.
  if run "$out/aom-720p.mp4" aom-720p.reset --planar --reset-at 30; then
    check "aom-720p (references reset at the key frame of sample 30)" "$out/aom-720p.libdav1d.ref" "$out/aom-720p.reset.md5"
  fi
fi

# One sample of aom-720p padded with an OBU_PADDING, which changes no frame,
# to just under and just over the guest's 0x200000 bytes a message: its
# references are aom-720p's.
padded() {
  local name=$1 size=$2
  wanted "$name" || return 0
  if [[ ! -s $out/aom-720p.mp4 || ! -s $out/aom-720p.libdav1d.guest ]]; then
    echo "FAIL $name: no aom-720p to pad; run aom-720p first"
    failed=1
    return 0
  fi
  "$pad" "$out/aom-720p.mp4" "$out/$name.mp4" 2 "$size" > /dev/null
  cp "$out/aom-720p.libdav1d.ref" "$out/$name.libdav1d.ref"
  cp "$out/aom-720p.libdav1d.guest" "$out/$name.libdav1d.guest"
  if run "$out/$name.mp4" "$name.mp4" --planar; then
    check "$name (mp4, aom-720p's references)" "$out/$name.libdav1d.ref" "$out/$name.mp4.md5"
  fi
}
padded aom-720p-pad-under 2093056
padded aom-720p-pad-over 2101248

# A new sequence at a key frame, with another size or bit depth, mid-stream:
# the decoder makes a new session and goes on. The reference is the two
# halves'.
concat() {
  local name=$1 first=$2 second=$3
  wanted "$name" || return 0
  { cat "$out/$first.ivf"; tail -c +33 "$out/$second.ivf"; } > "$out/$name.ivf"
  cat "$out/$first.libdav1d.ref" "$out/$second.libdav1d.ref" > "$out/$name.libdav1d.ref"
  if run "$out/$name.ivf" "$name.ivf" --planar; then
    check "$name (ivf, the halves' references)" "$out/$name.libdav1d.ref" "$out/$name.ivf.md5"
  fi
}
concat switch-720p-1080p aom-720p svt-1080p
concat switch-8bit-10bit aom-720p rav1e-720p-10bit
concat switch-grain aom-720p svt-720p-grain

for entry in "${refused[@]}"; do
  IFS='|' read -r name encoder source size frames bits opts <<<"$entry"
  [[ " ${made[*]} " == *" $name "* ]] || continue
  for form in ivf mp4; do
    refused "$name ($form)" "$out/$name.$form" "${refusal[$name]}"
  done
done

exit "$failed"
