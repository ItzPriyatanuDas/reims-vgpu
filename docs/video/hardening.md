# Hardening

Every byte the guest sends is untrusted. The library runs inside QEMU's
process on a shared host, so a guest must not be able to make it panic,
abort, overflow its stack, allocate without bound, loop for long, read or
write out of bounds, or hand a driver a parameter outside what the
bitstream's specification allows: a bad decode can hang the GPU, and that
resets it under everyone.

This file lists each hazard a review as an attacker and the fuzzers found
(F1 and up): how a guest reaches it, the bound or check that holds it,
what the guest sees, and the tests that keep it. Then the bounds that are
ours, what is left open, and the fuzzers. Where a bound makes the guest
see something other than Apple's host would, it is in
[devices.md](devices.md), "Where we differ from Apple", too.

Paths are under `crates/`, in each crate's `src/`: `wire/`, `host.rs`
and `host/`, `scaler/`, `motion.rs`, `surfaces.rs`, `device_memory.rs`
and `encoder_backend.rs` are core's (`reims-vgpu-video-core`); `h264/`,
`hevc/`, `av1/`, `vp9/`, `jpeg/` and `encode/` the codec crate's
(`reims-vgpu-video-codec`); `va/` the VA-API backend's
(`reims-vgpu-video-vaapi`); `nvdec/` the NVIDIA backend's
(`reims-vgpu-video-nvidia`); and `macos/` the VideoToolbox backend's
(`reims-vgpu-video-videotoolbox`). A test is named by its module path in
its crate.

## Findings

### Property lists

