# The VideoToolbox backend

`crates/reims-vgpu-video-videotoolbox`: the host's codecs and pixel
transfers through VideoToolbox, with no FFmpeg or other layer in
between, for a macOS host. It is core's `Backend`: the decoders, the
encoders and motion estimation, and no JPEG decoder. The scaler device
runs on the library's own GPU kernel instead ([scaler.md](scaler.md),
"The kernel"). This doc says what the crate does, what the M3 Max's
VideoToolbox answers, and how the crate is checked. Every fact names its
run.

**Status.** The backend is built and unit-tested on the M3 Max, and has
never met a guest: no VM on a macOS host has driven the video device on it
([bugs.md](bugs.md), T11 and T12). Zero copy into a guest's pages is on
and unmeasured ("Zero copy").

**The host**, for every run below: an Apple M3 Max, macOS 27.0.1
(26A434), SDK 27.0, on 2026-10-24 unless the run gives another date.

## What is here

Paths in the table are the crate's.

| what | where |
|---|---|
| Our declarations of the CoreFoundation, CoreMedia, CoreVideo, IOSurface and VideoToolbox calls, each checked against the SDK's headers by a test | `src/macos/ffi.rs`, `src/macos/ffi/` |
| VideoToolbox's decoder capabilities dictionary, which has no header ("The private calls") | `src/macos/ffi/private.rs` |
| CoreFoundation objects by its ownership rules: owned references (Create, Copy), borrows that live no longer than what they came from (Get), the framework constants; property lists both ways | `src/macos/cf.rs` |
| IOSurface-backed pixel buffers, locked plane by plane | `src/macos/pixel_buffer.rs` |
| VideoToolbox's sessions, each invalidated before it is released, and the frames' reference values ("Sessions and callbacks") | `src/macos/session.rs` |
| VideoToolbox's statuses as core's backend errors | `src/macos/status.rs` |
| What each codec's record says: its format description, its pixel format | `src/macos/stream_format.rs` |
| What the decoders list | `src/macos/capabilities.rs` |
| The decoders: `VideoToolboxBackend`, `Decoder`, `Frame` | `src/macos/decoder.rs` |
| Zero copy: a decoded frame written into the guest's buffer by a pixel transfer, and the checks before it ("Zero copy") | `src/macos/zero_copy.rs`, `src/macos/ffi/mach.rs` |
| The encoders: `VideoToolboxEncoderBackend`, `Encoder`, and the guest's properties against what each encoder lists | `src/macos/encoder.rs`, `src/macos/encoder/properties.rs` |
| Motion estimation: `VideoToolboxMotionBackend`, on VTMotionEstimationSession, and its declarations ("Motion estimation") | `src/macos/motion.rs`, `src/macos/ffi/motion_estimation.rs` |
| The whole backend, `backend()` | `src/macos/whole.rs` |
| The encoders and decoders against each other | `src/macos/tests.rs` |
| The conformance suite on this backend (DEVICE `hardware`) | `examples/conformance.rs` |
| `vt-check`: a decode through the backend alone, hardware or software, against an MD5 list; `vt-check list`, what the backend offers | `examples/vt-check/` |
| Every check at once ("Checks") | `tools/videotoolbox-check.sh` |

`src/macos` is macOS's alone (`cfg(target_os = "macos")` in lib.rs): on
Linux the crate builds and offers nothing, and `cargo xtask check`'s
clippy for x86_64 Linux checks that. Every `unsafe` block is under
`src/macos`, each with the reason it is sound, which lib.rs's
`deny(clippy::undocumented_unsafe_blocks)` holds it to.

## Sessions and callbacks

Every session the backend makes, of each of the five kinds, is a
`Session` (`session.rs`): torn down by its kind's `...Invalidate`, once,
before its reference is released, so that it calls back no more from
the moment its owner lets it go, whoever else still holds a reference.

A decoder's and an encoder's output callbacks reach their state through
their reference value, `Arc::as_ptr` of state the decoder or encoder holds
until its Drop has invalidated the session; the state is behind a mutex,
since VideoToolbox may call back on a thread of its own, and the encoder's
does. A frame's reference value is its ID's bits, which nothing
dereferences. Neither callback panics, and an `extern "C"` function that
did would end the process rather than unwind into VideoToolbox. The motion
estimator's handler is a block, which the session copies; the copy holds
a reference of its own to where it delivers.

