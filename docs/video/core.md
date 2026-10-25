# Core

`crates/reims-vgpu-video-core` is the host side of the VideoToolbox
device's protocol, generic over the host's backends. The guest's contract
is [devices.md](devices.md); this file is how the crate meets it, what it
lacks, and what was measured of it. The M2 scaler device's requests, in
the same crate (`src/scaler.rs`), are [scaler.md](scaler.md)'s.

## State

What the crate has:

- **The wire format** (`src/wire/`): the `vtpv` header and `para`
  parameters with Apple's scan rules, every value type, fragments, binary
  plists, the ImageDescription with its codec record (`avcC`, `hvcC`,
  `av1C`, `vpcC`), the `sbuf` sample buffer serialization with its 212
  known keys, and the `subs` and `rels` boxes.
- **The host session** (`src/host.rs`): raw queue-0 bytes in, the status
  out; `decs` and `dcap` from the backend, `encs` from the encoder
  backend, `meps`, and the JPEG engine's `jcap`, `jdec` and `jenc` on the
  backend's JPEG decoder (`src/host/jpeg.rs`, `src/jpeg.rs`); `-dec`,
  `-enc`, `-mep`, `-uui`, `-all`; routing by UUID; surface holds and the
  `rels` sweep; invalidate and complete-invalidate. The decoder, encoder
  and motion-estimation sessions share their create reply, their queue
  and thread, the reply clerk's waits and their two-phase end
  (`src/host/session.rs`, `src/host/session_thread.rs`).
- **Decoder sessions** (`src/host/decoder.rs`, `src/host/reply_clerk.rs`):
  `+dec` for a codec the backend lists opens one, on a thread of its own;
  `dsta`, `dfrm` (fragments, sync and async), `dfin`, `dinv`, `dcan`, the
  properties and the tile messages; `Dpba`, `Dbuf`, `Demt`, `Dfub` and
  the sweep, in the order measured with Apple's AVC decoder.
- **Encoder sessions** (`src/host/encoder.rs`,
  `src/host/encoder/properties.rs`, `src/encoder_backend.rs`): `+enc` for
  a codec the encoder backend lists opens one, on a thread of its own; the
  properties in AVE's shape, `esta` with `Epba`, `efrm` read from the
  guest's surface, `Efds`, `Eemt` in fragments, `ecfr`, `einv`, and
  -12782 for multipass, tiles and multi-image frames, in the order
  measured with Apple's software encoders ([devices.md](devices.md), "The
  encoder session"). Tested against a fake guest and a stand-in encoder
  (`src/host/encoder/tests.rs`). `MultiPassStorage` answers as Apple's
  host does for an encoder of one pass, `mpin` included
  ([devices.md](devices.md), "Multi-pass").
- **Motion-estimation sessions** (`src/host/motion_estimation.rs`,
  `src/motion.rs`): `meps` lists our processor where the backend has a
  motion part, and `+mep` choosing it opens a session on a thread of its
  own, else Apple's not-found status. `mest` with `Mspa` and `Mvpb`,
  `mepf` with `Mbuf`, `Memt` and `Mdpb` in the order measured with
  Apple's processor, the properties, `mcfr`, `msav`, `minv`. The
  backend's field is resampled to Apple's grid and written as `2C0h` half
  floats ([devices.md](devices.md), "This host's motion estimation").
  Tested against a fake guest and a stand-in estimator
  (`src/host/motion_estimation/tests.rs`), and fuzzed (`motion-sessions`,
  [hardening.md](hardening.md)).
- **The backends** are crates of their own on core's traits: VA-API's,
  NVIDIA's and VideoToolbox's ([backends.md](backends.md)). A stand-in
  guest driver decodes the test streams through `HostSession` on
  `VaapiBackend`, and every frame matches its software reference
  ([vaapi.md](vaapi.md), "Through the host session").

What it lacks: `EnableTemporalProcessing`'s display order, and tiles
beyond answering -12782. For encoding: multipass, tiles, multi-image
frames, and `,pat` on a source frame, which is read and not applied.

## The API, for the device and the backend

In reims-vgpu the host session is reached through the video device,
`VideoDevice` in
[crates/reims-vgpu/src/video.rs](../../crates/reims-vgpu/src/video.rs):
at the guest's DRIVER_OK it makes a `HostSession::with_backend` on the
platform as its `Runtime` (`crates/reims-vgpu/src/video/runtime.rs`), over
`MapperSurfaces`, its `Surfaces` through the IOSurface device's mapper,
and `CallbackSink`, its `GuestSink`, the host's `send_to_guest`; it hands
each queue-0 message to `deliver`, and a reset runs `invalidate` and
`complete_invalidate`. QEMU's `apple-videotoolbox-pci`
(`vendor/qemu/hw/display/apple-videotoolbox-pci.c`) drives it through the
C API (`crates/reims-vgpu-c/src/video.rs`).

- `HostSession::new(runtime, sink, surfaces, backend)`, or
  `HostSession::with_encoders(runtime, sink, surfaces, backend, encoders)`
  to encode too, or `HostSession::with_backend(runtime, sink, surfaces,
  &Backend)` for every part a `Backend` has (its decoders, encoders,
  motion estimation and JPEG decoder, each optional); `deliver(&[u8]) -> i32`
  takes the queue-0 bytes as they came and reads `subs` itself;
  `deliver_from_guest(&[u8], &[MappingId])` takes IDs the caller read;
  `invalidate()`, `complete_invalidate()`; drop runs both.
- `Runtime` (`src/runtime.rs`): the host's threads and clock, which the
  session's threads, its bounded waits and its timings come from:
  `spawn(name, body)`, `now()`, and `wait_until(condvar, guard,
  deadline)`, one timed wait on a `Condvar` until a deadline on the clock.
  reims-vgpu's is its platform: each thread is a worker of the platform's
  `Threads`, marked as the library's, so that a panic on it ends the
  process as on every library thread, and the clock is the platform's, a
  test's virtual one included. `StdRuntime` is std's threads and a
  monotonic clock, for the stand-in guests, the examples, the fuzzers and
  the tests. `Bell` and `answer()` are the waits built on it: a bound
  counts on the runtime's clock.
- Every lock is parking_lot's (`src/sync.rs`), as in the library's other
  crates: none is poisoned, so a panicking thread's lock is the next
  locker's as it was.
- `GuestSink::send(Vec<u8>)`: one whole message, in order; it must queue
  and return, never wait, and never deliver back on the same thread. Sends
  happen under the session's invalidation lock, so none starts after
  `invalidate` returns.
- `Surfaces::retain(MappingId) -> Box<dyn GuestSurface>`, released on
  drop; `GuestSurface::layout()` and `write(&mut dyn FnMut(&mut [u8]))`,
  the whole allocation with each plane at its offset. Over reims-vgpu's
  `MappedSurface` that is its bytes from its base address to its
  allocation's end, lent through the platform's `lend_surface_bytes`.
  `GuestSurface::read(&mut dyn FnMut(&[u8]))` is the same for reading,
  which the encoders, the motion-estimation sessions, the JPEG engine and
  the scaler use: its default refuses, so a host that offers none of them
  need not give it. `SurfaceLayout::split_for_reading` cuts the
  allocation into `PlaneSource`s.