**F1. A binary plist can decode to far more values than its bytes, and a
deep one overflows the stack when it is dropped.** Every `=pls` parameter
and every sample buffer leaf is a property list from the guest, which
`wire/property_list.rs` reads with the plist crate (1.10). A binary plist
names its objects by index, and the crate reads a container again at each
reference to it (`binary_reader.rs`, `read_next`; it caches scalars only;
read in its source). So 41 objects, each an array of two references to
the next, decode to 2^40 values: a guest's `dset` of a few hundred bytes
would make QEMU allocate until the host's OOM killer acts. Separately, a
value nested 100,000 deep, as a chain of binary arrays or as XML, decodes
(the crate's builder does not recurse), but dropping it recurses once a
level and overflows the thread's stack.
- Bound: a binary plist's object graph is walked before it is decoded, as
  the reader reads it (trailer, offset table, markers, references), each
  object once, and refused when it would decode to more than 65,536
  values (`MAX_VALUES`), more than 64 levels (`MAX_DEPTH`), or more than
  16 MiB of strings and data beyond its own length (`MAX_EXTRA_BYTES`),
  or when it refers to itself. The value of any encoding is then built
  from the crate's events and held to the same values and depth as it is
  built (F44).
- What the guest sees: -19093 (malformed) where a parameter is read, as
  for a plist that does not decode. Apple's CoreFoundation reader shares
  objects, so the bounds are a difference ([devices.md](devices.md),
  "Where we differ from Apple").
- Tests: `wire::property_list::tests`: the expansion bomb, a string
  referred to 30,000 times, a chain one past the depth and 20,000 deep, a
  cycle, and XML 200,000 deep.

**F44. A text plist that nests deep and then fails to read overflows the
stack.** The plist crate reads XML and OpenStep (ASCII) plists too, and its
builder (1.10, `value.rs`, `Builder`) keeps the containers it has open on
a stack of its own, but holds each one it has closed inside the one around
it. When the plist then fails to read, its error drops what it built, and
that drop recurses once a level. So containers nested N deep and closed
all but the outermost, then the plist's end, made N levels of recursion
before anything of ours saw the value: in an ASCII plist, 2 bytes a
level. In a release build on the M3 Max (2026-10-25), 10,000 levels, a
20 KB `=pls` parameter, overflowed a 512 KiB thread, a session's thread in
the library, and 40,000 a 2 MiB one; 5,000 did not. F1's bound held only a
value that read whole. Found by reading, for the stack's headroom on the
platform's threads; the property-lists fuzzer, which wrote no text plist
deeper than its generator's 4 levels, reached it once it did.
- Bound: core builds the value from the crate's events
  (`wire/property_list.rs`, `build`, the crate's `stream` module, which its
  unstable feature makes public, held to 1.10 in core's `Cargo.toml`), as
  the crate's builder builds it, and refuses the first value past 65,536
  or 64 levels as it comes. No value deeper than 64 levels is made, so any
  refusal drops at most 64 levels.
- What the guest sees: -19093 (malformed) where a parameter is read, as
  for a plist past F1's bounds. A plist within them decodes as before.
- Tests: `wire::property_list::tests::deep_text_that_breaks_off_is_refused_without_recursion`,
  `text_nests_as_deep_as_the_limit_and_no_deeper`,
  `text_of_more_values_than_the_limit_is_refused` and
  `text_is_built_as_the_crates_builder_builds_it`.

**F49. F1's walk read every object's references whole, and added up only
at its end.** A binary plist's offset table may give many objects one
offset, or offsets inside one another's references, and the walk visited
each object once by its index, reading all of a container's references
into a list as it opened it, and checked the bounds only once the root
was done. So a root of 20,000 references to 20,000 objects at one offset,
an array of 20,000 references, 240 KB, took 7.5 s of a release build on
the M3 Max (2026-10-25): the time goes as the square of the plist's size,
minutes for a 1 MB `=pls`. And a path of containers whose references
overlap kept each one's list while it was open, 16 bytes a reference, so
64 containers sharing a megabyte of references would have held 1 GB
before the walk reached its end. Found by reading.
- Bound: the walk reads a container's references one at a time as it
  reaches them, adds each child to its container as it is done, and
  refuses as soon as it has read more than `MAX_VALUES` references, has
  more than `MAX_DEPTH` containers open, or a container has added up past
  a bound. Each reference read is a value the plist decodes to at least
  once, so a plist refused early is one the end's check refused too, and
  the walk reads at most 65,536 references.
- What the guest sees: the same as before, -19093 for a plist past the
  bounds.
- Tests: `wire::property_list::tests::objects_that_share_one_offset_are_refused_in_linear_time`,
  `references_as_many_as_the_values_allow_decode`.

**F51. The plist crate keeps 9 bytes for each object of a binary plist's
offset table, reached or not.** Its reader reads the whole table as it
starts (1.10, `binary_reader.rs`, `read_trailer`; read in its source),
an offset and a tag an object, before the walk's bounds come into it: the
walk counts only the objects the root reaches. A table of 1-byte offsets
is 9 times its own size there, so a sample buffer leaf of 100 MB, in a
`dfrm` of 128 MiB (F6), would hold 900 MB while it is read. Found by
reading.
- Bound: a binary plist of more objects than the values left
  (`MAX_VALUES` for one alone, F50's budget in a sample buffer) is
  refused before it is read. CoreFoundation's writer writes each object
  the root reaches once, and no other, so such a plist from it decodes to
  more values than the bound anyway.
- What the guest sees: -19093 (malformed) where a parameter is read, and
  no reply to a `dfrm`, as for a plist past F1's bounds. Only a plist
  with objects nothing refers to is refused that was not before.
- Tests: `wire::property_list::tests::objects_as_many_as_the_values_allow_decode`.

### Messages

**F2. Walking a fragmented value can take quadratic time.**
`Message::fragmented` (`wire/message.rs`), which reads `=dat` and `=sbf`,
looks for each fragment's header from where the walk stands, as Apple's
reader does (read from Apple's code). Were the header looked for again at
each piece, a message of one header flagged 0x10, N one-byte pieces and a
second such header would have each piece's search run to the far header:
N^2/2 atom steps in all, about 5e9 for a 1.7 MB `dfrm`, minutes of a
session's thread, and days for a reassembled `dfrm` of 64 MiB.
- Bound: the header found stays the first at or after the walk's place
  until the walk passes it, so it is looked for again only then, and the
  walk is linear. What is read is what Apple's reader reads.
- Test: `wire::message::tests::many_pieces_before_a_far_header_walk_in_linear_time`.

### Sample buffers

**F3. Nested values can overflow the stack, and many keys take quadratic
time.** `SampleBuffer::parse` (`wire/sample_buffer.rs`) reads `dict`,
`aray` and `fdsc` values by recursion, as Apple's reader does, with no
limit there (read from Apple's code). An `aray` inside an `aray` costs 8
bytes a level, so a `dfrm` of 2.4 MB nests 300,000 deep, past the 512 KiB
stack of a session's thread, a platform worker in the library: the thread
would overflow and QEMU die. And a dictionary of N string keys, each
checked for a repeat against every entry before it, would take N^2/2
comparisons.
- Bound: values nest 32 deep at most (`MAX_NESTING`), the attachments'
  own dictionary or array being 1, and past that the serialization is
  malformed. The keys seen are kept in a set.
- What the guest sees: a `dfrm` that does not read gets no reply, as for
  any malformed serialization.
- Tests: `wire::sample_buffer::tests::values_nest_as_deep_as_the_limit_and_no_deeper`,
  `a_repeated_key_keeps_its_first_value`.

**F50. A sample buffer's leaves were each held to F1's bounds alone.**
Every leaf (`nmbv`, `strk`, `datv` and the rest) and every `strk` key is a
property list, and F1 held each to 65,536 values and 16 MiB of strings
and data on its own, with no bound on how many a sample buffer has. A
binary plist of about 100 bytes, fifteen arrays each of two references to
the next and an integer at the end, decodes to 65,535 values, at least
5.2 MB of the plist crate's 80-byte values; so a 1 MiB `dfrm` of 8,000
such leaves would decode to about 40 GB. A leaf of 33 KB whose 16,000
references name one 1 KiB string decodes to 16 MB of it. The `dfrm` is
gathered up to 128 MiB (F6). Found by reading; the sizes are computed.
- Bound: one budget for the whole sample buffer, of what one property
  list may decode to (`property_list::Budget`): 65,536 values, its own
  dictionaries, arrays, keys, nulls and format descriptions and its
  leaves' values between them, and 16 MiB of strings and data beyond the
  serialization's length that its binary leaves decode to. Each leaf is
  checked against what is left before it is decoded, as F1 and F49 check
  one plist.
- What the guest sees: a `dfrm` whose sample buffer passes the budget
  gets no reply, as for any malformed serialization. Apple's reader has
  no such bound; a guest's sample buffer holds a few dozen values
  ([devices.md](devices.md), "Where we differ from Apple").
- Tests: `wire::sample_buffer::tests::its_values_and_its_leaves_share_one_bound`,
  `its_leaves_share_one_bound_of_bytes`.

### The host session

**F4. A message to the guest longer than its buffers ends QEMU.** The
guest's kext posts queue-1 buffers of 0x4000 bytes, and the device ends
QEMU on a longer message, as Apple's host ends the VM (read from Apple's
code). Two ways get there:
- a reply that echoes the guest: `dset` of `RealTime` with 20,000 bytes of
  data, then `dcop` of it, and alike `eset`/`ecop` of `DataRateLimits`
  with 3,000 numbers;
- a sweep that releases many surfaces at once, in one `rels` of 8 bytes a
  surface: past 2,047, as when `-dec` drops a queue of messages that named
  different surfaces.
- Bound: `HostShared::send` (`host.rs`), which every message to the guest
  goes through, sends none longer than 0x4000 bytes (`GUEST_BUFFER_SIZE`)
  and answers -12902; the guest's wait for that reply times out. A sweep
  sends its IDs in `rels` boxes of 2,047 at most
  (`MAX_RELEASES_PER_MESSAGE`), which the kext reads one by one.
- What the guest sees: a wait that times out where Apple's host would end
  the VM ([devices.md](devices.md), "Where we differ from Apple").
- Tests: `host::bounds_tests::a_reply_longer_than_the_guests_buffer_is_not_sent`,
  `releases_past_one_message_go_in_several`.

**F5. Fragments can be gathered without end.** A `dfrm` flagged 0x10 is
kept until its last fragment, and so is a reply flagged 0x10 to a host
request, with no limit in Apple's host (read from Apple's code): a guest
that never sends the last fragment would grow the host's memory for as
long as it liked.

**F6. The messages for a session's thread can queue without end.** The
device's thread hands each message to the session's queue and returns,
and while the session waits for the guest (up to 10 s for a `Demt`), a
guest could queue gigabytes.

The bound on both (`host/guest_bounds.rs`):
- A host session holds 512 MiB and 4,096 of the guest's messages at most
  (`MAX_HELD_BYTES`, `MAX_HELD_MESSAGES`), queued for its sessions'
  threads or kept as a `dfrm`'s fragments, and one decoder, encoder or
  motion-estimation session half those bytes and a quarter of those
  messages (`MAX_HELD_BYTES_PER_SESSION`, `MAX_HELD_MESSAGES_PER_SESSION`),
  so that one flooded session cannot starve the rest. A hold must fit
  under both (`HeldMessages`), and a refused one charges neither. A
  message past them is not queued, and its delivery answers -12904
  (`kVTAllocationFailedErr`). Replies to the host go straight to the reply
  clerk, outside these bounds.
- One `dfrm` gathers 128 MiB at most (`MAX_FRAME_MESSAGE_BYTES`), past the
  80 MB of the largest picture a driver here decodes, 8192x4352 at 12
  bits; past that it is dropped with its later fragments, with no reply,
  as one that does not read (`host/decoder.rs`, `decode_frame`).
- A reply gathers 16 MiB at most (`MAX_REPLY_BYTES`), and past that is
  malformed, so the host's wait times out (`host/reply_clerk.rs`).
- Left as it is: a guest that floods two sessions with 256 MiB each, or
  four with 1,024 messages each, still fills the whole bound and has its
  own other sessions' messages refused. The host's memory stays bounded,
  and only the guest's own sessions wait.
- Tests: `host::bounds_tests::a_frame_message_past_the_bound_is_dropped`,
  `messages_past_what_the_host_holds_are_refused`,
  `a_flooding_session_does_not_starve_another`,
  `a_reply_past_its_bound_is_malformed`, and `host::guest_bounds::tests`.

**F7. A guest can open sessions without end.** Each `+dec` or `+enc` for
a codec the backend has, and each `+mep` it takes, makes a session with a
thread of its own, and a decoder, after `dsta`, its backend's decoder
with surfaces on the GPU: about 480 MB of VRAM for a 4096x4096 H.264
stream with a 16-frame DPB. A refused one still keeps a table entry. A
guest that opens them in a loop would starve the host of threads, memory
and the shared GPU's VRAM.
- Bound: a host session keeps 16 decoder sessions at most, decoding or
  refused, 16 encoder sessions and 16 motion-estimation sessions
  (`host::MAX_SESSIONS`). A `+dec` past them is answered with the create
  reply Apple's host sends for a codec it cannot make, its `,err` -12913
  (`kVTVideoDecoderNotAvailableNowErr`, `VTErrors.h` in the macOS SDK),
  and keeps no session; `+enc` likewise with -12915, and `+mep` with its
  own not-found, -19350 (`host.rs`, `refuse_for_now`). The count does not
  bound VRAM (B14, "Open findings").
- What the guest sees: it fails its create on the `,err`, as for a codec
  the host lacks.
- Test: `host::bounds_tests::sessions_past_the_bound_are_refused_for_now`.

**F46. A queue element's readable part has no bound of its own.** QEMU's
device gathers each guest-to-host element's readable part into one buffer
before the library reads it, and the guest's descriptors, 1,024 of up to
4 GiB each, can add up to many times the host's RAM: the allocation
aborts QEMU, or the copy runs until the OOM killer acts. The guest's
largest message is a 0x200000-byte sample and its header (devices.md,
"The queues").
- Bound: a message over 16 MiB (`reims_vgpu::MAX_MESSAGE_SIZE`,
  `RVG_VIDEO_MAX_MESSAGE_SIZE` in the C API) is answered -12904
  (`kVTAllocationFailedErr`), unread: `VideoDevice::deliver` answers it
  before the host session sees it, and the QEMU device
  (`vendor/qemu/hw/display/apple-videotoolbox-pci.c`) answers it from the
  descriptors' sizes, before it allocates or copies.
- What the guest sees: the status the host session gives a message past
  what it holds (F6), and no reply.
- Test: `video::tests::a_message_past_the_bound_is_answered_unread`
  (`crates/reims-vgpu`), and the C harness's video device.

### H.264

The H.264 parsers range-check each syntax element as they read it
(`h264/{sps,pps,slice}.rs`), so what is left is what no one element
shows: how elements combine, and what the decoding process makes of them.

**F8. Pictures the standard forbids can reach the accelerator.** Each of
these would go to `vaRenderPicture` as it came:
- `SliceQPY` past 51: `slice_qp_delta` is read to the 14-bit range, -87 to
  77, so `pic_init_qp_minus26` 25 and `slice_qp_delta` 26 give 77 at 8
  bits (7.4.3), and a driver or its firmware indexes its QP tables with
  it. `pic_init_qp_minus26` below -26 likewise (7.4.2.2).
- A PPS or SPS resent with other content between two slices of one
  picture (7.4.1.2.1 forbids it): the picture's parameters come from the
  first slice's sets, and the later slices' headers are parsed with the
  new ones, so the driver reads their slice data at the wrong offsets.
- More references than `max_num_ref_frames`: 16 long-term frames and a
  short-term one, or a long-term IDR and a P frame when it is 1, which the
  sliding window cannot help (8.2.5.3). VA-API's `ReferenceFrames` holds
  16, and a 17th would be dropped without a word.
- A slice with nothing after its header (7.3.4 has at least one
  macroblock).
- `DiffPicOrderCnt()` past 16 bits where a B slice's temporal direct
  prediction or implicit weights use it (8.2.1).
- Check: `h264/bounds.rs` (`check_picture`) checks each picture as
  `Decoder` hands it to its accelerator, whatever accelerator it is: the
  parameter sets of every slice are the picture's, `first_mb_in_slice` is
  in the frame, `SliceQPY` is in range, slice data follows each header,
  the references fit `max_num_ref_frames`, each list is as long as the
  slice says and names only those references, and the order count
  differences fit. A picture past them decodes nothing.
- What the guest sees: the frame's emit carries -12909
  (`kVTVideoDecoderBadDataErr`), as for any stream that does not decode.
- Tests: `h264::tests::a_slice_qp_past_51_never_reaches_the_accelerator`,
  `a_slice_with_no_slice_data_never_reaches_the_accelerator`,
  `a_pps_changed_between_slices_of_a_picture_never_reaches_the_accelerator`,
  `references_past_max_num_ref_frames_never_reach_the_accelerator`,
  `order_count_differences_past_16_bits_are_refused_where_b_slices_use_them`.

**F9. An SPS that renumbers frames mid-sequence underflows the gap
process.** A new SPS that makes frames like the active one's takes over
without a reconfiguration (`h264/decoder.rs`, `activate`). One with a
smaller `MaxFrameNum` at a non-IDR picture would leave `PrevRefFrameNum`
past it, and the frame number gap that followed would compute
`frame_num + MaxFrameNum - PrevRefFrameNum - 1` below zero: a panic on
the session's thread in a debug build, and a wrapped gap in a release
one. Within a sequence an SPS may not change at all (7.4.1.2.1).
- Check: at a non-IDR picture a new SPS takes over only when it differs
  from the active one in its VUI alone (`same_but_vui`), which the
  decoding process reads only for output order.
- What the guest sees: -12909 for the picture, as for a new SPS that
  changes the frames there.
- Test: `h264::tests::an_sps_that_changes_frame_numbering_at_a_non_idr_picture_is_refused`.

**F10. Operation 5 can overflow on order counts at the ends of 32 bits.**
After `memory_management_control_operation` 5, a frame's counts drop by
the smaller of them (8.2.1). Type 0 computes a bottom count as
`TopFieldOrderCnt` plus `delta_pic_order_cnt_bottom`, wrapping, so a top
count of 10 and a delta of 2^31 - 1 wrap the bottom to near -2^31, and a
plain subtraction from the top would overflow: a panic in a debug build.
- Bound: the subtraction wraps (`h264/decoder.rs`), as the counts'
  computation does; only output order follows from a stream past 32 bits.
- Test: `h264::tests::operation_5_on_order_counts_at_the_ends_of_32_bits_does_not_overflow`.

**F41. A picture's slices can be gathered without bound, and can start at
one macroblock.** A picture's slices are kept, each with its parsed
header, until the picture ends (`h264/decoder.rs`, `Decoder::decode`),
and 7.4.3 forbids two slices that start at one macroblock. Unbounded, a
sample of an SPS, a PPS and 1,000,000 copies of one 6-byte IDR slice,
10 MB, reached the accelerator as one 1920x1080 picture of 1,000,000
slices, in 0.16 s and 400 MB of the process (measured with a release
build on the M3 Max, 2026-10-24); a `dfrm` of 128 MiB holds 13 million of
them, about 5 GB. The NVIDIA backend hands NVDEC every slice, as
`nNumSlices` and `pSliceDataOffsets` (`nvdec/h264.rs`), and the VA-API
backend a slice parameter buffer each. What either driver does with such
a picture is unknown: it would be a stream no encoder makes, on a shared
GPU. F19 bounds HEVC's segments alike.
- Bound: a picture's slices are gathered no further than its SPS's
  macroblocks, as HEVC's segments are no further than its CTBs, and a
  picture with two slices that start at one macroblock is refused before
  the accelerator (`h264/bounds.rs`).
- What the guest sees: -12909 for the picture.
- Test: `h264::tests::slices_at_one_macroblock_or_past_the_macroblocks_never_reach_the_accelerator`.

Checked and sound: type 1 order counts are summed in 64 bits, where
`(AbsFrameNum / cycle) * ExpectedDeltaPerPicOrderCntCycle` stays below
2^63, since `FrameNumOffset` is 32 bits, so `AbsFrameNum` is below
2^31 + 2^16, and each offset is below 2^31 in magnitude; the
cropping offsets are bounded when the SPS is read; reference list
modification works in 64 bits.

### The encoders

**F11. A NaN `Quality` can overflow an encoder's QPs.** `f64::clamp`
passes a NaN through. With no bitrate set, the session encoder makes
`Quality` a constant QP (`encode/session.rs`, `constant_qp`): a NaN rounds
to QP 0, and its I pictures' `qp - 2` overflows, at `esta` and at an
`eset` after it. In a debug build, or reims-vgpu's `checked` profile, the
session's thread would panic, which in reims-vgpu ends the process, as a
panic on any of the library's threads does (the session's threads are its
platform's, [core.md](core.md), "The API"); in release the QP wraps to 254
and `esta` fails with -12902.
- Bound: `eset` refuses a `Quality` outside 0 to 1, a NaN or an infinity
  among them, with -12900, as Apple's AVE does (measured in process on
  the M3 Max, macOS 27.0.1, 2026-10-24: `host/encoder/properties.rs`,
  `in_range`). Behind that, the settings leave a NaN `Quality` unset, and
  `constant_qp` takes a NaN as 0, so any number makes QPs within 16 to 42.
- What the guest sees: -12900 for the `eset`; the session encodes as with
  no `Quality`.
- Tests: `host::encoder::properties::tests::numbers_out_of_aves_ranges_are_refused`,
  `encode::session::tests::any_quality_makes_qps_within_51`.

**F12. A `DataRateLimits` that rounds to 0 bit/s fails the session, and
infinite rates pass as set.** `DataRateLimits` of `[1, 100]`, 8 bits over
100 s, makes a peak of 0, and the VA-API session refuses a VBR of 0: taken
as set, `esta` would answer -12902, and after `esta` every `efrm` of a
constant-QP stream would fail. An infinite `MaxKeyFrameIntervalDuration`
or `ExpectedFrameRate` passes a `> 0` check.
- Bound: a limit that rounds to nothing is none, as an `AverageBitRate`
  of 0 is (`host/encoder/properties.rs`, `max_bit_rate`), and the
  settings take only finite durations and rates. `eset` refuses an
  infinite `ExpectedFrameRate` with -12900, as AVE does, and takes an
  infinite `MaxKeyFrameIntervalDuration`, as AVE does, which the settings
  then leave unset (`in_range`, measured as in F11).
- What the guest sees: the session encodes as with no limit.
- Test: `host::encoder::properties::tests::infinite_rates_and_limits_that_round_to_nothing_are_unset`.

**F13. A source frame's planes come as the guest laid them out.** A source
surface need only fit its memory (`surfaces.rs`, `plane_ranges`), so a
`420v` frame can come with one plane, with rows narrower than the
picture, or with a chroma plane of no rows, which fits at any stride,
`usize::MAX` included. A backend that takes such a frame at its word
indexes a second plane that is not there, or past a plane, as the VA-API
upload would (`va/encoder/surfaces.rs`, `copy_planes`, `check_planes`).
- Bound: core passes a frame on only when it has two planes, each with
  the picture's rows and each row's bytes (`host/encoder.rs`,
  `planes_hold`), and answers the `efrm` with -12902 otherwise, as for a
  source in a format the session does not take. What the backend gets is
  core's copy of the picture, each plane's rows packed
  (`host/encoder/source_copy.rs`), as `SourceFrame::planes` promises.
- Tests: `host::encoder::tests::a_source_whose_planes_do_not_hold_the_picture_never_reaches_the_backend`,
  whose stand-in backend panics on the first such frame, and
  `host::encoder::source_copy::tests::planes_that_do_not_hold_the_picture_copy_nothing`.

### The JPEG engine

**F14. A `jdec` destination's planes can be smaller than its output.** A
destination whose layout has the output's width and height can have
planes of a byte each: a 640x480 `420f` destination whose two planes are a
byte each would reach the decoder with rows of one byte, and a backend
that writes the output as `JpegDecoder::decode` promises writes past
them.
- Check: the destination is lent first, with nothing written, and each
  plane must have the output's rows and each row's bytes for the format
  (`host/jpeg.rs`, `planes_hold`), or the `jdec` is answered as for a
  destination smaller than the decode, kIOReturnBadArgument.
- Test: `host::jpeg::tests::a_destination_whose_planes_do_not_hold_the_decode_is_a_bad_argument`.

**F15. A stream can carry Huffman tables no 8-bit baseline decode has.** A
DHT may hold up to 256 values (`jpeg/syntax.rs`): a DC table of 16 codes,
an AC table of 200, AC values of size 15, tables with no codes. 8-bit DC
categories are 0 to 11 and AC values a run with a size of 1 to 10, or EOB
or ZRL (F.1.2.1, F.1.2.2): 12 and 162 values, which are what VA-API's
baseline Huffman buffer holds.
- Check: a scan refuses a table it uses that is empty, a DC table of over
  12 codes, and an AC table of over 162 or with a value that is no run
  and size.
- What the guest sees: kIOReturnBadMedia, as for any stream that does not
  decode.
- Test: `jpeg::tests::huffman_tables_past_what_8_bit_coefficients_take_are_refused`.

**F25. A stream can define Huffman tables its scan does not use, which
F15 does not check.** The parser checks a table's values only for the
tables its scan names: a DC table 1 of category 255 and an AC table 1 of
size 15 parse beside a scan of tables 0 alone (measured with a test of
the parser on the M3 Max, 2026-10-24). Loaded, they would go to the
driver.
- Check: `va/jpeg.rs` loads only the Huffman tables the scan names.
- Test: `va::jpeg::tests::only_the_huffman_tables_the_scan_names_are_loaded`,
  an image written from T.81's syntax with those two tables 1 beside a
  scan of tables 0: its parameters load table 0 alone and leave table 1
  zero. It runs where the vaapi crate's tests run, on Linux.

### AV1

**F16. Film grain points that do not increase reach a driver that divides
by their step.** Mesa's VCN path computes each scaling segment's slope as
`(65536 + (delta_x >> 1)) / delta_x` (`ac_vcn_dec.c`, read in a Mesa
checkout of 2026-10-21, not run), so two equal `point_y_value`s would
divide by zero in QEMU's process: SIGFPE on x86_64, the VM gone. 6.8.20
has the values strictly increase, and 4:2:0 code points for both chroma
components or neither.
- Check: `film_grain_params` (`av1/frame.rs`) refuses a frame header with
  either.
- What the guest sees: -12909 for the frame.
- Test: `av1::tests::film_grain_points_that_do_not_increase_are_refused`.

**F17. A frame OBU's tiles can start past tile 0.** A tile group OBU's
`tg_start` must follow the group before, and a frame OBU carries its own
group: taken from any `tg_start`, a two-tile frame with `tg_start` 1 would
reach the accelerator with tile 1 alone. Mesa numbers an AV1 picture's
tiles as they arrive and hands VCN all 256 of its tile entries, kept from
frame to frame (`picture_av1.c`, `ac_vcn_dec.c`, read in Mesa's source,
not run), so the GPU would read offsets and sizes left by earlier frames,
which may point past the bitstream.
- Check: a frame OBU's tiles start at tile 0 (`av1/decoder.rs`), as 6.10.1
  has a frame OBU's tile group do.
- Test: `av1::tests::a_frame_obus_tiles_start_at_tile_0`.

**F18. A frame of up to 4,096 tiles can reach a driver that holds 256.**
`tile_info()` allows 64 by 64 tiles (`av1/frame.rs`). Mesa's VA-API AV1
decoder keeps 256 tiles (`AV1_MAX_NUM_TILES`, `ac_video_dec.h`), drops the
slices past them, and still gives VCN the frame's 64 by 64 (read in Mesa's
source, not run); what the firmware does then is unknown.
- Bound: a frame header of more than 256 tiles is refused as unsupported
  (`av1::frame::MAX_TILES`, a bound of our own).
- What the guest sees: -12910 (`kVTVideoDecoderUnsupportedDataFormatErr`)
  for the frame.
- Test: `av1::tests::frames_of_more_tiles_than_the_hardware_holds_are_refused`.

**F35. AV1 references can be of any scale, and a temporal unit can show
any number of frames.** Unchecked (measured with tests on the M3 Max,
2026-10-24):
- an inter frame may predict from a reference of any size: a 16x16 inter
  frame against a 64x48 reference reached the accelerator, past
  7.11.3.3's 2:1 down and 16:1 up;
- a `show_existing_frame` may show a key frame any number of times;
- a temporal unit may show a frame any number of times: 1,000 shows in one
  unit gave 1,000 outputs, each a readback for the guest.

None was seen to reach the driver with a value outside its range; the
message bounds bound the shows too.
- Check (`av1/frame.rs`, `av1/decoder.rs`): every one of an inter frame's
  7 references must have `2 * FrameWidth >= RefUpscaledWidth`,
  `2 * FrameHeight >= RefFrameHeight`, `FrameWidth <= 16 * RefUpscaledWidth`
  and `FrameHeight <= 16 * RefFrameHeight`, as dav1d checks all 7 (libaom
  refuses only when none is valid). A key frame is shown again once at
  most. A sample, the temporal unit VideoToolbox hands over, shows one
  frame of each spatial layer at most (7.5), checked before anything of
  the frame changes the decoder.
- Not checked: a frame coded `showable_frame` 0, which 6.8.2 does not let
  a `show_existing_frame` show, is shown, as Apple's host shows it: libaom
  3.15 writes its filtered forward key frames so, and Apple's host shows
  every frame of such a stream equal to libdav1d's (measured, 2026-10-24;
  [devices.md](devices.md), "AV1").