## The declarations

There is no binding crate: the crate depends on nothing but `log` and the
video crates. Each function and static is declared once, through
`declare!`, which also records its Rust types.
`ffi::tests::declarations_match_the_sdk_headers` writes a C file of
`_Static_assert(__builtin_types_compatible_p(__typeof__(NAME), ...))`, one
per function and static, and compiles it with `xcrun clang -Werror`
against the SDK, so a wrong parameter or return type fails the test; it
also compares the sizes of the types, the field offsets of the structs
(`CMTime`, `CMSampleTimingInfo`, `VTDecompressionOutputCallbackRecord`,
`vm_region_basic_info_data_64_t`) and every constant's value with the
headers'. A return type made wrong on purpose (`CMSampleBufferGetNumSamples`
as `i32`) failed it (measured, 2026-10-24). Only what the crate calls or
reads is declared, besides the constants a test holds core's codec and
pixel format codes to.

### The private calls

Apple's host lists to a guest the profiles and levels of VideoToolbox's
decoder capabilities dictionary, which no header declares (read from
Apple's code). The backend reads the same:
`VTCopyDecoderCapabilitiesDictionaryForCodecTypes` (its signature read
from Apple's code: a CFArray of codec types as CFNumbers in, a retained
CFDictionary out) and the keys
`kVTDecoderCapability_CodecSupportDict`,
`kVTDecoderCodecCapability_PerProfileSupport`,
`kVTDecoderProfileCapability_MaxDecodeLevel` and
`kVTDecoderProfileCapability_IsHardwareAccelerated`. Each name is in the
SDK's `VideoToolbox.framework/VideoToolbox.tbd` exports. They are looked
up with `dlsym(RTLD_DEFAULT)`, not linked, so a VideoToolbox without them
answers none and the backend falls back to the public calls: each wanted
profile, with no level. `every_private_name_resolves_here` checks the
keys' strings (`VTCodecSupportDict`, `VTPerProfileSupport`,
`VTMaxDecodeLevel`, `VTIsHardwareAccelerated`).

## What the M3 Max's VideoToolbox offers

`vt-check list` on the M3 Max (2026-10-24):

| part | offered |
|---|---|
| H.264 decode | profiles 66, 77, 88, 100, 110, each to level 80; up to 8192x4320 |
| HEVC decode | profiles 1, 2, 3, each to level 186 (6.2); up to 8192x4320 |
| AV1 decode | profile 0 to level 19 (6.3); up to 16384x8704 |
| VP9 decode | profiles 0 and 2, no level; up to 16384x16384 |
| H.264 encode | profiles 66, 77, 100; up to 4096 wide and 4096 tall; NV12 sources; reordering |
| HEVC encode | profiles 1 and 2; up to 16384 wide and 16384 tall; NV12 and P010 sources; reordering |
| JPEG | none |