- `DecoderBackend::{capabilities, open}`, `BackendDecoder::{output_format,
  decode, flush, can_accept, decodes_while_copying, frames_decoded}`,
  `DecodedFrame::{frame_id, pixel_format, width, height, read_back, wait,
  write_to_guest}`,
  and `FrameReadBack::copy_to`, the CPU's copy out of what `read_back`
  left in host memory ("The copy into the guest's buffer", below);
  `backend::copy_biplanar` does the row copy, and `backend::read_back_by`
  makes a read back of a frame already in host memory.
  `decodes_while_copying` (default no) and `wait` (default: nothing to
  wait for) are for "Decoding ahead", `frames_decoded` for "AV1's
  buffers" and `write_to_guest` for "Zero copy", below.
  `decode` returns a frame's picture at once: frames come out in decode
  order, as VideoToolbox's decoders emit them, and a frame with none is
  emitted as dropped. `ProfileSupport::max_level` is `None` where the
  backend knows no level.
- `EncoderBackend::{encoders, open}`, `BackendEncoder::{configuration,
  encode, flush, reconfigure}` (`src/encoder_backend.rs`). `encode` gets a
  `SourceFrame` whose planes borrow the session's copy of the guest's
  picture, its rows packed, with no lend of the surface held, and returns
  the frames ready, in decode order: none, this one, or ones it held. An
  `Err` is a frame not taken, which fails `efrm` and is never emitted.
  Each frame taken comes back once: with a sample, with no data as
  dropped, or with `error` set and no data, failed, which is emitted with
  that error's status alone, so an error loses no other frame
  (`EncodedFrame::{dropped, failed}`).
  `EncodedFrame::data` is 4-byte length-prefixed NAL units without
  parameter sets, which go in `configuration()`, the `avcC` or `hvcC`.
  The codec crate's encoders turn an Annex B coded buffer (VA-API's,
  NVENC's) into that (`encode::nal`). `NoEncoders` lists none. The trait is
  backend-neutral: `EncoderSettings` has the guest's properties mapped
  into fields and, in `properties`, as the guest set them, for a
  VideoToolbox backend to set as they are;
  `EncoderBackend::check_property` refuses one with the status the guest
  gets, and a running encoder's `reconfigure` refusing one gives -12900.
  The property keeps its old value either way.
  `EncoderBackend::session_capabilities` is what a `+enc` gets, and
  `BackendEncoder::prepare` runs at the guest's `epfr`; both have
  defaults. Core re-exports `plist`.
- `MotionEstimationBackend::{capabilities, open}`,
  `MotionEstimator::estimate(reference, current) -> MotionField`
  (`src/motion.rs`): the field is the backend's own blocks, in pixels with
  Apple's sign; `motion::resample` makes Apple's grid of it, and
  `half_float_pair` its bytes ([backends.md](backends.md), "The traits").