- What the guest sees: -12909 for the frame.
- Tests: `av1::tests::references_past_2_to_1_down_or_16_to_1_up_are_refused`,
  `a_frame_coded_not_showable_is_shown_as_apples_host_shows_it`,
  `a_temporal_unit_shows_one_frame_of_its_layer`.

### HEVC

**F19. A picture's slice segments can be gathered in quadratic time and
without bound.** For each slice segment the decoder needs the last
independent one before it (`hevc/decoder.rs`, `Decoder::decode`), and it
keeps each segment's `SliceHeader`, 248 bytes. Searched for back through
every segment, with no limit on their number, one sample of an IDR slice
and N dependent segments of a few bytes each took 24 s at N = 160,000 in
a release build, and about 62 times its own size in headers (measured on
the M3 Max, 2026-10-24).
- Bound: the last independent segment is kept by its index, and a picture
  of more segments than CTBs is refused: each segment holds a CTU at least
  (7.3.8.1), at an address of its own (7.4.7.1). With the index, the
  160,000 segments took 22 ms.
- Test: `hevc::tests::a_picture_has_no_more_slice_segments_than_ctbs`.

**F20. `SpsMaxLatencyPictures` can overflow 32 bits.**
`Sps::max_latency_pictures` (`hevc/sps.rs`) adds `sps_max_num_reorder_pics`
and `sps_max_latency_increase_plus1 - 1`, and the increase may be up to
2^32 - 2 (7.4.3.2.1): in 32 bits, a panic in a debug build in display
order, and a wrapped latency in release.
- Bound: the sum saturates, as each picture's `PicLatencyCount` does
  (`hevc/decoder.rs`): the two compare as the standard's do until a
  picture has waited 2^32 - 1 pictures.
- Test: `hevc::tests::the_largest_latency_increase_does_not_overflow`.

**F21. Pictures the standard forbids can reach the accelerator, as F8's
can for H.264.** No one element shows these, and each reached the
accelerator when only a picture's PPS and `SliceQpY` were checked
(measured on the M3 Max, 2026-10-24):
- an SPS resent with another size between two slices of a picture, so a
  `slice_segment_address` of 3,000 reached a picture of 12 CTBs;
- slice segments of one picture that disagree on their NAL unit type,
  order count, reference picture sets or `pic_output_flag`, while only the
  first's reach the picture's parameters;
- a slice segment with nothing after its header;
- more references than `sps_max_dec_pic_buffering_minus1`: 15 for a DPB
  of 1, where the surfaces are made for the DPB;
- a reference with the current picture's `PicOrderCntVal`, by a long-term
  picture named by its LSB or by an order count saturated at 2^31 - 1,
  where `DiffPicOrderCnt` 0 is a divisor of motion vector scaling;
- short-term references more than 2^15 from the current picture (8.3.1).
- Check: `hevc/bounds.rs` checks each picture just before
  `Accelerator::decode_picture`, as `h264/bounds.rs` does.
- What the guest sees: -12909 for the frame.
- Tests: `hevc::tests::slice_segments_of_one_picture_keep_its_sps_and_agree`,
  `a_slice_segment_with_no_data_is_refused`,
  `references_past_the_dpb_or_at_the_current_order_count_are_refused`.

**F42. Slice segments of a picture can share an address.** F19 bounds a
picture's segments by its CTBs, and 7.4.7.1 forbids two with one
`slice_segment_address`: unchecked, an IDR picture of segments at
addresses 0, 3 and 3 reached the accelerator (measured on the M3 Max,
2026-10-24). NVDEC reads each segment's header itself from the bitstream
the NVIDIA backend hands it (`nvdec/hevc.rs`), so it would get segments
that cover CTBs twice; what it does with them is unknown, as for H.264's
(F41).
- Check: `hevc/bounds.rs` refuses slice segments that share an address.
- What the guest sees: -12909 for the picture.
- Test: `hevc::tests::slice_segments_at_one_address_never_reach_the_accelerator`.

**F22. Listing a sample's NAL units whole costs 16 bytes a unit before any
is read.** With one-byte length prefixes a zero byte is an empty unit, so
a mebibyte of zeros lists 1,048,576 units, 16 MiB, before the first is
refused (measured, 2026-10-24): a `dfrm` of 128 MiB would ask for 2 GiB.
- Bound: `h264::nal::sample_units`, which both the H.264 and the HEVC
  decoder read a sample with (`h264/decoder.rs`, `hevc/decoder.rs`), checks
  every length prefix first, so a sample with a bad one still yields
  nothing, then hands the units over one at a time, keeping none.