What is offered, and why:
- **Hardware only.** A codec is listed where `VTIsHardwareDecodeSupported`
  says so, and a profile where the dictionary marks it hardware, as Apple's
  host marks each profile for its guest (read from Apple's code). Sessions
  require the hardware decoder or encoder
  (`RequireHardwareAccelerated...`). The software decoders are used only
  by the tests, as references.
- **A dictionary with no hardware profile is read again.** Beside
  another process's VideoToolbox tests, the dictionary listed HEVC's
  profiles, then H.264's, with none of them marked hardware, for one read
  each among 25,473 reads in 90 s, while `VTIsHardwareDecodeSupported`
  still said hardware (measured on the M3 Max, 2026-10-24). A backend made
  then would have offered no decoder for that codec. So a codec that
  decodes in hardware but lists no hardware profile is read up to 10 more
  times, 20 ms apart, and the first read that lists some is taken
  (`settled_profiles`); a codec that lists none for good costs 0.2 s once.
- **4:2:0 profiles only.** Core's buffers are NV12 and P010, so H.264's
  122, 244 and 44, HEVC's 4, AV1's 1 and 2 and VP9's 1 and 3 are left out,
  though the hardware lists them; a record of more than 10 bits, or of
  4:2:2 or 4:4:4, is refused at `dsta` as unsupported.
- **Levels** are the dictionary's, as Apple's host lists them: 80 for
  H.264 is above H.264's highest level, 6.2, and is what Apple lists too
  ([devices.md](devices.md), "The levels Apple lists").
- **Decoder sizes** are not in anything VideoToolbox answers. They are
  each standard's picture at the highest level listed, from its tables,
  not measured on the hardware: 8192x4320 for H.264 and HEVC at 6.2
  (H.264 Table A-1, H.265 Table A.8), 16384x8704 for AV1 at 6.3 (Annex
  A.3), 16384 a side for VP9 at 6.2 (its levels table). Core does not
  check them against `dsta`; a stream too large is the decoder's to
  refuse.
- **Encoders are listed whatever their state.** The hardware H.264 and
  HEVC encoders are listed as VideoToolbox's encoder list names them,
  which it does while every hardware encoder session of the Mac is taken,
  as Apple's host lists AVE then ([devices.md](devices.md), "The encoder
  session"). What each encodes and the properties it takes are read once
  VideoToolbox answers for it: at the backend's making, or at the first
  `+enc` that finds a session free, where none was then; until then it is
  listed by its codec alone. Each `+enc` asks for the encoder's properties
  again (about 3 ms, measured on the M3 Max), and answers -12912 where
  VideoToolbox has none, as Apple's host answers `+enc` while every
  session is taken.
- **Encoder sizes** are the largest side of 16384, 8192, 4096, 2048 and
  1920 at which a hardware session opens with the other side 64. They are
  per side, as core's `max_width` and `max_height` are: HEVC encodes
  16384x2048 and 8192x8192, but its session does not open at
  16384x16384 (-12903), which `esta` then answers (measured with a probe
  on the M3 Max, 2026-10-24).

## Decode

A session is a `VTDecompressionSession` on a format description made from
the guest's record as its sample description extension atom (`avcC`,
`hvcC`, `av1C`, `vpcC`), as AVFoundation makes one from an MP4's sample
entry. Each sample is decoded synchronously with no temporal processing,
so its frame is called back before `VTDecompressionSessionDecodeFrame`
returns: frames come in decode order, one a sample, as the guest's
VideoToolbox emits them, and `dfin` has nothing held back. A `dfrm`'s
bytes go as one sample whatever sizes its sample buffer lists, so that it
makes one frame at most ([hardening.md](hardening.md), "The VideoToolbox
backend", F38). Frames are IOSurface-backed `420v`, `420f`, `x420` or
`xf20`, as the record's sequence header says, and are written into the
guest's surface by a pixel transfer where the host lends it ("Zero copy",
below), or else copied row by row: 1.4 ms for a 1080p P010 frame
(vt-check, `VT_CHECK_TIMES`). The pixel buffer is locked for reading
(`CVPixelBufferLockBaseAddress`) as the frame is read back, before core
lends the guest's surface, and unlocked after the copy, so that no
CoreVideo call runs inside the lend ([core.md](core.md), "The copy into
the guest's buffer"). A record that changes is taken by the session where
`VTDecompressionSessionCanAcceptFormatDescription` says so and the pixel
format stays, and by a new session otherwise.

VideoToolbox makes each frame the format description's size, with the
decoded picture at its top left: a description of 1x1 over a 640x360
stream gave 1x1 frames, and one of 1280x720 gave 1280x720 frames. So each
frame is cut to the picture the record gives (`stream_size`: the first
SPS's cropped size, or the largest frame of AV1's sequence header; none
for VP9), and core copies only that into the guest's buffer, as Apple's
host leaves the rest of the buffer as it was ([devices.md](devices.md),
"A format description of another size").

VP9's decoder is supplemental: VideoToolbox opens no VP9 session, and
says it decodes none, until `VTRegisterSupplementalVideoDecoderIfAvailable`
has asked for it (-12906 before; `VTUtilities.h`). The backend registers
it once a process, before its first listing or session. There is no
software VP9 decoder.

**What it refuses:**
- a stream of any codec under 64 pixels a side: core leaves the size to
  the backend ([devices.md](devices.md), "Where we differ from Apple"),
  and the M3 Max's hardware sessions refuse it themselves with -12911, as
  on Apple's host. Measured on the M3 Max (2026-10-24) through core's host
  session, synchronously and not: every stream with a side under 64
  refused at `dsta` with -12911, H.264 and HEVC from 16x16 and VP9 and AV1
  from 2x2, at 8 and 10 bits, and 64x64, 128x128 and 144x144 decoded,
  every frame equal to ffmpeg's;
- a VP9 stream of 12 bits: core answers `dsta` with -12911 before asking
  the backend, as Apple's decoder does ([devices.md](devices.md), "VP9");
- a VP9 frame not of the description's size: VideoToolbox's decoder
  answers it -12909 itself, and core emits it so, with no image, as
  Apple's host does: a VP9 stream of 60 frames at 1280x720 and 180 at
  960x540 gave the first 60 through the session, each equal to ffmpeg's,
  and the 180 with -12909, synchronously and not (measured on the M3 Max,
  2026-10-25). On the other backends such a frame is shown
  ([devices.md](devices.md), "Where we differ from Apple");
- every status a VideoToolbox, CoreMedia or CoreVideo call answers goes
  to the guest as it is, as core's `BackendError::Native`, as Apple's host
  sends its decoder's own (`status.rs`): -17694, a missing reference,
  reaches the guest as -17694, not as -12909;
- but a leading picture the decoder drops, -17694 with
  kVTDecodeInfo_SkippedLeadingFrameDropped in its callback, goes to core
  as `SkippedLeadingPicture` (`output_error`, `status.rs`), so it is
  emitted with Apple's host's `,inf` 0xa and its reply after the emit
  ([devices.md](devices.md), "HEVC"): a BLA stream's RASL picture and a
  seek's seven did so on the M3 Max, synchronously and asynchronously
  (2026-10-24).

## Zero copy

[core.md](core.md), "Zero copy", has the seam every backend writes
through; what it leaves to the CPU's copy is [bugs.md](bugs.md), D22.
Where the host lends a held guest buffer as its IOSurface (core's
`DeviceMemory::io_surface`), `Frame::write_to_guest` writes the decoded
frame into it with no copy by the CPU (`zero_copy.rs`): a pixel buffer
over the IOSurface at the guest's planes
(`CVPixelBufferCreateWithIOSurface`), and one `VTPixelTransferSession` a
decoder, made at its first write with no properties, which transfers the
frame into it. The pixel buffer is released before the write returns:
the host's mapper answers the guest's unmap Busy while anything but it
retains the IOSurface, so a pixel buffer kept per mapping would be a hold
the guest sees ([core.md](core.md), "For VideoToolbox").
The conformance suite's `--no-zero-copy` keeps every frame on the CPU's
copy, as on the other backends ([core.md](core.md), "Zero copy").

**A surface read-only for its clients never reaches Apple's media
hardware.** On the M3 Max (macOS 27.0.1), a process of ours gave Apple's
scaler an IOSurface over pages read-only for its clients as its
destination; the scaler wrote into it, and the kernel panicked:
`dart-scaler0 ... SID 0 write protect exception on write of DVA
0x100000003c0` (twice, 2026-10-24; [devices.md](devices.md), "Surfaces
read-only for their clients"). The guest decides which of its buffers are
read-only, and a pixel transfer may run on that scaler, so the rule holds
on every path of the backend:
- zero copy is the one path that hands VideoToolbox a guest's surface.
  Elsewhere the CPU copies between the guest's surfaces and buffers of the
  backend's own: a decoded frame's copy, the encoders' sources, and
  motion estimation's frames;
- reims-vgpu lends no device a protected surface, or one the guest made
  read-only for its clients (`crates/reims-vgpu-metal/src/os/device_lend.rs`);
- the panic is not reproduced on purpose: the tests give the hardware
  ordinary IOSurfaces of their own memory alone ("The tests", below).

**Nothing of the buffer reaches CoreVideo or VideoToolbox before three
checks of the backend's own pass** ([hardening.md](hardening.md), "Zero
copy on VideoToolbox"):
1. the host's word that it would write the surface itself
   (`LentIoSurface::writable`), read before the surface is; reims-vgpu
   asks its record of the surface again for each write;
2. a base address, which a protected surface lacks;
3. every page of the process's own mapping of the buffer, from the
   surface's base address over the buffer's length, writable
   (`mach_vm_region`).

Then the frame and the IOSurface must be one pixel format (no 420v to
420f), the picture the buffer's whole size, its sides even, and the
IOSurface's planes and the pixel buffer's those core checked against the
memory (`GuestTarget`). What fails is declined, with why, and core copies
that frame on the CPU. Declined so:
- **an odd size**: a transfer of 641x361 420v wrote the last luma column
  as the one before it, and the last row too, 1001 luma samples and 1002
  chroma bytes unlike the CPU's copy (measured on the M3 Max, 2026-10-25;
  `a_transfer_writes_what_the_cpu_copies` now checks that such a frame is
  declined);
- a frame cut to the buffer, a buffer larger than the picture, or one of
  another format, which a transfer would scale or convert.

**The tests** (`zero_copy/tests.rs`). The refusals are tested with
stand-ins that count every call that would reach CoreVideo or
VideoToolbox and fail the test if one does: a buffer the host would not
write, a protected one (no base address), one whose pages are read-only,
wholly or in only their last 16 KiB page (`mach_vm_protect` of the test's
own pages), and one not of the frame's format, size or layout. None
reached a pixel buffer or a transfer. Then, on the M3 Max's hardware
(2026-10-25), **into ordinary IOSurfaces alone, made by `IOSurfaceCreate`
of their own memory with a plane list as a guest lays its buffer out,
never over client memory**: a transfer of a drawn frame gave every sample
equal to the CPU's copy, and every byte outside the samples as it was,
for 420v at 64x48 and 3840x2160, 420f at 1920x1080, 420v at 1366x766,
x420 at 1280x720 and 3840x2160, and xf20 at 642x362; 641x361 and 643x363
were declined. The IOSurface's retain count was the same after each write
as before.

**What it costs** (`how_long_a_write_takes`, ignored, release build,
2026-10-25):

| frame | the CPU's copy | the transfer | a transfer into a kept pixel buffer |
|---|---|---|---|
| 420v 1920x1080 | 0.17 ms, 0.15 ms of CPU | 1.62 ms, 0.19 ms | 1.62 ms, 0.17 ms |
| x420 1920x1080 | 0.19 ms, 0.19 ms | 1.63 ms, 0.19 ms | 1.65 ms, 0.19 ms |
| 420v 3840x2160 | 0.40 ms, 0.40 ms | 3.12 ms, 0.26 ms | 3.06 ms, 0.23 ms |
| x420 3840x2160 | 1.70 ms, 1.42 ms | 3.13 ms, 0.28 ms | 3.17 ms, 0.31 ms |

So the transfer takes longer than the copy, and saves the writing
thread's CPU only at 4K 10-bit, about 1.1 ms a frame; a pixel buffer kept
from frame to frame would save nothing. Through the stand-in guest
(`conformance hardware decode-check`, `RVGV_LOG=info`): HEVC Main 10 at
1080p wrote each frame in 1.89 ms against the CPU's 0.38 to 0.42 ms, at
133 to 148 fps against 149 to 152; AV1 and VP9 at 2160p 10-bit in 3.9 to
4.2 ms against 1.7 to 2.0 ms. It stays on at every size, as core's rule
has it for every backend: where a device can write the guest's buffer
itself, it does ([core.md](core.md), "Zero copy"). Whether VideoToolbox's
should stay on below 4K is open ([bugs.md](bugs.md), D22).

**Real guests' buffers are written so, unmeasured.** reims-vgpu lends a
device a writable IOSurface over the guest's pages: `CLIENT_MEMORY_LENT`
is true in `crates/reims-vgpu-metal/src/os/device_lend.rs`
([core.md](core.md), "For VideoToolbox"). No transfer into an IOSurface
over client memory has run on the M3 Max's hardware, and no guest has run
on a macOS host, so a guest's first frames there are the first writes
through one. A failure then is fixed forward, or `CLIENT_MEMORY_LENT` is
set false, which lends no device such a surface and leaves the guest's
buffers to the CPU's copy. The stand-in guest's buffers are IOSurfaces of
their own memory at a guest's planes
(`crates/reims-vgpu-video-conformance/src/guest_surface.rs`), which is
what the runs in "Checks" wrote through.

## Encode

A session is a `VTCompressionSession` that requires the hardware encoder.
The guest's properties go to it one to one: core hands them over by their
`kVTCompressionPropertyKey_*` strings, which are VideoToolbox's own. A
key the encoder's supported property dictionary does not list, or a value
outside the list it gives, is refused with -12900 before it is applied
(`check_property`); a change sets only what changed and clears what was
removed. `MultiPassStorage`, which core adds, never reaches the session.

Source frames are copied into buffers of the session's pool, or, for a
frame of another format (P010 on an HEVC Main session), into an IOSurface
buffer of its own, which VideoToolbox converts. Samples come back on
VideoToolbox's thread, each with its sync and dependency flags from its
attachments and its times as VideoToolbox gives them, and the
configuration record is the format description's `avcC` or `hvcC`. With
reordering, a B picture's DTS is up to two frames after its PTS, since
each sample's DTS is the PTS of the frame sent in its place, as Apple's
host gives the guest its encoder's ([devices.md](devices.md), "Decode
times"). Where the guest turned frame reordering off, an encode waits up
to 100 ms for its own frame, so that each frame is emitted with its
`efrm`; a session whose encoder holds a frame past that stops waiting
(`OWN_FRAME_WAIT`).

The key frames are AVE's, as on Apple's host: the session gets the
guest's key frame intervals as properties and its forced key frames as
`kVTEncodeFrameOptionKey_ForceKeyFrame`, and places the rest itself. The
codec crate's `KeyFrames`, which places the other backends' and adds one
2^30 frames after an IDR picture at the latest ([vaapi.md](vaapi.md),
"Shape"), takes no part.

## JPEG

None: the backend's `Backend` has no JPEG decoder, so the guest is told
the host has no JPEG engine and decodes JPEG itself. VideoToolbox does
decode JPEG in hardware on the M3 Max
(`VTIsHardwareDecodeSupported('jpeg')` is 1), so a JPEG engine on it is
possible; none is built.

## ProRes

Not offered, and a gap on macOS hosts alone. Apple's host lists a ProRes
decoder to its guest ([devices.md](devices.md), "What an entry holds":
six decoders, ProRes among them), and the M3 Max decodes every ProRes in
hardware (`VTIsHardwareDecodeSupported` is 1 for `apcn`, `apch`, `apcs`,
`ap4h` and `aprn`) and lists hardware ProRes encoders (`AppleProResHW
422` to `4444 XQ`). VA-API and NVDEC have none, so it would be this
backend's only. It needs core to have a ProRes codec and buffer formats
past 4:2:0, which it has not: ProRes is 4:2:2 and 4:4:4 at 10 bits and
more.

## Motion estimation

`VideoToolboxMotionBackend` is core's motion part on
`VTMotionEstimationSession`, which VideoToolbox has from macOS 26. Its five
calls and two keys are looked up with `dlsym`, so an older macOS gives the
backend no motion part, and their types are checked against the SDK's
headers with the rest ("The declarations"). The output handler is a block,
made by hand to the blocks ABI: a stack block whose copy and dispose
helpers hold a reference of their own to where the output goes.

On the M3 Max it is Apple's own processor, the one Apple's host forwards a
guest to, so the backend hands core the session's vectors as they come:
the same grid, unit and sign ([devices.md](devices.md), "The
motion-estimation session").
- **What it offers.** At start a 64x64 session is made and its source
  attributes read. Of their formats core knows `420v`, `420f`, `x420` and
  `xf20`, NV12 and P010 in both ranges, in that order. The largest size is
  Apple's, 16384 a side (core's `motion::MAX_SIDE`, core holding the area
  to 16384x8192).
- **A session** is made at each estimator's open: `MotionVectorSize` the
  guest's block size, 4 or 16, and `UseMultiPassSearch` where the guest
  asked for true motion.
- **A frame pair** is copied from the guest's planes into two
  IOSurface-backed pixel buffers of the backend's own, kept while the
  format stays. They are ordinary buffers, never the guest's memory
  ("Zero copy": a surface read-only for its clients never reaches Apple's
  media hardware). Then `EstimateMotionVectors` with flags 0, and
  `CompleteFrames`, which returns once the handler has run. A reference
  and a current frame of two formats are refused before VideoToolbox sees
  them.
- **The vectors** are read from the session's `2C0h` buffer: the
  picture's ceil(W/16) by ceil(H/16) blocks of 16, or four times as many a
  side at 4, left of and above the grid's padding, which core puts back.

Measured on the M3 Max (2026-10-24): `conformance hardware motion-check`
passed 8 of 8 (below), each pair in 13 to 20 ms, as did the crate's
tests of (+8, +4) at 16 and (3, -2) at 4. The vectors are Apple's
processor's, in its steps: whole pixels at 4, multiples of 4 at 16, so
(12, -7) came back as (12, -8) in most blocks and (-7, 9) at 641x361 in
75% of them within 2 pixels ([devices.md](devices.md), "Its precision").

| case | share of cells the way back | commonest |
|---|---|---|
| b16-256x128, (8, 4) | 95.3% within 2 | (-8, -4) x122 |
| b16-256x128, (-12, 7) | 84.4% within 2 | (12, -8) x108 |
| b4-256x128, (3, -2) | 96.3% | (-3, 2) |
| b4-256x128, (-5, 7) | 92.3% | (5, -7) |
| b16-641x361, (7, -9) | 75.3% within 2 | (-8, 8) |
| b4-641x361, (-4, 3) | 96.8% | (4, -3) |
| b16-1280x720, (20, -12) | 96.6% within 2 | (-20, 12) |
| b16-128x48, (4, 3) | 100% within 2 | (-4, -4) |

## Checks

```sh
crates/reims-vgpu-video-videotoolbox/tools/videotoolbox-check.sh [stream dir...]
```

It runs the crate's tests (`cargo nextest`), the conformance suite's
decode check of every MP4 with a reference beside it, synchronously and
asynchronously, through core's host session as a guest drives it,
holding the Mac's GPU lock once they are built
(`scripts/internal/gpu-locks.sh`). A decode check passes where the suite exits 0
having printed a PASS line and no FAIL line; a stream with no reference
is named in a NOTE line. The streams and their references are made
outside this repository and never committed; with no stream dir given it
reads `build/streams`, `build/av1-streams` and `build/vp9-guest` under the
repository root, each named in a NOTE line where it is missing, while a
dir named on the command line that is missing is a FAIL. The references are a software VideoToolbox
decode (`NAME.mac-sw.md5`), a libdav1d decode (`NAME.md5`), ffmpeg's VP9
decode packed into NV12 or P010 by the conformance suite's `planar-packed`
(`NAME.vp9.guest`, as `vp9-check.sh` writes it: exact at every size, the
odd ones too), or ffmpeg's decode or the encoder's reconstruction
(`NAME.expect`). A stream `hevc-check.sh` crafted (`NAME.craft` beside it
names its source and hevc-craft's edits) is crafted again from its source
before the lock, and goes to the media hardware only where that gives
`NAME.265`'s bytes and the codec crate's `--check` passes on them, since
invalid streams panicked this Mac's media hardware twice; its MP4 is
ffmpeg's mux of that stream ([vaapi.md](vaapi.md), "Crafted HEVC streams"). The encoders' conformance cases are
`conformance hardware encode-check`, which the script does not run
([backends.md](backends.md), "The conformance suite").

**The run of 2026-10-24**, over the three default stream dirs:
`result: pass`, 53 checks, the backend's scaler's among them, which it
has had no longer since:
- the tests, 27;
- H.264 High with B-frames at 1080p and Main at 720p, HEVC Main and
  Main 10 with B-frames at 1080p: every frame bit-exact with the software
  decoder's, both ways;
- AV1, nine streams (libaom tiles, 10-bit, padded both ways, superres;
  rav1e S-frames; SVT 2160p 10-bit, film grain): every frame bit-exact
  with libdav1d's, film grain included;
- VP9, profile 0 at 720p, 1080p with tiles, 641x361, alt-refs, every
  segment feature, intra-only frames; profile 2 at 720p and 2160p, shown
  existing frames, and 643x363: every frame bit-exact with ffmpeg's (and
  with the VA-API backend's at 643x363); two resized streams: every frame
  of the first size bit-exact, the rest refused; 63x65 refused at `dsta`
  with -12911 by the hardware decoder;
- the scaler: 123 of 123 cases, the two nearest-filter scales refused.

**The runs of 2026-10-25**, with zero copy on and with it off (the
check's `RVGV_ZERO_COPY=off`), over the same three directories:
`result: pass` both ways, 65 checks each, the outputs equal but for the
tests' time. With zero copy on and `RVGV_LOG=info`, the device wrote
3690 frames, every frame with an image of every stream but the two of
odd size, whose 120 were declined and copied by the CPU. The backend's
scaler, since removed, passed its 132 cases, 7 refused. Run again the same day with each VP9 stream's
reference packed by `planar-packed`, and the stand-in opening its sessions
with `decs`'s IDs: `result: pass`, the same 65 checks, p2-643x363's 30
frames against `planar-packed`'s P010, which equalled the VA-API backend's.

**The HEVC runs of 2026-10-25** (macOS 27.0.1), over the directory
`hevc-check.sh` made on a Linux host: before hevc-craft, its 29 streams
with references, `result: pass`, 60 checks (the tests, the backend's
scaler, since removed, and each stream both ways); with hevc-craft, `result: pass`, 92 checks, the 60
with the same verdicts and 32 more: main-720p-ref1, kvz-1080p-ref1 and the
fourteen crafted streams ([vaapi.md](vaapi.md), "Crafted HEVC streams")
both ways, every frame their source's reference's. Each crafted stream
came out of hevc-craft here the same bytes, its `--check` passing, before
the hardware saw it. The refusals' streams (Main 4:2:2 10, Main 4:4:4, Main
12) carry no reference and are named in NOTE lines.

The decode check's frame rates (6 to 21 fps) are the stand-in guest's,
which hashes every frame; the decoder's own time is not measured there.

**The tests** (`cargo nextest run -p reims-vgpu-video-videotoolbox`, part
of `cargo xtask check` on a Mac and of `tools/videotoolbox-check.sh`) draw
their pictures, encode them on the hardware encoders and decode them back:
- H.264 High, HEVC Main, HEVC Main 10 from P010, and P010 on an HEVC Main
  session: one sample a frame, a sync first, 4-byte NAL units with no
  parameter sets in the samples, a record that parses, every picture's
  luma and chroma above 30 dB;
- the hardware and software decoders give the same bytes for H.264 and
  HEVC;
- a forced key frame is a sync sample, and every sample's sync flag
  matches its picture's NAL type;
- 200 kbit/s of `AverageBitRate`, which the picture exceeds at the
  encoder's own choice, lands within 0.75 to 1.3 of it over a second;
- keys and values the encoder does not list are refused with -12900;
- cut, overlong, noisy and empty samples are bad data or a frame without
  an image, never a malfunction, and the decoder goes on;
- the whole backend has every part but JPEG and the scaler.

The HEVC encoder adds IDR pictures of its own when other sessions share
it: three of four parallel runs had one at frames 1 to 4. The sync test
checks flags against the bitstream for that reason.

The Mac's 32 hardware encoder sessions are every process's: 32 of 40
asked opened, H.264 at 1080p and HEVC at 2160p alike, and the 33rd
answered -12903; with one process holding 20, a second opened 12
(measured with a probe on the M3 Max, 2026-10-24; [bugs.md](bugs.md),
B31). The backend sets no cap of its own. Other processes may hold
sessions while the tests run, so the tests open each encoder session, and
make each encoder backend, once the Mac has one to give, waiting up to a
minute (`when_encoders_are_free`). One test takes all 32, to check that a
backend made while every session is taken lists both encoders by their
codecs, answers `+enc` with -12912, and reads each once the sessions are
let go; it is ignored for that reason. Run it alone, holding the Mac's GPU
lock as the check does, while nothing else on the Mac encodes:
`scripts/internal/gpu-locks.sh -- cargo nextest run -p reims-vgpu-video-videotoolbox --lib --run-ignored only -E 'test(every_encoder_session)'`.

## Where it differs from Apple's host

- **Error codes** are VideoToolbox's own, passed through as Apple's host
  passes its codec's (`status.rs`), but they come from the public session
  API a layer above the codec Apple's host calls. For a size AVE does not
  take, Apple's host answers `esta` with -19354 from
  `VTVideoEncoderStartSession` ([devices.md](devices.md), "The encoder
  session"); core answers the same for a size past the listed maximum,
  and a size under it that `VTCompressionSessionCreate` refuses reaches
  the guest as its status, -12903 for HEVC at 16384x16384, where Apple's
  would be -19354.
- **No JPEG engine** and **no ProRes**, which Apple's host offers.
- **No 4:2:2 or 4:4:4 decode**, which Apple's host offers where the
  hardware has it.