- Each backend crate opens its whole `Backend` at its root
  (`open_backend` for VA-API and NVIDIA, `backend` for VideoToolbox;
  [backends.md](backends.md), "The interface each backend crate
  exports"). Each depends on core, which stays free of libva, CUDA and
  Apple's frameworks.

## Decoding ahead

A decoder session handles its messages one at a time
(`src/host/decoder.rs`). For a frame decoded on a GPU, the copy back waits
for the whole decode, so, one frame at a time, the GPU sits idle through
each copy and each round trip to the guest. An asynchronous frame's
`dfrm` reply goes as soon as its buffer is lent, before the copy, and the
guest then sends its next `dfrm` at once. So, for an asynchronous frame of
a decoder whose `decodes_while_copying` says yes:
1. The session copies the frame on a thread of its own, the runtime's
   (`reims-vgpu-video.decoder-copy`, one for each copy), which first waits
   for the frame to be whole on its device (`DecodedFrame::wait`), then
   reads it back and copies it ("The copy into the guest's buffer").
2. Meanwhile the session's thread takes its queue. Once the frame is
   whole, the first message, if it is a whole `dfrm` in one message, is
   read and handed to the backend's `decode`.
3. Everything else waits for its turn: that `dfrm`'s configuration change,
   whether the guest gave up on it, its `Dbuf`, reply, copy and `Demt`, and
   every other message, in the order they came.

What the guest sees is what it sees one frame at a time:
- every message, reply and emission, and their order: the next frame's
  `Dbuf` still follows this frame's `Demt`, `Dfub` and `rels`;
- the holds on surfaces: a message taken early keeps its lent surfaces and
  its share of what the host session holds until its turn;
- the given-up count: a `dfrm` taken early counts as queued until its
  turn, so a frame the guest gave up on is answered as one frame at a
  time.

Nothing goes ahead for a synchronous frame, for `dfin`'s flushed frames,
after invalidation, or behind a message that is not a whole `dfrm`. A
fragmented `dfrm` waits too, and so does one whose format description
changes the decoder's configuration, as a backend may reopen its session
for it. A reconfiguration inside the stream, which only the backend sees,
is why the next `decode` waits for `wait`: VA-API's decoders then make a
new context, and the frame being copied must be whole first. The frame
being copied still holds its surface and its pool.

Which backends: VA-API's four decoders say yes. Mesa's VA entry points
take the display's lock, and `vaSyncSurface` holds it while it waits for
a processing run (**read** in Mesa 26.2.4; [bugs.md](bugs.md), U17), so
the copy waits for its runs by `poll` on the dma-bufs they write first,
with no lock held ([vaapi.md](vaapi.md), "The driver's lock"). The checks
with calls on both threads, every frame bit-exact on both GPUs
([vaapi.md](vaapi.md), "The copy back"), are the evidence that the two
threads can share a display. NVIDIA's say no: every
call on an NVDEC decoder's handle runs under one lock, its copy included,
so the next `decode` would wait for the copy; and a VP9 resize
reconfigures the decoder, which would race a frame's map
([bugs.md](bugs.md), D17). VideoToolbox's say no.

**The proof.** Core's session tests, with a backend whose frames' waits
and copies a test holds:
- an asynchronous frame's next `dfrm` decodes after its wait and before
  its copy is over;
- a synchronous frame is copied before the next decodes;
- a message before the next `dfrm` keeps both in their turn;
- a close during a copy ends the session;
- `decoding_ahead_leaves_what_the_guest_sees_as_it_was`: nine frames over
  every path a frame takes (a picture, none, one that does not decode, one
  not to be output, synchronous and asynchronous), each copy 20 ms, give
  the same messages and emissions with and without decoding ahead.

And the stand-in guest's record of the host's messages (`--sequence`,
which `crates/reims-vgpu-video-vaapi/tools/perf-check.sh` writes for each
run): its five streams, sync and async, on the RX 7900 XTX (Navi 31) and
the Radeon 780M (Phoenix), radeonsi with Mesa 26.2.4, gave the same 605
lines decoding ahead as one frame at a time, compared with `cmp`
(2026-10-24). Two builds of the host session are compared so wherever a
change must not show to the guest.

**Measured**, the same runs: fps through the host session, one frame at
a time and then decoding ahead, every frame checked as a set; the
session's log counts 149 of 150 frames decoded ahead in each asynchronous
run, and none in a synchronous one.

| stream | Navi 31, async | Navi 31, sync | Phoenix, async | Phoenix, sync |
|---|---|---|---|---|
| 4K HEVC Main | 118.6 to 139.1 | 118.4 to 117.0 | 118.8 to 144.5 | 129.1 to 113.4 |
| 4K HEVC Main 10 | 53.5 to 99.8 | 53.6 to 53.9 | 86.9 to 100.9 | 88.5 to 86.7 |
| 4K AV1 | 135.0 to 142.9 | 126.5 to 128.9 | 146.3 to 165.4 | 144.0 to 143.8 |
| 4K AV1 10-bit | 56.1 to 83.4 | 59.5 to 60.1 | 101.5 to 101.5 | 101.9 to 100.9 |
| 1080p H.264 High | 252.1 to 281.8 | 294.0 to 291.5 | 366.0 to 432.7 | 358.4 to 350.5 |

- In the asynchronous runs, the backend's `decode` call rose from 0.3 to
  0.9 ms to 1.2 to 5.3 ms: the next decode waited in the driver while the
  copy's thread waited for its processing run in `vaSyncSurface`, which
  holds Mesa's display lock ([bugs.md](bugs.md), U17). Since the copy
  waits for its run outside that lock (2026-10-25), `decode` takes 0.3 to
  0.9 ms decoding ahead, and the asynchronous 4K rates rose 8% to 59% with
  the CPU copying ([vaapi.md](vaapi.md), "The driver's lock").
- Phoenix's 10-bit 4K stops near 100 fps with or without it, where the
  `Demt` round trip grew to 4.2 ms: the stand-in guest answers a `Demt`
  once its hashing threads take the frame, and four of them hash about 100
  frames of 24.9 MB a second.

## The copy into the guest's buffer

A decoded frame reaches the guest's buffer in two steps
(`src/host/decoder.rs`, `copy_frame`):
1. `DecodedFrame::read_back` makes every GPU and driver call the copy
   needs, and leaves the frame where the CPU can read it: VA-API's wait,
   processing run and sync, or its image and map ([vaapi.md](vaapi.md),
   "The copy back"); NVDEC's map, two `cuMemcpy2DAsync` into page-locked
   staging memory and unmap ([nvidia.md](nvidia.md)); CoreVideo's lock of
   the pixel buffer ([videotoolbox.md](videotoolbox.md)).
2. Inside the guest buffer's write lend (`GuestSurface::write`),
   `FrameReadBack::copy_to` copies the rows with the CPU alone. The read
   back is dropped after the lend, which unmaps or unlocks what it holds.

So no backend runs inside a lend. reims-vgpu lends one surface at a time
in the process, and a driver call that never returned inside one would
hold every other session's lends with it. A read back that fails answers
its status as the copy did, before any lend. A frame already in host
memory, as a stand-in's, makes its read back with
`backend::read_back_by`.

Where the guest's buffer lies is reims-vgpu's. Nearly every page of a
guest's buffer is a run of its own. On Linux a surface over several runs
is lent through its view of the guest's pages, a mapping of the guest's
memory reims-vgpu keeps at the surface's base, so the copy writes into the
pages in place (`crates/reims-vgpu-vulkan/src/os/surface.rs`,
`lend_surface_bytes`). The Vulkan platform on macOS lends such a surface
through a copy of all of it, gathered from the pages and scattered back
into them; the Metal platform lends IOSurface's own mapping of the pages,
under its lock (`crates/reims-vgpu-metal/src/os/surface_bytes.rs`).

The session's log splits each copy: `a copy waited W ms for its frame,
read it back in R ms, copied it into the guest's buffer in F ms, and took
L ms more to lend that buffer`, means over the frames copied. L is the
lend's cost past the copy: reims-vgpu's lock, its lookup of the surface's
pages, and a gather and scatter where it lends a copy.

**The proof.** Core's tests: the stand-in frames of the session tests
gate and delay their read back, which runs before the lend, and copy in
the closure; the host fuzzer's stand-in fails a read back in 31 and a copy
in 29. The stand-in guest's record of the host's messages (`--sequence`)
for perf-check.sh's five streams, sync and async, was the same with the
read back outside the lend as with it inside, by `cmp`, on the RX 7900 XTX
and on the RTX 5080 Laptop GPU (driver 615.71.09), 2026-10-24
([nvidia.md](nvidia.md), "How fast a session decodes").

**Measured**, the same runs through the host session with the stand-in
guest, on the RX 7900 XTX (radeonsi, Mesa 26.2.4, 2026-10-24), sync /
async, every frame matching as a set:

| stream | fps | read back, copy, lend |
|---|---|---|
| 4K HEVC Main | 117.1 / 140.1 | 6.82, 1.04, 0.00 / 2.80, 1.19, 0.00 ms |
| 4K HEVC Main 10 | 53.6 / 100.4 | 15.69, 2.01, 0.00 / 5.31, 2.05, 0.00 ms |
| 4K AV1 | 133.1 / 151.1 | 5.61, 1.09, 0.00 / 2.77, 1.07, 0.00 ms |
| 4K AV1 10-bit | 60.0 / 75.7 | 13.18, 2.06, 0.00 / 5.34, 2.04, 0.00 ms |
| 1080p H.264 High | 284.6 / 277.5 | 2.83, 0.20, 0.00 / 1.06, 0.21, 0.00 ms |

On VA-API the split costs nothing beyond the runs' spread, each rate
within 4% of the one with the read back inside the lend: the CPU's copy
is the second half of the read back either way. It made NVIDIA's copy
two, which cost it 10% to 30% of its 4K rate, still above 4K120
([nvidia.md](nvidia.md)); zero copy makes that one GPU copy wherever CUDA
registers the guest's buffer ([bugs.md](bugs.md), D17).

In a guest on the RX 7900 XTX (2026-10-24), the lend of a 4K buffer took
0.04 to 0.26 ms through its view of the guest's pages, against 3.1 to
6.5 ms through a gathered copy, and the guest's copy came to at most
2.5 ms above the stand-in's, and below it in 9 of its 15 runs.

## Zero copy

Where a backend's GPU can write a decoded frame into the guest's buffer
itself, it does, and the CPU copies nothing; where it cannot, the frame
takes the copy above, with a line in the log saying why
([bugs.md](bugs.md), D22, has what is left on the CPU's copy). Zero copy
never makes a hold the guest sees longer: a device writes a buffer only
while the session holds it, in the window the CPU's copy writes it in.

**The seam** (`src/device_memory.rs`, ours):
- `GuestSurface::device_memory` gives a held surface's memory as a device
  reaches it, outside any lend: a `DeviceMemory`, with ranges of its
  pages as a dma-buf (`dma_buf_of`, for VA-API's import, refused with
  `DmaBufError::TooLarge` and the host's count where they are more than
  one dma-buf holds, so that a backend asks for fewer), as one contiguous view in the
  host's address space (`host_view`, for NVIDIA's `cuMemHostRegister`), or
  on macOS as the host's IOSurface over them (`io_surface`, a
  `LentIoSurface` borrowed for the hold's life, with the host's word on
  whether it would write the surface itself, for VideoToolbox's pixel
  transfer), each the pages the surface's lends give. The default is
  `None`, and reims-vgpu gives none for a surface the guest made read-only
  for its clients, or a protected one
  (`crates/reims-vgpu-vulkan/src/os/linux/device_lend.rs` and
  `crates/reims-vgpu-metal/src/os/device_lend.rs`, `lend_to_devices`).
- `MemoryIdentity` names one mapping of one surface: the same for every hold
  on it, never the same for another, from one counter for the process. The
  host makes a `MemoryLife` with each mapping, attached to reims-vgpu's
  surface for as long as its last object lives
  (`crates/reims-vgpu/src/platform/vulkan/lent_memory.rs`), and dropping
  it tells everything kept by that identity. The same pages mapped again
  are another surface with another identity, so an import made for the
  first is never written again.
- `DeviceImports` keeps a backend's imports by identity and a variant (the
  frame's size and format, for VA-API), made once per mapping, the oldest
  dropped past a capacity, and each dropped on a thread of its own, the
  runtime's (`reims-vgpu-video.imports`), once its mapping is gone: never kept
  past it, never dropped on the host's thread, which may hold reims-vgpu's
  locks. An import still in use when its mapping goes lives until that use
  ends. Nothing that retains a surface's IOSurface is kept in one: on
  macOS the mapper answers the guest's unmap Busy while anything but it
  retains the IOSurface (`crates/reims-vgpu-core/src/iosurface/table.rs`,
  `unmap`), so such an import would lengthen a hold the guest sees ("For
  VideoToolbox", below).
- `GuestTarget` is the surface's layout (`SurfaceLayout`, the guest's
  planes, offsets and rows) checked against its memory's length: every
  plane's rows, at its pitch, inside the memory, the planes in order and
  not overlapping, the pixel format one core decodes into. A device is
  told to write only within it, and `written_size` is the frame's crop cut
  to the buffer, as the CPU's copy cuts it.
- `DecodedFrame::write_to_guest(&GuestTarget)` writes the frame's samples
  into those planes, and only those, every driver call made before it
  returns: `DeviceWrite::Written`, or `Declined` with why, and the CPU then
  copies the frame. An `Err` is a frame that could not be read back either,
  and fails as a read back's failure does. The default declines. A write
  the device has not finished by its deadline is an `Err`, and the backend
  keeps the target's hold (`GuestTarget::hold`) until the device is done
  ("Waiting for the device").

**Where it runs** (`src/host/decoder.rs`, `copy_frame` and `ZeroCopy`): in
place of the read back and the copy, while the session holds the buffer,
before its `Demt`, outside any lend, at the point the CPU's copy would have
run. So the guest's buffer is written in the window the copy wrote it in,
by the frame's samples alone, and no driver call runs inside a lend or
under reims-vgpu's locks. A held surface's pages cannot change while the
device writes them: the mapper answers the guest's unmap of a held
surface Busy, and a map of a mapping ID already mapped changes nothing
(`crates/reims-vgpu-core/src/iosurface/device.rs`,
`a_held_surface_keeps_its_pages`).

**The switch.** Zero copy is on, with nothing to set: whether a driver's
writes are trusted is its backend's to decide, per driver (the VA-API
crate's `Display::writes_imports`). `HostSession::set_zero_copy(false)`
has the CPU copy every frame of the sessions opened after it, and the log
says so as each opens: for the checks that compare the two, which take it
as a flag of their own (the conformance suite's `--no-zero-copy`, which
the checks' scripts pass for `RVGV_ZERO_COPY=off`).

**The log.** A session logs each reason it declined, the first time it
comes, at most eight reasons a session: `zero copy: the CPU copies frame
0x… into the guest's buffer, and the session's like it: WHY`. Its end line
adds the split: `N written by the device in D ms each, M declined, and C
copied by the CPU, which read back in R ms and copied in F ms each`.

**Which frames take which way:**

| backend | written by the device | copied by the CPU, and why |
|---|---|---|
| VA-API on radeonsi | every buffer whose rows and planes radeonsi takes: rows a multiple of 256 bytes, planes 256 bytes apart from the dma-buf's start, an even size: 4K and 720p at 8 and 10 bits, 1080p at 10 bits, at udmabuf's stock limits too, in strips of rows with a dma-buf a plane | 1080p 8-bit and every other width whose rows at Apple's 64-byte alignment are not a multiple of 256 (the guest's 1920, 640 and 1408 bytes), odd sizes, a buffer that would take more than 64 strips at the host's udmabuf limits, which none of the decoders' sizes does at the stock ones ([vaapi.md](vaapi.md), "Zero copy") |
| VA-API on another driver | none | its writes are not measured right: NVIDIA's VA-API driver (libva-nvidia-driver 0.0.18, on the RTX 5080 Laptop GPU, 2026-10-25) answered success and wrote nothing ([bugs.md](bugs.md), U16) |
| NVIDIA | every buffer CUDA registers: each frame two `cuMemcpy2DAsync` from the mapped picture into the guest's pages | a registration CUDA refuses, or a copy that fails ([nvidia.md](nvidia.md), "Zero copy") |
| VideoToolbox | every buffer of an even size whose IOSurface the host lends, one format and size with the frame, by a `VTPixelTransferSession`: in the stand-in guest every even-sized frame of every stream ([videotoolbox.md](videotoolbox.md), "Zero copy") | odd sizes, which a transfer writes otherwise; a frame cut to the buffer or smaller than it. Buffers over a guest's pages are lent (`CLIENT_MEMORY_LENT`, below), though no transfer into one has run on the M3 Max yet ([bugs.md](bugs.md), D22) |
| JPEG, the scaler, the encoders | not covered | the JPEG engine's output, the scaler's and the encoders' inputs are copied by the CPU |

On Linux every surface the guest's decoders use is reims-vgpu's over its
RAM, so `device_memory` is `None` only for read-only and protected
surfaces; on the Metal platform too, since reims-vgpu's
`CLIENT_MEMORY_LENT` is true (below). Set false, it is `None` for every
surface over the guest's pages.

**Not taken: the decoder writing straight into the guest's buffer.** That
is Apple's own way (its host decoder decodes into the IOSurface the guest
lends with `Dbuf`, read from Apple's code), and it would save the GPU's
write too. It is not taken because a decode surface is also a reference:
VA-API and NVDEC keep each picture in their DPB for as long as later
pictures predict from it, which is after the guest's `rels` of its
buffer, and core's holds cover a buffer only until then. Decoding into
the guest's buffer would make the host keep that buffer past the guest's
release, a longer hold the guest would see, or let the guest's next use
overwrite a reference, which decodes wrongly. VideoToolbox's public API
gives no per-frame output buffer either (`VTDecompressionSession`'s pool
is its own).

**The proof.** Core's tests (`src/host/decoder/tests.rs`): a device writes
the frames of buffers the host lends (`a_device_writes_frames_into_the_buffers_the_host_lends_it`),
what it declines the CPU copies (`what_no_device_writes_the_cpu_copies`),
a read-only buffer is never lent (`a_read_only_buffer_is_never_lent_to_a_device`),
a layout past its memory is never given to a device
(`a_layout_past_its_memory_is_never_given_to_a_device`), a write that fails
fails its frame (`a_device_write_that_fails_fails_its_frame`), and the
guest's messages, sync and async, are the same with zero copy on and off
(`zero_copy_leaves_what_the_guest_sees_as_it_was`); `src/device_memory/tests.rs`
covers the identities, the imports' lives and the targets. The host
fuzzer's stand-in host lends three in four surfaces to its stand-in
device, which declines one write in five and fails one in 37, and asserts
that no write runs inside a lend and that every byte it writes lies in the
surface (`tests/fuzz/host/standins.rs`). The conformance crate's stand-in
guest lends its buffers as a guest's are: memfd pages, scattered one a run,
viewed in order, and a udmabuf of ranges of them, one piece a page,
refused past udmabuf's limits as the host refuses
(`crates/reims-vgpu-video-conformance/src/guest_pages.rs`), so a backend's
zero copy runs against it as against reims-vgpu.

On the RX 7900 XTX and the Radeon 780M (radeonsi, Mesa 26.2.4,
2026-10-25, udmabuf at its stock limits) every decode check passed with
zero copy on, and the stand-in guest's messages were the same by `cmp` on
and off, sync and async, for 144 runs a GPU and perf-check.sh's ten
([vaapi.md](vaapi.md), "Zero copy"):
`crates/reims-vgpu-video-vaapi/tools/host-session-check.sh`, run again with
`RVGV_ZERO_COPY=off` into another `RVGV_HOST_SESSION_OUT`, must give every
sequence alike. On the RTX 5080
Laptop GPU (driver 615.71.09) the NVIDIA backend passed its 387 decode
checks with zero copy on ([nvidia.md](nvidia.md)).

**What it saves**, VA-API, at udmabuf's stock limits, 2026-10-25
([vaapi.md](vaapi.md) has the table): the CPU's copy, 1.2 to 2.3 ms of a
4K frame on the RX 7900 XTX and 1.4 to 3.1 ms on the Radeon 780M, for a
GPU write that takes 1.0 to 3.1 ms longer than the copy back's processing
run. Synchronous decodes, where the copy is on the frame's path, ran up to
18% faster through the stand-in guest, and 7% and 20% slower in two rows
of AV1 ([bugs.md](bugs.md), D22); asynchronous ones, where the CPU's copy
runs beside the next decode, 0% to 12% slower, every row past 4K60 both
ways. NVIDIA's copy is the GPU's alone, 4K 19% to 37% faster
([nvidia.md](nvidia.md), "Zero copy").

**In the guest** (2026-10-25, before the strips), every frame matching on
and off: on the RX 7900 XTX, with udmabuf's limits raised, so that each
buffer was one strip, the host's CPU time a 4K
frame, the session's own threads', fell by 0.9 to 2.5 ms, to 0.4 to
0.9 ms, and the guest's synchronous 4K decodes ran 14% to 75% faster; on
the RTX 5080 Laptop GPU it fell by 1.2 to 2.7 ms, and every 4K decode ran
27% to 63% faster. The wallpaper and the screen saver's Aerial played on
the RX 7900 XTX on and off, with no reply timeout or pixel buffer
failure. The guest paces the Aerial at 30 fps, so it ran no faster; its
session's threads used 0.56 ms of the CPU a frame on and 3.61 off.

**For VideoToolbox** ([videotoolbox.md](videotoolbox.md), "Zero copy").
reims-vgpu's Metal platform makes each guest surface an IOSurface over the
guest's pages (`kIOSurfaceClientAddressRanges`), with a mapping of its own
at its base address (`crates/reims-vgpu-metal/src/os/surface_bytes.rs`),
and lends that IOSurface to a device
(`crates/reims-vgpu-metal/src/os/device_lend.rs`,
`crates/reims-vgpu/src/platform/metal/lent_memory.rs`):
- `MetalPlatform::surface_device_memory` gives the surface's own
  IOSurface, with a reference the lend holds for the hold's life, and an
  identity attached to the surface by its `IOSurfaceID`, which goes when a
  surface made later takes that ID, or, its surface gone, as the table
  grows. It refuses a protected surface (protection options set, or no
  base address) and any the guest made read-only for its clients, over
  its pages or its own memory, and asks again before each write
  (`DeviceLend::writable`, the `LentIoSurface`'s `writable`);
- a writable surface over the guest's pages is lent
  (`CLIENT_MEMORY_LENT`, true). No transfer into an IOSurface over client
  memory has run on the M3 Max yet, so a guest's first frames on a macOS
  host are its first measure ([bugs.md](bugs.md), D22);
- the backend (`crates/reims-vgpu-video-videotoolbox/src/macos/zero_copy.rs`)
  checks three ways before anything of the buffer reaches CoreVideo or
  VideoToolbox: the host's word; the IOSurface's base address, which a
  protected surface lacks; and every page of the process's own mapping of
  the buffer writable (`mach_vm_region`), the OS's answer, not the host's.
  Then one format and one even size with the frame, the IOSurface's planes
  those `GuestTarget` checked, and a pixel buffer over it
  (`CVPixelBufferCreateWithIOSurface`) laid out the same, which one
  `VTPixelTransferSession` a decoder writes the frame into;
- **the pixel buffer is made for each write and released before it
  returns, not kept in a `DeviceImports`**: it retains the IOSurface, and
  the mapper answers the guest's unmap Busy while anything but it retains
  one, so a kept pixel buffer would hold the guest's buffer past its
  `rels`. Measured on the M3 Max (macOS 27.0.1, 2026-10-25): the
  IOSurface's retain count was the same before and after each write,
  transfer included. A pixel buffer kept from frame to frame would save
  nothing: its transfer took as long ([videotoolbox.md](videotoolbox.md),
  "Zero copy").

Such a transfer may run on Apple's scaler, which wrote into an IOSurface
over pages read-only for its clients and panicked the kernel (the M3 Max,
macOS 27.0.1, 26A434, twice on 2026-10-24). So the refusals are tested
with stand-ins that count what reaches CoreVideo and the transfer
([hardening.md](hardening.md), "Zero copy on VideoToolbox"), and on the
M3 Max the transfer has run only into ordinary IOSurfaces, of their own
memory, made by IOSurface.

**What it costs on the M3 Max** (macOS 27.0.1, 2026-10-25;
[videotoolbox.md](videotoolbox.md), "Zero copy"): the transfer takes
longer than the CPU's copy and less of the CPU at 4K. Through the stand-in
guest a 1080p Main 10 frame's write took 1.89 ms against the CPU's
0.42 ms, a 4K 10-bit frame's 3.9 to 4.2 ms against 1.7 to 2.0 ms; alone,
the writing thread's CPU for a 4K 10-bit frame fell from 1.42 to 0.28 ms,
and at 1080p was about the same, 0.19 against 0.15 to 0.19 ms. Zero copy
stays on at every size here, as on every backend; whether it should below
4K waits for a guest on a macOS host to measure it ([bugs.md](bugs.md),
D22).

## Waiting for the device

A decode, an encode, a motion estimate or a copy runs on the device after
the driver call that submits it returns, and the backend waits for it
before it reads what the device wrote. No driver wait has a bound of its
own (NVDEC's map, NVENC's lock, CUDA's synchronous copy, VA-API's sync),
so a device that never finished would hold the session's thread, and the
session, for good. Each backend waits instead with the driver's own
non-blocking question, asked until the work is done or `DEVICE_WAIT` has
passed (`src/device_wait.rs`, `poll_until`): every 100 µs for the first
50 ms, then every millisecond, each pause a timed wait on the runtime's
clock. A call that returns only once the device is done, and has no
non-blocking form (NVENC's lock, NVDEC's VP9 decode, CUDA's copy into
pageable memory; VA-API's `vaGetImage`, map of a derived image, and
`vaSyncSurface` on a driver without `vaSyncSurface2`), is made on a thread
of the decoder's or encoder's own, or of the VA display's, and its answer
waited for as long; past the deadline the call is left to return there,
and nothing it holds is used until it has ([nvidia.md](nvidia.md), "How a
session waits for the GPU"; [vaapi.md](vaapi.md), "The display's calls").
Where such a call holds a lock the driver's other calls take, as Mesa's VA
frontend holds its display's, the backend's other calls on it wait for
the call by their own deadlines, not in the driver.

**The deadline is 5 s** from a frame's first wait, ours. The slowest
frame measured on any of the hosts' GPUs is the first decode after an RX
7900 XTX has idled 5 s, 0.88 s, most of it the GPU waking ([bugs.md](bugs.md),
B61); 5 s is over five times that. It stays under the guest's own wait
for a codec operation, 10 s (the reply wait of a synchronous `dfrm` and
of a `Demt`), so the guest hears of the frame before its wait runs out,
and under the 10 s a session's end waits for the message in hand
("Decisions", Threads), which so never meets a stuck call.

**What the guest sees past it** is the reply Apple's host gives a frame
its hardware fails, the backend's malfunction: a decoded frame emitted
with kVTVideoDecoderMalfunctionErr (-12911) and no image, its buffer named
in a `Dfub`; an encoded frame with kVTVideoEncoderMalfunctionErr (-12912)
and no sample; a motion estimate with -17691 and no vectors
([backends.md](backends.md), "How errors reach the guest"). The decoder,
encoder or estimator whose device missed a deadline then fails each later
call at once, the same way, so no later frame waits a deadline of its own
behind it; the guest makes a new session, as it does after any
malfunction. The JPEG engine's decode fails with kIOReturnDeviceError, and
a scaler transform with kIOReturnError. Those two outlive the sessions
they serve, the JPEG engine the process and a scaler's accelerator the
guest's DRIVER_OK to the device's reset, so each fails later calls at
once only until the device is done with what it kept, and then goes on.
The guest waits 1.1 s for a `jdec`'s reply ([devices.md](devices.md),
"The JPEG engine"), so for a JPEG decode its own wait runs out first, as
it does where Apple's host answers late ("Messages" there); QEMU's reset of the scaler device waits
for the transform in hand, which so ends by the deadline.

**What is kept.** Nothing the device may still write is handed back or
used again until the device is done with it:
- the backend's own memory the work writes and the handle it runs on (a
  decode surface, staging memory, a mapped picture, an NVENC session and
  its buffers), which the backend gives the process's `Lingering` list
  with the driver's question;
- a guest buffer the device writes itself (zero copy): the hold core
  makes for each buffer it lends a device (`GuestTarget::hold`, a
  `GuestHold`), which the backend keeps with the rest. While it lives the
  session sends no `rels` for the buffer, so the guest sees the buffer in
  use, and its pages are neither reused nor unmapped: the mapper answers
  an unmap of a held surface Busy.

`Lingering`'s thread (`reims-vgpu-video.lingering`) asks each kept work's
question every 100 ms, and drops it once the answer says the device is
done: its memory is freed, its handle destroyed, and its guest buffer let
go, whose `rels` the session's next sweep sends. An answer that is an
error says nothing of the device's writes, so the work stays kept. Work a
device never finishes is kept until the process ends: that is the leak,
on purpose, since freeing it would hand memory the device may still write
to the host's allocator or back to the guest. A device reset ends the host
session with work still kept, and its guest buffers stay held by this
host, though the guest's driver has let them go with the reset.

Core's stand-in decoder tests the guest's side
(`a_device_write_past_its_deadline_keeps_its_buffer_until_the_device_is_done`,
`src/host/decoder/tests.rs`): the frame emitted with -12911 and no image,
the buffer held while the device writes, and its `rels` once it is done.
Each backend's own section says which question it asks and what it
keeps: [nvidia.md](nvidia.md), "How a session waits for the GPU", and
[vaapi.md](vaapi.md), "Waiting for the GPU".

## AV1's and VP9's buffers

Apple's AV1 and VP9 decoders lend a guest buffer for every frame a
temporal unit or a sample carries and hold it while the frame is
referenced ([devices.md](devices.md), "When AVD lets each buffer go", in
"AV1" and in "VP9"). A hold is visible to the guest, since the mapper
answers its unmap of a held surface Busy, so core's session does the same
(`src/host/decoder.rs`, `emit_frames_decoded`;
`src/host/decoder/lends.rs`):
- after each decode a backend says which frames the unit carried,
  `BackendDecoder::frames_decoded`: each a `FrameDecoded`, its number, size,
  whether it was shown again or refused, the slots after it, its primary
  reference, whether it is output, and the sequence's largest frame (a
  VP9 frame's own size). VA-API's and
  NVIDIA's AV1 sessions take them from the codec crate's decoder
  (`av1::Decoder::steps`), and their VP9 sessions from its VP9 decoder,
  read once a decode (`vp9::Decoder::take_steps`); VideoToolbox's reads
  them from the headers beside its own decode, with the codec crate's
  decoder over `FramesOnly`, an accelerator that decodes nothing;
- for each frame, in order, the session asks `Dpba` where its size is not
  the last asked for, from the sequence's largest frame on, and lends a
  `Dbuf`; then it follows the frame's slots, copies the frame output, if
  it is, into the buffer its picture was decoded into (a frame shown again
  into its first buffer, a step's own where the session holds none),
  emits it, cleans up (`Dfub`, sweep), and lets go of the buffers due at
  the next step. Synchronously each frame is seen through before the
  next is asked for; asynchronously every frame's `Dpba` and `Dbuf` come
  first, then the reply, then the rest of each frame in turn, as AVD
  orders them when it keeps up with the guest;
- a VP9 sample that does not decode is asked for its frames too: those
  before the failure, decoded, then the frame refused and each after it.
  The session lends a buffer for each, asks no `Dpba` for a frame
  refused, emits the sample with its status and no image at its last
  frame, before that frame's clean-up, and replies after it,
  asynchronously too, as Apple's VP9 decoder does. A frame refused is
  never written and goes at the next step, and no slot holds a frame
  after it, so the slots' buffers go with it. After any other decode that
  fails the session reads no frames, and lends as before;
- a buffer let go is held until the next sweep, at the guest's next
  message, a `Dbuf`'s reply or a message for the session, or at the next
  step's clean-up, so its `rels` comes where Apple's most often does,
  never sooner by a sweep of the device's thread; the buffers let go
  there are named in one `rels` ("Decisions", Holds);
- at most 16 buffers are held, the eight slots twice. Seen through one
  by one, the rule holds at most 11, the slots, the last step's frame and
  primary reference, and the step in hand; asynchronously it holds one
  more for each step asked for ahead, and Apple's held 13. Past 16 an
  asynchronous unit sees its first steps through before it asks for
  more, and then a buffer is let go early with a warning, which Apple's
  rule never needs;
- a backend that says nothing (`frames_decoded` empty), or whose frames
  output are not those it returned, gets one buffer a frame output.

A frame decoded hidden is copied out only when it is shown, into the
buffer it was lent at its decode, and nothing is written into a buffer
whose frame is never shown: AVD's picture of a hidden frame reaches the
guest only through its later `Demt`.

**The proof.** The session tests replay the first 13 units of
aom-720p-fwdkf, synchronously and paced asynchronously, and the first 5
of svc-720p-l2t1, with a stand-in backend, and get Apple's messages
buffer for buffer, but for one `Dfub` that AVD's hardware sent before its
unit's last `Dbuf`
(`av1_frames_are_lent_and_held_as_apples_decoder_lends_and_holds_them`,
`an_asynchronous_av1_unit_asks_for_its_buffers_before_it_decodes`,
`both_spatial_layers_are_lent_buffers_at_their_own_sizes`);
`a_long_asynchronous_unit_holds_no_more_than_the_bound` and `lends.rs`'s
tests check the release rule and its bound. For VP9 they replay the first
13 samples of p0-720p-arf6, the first 5 of p0-720p-resize-early and the
recovery at a key frame after refused ones, synchronously and
asynchronously, and the first 3 of `vp9-svc 3 1 0 0`'s refused
superframes, and get Apple's messages buffer for buffer
(`vp9_frames_are_lent_and_held_as_apples_decoder_lends_and_holds_them`,
`a_refused_vp9_frame_is_lent_a_buffer_let_go_with_the_next_dfrm`,
`every_frame_of_a_refused_vp9_superframe_is_lent_a_buffer`). On whole
streams, each
backend's `conformance DEVICE framemd5 STREAM --sequence FILE` names each
message's buffers as the Apple-host probe's record of Apple's runs does
("The Apple-host probe"), and the two were set side by side with two
verdicts, since Apple's threads race:
- in order, races aside: the messages but `rels` and the replies in
  order, and each `rels` after the same `Dbuf`s or before the same reply;
- asks and ends: the `Dpba`s, `Dbuf`s and replies in order, and the
  `Dfub`s, `Demt`s and `rels` in order, each on its own, for an
  asynchronous run, where the hardware sets how the two interleave.

Against Apple's runs of the nine streams ([devices.md](devices.md), "When
AVD lets each buffer go"), every backend's synchronous sequence met both
verdicts, and its asynchronous one the second against Apple's paced runs,
on VideoToolbox (the M3 Max), VA-API on the RX 7900 XTX and the Radeon
780M, and NVDEC (the RTX 5080 Laptop GPU) alike (2026-10-24). Against
Apple's run sent as fast as AVD answered, the asks are the same and the
ends are not: AVD ran up to three units behind. VP9's 25 streams
([devices.md](devices.md), "VP9") met the verdicts on VideoToolbox, all
50 runs, and the 17 with no frame of another size on VA-API and NVDEC,
on the same GPUs, as "VP9" says (2026-10-25). Apple's records are not in this
repository, so a change to the lends is checked by the session tests
above.

## Decisions

- **Threads.** Apple's decoder session runs each session's messages on a
  serial dispatch queue, and a handler that asks the guest for a buffer
  blocks that queue until the guest's reply comes through a delivery. The
  device delivers on its own thread, so each decoder session has a thread
  of its own (`reims-vgpu-video.decoder`), the runtime's, with a channel
  as its queue;
  replies (flag 0x8) go to its reply clerk on the delivering thread.
  Nothing the device calls waits for the guest. `-dec` and `invalidate`
  wait for the session's thread as Apple's `dispatch_sync` does, and
  cancel its waits first: Apple's run out, since the device's thread that
  would carry the reply is the one waiting ([devices.md](devices.md),
  "Where we differ from Apple"). A session's end waits 10 s at most on
  the runtime's clock, for the message in hand and then for the thread to
  end; a reset waits one 10 s for every session it ends in each of its two
  phases. Past it the thread is left running, detached; while 4 are
  detached and running in the process, the host makes no new session
  (`src/host/session_thread.rs`).
- **Copies.** The backend decodes into its own surfaces (radeonsi has no
  `USER_PTR` import), and each frame reaches the guest's buffer between
  `Dbuf`'s reply and `Demt`: written by the backend's device ("Zero
  copy"), or copied by the session through `GuestSurface::write` and
  `SurfaceLayout::split`. A guest buffer is held from `Dbuf`'s reply to
  the end of its frame, at most one at a time but for AV1's ("AV1's
  buffers"), so a `rels` follows each frame, as with Apple's AVD.
- **Ordering.** The sweep sends its `rels` under the surface table's
  lock, so a session's reply, sent after its own sweep, never overtakes a
  `rels` the device thread swept first.
- **Holds.** A `SurfaceHold` stands for Apple's child surface: the
  session's table keeps a `Weak`, a delivery and a reply keep the `Arc`,
  and the sweep after every delivery sends `rels` for the dead ones. A
  failed retain drops the message's surfaces and leaves an ID-0 entry, as
  Apple's does.
- **Retaining.** The mapper's retain runs outside the table's lock: over
  reims-vgpu's mapper it waits for the IOSurface device's queue, behind
  the guest's ring commands and the host's `raise_interrupt`, which the C
  API lets deliver a message. A delivery claims the surface's entry under
  the lock, retains, and publishes the hold under the lock again; the
  sweep keeps a claimed entry, so it never names one as ID 0, or as
  released, while its retain is in flight. Each publish is as if the whole
  lookup were made then: a hold another delivery published meanwhile is
  the surface's, and the retain's own surface is let go, outside the lock;
  a hold that ended meanwhile is replaced unnamed, as a lookup replaces a
  dead hold; and a failure leaves an entry that had a hold as it was, and
  otherwise an ID-0 entry of its own (`src/host/mapped_surfaces.rs`,
  `hold`). The scaler device retains a transform's two surfaces outside
  its accelerator's lock for the same reason, between a check under it
  that there is an accelerator and a second once both are held: a reset
  between the two makes the transform a needs-reset, a restart a
  transform on the new accelerator (`src/scaler.rs`, `transform`).
- **Letting go.** A hold ends once the mapper's hold on its surface has
  been let go, not as its last `Arc` starts to drop, so no `rels` names a
  surface the host still holds. Every thread lets go of holds through
  `HostShared::let_go`, which lets go of the surfaces outside the table's
  lock, then ends the holds and sweeps under it: holds let go together
  (a frame's two sources, the buffers due at a step, a message's
  surfaces) are named in one `rels`, whichever thread's sweep comes
  between their drops, and that `rels` comes where the session lets them
  go. A sweep of the device's thread can still land before or after a
  session's whole release, as Apple's can (`src/host/mapped_surfaces.rs`,
  `ending_together`).
- **Encoder threads.** Each encoder session has a thread of its own
  (`reims-vgpu-video.encoder`), as each decoder session has, since its
  handlers wait for the guest on `Epba`, `Efds` and `Eemt`. A source frame
  is read from the guest's surface during the `efrm` that lent it, and
  the surface is held until the frame is both encoded and emitted, as
  Apple's pending frames hold their pixel buffers. A backend that keeps
  frames so keeps their surfaces held.
- **Format descriptions.** The session asks the guest for one (`Efds`)
  before the first emission and whenever the backend's configuration
  record changes, and the first sample after carries it. The `fdsc` the
  guest gets is built from the guest's `=fds` with the extensions Apple's
  host gave it. The host has no CoreMedia to parse it, so they are written
  out.
- **The stand-in.** The tests' encoder writes as its sample bytes it read
  from the frame, so a read at the wrong stride or offset shows. Core
  encodes nothing itself: the encoders are the backends'.
- **`-all`** does nothing, as on Apple's customer builds.
- **JPEG** decodes on the backend's `JpegDecoder`, the `jpeg` part of
  its `Backend`, which a session made with `HostSession::with_backend`
  takes (`new` and `with_encoders` have none). Core reads CMPhoto's
  options and picks the region, scale and destination layout, and the
  backend writes the pixels. With no decoder, and for every `jenc`, the
  answer is -16994, Apple's where it has no JPEG hardware
  ([devices.md](devices.md), "The JPEG engine").
- **The JPEG engine's lends.** A `jdec` lends its two surfaces three
  times, and calls its backend in none of them:
  1. the source's read lend, which copies all of it out, as Apple's host
     copies it;
  2. the destination's write lend, which writes nothing: it checks the
     host can write the destination and that its planes hold the output,
     so that the backend is asked only for a decode that can be
     delivered, as when it wrote into the destination itself;
  3. after the decode, the destination's write lend again, which copies
     the output's rows into its top-left corner.

  Between 2 and 3 the backend decodes into a buffer of the host's, its
  planes packed at the output's size, on a thread of its own
  (`reims-vgpu-video.jpeg`), which the delivering thread waits for 10 s
  at most, the bound of a session's end, through
  `SessionThread::answer` (`src/host/session_thread.rs`). Past it the
  thread is left running, detached, and counts against the same 4; the
  answers are in [devices.md](devices.md), "Where we differ from Apple".
  The buffer is kept for the next decode up to 64 MiB, so that later
  decodes fault in no new pages, and is not cleared: the backend writes
  all of the output, as `JpegDecoder::decode` promises. The core tests'
  stand-in counts the lends in progress on every thread and records any
  decode made during one; the host fuzzer's refuses a decode inside a
  lend on its own thread.
- **Names.** Decoder IDs (`reims-vgpu-video.videodecoder.avc`), their
  display names and `VTHostBuildVersion` (`reims-vgpu-video <version>`) are
  ours, as are the encoder IDs (`reims-vgpu-video.videoencoder.avc`,
  `.hevc`) and names. The decoder and encoder ratings are Apple's
  hardware codecs' 400. An encoder session also takes Apple's AVE IDs in
  `,cid`, which the guest's two fixed encoders send.
- **Where Apple's scanners overread.** Apple's fragment walk scans from an
  offset with the whole message's length (read from Apple's code); ours
  bounds it by what remains. The difference shows only on a malformed
  message, which both refuse; the status is not reported to the guest for
  a decoder message.
- **Empty `dict` atoms** read as empty dictionaries. What Apple's atom
  reader does with a parent that has no children is unknown, and Apple's
  writer writes none where a dictionary is read: the empty `dict` the
  guest writes as an array's element (`[{}]`, the attachments of
  AVAssetReader's first AV1 sample) is skipped, as an element with no
  payload (`src/wire/sample_buffer.rs`).

## Measured on this Mac

The M3 Max, macOS 27.0.1 (26A434), arm64, 2026-10-24, by throwaway probes
(nothing they printed is committed):

- `FigCreateCFDataFromCFPropertyList` writes `bplist00`; NULL is no bytes.
- The 212 known keys' strings, by `dlsym` of each constant.
- The `decs` levels (80 for AVC, 186 for HEVC) and AVD's `dsup`.
- The ImageDescription CoreMedia writes for a 64x48 H.264 stream.
- The exported `VTParavirtualizationHostSession*` functions, driven with
  hand-built messages: every list reply, the error statuses, the decoder
  session's order for `dsta` and `dfrm` with and without asynchronous
  decode, the `Dpba` attributes, the 10 s `Demt` timeout, and the AVC
  decoder's own answers ([devices.md](devices.md), "The decoder session").
- `VTParavirtualizationCreateMessageAndByteStream` and the other message
  helpers are not exported on macOS 27, so serializations could not be
  dumped through Apple's writers; their formats are read from Apple's code
  alone, and the tests are written by hand from it.

The M3 Max, macOS 27.0.1 (26A434), 2026-10-24, by a throwaway probe
driving the same functions as the guest's encoder would:

- `encs`, `+enc` with each kind of `,cid`, and AVE's properties and
  `Epba`.
- AVE cannot encode in an unentitled process (`AVE_UC_Start` 0xe00002c2),
  and traps on `ecop` of a key it lacks. So the frame exchange was
  measured with Apple's software H.264 and HEVC encoders: `Efds`, `Eemt`
  and its fragments, the order of replies and `rels`, the sample buffer
  and its `fdsc` ([devices.md](devices.md), "The encoder session").

macOS 27 is not the guest's release: a fact measured here is a hypothesis
for 26.7 until a 26.7 guest confirms it.

## The Apple-host probe

The facts the docs cite to "the Apple-host probe" were measured on the
M3 Max (macOS 27.0.1, 26A434), 2026-10-24, with a probe that is not part
of reims-vgpu. It found the four exported
`VTParavirtualizationHostSession{Create,DeliverMessageFromGuest,
Invalidate,CompleteInvalidate}` by `dlsym`, built the guest's messages
with core's `MessageWriter`, read the replies with `Message`, and played
the guest's decoder, encoder, motion-estimation processor or multi-pass
client, lending ordinary writable `CVPixelBuffer`s that CoreVideo
allocated. Besides the colour, motion-estimation, multi-pass, restart and
buffer-lifetime facts [devices.md](devices.md) gives with it, it measured:
- AVD decodes in an unentitled process, as AVE's motion estimation does;
  AVE's encoders do not ("Measured on this Mac").
- With every `dfrm` of a 1920x1080 HEVC Main 10 stream asynchronous
  (`,dfl` 1), the host replied to each in about 0.7 ms (the slowest a few
  ms), whether the guest recycled its buffers or held every one, and kept
  one to two buffers in flight (`Dbuf` for the next frame before `Demt`
  of the last), well within the guest's 1.1 s wait. Its `Dpba` carried
  `BytesPerRowAlignment` 64, `PlaneAlignment` 256,
  `PreferRealTimeCacheModeIfEveryoneDoes` true and `ExtendedPixelsBottom`
  8 (1088 coded minus 1080 displayed).
- Apple's message sequences, each message's buffers named as the
  conformance crate's `--sequence` names ours, which "AV1's and VP9's
  buffers" compares.

On 2026-10-25 (same Mac and release) the same probe lent the buffers as
the guest lends them, from a CoreVideo pool made of each `Dpba`'s whole
attributes and kept until the host's `rels`; it sent each `dfrm` whose
sample buffer passed 0x200000 bytes in fragments, as the guest's decoder
cuts them; and it sent `dsup`, `dcop` and `dset` to a decoder session,
before `dsta` and after. Never smaller than the stream's largest frame,
its buffers kept AVD from writing past one. What it measured of VP9 is in
[devices.md](devices.md), "VP9".