- Test: `h264::nal::tests::a_samples_units_come_one_at_a_time_once_every_prefix_reads`.

**F28. An SPS's block sizes, transform depths and PCM can be inconsistent
with each other.** `Sps::parse` (`hevc/sps.rs`) reads each size within its
own syntax range, which lets through CTBs of 8, transform blocks as large
as the smallest coding block or larger than the CTB or 32, transform
trees deeper than the CTB splits, and PCM samples of more bits than the
picture's, in blocks outside the coding blocks' sizes (7.4.3.2.1, A.3.2).
Unchecked, each reached VA-API's picture parameters as read (measured on
the M3 Max, 2026-10-24); none was seen to fault.
- Check: the parser refuses `MinTbLog2SizeY` not under `MinCbLog2SizeY`,
  `MaxTbLog2SizeY` past `Min(CtbLog2SizeY, 5)`, depths past
  `CtbLog2SizeY - MinTbLog2SizeY`, `PcmBitDepthY` or `PcmBitDepthC` past
  the picture's, and PCM block sizes outside `Min(MinCbLog2SizeY, 5)` to
  `Min(CtbLog2SizeY, 5)`. `check_supported` (`hevc/decoder.rs`) refuses
  CTBs of 8, which the Main profiles do not have (A.3.2 to A.3.4).
- What the guest sees: -12909 for the frame, as for a stream that does
  not decode.
- Tests: `hevc::tests::ranges::sps_block_sizes_depths_and_pcm_past_their_ranges_are_refused`,
  `ctbs_of_8_are_refused_and_16_taken`.

**F29. A PPS's values that its SPS bounds can pass the picture's SPS.**
`Pps::parse` (`hevc/pps.rs`) reads a PPS before its SPS is known, so it
takes `init_qp_minus26` from -74, `diff_cu_qp_delta_depth` to 3 and
`log2_parallel_merge_level_minus2` to 4, the ranges of any SPS, where
7.4.3.3 holds them to the picture's SPS: an 8-bit picture's -27, QP deltas
finer than its smallest coding block, a merge level past its CTB. Tile
columns under 256 luma samples and rows under 64, which the Main profiles
do not have (A.3.2), parse too. Unchecked, each reached VA-API's picture
parameters; none was seen to fault.
- Check: `check_pps_supported` (`hevc/decoder.rs`) takes the picture's SPS
  and refuses each.
- What the guest sees: -12909 for the frame.
- Tests: `hevc::tests::ranges::pps_values_past_what_their_sps_allows_are_refused`,
  `tiles_under_256_luma_samples_across_or_64_down_are_refused`.

**F30. A slice segment's entry points can pass its tiles and CTB rows, and
VA-API's count is 16 bits.** 7.4.7.1 allows a segment one entry point a
tile, a CTB row, or a CTB row of each tile column, but the first, and
`VASliceParameterBufferHEVC` holds the count in 16 bits (`va/hevc.rs`). A
count bounded only by `PicSizeInCtbsY` would be narrowed there, silently.
The hardware reads the entry points from the slice data that follows.
- Bound: `SliceHeader::parse` (`hevc/slice.rs`) holds
  `num_entry_point_offsets` to 7.4.7.1's range, which is under 2^16 for
  any picture: 20 tile columns of 1,056 CTB rows at most.
- What the guest sees: -12909 for the frame.
- Test: `hevc::tests::ranges::entry_points_past_the_tiles_and_rows_are_refused`.

**F31. A slice header's chroma offsets, weight flags and an IRAP picture's
references can pass their ranges.** 7.4.7.1 holds the sum of the PPS's
and the slice's chroma QP offset to -12 to 12, not only each alone;
7.4.7.3 allows 24 weight flags (a luma flag counting 1 and a chroma flag
2, over both lists of a B slice); and 7.4.7.1 has `NumPicTotalCurr` 0 for
a CRA or BLA picture, whose set may not name pictures used by the current
one. Unchecked, a mid-stream CRA reached the accelerator with references
in its current sets; none was seen to fault.
- Check: `SliceHeader::parse` (`hevc/slice.rs`) refuses each.
- What the guest sees: -12909 for the frame.
- Tests: `hevc::tests::ranges::a_cra_or_bla_picture_predicting_from_its_set_is_refused`,
  `chroma_qp_offsets_past_12_together_are_refused`,
  `weight_flags_past_24_are_refused`.

**F32. A scaling list entry can be 0.** `ScalingLists::parse`
(`hevc/scaling.rs`) takes each entry as the sum of the deltas modulo 256,
which can be 0, where 7.4.5 has every entry over 0. The DC values are 1 to
255 by their syntax. The scaling factor of an entry divides nothing in
VA-API's path that was read, and no fault was seen.
- Check: an entry of 0 is refused, in the SPS's lists and the PPS's.
- What the guest sees: -12909 for the frame that brings the set.
- Test: `hevc::tests::ranges::a_scaling_list_entry_of_0_is_refused`.

**F33. Short-term reference picture sets can pass the SPS's DPB.**
`ShortTermRps::parse` (`hevc/rps.rs`) reads a coded set of up to 16
pictures, where 7.4.8 holds `num_negative_pics` and `num_positive_pics`
to `sps_max_dec_pic_buffering_minus1` together, and a predicted set has
one more picture than the set it predicts from, so a chain of the SPS's
64 sets could grow to 80. F21 refuses a picture with more references than
the DPB, after the sets are parsed.
- Bound: every set, the SPS's and a slice header's own, coded or
  predicted, names `sps_max_dec_pic_buffering_minus1` pictures at most, of
  the SPS's highest sub-layer: 7.4.8's bound for a coded set, and for a
  predicted one ours, since its pictures are kept in the DPB beside the
  current one. F21's test passes the DPB with a long-term picture beside
  a set the parser takes, which F21 still counts.
- What the guest sees: -12909 for the frame that brings the set.
- Test: `hevc::tests::ranges::short_term_sets_of_more_pictures_than_the_dpb_holds_are_refused`.

**F34. An SPS that differs in more than its VUI can take over at a picture
that is not IRAP.** A new SPS of pictures alike (`same_pictures`: size,
format and DPB) takes over without a reconfiguration (`hevc/decoder.rs`,
`Decoder::activate`), where within a coded video sequence an SPS may not
change (7.4.3.2.1): at any picture, it would let an SPS with another
`log2_max_pic_order_cnt_lsb_minus4`, or other sets, in at a trailing
picture, as F9 is for H.264. No arithmetic fault follows from it: order
counts are worked in 64 bits.
- Check: at a picture that is not IRAP, a new SPS takes over only when it
  differs from the active one in its VUI alone (`same_but_vui`). An IRAP
  picture still takes one of pictures alike, a CRA in mid-stream among
  them, though the standard has the sequence go on there; a stream
  spliced at a CRA with no end of sequence would have one.
- What the guest sees: -12909 for the picture.
- Test: `hevc::tests::ranges::an_sps_that_differs_in_more_than_its_vui_at_a_non_irap_picture_is_refused`.

### The scaler

**F23. A 4:2:2 destination window listed byte by byte costs 32 bytes a
byte.** `scaler::coverage::covered`, which the scaler's kernel calls to
copy a staged destination back, lists the bytes a window writes as runs.
A 32-byte `Run` for each byte a `yuvs` window writes would make 4,147,200
runs, 128 MiB, for a 1920x1080 window, and ask 16 GiB for a 16384x16384
one (measured with a test, and arithmetic, on the M3 Max, 2026-10-24).
- Bound: pairs inside the window are written whole, so a row is one run
  but for a partial pair at either end; the bytes covered are the same.
- Test: `scaler::coverage::tests::a_422_window_is_a_few_runs_a_row_over_the_same_bytes`.

**The kernel** (`crates/reims-vgpu/src/scaler/gpu.rs`, `gpu/params.rs`),
read for what a guest controls (2026-10-25): its surfaces' layouts, their
windows, the flags, and which surfaces it names.
- Sizes: a side outside 4 to 65536 is refused before anything is made,
  and the plan refuses ratios past 8, 8, 8 and 4, so a destination pixel
  takes at most 9 source pixels along an axis.
- Arithmetic: each plane's offset, pitch and extent, and every word,
  row and byte a dispatch covers, are worked out with checked arithmetic
  from the plan, and a plane whose bytes lie past the kernel's 32-bit
  offsets is kIOReturnUnsupported (`TooLarge`). A surface bound in place
  must hold its planes (`PlaneSpan::fits` against the bytes the host lends),
  and a staged one's copies are bounded by its planes and the window's
  coverage.
- Writes: the kernel writes the destination's words its window covers
  and no others; a word partly covered is read and written back whole,
  the bytes outside the window as they were. A surface the guest made
  read-only for its clients is never bound for writing (its write lend
  refuses, kIOReturnNotWritable), and a destination that is the source's
  own memory is staged.
- Lends and waits: no GPU work runs inside a lend; the wait for the GPU
  ends at core's deadline, past which what it may still write is kept
  with the device's hold on both surfaces, and the accelerator refuses
  every later transform.
- Tests: `scaler::gpu::params::tests::a_plane_past_the_kernels_offsets_is_too_large`,
  `scaler::gpu::tests::a_side_outside_apples_is_refused`,
  `a_misaligned_destination_is_staged_and_the_same` and
  `platform_surfaces_are_reached_in_place`, in reims-vgpu.

### The VideoToolbox backend

What a guest can reach in `crates/reims-vgpu-video-videotoolbox`, each
finding measured with a probe of the backend on the M3 Max (macOS 27.0.1),
2026-10-24.

**F38. A sample buffer of several samples decodes to as many frames, all
held at once.** `VTDecompressionSessionDecodeFrame` decodes each sample
of a sample buffer as a frame of its own, calling back for each before it
returns. Given the guest's sample sizes, a `dfrm` whose sample buffer held
N copies of a key frame made N frames, each a pixel buffer VideoToolbox
allocated and the backend kept until the call returned, then N emits,
each waiting on the guest's `Dbuf`. 1,000 copies of a 473-byte 1080p
H.264 key frame took 1.5 s and held 3.1 GB of the host's free memory
(`vm_stat`), none of it in the process's own footprint, since the frames
are IOSurfaces VideoToolbox's decoder service makes; a `dfrm` of 128 MiB
holds 280,000 such copies. The guest's own VideoToolbox sends one sample
a `dfrm`.
- Bound: `sample_buffer` (`macos/decoder.rs`) gives VideoToolbox the
  bytes as one sample, whatever sizes the guest's sample buffer lists, as
  Apple's host hands its decoder the guest's sample buffer as one frame
  (read from Apple's code), for which a decoder emits once, and as the
  VA-API backend reads a sample. The 1,000 copies as one sample were
  refused with -12909 in 4 ms, and 100,000 copies of a 4K HEVC key frame
  with -12911 in 25 ms.
- What the guest sees: one emit for the `dfrm`, -12909 with no image for
  those copies. What Apple's AVD makes of a guest's sample buffer of
  several samples is unknown.
- Test: `macos::tests::a_sample_buffer_of_several_samples_decodes_to_one_frame_at_most`.

**F39. A property value of another type than the encoder gives its key can
end the process.** `ExpectedFrameRate` set to a string threw an uncaught
NSInvalidArgumentException (`-[NSTaggedPointerString _getValue:forType:]`)
in `VTCompressionSessionSetProperty` on both hardware encoders, and so did
an array, a dictionary and data on H.264's: the process ended. No guest
reaches it, since core's property table types each value before the
backend sees it (`host/encoder/properties.rs`, `set`), but the backend
does not rest on that.
- Check: `PropertyCheck::check` (`macos/encoder/properties.rs`) holds a
  guest's property to the keys and value lists of the encoder's
  supported-property dictionary, and to the `PropertyType` it gives each
  key: a value not of its key's type (Boolean, Number or Enumeration) is
  refused with -12900 before the session sees it; a key listed with no
  type, as `DataRateLimits` is, takes any value.
- What the guest sees: nothing new, since core refuses such a value with
  -12900 first.
- Tests: `macos::encoder::properties::tests::a_value_of_another_type_than_the_encoder_gives_is_refused`
  and `macos::tests::a_value_of_another_type_than_the_encoder_lists_is_refused`,
  on the M3 Max's encoders' own lists.
- Checked and sound: every value of the types core passes was taken or
  refused with a status, on both hardware encoders, set at the open and
  then cleared with NULL on the running session (-12902 for every key):
  NaN, both infinities, ±1e308, 1e-308, the ends of 64 bits and 2^64 for
  each number; `DataRateLimits` of 0 to 65,000 numbers, with zeros, NaNs,
  infinities and negatives; every enumeration value core lists.

**F40. Frames a VideoToolbox encoder drops must still be emitted.**
VideoToolbox drops every frame of a session one pixel wide or tall: each
encode answers 0, and its callback status 0, `kVTEncodeInfo_FrameDropped`
and no sample, on both hardware encoders (1x1 to 3x1, 1x2, 1x3, 4096x1,
1x4096, 8192x1 and 1x8192 dropped; 2x2 and up encoded). A frame that is
never emitted stays pending, with its surface held, until the guest's
`ecfr` answers it -12912 (`host/encoder.rs`, `complete_held_frames`): a
guest whose encoder drops every frame adds a pending frame and a hold
with each `efrm`, without end until it completes, and a client there
never hears of its frames. Apple's host forwards a dropped frame's emit,
with its flags and no sample (read from Apple's code).
- Bound: a backend gives a dropped frame back as an `EncodedFrame` with
  no data (`EncodedFrame::dropped`, `encoder_backend.rs`), which core
  emits at once, with `,efl` `kVTEncodeInfo_FrameDropped` and no sample or
  format description, and lets its surface go; the VideoToolbox backend's
  callback (`macos/encoder.rs`, `encoded`) gives each frame its encoder
  drops so. And `esta` of a side of 1, which rounds to no even size to
  encode, is answered -12902 before the backend is asked
  (`host/encoder.rs`, `start_session`), so no session one pixel wide
  reaches VideoToolbox.
- What the guest sees: an `Eemt` of status 0 flagged dropped for each
  frame the encoder drops, as Apple's host sends for a frame its encoder
  drops. Whether AVE takes a session one pixel wide at all is unknown.
- Tests: `host::encoder::tests::a_frame_the_encoder_drops_is_emitted_as_dropped_with_no_sample`
  in core, and `macos::tests::frames_the_encoder_drops_come_back_as_dropped`.

Checked and sound, by reading and with the same probe:
- Lifetimes: the decoder is synchronous, each decode waiting for its
  frames before it returns; an encoder's `VTCompressionSessionInvalidate`
  waits for callbacks in flight, so none reaches freed state (a test whose
  callback slept); each CF object the backend makes is released once.
- Queues: an encoder has about 10 frames in VideoToolbox's hands at once
  (200 frames sent at 1080p H.264 without waiting, 190 back before the
  flush; 100 at 2160p HEVC, 90), so nothing grows with the guest's rate.
- Times: decodes and encodes at times of value 0, the ends of 64 bits,
  timescales of 0 and -1, and odd flags and epochs each decoded and
  encoded.
- Guest bytes: 4,800 sessions of mutated configuration records and
  samples, 1,600 each of H.264, HEVC and AV1, were each refused with a
  status or decoded, with no crash; 800 of VP9 were all refused at the
  open.
- Statuses: what the guest gets is VideoToolbox's status, with no text
  of the host's.
- The private capabilities dictionary is read with each value's type
  checked before it is used.
- reims-vgpu's Metal platform's lends of a guest surface were sound.

Open ([bugs.md](bugs.md)): B31, VideoToolbox's hardware encoder sessions,
the whole Mac's 32.

### The NVIDIA backend

What a guest can reach in `crates/reims-vgpu-video-nvidia`, read file by
file (2026-10-24): each guest value followed from core to the CUDA, NVDEC
or NVENC call, and the codec crate's paths into NVDEC's picture
parameters read again for what NVDEC, which parses each slice header
itself, is handed. Probes ran on an RTX 5080 Laptop GPU (driver
615.71.09) with valid streams and parameters only: nothing fuzzed or
invalid went to its GPU.

Its two findings are the codec crate's, F41 (H.264) and F42 (HEVC):
slices no conforming stream has, which NVDEC would be handed whole. The
NVIDIA crate itself needed no fix. Checked and sound:
- The binding: `layout`'s test (the crate's `layout.rs`) passed against
  ffnvcodec 13.1.15.0's headers on the RTX 5080 Laptop GPU's host: every
  struct's size and offsets, every bit-field, constant and GUID, and the
  signature of every function, NVENC's function list in its order
  included. A bit-field is written cut to its width
  (`nvdec::ffi::bits::Bits::put`), so no value spills into the next.
- Surfaces: every surface index NVDEC gets is one of the decoder's own,
  from its free list, or its "none" (-1, 0xff). H.264's 16 DPB entries and
  HEVC's 16 `RefPicIdx` are within F8's and F21's reference bounds; HEVC's
  current sets past 8 are refused (`nvdec/hevc.rs`, `current_set`); HEVC's
  20 tile columns and 22 rows at most fit NVDEC's 21 explicit sizes, the
  last being implied; AV1's 256 tiles at most (F18) have offsets within
  their tile groups, and its film grain 14 and 10 points; VP9's two header
  sizes are within the frame (all the codec crate's).
- Sizes: a VP9 or AV1 frame larger than its decoder's surfaces is refused
  before it takes one; a decoder is retargeted only to a frame's own size,
  never past its own. The copy out is cut to the mapped frame, the surface
  pitch and the guest's plane (`nvdec/decoder.rs`, `copy_plane`), so it
  reads nothing past the mapped frame and writes nothing past the plane.
- Pools: a decoder's surfaces are fixed when it is made, the DPB and 3, 32
  at most, and a decode that finds none free is refused; NVENC's buffers
  are the B pictures and 4. Nothing grows with a guest's sizes. What they
  cost is B14's: one 8K HEVC Main 10 decoder held 1,020 MiB.
- Lifetimes: a picture holds its decoder and the decoder its GPU, so a
  frame is copied after its decoder was replaced; calls on a decoder are
  made one at a time under its lock, with the context pushed; there are no
  NVIDIA callbacks, as no NVIDIA parser is used; the libraries are never
  unloaded. NVENC runs synchronously, and every lock of an input or
  bitstream buffer is unlocked on every path; an encoder dropped with B
  pictures pending flushes first (measured: dropped in 95 ms).
- Encoder values: rates are floored at 32,000 bit/s (the crate's
  `encode.rs`, `MIN_BITS_PER_SECOND`), frame rates are within 1/1000 and
  1000 a second (the codec crate's `frame_rate_of`), and sizes, B
  pictures and presets within NVENC's caps; the display aspect NVENC
  derives the sample aspect from is the frame's own, since nothing of the
  guest's sets `sample_aspect_ratio`, so no 0 reaches it.
- Statuses: the guest gets core's codes, and the driver's text stays in
  the host's log.

Open ([bugs.md](bugs.md)): B39, NVENC's sessions are the GPU's, 12 on the
RTX 5080 Laptop GPU, which one guest can take whole; B40, VP9 blocks that
may predict from a reference of any size, on either backend.

### The VA-API backend

What a guest can reach in `crates/reims-vgpu-video-vaapi`, read file by
file (2026-10-25): each guest and stream value followed to the libva call
it reaches, and each VA object from its making to its destroy. The tool
checks ran on a Radeon RX 7900 XTX and a Radeon 780M (Mesa 26.2.4, libva
2.24.1), with every decode bit-exact.

The guest sees nothing new from it: each fix refuses with a status what
no sizes core admits reach, or moves where the host's own work runs.
Checked and sound:
- The binding: the six ignored layout tests (`va::ffi`'s,
  `va::vpp_ffi`'s, and the AV1, VP9, JPEG and encoder bindings') compile
  against libva's headers and compare every struct's size and offsets,
  and the values of 140 of the constants the crate names, drm_fourcc.h's
  among them.
- Lifetimes: each VA object is held by a value that destroys it once, as
  it drops: a buffer, an image (`va/image.rs`) and the mapping that
  borrows it, a context, which holds its config, a surface. A picture
  begun is ended on every path (`va/buffer.rs`, `render_picture`), and the
  encoder's coded buffer is destroyed before its context. No Rust callback
  is registered with libva, so none unwinds through it.
- Arithmetic: every offset, pitch and size the copies derive from a
  guest's or a stream's sizes is checked, and one past the address space
  is refused rather than wrapped: the read back's row copy
  (`va/readback.rs`), the surfaces' plane copies (`va/surface.rs`) and the
  encoder's upload (`va/encoder/surfaces.rs`).
- Driver lists: a query into no room asks the driver nothing, and the
  encoder reads 64 coded segments a frame at most.
- Waits: each wait for the GPU, the poll before a run's sync among them,
  ends at core's deadline, past which the frame fails and what the GPU
  may still write is kept (`va/wait.rs`; [vaapi.md](vaapi.md), "Waiting
  for the GPU"). The driver calls measured to wait inside radeonsi are
  made on the display's call thread, and every other call waits for them
  by its own deadline, not in the driver (`va/calls.rs`; [vaapi.md](vaapi.md),
  "The display's calls"); a release behind them is ordered there, so
  nothing a call left in the driver uses is destroyed before it returns.

Open ([bugs.md](bugs.md)): B78, the calls measured not to wait on radeonsi
have no bound of their own on another driver; B90, where the JPEG engine
cannot write host memory, its maps hold the display's lock for
milliseconds; B92, after any memory of the process's GPU is freed, its
next VA-API work waits for every job then on the GPU, a new decoder's
first `vaEndPicture` on the CPU, with the display's lock held, on the
display's call thread by the frame's deadline.

### Zero copy

What a guest controls on the zero-copy path ([core.md](core.md), "Zero
copy"), and what bounds it, read from the code; no fuzzed or invalid
input went to a GPU. A guest controls a buffer's layout (its pixel
format, size, planes' offsets and rows, through its IOSurface's
properties), which buffer it lends with `Dbuf`, when it unmaps it, and
whether it made it read-only for its clients. Checked and sound:
- **Where a device writes.** Before any write, `GuestTarget::new`
  (`device_memory.rs`) checks the layout against the memory the host
  lends: two planes of a format core decodes into, each row long enough
  for the frame's samples, each plane's whole rows, at its pitch, ending
  inside the memory, with checked arithmetic, and the chroma plane after
  the luma's end. A layout past its memory is refused and the frame copied
  by the CPU, whose own split refuses the same. VA-API's import names
  exactly those planes and the dma-buf's size, and its run's region is the
  frame cut to the buffer; NVIDIA's copies get each plane's start and the
  bytes to the memory's end, and write `rows` rows of `row_bytes`. Core's
  `host::decoder::tests::a_layout_past_its_memory_is_never_given_to_a_device`,
  `device_memory::tests::a_target_past_its_memory_or_out_of_shape_is_refused`,
  and the host fuzzer's device, which asserts every byte it writes, hold
  it.
- **When it writes.** Only while the session holds the buffer, between
  its `Dbuf` and its `Demt`, as the CPU's copy does. The guest's unmap of
  a held surface answers Busy, and a map of a mapped ID changes nothing
  (`crates/reims-vgpu-core/src/iosurface/device.rs`,
  `a_held_surface_keeps_its_pages`), so the pages under a write cannot
  change. The write is synchronous: when it returns the device is done
  with the buffer.
- **What an import outlives.** Imports and registrations are kept by the
  mapping's identity, which no other mapping, earlier or later, ever has.
  Each is dropped when its mapping goes, on a thread of its own, so a
  guest that unmaps and maps again gets new imports, never a stale one;
  NVIDIA's registration is undone before the hold that keeps its pages
  mapped is let go, so CUDA never holds pages the host has unmapped. A
  guest cycling mappings fast makes imports at most once a mapping, at
  most 96 kept, the oldest dropped first (`va/zero_copy.rs`,
  `MOST_IMPORTS`; the NVIDIA crate's `zero_copy.rs`,
  `MOST_REGISTRATIONS`).
- **Read-only and protected surfaces** are never lent to a device
  (reims-vgpu's `lend_to_devices`, in
  `crates/reims-vgpu-metal/src/os/device_lend.rs` and
  `crates/reims-vgpu-vulkan/src/os/linux/device_lend.rs`; core's
  `host::decoder::tests::a_read_only_buffer_is_never_lent_to_a_device`).
- **No driver call inside a lend or under reims-vgpu's locks**: the
  device's write runs where the read back does, outside every lend.
- **udmabuf's limits.** The host refuses a dma-buf past them before it
  asks the kernel, and the VA-API backend then imports the buffer in more
  strips of rows, each a VA surface over two dma-bufs, up to 64 strips;
  past 64 the refusal is a decline, and the frame is copied by the CPU. A
  guest cannot make a lend larger than its own buffer, which the host's
  maximum frame size bounds, so an import holds at most 64 surfaces and
  128 dma-buf descriptors, and a backend's 96 imports 12,288 of them; at
  udmabuf's stock limits an import holds at most 18 surfaces and 36
  descriptors (8192x4352 P010), and a 4K one at most four and eight. QEMU
  raises its open-file limit to the hard limit as it starts
  (`os_setup_limits`, **read** in `vendor/qemu/os-posix.c`). The strips'
  rows are counted with checked arithmetic, every strip at least two rows
  and inside the layout `GuestTarget` checked.
- **A driver that answers success and writes nothing** (U16) shows the
  guest wrong frames, not the host harm: only drivers measured right get
  these writes.

### Zero copy on VideoToolbox

The VideoToolbox backend's zero copy ([videotoolbox.md](videotoolbox.md),
"Zero copy"; [core.md](core.md), "Zero copy"). A guest controls there
what it controls on the other backends, and one thing more matters:
whether a buffer it lends is read-only for its clients. On the M3 Max
(macOS 27.0.1) the kernel panicked when Apple's scaler wrote into an
IOSurface over pages read-only for its clients (`dart-scaler0... write
protect exception`, measured twice, 2026-10-24), so what keeps such a
buffer from every device comes first. The checks below were run with
stand-ins and ordinary IOSurfaces of their own memory; nothing else went
to the hardware (2026-10-25).
- **A read-only buffer never reaches VideoToolbox: four checks, each
  alone enough.** reims-vgpu lends no device a protected surface, or one
  the guest made read-only for its clients, over its pages or its own
  memory (`crates/reims-vgpu-metal/src/os/device_lend.rs`,
  `lend_to_devices`; its test `read_only_and_protected_surfaces_are_not_lent`,
  and `crates/reims-vgpu/src/platform/metal.rs`'s
  `a_surface_is_lent_to_a_device_as_its_own_iosurface`, in which a surface
  of `kIOSurfaceClientDirection` 2 is refused). The backend then checks,
  before every write and before it reads anything of the surface, the
  host's word asked again (`LentIoSurface::writable`, which reims-vgpu
  answers from its record of the surface at that moment,
  `a_lend_asks_again_before_each_write`); then the IOSurface's base
  address; then the OS's protection of every page of the process's
  mapping of the buffer (`macos/zero_copy.rs`, `mach_vm_region`), a check
  that rests on nothing of the host's. A surface over pages read-only for
  its clients is mapped read-only
  (`crates/reims-vgpu-metal/src/os/surface_bytes.rs`), so the last check
  refuses it even were the host's record wrong.
- **The tests show it with stand-ins that count.**
  `macos/zero_copy/tests.rs`'s
  `a_buffer_the_host_would_not_write_never_reaches_videotoolbox`,
  `a_protected_buffer_never_reaches_videotoolbox` and
  `a_read_only_mapping_never_reaches_videotoolbox` (pages the test made
  read-only, wholly and in their last 16 KiB page alone) give the writer a
  destination that counts each pixel buffer asked of it and a transfer
  that counts each call and fails, and each asserts both counts 0 and the
  frame declined. Core's stand-in devices and the host fuzzer's lend their
  memory as an IOSurface on macOS too, writable only where the stand-in's
  memory is, and assert a device is never handed one it would not write.
- **Where a transfer writes.** Only into the planes `GuestTarget` checked
  against the memory, as on the other backends, and only where the
  IOSurface's planes are those planes, every plane's rows end inside the
  memory, the IOSurface holds at least the memory's bytes, and the pixel
  buffer CoreVideo makes over it has the same planes, rows and size; the
  frame must be the buffer's whole size and format, so a transfer neither
  scales nor converts nor writes past a plane
  (`a_buffer_not_of_the_frame_or_its_layout_never_reaches_videotoolbox`).
  `a_transfer_writes_what_the_cpu_copies` checks, on the hardware, every
  byte outside the planes' samples unchanged.
- **When it writes.** As on the other backends, only while the session
  holds the buffer, and synchronously: the transfer has returned before
  the write does.
- **What it keeps.** Nothing past the write: the pixel buffer over the
  IOSurface, which retains it, is released before the write returns, so a
  guest's unmap after its `rels` is never answered Busy for a reference of
  ours (`an_ordinary_writable_surface_reaches_the_transfer_once` checks the
  retain count is back). The lend's own reference is the hold's, and goes
  with it. The identity reims-vgpu attaches to a surface is dropped when a
  surface made later takes its `IOSurfaceID`, so a reused ID is a new
  mapping (`an_attachment_is_one_value_until_its_id_is_made_again`).
- **Writable surfaces over the guest's pages are lent**
  (`CLIENT_MEMORY_LENT`, true, in `device_lend.rs`), with no transfer into
  one yet measured on the M3 Max's hardware ([bugs.md](bugs.md), D22);
  set false, it lends none. The refusals above stand: a protected surface
  and one read-only for its clients reach no device, and each write asks
  again.

### Found by the fuzzers

Each fuzz target and its runs are in "Fuzzing" below.

**F26. A refused configuration can leave a decoder taking its accelerator
to be configured for the stream before it.** VA-API's accelerators drop
their session before they make the next (`va/h264.rs`, `va/hevc.rs`,
`va/vp9.rs`, `configure`). A decoder that recorded the configuration it
asked for (the active SPS, VP9's `configured`) once the call returned,
and kept the one before when it failed, would, after a refused one, as
for a key frame past the driver's largest size, take the accelerator to
be configured for the old stream and never ask again: every later picture
of the old size and format would fail in `new_picture`, until one of
another came. The VP9 fuzzer reached it (the M3 Max, 2026-10-24); H.264
and HEVC had the same shape, by reading. AV1 records its configuration
only after it succeeds.
- Bound: the H.264, HEVC and VP9 decoders forget their configuration
  before they ask for the next.
- What the guest sees: after a refused key frame, frames of the old
  stream decode again from its next key frame.
- Tests: `h264::tests::fuzz::a_refused_configuration_is_asked_for_again`,
  and the same in `hevc` and `vp9`.

**F27. H.264's frame number gap can overflow across a new configuration.**
A picture that activates a new SPS need not be an IDR: the decoder starts
at an I picture, and after a refused configuration (F26) none is active,
so the next picture configures whatever SPS it names. Were the DPB cleared
there but the last reference's `frame_num` and the order count state
kept, both of the old SPS, `fill_frame_num_gap` would compute the gap as
`frame_num + MaxFrameNum - prev - 1`, which overflows where the old frame
number is past the new MaxFrameNum (frame 20 under MaxFrameNum 256, then
a frame of an SPS with MaxFrameNum 16): a panic in a debug build, and in a
release build a gap of frames that were never there. The long run of
h264-streams reached it (the M3 Max, 2026-10-24).
- Bound: activating a new SPS resets the order count and the last frame
  number with the DPB, as at the stream's start (`h264/decoder.rs`,
  `activate`); one that takes over without a new configuration resets
  them too where it numbers frames otherwise (F45).
- What the guest sees: the picture decodes as the first of a stream.
- Test: `h264::tests::fuzz::frame_numbers_start_afresh_with_a_new_configuration`.
  HEVC computes its order count in 64 bits and masks the last one with the
  new SPS's MaxPicOrderCntLsb, so it has no such gap (`hevc/decoder.rs`,
  `order_count`, read).

**F36. H.264's order count can overflow after operation 5 at the ends of
32 bits.** After `memory_management_control_operation` 5,
`prevPicOrderCntLsb` is the picture's `TopFieldOrderCnt` less the smaller
of its counts (8.2.1), which F10's wrapping leaves anywhere in 32 bits,
near -2^31 for a bottom count that wrapped. Type 0's next count compares
`pic_order_cnt_lsb - prevPicOrderCntLsb` against half of
MaxPicOrderCntLsb, which in 32 bits overflows: a panic in a debug build,
and in a release build a wrapped difference that picks the wrong `msb`.
The long run of h264-streams reached it (on the Linux host's CPU,
2026-10-24), as F10's test does with one more picture.
- Bound: the difference is taken in 64 bits (`h264/poc.rs`, `type0`); the
  count itself still wraps, as F10's does.
- What the guest sees: the picture decodes, in the order its counts give.
- Test: `h264::tests::fuzz::an_order_count_after_operation_5_at_the_ends_of_32_bits_does_not_overflow`.

**F37. Arithmetic on a `jdec`'s largest sides can overflow.** A `jdec`'s
`Width` and `Height` are the guest's, up to 2^32 - 1 each: a region's area
times the blocks an MCU holds passes 2^64 for sides near 2^32, a panic in
a debug build, and in a release build a wrapped area. The long run of
host-session reached it (the M3 Max, 2026-10-24).
- Bound: core holds a decode to no least area, and sends a decode of any
  area to the backend ([devices.md](devices.md), "Where we differ from
  Apple"). The region's arithmetic is checked (`host/jpeg.rs`, `region`),
  and no destination surface holds a decode that large.
- What the guest sees: kIOReturnBadArgument.
- Test: `host::jpeg::tests::options_of_the_largest_sides_are_refused_without_overflow`.

**F43. Core's avcC reader can overflow on a scaling list's delta.** Core
reads an `avcC` record's first SPS as far as `frame_mbs_only_flag`, for
Apple's answer to fields at `dsta` (`host/decoder/avc_record.rs`,
`fields_allowed`), and passes a High SPS's scaling lists on the way by
adding each `delta_scale`, a guest's `se(v)` of up to 2^31 - 1, to the
last scale: in 32 bits, a panic in a debug build, and in a release build a
wrapped scale that misreads the rest of the SPS. The host-session fuzzer
reached it once it sent SPSs of its own writing (the M3 Max, 2026-10-24).
- Bound: a delta outside -128..=127 (7.4.2.1.1) makes the SPS unreadable
  to core, which leaves the record to the backend, as the codec crate
  refuses it.
- What the guest sees: the backend's refusal.
- Test: `host::decoder::avc_record::tests::a_scaling_list_delta_past_its_range_says_nothing`.

**F45. H.264's frame number gap can overflow after an SPS of alike frames
takes over.** An SPS whose frames are alike (`same_frames`: the size, the
format, the profile and the DPB) takes over at an IDR picture without a
new configuration, and its MaxFrameNum with it; the IDR picture's own
decode then starts the frame numbers and order counts again. Where the
picture failed after its SPS took over, as one of SI slices does, which
the decoder does not take, the last reference's `frame_num` of the old
SPS stayed, and the next picture's gap (F27's) was computed from it: an
overflow where it was past the new MaxFrameNum (frame 1845 under
MaxFrameNum 2048, then frame 1 under 1024), a panic in a debug build, and
in a release build a gap of frames that were never there. F27's reset is
on the path of a new configuration only. The long runs of h264-streams
reached it three times, from different seeds (on an AMD Ryzen 7 7840HS at
512 KiB stacks and an Intel Core Ultra 9 275HX, 2026-10-25).
- Bound: an SPS of alike frames that numbers them otherwise resets the
  order count and the last frame number as it takes over
  (`h264/decoder.rs`, `activate`), as the IDR picture's decode does; one
  that changes only its VUI goes on from the active SPS's, as within a
  sequence.
- What the guest sees: the pictures after the failed one decode as after
  any refused picture, from the next of I slices alone.
- Test: `h264::tests::fuzz::frame_numbers_start_afresh_with_an_sps_of_alike_frames`.
  HEVC's SPS of alike pictures takes over at an IRAP picture the same way
  and keeps its last order count, which it computes in 64 bits and masks
  with the new SPS's MaxPicOrderCntLsb (`hevc/decoder.rs`, `activate` and
  `order_count`, read): no overflow there.

## Bounds of our own

| what | bound | where |
|---|---|---|
| values one property list, or one sample buffer with its leaves, decodes to | 65,536 | `property_list::MAX_VALUES`, `Budget` |
| nesting of a property list | 64 | `property_list::MAX_DEPTH` |
| string and data bytes a property list, or one sample buffer's leaves, decode to, beyond its length | 16 MiB | `property_list::MAX_EXTRA_BYTES`, `Budget` |
| nesting of a sample buffer's values | 32 | `sample_buffer::MAX_NESTING` |
| a message to the guest | 0x4000 bytes, Apple's buffer | `host::GUEST_BUFFER_SIZE` |
| one element of the scaler device, its readable part | 64 KiB | `scaler::MAX_REQUEST_SIZE` |
| one element of the VideoToolbox device, its readable part | 16 MiB | `reims_vgpu::MAX_MESSAGE_SIZE` |
| mapping IDs in one `rels` | 2,047 | `host::MAX_RELEASES_PER_MESSAGE` |
| decoder, encoder and motion-estimation sessions of one host session | 16 each | `host::MAX_SESSIONS` |
| guest messages a host session holds, queued or as fragments | 512 MiB and 4,096 messages | `host::MAX_HELD_BYTES`, `MAX_HELD_MESSAGES` |
| guest messages one session alone holds | 256 MiB and 1,024 messages | `host::MAX_HELD_BYTES_PER_SESSION`, `MAX_HELD_MESSAGES_PER_SESSION` |
| one `dfrm`, gathered from its fragments | 128 MiB | `host::MAX_FRAME_MESSAGE_BYTES` |
| one reply from the guest, gathered | 16 MiB | `host::MAX_REPLY_BYTES` |
| a session's end, waiting for its thread to finish the message in hand and then to end | 10 s for both; a reset 10 s in each of its two phases for all its sessions: past it the thread is left running, detached | `host::session_thread::TEARDOWN_WAIT` |
| session threads left running, detached, in the process | 4: past it no session is made, and a `jdec` answers kIOReturnBusy | `host::session_thread::MAX_DETACHED_THREADS` |
| tiles of one AV1 frame | 256, of the syntax's 4,096 | `av1::frame::MAX_TILES` |
| pixel buffers one motion-estimation session remembers | 256: past it the session cleans up first, sending `Mdpb` sooner than Apple's, and a frame it still has no room for is malformed | `host::motion_estimation::MAX_PIXEL_BUFFERS` |
| a backend's blocks read for one cell of Apple's vector grid | 4 by 4, sampled evenly | `motion::VOTES_PER_SIDE` |
| frames a motion-estimation session keeps between `mepf`s | one, the current frame `,mef` 1 asks for, as Apple's | `host/motion_estimation.rs`, `kept` |

The guest's own property lists are dictionaries of a few dozen values,
written by CoreFoundation's binary writer, which does not share
containers.

A guest's input drives recursion in two places, both bounded: reading a
sample buffer's values, 32 levels (`MAX_NESTING`), and a property list's
value, 64 levels (`MAX_DEPTH`), which dropping, cloning and printing it
walk, as VideoToolbox's `cf::from_plist` does (read, 2026-10-25: no other
reader of guest input in core, the codec crate, the backends or the glue
recurses on it). A session's thread in the library has a platform
worker's 512 KiB of stack. In a debug build on the M3 Max (2026-10-25), a
sample buffer 32 levels deep read on a thread of 128 KiB and overflowed
one of 96 KiB; in a release build the deepest sample buffer and the
deepest property list each read on 16 KiB.

A motion-estimation picture is held to Apple's sizes, sides of 2 to 16384
and 16384x8192 at most (`motion::MAX_SIDE`, `MAX_AREA`; measured,
[devices.md](devices.md), "The motion-estimation session"), and to the
backend's. A guest's processor lends two or three pixel buffers at a
time, and only malformed `mepf`s, which get no clean-up, fill the table.
One that names a surface still lent under 256 UUIDs fills it with live
buffers, and its session takes no frame until a `mest` or `minv` lets the
kept frame go.

## Open findings

Found and not fixed; each is in [bugs.md](bugs.md) with its evidence:
- B14: a host session has no VRAM budget, as Apple's host has none: the 16
  decoder sessions of F7 can each hold a DPB of driver-maximum surfaces;
- B31: VideoToolbox's hardware encoder sessions are the whole Mac's, 32 in
  all;
- B39: NVENC's sessions are the GPU's, 12 on an RTX 5080 Laptop GPU;
- B40: a VP9 block may predict from a reference of any size.

## Fuzzing

Every reader of guest input has a seeded fuzzer of our own. Core's are its
`fuzz` test binary (`crates/reims-vgpu-video-core/tests/fuzz/`); the codec
crate's are a `fuzz` module beside each codec's tests
(`crates/reims-vgpu-video-codec/src/{h264,hevc,av1,vp9,jpeg}/tests/fuzz.rs`,
and `src/encode/session/fuzz.rs`). Each target is a plain `#[test]`, its
quick run, which `cargo xtask check` runs with every other test, and a
long run of the same name with `_long`, behind `#[ignore]`. They need no
fuzzing crate and no nightly compiler. They run in the debug test build,
whose overflow checks are part of the point, since a release build wraps
where a debug one panics (`overflow-checks` in the workspace's
`Cargo.toml`).

### The harness

`crates/reims-vgpu-video-core/tests/fuzz/support.rs`, ours, which the
codec crate's tests include by path (`src/lib.rs`, `fuzz_support`):
- **Seeds.** A SplitMix64 generator. Input `i` of a target is made from
  a seed of its own, a mix of the run's seed, the target's name and `i`,
  so one input replays alone.
- **Mutators.** Flip a bit, set a byte or a big-endian u32 to a boundary
  value, insert, delete, duplicate or splice a range, or truncate.
- **Memory.** A counting global allocator: the peak of the thread that
  runs the input, or of the whole process for a target with threads of
  its own (host-session and motion-sessions), which then runs alone among
  the binary's fuzzers.
- **Bounds.** Each input is held to 2 s of CPU time and to 256 MiB
  beyond what was live before it (`Target::new`), and its panics are
  caught, on every thread for a target counted over the process. The time
  is the CPU time of the thread that runs the input, or of the whole
  process for a target counted over it (`clock_gettime`'s
  `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_PROCESS_CPUTIME_ID`), so a host
  loaded with other work does not fail an input that the wall clock
  would.
- **Aborts.** A stack overflow ends the process, with no panic to catch:
  Rust's handler names the thread and aborts, and the harness's SIGABRT
  handler then prints the input in hand: its target, seed and index, of
  the last input to start where targets run side by side.

A failure stops the run and prints the seed, the input's index and the
command that replays it, as F36's did:

```
fuzz h264-streams: input 178615 of seed 0xbc7b0e78348b5a00 panicked: attempt to subtract with overflow
replay: RVGV_FUZZ_SEED=0xbc7b0e78348b5a00 RVGV_FUZZ_INDEX=178615 cargo test --offline -p reims-vgpu-video-codec -- --exact h264::tests::fuzz::streams_long --include-ignored
```

A finding's regression test is then written with the tests' own writers,
from what the input did, never from captured bytes: F26's, F27's, F36's
and F37's tests.

### The targets

| target | test | what it reads | what each input must do, beyond the bounds |
|---|---|---|---|
| property-lists | core, `wire::property_lists` | binary, XML and ASCII plists, written and mutated, and containers nested in text, most often up to 80 deep and now and then up to 30,000, closed, left open or broken off | decode within F1's bounds or be refused; what decodes encodes and decodes again; a text plist of 64 KiB and 300 `(`, `{` and `<` at most decodes as the plist crate's builder makes it, within the bounds |
| messages | core, `wire::messages` | messages, and every key the host reads as every type | read or be refused |
| fragments | core, `wire::fragments` | sample buffers cut as the guest cuts a `dfrm` | join back to the whole; mutated, read or be refused |
| sample-buffers | core, `wire::sample_buffers` | sample buffers, written and mutated | read or be refused; one that reads holds no more values than F50's bound, and is written again |
| image-descriptions | core, `wire::image_descriptions` | image descriptions | read or be refused; one that reads is written back as it read |
| surface-boxes | core, `wire::surface_boxes` | `subs` and `rels` | what the host writes reads back; any bytes read or are refused |
| host-session | core, `host::host_session` | a guest's whole session: below | below |
| scaler-requests | core, `scaler::scaler_requests` | the M2 scaler's requests: below | below |
| motion-sessions | core, `motion::motion_sessions` | a guest's motion-estimation processor: below | below |
| h264-streams, h264-records | codec, `h264::tests::fuzz` | streams of the tests' writers with random specs, some outside the standard's ranges, Annex B or length-prefixed, mutated by unit or by sample; `avcC` records | decode to an accelerator that checks what VA-API's is handed: 16 references, lists of 32 naming only those, a configuration before the first picture |
| hevc-streams, hevc-records | codec, `hevc::tests::fuzz` | the tests' hierarchical GOP and sets that stray from it, mutated; `hvcC` records | the same: fifteen references, lists that index them, slice segments within the picture's CTBs, the last one marked last |
| av1-streams, av1-records | codec, `av1::tests::fuzz` | key, inter, intra-only and switch frames, shown existing frames, tile groups, mutated by OBU or by unit; `av1C` records | the same: every tile once inside its group's data, no more than `MAX_TILES`, film grain points that increase (F16) |
| vp9-streams, vp9-records | codec, `vp9::tests::fuzz` | key, inter and intra-only frames, shown existing frames, superframes, tile markers; `vpcC` records and superframe indexes | the same: a supported format, a picture of the frame's size, tiles where their markers say |
| jpeg-images | codec, `jpeg::tests::fuzz` | images of the tests' segment writer, with tables, frame headers of every process, scans and restart intervals in any order; a third with a scan whose data decodes, of the walk's tests' writer (`jpeg/entropy/tests.rs`), damaged token by token | parse or be refused; one that parses names only tables it has, within an 8-bit baseline decode (F15), and its scan walks, as VA-API's decoder walks it (`va/jpeg.rs`), or is refused |
| encode-sessions | codec, `encode::session::fuzz` | settings of every value for every property the guest sets, at open and while it runs; sizes at and past the driver's limits; frames of both depths with any time stamps; flushes | every frame that comes out is one that went in |

The four decoders' accelerators fail one call in 64, as a driver may,
and the streams reset the decoder now and then.

**host-session** (`tests/fuzz/host.rs`). A stand-in guest opens decoder,
encoder and motion-estimation sessions, closes and starts them, with
`avcC` and `hvcC` records whose SPS it writes from scratch, of any field
and sizes about 64 most often, then mutates
(`tests/fuzz/generate/record.rs`, for core's reading of the record), sends frames whole and in fragments,
sets and copies properties of every key `host/encoder/properties.rs` and
`host/decoder.rs` read with values of every type, has the JPEG engine
decode, asks for the host's lists, and sends messages of anything, now and
then mutated. A thread of its own answers what the host asks (`Dbuf`,
`Demt`, `Epba`, `Efds`, `Eemt`), mostly as a guest does, and now and then
late, in pieces, with an error, mangled, twice, to another request or not
at all. Each input must keep:
- every message to the guest within its 0x4000-byte buffer, and readable
  (F4);
- every frame and buffer a `Demt` or `Dfub` names one the guest gave;
- every `Eemt` one message from its fragments, with a sample buffer that
  reads and holds the sample the encoder made, unless it carries an
  error;
- no surface held once the session is dropped, after a teardown of any
  order.

The backends are stand-ins (`tests/fuzz/host/standins.rs`) whose answers
come from a hash of what they are asked, so a run does not depend on its
threads' order. The stand-in host lends three surfaces in four to a
device, never one it cannot reach, and the stand-in decoder's device
declines one write in five, fails one in 37, and otherwise writes each
plane's rows, asserting that it runs outside every lend and that each byte
it writes lies in the surface's memory ([core.md](core.md), "Zero copy").
A stand-in AV1 decoder says, three decodes in four, what each decoded and
showed again (`BackendDecoder::frames_decoded`): up to 20 hidden frames
among those it outputs, frames shown again from its reference slots, and
the slots each frame refreshes, so that the session lends and lets go of
the guest's buffers frame by frame (`host/decoder/lends.rs`). The
stand-in JPEG engine hands `write_samples` its planes as a driver's image
holds them, rows padded past their samples after an offset and chroma
planar or interleaved, and converts BGRA itself for `write_bgra`, as
VA-API's does.

**motion-sessions** (`tests/fuzz/motion.rs`). A stand-in guest's
processor asks `meps`, opens sessions with specifications of every key
and type, and sends `mest` of any size, `mepf` lending any of eight
surfaces (sources, vector buffers or anything, of any layout and format)
under any UUIDs and flags, the property calls, `minv`, `-mep` and
messages of anything, now and then mutated. A thread of its own answers
`Mspa`, `Mvpb`, `Mbuf` and `Memt`, mostly as a guest does, and now and then
with an error, the wrong surface, mangled, twice or not at all. The
stand-in estimator has random formats and largest sizes, and gives fields
of any block size, count and values. Each input must keep:
- every message to the guest within its 0x4000-byte buffer, and readable;
- every UUID a `Memt` or `Mdpb` names one the guest gave;
- no surface held once the session is dropped.

One surface lent as both frames would be read inside its own read, and a
mapper's lock, as reims-vgpu's, would wait on itself for ever: the session
reads such a pair once
(`host::motion_estimation::tests::one_surface_lent_as_both_frames_is_read_once`).
The stand-in host lends one surface at a time across its pool, as
reims-vgpu's mapper does: a lend on another session's thread waits for the
one in hand, and a lend inside another on one thread, or a wait of more
than 10 s, is the input's failure. The host session's stand-in fails a
lend inside another on one thread too.

**scaler-requests** (`tests/fuzz/scaler.rs`). Capabilities and transform
requests as the guest kext lays them out, with rectangles, flags, side
buffers and scatter-gather lists of any kind, against surfaces of every
layout and guest RAM in two regions, one at the top of the address
space, and requests of any type and size, before and after DRIVER_OK and
a reset. The capabilities the device is given bound the ratios at
anything from 1 to none, and the stand-in refuses a transform past 16 to
1 either way, as a driver may: past it core's CPU reference took a debug
build 70 s for one transform (the M3 Max, 2026-10-24), where the scaler's
kernel offers 8, 8, 8 and 4 (`MAX_SCALE`, `crates/reims-vgpu/src/scaler/gpu.rs`).
Each must keep:
- a reply of the request's size, and a capabilities reply no larger than
  its room and the capabilities;
- an end of the VM only for a scatter-gather list past eight entries, as
  Apple's host aborts;
- no surface held once the request is answered;
- after a transform that succeeded, every destination byte outside what
  `scaler::coverage` says it writes unchanged.

### No fuzz input reaches a GPU

The codec targets stop at accelerators of their own; the host session
runs on stand-ins, the scaler on core's CPU reference scaler, the session
encoder on the tests' fake accelerator. Nothing of VA-API, NVDEC, NVENC or
VideoToolbox is called.

Not fuzzed: the NVIDIA and VideoToolbox backends, QEMU's devices, and the
VA-API backend's own code, which needs a driver; what the codec crate
hands it is checked at the accelerator. Nor are the codec crate's readers
that only its examples call, which no guest reaches: `h264::sei`,
`av1::metadata` and the access unit splitters (`h264::nal` and
`hevc::nal`, `access_units`).

### Running them

```sh
cargo xtask check          # the quick runs, of seed 0, among every test
cargo test --offline -p reims-vgpu-video-core --test fuzz      # core's quick runs alone
cargo test --offline -p reims-vgpu-video-codec --lib fuzz      # the codec crate's
RVGV_FUZZ_CASES=1000000 RVGV_FUZZ_SECONDS=900 \
  cargo test --offline -p reims-vgpu-video-core --test fuzz -- --ignored --nocapture
RVGV_FUZZ_CASES=3000000 RVGV_FUZZ_SECONDS=1800 \
  cargo test --offline -p reims-vgpu-video-codec --lib fuzz -- --ignored --nocapture
```

- `RVGV_FUZZ_SEED`: the run's seed; 0 for a quick run, and from the clock
  for a long one, which prints it.
- `RVGV_FUZZ_INDEX`: one input alone.
- `RVGV_FUZZ_CASES`: a long run's inputs, 100,000 by default.
- `RVGV_FUZZ_SECONDS`: a long run's time, unbounded by default.
- `RVGV_FUZZ_TRACE`: the host session's messages each way and its log,
  and the session encoder's refusals, on stderr.

Each run prints its inputs, time, slowest input and most memory, and the
deeper targets how far their inputs got.

### Runs

**Quick runs**, of seed 0 (the M3 Max, debug, `cargo test`, 2026-10-24,
and 2026-10-25 for property-lists, host-session and jpeg-images, with
other builds on its CPU). The codec crate's fuzzers ran among its
unit tests, 1.2 s for all of them; core's fuzz binary took 3.8 s, most of
it the host session's.

| target | inputs | time | how deep |
|---|---|---|---|
| property-lists | 20,000 | 0.81 s | |
| messages | 1,500 | 0.88 s | |
| fragments | 800 | 1.0 s | |
| sample-buffers | 12,000 | 0.44 s | |
| image-descriptions | 30,000 | 0.21 s | |
| surface-boxes | 6,000 | 0.32 s | |
| host-session | 200 | 3.6 s | 8,513 messages to the guest; 68 decoded and 65 encoded frames emitted; 315 AV1 frames told of; 24 JPEG decodes |
| scaler-requests | 3,000 | 0.88 s | 779 transforms done |
| h264-streams, h264-records | 8,000, 3,000 | 1.2 s, 0.12 s | 29,793 pictures to the accelerator |
| hevc-streams, hevc-records | 3,000, 3,000 | 1.1 s, 0.20 s | 6,513 pictures |
| av1-streams, av1-records | 5,000, 3,000 | 1.1 s, 0.03 s | 28,411 frames |
| vp9-streams, vp9-records | 6,000, 4,000 | 0.64 s, 0.07 s | 22,624 frames |
| jpeg-images | 20,000 | 0.62 s | 10,767 parsed, 2,075 scans walked |
| encode-sessions | 1,200 | 0.53 s | 1,233 frames out |
| motion-sessions | 60 | 3.1 s | 1,456 messages to the guest; 15 frames emitted with vectors, 46 with an error |

**Long runs**, in the debug build, 2026-10-25, about two hours of each
target on each CPU, under `nice -n 10` and on under half its cores: an
Intel Core Ultra 9 275HX, at the test threads' 2 MiB stacks, ran every
target; an AMD Ryzen 7 7840HS, at the library's 512 KiB
(`RUST_MIN_STACK=524288`, which the test threads and the threads a
target starts take), ran the host session, the motion-estimation
sessions, property lists and the codec crate's. Earlier long runs found
F27, F36, F37 and F43; these found F45, three times. The inputs are those
of the runs that ended; a run stopped for a newer build is not counted.

| target | where | inputs | time | how deep |
|---|---|---|---|---|
| property-lists | Intel | 52,747,634 | 1,636 s | |
| property-lists | AMD, 512 KiB | 15,000,000 | 697 s | |
| messages | Intel | 6,751,557 | 3,600 s | |
| fragments | Intel | 2,901,468 | 3,600 s | |
| sample-buffers | Intel | 48,000,000 | 1,236 s | |
| image-descriptions | Intel | 48,000,000 | 178 s | |
| surface-boxes | Intel | 48,000,000 | 1,438 s | |
| host-session | Intel | 355,539 | 5,145 s | 15,825,088 messages to the guest; 146,530 decoded and 120,202 encoded frames emitted; 633,834 AV1 frames told of; 32,152 JPEG decodes |
| host-session | AMD, 512 KiB | 396,642 | 5,745 s | 17,708,096 messages to the guest; 163,534 decoded and 133,640 encoded frames emitted; 708,218 AV1 frames told of; 35,641 JPEG decodes |
| motion-sessions | Intel | 119,747 | 5,982 s | 2,775,842 messages to the guest; 24,279 frames emitted with vectors, 79,254 with an error |
| motion-sessions | AMD, 512 KiB | 129,727 | 6,600 s | 3,118,433 messages to the guest; 27,494 frames emitted with vectors, 92,989 with an error |
| scaler-requests | Intel | 28,618,638 | 7,200 s | 7,598,484 transforms; slowest input 1.1 s |
| encode-sessions | Intel | 20,312,746 | 7,200 s | 27,451,292 frames out |
| encode-sessions | AMD, 512 KiB | 6,510,987 | 3,600 s | 8,804,460 frames out |
| h264-streams | Intel | 16,013,352 | 1,757 s | 59,177,315 pictures; and 45,220,450 inputs to F45 |
| h264-streams | AMD, 512 KiB | 18,508,406 | 2,357 s | 68,402,192 pictures; and 19,322,860 and 12,075,812 inputs to F45 |
| h264-records | Intel | 54,000,000 | 779 s | |
| h264-records | AMD, 512 KiB | 15,000,000 | 240 s | |
| hevc-streams | Intel | 28,209,376 | 7,200 s | 59,943,738 pictures |
| hevc-streams | AMD, 512 KiB | 19,638,275 | 6,600 s | 41,741,705 pictures |
| hevc-records | Intel | 54,000,000 | 1,500 s | |
| hevc-records | AMD, 512 KiB | 15,000,000 | 518 s | |
| av1-streams | Intel | 48,698,103 | 7,200 s | 278,801,284 frames |
| av1-streams | AMD, 512 KiB | 38,170,673 | 6,600 s | 218,593,599 frames |
| av1-records | Intel | 54,000,000 | 218 s | |
| av1-records | AMD, 512 KiB | 15,000,000 | 66 s | |
| vp9-streams | Intel | 118,228,743 | 7,200 s | 441,946,553 frames |
| vp9-streams | AMD, 512 KiB | 98,615,994 | 6,600 s | 368,642,204 frames |
| vp9-records | Intel | 54,000,000 | 177 s | |
| vp9-records | AMD, 512 KiB | 15,000,000 | 50 s | |
| jpeg-images | Intel | 172,571,757 | 5,145 s | 92,481,438 parsed, 17,732,662 scans walked |
| jpeg-images | AMD, 512 KiB | 28,832,127 | 724 s | 14,383,984 parsed, 1,933,800 scans walked |

motion-sessions' inputs are slow by design, each waiting out the replies
its stand-in guest withholds.
