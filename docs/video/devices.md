# The guest contract

What the guest's drivers expect of the host, as macOS 26's
Virtualization.framework gives it. The guest kexts and VideoToolbox's guest
half were written against Apple's host, so Apple's host behaviour is the
contract: what each device answers, refuses and sends, in what order, and
where it resets.

A fact about Apple's side is tagged by where it comes from:
- **read** from Apple's code: macOS 26.7's host side (25G229), and the
  26.7.1 guest (25G241), its kexts and frameworks. A fact with no other
  tag is read. A read fact is a hypothesis until a run confirms it, and
  the run then goes beside it.
- **measured on Apple's host session**: VideoToolbox's own host session
  (`VTParavirtualizationHostSession*`, which Apple's VideoToolbox device
  drives), driven by a process of ours on the M3 Max (macOS 27.0.1,
  26A434), with messages built by hand or by core's writer, and the
  guest's buffers made with CoreVideo where a session asks for them.
  macOS 27 is not the guest's host release, so such a fact is a
  hypothesis for 26.7 where Apple's code does not show it too.
- **measured** otherwise: in a process on the M3 Max, on a Linux GPU, or
  in an x86_64 26.7.1 (25G241) guest on this host; the text says which.

Facts about this host are tagged as reims-vgpu's are
([testing.md](../testing.md), "Tags").

## The two devices

| | VideoToolbox device | M2 scaler device |
|---|---|---|
| Apple's model | `VzCore::Hardware::AvpVideoToolboxDevice` | `VzCore::Hardware::AvpM2ScalerDevice` |
| device type string | `com.apple.virtualization.avp.videotoolbox` | `com.apple.virtualization.avp.AppleM2ScalerDevice` |
| PCI vendor:device | 106b:1a03 | 106b:1a0b |
| subsystem, class | unknown | 106b:1a0b; class 0x12, subclass 0, prog-if 0 |
| queues | 2 | 1 |
| device features | none | none |
| guest kext | AppleVideoToolboxParavirtualization | AppleM2ScalerParavirtDriver |

The PCI IDs are the `IOPCIPrimaryMatch` of the guest kexts'
personalities, 0x1a03106b and 0x1a0b106b (read 2026-10-24); the rest is
read from Apple's host.

### Transport

Both are virtio 1.x modern PCI devices on Apple's `VirtioDevice` base. The
vendor is Apple's 0x106b, not virtio's 0x1af4. QEMU's own
`vmapple-virtio-blk-pci` (106b:1a00) is the same kind of device, and its
source is the template (`vendor/qemu/hw/vmapple/virtio-blk.c`,
`vendor/qemu/include/hw/pci/pci_ids.h:199`).

- Features: the base offers `features | mandatory | 0x5_3000_0000`, which
  is bits 28 (indirect descriptors), 29 (event index), 32 (`VERSION_1`) and
  34 (packed ring). It requires `VERSION_1` alone. Both devices pass no
  features of their own.
- BAR 0 holds the common config at 0x0, ISR at 0x1000, notify at 0x4000
  (multiplier 4) and the device config at 0x8000, the last only when there
  is one. MSI-X is in BAR 2, with one vector per queue and one for config.
  INTx is the fallback.
- A queue misused by the guest sets DEVICE_NEEDS_RESET and raises a config
  interrupt; there is no error reply.
- **Queues hold 256 descriptors at most.** Each queue's configuration
  starts at size 0x100, and the base's queue enable refuses a size of
  0x101 or more with "Queue size exceeds the maximum size", for every
  device on it (read in 26.7's `VirtualMachine` binary, 2026-10-24).
- **What the guest sends in one message is bounded by the queue.** Read in
  the x86_64 slice of 26.7.1's `AppleVirtIO.kext` (2026-10-24):
  - `AppleVirtIOQueue::initWithTransport` sizes a queue by what the device
    reports;
  - the video kext's transactions are direct: it calls
    `AppleVirtIOTransaction::init`, which clears the flag `isIndirect`
    reads, and its request buffer is one `IOBufferMemoryDescriptor` of the
    whole message, made without asking for contiguous memory;
  - `AppleVirtIOQueue::prepareTransaction` refuses with
    `kIOReturnBadArgument` a transaction with no segment or more physical
    segments than the queue holds, counted by
    `getTransactionSegmentCount`, one a segment of the request and
    response buffers.

  VideoToolbox sends a sample buffer of up to 0x200000 bytes in one
  message (Messages, "Fragments"). That is 128 pages where a page is
  16 KiB, as on arm64 macOS, and 512 on x86_64, where 256 descriptors
  carry 1 MiB at worst: with a queue of 256, an x86_64 guest's
  1,886,022-byte AV1 sample was refused so, and never reached the host
  (measured in an x86_64 26.7.1 guest). This host's guest-to-host queue
  holds 1024 ("Where we differ from Apple").
- In the guest, AppleVirtIO's `AppleVirtIOPCITransport` binds to the
  `IOPCIDevice` and the kext binds to the transport. Both personalities
  ship in each kext's Info.plist. AppleVirtIO.kext ships for x86_64 too.

### Lifecycle

- DRIVER_OK creates the device's host side: the VideoToolbox device's host
  session, and the scaler's accelerator.
- Reset and stop tear it down. On the VideoToolbox device that runs in two
  phases, invalidate and then complete-invalidate once outstanding work has
  drained.
- Restore into a running device makes a new host session and sends the
  guest the terminate-all message: a 48-byte `Bye!` header with no flags
  and the zero UUID. The guest kext copies it to every session it has,
  with each session's UUID written in, so that the guest drops sessions
  the new host does not have. It is not `-all`, which is a guest message.
  reims-vgpu's QEMU does not migrate the VideoToolbox device: its
  `vmstate` is `unmigratable`
  (`vendor/qemu/hw/display/apple-videotoolbox-pci.c`). So this matters
  only if migration is ever added; core has it as
  `terminate_all_sessions_message`.

### Surfaces read-only for their clients

**How one reaches the host.** The guest's mapper sets a surface's
read-only bit where its memory descriptor's direction is
`kIODirectionOut`. Apple's host then makes its surface over the guest's
pages with `kIOSurfaceClientDirection` 2, logging "creating readonly
IOSurface" (read in macOS 26.6.2's host). Both devices take a message's
surfaces from that mapper. The host writes a transform's destination, a
decoder's output buffers and `jdec`'s destination, and reads a
transform's source, an encoder's frames and `jdec`'s stream.

**No x86_64 guest makes one, as measured.** Read and run in an x86_64
26.7.1 (25G241) guest on this host, 2026-10-24:
- The kernel refuses a surface over a process's own memory. With
  `kIOSurfaceClientAddressRanges`, as one range or a range a page, and
  with or without the direction, `IOSurfaceCreate` failed.
  - The kernel logged "allocate error - no support for client range".
  - IOSurface logged `IOSurface creation failed: e00002c2`.
  - The same properties without the ranges made a surface of the same
    allocation size.
- The direction alone, over IOSurface's own memory, makes a surface, and
  the mapper does not mark it. The surface was mapped read-write for the
  process (`mach_vm_region`). 150 such frames were encoded through
  VideoToolbox, the scaler took them as sources and destinations, and the
  host never logged the bit. The host logs every surface the bit reaches,
  at debug (`reims_vgpu_core::iosurface::descriptor`).
- None of Apple's guest kexts makes an IOSurface over a `kIODirectionOut`
  descriptor. The ones they make (AppleParavirtChannel's, the video and
  scaler transactions' request buffers, the page table) are not surfaces.
- In 25G241's `x86_64h` dyld cache the keys' values, `IOSurfaceAddressRanges`
  and `IOSurfaceDirection`, occur once each, in IOSurface's own strings.
  The cache merges each name into one copy, so this does not exclude a
  framework that imports the constants. On the M3 Max (macOS 27.0.1,
  arm64e), `dyld_info -imports -all_dyld_cache` names ParavirtualizedGraphics
  alone as an importer of either.

So the bit is for arm64 guests, whose IOSurface takes client ranges, as
macOS 27.0.1's does on the M3 Max (reims-vgpu's Metal platform makes such
surfaces). That is not measured in an arm64 guest.

**What Apple's host does, read in its code alone.** Nothing on either
device's path reads the direction. In Apple's host code, `ClientDirection`
occurs only where the mapper sets it.
- VideoToolbox's host session makes a child of each guest surface with
  the parent's creation properties, which carry the direction.
- The scaler retains the mapper's surfaces and hands them to the kernel
  scaler.

So a read is a read, and a write goes to Apple's hardware. On the M3 Max
(macOS 27.0.1), Apple's scaler was given such a destination by a process
of ours. It wrote into it, and the kernel panicked: `dart-scaler0 ... SID 0
write protect exception on write of DVA 0x100000003c0` (twice,
2026-10-24). By that measure Apple's host given a guest's read-only
destination ends the host, not the VM. That is a hypothesis, from a
process on macOS 27 and not a VM on 26.7, and it is not to be measured:
the measure is a host kernel panic.

**This host** lends such a surface for reading and refuses to lend it for
writing: each platform's write lend refuses it
(`crates/reims-vgpu-metal/src/os/surface_bytes.rs`,
`crates/reims-vgpu-vulkan/src/os/surface.rs`), and the glue's lends pass
the refusal on (`crates/reims-vgpu/src/video.rs`). Core answers the
refusal, and leaves the surface as it was:
- a transform into it: kIOReturnNotWritable, 0xe00002cf
  (`scaler::tests::a_read_only_surface_is_a_source_and_never_a_destination`);
- a decoder's frame into it: emitted with -12911 and no buffer, and the
  buffer goes back in a `Dfub`
  (`decoder::tests::a_frame_for_a_read_only_buffer_is_a_malfunction_and_leaves_it`);
- `jdec` into it: -16990, before the backend is asked
  (`jpeg::tests::a_read_only_destination_is_never_written`).

A read-only source is read as any other. Those are core's tests, with
stand-ins that refuse as the glue does, since no x86_64 guest can send
such a surface. "Where we differ from Apple" lists the refusal.

## The VideoToolbox device

### The queues

- **Queue 0, guest to host.** Each element is one message: the readable
  part is the message, and the writable part takes the 4-byte status the
  host session returned for it, an i32 in the host's byte order, which the
  guest kext reads in its own: little-endian on x86_64 both sides. A
  writable part shorter than 4 bytes is logged, and the status is dropped.
  A readable part over 16 MiB is answered -12904 unread ("Where we differ
  from Apple").
- **Queue 1, host to guest.** The guest posts buffers of 0x4000 bytes.
  The host keeps its messages in a ring and writes them into those buffers
  as they come back. A notification on queue 1 only drains that ring. Any
  other queue index is an assertion.

### Messages

A message is a `vtpv` container, every scalar big-endian:

| offset | size | field |
|---|---|---|
| 0x0 | 4 | atom size, header included |
| 0x4 | 4 | `vtpv` |
| 0x8 | 4 | message type (FourCC) |
| 0xc | 4 | flags |
| 0x10 | 16 | guest UUID: the session the message belongs to |
| 0x20 | 8 | reply identifier |
| 0x28 | 8 | reserved, 0 |

The header atom is exactly 48 bytes, and the parameters follow it as its
siblings, not inside it. A parameter is
`[u32 size][para][u32 key][u32 value type][payload]`, big-endian, 16 bytes
before the payload. Scanning a message:

- The header is the first `vtpv` atom: other atoms before it, such as
  `subs`, are skipped. An atom smaller than 8 bytes or longer than what
  remains is malformed, and so is a `vtpv` atom under 48 bytes or none at
  all.
- A parameter is the first `para` atom with its key. One whose key matches
  but whose value type differs is malformed: the message is refused with
  -19093, and a handler that needs it sends no reply. A parameter that is
  absent reads as its default (0, NULL, `kCMTimeInvalid`) with no error.
  Measured on Apple's host session on the M3 Max, 2026-10-24: `dcap` with
  `,cod` as `=u32` returns -19093 and no reply; the host reads `,cod` as
  `=s32`.
- Each getter checks the length: `=u32`, `=s32` 20 bytes in all; `=s64`,
  `=wxh`, `=32p`, `=32s` 24; `=tim` 40; `=uui` 32; `=ui+` 16 plus a whole
  number of 16-byte UUIDs; `=fds` at least 16 + 86.

| value type | payload |
|---|---|
| `=u32`, `=s32`, `=s64` | the integer, big-endian |
| `=tim` | CMTime, big-endian: value i64, timescale i32, flags u32, epoch i64 |
| `=wxh`, `=32p`, `=32s` | two big-endian i32: dimensions, a point, a size |
| `=uui`, `=ui+` | one UUID's 16 bytes, or several |
| `=pls` | a property list; no bytes is NULL |
| `=dat` | raw bytes, fragmented when large |
| `=fds` | a format description as a big-endian QuickTime ImageDescription, the 86-byte header then extension atoms such as `avcC`, then a zero u32 (CoreMedia's `CMVideoFormatDescriptionCopyAsBigEndianImageDescriptionBlockBuffer`) |
| `=sbf` | a sample buffer's serialization, fragmented when large |
| `=tc+` | FigTagCollections, as `tagc` children (multi-image frames) |

- **Property lists** are binary (`bplist00`), and NULL is no bytes at all:
  measured on the M3 Max (macOS 27.0.1), 2026-10-24, by calling
  `FigCreateCFDataFromCFPropertyList` on a dictionary and on NULL. Its
  reader, `FigCreateCFPropertyListFromData`, reads what CoreFoundation's
  `CFPropertyListCreateWithData` reads: binary, XML and OpenStep plists.
  In OpenStep, an unquoted `400` is a string and `<0001>` data (measured
  there, 2026-10-25). Ours reads all three with the plist crate, which
  reads OpenStep otherwise ([bugs.md](bugs.md), B82).
- **The ImageDescription** CoreMedia writes for a 64x48 H.264 stream,
  measured the same way: data reference index 0xffff, temporal and
  spatial quality 0x200, 72 dpi, one frame, compressor name `\x05H.264`,
  depth 24, colour table -1, an `avcC` atom, then four zero bytes; 129
  bytes in all.
- **Fragments.** A value too large for one message is cut across several
  messages sent one after another, each but the last flagged 0x10 with
  "expects reply" cleared, and only the last waits for the reply; the
  guest's decoder cuts at 0x200000 bytes a message. A reader walks them:
  each fragment's flags come from the next header, its piece is the next
  parameter with the key, and an empty piece is malformed. The host
  decoder keeps flag-0x10 fragments without a reply and answers the last
  one, from the last one's header.
- **Sample buffers** are an `sbuf` atom whose children are, in the
  writer's order: `opts` (the output time stamp, when valid), `stia` (the
  CMSampleTimingInfo array: duration, presentation, decode, 72 bytes each),
  `sdat` (the data), `nsmp` (the count, when nonzero), `ssiz` (i32 sizes),
  `fdsc` (the format description, only when it differs from the last one
  written: `mdia`, `vdim`, `codc`, and `extn` holding its extensions),
  `crpt` (a cryptor recipe), then `satt`, `sat2` and `sary` (the
  propagating, the other and the per-sample attachments). Atom headers are
  big-endian; the scalars in them are native, little-endian on x86_64.
  Dictionaries are `dict` of `keyv` entries, each a key then a value; a
  key is `idxk`, a u16 index into a table of 212 known keys (their
  strings read by `dlsym` on macOS 27), or `strk`, a plist string. Leaves
  are one-value plists: `nmbv`, `bulv`, `strk`, `datv`, `dtev`; also
  `null`, `aray`, `clrs`, `urlv` and `fdsc`. The reader refuses a second
  `stia`, `ssiz` or `fdsc`, an `ssiz` that is not whole u32s and a root
  that is not `sbuf`, skips atoms it does not know, and counts one sample
  when there is data and no `nsmp`. A sample buffer with no `fdsc` uses
  the session's last one.

**Flags**: 0x1 a guest request that waits for the host's reply; 0x2 a host
request that waits for the guest's; 0x4 the host's reply; 0x8 the guest's
reply; 0x10 more fragments follow. A reply has its request's type, UUID
and reply identifier. Each side's reply clerk numbers its requests from
100 and waits 1.1 s for a reply by default, 10 s for a codec operation;
measured on Apple's host session on the M3 Max, the host decoder waited
10.0 s for a `Demt` reply that never came. A timeout is -19092; a reply to
no pending request is -19093.

**Surfaces.** A message that names IOSurfaces starts with a `subs` atom of
exactly 0x30 bytes: its header and five big-endian u64 mapping IDs, the
used ones first and the rest 0. The device resolves each nonzero leading ID
through the GPU's IOSurface mapper, and passes the surfaces with the whole
message, `subs` atom included. A message shorter than 8 bytes, a first atom
longer than the message, or a `subs` atom of another size is logged, then
delivered with no surfaces.

- The host session keeps one entry per guest surface: its ID and a weak
  reference to the child surface it made of it. A message naming the
  surface again reuses the live child. If one child cannot be made, the
  message gets no surfaces at all.
- At the end of every delivery, malformed ones too, the session drops the
  entries whose child died and sends their IDs in a `rels` box:
  `[u32 8 + 8n][rels][n big-endian u64]`, alone, not in a `vtpv` message.
  A child dies when the last message, reply or pixel buffer using it lets
  go. Sends are dropped once the session is invalidated.
- The guest kext keeps its own mapping of each surface it lent, and lets
  it go only when a `rels` names it. It counts the IDs from the whole
  message's length, needs at least one, and skips a zero ID. A child that
  failed to be made leaves an entry with ID 0, which the next sweep sends.

**Guest to host.** The host session switches on the message type. The
status it returns is the device's answer; a reply goes out through the
device's queue 1 and returns noErr, or -19098 once the session is
invalidated.

| type | what the host does |
|---|---|
| `decs` | replies `,dec`, its hardware decoders, and `,ver` 6. The guest's `,ver` is read, not checked |
| `dcap` | replies `,dca`, the capabilities of the decoder for `,cod` (`=s32`), NULL for a codec it lacks, and `,ver` 6 |
| `encs` | replies `,enc`, its hardware encoders, and `,ver` 6 |
| `meps` | a guest `,ver` under 6 is -19960 with no reply; otherwise `,mep`, its motion-estimation processors, and `,ver` 6 |
| `+dec`, `+enc`, `+mep` | a UUID that has one is -19093 with no reply. Otherwise it makes the session, even when the codec cannot be made, and replies `,err` (the codec's creation status), `,ver` 6 and `,hin` {`VTHostBuildVersion`} |
| `-dec`, `-enc`, `-mep` | invalidates the session, waits for it, and drops it; noErr |
| `-uui` | the same for the UUID's decoder and encoder |
| `-all` | on an internal build only, sends `Bye!` and drops the decoder and encoder sessions; noErr either way, so a customer build does nothing. Measured on Apple's host session: noErr, nothing sent |
| `jcap`, `jdec`, `jenc` | the JPEG engine: replies sent unless invalidated, noErr either way |
| anything else | the decoder session with the UUID, then the encoder, then the motion-estimation one; -19094 for none |

Measured on Apple's host session on the M3 Max, 2026-10-24, with
hand-built messages: an unknown UUID is -19094; a second `+dec` is -19093
and no reply; after invalidate, `decs` is -19098.

Once a session is open, its own messages go to it by UUID.

**Host to guest.** The guest decoder handles `Dfub`, `Ddpa`, `Drpa`,
`Dpba`, `Demt`, `Dbuf`, `Dmif`, `TDsr`, `TDem` and `Bye!`. The guest
encoder handles `Epba`, `Efds`, `Eemt`, `Esps`, `TEsa`, `TEcd`, `TEsr`,
`TEem`, the multipass storage's `mpsi`, `mpid`, `mpst`, `mpdt`, `mpcd`,
`mpsd` and `mpin`, and `Bye!`. One thing decides the decoder's shape: the
host does not allocate output buffers. It asks the guest for one (`Dbuf`),
the guest creates the pixel buffer, lends it to the host by UUID and
IOSurface, and takes it back when the host emits the frame into it
(`Demt`) or drops it (`Dfub`).

### How the guest finds the host's codecs

- VideoToolbox counts as running in a guest only when
  VideoToolboxParavirtualizationSupport.framework sets up, which opens the
  video kext's user client. Without the kext, nothing in the guest's
  VideoToolbox paravirtualizes.
- It then asks the host for its encoders (`encs`, with `,ver` 6) and
  registers `paravirtualized:<name>` for each one the host lists, flagged
  hardware-accelerated. It does the same for decoders with `decs`.
- So the host's lists decide which codecs the guest offers its apps. Ours
  lists only what its backend reports.
- The guest's two fixed encoders name Apple's AVE encoders:
  `com.apple.videotoolbox.videoencoder.ave.avc` and `...ave.hevc`.
- **What an entry holds**: measured on Apple's host session on the M3 Max,
  2026-10-24, the AVC entry is `CMClassImplementationID`
  `com.apple.videotoolbox.videodecoder.avd.avc`, `CMClassImplementationName`,
  `IsPreferredInternalPlugin` false, `VTCodecType` an array of FourCC
  strings (`avc1`), `VTDecoderCapabilities` {`VTPerProfileDetails`, keyed
  by the profile as a decimal string, each {`VTMaxDecodeLevel`};
  `VTSupportedProfileArray`}, and `VTRating` 400. The guest registers it
  as `paravirtualized:<ID>` and sends the ID back as `,cid` in `+dec`; the
  higher rating wins. Apple's list had six decoders (JPEG, VP9, HEVC, AVC,
  ProRes, AV1) and nine encoders.
- **`dcap` for `avc1`**, measured the same way: {`VTIsHDRAllowedOnDevice`,
  `VTPerProfileSupport` {profile string: {`VTIsHardwareAccelerated`,
  `VTMaxDecodeLevel`, `VTMaxPlaybackLevel`}}, `VTSupportedProfiles`}. The
  guest compares a stream's level with `VTMaxDecodeLevel` and
  `VTMaxPlaybackLevel`, so a level is the bitstream's: `level_idc` for
  H.264.
- **The levels Apple lists**, measured on Apple's host session on the M3
  Max, 2026-10-24: `VTMaxDecodeLevel` 80 for each of `avd.avc`'s eight
  profiles (66, 77, 88, 100, 110, 122, 244, 44), above H.264's highest
  level, 6.2; 186, HEVC's level 6.2, for each of `avd.hevc`'s four (1 to
  4).
- **This host** lists the decoders its backend reports, with IDs and names
  of ours (`reims-vgpu-video.videodecoder.avc`) and rating 400, the
  encoders its encoder backend reports in AVE's shape
  (`reims-vgpu-video.videoencoder.avc`, "reims-vgpu-video Encoder - AVC",
  `VTCodecName` and `VTCodecType` as AVE's, rating 400), and the
  motion-estimation processor its backend has, if any ("The
  motion-estimation session", below;
  `crates/reims-vgpu-video-core/src/host/codec_lists.rs`). A profile whose
  backend reports no level, as VA-API reports none, gets Apple's level for
  its codec; `VTIsHDRAllowedOnDevice` is true, as Apple's ("Colour and
  HDR", below). On radeonsi that is H.264 profiles 66, 77 and 100.

### The decoder session

**Opening.** The guest's decoder sends `+dec` with `,cod` (`=s32`), `,ver`
6, `,cid` the `HostDecoderID` of its specification where it was made with
one, as a decoder the guest registered from `decs` is, with its entry's
`CMClassImplementationID`, and `,gin` {`VTGuestBuildVersion` the guest's
build, `25G241` for 26.7.1}. Its encoder's `+enc` and motion-estimation
processor's `+mep` carry the same `,gin` (read from the guest's
VideoToolbox).

A `+dec` session runs its messages on a serial queue of its own: the
session's deliver returns noErr at once and the handler replies later. A
guest reply (flag 0x8) skips the queue and goes straight to the session's
reply clerk, with the one surface it lent and that surface's mapping ID.
While a handler waits for a guest reply the queue waits too. Every handler
replies with flag 0x4; one that cannot read its parameters sends nothing,
and the guest times out.

| message | reply |
|---|---|
| `dsup` | `,err`, `,sup` the supported properties |
| `dcop` | `,key` must be a string; `,err`, `,val` when there is one |
| `dset` | `,key` must be a string and not `Paravirtualized`, which the host keeps for itself and refuses with no reply; `,err` |
| `dmul` | `,pro`, minus `Paravirtualized`, must not be empty; `,err` |
| `dsta` | `,fds` required, `,blu` read; `,err`. The decoder asks for its output buffers' attributes (`Dpba`) from inside its start, so `Dpba` comes before the reply |
| `dfrm` | `,frm` (`=s64`), `,sbf` required, `,dfl` decode flags, `,fop` options; `,err`, `,inf` |
| `dfin`, `dinv`, `dtfi` | `,err` |
| `dcan` | `,can`, 1 or 0 |
| `dsav` | `,err`, `,pro` when there are some |
| `dsts`, `dtil` | tile decoding: `,err`, and `,inf` for `dtil` |

**Output buffers** are the guest's. For each frame it outputs, the host
decoder asks with `Dbuf` (flag 0x2, `,frm`, `,pop` options), and waits
1.1 s for the reply: `,err`, `,uui` the pixel buffer's UUID, and its
surface. It emits the frame with `Demt` (flag 0x2: `,frm`, `,err`, `,inf`,
the surface attachments, `,uui`) and waits 10 s for the guest's `,err`. A
buffer that dies unemitted is named in a `Dfub` (`,ui+`, no flags) by the
cleanup the decoder runs after each frame, which then sweeps the
surfaces. The guest emits a `Demt` it cannot read, or one for a UUID it
does not know, with an error and does not reply.

Measured on Apple's host session on the M3 Max, 2026-10-24, with Apple's
AVC decoder, a 64x48 stream and buffers made from the `Dpba` attributes:

- `dsta`: `Dpba` with {`PixelFormatType` [`420v`], `Width` 64, `Height` 48,
  `IOSurfaceProperties` {}, `BytesPerRowAlignment` 64, `PlaneAlignment`
  256, `ExtendedPixelsRight` 128, `ExtendedPixelsBottom` 48,
  `PreferRealTimeCacheModeIfEveryoneDoes` 1}, then the reply.
- `dfrm` without the asynchronous flag: `Dbuf`, its reply, `Demt` (`,inf`
  0), its reply, `rels` for the buffer's surface, then the `dfrm` reply
  (`,err` 0, `,inf` 0). With it (`,dfl` 1): `Dbuf`, its reply, the `dfrm`
  reply (`,inf` 0 at 64x48; 0x1 from AVD, below), then `Demt` with `,inf`
  1, its reply, `rels`.
- Frames came out in decode order, one per `dfrm`, frame reordering in
  the stream notwithstanding.
- A `Demt` left unanswered held the `dfrm` reply 10.0 s; then `rels`, then
  the reply with `,err` 0.
- The decoder's own answers: `dfin` -12782 (unimplemented), `dsav`
  -12782, `dmul` -12782, `dtfi` -12782, `dinv` 0; `dcop` of
  `UsingHardwareAcceleratedVideoDecoder` true, of anything it lacks
  -12900; `dset` of `RealTime` 0, of anything it lacks -12900; `dcan` 0
  before `dsta` with a format; `dsup` lists 13 properties. These are the
  decoder's, not the protocol's: a host decoder answers for itself.
- `Demt` carried a 544-byte `,dp2`, the AVD surface's data property, and
  no `,pat` or `,bat`: no colour, for any stream ("Colour and HDR",
  below).
- `dsup` listed `ActiveVideoResolution`, `DecoderUsage`,
  `EnableIOFenceDecode`, `EnableSliceQpExtraction`, `MaxDecoderPixelRate`,
  `MaximizePowerEfficiency`, `NegotiationDetails`, `OnlyTheseFrames`,
  `RealTime`, `SuggestedQualityOfServiceTiers`,
  `UsingHardwareAcceleratedVideoDecoder` and
  `VideoResolutionAdaptationType`; most with an empty dictionary for a
  description, `OnlyTheseFrames` and `ActiveVideoResolution` with their
  types and values.

**The `dfrm` reply's `,inf`** is the decoder's own answer to its decode
call, which the guest hands its client. AVD's is 0x1,
kVTDecodeInfo_Asynchronous, for each frame sent with the asynchronous
flag, and 0 for each sent without, whether the frame decodes or fails.
Measured on Apple's host session on the M3 Max, 2026-10-24, over streams
of 30 frames of ffmpeg's `testsrc2`, every frame both ways:
- H.264 (x264, High, two B frames) and HEVC (x265, Main, two B frames) at
  64x64, 640x360, 1280x720 and 1920x1080, clean and with sample 10's last
  slice cut to half, after which AVD emitted frames 10 to 29 with -12909
  and no image. The failed frames' replies were the decoded frames': 0x1
  asynchronously, 0 synchronously, `,err` 0. Each `Demt` carried the same
  flag.
- VP9 (libvpx, realtime) and AV1 (SVT-AV1) at 640x360 and 1280x720.
- The exceptions: a RASL picture AVD drops replies 0 either way ("HEVC",
  below), and a `dfrm` after a `dsta` AVD refused answers `,err` 311 with
  `,inf` 0.

The 64x48 stream of the measurement above, whose asynchronous reply had
0, cannot be measured again: AVD refuses that size at `dsta` (below), so
which decoder answered it is not known.

**`kVTDecodeFrame_DoNotOutputFrame` changes nothing in AVD's answers.** A
frame sent with it (frame 10) was emitted as the frames around it, with
its image and the same `,inf`, and its reply was theirs: H.264 and HEVC at
1280x720, VP9 and AV1 at 640x360 and 1280x720, both ways (the same runs).
Core emits it so ("This host's decoder", below).

**AVD's H.264 and HEVC decoders refuse a coded side under 64**, at `dsta`,
with -12911 and no `Dpba`, as its AV1 decoder does ("AV1", below); each
`dfrm` after it answers `,err` 311 and emits nothing. The size is the
SPS's coded one (the same runs, 3 frames each):
- H.264 16x16, 48x64 and 64x48 were refused; 50x64 and 64x50 (each coded
  64x64), 56x64, 58x64, 62x64, 64x56, 64x62 and 64x64 decoded.
- HEVC 16x16, 48x64, 64x48, and 50x64, 56x64, 64x50 and 64x56 coded at
  x265's smallest coding block of 8 (so 56 a side) were refused; 58x64,
  62x64 and 64x62 (coded 64) and 64x64 decoded, and so did 50x64 and 64x50
  coded with blocks of 16 (`min-cu-size=16`, coded 64).

Ours leaves the size to the backend ("Where we differ from Apple", below).
Each refuses a coded size under its smallest the same way, at `dsta` with
-12911 and no `Dpba`: VA-API under the driver's smallest surface, 64x64
for H.264 and HEVC on radeonsi (an RX 7900 XTX and a Radeon 780M, Mesa
26.2.4) and so AVD's own; NVIDIA under NVDEC's, 48x64 for H.264 and
144x144 for HEVC on an RTX 5080 Laptop GPU (driver 615.71.09);
VideoToolbox by AVD itself.

**A decoder starts once, and answers a frame with no session 311.**
Measured on Apple's host session on the M3 Max, 2026-10-24, with frames
sent before any `dsta`, after a second `dsta` and after `dinv`, on AVD's
H.264, HEVC, VP9 and AV1 decoders, 2 to 5 frames a stream, each case with
the asynchronous flag and without. The streams were x264's and x265's of
`testsrc2` at 16x16, 48x64, 64x64 and 320x240, x264's `--tff` and
`--fake-interlaced` at 320x240, and VP9 and AV1 streams of 16x16 to 64x64:
- **With no decode session** each `dfrm` answers `,err` 311 with `,inf` 0
  at once, and the host sends nothing else: no `Dbuf`, no `Demt`. That is
  before any `dsta`, after a `dsta` the decoder refused (-12911: H.264
  interlaced or fake-interlaced, and every codec under 64 a side), and
  after `dinv`. Apple's host hands every frame to its decoder, so 311 is
  AVD's own.
- **Every `dsta` after the first answers -12909**, with no `Dpba`,
  whatever the first answered and whatever the second describes: the
  same stream again, another the decoder takes, or one it refuses. After
  a refused first, the frames go on answering 311. After a first it took,
  the session decodes on as that one started it: its stream's frames
  decode, and a 48x64 stream's frames in a 64x64 session are emitted with
  -12909 and no image.
- `dfin` answers -12782 in each of these states, started or not, and
  `dinv` 0.

Ours answers the same ("This host's decoder", below), on every backend, as
the state is core's.

**This host's decoder** follows that order, synchronous and asynchronous,
with a backend's decoder where Apple's has AVD
(`crates/reims-vgpu-video-core/src/host/decoder.rs`):
- `Dpba` is {`PixelFormatType` [`420v`, or `420f` for a full-range SPS],
  `Width`, `Height` from `dsta`'s format description,
  `IOSurfaceProperties` {}, and AVD's constant keys `BytesPerRowAlignment`
  64, `PlaneAlignment` 256 and `PreferRealTimeCacheModeIfEveryoneDoes`
  true}; the frame is copied into the buffer's surface between `Dbuf`'s
  reply and `Demt`, at whatever stride and plane offsets the guest's pool
  then chooses, read from the surface's layout.
  `ExtendedPixelsRight`/`Bottom`, which AVD sets to the padding from the
  displayed size to the codec's aligned coded size (measured on Apple's
  host session: `ExtendedPixelsBottom` 8 for 1920x1080 Main 10, 1088
  coded minus 1080; the 64x48 H.264 stream had both; the 1280x720 stream,
  already aligned, had neither), are left out: ours does not carry the
  coded size here.
- Frames come out in decode order, one per `dfrm`. A sample with no
  picture is emitted with `kVTDecodeInfo_FrameDropped` and no buffer, and
  a frame flagged `kVTDecodeFrame_DoNotOutputFrame` with its image, as
  AVD emits it; a frame that does not decode is emitted with the backend's
  status and no buffer, and the `dfrm` reply says noErr. For H.264 and
  HEVC it asks for a buffer first, as AVD does: `Dbuf`, `Demt` with no
  buffer, `Dfub`, `rels` ("H.264", below). With no decode session, before
  any `dsta`, after one refused, or after `dinv`, `dfrm` answers 311 with
  `,inf` 0 and emits nothing; every `dsta` after the first answers -12909
  with no `Dpba`, and the session stays as the first left it, as AVD's
  (above).
- The `dfrm` reply's `,inf` is AVD's: 0x1 for a frame sent with the
  asynchronous flag and 0 for one sent without, whatever the frame gave,
  and 0 for a RASL picture dropped and without `dsta`.
- A new `avcC` in a sample buffer's format description reconfigures the
  decoder from that frame.
- `dfin` emits what the backend still holds and answers -12782, as AVD's
  decoders answer it, started or not (above); `dinv` drops the decoder and
  answers noErr; `dsup` lists `UsingHardwareAcceleratedVideoDecoder`
  (true, read-only), `RealTime` and `MaximizePowerEfficiency` (settable),
  but for VP9, whose properties are Apple's VP9 decoder's ("VP9", below);
  `dmul`, `dsav`, `dtfi`, `dsts` and `dtil` answer -12782, as AVD's did.

**A format description of another size.** A guest's description may give
another size than its stream's. Measured on Apple's host session on the
M3 Max, 2026-10-24, with descriptions of other sizes than their streams'
and buffers made by `CVPixelBufferCreate`, their luma filled with a mark:
- H.264 1280x720 under a 1920x1080 description: `dsta` noErr, `Dpba` at
  1920x1080 with `ExtendedPixelsBottom` 8, as for a 1080-line stream, and
  every frame emitted noErr. AVD wrote the first 1280x720 of each buffer,
  the same luma as under a 1280x720 description, and left the rest as it
  was: the picture is not scaled.
- HEVC 1920x1080 (1088 coded) under 2560x1440: `Dpba` at 2560x1440, and
  AVD wrote 1920x1088, the coded rows below the picture too.
- AV1 1280x720 under 1920x1080: `Dpba` at 1920x1080, 1280x720 written.
- A description smaller than its stream was not run on Apple's hardware,
  since AVD would then be given buffers smaller than what it decodes, and
  the M3 Max has panicked on a hardware write outside what a process may
  write ("Surfaces read-only for their clients", above).

This host writes the picture alone into the guest's buffer, at its top
left, and leaves the rest as it was. Where the buffer is smaller, it takes
what fits (`FrameReadBack::copy_to`). The VideoToolbox and VA-API
backends gave the same bytes in every case: H.264, HEVC Main and Main 10
and AV1 under larger and smaller descriptions, down to 1x1 (the
conformance crate's `framemd5 --description`: VideoToolbox on the M3 Max
and VA-API on an RX 7900 XTX, 2026-10-24). VideoToolbox makes its frames
the description's size, with the picture at their top left, and the
backend copies only the picture of each (`stream_size`,
`crates/reims-vgpu-video-videotoolbox/src/macos/stream_format.rs`).
VA-API copies what fits (`copy_fitting_to`,
`crates/reims-vgpu-video-vaapi/src/va/frame.rs`). The rows below the
picture that AVD also writes (1080 to 1087) are not written here, as
`ExtendedPixels*` is not sent either ("The decoder session", above).

### H.264

**Apple's host**, measured on Apple's host session on the M3 Max,
2026-10-24, with AVD's AVC decoder. The streams were written by x264 0.165
from ffmpeg's `testsrc2`, and edited by the VA-API crate's `h264-craft`
example ([vaapi.md](vaapi.md), "Crafted streams"); its `h264-info` read
each.
- **Interlaced.** `dsta` answers -12911 and sends no `Dpba` for every
  SPS with `frame_mbs_only_flag` 0. That covers x264's MBAFF (`--tff` and
  `--bff`, CABAC and CAVLC, 480i to 1080i) and `--fake-interlaced`, whose
  pictures are frames alone. Each `dfrm` after it answers `,err` 311 and
  emits nothing. In a process on the M3 Max, VideoToolbox's hardware
  session fails with -12911 too, and its software decoder decodes the
  same streams. x264 writes no field pictures, so no PAFF stream, or one
  mixing fields and frames, was written. Both need the same flag, which
  is what `dsta` refuses.
- **An interlaced SPS in band**, after a progressive `avcC`: each frame is
  `Dbuf`, then `Demt` with -12909 and no buffer (`,frm ,err ,inf` alone),
  then `Dfub`, `rels` and the `dfrm` reply with noErr. The next
  progressive IDR picture decodes.
- **NAL unit lengths.** 1, 2 and 4 bytes decode. With 3, `dsta` answers
  -12910. In a process, the hardware session is not made (-12910), and the
  software decoder fails with -1.
- **`dcan`** answers 1 for the same length size and dimensions, another
  profile included. It answers 0 for another length size, another size,
  3-byte lengths, an interlaced SPS or another codec, and from a session
  `dsta` refused.
- **A frame that does not decode** gives `Dbuf`, `Demt` with -12909 and
  no buffer, `Dfub`, `rels`, and the `dfrm` reply with noErr. That covers
  a truncated slice (cut to 10, 50 or 95%), a garbage slice NAL unit, and
  a NAL unit with `forbidden_zero_bit` set. **Every frame after it is
  answered so, until an I picture, IDR or not, decodes.** That holds for
  a damaged reference and for a non-reference frame alike: with a garbage
  NAL unit after frame 20 of a stream with an I picture at 30, frames 20
  to 29 failed, and 30 on decoded. The asynchronous mode once differed:
  after a truncated non-reference B frame (ltr-a, frame 3), the next
  frame, already in flight, decoded asynchronously and failed
  synchronously. After a garbage NAL unit, found before any decode, none
  did.
- A NAL unit of reserved type 23 is ignored.
- **Lost frames**, whether a reference, several, a non-reference, or a P
  in a stream with B-frames: every frame sent is emitted with an image and
  noErr. Drop20 (frame 20 of 60 left out) gave 59 of 59.
- **Long-term references and MMCO 2 to 6** (`ltr-mmco-cabac`,
  `ltr-mmco-cavlc`), and **frame number gaps** the SPS allows (`gaps5-ref1`,
  `gaps2-ref4`, `gaps9-ref4`): every frame decoded, and in a process the
  hardware's frames equalled the software decoder's.
- **Asynchronously**, a `dfrm`'s reply comes once its `Dbuf` is answered.
  Up to three frames are in flight: `Dbuf` 0, 1 and 2 come before `Demt` 0.
- **HEVC fails the same way**, but only an IRAP picture ends it ("HEVC",
  below). A truncated frame (3, 4 or 7) of a 1080p HEVC Main stream with
  B-frames gave -12909 for it and every frame after it, to the 40th sent.
  Asynchronously, one frame in flight after it decoded. The order was
  H.264's.

**This host** (`crates/reims-vgpu-video-core/src/host/decoder.rs`,
`decoder/avc_record.rs`; the codec crate's `h264/decoder.rs`):
- Core reads the `avcC` record before any backend does. It refuses 3-byte
  lengths at `dsta` with -12910, and an SPS with `frame_mbs_only_flag` 0
  with -12911, sending no `Dpba`. A coded size under the backend's
  smallest is the backend's to refuse, with -12911 and no `Dpba` ("The
  decoder session", above).
  `dcan` answers as Apple's does where that was measured.
- The decoder refuses an interlaced SPS in band with `Interlaced`, which
  is -12909 for each frame until a progressive IDR picture.
- After a sample that does not decode, each picture is decoded, so that
  its references go on, but refused with `Unrecovered` (-12909), until an
  I picture decodes. On NVIDIA, a frame whose decode NVDEC reports failed
  at its copy, as it reports a truncated slice, is a sample that does not
  decode too: it is answered -12909, and the decoder's state is set before
  the next sample (`Failures`, `crates/reims-vgpu-video-nvidia/src/decode.rs`).
- A frame lost where the SPS allows no gap is stood in for by the last
  reference (`Accelerator::conceal`): a copy on VA-API, its surface on
  NVIDIA ([nvidia.md](nvidia.md), "Concealment"), so the frames after it
  decode.
- A failed H.264 or HEVC frame is emitted in Apple's order: `Dbuf`,
  `Demt` with no buffer, `Dfub`, `rels`.
- Checked on an RX 7900 XTX and a Radeon 780M (radeonsi, Mesa 26.2.4),
  2026-10-24. `crates/reims-vgpu-video-vaapi/tools/h264-check.sh` passed
  62 of 62 on each. The H.264 part of `host-session-check.sh`, beside it,
  passed 35 of 35 on each. That part covers the refusals at `dsta`, and
  drop20 (59 frames, the source's 50 outside the loss among them). It also
  covers garbage after frame 20, as a slice and with the forbidden bit: 10
  refused, and the source's other 50 frames. Each stream ran
  synchronously and asynchronously ([vaapi.md](vaapi.md), "Crafted
  streams").
- On VA-API, unlike Apple's, ours does not fail a truncated slice
  ("Where we differ from Apple"). On NVIDIA it does, as Apple's does
  synchronously: main-gapsrc-720p with frame 20's slice cut to 10, 50 or
  95% (`h264-craft --truncate 20:PCT`) gave -12909 for frames 20 to 29,
  and the other 50 frames equalled the undamaged stream's, synchronously
  and asynchronously, with zero copy and without, on an RTX 5080 Laptop
  GPU (driver 615.71.09), 2026-10-25.

**What the guest does with it**, measured in an x86_64 26.7.1 (25G241)
guest on this host: for a fake-interlaced stream, VideoToolbox's hardware
session is not made (-12911), as in a process on the M3 Max, and its
software decoder decodes the stream, 30 of 30 frames.

### HEVC

**Apple's host**, measured on Apple's host session on the M3 Max,
2026-10-24, with messages built by core's writer and streams x265 wrote:
- **`decs`** lists `com.apple.videotoolbox.videodecoder.avd.hevc`,
  "Apple Video Decoder - HEVC family", `VTCodecType` [`hvc1`] alone (no
  `hev1`), `VTSupportedProfileArray` [1, 2, 3, 4], each profile at
  `VTMaxDecodeLevel` 186, and `VTRating` 400.
- **`dcap` for `hvc1`**: `VTIsHDRAllowedOnDevice` true, and for each of
  profiles 1 to 4 {`VTIsHardwareAccelerated` true, `VTMaxDecodeLevel`
  186, `VTMaxPlaybackLevel` 186}; `VTSupportedProfiles` [1, 2, 3, 4].
- **`Dpba`**, after `+dec` (`,cid` the AVD ID above) and `dsta` with the
  stream's `hvcC`, before the `dsta` reply as for H.264: {`PixelFormatType`
  [`420v`], `Width` 1280, `Height` 720, `IOSurfaceProperties` {},
  `BytesPerRowAlignment` 64, `PlaneAlignment` 256,
  `PreferRealTimeCacheModeIfEveryoneDoes` true} for Main at 1280x720. For
  Main 10 the format is [`x420`]. With `video_full_range_flag` set, at
  640x360, it is [`420f`] for 8 bits and [`xf20`] for 10. These sizes had
  no `ExtendedPixels` keys, which the 64x48 H.264 stream's had.
- **`dfrm`**: frames come out in decode order, one per `dfrm`, as for
  H.264 (below).

**Apple's host on tiles, dependent slice segments and fields**, measured
on Apple's host session on the M3 Max, 2026-10-24, synchronously and not,
with CoreVideo buffers, AVD's HEVC decoder and kvazaar 2.3.2's eleven
streams of `crates/reims-vgpu-video-vaapi/tools/hevc-check.sh`
([vaapi.md](vaapi.md), "Testing HEVC"): tiles 3x3 uniform and 4x3
explicit in one slice, 2x2, 2x3 and 4x4 in a slice each (1080p, 1366x766,
4K), WPP rows in dependent slice segments with and without an open GOP's
CRA and 30 RASL pictures, low-delay B, 8 current references, slice headers
with their own short-term sets, and field pictures (`field_seq_flag`, 60
fields of 1280x360):
- every `dsta` answered 0, and `Dpba` asked for `420v` at the stream's
  size, for the fields 1280x360: each field is a picture of its own;
- every frame came out with an image and noErr, one per `dfrm`, in decode
  order, in both modes, and every frame's luma was the luma of ffmpeg's
  decode, which was kvazaar's reconstruction;
- the fields' frames carried no attachments the others lacked (`,pat`
  none), and their format description's `CVFieldCount` was 1, as
  CoreMedia makes it of the MP4's sample entry for every stream.

Not given to Apple's host: kvazaar's stream of up to 15 current
references, past the 8 va.h allows, and the one with a tile column
narrower than Main allows, which may be outside the standard and so was
given to no Apple hardware.

**Apple's host after a frame that fails**, measured on Apple's host
session on the M3 Max, 2026-10-24, synchronously and not, with the frame's
slices cut to half, AVD's HEVC decoder and CoreVideo buffers. The streams
were x265 4.3's, of `testsrc2` at 1280x720, 60 frames, `--preset fast
--bframes 3 --no-scenecut`, with the VA-API crate's `hevc-info
--pictures` saying what each picture is: `idr` (`--keyint 30
--no-open-gop`, an IDR picture at 30), `radl` (the same with `--radl 2`:
IDR_W_RADL at 28 and two RADL pictures), `cra` (`--open-gop`: a CRA
picture at 29 and one RASL picture), `cra7` (`--bframes 7 --b-adapt 0
--keyint 32`: a CRA picture at 25 and seven RASL pictures), `itrail`
(`--keyint 200`, a qpfile's `30 i`: a TRAIL_R picture of I slices alone at
29), `bla` (`cra` with its CRA picture made BLA_W_LP by its `hevc-cut
bla`) and `cra7-from25` (`cra7` cut at its CRA picture by `hevc-cut from`,
as a seek). Frames count in decode order.
- **Every frame after one that fails is answered -12909, until an IDR,
  CRA or BLA picture that decodes.** With frame 17 or 20 cut to half,
  `idr` failed from it to 29 and decoded 30 on; `radl` failed 20 to 27,
  and decoded its IDR picture, both RADL pictures and the rest.
- **A picture of I slices alone that is not IRAP ends nothing,** unlike
  H.264's I picture: `itrail` failed from the damaged frame to the 60th.
- **An IRAP picture that fails itself ends nothing:** with `idr`'s IDR
  picture or `cra`'s CRA picture cut, every frame from it on failed.
- **The CRA picture that ends a failure is taken as a BLA one:** its RASL
  pictures are dropped. `cra` with frame 20 cut failed 20 to 28, decoded
  the CRA picture, dropped the RASL picture at 30 and decoded the rest;
  `cra7` failed 20 to 24 and dropped all seven RASL pictures, 26 to 32.
  Undamaged, both decoded all 60, their RASL pictures too.
- **A dropped RASL picture** gives `Dbuf`, `Demt` with -17694
  (kVTVideoDecoderReferenceMissingErr), `,inf` 0xa
  (kVTDecodeInfo_FrameDropped and SkippedLeadingFrameDropped) and no
  buffer, `Dfub`, `rels`, then the `dfrm` reply, the same synchronously
  and asynchronously: no asynchronous flag in `,inf`, and the reply last.
  A BLA picture's RASL pictures are dropped so whether or not a frame
  failed (`bla`: the one at 30), and so are those of a CRA picture a
  stream starts at (`cra7-from25`: 1 to 7).
- **Asynchronously** the two frames after the damaged one, already in
  flight, decoded, and the frames after them failed: in every stream,
  frames 21 and 22 came out with images.

**This host after a frame that fails** (the codec crate's
`hevc/decoder.rs`, core's `host/decoder.rs`):
- After a sample that does not decode, each picture is refused with
  `Unrecovered` (-12909) until an IRAP picture decodes. The pictures
  between are not decoded: none after the IRAP picture predicts from
  them.
- The CRA picture that ends it has `HandleCraAsBlaFlag` 1, so its RASL
  pictures are skipped, as a BLA picture's and those of a CRA picture the
  decoder starts at. A skipped RASL picture is refused with
  `SkippedLeadingPicture`, which core emits as Apple's AVD drops one:
  -17694, `,inf` 0xa, no buffer, in a failed frame's order with the reply
  last.
- The state starts at a frame the parser refuses, a garbage NAL unit or
  one with the forbidden bit, and on NVIDIA at a frame whose decode NVDEC
  reports failed at its copy, as it reports a truncated slice (status 9,
  `cuvidDecodeStatus_Error_Concealed`), set before the next sample
  (`Failures`, `crates/reims-vgpu-video-nvidia/src/decode.rs`). On VA-API
  a truncated slice is not found ([bugs.md](bugs.md), B41).
- Checked on the streams above, each damaged by a NAL unit with
  `forbidden_zero_bit` after frame 20's slices (`hevc-cut forbid`), which
  no GPU sees. Each stream decoded synchronously and asynchronously, and
  every answer was Apple's synchronous one: `idr` 10 refused, `radl` 8,
  `cra` 9 and one skipped, `cra7` 5 and seven skipped, `itrail` 40, `bla`
  9 and one skipped; undamaged, `bla` one skipped and `cra7-from25` seven.
  Every frame that came out equalled the undamaged stream's, and those
  ffmpeg's decode. 26 of 26 passed on an RX 7900 XTX and a Radeon 780M
  (radeonsi, Mesa 26.2.4) and on an RTX 5080 Laptop GPU's NVDEC (driver
  615.71.09), 2026-10-24.
- Checked on NVIDIA with the same streams' frame 20 cut to half its slice
  (`hevc-cut truncate`), on that GPU, 2026-10-25, synchronously and
  asynchronously, with zero copy and without: every answer was Apple's
  synchronous one, `idr` 20 to 29 refused, `radl` 20 to 27, `cra` 20 to 28
  and its RASL picture skipped, `cra7` 20 to 24 and its seven skipped,
  `bla` 20 to 28 and one skipped, and every frame that came out equalled
  the undamaged stream's. Before the state was set from NVDEC's status,
  frame 20 alone was refused.
- On VideoToolbox, Apple's decoder keeps this state itself. A RASL picture
  it drops reaches the backend's callback with -17694 and flags 0xa, and
  the backend hands it to core as `SkippedLeadingPicture`, so it is
  emitted as from Apple's host. On the M3 Max (macOS 27.0.1, 2026-10-24)
  `bla`'s RASL picture and `cra7-from25`'s seven came out so, in both
  modes, and every other frame as the undamaged stream's. A damaged stream
  was not sent to it here.

**This host** (`crates/reims-vgpu-video-vaapi/src/va/backend.rs`, core's
`host/decoder.rs`):
- lists `reims-vgpu-video.videodecoder.hevc` for `hvc1`, with profiles 1
  and 2 where the driver has Main and Main 10, and 3, Main Still Picture,
  wherever it has Main, each at 186 (radeonsi: all three);
- asks in `Dpba` for the format the `hvcC` record's first SPS gives:
  `420v`, `420f`, `x420` or `xf20`, by bit depth and range, as Apple's
  host does for the same four cases;
- copies P010 frames into `x420` and `xf20` buffers two bytes a sample,
  and an `hvcC` in a sample buffer's format description that differs from
  the session's reopens the decoder from that frame, as an `avcC` does;
- leaves the coded size to the backend, which refuses one under its
  smallest at `dsta` with -12911 and no `Dpba`, as AVD refuses a side
  under 64 ("The decoder session", above): VA-API reads it from the
  `hvcC`'s SPS (`va/backend.rs`, `session_size`);
- decodes kvazaar's tiles, dependent slice segments and fields as Apple's
  host does, every frame of each stream bit-exact on both radeonsi GPUs,
  NVDEC and VideoToolbox, field pictures at half the frame's height, with
  `Dpba` at that size ([vaapi.md](vaapi.md), "Testing HEVC");
- refuses a picture whose current reference sets hold more than 8 on
  VA-API, and a set of more than 8 on NVIDIA, with -12910 and no image,
  then -12909 for each picture after it until an IRAP picture, as after
  any frame that fails. What Apple's host does with such a picture is not
  measured.

**What the guest does with it**, measured in an x86_64 26.7.1 (25G241)
guest on this host:
- VideoToolbox lists ours as `hardware yes paravirtualized yes`, and
  registers its HEVC-based decoders on it: Depth, Disparity, Dolby Vision
  (HEVC) and Muxed Alpha each name
  `...:paravirtualized:reims-vgpu-video.videodecoder.hevc`.
- A decode session's Main and Main 10 streams ask for `420v` and `x420`,
  and get frames bit-exact to the guest's own software decoder.
- At the login the Aerial wallpaper (`WallpaperAerialsExtension`) opens a
  3840x2160 Main 10 session on ours, and decodes synchronously. On a guest
  whose 10 s waits end at once ("What an x86_64 guest has", "The guest's
  10 s waits expire at once on x86_64") it gives up on each frame, and
  after a few leaves `Dbuf` unanswered; what this host does then is "A
  frame the guest has given up on is decoded and not emitted" ("Where we
  differ from Apple").

### AV1

**Apple's host**, measured on Apple's host session on the M3 Max,
2026-10-24, with hand-built messages and libaom streams:
- **`decs`** lists `com.apple.videotoolbox.videodecoder.avd.av1`, "Apple
  Video Decoder - AV1 family", `IsPreferredInternalPlugin` false,
  `VTCodecType` [`av01`] and `VTRating` 400, with no
  `VTDecoderCapabilities`, as JPEG, VP9 and ProRes have none: the key comes
  only from the decoder's IORegistry capabilities.
- **`dcap` for `av01`**: `VTSupportedProfiles` [0, 1, 2] and
  `VTPerProfileSupport` with, for each, {`VTChromaSubsampling` [`110`,
  `111`], `VTColorDepth` [`08`, `10`] ([`08`, `10`, `12`] for profile 2),
  `VTIsHardwareAccelerated` true, `VTMaxDecodeLevel` 19,
  `VTMaxPlaybackLevel` 17, `VTMaxHDRPlaybackLevel` 16}, and no
  `VTIsHDRAllowedOnDevice`. That is exactly what 26.7's code writes for a
  hardware decoder. The levels are `seq_level_idx` values: 19 is level
  6.3, 17 level 6.1, 16 level 6.0.
- **`dsta`** asks with `Dpba` before its reply, as for H.264:
  {`PixelFormatType` [`420v`] for 8 bits, [`x420`] for 10, `Width`,
  `Height`, `IOSurfaceProperties` {}, `BytesPerRowAlignment` 64,
  `PlaneAlignment` 256, `PreferRealTimeCacheModeIfEveryoneDoes` true} at
  128x96 and 1280x720, with no `ExtendedPixels` keys. At 64x48 `dsta`
  answers -12911 with no `Dpba`; a local session refuses 128x48 and 48x128
  too and takes 64x64, so it is AVD's minimum, not the protocol's.
- **Its edge is 64 pixels a side**, as VP9's is ("VP9", below):
  VideoToolbox's hardware decode sessions in a process on the M3 Max
  (2026-10-24), on aomenc streams of 10 frames, were not made, -12911, for
  every stream under 64 pixels on either side: 16x16, 32x32, 48x48, 63x63,
  48x64, 64x48, 63x720, 1280x63, and 63x64 and 64x63 at 8 and 10 bits.
  64x64 at 8 and 10 bits, 65x65, 64x720 and 1280x64 decoded all 10 frames.
- **`dsup`** lists seven properties, each with an empty description, the
  same before and after `dsta`: `AV1FilmGrainMode`,
  `AllowBitstreamToChangeFrameDimensions`, `MaxDecoderPixelRate`,
  `RequestedMVAV1SpatialIDs`, `SuggestedQualityOfServiceTiers`,
  `SupportedPixelFormatsOrderedByQuality` and
  `UsingHardwareAcceleratedVideoDecoder`; not `RealTime`.
- **`dcop`** after `dsta`: `UsingHardwareAcceleratedVideoDecoder` true
  (before `dsta` too), `AV1FilmGrainMode` "Immediate",
  `AllowBitstreamToChangeFrameDimensions` true, `MaxDecoderPixelRate`
  7365427200, `SuggestedQualityOfServiceTiers` four tiers,
  `SupportedPixelFormatsOrderedByQuality` [`420v`] or [`x420`], and
  `RequestedMVAV1SpatialIDs` -12911 with no value.
- **`dset` of `AV1FilmGrainMode`**: "Immediate" and "Defer" both answer
  0, and `dcop` then reads "Defer": the host passes the value through, its
  translation table being empty (read).
- Its frames are the reference decode's: hardware decode sessions in a
  process on the M3 Max (2026-10-24) matched libdav1d's MD5 list frame for
  frame on seven streams, 290 frames, film grain included. AV1's film
  grain synthesis is normative, so grain is part of the decode. A
  software-only `av01` session is refused there with -12906.
- **Its `dfrm`s, with buffers lent**, measured on Apple's host session on
  the M3 Max, 2026-10-24, asynchronously, buffers recycled, on libaom
  3.15.1 streams ([vaapi.md](vaapi.md), "AV1 on the AMD GPUs"): forward key
  frames hidden and shown again, key frames shown through an overlay,
  ROTZOOM global motion, inter frames of 569x320 between key frames of
  640x360, and av1-svc's temporal and spatial layers
  (`crates/reims-vgpu-video-vaapi/tools/av1-svc.c`). Every stream decoded
  whole, every frame's luma equal to ffmpeg's libdav1d decode of it, in
  order:
  - **A `Dbuf` for every frame AVD decodes and every frame shown again**,
    shown or not: the forward key frame stream, 60 frames decoded and 27
    shown with `show_existing_frame`, had 87 `Dbuf`s and 60 `Demt`s; the
    overlay stream 83 and 60. AVD decodes into the guest's buffers, which
    are its references, and sends `Dfub` and `rels` as they stop being
    referenced (27 and 64, 21 and 60).
  - **A frame of another size gets a `Dpba` at its own size**, before its
    `Dbuf`: 640x360, then 569x320 with `ExtendedPixelsRight` 7, then
    640x360 with `ExtendedPixelsBottom` 8 at the next key frame, six in 30
    frames. Its buffers are of each frame's size, with no `Dpba` for the
    first frame beyond `dsta`'s.
  - **Spatial layers are decoded, and the top one emitted.** A unit of two
    spatial layers, 640x360 under 1280x720, had a `Dpba` at each layer's
    size before each layer's `Dbuf` (121 `Dpba`s, 120 `Dbuf`s in 60
    units) and one `Demt`, the 1280x720 frame; three layers (320x180 with
    `ExtendedPixelsBottom` 12, 640x360, 1280x720) 181 `Dpba`s, 180 `Dbuf`s
    and 60 `Demt`s. Two layers of one size had one `Dpba`, 120 `Dbuf`s and
    60 `Demt`s. Operating point 0 is decoded: every layer.
  - **A frame coded `showable_frame` 0 and shown again is shown.** libaom
    3.15 codes its filtered forward key frames so ([bugs.md](bugs.md),
    U12); 6.8.2 has such a frame showable, and aomdec stops there. AVD
    decoded and emitted all 60 frames of such a stream, equal to ffmpeg's
    libdav1d. Measured a second time, AVD refused it ("When AVD lets each
    buffer go", below). The two runs disagree, and AVD is not measured on
    it again: ours shows it ("Where we differ from Apple").
- **When AVD lets each buffer go**, measured on Apple's host session on
  the M3 Max, 2026-10-24, synchronously and not, buffers recycled, on the
  libaom, SVT-AV1 and av1-svc streams of
  `crates/reims-vgpu-video-vaapi/tools/av1-check.sh`: aom-720p,
  aom-720p-fwdkf, aom-360p-fwdkf-filtered, aom-720p-kfflt2,
  aom-360p-resize-569, svt-1080p and svc-720p-l1t2, -l2t1, -l2t1q and
  -l3t3, each frame's references read with `av1-info --frames`; and
  asynchronously with each frame sent 33 ms after the last one's reply,
  as a 30 fps player sends, on aom-720p-fwdkf, svc-720p-l2t1 and
  svt-1080p. Number the frames a temporal unit carries as AVD takes them,
  a step each, every spatial layer's and every frame shown again
  included:
  - **Each step is lent a buffer**: a `Dbuf`, after a `Dpba` at the
    frame's size (`UpscaledWidth` by `FrameHeight`) where that is not the
    size last asked for. The size last asked for starts at the sequence's
    largest frame, not `dsta`'s: svc-720p-l2t1, a 1280x720 sequence, had
    a `Dpba` at 640x360 before its first `Dbuf`, and aom-720p under a
    1920x1080 description had none.
  - **A frame is decoded into its step's buffer, and a frame shown again
    is emitted in that buffer**: the `Demt` of a `show_existing_frame`
    names the buffer its frame was decoded into, so a hidden frame's
    picture reaches the guest then. The buffer lent for showing it again
    is never named by a `Demt`.
  - **A buffer is let go at the start of step k**, the latest of: one
    past the step after which no reference slot holds its frame; two past
    the step that decoded into it, or one past the step that lent it to
    show a frame again; and two past the last step whose
    `primary_ref_frame` named its frame. Its `rels` comes with the next
    sweep: the guest's next message's, the next `Dbuf`'s reply or the
    next `dfrm`, or the clean-up's after step k, whichever comes first;
    and a buffer no `Demt` named comes back in the `Dfub` after step k.
    The rule gave the step of every buffer let go in the synchronous runs,
    794 of them, and in the paced asynchronous ones, 271. At most 10
    buffers were held at once synchronously; 12, 6 and 11 paced
    asynchronously; and 13 asynchronously with frames sent as fast as AVD
    answered.
  - **Held so, a buffer is busy to the guest** for 7.5 to 131 ms when
    emitted (13.7 to 119.7 ms asynchronously), and a buffer lent to show a
    frame again for 3.2 to 3.9 ms (8.8 to 16.3 ms).
  - **Asynchronously, AVD asks for a unit's buffers before it decodes
    the unit's frames**: every step's `Dpba` and `Dbuf`, then the
    `dfrm`'s reply, then each step's `Dfub`, `Demt` and `rels` in turn as
    the hardware finishes it. Paced, the rest of a unit's steps came after
    its reply and before the next `dfrm`, but for a first step's `Dfub`
    that came before the unit's last `Dbuf` where the unit had three or
    four (aom-720p-fwdkf's units 9 and 18). Sent as fast as AVD answered,
    the guest ran up to three units ahead, and each unit's `Demt`, `Dfub`s
    and `rels` came among the next units' `Dbuf`s; the last four frames'
    `Demt`s came after `dfin`'s reply, 3.1 ms apart.
  - **Some orders are races between Apple's threads**, and differ from
    run to run: a `Demt`, sent from AVD's thread, can come after a
    synchronous `dfrm`'s reply (svc-720p-l2t1q's frame 1, svt-1080p's
    25); a `rels` can come before the reply, where it most often comes
    with the next `dfrm` (svt-1080p's frame 22), or after the next
    `Dbuf`, where it most often comes before it (svt-1080p's frames 53
    and 59, svc-720p-l2t1q's 13), or after `dfin`'s reply (svt-1080p's
    last); and asynchronously, where a unit's `Dfub`s, `Demt` and `rels`
    fall among its own and the next units' `Dbuf`s and replies is the
    hardware's pace.
  - **aom-360p-fwdkf-filtered was refused** at frame 48, which shows the
    key frame sample 34 coded `showable_frame` 0: a `Demt` of -12909 with
    no `Dbuf` and no image, then every frame to the last, 49 to 59, lent
    its buffers and emitted -12909 with no image, synchronously and not.
    AVD so refused what 6.8.2 refuses, as aomdec and the dav1d command
    line do.

**The guest**, read in its VideoToolbox:
- Its paravirtualized decoder rewrites a set of `AV1FilmGrainMode`
  "Defer" to "Immediate" before the host sees it, in `dset` and `dmul`:
  the host applies grain where it decodes.
- Decoder selection drops a decoder for `av01` that is not
  hardware-accelerated unless the session's specification has
  `EnableAppleOnlySWAV1` true. The guest's paravirtualized decoders are
  registered hardware-accelerated ("How the guest finds the host's
  codecs").
- For `av01` alone, a hardware decoder in the registry counts only where
  `VTIsHardwareDecodeSupported('av01')` is true, whose code was not read.

**This host** (`crates/reims-vgpu-video-vaapi/src/va/backend/av1_session.rs`,
core's `host/codec_lists.rs` and `host/decoder.rs`):
- lists `reims-vgpu-video.videodecoder.av1`, "reims-vgpu-video Decoder -
  AV1", [`av01`], rating 400, with no `VTDecoderCapabilities`, as Apple's
  entry, where the driver has `VAProfileAV1Profile0` with 8- and 10-bit
  4:2:0 surfaces (radeonsi: both GPUs);
- answers `dcap` in Apple's shape for profile 0 alone: `VTSupportedProfiles`
  [0], and for it [`110`, `111`], [`08`, `10`] and levels 19, 17 and 16;
- asks in `Dpba` for the format the `av1C` record gives: `420v` or
  `x420` by its `high_bitdepth`, `420f` or `xf20` when its sequence header
  has `color_range` set; `Width`, `Height` and `IOSurfaceProperties` {};
- lists `AV1FilmGrainMode` in `dsup` beside the three every session has,
  reads it as "Immediate", and takes "Immediate" with noErr;
- emits one frame a `dfrm`, a temporal unit showing one frame, at its
  upscaled width and frame height, with film grain applied; an `av1C` in a
  sample buffer's format description that differs from the session's
  reopens the decoder from that frame;
- of a unit of several spatial layers, emits the top layer's frame, as
  Apple's host does; a unit missing its top layer emits the highest it
  has, which no stream here has measured on Apple's;
- shows a frame coded `showable_frame` 0, as Apple's host was first
  measured to, but a key frame shown so only once; measured again,
  Apple's refused it, and ours shows it (above; "Where we differ from
  Apple");
- lends, holds and lets go of the guest's buffers as Apple's host does
  ("When AVD lets each buffer go", above; [core.md](core.md), "AV1's and
  VP9's buffers"): a `Dbuf` for every frame a unit carries, with a `Dpba` before
  it where the frame's size is not the last asked for; each frame shown
  copied into the buffer it was decoded into, and nothing into a buffer
  whose frame is never shown; each buffer let go at Apple's step, its
  `rels` at the next sweep and its `Dfub` after the next step; and
  asynchronously, every step's `Dbuf` first, then the reply, then each
  step's `Dfub`, `Demt` and `rels`, as AVD does when it keeps up with the
  guest. The conformance crate's stand-in guest, whose message order its
  `framemd5 --sequence` writes, gets Apple's order on the nine streams,
  but for the races above, against Apple's synchronous runs and its three
  paced asynchronous ones, on VideoToolbox, both radeonsi GPUs and NVDEC.
  The backends say which frames they decoded: VA-API's and NVIDIA's from
  the codec crate's decoder, VideoToolbox's from the headers alongside its
  decode;
- refuses at `dsta` an `av01` description with no `av1C`, profiles 1 and
  2, and a stream the driver cannot size ([vaapi.md](vaapi.md), "What it
  refuses"). A size is the backend's: each refuses one under its smallest
  with -12911 and no `Dpba`, as Apple's decoder refuses a side under 64
  ("Where we differ from Apple", below). That is 16x16 on radeonsi, by the
  sequence header's largest frame size, and 128x128 on NVDEC;
  VideoToolbox's is AVD's 64. VA-API also refuses 10-bit streams 32 or
  fewer samples wide, whose inter frames radeonsi decodes wrongly
  ([bugs.md](bugs.md), U15).

### VP9

**Apple's host**, on the M3 Max, 2026-10-24: `decs` and `dcap` measured
on Apple's host session with hand-built messages, and the decoder's
behaviour in VideoToolbox decode sessions in a process, with vpxenc
streams ([vaapi.md](vaapi.md), "VP9 on the AMD GPUs"):
- **`decs`** lists `com.apple.videotoolbox.videodecoder.avd.vp9`, "Apple
  Video Decoder - VP9 family", `IsPreferredInternalPlugin` false,
  `VTCodecType` [`vp09`] and `VTRating` 400, and no
  `VTDecoderCapabilities`: the entry gets that key only from a decoder's
  IORegistry capabilities.
- **`dcap` for `vp09`** answers `,dca` as an empty `=pls`, NULL, then
  `,ver` 6, with no error: the code that builds the capabilities has no
  case for VP9 (read).
- **Nothing has to be set up first.** `VTParavirtualizationHostSessionCreate`
  calls `VTRegisterSupplementalVideoDecoderIfAvailable('vp09')`, and the
  host decoder's specification sets `AllowVP9Decode` (read). The answers
  were the same with the call made before the session.
- **The decoder refuses at session creation**: a stream under 64 pixels
  on either side (63x64 and 64x63 as well as 2x2 to 48x48; 64x64, 65x65
  and larger decode) and one of 12 bits with -12911, Profiles 1 and 3
  (4:4:4) with -12910.
- **A frame whose coded size is not the description's** is emitted with
  -12909 and no image: every inter frame after libvpx's dynamic resize
  (p0-720p-resize: 60 frames of 1280x720 decoded, 180 of 960x540 refused;
  p0-720p-resize-early: 1 and 149), and a key frame of another size
  (1080p frames in a 720p description). A key frame of the description's
  size decodes again (720p appended to the resize stream: 120 decoded).
  DecodeFrame itself returns noErr: the status comes through the output
  callback.
- **Its frames are the reference decode's**: on 12 streams, Profile 0 into
  `420v` and Profile 2 into `x420`, odd sizes, 4K at 10 bits, tiles,
  intra-only, shown-existing and segment features among them, every frame
  matched ffmpeg's NV12 or P010 of its own decode (our decode's, at odd
  10-bit widths).
- **There is no software VP9 decoder**: a session without hardware
  acceleration fails with -12906, and so does every `vp09` session of a
  process that has not called `VTRegisterSupplementalVideoDecoderIfAvailable`.

**Apple's host session with the guest's buffers**, measured on Apple's
host session on the M3 Max, macOS 27.0.1, 2026-10-25, lending buffers of
a CoreVideo pool made of each `Dpba`'s whole attributes, as the guest's
VideoToolbox makes its pool, each kept until its `rels`; libvpx streams
from vpxenc and from ffmpeg's `libvpx-vp9`, each decoding with no error
in ffmpeg's `vp9` and in libvpx:
- **`Dpba`**, before the `dsta` reply: {`PixelFormatType` [`420v`] for
  8 bits, [`x420`] for 10, [`420f`] and [`xf20`] with the `vpcC`'s full
  range flag set, `Width` and `Height` the description's,
  `IOSurfaceProperties` {}, `BytesPerRowAlignment` 64, `PlaneAlignment`
  256, `PreferRealTimeCacheModeIfEveryoneDoes` true}, and no
  `ExtendedPixels` keys at any size: 64x64, 66x68, 200x120, 641x361,
  643x363, 1280x720, 1920x1080 and 3840x2160.
- **`dsup`** lists three properties, each with an empty description, the
  same before `dsta` and after: `MaxDecoderPixelRate`,
  `SuggestedQualityOfServiceTiers` and
  `UsingHardwareAcceleratedVideoDecoder`.
- **`dcop`**, before `dsta` and after: `UsingHardwareAcceleratedVideoDecoder`
  true; `MaxDecoderPixelRate` 7365427200 at every size, as AVD's AVC
  decoder answers it too; `SuggestedQualityOfServiceTiers` four tiers,
  {`OnlyTheseFrames` `AllFrames`}, then `NonDroppableFrames`, `IFrames`
  and `KeyFrames`; `OnlyTheseFrames`, which it does not list, `AllFrames`;
  `RealTime`, `MaximizePowerEfficiency`, `AV1FilmGrainMode` and any other
  key -12900 with no value.
- **`dset`** takes nothing: `RealTime`, `MaximizePowerEfficiency`,
  `OnlyTheseFrames` and `MaxDecoderPixelRate` answer -12900,
  `UsingHardwareAcceleratedVideoDecoder` and
  `SuggestedQualityOfServiceTiers` -12901, and each reads as before. The
  30 frames sent after them all came out with their images.
- **A `vp09` description with no `vpcC`**, CoreMedia's of p0-720p's
  sample entry without the box: `dsta` answers -8971, with no `Dpba`, and
  each of 10 `dfrm`s after it 311 with `,inf` 0 and nothing emitted, the
  answer to frames with no decode session ("The decoder session",
  above), synchronously and not. The 10-bit stream was not sent so, as
  AVD could have taken 8-bit buffers for its 10-bit frames.
- **Each frame after one it refuses is refused too, until a key frame of
  the description's size**: -12909 with no image, the description's size
  or not. libvpx's spatial layers at 1280x720 (`vp9-svc 2 1 0 0` and `3 1
  0 0`, [bugs.md](bugs.md), B40) in a 1280x720 description, whose key
  frame is the lowest layer's: every one of 20 superframes, and every one
  of 30 samples of the same streams split into one frame a sample
  (ffmpeg's `vp9_superframe_split`), the top layer's 1280x720 frames among
  them. p0-720p-resize with 60 frames of p0-720p appended, from its key
  frame: 60 frames decoded, 180 refused, then 60 decoded, each luma
  equal to ffmpeg's. Whether a frame that predicts only from frames
  decoded before the first refused one decodes is not measured: no valid
  stream here codes one.
- **`render_size` changes nothing**: p0-720p rewritten by `vp9-craft`
  with a `render_size` of 1920x1080, 1280x704 or 640x360 in every frame
  ([vaapi.md](vaapi.md), "VP9 on the AMD GPUs") gave the same 1280x720
  `Dpba`, no other, and 10 frames of 1280x720 whose luma equalled the
  unrewritten stream's, synchronously and not.
- **A sample over 0x200000 bytes** is sent as the guest sends one, in
  fragments ("The decoder session", above): p0-2160p-big's two key
  frames, 2,976,601 and 2,978,318 bytes (ffmpeg's `libvpx-vp9` at CRF
  10 of `testsrc2` at 3840x2160 under heavy noise, a key frame every
  second frame), each in two `dfrm` messages, the first flagged 0x10.
  Each decoded with one `dfrm` reply, after its last fragment, and every
  frame's luma equalled ffmpeg's, synchronously and not.
- **When AVD lets each buffer go**, as its AV1 decoder lets them go
  ("When AVD lets each buffer go" in "AV1", above), measured
  synchronously and with each frame sent 33 ms after the last one's
  reply, on 25 libvpx streams: p0-720p, p2-720p, p0-720p-arf6 (vpxenc's
  `--auto-alt-ref=6`, up to five hidden frames a superframe), vp9-craft's
  shown-existing and intra-only streams at 8 and 10 bits,
  p0-720p-parallel, -resilient, -aq3 and -features, p0-641x361,
  p0-360p-screen, the three resize streams and p0-720p-resize with
  p0-720p after it, and `vp9-svc`'s two and three spatial layers (`2 1 0
  0`, `3 1 0 0`, `2 1 2 0`, `3 3 0 0`), each whole and split into one
  frame a sample (ffmpeg's `vp9_superframe_split`), each frame's slots
  read with `vp9-info --frames`. Number the frames a sample carries as AVD
  takes them, a step each:
  - **Each step is lent a buffer**: every frame decoded, hidden
    alternate references and lower spatial layers included, every frame
    shown again (`show_existing_frame`), and every frame AVD refuses,
    those after it in its sample too. p0-720p-arf6 had 88 `Dbuf`s and 60
    `Demt`s; `vp9-svc 3 1 0 0`, every sample refused, 180 `Dbuf`s and 60
    `Demt`s. There is no `Dpba` but `dsta`'s, at any size: the resized
    frames and the spatial layers, every one refused, asked for none.
  - **AVD refuses a frame whose size is not the description's**, and
    each frame after it until a key frame of the description's size,
    frames shown again too: that rule gave every sample emitted with
    -12909 in the 50 runs. A frame before it in its sample is decoded:
    `vp9-svc 3 1 0 0` under a 320x180 description has its 320x180 key
    frame decoded and its 640x360 frame refused.
  - **A frame is decoded into its step's buffer, and a frame shown again
    is emitted in that buffer**, as for AV1: the buffer lent to show it
    again is never named by a `Demt`, and a hidden alternate reference's
    picture reaches the guest only if a frame shows it again; libvpx's
    are never shown, and their buffers come back in a `Dfub`.
  - **A buffer is let go at the start of step k**, the latest of: one
    past the step after which no reference slot holds its frame; two past
    the step that decoded into it; and one past the step that lent it to
    show a frame again or for a frame refused. After a frame refused no
    slot holds a frame, so the buffers the slots held go one past it too:
    p0-720p-resize's first refused frame, its 61st, had its `rels` with
    the buffers of the key frame, of the last golden frame and of the
    frame before it. Its `rels` comes with the next sweep and its `Dfub`
    after step k, as for AV1. The rule gave the step of every one of the
    5,628 buffers let go in the 50 runs.
  - **A sample refused**: each of its frames' `Dbuf`s, the sample's
    `Demt` with -12909 and no image at its last frame, before that
    frame's `Dfub`, then the reply: last asynchronously too, where a
    sample decoded has its reply after its `Dbuf`s and before its
    `Demt`; the reply and the `Demt` carry `,inf` 0x1 asynchronously.
  - **Asynchronously** AVD asks for a sample's buffers before it decodes
    its frames, as for AV1, and the races between Apple's threads are
    AV1's.

**The guest**, read in its VideoToolbox and measured in an x86_64 26.7.1
(25G241) guest on this host:
- VideoToolbox finds no `vp09` decoder unless the specification has
  `AllowVP9Decode`, which it adds only for a process for which VP9
  decoding is enabled, as
  `VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9)`
  enables it. In the guest, a decode session without the call got -12906;
  with it, our decoder.
- `VTCopyVideoDecoderList` lists ours as
  `paravirtualized:reims-vgpu-video.videodecoder.vp9`, hardware yes, in a
  process that has not called it. `VTIsHardwareDecodeSupported('vp09')`
  answers no there.
- There is no software VP9 decoder in the guest either: -12906 with
  hardware acceleration off, the call made. So a guest's reference is an
  MD5 list made on the host.
- AVFoundation reads VP9 in MP4, with its `vpcC` (12 bytes, version 1),
  and not in WebM ("Cannot Open"), on 26.7.1 as on 27.

**This host** (`crates/reims-vgpu-video-vaapi/src/va/backend/vp9_session.rs`,
core's `host/codec_lists.rs` and `host/decoder.rs`):
- lists `reims-vgpu-video.videodecoder.vp9`, "reims-vgpu-video Decoder -
  VP9", [`vp09`], rating 400, with no `VTDecoderCapabilities`, as Apple's,
  where the driver has `VAProfileVP9Profile0` with 8-bit 4:2:0 surfaces
  or `VAProfileVP9Profile2` with 10-bit ones (radeonsi: both, both GPUs);
- answers `dcap` for `vp09` with NULL, then `,ver`, as Apple's;
- asks in `Dpba` for the format the `vpcC` record gives: `420v` or `x420`
  by its bit depth, `420f` or `xf20` with its full range flag;
- lists `UsingHardwareAcceleratedVideoDecoder` and
  `SuggestedQualityOfServiceTiers` in `dsup`, and answers `dcop` and
  `dset` of every key as Apple's VP9 decoder does (above), but for
  `MaxDecoderPixelRate`, which it does not list and answers -12900 ("Where
  we differ from Apple", below);
- refuses at `dsta`, as Apple's decoder does, a stream of 12 bits with
  -12911, Profiles 1 and 3 or any chroma but 4:2:0 with -12910, and a
  `vp09` description with no `vpcC` with Apple's -8971, every backend
  alike (`session_refusal`). A size is the backend's, refused under its
  smallest with -12911 and no `Dpba`, as Apple's decoder refuses a side
  under 64: 16x16 on radeonsi, 128x128 on NVDEC, AVD's 64 on
  VideoToolbox. VA-API also refuses 10-bit streams 32 or fewer samples
  wide, as for AV1 above;
- emits each frame a sample shows, hidden frames decoded and not shown,
  and a shown existing frame again with nothing decoded; a frame whose
  coded size is not the last asked for gets a `Dpba` at its own size
  before its `Dbuf`, as an AV1 frame does, and is shown at that size,
  where Apple's decoder emits it with -12909 and no image ("Where we
  differ from Apple", below). NVIDIA refuses a frame larger than the key
  frame ([bugs.md](bugs.md), D16), and VideoToolbox's is AVD's own answer;
- after a frame that does not decode, refuses each frame with -12909 and
  no image, and decodes none, until a key frame decodes, as Apple's
  decoder refuses the frames after one it refuses (above): shown existing
  and intra-only frames too (the codec crate's `vp9/decoder.rs`,
  `Unrecovered`). On NVIDIA a frame NVDEC reports failed at its copy does
  the same from the next sample (`Failures`,
  `crates/reims-vgpu-video-nvidia/src/decode.rs`), though NVDEC reported
  no VP9 frame failed in the damaged-input runs ([nvidia.md](nvidia.md));
- emits each frame at its frame size, whatever its `render_size`, as
  Apple's decoder does: the three `render_size` streams above came out as
  p0-720p's frames, every one matching ffmpeg's, on the RX 7900 XTX, the
  Radeon 780M, the RTX 5080 Laptop GPU and VideoToolbox, synchronously
  and not (2026-10-25);
- takes a sample over 0x200000 bytes in the fragments the guest sends it
  in, as for every codec ("The decoder session", above): p0-2160p-big
  through the conformance suite's stand-in guest, which cuts a `dfrm` as
  the guest's decoder does, every frame matching ffmpeg's on the same
  four (2026-10-25);
- lends, holds and lets go of the guest's buffers as Apple's VP9 decoder
  does ("When AVD lets each buffer go", above; [core.md](core.md), "AV1's
  and VP9's buffers"): a `Dbuf` for every frame a sample carries, decoded,
  shown again or refused; each frame shown in the buffer it was decoded
  into; each buffer let go at Apple's step; and a sample refused emitted
  with its status at its last frame, the reply last. VA-API's and
  NVIDIA's sessions say which frames they took from the codec crate's
  decoder (`vp9::Decoder::steps`), VideoToolbox's from the headers beside
  AVD's decode, a sample AVD refuses refused from its first frame of
  another size, or its first. The conformance crate's stand-in guest,
  whose message order its `framemd5 --sequence` writes, got Apple's order
  on all 25 streams, synchronously and asynchronously, on VideoToolbox
  (the M3 Max, 2026-10-25), by both of the comparison's verdicts but for
  AV1's races: in order, but for `p0-720p-arf6`'s asynchronous run, where
  AVD's hardware sent a sample's first `Dfub` before its last `Dbuf`, as
  it does for AV1; and asks and ends, but for `vp9-svc 2 1 0 0`'s
  asynchronous run, whose `rels` came where Apple's threads put them.
  VA-API (the RX 7900 XTX and the Radeon 780M) and NVDEC (the RTX 5080
  Laptop GPU) got the same on the 17 streams with no frame of another
  size, `p0-720p-arf6`'s asynchronous run by the second verdict alone
  (2026-10-25). NVDEC refuses `vp9-svc`'s larger layers ([bugs.md](bugs.md),
  D16), and got Apple's order by the first verdict in all 16 of their
  runs but for the first frame refused, -12910 where AVD's is -12909.
  VA-API decodes the frames AVD refuses of another size, and NVDEC those
  no larger than the key frame, so their lends follow those frames
  decoded ("Where we differ from Apple", below);
- a `vpcC` in a sample buffer's format description that differs from the
  session's reopens the decoder from that frame.

### Colour and HDR

**Apple's host sends no colour with a frame.** Measured on Apple's host
session on the M3 Max, 2026-10-24, on 29 streams of 512x288, 10 frames
each (x264 0.165.3222, x265 4.3, SVT-AV1 v4.2.0, vpxenc v1.17.0, and
ffmpeg n9.0.2's libvpx-vp9 for VP9's `vpcC` tags; the table below says
what each holds), each decoded by AVD's decoder for its codec:
- **No `,pat` and no `,bat`** in any `Demt`, of any stream:
  - for H.264, HEVC Main and Main 10, and AV1 at 8 and 10 bits: untagged;
    BT.709; BT.601 at full range; BT.2020 with PQ, with mastering display
    and content light level SEI or, for AV1, metadata OBUs; BT.2020 with
    HLG, coded as the transfer or by an alternative transfer SEI; P3 with
    sRGB's transfer; for H.264 also codes CoreVideo has no name for
    (BT.470M, linear, FCC);
  - for VP9, Profiles 0 and 2 with `color_space` unknown, BT.601, BT.709
    and BT.2020, and with `vpcC`'s colour fields set;
  - with the format description CoreMedia makes of the MP4's sample entry,
    which carries the `colr` box's colour, and
    `MasteringDisplayColorVolume` and `ContentLightLevelInfo` once `mdcv`
    and `clli` boxes are added; with the codec record alone; and with a
    `colr` that says otherwise than the bitstream.
- **`,dp2` with every frame**, 544 bytes in one layout for every codec and
  depth: the bytes `1tshdvaa`, six 32-bit words (one that differs between
  sessions, a frame counter from 1, 128, 32, then 0), then 128
  little-endian 32-bit counts. The counts sum to the frame's pixels
  (147,456 at 512x288) and change frame by frame, so it is a histogram of
  the frame; streams of the same pictures with other tags (hevc10-pq,
  hevc10-untagged) gave the same counts. The guest sets it as its
  surface's data property 2 (read). What reads it in the guest is not
  known.
- **`Dpba` has no colour key.** Its format follows the bitstream's depth
  and range, `420v`, `420f`, `x420` or `xf20`: the range is the SPS's
  `video_full_range_flag`, the `av1C` sequence header's `color_range`, or
  `vpcC`'s flag. A `colr` box that said otherwise changed nothing, for
  each codec.
- **`dcap`**: `VTIsHDRAllowedOnDevice` true for `avc1` and for `hvc1`, the
  device's `FigIsHDRAllowedOnDevice()` (read); `av01` without it, and
  `VTMaxHDRPlaybackLevel` 16 for each profile ("AV1", above); `vp09` NULL;
  and `dvh1`, which `decs` does not list, {`VTDoViIsHardwareAccelerated`
  true, `VTDoViSupportedLevels` "01" to "09", `VTDoViSupportedProfiles`
  ["05", "08"]}.

**So the colour a guest's frames carry is the guest's own.** A pixel buffer
that comes back with no colour attachment gets its format description's,
or a guess, from the VideoToolbox that decoded it. Measured in a process on
the M3 Max, 2026-10-24, with VideoToolbox's own AVD, which attaches none
either (the host's `,pat` is its pixel buffer's attachments, read), on the
same 29 streams:
- each hardware frame's attachments are the format description's colour
  extensions: `CVImageBufferColorPrimaries`, `TransferFunction` and
  `YCbCrMatrix`, both chroma locations, `CVFieldCount` and
  `CVPixelAspectRatio`, with a `CGColorSpace` made of them;
- without them VideoToolbox guesses, and says so with `ColorInfoGuessedBy`
  VideoToolbox: at 512x288 SMPTE C primaries, the 709 transfer function
  and the 601 matrix; at 1280x720 and 1920x1080 BT.709 all three;
- the format description AVFoundation makes takes the colour from the
  `colr` box, or else from the bitstream: the SPS's VUI for H.264 and
  HEVC, the sequence header for AV1. It takes the chroma location from the
  bitstream either way, and never reads `vpcC`'s colour, so every VP9
  stream came out guessed, `vp9-709`, whose bitstream says BT.709, among
  them;
- no frame had `MasteringDisplayColorVolume` or `ContentLightLevelInfo`,
  though the PQ streams' SEI and OBUs carry them: CoreMedia makes those
  extensions of `mdcv` and `clli` boxes alone;
- Apple's software decoders do attach colour, of the bitstream's, and it
  wins over the format description's: hevc10-atc-hlg's software frames
  have its VUI's transfer (BT.2020 10-bit, named `ITU_R_709_2`), its
  hardware frames the format description's HLG, from the `colr` box
  ffmpeg wrote from the alternative transfer SEI. H.264's software frames
  also have `CVImageBufferChromaSubsampling` "4:2:0".

**This host** sends what Apple's sends with a frame: no `,pat` and no
`,bat` (`crates/reims-vgpu-video-core/src/host/decoder.rs`,
`emit_decoded_frame`), and a `Dpba` format by the bitstream's range and
depth ("The decoder session", above). Its `dcap` says
`VTIsHDRAllowedOnDevice` true for H.264 and HEVC, as Apple's host does
(`crates/reims-vgpu-video-core/src/host/codec_lists.rs`). Ours decodes and
copies: an HDR stream decodes as any other, the profiles listed say which
streams the backend takes, and whether the guest then tone-maps or shows
HDR is the guest's business. With false, the guest's
`VTGetHEVCCapabilitesForFormatDescription` refuses every HEVC format
description whose dynamic range is HDR. It sends no `,dp2`, and answers
`dcap` for `dvh1` with NULL ("Where we differ from Apple").

**In the guest, on this host, the frames are as on Apple's.** Measured in
an x86_64 26.7.1 (25G241) guest with VtWaitPatch on this host, on
radeonsi, 2026-10-24, with `VTIsHDRAllowedOnDevice` false and then true:
the guest decoded the 29 streams, and every run's frame attachments,
hardware and software, asynchronous and synchronous, were the same as the
M3 Max's run of its kind: tagged streams carry their colour, untagged ones
and VP9 VideoToolbox's guess, as on the M3 Max. The streams hold what they
were written to hold, by our own parsers (the VA-API crate's info examples,
2026-10-24):

| stream | VUI or sequence: range, primaries/transfer/matrix, chroma | HDR metadata |
|---|---|---|
| h264-, hevc-, hevc10-untagged | limited, none, none | none |
| h264-unspecified | limited, none (VUI without colour), none | none |
| h264-, hevc-709 | limited, 1/1/1; H.264 none, HEVC 0,0 | none |
| h264-601-full; hevc-601-full | full, 6/6/6, 1,1; full, 5/6/5, 1,1 | none |
| h264-, hevc-p3-srgb | limited, 12/13/1, 3,3 | none |
| h264-odd | limited, 4/8/4, 5,5 | none |
| h264-2020-pq, hevc10-pq | limited, 9/16/9, 2,2 | SEI: mastering display (G 13250,34500; B 7500,3000; R 34000,16000; white 15635,16450; 1000 and 0.0001 cd/m2), light level 1000/400, once |
| hevc10-hlg | limited, 9/18/9, 2,2 | none |
| h264-2020-atc-hlg, hevc10-atc-hlg | limited, 9/14/9, none | SEI: alternative transfer 18, once |
| hevc10-2020-full | full, 9/14/9, none | none |
| av1-untagged | video, 2/2/2, position 0 | none |
| av1-709; av1-601-full | video, 1/1/1, 1; full, 6/6/6, 2 | none |
| av1-p3-srgb | video, 12/13/1, 0 | none |
| av1-10-pq | video, 9/16/9, 0 | OBUs: mastering display (R 0.68,0.32; G 0.265,0.69; B 0.15,0.06 in 0.16; 1000 cd/m2), light level 1000/400, once |
| av1-10-hlg | video, 9/18/9, 0 | none |
| vp9-untagged, -601, -709, -10-2020 | `color_space` 0, 3, 2, 5; limited | none |
| vp9-709-ffmpeg-tags; vp9-10-pq-ffmpeg-tags | `color_space` 2, full; 5, limited (`vpcC` 2/2/1; 2/2/9) | none |

With `VTIsHDRAllowedOnDevice` true the guest's HDR checks answer as the M3
Max's: `VTGetHEVCCapabilitesForFormatDescription` takes hevc10-pq, -hlg
and -atc-hlg, which it refused with false. Nothing else the guest printed
changed with the flag. Its HEVC dictionary had Dolby Vision's keys both
times, as the M3 Max's has: with no hardware decoder for `dvh1` among its
matching infos, the guest makes them itself from HEVC's Main 10 entry and
does not ask the host (read). Had it asked, ours would answer NULL, and
the merge would have no keys to set. `AVPlayer.eligibleForHDRPlayback` was
false in the guest and on the M3 Max alike.

### The encoder session

**Opening.** `+enc` reads `,cod`, `,ver`, `,cid` and `,gin`. A zero
`,cod`, or a parameter that does not read, is -19093 with no session and
no reply. Otherwise the host selects an encoder for `,cod` with `,cid` as
its encoder ID, makes the session whatever the selection's status, and
replies `,err` (that status), `,ver` 6 and `,hin`. The guest stores the
host's `,ver` and fails its create on a nonzero `,err`. Three kinds of ID
reach `,cid`:
- **An encoder the guest registered from `encs`** sends its entry's
  `CMClassImplementationID`. The guest makes it with a host
  specification whose `VTHostEncoderID` is that ID. The guest encoder
  sends the specification's `VTHostEncoderID` as `,cid`.
- **The guest's two fixed encoders** send
  `com.apple.videotoolbox.videoencoder.ave.avc` and `...ave.hevc`. They
  put the ID in their specification under `kVTVideoEncoder_HostEncoderID`.
  That constant is the string "VTHostEncoderID" (measured on the M3 Max,
  macOS 27.0.1, 2026-10-24, by `dlsym`).
  `kVTVideoEncoderSpecification_HostEncoderID` is not exported, but its
  slot in 26.7 holds the same string. So they are one key.
- **An encoder made without a specification** sends none.

So a host takes its own IDs, Apple's two AVE IDs and none.

**Messages.** Each runs on the session's serial queue, as the decoder's
do. The reply has flag 0x4. A handler whose parameters do not read sends
nothing, and the guest times out: 1.1 s for most, 10 s for `efrm` and
`ecfr`.

| message | reply |
|---|---|
| `esup` | `,err`, `,sup` the supported properties; the guest needs a dictionary |
| `ecop` | `,key` must be a string; `,err`, `,val` when there is one |
| `eset` | `,key` must be a string and not `Paravirtualized`, else no reply; `,val`; `,err` |
| `emul` | `,pro` minus `Paravirtualized` must not be empty, else no reply; `,err` |
| `esav` | `,err`, `,pro` when there are some |
| `esta` | `,vdi` (`=wxh`); a dimension under 1 is -19093. Otherwise the encoder's start, from inside which the encoder asks for its source buffers (`Epba`), so `Epba` comes first; `,err` |
| `epfr` | `,err` |
| `efrm` | `,frm` (`=s64`), exactly one surface (else no reply), `,pts`, `,dur`, `,fpr` frame properties, `,uui` the pixel buffer's UUID, and its attachments; `,err`, `,inf` |
| `emif` | a multi-image frame: `,err`, `,inf` |
| `ecfr` | `,pts`; `,err`, once the encoder has emitted what it held |
| `einv` | `,err` |
| `ebps`, `eeps` | multipass: `,bfl` for `ebps`; `,err`, with `,fpr` first for `eeps` |
| `ests` | `,esp`, `,twh` (`=32s`, under 1 is -19093); `,err` |
| `epti`, `etil`, `ecti` | tiles: `,ept`; `,til`, one surface, `,tio`, `,tia`, `,tip`, `,uui`, answered `,err`, `,efl`; `,err` |

An encoder without the multipass, tile or multi-image calls answers
-12782, `kCMBaseObjectError_UnimplementedFunction`, from the host's own
dispatch.

**Host to guest.** The host sends these when its encoder asks:
- **`Epba`** carries `,pba`, the encoder's source pixel buffer
  attributes, filtered. For a guest of protocol 3 or later it has flag
  0x2, and the host waits 1.1 s for the guest's `,err`. For an older guest
  it has no flags and no wait. The guest adds `IOSurfaceProperties` {} and
  makes its source buffers from it.
- **`Efds`** asks the guest to make the stream's format description:
  `,cod`, `,wdt`, `,hgt` and `,ext` the extensions, with flag 0x2. The
  host waits 1.1 s for `,err` and `=fds`, the description as an
  ImageDescription.
- **`Eemt`** emits an encoded frame, with flag 0x2: `,frm`, `,err`,
  `,efl`, then `,uui` (or `,ui+` for several buffers), then `,sbf` the
  sample buffer. A sample buffer over 0x4000 bytes goes in fragments. Each
  fragment but the last has flag 0x10 and reply ID 0, and later fragments
  hold only their piece of `,sbf`. The last fragment has flag 0x2 and the
  reply ID, and the host waits 10 s for the guest's `,err`. The guest
  gathers the fragments and answers the last one. A sample buffer without
  a format description takes the previous one.
- `Esps` and the multipass storage's `mpsi` to `mpsd`, and the tile
  session's `TE*`, come only from an encoder that uses them. `mpin` can
  come from any encoder ("Multi-pass", below).

**Multi-pass.** The guest's encoder sends `eset` of `MultiPassStorage`
true when its client gives the session a multi-pass storage, and false when
it takes it away. Apple's host translates the property before its encoder
sees it:
- true makes a host storage, whose calls reach the guest's as `mpsi` to
  `mpsd`, and passes the encoder that;
- anything else releases the storage the session holds and passes the
  encoder kCFNull.

Releasing the storage sends `mpin`, with flag 0x2, and waits 1.1 s for
the guest's `,err`, an answer that changes nothing (measured below).
`mpin` is the host session's half of the storage's invalidate; that the
storage's finalize calls its invalidate is a hypothesis, since only the
invalidate was read. The guest invalidates its own storage and answers. A
second true replaces the storage without releasing the first, and the
session's end clears the storage's way back to the session before it lets
it go, so neither sends `mpin`. The encoder's answer to the set is the
reply's.

Measured on Apple's host session on the M3 Max, 2026-10-24: the session
was sent `esup`, `esta` at 640x480, `ecop`, `eset` true, false and true,
`ecop`, `ebps` and `eeps`, and `mpin` was not answered, so each `mpin`
cost its 1.1 s:

| encoder (`,cid`, `com.apple.videotoolbox.videoencoder.`...) | in `esup` | `eset` true, false, true | `mpin` | `ebps`, `eeps` |
|---|---|---|---|---|
| `hevc.vcp`, HEVC in software, one pass | absent; `ecop` -12900 before and after | -12900, -12900, -12900 | once, inside the false | -12782; -12782 with `,fpr` 0 |
| `h264`, H.264 in software | {} | 0, -12902, 0 | once, inside the second true: the encoder kept the first storage when it refused kCFNull, and let it go for the second | 0; -12912 with `,fpr` 0 |
| `ave.avc`, `ave.hevc` | {`ReadWriteStatus` `ReadWrite`} | not sent | none | -17691: AVE does not start behind a host session in a process of ours ("Measured on macOS 27", below), so `esta` was -17691 |

So a guest learns that an encoder has no multi-pass from `esup` and from
the -12900, and the release of a storage it asked for still reaches it as
`mpin`.

**Pending frames.** `efrm` keeps the frame's pixel buffer, and so its
surface, with its UUID, keyed by `,frm`. The frame is let go when the
encode call has returned and the frame has been emitted, or at once when
the encode fails. `Eemt` takes the UUIDs before it sends, and the
surfaces are swept after each emission and each message. So a frame's
`rels` follows its `Eemt` reply, or its `efrm` reply when it is emitted
from inside the encode. CompleteInvalidate waits for every pending frame
to be emitted, logging every 2, 4, 8... seconds.

**Measured on macOS 27**, on Apple's host session on the M3 Max,
2026-10-24, as for the decoder. Apple's AVE encoders open, answer their
properties and start, but cannot encode in an unentitled process:
`AVE_UC_Start` fails with 0xe00002c2 and `efrm` answers -12902. So the
frame exchange was measured with Apple's software encoders,
`com.apple.videotoolbox.videoencoder.h264` and `...hevc.vcp`, named in
`,cid`. These facts are likely for 26.7, and stay hypotheses until a 26.7
guest confirms them.
- `encs` listed nine encoders. `ave.avc` is {`CMClassImplementationID`,
  `CMClassImplementationName` "Apple Video Encoder - AVC family",
  `VTCodecName` "H.264", `VTCodecType` 1635148593 (`avc1` as a number),
  `VTRating` 400}. `ave.hevc` has "Apple Video Encoder - HEVC family",
  "HEVC" and 1752589105. `Hide` appears only where it is true.
- `+enc` answered `,err` 0, `,ver` 6 and `,hin` {`VTHostBuildVersion`},
  with `ave.avc` in `,cid` and with no `,cid`. An unknown ID, or
  `ave.hevc` with `,cod` `avc1`, answered -12908.
- **While every hardware encoder session is taken.** With a process
  holding all 32 of the M3 Max's hardware encoder sessions, the 33rd
  answering -12903, `encs` listed the same nine encoders, AVE's two among
  them: it reads the registry alone. `+enc` with `ave.avc` or `ave.hevc`
  in `,cid` answered -12912, the encoder's creation status, and the
  session's `esup` and `esta` got no reply. So a guest is offered AVE
  whatever the Mac's state, and its session fails at its creation. The
  VideoToolbox backend answers the same. On NVIDIA a session past the
  GPU's limit fails at its first `efrm` instead, by reading
  ([bugs.md](bugs.md), B39).
- **AVE's properties.** `esup` entries are {`PropertyType`,
  `ReadWriteStatus`, `SupportedValueList` for an enumeration}. `ecop` of
  `UsingHardwareAcceleratedVideoEncoder` is true; numbers and booleans
  not yet set read 0. `eset` of an unknown key or a wrong type is -12900.
  `emul` is 0, `esav` -12782 and `epfr` 0; the software H.264 encoder's
  `epfr` is -12782. `ecop` of `Paravirtualized` or of an unknown key is
  -12900 from the software encoders, but AVE traps on it and the host
  process dies.
- **`esta`** gives `Epba` (flag 0x2, reply ID 100), the guest's reply,
  then the `esta` reply. A size AVE does not take is answered -19354 at
  once, with no `Epba`: H.264 at 8192x8192 and 16384x16384, HEVC at
  16384x16384; HEVC at 8192x8192 and H.264 at 4096x2304 went on to
  `Epba`. The status is the encoder's start's, sent as it is. AVE's
  attributes are {`BytesPerRowAlignment` 64, `PlaneAlignment` 64,
  `CacheMode` [six modes], `IOSurfaceWiringAssertion` 1,
  `IOSurfaceProperties` {}, `Width`, `Height`, `PixelFormatType` [19
  formats, `420v` and `420f` first]}. An odd size went on to `Epba` as an
  even one did, from 1x1 to 641x361: every `esta` there answers -17691
  after `Epba`, even sizes too, so the measurement tells a refusal (no
  `Epba`) from a start and nothing more.
- **Odd sizes, encoded.** Through VTCompressionSession in a process of
  its own on the M3 Max, where AVE encodes (2026-10-24), AVE took H.264
  and HEVC at 641x361, 640x361 and 641x360. Its pixel buffer attributes
  kept the odd `Width` and `Height`, and the format description and the
  stream (ffprobe) were 640x360: each side rounded down to even. The
  decoded frames matched the source's top left at 46.8 dB of luma, as an
  even 640x360 encode did, and at 16 dB one pixel off, so the last column
  and row are left out, not scaled. Apple's software encoders do
  otherwise: the format description keeps the odd size and the stream is
  rounded up (642x362; HEVC coded 648x368). The guest's fixed encoders
  name AVE, so a guest of Apple's host gets AVE's. That the host
  session's AVE does the same is a hypothesis: AVE does not encode behind
  a host session in a process of ours (above).
  On each `efrm` it failed, AVE sent a second `Epba` narrowed to the
  frame's `420v`. The software H.264 encoder's are
  {`ExtendedPixels…` 0, `PixelFormatType` [`420v`, `420f`, `2vuy`]}.
  `hevc.vcp`'s have alignment 128 and `x420`, `xf20` and the 8-bit
  formats.
- **The first emission** gives `Efds` (flag 0x2: `,cod`, `,wdt`, `,hgt`,
  `,ext` {`FormatName`, `SampleDescriptionExtensionAtoms` {`avcC` or
  `hvcC`}}), the guest's `,err` and `=fds`, then `Eemt`.
- **The order.** The software H.264 encoder holds every frame until
  `ecfr`, so each `efrm` is answered `,err` 0 and `,inf` 0 at once. Then
  for each frame in turn: `Eemt` (`,efl` 0), its reply, `rels` for that
  frame's surface. Then the `ecfr` reply. `hevc.vcp` emits from inside the
  encode: `efrm`, [`Efds`], `Eemt`, its reply, the `efrm` reply, then
  `rels`.
- **The sample buffer** has `opts` (the PTS), one `stia` entry {duration,
  PTS, DTS equal to the PTS}, `sdat` (4-byte lengths, SEI NAL units
  first), `nsmp` 1, `ssiz`, and `fdsc` on the first sample only. Its
  `sary` is [{`DependsOnOthers` false, `EarlierDisplayTimesAllowed`
  false}] for a sync sample and [{`NotSync` true, `DependsOnOthers` true,
  `EarlierDisplayTimesAllowed` false}] otherwise.
- **The `fdsc`** is what the host made of the guest's `=fds`, not what it
  sent in `Efds`. Its extensions are `FormatName`, `SpatialQuality` 512,
  `TemporalQuality` 512, `Version` 0, `RevisionLevel` 0,
  `VerbatimSampleDescription` (the `=fds` bytes), both
  `CVImageBufferChromaLocation…Field` "Left", `CVPixelAspectRatio` {1, 1},
  `SampleDescriptionExtensionAtoms`, `FullRangeVideo` false,
  `CVFieldCount` 1 and `Depth` 24. Neither the encoder nor the guest had
  set chroma location.
- **Fragments.** A 640x480 frame's `Eemt` came in 0x4000-byte messages:
  flag 0x10 and reply ID 0 on all but the last, which had flag 0x2.
- Neither software encoder honoured `kVTEncodeFrameOptionKey_ForceKeyFrame`
  at frame 5, nor made B-frames with `AllowFrameReordering`.
- `einv` answered 0, and `-enc` returned noErr.

**This host's encoder** follows that order, with a backend encoder where
Apple's has AVE (`crates/reims-vgpu-video-core/src/host/encoder.rs`):
- **`+enc`.** The session opens for a codec the encoder backend lists, with
  `,cid` naming ours (`reims-vgpu-video.videoencoder.avc`, `.hevc`),
  Apple's AVE encoder for that codec, or nothing. Any other ID, or an
  unlisted codec, keeps the refused session and its -12908.
- **The properties** are AVE's shape, from the backend's capabilities
  (`src/host/encoder/properties.rs`). They are `ProfileLevel` (each
  profile listed, at AVE's H.264 levels and AutoLevel), the bit rates,
  `Quality`, the key frame intervals, `ExpectedFrameRate`,
  `AllowFrameReordering`, `RealTime`, `H264EntropyMode` for H.264, and the
  colour keys. `UsingHardwareAcceleratedVideoEncoder` reads true and is
  read-only (-12901). `Paravirtualized` reads true, and `eset` and `emul`
  never reach it, as with Apple's host. `MultiPassStorage` is answered as
  Apple's host answers it for `hevc.vcp`, an encoder of one pass: it is
  not listed, `ecop` and every `eset` are -12900, and an `eset` of
  anything but true after a true sends `mpin` first, whose answer is not
  read (`Worker::set_multi_pass_storage`; unit test
  `multi_pass_storage_is_refused_and_its_release_invalidates_the_guests`).
  An unknown key or a wrong type is -12900, as from Apple's
  software encoders, not AVE's trap. So is a number outside the range
  AVE takes (`in_range` in `properties.rs`), measured through
  VTCompressionSession in a process, where AVE runs, H.264 and HEVC alike
  (the M3 Max, macOS 27.0.1, 2026-10-24):
  - `Quality` from 0 to 1, NaN and infinity refused;
  - `ExpectedFrameRate` from 1 to under 100000 (0.99 and 100000
    refused, 99999.9 taken), NaN and infinity refused;
  - `MaxKeyFrameIntervalDuration` 0 or more, infinity taken, NaN refused;
  - `AverageBitRate` and `MaxKeyFrameInterval` read as signed 32-bit
    integers and refused when negative: 4294967296 is taken (its low 32
    bits are 0), 2147483648 and 4294967295 are refused, a real is cut
    toward zero (-0.5 taken, -1.5 refused);
  - any array of numbers for `DataRateLimits`, of any length, zeros and
    negatives among them.

  That these are AVE's checks, not the session layer's above it, is a
  hypothesis; a guest is refused either way, by its own VideoToolbox or
  by the host. A setting made after `esta` reaches the running encoder.
- **`esta`** opens the backend encoder at `,vdi`, then sends `Epba` with
  {`PixelFormatType` the backend's source formats, `Width`, `Height`,
  `IOSurfaceProperties` {}, `BytesPerRowAlignment` 64, `PlaneAlignment`
  64}. A size over the backend's maximum is -19354, with no `Epba`, as
  from Apple's host. An odd size is taken as AVE takes it: `Epba` and the
  source frames keep it, the backend encodes each frame's top left at the
  size rounded down to even, and `Efds` describes that size
  (`encoded_size` in `crates/reims-vgpu-video-core/src/host/encoder.rs`).
  So every backend gives the guest AVE's odd sizes (the conformance
  suite's `-641x361` cases: VA-API on both radeonsi GPUs, NVENC on the
  RTX 5080 Laptop GPU, VideoToolbox on the M3 Max, 2026-10-24). A side of
  1 rounds to 0: `esta` answers -12902 with no `Epba`. AVE went on to
  `Epba` at 1x1 on Apple's host session; what it does with such frames is
  not measured. A backend that cannot open is -12902, a backend
  malfunction -12912, and a status of the backend's own API, which only
  the VideoToolbox backend gives, goes as it is, as Apple's host sends
  its encoder's ([backends.md](backends.md), "How errors reach the
  guest"). A second `esta` first completes the frames the encoder before
  it holds, as `ecfr` does, and then opens a new one. Apple's host starts
  its one encoder again; what AVE does then with frames it holds is not
  measured.
- **`efrm`** copies the frame out of its surface, and hands the copy to
  the backend, with no lend of the surface held ("Where we differ from
  Apple"), with its times and whether `,fpr` forces a key frame: its
  `kVTEncodeFrameOptionKey_ForceKeyFrame`, whose string is
  `EncoderForceKeyframe` (the SDK's constant, printed on macOS 27.0.1 and
  in an x86_64 26.7.1 guest). A format the backend does not take, or a
  size other than `esta`'s, is -12902. `efrm` before `esta` is -12903. A
  frame the backend does not take fails `efrm` with the backend's status,
  and is never emitted. Each frame the backend returns is emitted at once,
  before the `efrm` reply. A backend that holds frames emits them at
  `ecfr`, as the software H.264 encoder did. A held frame keeps its
  surface until its `Eemt` is answered, so its `rels` follows, whichever
  thread's sweep sends it: the measured order above.
- **A frame the backend fails** after taking it is emitted in its place
  with the backend's status, `,efl` 0 and no sample, as Apple's host
  forwards each of its encoder's emits with its own status and flags, and
  its surface goes. The frames given back with it keep their samples:
  VideoToolbox's callback gives each frame its own status, and NVENC each
  bitstream its own lock's. Where a failure names no one frame, a frame
  coded in the same run as a picture that failed, or one NVENC still held
  when its output could not be read, fails -12912 with it. The encode
  driver then codes the next frame as an IDR picture. Tested with
  stand-ins only (core's `host::encoder::tests`, the codec crate's
  encoder and session tests and fuzzer): no backend is known to fail a
  frame it took.
- **A frame the backend loses**, one it never gives back, is answered at
  the next `ecfr` or `esta`, once the backend has given all it holds:
  `Eemt` with -12912 and no sample, so that the client hears of every
  frame it gave, and its surface goes.
- **The bitrate.** Apple's AVE takes any `AverageBitRate` and makes what
  its quantiser can (through VTCompressionSession on the M3 Max,
  2026-10-24): 640x480 H.264 at 1 and at 50,000 bit/s gave the same
  265,688 bit/s stream, at 25.2 dB, and 100,000 gave 301,736; 1280x720 at
  1 and at 100,000 gave 642,872. Ours refuses none either. The VA-API
  backend gives the driver no rate and no buffer under 32,000 bit/s, where
  VCN's output has stopped falling ([vaapi.md](vaapi.md), "The bitrate
  floor"), so 640x480 H.264 at 1 gives about 294 kbit/s of `testsrc2`.
  `ecop` reads back what the guest set.
- **The rate properties, on AVE.** Measured through VTCompressionSession
  in a process, where AVE encodes, and through the VideoToolbox backend in
  the conformance suite, H.264 and HEVC alike (the M3 Max, macOS 27.0.1,
  2026-10-24). That AVE behind Apple's host does the same is a
  hypothesis: AVE does not encode behind a host session in a process of
  ours.
  - `AverageBitRate` is met: 1.00 to 1.02 of it over 10 s of 720p. On
    harder frames too, through the VideoToolbox backend in the
    conformance suite's `encode` (2026-10-25): heavy noise at 352x288
    (ffmpeg's `noise=alls=40:allf=t+u` over grey) at 5 Mbit/s,
    `mandelbrot` at 1280x720 at 3 Mbit/s (HEVC 2) and `testsrc2` at
    1920x1080 at 4 Mbit/s, H.264 and HEVC, with a key frame at the start
    only, every 2 s, and with reordering. As a share of the average:

    | | first second | any later second | over 3 s | over 10 s |
    |---|---|---|---|---|
    | `AverageBitRate` | 0.95 to 1.27 | 0.82 to 1.26 | 0.97 to 1.16 | 0.98 to 1.04 |
    | `DataRateLimits` alone, at 1.5 times those | 1.00 to 1.24 | 0.83 to 1.10 | 1.00 to 1.01 | 1.00 |

    The shares under `DataRateLimits` are of two thirds of the limit;
    its busiest second came to 0.67 to 0.82 of the limit. AVE does not
    fill pictures cheaper than their share: static bars at 640x480 at
    5 Mbit/s came to 0.01 of it or less with either property, and the encode
    check's waves at 640x360 and 3 Mbit/s to 0.30 to 0.94. The NVIDIA
    and VA-API backends peak a stream at `AverageBitRate` alone at 1.05
    times it, with a buffer of a second ([nvidia.md](nvidia.md), "The
    bitrate through the session"; [vaapi.md](vaapi.md), "The host
    session's encoders"): over 10 s NVENC made 1.01 to 1.09 of it and
    radeonsi 0.94 to 0.99, and NVENC's first second runs over AVE's
    ([bugs.md](bugs.md), B87).
  - `DataRateLimits` alone gives 0.66 to 0.67 of the limit, every second
    under 0.72 of it: the "safe margin below" the limit that
    VideoToolbox's header promises when no average is set
    (`VTCompressionProperties.h`). With an average under the limit the
    average is met, every second at 0.65 of the limit or less. The
    VA-API and NVIDIA backends peak at two thirds of a limit, with an
    HRD buffer of half a second, so that no second passes it, and aim
    at the peak where no average is set ([vaapi.md](vaapi.md), "The host
    session's encoders").
  - `DataRateLimits` is taken only before the session is prepared: set
    after `VTCompressionSessionPrepareToEncodeFrames`, before the first
    frame, it was answered 0 and the stream came out at the encoder's own
    rate. `AverageBitRate` set then was met. With no prepare call, a limit
    set before the first frame was kept, and one set before the second
    lost, as after a prepare: the session prepares at its first frame
    (300 frames of 720p H.264 at a limit of 1.5 Mbit/s with no prepare
    call: 5.4 Mbit/s lost, every second at 0.74 of the limit or less
    kept, the M3 Max, 2026-10-24). Apple's host prepares AVE at the
    guest's `epfr` and nowhere else, not at `esta`.
  - **When the guest sends `epfr`.** Once a session, always before its
    first `efrm`: the guest's encoder sends it from its
    `PrepareToEncodeFrames`, which VideoToolbox calls at the client's
    `VTCompressionSessionPrepareToEncodeFrames` and, where the client
    never calls it, inside the client's first
    `VTCompressionSessionEncodeFrame`, after every property set before
    that frame, 5.6 to 10.2 ms before the `efrm`. Each session sent 8
    `ecop`s, and one of them moved with `epfr`, so it is likely the
    prepare's own. Measured in all 12 sessions of an x86_64 26.7.1
    (25G241) guest on this host, H.264 and HEVC at 1280x720 with and
    without the client's prepare (2026-10-24). So behind Apple's host a
    `DataRateLimits` set after `esta` reaches AVE before its prepare, and
    is kept, unless the client set it after preparing the session itself;
    then AVE has been prepared, and the limit is answered 0 and lost, as
    in process. The VideoToolbox backend prepares its session at `epfr`
    too (`BackendEncoder::prepare`), or at the first frame where none
    came, so it keeps and loses the same limits. The VA-API and NVIDIA
    encoders have nothing to prepare and take a limit whenever it comes,
    as they take a change mid-stream (below): in that guest a limit set
    before the client's prepare, after it, and with no prepare made the
    same stream, byte for byte.
  - **Changed mid-stream**, by a set between frames fed faster than real
    time, with `RealTime` false or unset, `AverageBitRate` and
    `MaxKeyFrameInterval` are answered 0 and not taken: 1 Mbit/s then 3
    at frame 150 gave 1.06 and 1.19 Mbit/s, and an interval of 60 then 10
    at frame 45 kept key frames at 0, 60, 120. With `RealTime` true the
    bitrate was taken in part, 1.10 then 1.77 Mbit/s. The VA-API and
    NVIDIA backends take both. A guest's client
    feeds in real time, so what it sees of AVE here is not measured.
  - `Quality` is honoured: 0.25 and 0.75 gave 93 and 685 kB of the same
    60 frames of H.264, 82 and 424 kB of HEVC.
  - `ForceKeyFrame` is honoured with reordering too: frames 17 and 44,
    each a B picture's place in the pyramid, came out as IDR pictures,
    sync, by their NAL unit types (the conformance suite's `-force`
    cases). Apple's software encoders do not honour it (above).
  - With reordering, AVE's pyramid reorders by 2: frames 0 4 2 1 3 in
    decode order. Its HEVC SPS says `sps_max_num_reorder_pics` 2; its
    H.264 SPS has no bitstream restriction, so a decoder allows the most.
    The VA-API and NVIDIA backends reorder by 1, one B picture between
    references; HEVC's SPS says 1 on both, and H.264's says 1 on VA-API
    and nothing on NVENC. No guest property asks for more.
  - **Full range.** From `420f` and `xf20`, AVE's SPS has
    `video_full_range_flag` 1 and the stream keeps black and white, 0 and
    255, decoded back; from `420v` and `x420` the flag is 0. The VA-API
    and NVIDIA backends do the same (the suite's `-420f` and `-xf20`
    cases).
  - A size past the encoder's largest, and 3840x2160 where an encoder
    lists less, is refused at `esta` with -19354 and no `Epba`, by every
    backend too (the suite's `-refused` cases).
- **Decode times.** Apple's host gives its encoder an encoder session
  with VideoToolbox's queue of the PTSes sent, the queue a compression
  session sets up for its encoder too. It forwards each sample buffer as
  the encoder emits it. Through a compression session on the M3 Max,
  AVE's H.264 and HEVC encoders with `AllowFrameReordering` gave each
  sample the PTS of the frame sent in its place as its DTS: frames 0 4 2 1
  3 came out with DTSes 0 1 2 3 4 frames, so a B picture's DTS was up to
  two frames after its PTS (macOS 27.0.1, 2026-10-24). That the guest on
  Apple's host sees the same is a hypothesis from that one queue: AVE
  does not encode behind a host session in a process of ours. The
  VideoToolbox backend sends VideoToolbox's own DTS, and the VA-API and
  NVIDIA backends make it by the same rule (the codec crate's
  `DecodeTimes`): for the same 60 frames every backend gave sample `k` the
  PTS of frame `k`, in the encode suite on both radeonsi GPUs, the RTX
  5080 Laptop GPU's NVENC and the M3 Max's AVE (2026-10-24). Which frame
  each sample holds is the encoder's: AVE emitted 0 4 2 1 3, the others
  0 2 1 4 3.
- **`Efds`** is sent before the first emission and whenever the backend's
  configuration record changes, with {`FormatName`,
  `SampleDescriptionExtensionAtoms`, and the colour the guest set}. The
  samples' `fdsc` has the measured extensions, built from the guest's
  `=fds`.
- **`einv`** drops the backend encoder and its pending frames, and their
  surfaces' `rels` comes before its reply. Apple's host keeps a pending
  frame's buffer until the frame is emitted. Whether Apple's encoders emit
  or let go of held frames on invalidation is not measured.
- **`epfr`** prepares the running encoder, and answers with its status,
  as Apple's host answers with its encoder's: the VideoToolbox backend's
  `VTCompressionSessionPrepareToEncodeFrames`, and 0 from the VA-API and
  NVIDIA backends, which have nothing to prepare. Before `esta` it is 0,
  as AVE's was.
- **Unimplemented.** `esav`, the multipass, tile
  and multi-image calls answer -12782, `eeps` with `,fpr` 0, as
  `hevc.vcp`'s do ("Multi-pass", above).

### The motion-estimation session

A guest's `VTMotionEstimationSession` (macOS 26's API) finds its
processors through the host, and each processor it makes is a
`ParavirtualizedMotionEstimationProcessor` that forwards every call to a
host session, one message a call. The x86_64 guest has its half: the
processor's log strings are in 25G241's `dyld_shared_cache_x86_64h`
VideoToolbox (read 2026-10-24).

**Finding processors.** The guest asks `meps` once, with `,ver` 6, and
needs a reply `,ver` of 6 or more and a `,mep` array of dictionaries. It
registers each entry under `paravirtualized:<CMClassImplementationID>` and
`paravirtualized:<CMClassImplementationName>`, with the entry's
`CMMatchingInfo` plus `VTIsParavirtualized` true, and keeps the host's ID
under `HostProcessorID`. The host lists the processors whose matching info
has both `VTIsParavirtualizationAware` and `VTIsHardwareAccelerated` true,
each as {`CMClassImplementationName`, `CMClassImplementationID`,
`CMMatchingInfo`}. The guest's `VTMotionEstimationSessionCreate` then
takes the first processor whose matching info has
`VTIsHardwareAccelerated` true, or the one a `HostProcessorID` option
names, and fails with -19350 where there is none: an x86_64 guest has no
processor of its own, so without the host's it has no motion estimation.

**The specification.** The guest's session copies its client's options,
adds `TrueMotion` true when the client asked for `UseMultiPassSearch`,
drops `AllowClientProcessEstimate`, and adds the host's `HostProcessorID`.
The processor sends that dictionary as `+mep`'s `,meo`. The session
answers `MotionVectorSize` (16 by default), `UseMultiPassSearch`,
`TrueMotion`, `Label`, `IncludeMotionConfidence` and `ClientPID` itself,
and forwards every other `VTSessionCopyProperty` to the processor. It
passes every `VTSessionSetProperty` to the processor and drops its
answer. Its supported-property call fails with -12902, since the guest
processor's answers -4, so a guest never sends `mesu`; in an x86_64
26.7.1 guest on this host the call answered -12902 and sent the host
nothing. When the client asks for `IncludeMotionConfidence`, the create
asks the processor for it (`mcop`) and fails unless it reads true.

**The guest's waits.** `+mep`, `mest`, `mcop`, `mset`, `mmul`, `msav` and
`minv` wait 1.1 s for their reply; `mepf` and `mcfr` wait 10 s
(`kVTParavirtualizationCodecOperationReplyTimeout`), so on an x86_64 guest
without VtWaitPatch they fail at once with -19092 ("What an x86_64 guest
has"). `-mep` is sent from the processor's finalize, with no flags and no
wait.

| message | the host's answer |
|---|---|
| `+mep` | reads `,ver`, `,meo` and `,gin`; one that does not read is -19093 with no session and no reply. Otherwise the session is made whatever the processor's creation gives, and the reply is `,err` (that status), `,ver` 6, `,hin` {`VTHostBuildVersion`}. A UUID that has a session is -19093 with no reply |
| `mest` | `,vdi` (`=wxh`); the processor's start, from inside which it sends `Mspa` and `Mvpb`; then `,err` |
| `mepf` | exactly two surfaces, `,rid` (`=s64`), `,ui+` of exactly two UUIDs, the reference's then the current's, `,mef` the frame flags, `,meo` the frame options; else no reply. The processor's process-frame status in `,err` |
| `mcfr` | the processor's complete-frames; `,err` |
| `mcop` | `,key` must be a string, else no reply; `,err`, then `,val` when there is one |
| `mset` | `,key` must be a string, else no reply; `,val`; `,err` |
| `mmul` | `,pro`; the processor's set-properties; `,err` |
| `msav` | `,err`, then `,pro` when there are some |
| `minv` | invalidates the processor; `,err` |
| `mesu` | nothing: no reply |
| anything else | logged, no reply |
| `-mep` | at the host session: invalidates the session, waits for its frames, drops it; noErr, also for a UUID with none |

Each runs on the session's serial queue, and a guest reply (flag 0x8)
goes straight to its reply clerk with the one surface it carries. `-uui`
and `-all` leave motion-estimation sessions alone, and the host session's
invalidate does not reach them; its complete-invalidate waits for each
one's frames and drops it.

**What the host asks of the guest.** Each has flag 0x2 and waits for the
guest's `,err`, 1.1 s, except `Memt`, 10 s:
- **`Mspa`**, `,pba` the source pixel buffer attributes, which the guest's
  session takes as its sources' attributes, converting a client's frames
  that do not match;
- **`Mvpb`**, `,pba` the motion vector pixel buffer attributes, from which
  the guest makes a pool. The host keeps them for the buffers it is lent.
  The guest's create fails with -12902 unless the processor's start set
  both;
- **`Mbuf`**, `,rid` and `,pop` options when there are some: the guest
  makes a motion vector buffer from its pool and replies `,err`, `,uui`
  and the buffer's surface;
- **`Memt`**, `,rid`, `,err`, `,inf`, `,mad` when there is additional
  info, the vector buffer's attachments (`,pat`, `,bat`, `,dp0`...) and
  `,uui` when there is a buffer. The guest calls its client's handler with
  the status and the buffer, which it forgets, and replies with the
  handler's status;
- **`Mdpb`**, with no flags and no reply: `,ui+`, the UUIDs of the pixel
  buffers the host made of guest surfaces and has let go, which the guest
  forgets. The host sends it from the processor's clean-up after each
  frame.

A UUID names a guest pixel buffer to the host: the guest gives each one it
sends a UUID, the same one each time it sends it again, and the host
keeps a weak pixel buffer per UUID, refusing as malformed a UUID it knows
with another surface.

**Measured on macOS 27**, on Apple's host session on the M3 Max,
2026-10-24, driven as the guest's processor would drive it, on 256x128
`420v` frames of the conformance crate's texture, the current one moved by
a known vector. These facts are likely for 26.7, and stay hypotheses until
a 26.7 guest confirms them.
- **`meps`** listed one processor: {`CMClassImplementationID`
  `com.apple.videotoolbox.motionestimationprocessor.ave`,
  `CMClassImplementationName` "Low Resolution Motion Estimation",
  `CMMatchingInfo` {`VTIsParavirtualizationAware` true,
  `VTIsHardwareAccelerated` true}}, and `,ver` 6. `,ver` 5 was -19960
  with no reply.
- **`+mep`** naming that processor answered `,err` 0, `,ver` 6 and `,hin`
  {`VTHostBuildVersion`}; naming another, `,err` -19350; with no `,meo`,
  `,err` 0, the first processor.
- **`mest`** sent `Mspa`, then `Mvpb`, then replied `,err` 0. `Mspa`'s
  attributes were {`Width`, `Height` the session's, `PixelFormatType` a
  list of 83 formats, `420v` and `420f` first, 4:2:2, 4:4:4, `L008`,
  `L010` and Apple's compressed formats among them, `BytesPerRowAlignment`
  64, `PlaneAlignment` 64, `IOSurfaceProperties` {}, `CacheMode` [six
  modes], `IOSurfaceWiringAssertion` true}. `Mvpb`'s were {`Width`,
  `Height` the vector grid, `PixelFormatType` `2C0h`
  (`kCVPixelFormatType_TwoComponent16Half`), `IOSurfaceProperties` {}}.
  A second `mest` sent both again.
- **The grid** is one vector per 16x16 block, with `MotionVectorSize` 16,
  the default, or 8: (ceil(W/16), ceil(H/16)), at least 10 by 4. With 4,
  each 16x16 block has 4x4 vectors: 4 x ceil(W/16) by 4 x ceil(H/16), at
  least 40 by 16. So 1920x1080 is 120x68 at 16 and 480x272 at 4, and
  16x16 is 10x4. `mest` started every size from 2x2 to 16384 on a side up
  to 16384x8192, and answered -12902, with no `Mspa`, a side of 1, 0 or
  less, past 16384, and 16384x16384. A `MotionVectorSize` other than 4
  (0, 1, 2, 8, 12, 32 and -4 were tried) gave the 16 grid, and `mcop`
  read 16 back. The cells past the picture are (0, 0): at 64x32 the
  picture covers 4 by 2 of the 10 by 4 cells, and the other 32 read
  (0, 0) in each of four frames.
- **A vector** is two half floats, x then y, in pixels: where the
  current frame's block is found in the reference, relative to the
  block. Content moved by (+8, +4) gave (-8, -4) in 122 of 128 blocks;
  the rest were edge blocks whose content came from outside the
  reference. Whole shifts of (3, -2) at 4, and (20, -12) at 16, came back
  exactly. Every vector seen was a whole number of pixels: shifts of
  (2.5, 1.25) and (0.5, -0.25) gave none of their fractions. Rows are 64
  bytes apart for 16 vectors, 256 for 64.
- **Its precision** is 4 pixels at 16, and 1 at 4. Every vector on 16x16
  blocks was a multiple of 4, the nearest to the way back, and at a tie
  the blocks split between the two: content moved by (-12, 6) gave
  (12, -4) in 61 of 128 blocks and (12, -8) in 28; (4, 2) gave (-4, 0) in
  61 and (-4, -4) in 31; (5, -7) gave (-4, 8) in 111; (2, 2) gave (0, 0),
  (0, -4), (-4, 0) and (-4, -4), none of them (-2, -2). On 4x4 blocks
  whole shifts came back exactly. The M3 Max's own
  `VTMotionEstimationSession`, the same processor, gave the same
  ([videotoolbox.md](videotoolbox.md), "Motion estimation").
- **A frame's order:** the `mepf` reply (`,err` 0), about 1 ms after it
  is sent; then `Mbuf` (`,rid`, no `,pop`), its reply, `rels` for the two
  source surfaces, `Memt` (`,rid`, `,err` 0, `,inf` 0, `,uui`; no `,mad`
  or attachments), its reply, then `Mdpb` naming the two source UUIDs
  and, at the next delivery's sweep or before `Mdpb`, `rels` for the
  vector buffer's surface. With `,mef` 1
  (`kVTMotionEstimationFrameFlags_CurrentBufferWillBeNextReferenceBuffer`)
  the processor kept the current frame: `rels` and `Mdpb` named the
  reference alone, and the current frame's surface was let go after the
  next frame that used it as its reference.
- **Malformed frames:** one surface, or one UUID, got no reply. The two
  UUIDs swapped were taken, the vectors the same.
- **The processor's properties:** `mcop` read `IncludeMotionConfidence`
  false, `MotionVectorSize` the specification's (16 for 8), and
  `UseMultiPassSearch` and `TrueMotion` the specification's, false unless
  it asked. `PropertyMode` read 0 and `VTIsHardwareAccelerated` false.
  `mset` of `TrueMotion`, `PropertyMode` and `ClientPID` answered 0, and
  `TrueMotion` and `PropertyMode` read back what was set; of an unknown
  key or `Paravirtualized`, -2002. With multipass asked for, 105 of 128
  vectors were the shift, 18 were (0, 0).
- **The rest:** `mcfr` 0; `mmul` of a dictionary 0, of NULL -12902;
  `msav` -12782; `minv` 0; `mesu` and an unknown type, noErr and no reply;
  `-mep` noErr, and a message to its UUID after it -19094.

**Where Apple's host ends the VM.** Each of these took down the whole
process that drove Apple's host session, as it would the VM's (the M3
Max, 2026-10-24):
- **`mcop` of a key the processor does not know** traps in AppleVideoEncoder
  (`CFCopyDescription` of NULL, `__HALT`): `Label` did, which the guest's
  session answers itself, so a guest sends such a key only for a property
  its client names that neither the session nor the processor has.
- **`mepf` after `minv`.** The processor emits the frame with -17691 and
  no buffer, then fails the frame, so the host leaves its processing group
  twice and `dispatch_group_leave` traps (read).
- **A second `+mep` on an open UUID.** It answers -19093 and leaves the
  table's lock held (read), so the next message that looks a session up
  aborts the process (`_os_unfair_lock_recursive_abort`).

**This host's motion estimation** follows that order, with a backend's
estimator where Apple's has AVE
(`crates/reims-vgpu-video-core/src/host/motion_estimation.rs`; the grid
and the vectors' format in `src/motion.rs`; [backends.md](backends.md),
"The traits" and "Who implements what"):
- **`meps`** lists one processor when the backend has an estimator, and
  none otherwise: {`CMClassImplementationName` "reims-vgpu-video Motion
  Estimation", `CMClassImplementationID`
  `reims-vgpu-video.motionestimationprocessor`, both names ours,
  `CMMatchingInfo` {`VTIsParavirtualizationAware` true,
  `VTIsHardwareAccelerated` true}}.
- **`+mep`** opens a session unless `,meo` names another processor: a
  `CMClassImplementationName` or `HostProcessorID` that is not ours, or a
  `VTIsHardwareAccelerated` that is not true. That, or a backend with no
  estimator, keeps a session that answers -19350, as Apple's answered a
  processor it does not have.
- **`mest`** takes Apple's sizes, held to the backend's largest, and
  answers anything else -12902 with nothing sent. It opens a new
  estimator, forgetting a frame the one before kept, then sends `Mspa`
  {`Width`, `Height`, `PixelFormatType` the backend's source formats,
  `BytesPerRowAlignment` 64, `PlaneAlignment` 64, `IOSurfaceProperties`
  {}} and Apple's `Mvpb`, each waiting 1.1 s for the guest's `,err`, which
  is `mest`'s answer when it is not 0.
- **`mepf`** reads as Apple's does, with no reply to a malformed one. Its
  frames are checked before the reply: before `mest` it is -12903, and a
  format `Mspa` did not list, or a size other than `mest`'s, -12902, with
  the frames let go and nothing more sent. What Apple's processor answers
  these is not measured. A frame that passes is answered 0, then goes in
  the measured order: the estimate, `Mbuf`, `rels` of the sources the
  session no longer needs, the vectors, `Memt` (`,rid`, `,err`, `,inf` 0,
  `,uui`), `Mdpb`, then the vector buffer's `rels`. `,mef` 1 keeps the
  current frame until the next frame is done. An estimate or `Mbuf` that
  fails is emitted with its status and no buffer. The UUID table is
  Apple's, weak entries that `Mdpb` names once their surface is gone.
- **The vectors** are Apple's grid and format whatever the backend's
  blocks: each cell takes the vector most of the backend's blocks under it
  have, from at most 4x4 of them sampled evenly, the first in a tie, and
  the cells past the picture are (0, 0) (`motion::resample`). The
  vectors' values are not the contract bit for bit, as an encode's are
  not. Their sign, unit and layout are. So the precision is the
  backend's: VideoToolbox's is Apple's processor's, whole pixels and
  multiples of 4 at 16, and NVENC's is a quarter of a pixel on 8x8
  blocks, so content moved by (2.5, -1.25) gives (-2.5, 1.25) exactly
  ([nvidia.md](nvidia.md), "Motion estimation"). Rounding NVENC's to
  Apple's steps would only lose what it found, so it is not done.
- **The properties** answer as Apple's processor's: `mcop` reads
  `IncludeMotionConfidence` false, `MotionVectorSize` 4 or 16 as the
  specification gave, `UseMultiPassSearch`, `TrueMotion`, `PropertyMode`
  and `VTIsHardwareAccelerated` false. `mset` takes `TrueMotion` and
  `PropertyMode`, -12902 with another type, `ClientPID` is 0, and anything
  else -2002. `mmul` sets each and answers the first refusal, and NULL is
  -12902. `UseMultiPassSearch` or `TrueMotion` asks the next `mest`'s
  estimator for the backend's smoother search, where it has one.
- **The rest:** `mcfr` 0, `msav` -12782, `minv` 0, after which the
  estimator and the kept frame are gone; `mesu` and an unknown type get no
  reply; `-mep` ends the session at once.

### The JPEG engine

The guest hands still-image JPEG decodes and encodes to the host's JPEG
hardware with three messages, `jcap`, `jdec` and `jenc`, which the host
session answers itself, outside any decoder or encoder session. Both
halves are thin: the guest's `VTParavirtualizedJPEGSession` forwards what
CMPhoto asks, and the host's handler passes it to CMPhoto's
`CMPhotoParavirtualizedHostJPEGHardware*` functions. So what the options
mean, what is in the two surfaces and which statuses come back are
CMPhoto's. Its facts below are read in CMPhoto in the 26.7.1 guest's
arm64e and x86_64 shared caches, and measured on the M3 Max (macOS 27.0.1,
26A434, 2026-10-24) by calling its CMPhoto host functions in a process,
with JPEGs cjpeg wrote from synthetic sources.

**Who uses it.** CMPhoto, and only on arm64.
- arm64e CMPhoto imports `VTParavirtualizedJPEGSessionCreate`,
  `…CopyCapabilities`, `…DecodeImage` and `…EncodeImage`. Its unified
  JPEG decoder's setup first opens its own JPEG hardware
  (`IOServiceOpen`). Only where that fails, as in a VM, does it make a
  session and read its capabilities. A session that makes, with a `,jca`
  that has `Version`, marks the engine paravirtualized, and from then on
  CMPhoto's hardware decode, `JPEGDecompressSurface` (exported),
  tail-calls the session's decode. Its hardware encode goes to the
  session the same way.
- **No x86_64 guest component uses it.** x86_64 VideoToolbox exports the
  four functions. But x86_64 CMPhoto imports none of them, and of the
  x86_64 cache's dylibs that import VideoToolbox, none imports a
  `VTParavirtualized…` symbol: each was extracted and its undefined
  symbols read with `nm -u` (2026-10-24). MediaToolbox imports only
  `VTParavirtualizationIsRunningInGuest`. So on an x86_64 guest the engine
  is reached only by a program that calls the session's exports directly.
- x86_64 CMPhoto's three host functions return -16994 and do nothing.

**`jcap`**, sent once by `VTParavirtualizedJPEGSessionCreate`, with flag
0x1, no surfaces and no parameters, waiting 1.1 s.
- The host: with surfaces, `,err` -12902. Otherwise
  `CMPhotoParavirtualizedHostJPEGHardwareCopyCapabilities`, then `,jca`
  (`=pls`) and `,err`.
- The guest's create fails on a nonzero `,err`, and keeps `,jca` for
  `CopyCapabilities`.
- CMPhoto's reader needs `Version`, else -16994. It reads
  `NumberOfCores` (1 when absent), `SupportsInterchangeFormats`,
  `SupportsMissingEOI`, `SupportsRSTLogging` and
  `Supports16BitQuantizationTables` (false when absent).
- CMPhoto's host side builds the dictionary once (a `dispatch_once`). The
  builder opens the JPEG hardware first: where that fails it makes no
  dictionary, and the status, which starts at -16994, stays so. So **an
  Apple host without JPEG hardware answers `,err` -16994 and no `,jca`**.
- Otherwise the dictionary is `Version` 1, `NumberOfCores` from the
  hardware, `Supports16BitQuantizationTables` true, and the other three
  from the hardware. The M3 Max's: {`NumberOfCores` 2,
  `Supports16BitQuantizationTables` 1, `SupportsInterchangeFormats` 1,
  `SupportsMissingEOI` 1, `SupportsRSTLogging` 1, `Version` 1}.

**`jdec`**, sent by `VTParavirtualizedJPEGSessionDecodeImage(session,
source, destination, options)`: flag 0x1, the two surfaces in that order,
`,joo` the options (`=pls`), waiting 1.1 s. The guest returns the host's
`,err`, or the transport's error.
- The host: `,joo` that does not read is its status in `,err`; not
  exactly two surfaces, or one that is not an IOSurface, is -12902. Then
  `CMPhotoParavirtualizedHostJPEGHardwareDecode(source, destination,
  options)`, and `,err` its status.
- **The source** holds the JPEG's bytes from offset 0. The host copies
  the whole surface, `IOSurfaceGetAllocSize` bytes, into a memory surface
  of its own. Its hardware reads `InputSize` bytes of it, or all of it
  when `InputSize` is 0 or more than that.
- **The destination** is a pixel surface the guest made. The image goes
  to its top-left corner; a larger one keeps the rest (measured: a
  700x500 `420f` took the 640x480 image and nothing past it).
- **The options** are CMPhoto's view of the JPEG's header
  (`JPEGParseJPEGInfo`, exported), as the guest builds them and the host
  reads them:

| key | type | what | absent |
|---|---|---|---|
| `InputSize` | s64 | the JPEG's length in the source | the whole source |
| `Width`, `Height` | s32 | the SOF's size | -16990 |
| `Subsampling` | s32 | 0 4:2:0, 1 4:2:2, 2 4:4:4, 3 4:1:1, 4 grey: one component, or three whose second and third sample 1x1, with the first's factors 2x2, 2x1, 1x1 or 4x1. Any other, 4:4:0 among them, is refused by CMPhoto's parser (-12910), so never sent | -16990 |
| `MCUsHor`, `MCUsVer` | s32 | the image's MCUs across and down: 16x16 for 4:2:0, 16x8 4:2:2, 8x8 4:4:4 and grey, 32x8 4:1:1 | -16990 |
| `Flags` | s32 | passed to the hardware; 1, 2, 4, 8, 16 and 256 changed nothing measured | -16990 |
| `DecodeWidth`, `DecodeHeight` | s64 | the size to decode to: the hardware scales by 1, 1/2, 1/4 or 1/8 | -16990 |
| `CropRect` | CGRect dictionary | a rectangle of MCUs to decode; its whole image is no crop | the whole image |

- **What Apple's host answered**, measured on the M3 Max with a 640x480
  stream unless named:

| case | status |
|---|---|
| a required key absent | -16990, nothing written |
| `InputSize` absent, or 1000 for a 93 KB stream | 0, the whole image |
| a destination smaller than the decode | 0xe00002c2 (kIOReturnBadArgument), nothing written |
| `DecodeWidth`, `…Height` 320x240, 160x120, 80x60 | 0: the image at 1/2, 1/4, 1/8, in the top-left |
| 500x375, 320x480 | 0: at 1/2, and at 1 |
| 40x30 (1/16) | kIOReturnBadArgument |
| `CropRect` {0, 0, 20, 15} | 0: the top-left 320x240 |
| `CropRect` {0, 0, 320, 240}, past the image's 40x30 MCUs | kIOReturnBadArgument |
| `Width` 320 | 0xe00002d1 (kIOReturnBadMedia) |
| `Subsampling` 1 or 2 for a 4:2:0 stream | 0xe00002e9 (kIOReturnDeviceError), part written |
| `Subsampling` 9, for a 4:4:0 stream | kIOReturnBadMedia |
| a progressive stream; one cut in half; one with bytes flipped | kIOReturnBadMedia |
| a stream without its EOI | kIOReturnBadMedia, the whole image written |
| restart markers every 3 MCU rows; 333x211 | 0 |
| a decode whose area is under 24 blocks, each component's counted at its sampling, none rounded up: under 1024 luma pixels of 4:2:0, 768 of 4:2:2, 512 of 4:4:4, 1536 of grey; whole or cropped, into any format | kIOReturnBadArgument |
| 24 blocks of area or more | 0 |

- The two rows on area were measured on the M3 Max (2026-10-24) with
  cjpeg's streams and crops of them. Crops at the edge of a 333x211 4:2:0
  and a 65x33 4:2:2 image tell the area from a count of blocks or MCUs:

  | sampling | refused | decoded |
  |---|---|---|
  | 4:2:0 | 16x16, 17x9, 48x16, 61x16, 64x3, 80x3, 13x67, 29x32, 29x35, 45x19 | 64x16, 13x80, 13x83, 61x19, 4095x7 |
  | 4:2:2 | 80x8, 16x40, 33x16, 17x33 | 96x8, 48x16, 16x48, 33x24 |
  | 4:4:4 | 15x15, 56x8, 8x56 | 64x8, 8x64 |
  | grey | 1x1, 184x8 | 192x8, 7x4095 |

  The closest pair was 4:2:0's: 29x35, 1015 pixels, refused, and 64x16,
  1024, decoded. 61x16 of 4:2:0 is 4 MCUs, as 64x16 is, and was refused;
  13x80 is 5 MCUs of 13 columns and decoded. 61x16 into `yuvs`, and 184x8
  of grey into `444f` and `420f`, answered as into the stream's own
  layout.
- **Damaged scan data**, measured on the M3 Max (2026-10-24) on 64x64
  images written from scratch, grey and 4:2:0, some with restart
  intervals (the walk's tests,
  `crates/reims-vgpu-video-codec/src/jpeg/entropy/tests.rs`, which hold
  each answer):

  | damage | status |
  |---|---|
  | a Huffman code no table has; an AC coefficient past the 63rd | kIOReturnBadMedia |
  | a restart marker missing, out of turn, extra, inside an interval, or with no DRI | kIOReturnBadMedia |
  | any data before a restart marker past the interval's last byte, one zero byte among it | kIOReturnBadMedia |
  | no EOI, whether or not the scan is whole | kIOReturnBadMedia |
  | the scan cut short and closed with its EOI, at an MCU or at a restart marker | 0xe00002e9 (kIOReturnDeviceError) |
  | a DC coefficient past 2047 | 0 |
  | data after the EOI; zeros as padding; fill bytes before the EOI; a restart marker after the last MCU with nothing after it | 0 |
  | data between the last MCU and the EOI | by how it decodes, below |

  What the hardware takes after the last MCU follows how the data
  decodes, as though the next MCU began:
  - what is left of the last MCU's byte, whatever it holds;
  - up to 32 whole bytes more, where the data runs out within two more
    blocks decoded from them, either between two codes or inside a
    Huffman code with one bit left, or fewer than 8 and all ones.

  It refuses the data where either block holds a bad code or runs past
  its 63rd coefficient, where both blocks end whole, and where the data
  stops elsewhere inside a code or a coefficient's own bits. A restart
  marker with nothing after it may follow. That rule is ours, fitted to
  these runs, and agrees with Apple's answers to:
  - all 46 designed cases;
  - each value of one byte after a 4:2:0 image's last MCU: taken below
    0x80;
  - runs of 0 to 64 zero bytes there, 4:2:0 and grey: 129 of 130. Apple's
    took no run of 33 bytes or more but one of 35, in the grey image;
  - random runs of 1 to 40 bytes: 60 the rule was fitted to and 80 drawn
    after it was written, 140 of 140; and 120 drawn from a fixed seed in
    the tests (`random_trailers`), measured after the rule was written,
    120 of 120.
- **The formats Apple's host writes**, by stream: `420f`, `420v`, `422f`,
  `422v`, `2vuy`, `yuvs` and `BGRA` from 4:2:0, 4:2:2, 4:4:4 and grey;
  `444f` and `444v` from 4:4:4 and grey only. It refused `L008`, `y420`,
  `f420` and `xf20` from every stream, and `444f` from 4:2:0 and 4:2:2,
  with 0xe00002bc (kIOReturnError). The samples are the JPEG's own, full
  range, whatever the format's range: `420v` held the same bytes as
  `420f`. Grey wrote chroma 0x80. Its `BGRA` from a 4:2:0 stream looks
  like JFIF's full-range equations on the nearest chroma: against ImageIO's
  decode it scored 31.56 dB, the same as its own `420f` converted that way
  (2026-10-24). That is a likeness, not a bit-for-bit comparison.

**`jenc`**, sent by `VTParavirtualizedJPEGSessionEncodeImage(session,
source, destination, options, &size)`: flag 0x1, the source pixel surface
and the destination surface, `,joo`, waiting 10 s. The host's encode
writes the JPEG into the destination and replies `,jes` (`=u32`) its size
with `,err`; the guest reads `,jes` only on noErr. The options are
CMPhoto's; this host has no JPEG encoder, and they are not read further
here.

**An old CMPhoto.** A host whose CMPhoto lacks the function for a message
returns noErr with no reply, so the guest waits out its timeout. That host
has no JPEG engine for its guests.

**Open for arm64 guests.** The capabilities say nothing of encode, so a
guest that sees `jcap` succeed sends its encodes to `jenc` as well.
Whether CMPhoto then falls back to software when `jenc` fails is not
measured in an arm64 guest.

## The M2 scaler device

One queue. Each element's readable part is copied into one buffer `R` of
`n` bytes. The reply is all of `R`, rewritten in place, written to the
writable part, with a used length of `n`.

- `n == 0`, a writable part shorter than `n`, or `n < 8` is a needs-reset,
  with no reply.
- `n` over 64 KiB is a needs-reset with no reply, before the readable part
  is copied: ours ("Where we differ from Apple").
- `R+0` is the type, and `R+4` takes the status: the IOReturn of
  `IOSurfaceAcceleratorParavirtProcessGuestData`.
- **Any other type** is logged, and `R` is replied unchanged.
- What the guest reads of a reply is `R+0`, `R+4` and, for a transform,
  `R[0x608]`. A nonzero status is logged, `Transform transaction failed =
  %#x`, for either type.
- **One at a time, in order.** Apple's handler pops an element, answers
  it, and returns it before it pops the next. Where it runs is a
  hypothesis: each virtio device has a work dispatch queue and a dispatch
  source a queue, so the handler likely runs there, off the vCPUs; the
  call was not traced. Its reset and stop drop the accelerator. Ours
  answers on a worker thread of QEMU's device, one element at a time,
  each returned before the next is popped; a reset waits for the element
  with the worker and discards its answer
  (`vendor/qemu/hw/display/apple-m2scaler-pci.c`; [qemu.md](../qemu.md),
  "The M2 scaler device").

core has all of it in `crates/reims-vgpu-video-core/src/scaler.rs`, and
the layouts below in `scaler/request.rs` and `scaler/capabilities.rs`.

### Type 1, capabilities

The guest kext sends one, from start, and waits for its reply before it
registers its service. `n` must be 0x20.

| `R+` | size | what |
|---|---|---|
| 0x00 | u32 | 1 |
| 0x04 | u32 | the status |
| 0x08 | u64 | the guest-physical address of the guest's capabilities, the 0x288 bytes the kext keeps them in |
| 0x10 | u64 | the guest writes 0. Apple's host writes its own pointer to that memory here, and the reply echoes it |
| 0x18 | u64 | the length: the guest asks 0x288. It is the kernel call's output size, in and out: the host's scaler writes at most that much and puts the count it wrote here |

- An address and length the host cannot translate is a needs-reset.
- Apple's host fills the memory with its own kernel scaler's answer to
  user-client selector 11. On the M3 Max (macOS 27.0.1), selector 11
  writes min(asked, 0x298) bytes and puts the count back: 0x288 for the
  guest's 0x288 ([scaler.md](scaler.md), "Apple's scaler, measured"). A
  hypothesis for 26.7's hosts until one is measured.
- On the reply the guest zeroes eight fields, 0x181, 0x182, 0x183, 0x188,
  0x190, 0x194, 0x198 and 0x1a4, so a guest never has directional
  scaling, enhancement, ASE or its ratios, whatever the host says. Then it
  publishes the rest as IORegistry keys on its service
  (`writeScalerCapabilities`). The guest's IOSurfaceAccelerator reads
  those keys.

**The capabilities**, 0x288 bytes. The layout is the guest kext's. Each
field's meaning is the key `writeScalerCapabilities` publishes it under,
or the check that reads it. Apple's value is the M3 Max's (macOS 27.0.1),
from selector 11 (measured 2026-10-24). Ours is `Capabilities::to_bytes`
with what the backend says.

| offset | type | key, or what reads it | the M3 Max's |
|---|---|---|---|
| 0x000 | i32 | not published. `mapCommParams` refuses side-buffer kinds 3 and up when it is below 8 | 0x11 |
| 0x004 | u32 | `ScalerVersion` | 0x030d0041 |
| 0x008 to 0x023 | u8 each | the format classes, each published as both `FormatIn…` and `FormatOut…`, nonzero for yes: 0x08 `2Planes420`, 0x09 `2Planes422`, 0x0a `2Planes444`, 0x0b `RGB565`, 0x0c `ARGB1555`, 0x0d `ARGB8888`, 0x0e `RGBA8888`, 0x0f `PMARGB8888`, 0x10 `Interleaved422`, 0x11 `ARGB2101010`, 0x12 `ARGB8101010`, 0x14 `ARGBV16`, 0x15 `PARGBV16`, 0x16 `1PlaneYCBCR10444`, 0x17 `1PlaneYonly8` and `LumaOnlyL008`, 0x18 `…Yonly10` and `L010`, 0x19 `1PlanePackedYonly10` (In only), 0x1a `…Yonly12` and `L012`, 0x1b `1PlanePackedYonly12` (In only), 0x1c `LumaOnlyL016`, 0x1d `1PlaneYCBCR10422`, 0x1e `2Planes42010`, 0x1f `2Planes42210`, 0x20 `2Planes44410`, 0x21 to 0x23 `2PlanesPacked420/422/44410` | 1 each, 0x13 unused |
| 0x028 | u32 | `MinSourceWidth` and `…Height` | 4 |
| 0x02c | u32 | `MinAseSourceWidth` and `…Height` | 32 |
| 0x030, 0x034 | u32 | `MaxSourceWidth`, `…Height` | 65536 each |
| 0x038, 0x03c | u32 | `MaxAseSourceWidth`, `…Height` | 32768 each |
| 0x040 | u32 | `MinDestWidth` and `…Height` | 4 |
| 0x044, 0x048 | u32 | `MaxDestWidth`, `…Height` | 65536 each |
| 0x10c, 0x10d | u8 | `SourceXYOffsetsWithTiling`, `DestXYOffsetsWithTiling` | 1, 1 |
| 0x10e | u8 | `LimitationXYOffsetsWithWidthHeight` | 0 |
| 0x11c, 0x120 | u32 | `FilterCoefficientsPrePointBits`, `…PostPointBits` | 4, 12 |
| 0x124 | u8 | `kSurfaceAcceleratorCapabilitiesVERF` | 1 |
| 0x128 to 0x134 | u32 | `MaxHUpscale`, `MaxHDownscale`, `MaxVUpscale`, `MaxVDownscale` | 8, 8, 8, 4 |
| 0x13c, 0x140 | u32 | `CurvatureDegammaLUTSize`, `…RegammaLUTSize`, published only when 0x245 is nonzero, else 0 | 0x501, 0x2c2 |
| 0x150 to 0x15c | u32 | `Histogram`, `HistogramFixedBins`, `…ProgrammableBins`, `…Values` | 1, 64, 8, 128 |
| 0x16e, 0x171 | u8 | `ColorManager`, `MediaCompInput` | 1, 0 |
| 0x174, 0x175 | u8 | `12bitYCbCr`, `16bitYCbCr` | 1, 1 |
| 0x181, 0x182 | u8 | `DirectionalScaling`, `Enhancement`; zeroed by the guest | 1, 1 |
| 0x183 | u8 | not published; zeroed by the guest. Side-buffer kinds 1 and 4 (ASE) need it | 1 |
| 0x184 | u8 | `HDR` and `HDREnable`. Kinds 2 and 3 need it, and the guest sends side buffers only with it and the transform's `HDREnable` flag | 1 |
| 0x188 | f64 | not published; zeroed by the guest | 2.0 |
| 0x190 to 0x198 | u32 | `ASEMinRatio`, `ASEPolyMaxHRatio`, `…VRatio`; zeroed by the guest | 1, 4, 2 |
| 0x1a4 | u32 | `ASENoiseEnhancePolyMinHRatio`; zeroed by the guest | 4 |
| 0x1aa, 0x1b0 | u8 | `MediaCompOutput`; `GpuCompInput` and `…Output` | 0, 0 |
| 0x1b2, 0x1b3 | u8 | `CommApi`, `VraScaling` | 1, 1 |
| 0x1b4, 0x1b8 | u32 | `HDRApiVersion`, `ASEApiVersion` | 2, 2 |
| 0x1f1, 0x1f2 | u8 | `InterchangeCompression`, `…LossyCompression` | 1, 1 |
| 0x1f4 | u8 | not published. Kinds 3 and 4 (the HCU-style parameters) need it | 1 |
| 0x1f5 | u8 | `HCUApiVersion`; kinds 3 and 4 take their newer units only at 2 | 2 |
| 0x1f6 | u8 | not published. With it, kinds 1 and 2 (the legacy parameters) are refused | 1 |
| 0x1f7 | u8 | not published; read by kind 4 | 0 |
| 0x1f8 to 0x1fc | u8 | `InterchangeStrideAlignment`, `…BaseAlignment`, `…MacroblockSizeNormal`, `…Large`, `…SubblockSize` | 0x80, 0x80, 0x10, 0x20, 4 |
| 0x1fd, 0x1ff | u8 | `YCbCrAlpha`, `MMR` | 1, 1 |
| 0x207, 0x208 | u8 | `UnequallySpacedLut`, `FilmGrain` | 1, 1 |
| 0x23c, 0x23d | u8 | `OOTF`, `FractionalDimensions` | 0, 1 |
| 0x245 | u8 | publishes the curvature LUT sizes | 0 |
| 0x25c, 0x25d | u8 | `AvdHistogram`, `FP16Regamma` | 1, 0 |
| 0x26c | u32 | `QuadraBayerRepacking` | 0 |
| 0x27c | u32 | `SymmetricScaling` | 0 |

Every other byte is unnamed in the guest's structure and read by nothing
in the kext. Keys are published with the `IOSurfaceAccelerator` prefix,
the `Capabilities` or `Format` word between, and the `kSurface…` and
`HDREnable`/`VraScaling` keys as named.

### Type 2, transform

`n` must be 0x610.

| `R+` | size | what |
|---|---|---|
| 0x000 | u32 | 2 |
| 0x004 | u32 | the status |
| 0x008 | 0x1b0 | the transform: IOSurfaceAccelerator's request for its kernel driver, below |
| 0x1b8 | u64 | the source's mapping ID |
| 0x1c0 | u64 | the destination's mapping ID |
| 0x1c8 | 8 × 0x88 | the side buffers' lists: eight `{u64 guest address, u32 length}` entries each, then a u32 count at +0x80 |
| 0x608 | u8 | 1: the side buffers are used |

**The transform, `R+8`.** The guest's IOSurfaceAccelerator builds it,
and the kext passes it through. It clears flag bit 6, and fills the side
buffers' lists (`mapCommParams`) and the mapping IDs
(`submitTransformTransaction`). The layout is IOSurfaceAccelerator's
request, built by `IOSurfaceAcceleratorTransformSurface` and its siblings
through `prepareTransformBuffersAndOptions`. These are read in the host's
IOSurfaceAccelerator: the guest's x86_64 IOSurfaceAccelerator builds the
same request by hypothesis, until a guest run shows it.

| `R+8+` | size | what |
|---|---|---|
| 0x000 | u32 | the source's IOSurface ID in the guest's client. Apple's host overwrites it with its own surface's |
| 0x004 | u32 | the destination's, likewise |
| 0x008 | 3 × u64 | the completion: a callback, nonzero for an asynchronous transform, and two words. The kext puts its thread's ID in the second and zeroes the third |
| 0x020 | u64 | the flags, below |
| 0x028 | 4 × u64 | the source window, x, y, width, height, in 16.16 (the builder puts a 2-bit fraction in at bit 14) |
| 0x048, 0x04c | u32 | the source surface's width and height |
| 0x050, 0x058 | u64 | zeroed by the builder; never set |
| 0x060 | 4 × u32 | the destination window, x, y, width, height |
| 0x070, 0x074 | u32 | the destination surface's width and height |
| 0x078 | 0x18 | the source's shared event: a Mach port, the value to wait for and the value to signal |
| 0x090 | 0x18 | the destination's |
| 0x0a8 | u32 | `AlphaOverride`'s value |
| 0x0ac | 0x20 | `BorderFill`: x, y, width, height, alpha, red or Y, green or Cb, blue or Cr. `BorderFillOddSizeYCbCr` reuses its alpha and Y |
| 0x0cc | 4 × u16 | `WriteOnlyMode`: alpha, red or luma, green or Cb, blue or Cr |
| 0x0d4, 0x0d8 | u32 | never set by the builder; zero |
| 0x0dc | 0x34 | the histogram: bin mode, x, y, width, height, eight programmable bins |
| 0x110 | 4 × 0x28 | the side buffers, pair `i` at `R+0x118+0x28*i`: an input `{i32 kind, u64 address, u32 size}` and an output `{i32 kind, u64 address, u32 size}` |

A zero width or height in a window means the whole surface: the
`TransferSurface` path leaves both windows zero, and on the M3 Max such a
transfer fills the destination ([scaler.md](scaler.md)).

**The flags**, each set by the IOSurfaceAccelerator option named:

| bits | option |
|---|---|
| 0 to 5 | `Transform`, the symmetric transform; at most one bit above bit 1 |
| 6 | `UseCustomFilter`; the kext clears it |
| 7 | `UseNearestFilter` |
| 8 | `OptOutBlitClip` |
| 9 | `LockInScaler` |
| 10 | `SkipAsyncCallback`; the kext's asynchronous wrapper reads it |
| 12 | `FixUpscaling`; also set by every `TransformSurface`, and not by `TransferSurface`. Apple's scaler keeps pixel centres along an axis that enlarges with it, and puts the windows' first pixels on each other along any other ([scaler.md](scaler.md), "Where a scale samples") |
| 13, 14 | `DitherControl`, under 4; 1 when the option is absent, and 1 with no options at all |
| 15 to 17 | the destination's chroma siting, 1 to 6, from its attachments; 0 for none. Apple's scaler writes the destination's chroma so, and at its first pixels for 0; it reads every source's on its first pixels ([scaler.md](scaler.md), "Chroma siting") |
| 18 | `AlphaOverride` |
| 28 | `BorderFill` |
| 29 | `HistogramBinMode` |
| 30 | `ZeroFill`, nonzero |
| 31 | `DisablePseudoLinearScaling` |
| 32 | `PixelAveraging` |
| 33 | `EdgeMirroring` |
| 35 | `UnwireSurface` (its value, shifted) |
| 36 to 38 | `ColorRemapMode`, under 5 |
| 39 | `DirectionalScalingEnable` |
| 40 | `HDREnable` |
| 41 | `EnhanceEnable` |
| 42 | `NoiseEstimationEnable` |
| 43 | `ReadOnly` |
| 44 | `HtpcLargeHeaders` |
| 45 | `VraScaling` |
| 46 | `NoPowerEarlyBail` |
| 47 | `FirmwareTimeoutInject` |
| 48 | `ItuVariantCSC` |
| 49 | `WriteOnlyMode`, all four values given |
| 50 | `FilmGrainEnable` |
| 51 | `ReducedPrecisionFractionalOffsets` |
| 52 | `NoCoalesceAsyncIOFenceWrites` |
| 53 | `DestinationDisplayBound` |
| 54 | `SymmetricScaling` |
| 55 | `DefaultChromaDownsample` |
| 57 | `BorderFillOddSizeYCbCrEnable` |

Bits 11, 19 to 27, 34, 56 and 58 to 63 are set by nothing in the builder.

**The `Transform` bits**, measured on the M3 Max's scaler
([scaler.md](scaler.md)): bit 2 turns the source window 90 degrees
clockwise, then bit 0 flips the result vertically and bit 1 horizontally.
So 1 is a vertical flip, 2 a horizontal one, 3 a half turn, 4 a quarter
turn clockwise, 5 the anti-transpose, 6 the transpose and 7 a quarter turn
anticlockwise. The source window is in the source's coordinates, before
the turn; the destination window is in the destination's.

**The side buffers.** With `R[0x608] == 1`, four optional pairs come:

- Each input is checked, `size <= 0xbd10` and `kind <= 6` (signed), then
  gathered from scatter-gather list `2i` when both are nonzero. Each output
  is checked, `size <= 0x1008` and `kind <= 3`, and given a buffer of its
  size. A failed check is a needs-reset.
- A gather or scatter is a needs-reset when the entries pass the buffer's
  size, or an entry's memory cannot be translated, a zero-length entry
  included. A count over 8 aborts Apple's VM process.
- After the transform, each output with a kind and a size is scattered to
  list `2i+1`, whatever the status.
- Apple's host writes its own buffers' addresses over the pairs'
  addresses, and the reply carries them.

What the kinds are, from IOSurfaceAccelerator's builder and the kext's
checks:

| kind | in | out |
|---|---|---|
| 1 | slot 0, 0x238 bytes: the legacy ASE parameters | 0x17c bytes |
| 2 | slot 1, 0x9008 bytes: the legacy HDR parameters | 0x1008 bytes |
| 3 | slot 2, its own size: HDR in the HCU style, a list of units (`base` 0x9008, `aser` 8, `aset` 4, `basf` 0x1404, and with `HCUApiVersion` 2 `frar` 0xc0 (MMR), `fras` 0x17d8 (the unequally spaced LUT), `frau` 0x2c (OOTF)) | its own size |
| 4 | slot 3, its own size: ASE in the HCU style (`fram` 0x238, `fran` 4, `frao` 0x14, `frap` 0x120, `fraq` 0x3c) | |
| 5, 6 | refused by the kext, `Comm API consistency error` | |

- The kext bounds each slot's input, slot by slot: 0x238 to 0x3dc, 0x9008,
  8 to 0x762f0, 0x3dc to 0x762f0. A unit list holds at most 10 units and
  0xbd10 bytes.
- A guest sends side buffers only when the capabilities' 0x184 is nonzero
  and the transform has `HDREnable`. It zeroes 0x183, so kinds 1 and 4
  never pass its checks.
- The units' contents are the kernel scaler's business, and not decoded
  here: the host kernel's AppleM2ScalerCSCDriver was not read. A host
  that lists no HDR never receives them.

**What Apple's host does with it.** It retains the two surfaces by mapping
ID from its IOSurface mapper and writes their host IDs into `R+8` and
`R+0xc`. Then `IOSurfaceAcceleratorParavirtProcessGuestData` zeroes the
completion's three words when the callback is nonzero, zeroes both shared
events, hands the 0x1b0 bytes to the kernel scaler (selector 1), and puts
its IOReturn at `R+4`. Those writes are in the reply. The guest's
completion runs from the reply, through the kext.

**Past the offered ratios** the kernel scaler answers kIOReturnBadArgument
(0xe00002c2), the ratio taken after the orientation: on the M3 Max's
AppleM2ScalerCSCDriver (macOS 27.0.1, 26A434, 2026-10-24), 8 up across
transformed and 16 did not, 8 down the frame did not against its
`MaxVDownscale` 4, and a quarter turn was held to the ratios of the turned
picture ([scaler.md](scaler.md), "Apple's scaler, measured"). A
hypothesis for macOS 26.7's hosts until measured there. Ours refuses the
same way against the ratios it offers, before the backend, with the
destination untouched.

**The conversions it refuses.** Every transform from or into 2vuy or
32ARGB is kIOReturnUnsupported (0xe00002c7): 28 of the 64 pairs of the
eight formats. Measured on the M3 Max's AppleM2ScalerCSCDriver (macOS
27.0.1, 2026-10-25) with 576 calls: each pair through TransformSurface,
TransferSurface and TransferSurface with `FixUpscaling`, as a copy at
64x48, a scale up from 64x48 to 96x72 and one down from 128x96 to 64x48.
The 28 pairs' 252 calls were refused; the 36 pairs among 420v, 420f,
x420, xf20, yuvs and BGRA succeeded, and no call answered another status
([scaler.md](scaler.md), "Apple's scaler, measured"). The capabilities
still list both formats' classes, `Interleaved422` and `ARGB8888`, as
inputs and outputs. A hypothesis for macOS 26.7's hosts until measured
there. Ours refuses the same 28 pairs, in the plan before the backend,
with the destination untouched, and logs the pair (`scaler/format.rs`,
`apple_refuses`; `scaler/plan.rs`).

**What stays unexplained.** `R+8+0x50`, `+0x58`, `+0xd4` and `+0xd8`,
which the builder zeroes or leaves zero; flag bits 11, 19 to 27, 34, 56
and 58 to 63; what the kernel scaler does with the hints (dither, the
filter's tuning); and the side buffers' units, past their names and
sizes. None changes what this host does: it refuses the options that
would change the output and it does not do ([scaler.md](scaler.md)), and
lists no HDR, so no unit reaches it.

### Which guests use it

The guest's IOSurfaceAccelerator opens `AppleM2ScalerParavirtDriver` only
where `kern.hv_vmm_present` reads 1 and `debug.iosa.simulated_model` is
unset; otherwise it looks for the hardware's `AppleM2ScalerCSCDriver`. The
x86_64 slice in 25G241's shared cache holds the same names and tests the
same flag before each lookup. Where the VM is hidden it also sends
selector 10, set client property, at create and from
`IOSurfaceAcceleratorSetProperty`, which a VM's framework answers itself.

With Apple's kext, a guest whose OpenCore hides the VM, as reims-util's
does by default, binds the kext and has its capabilities served, and then
never sends a transform: `IOSurfaceAcceleratorCreate` returns
kIOReturnNoDevice (measured in an x86_64 26.7.1 guest on this host). The
x86_64 port's kext therefore names its service `AppleM2ScalerCSCDriver`, a
subclass of ours of `AppleM2ScalerParavirtDriver`, and answers selector 10
with success ([scaler.md](scaler.md), "A guest that hides its VM"). The
guest then uses the scaler whether it sees its VM or not (measured the
same way). What the host sees is the same either way: the type 1 request
at start, and one type 2 request a transform.

Two other x86_64 images name the class. QuartzCore looks up
`AppleM2ScalerCSCDriver` and then `AppleM2ScalerParavirtDriver` with no
VM test, so it finds the port's service under either name. HDRProcessing
reads the capabilities of whichever service its own VM test names: where
the VM is hidden that is `AppleM2ScalerCSCDriver`, which the port's
service is too, so it reads ours, no HDR, as it does where the VM shows
([scaler.md](scaler.md)). In a guest that sees its VM, VideoToolbox's
`VTPixelTransferSession` and `VTPixelRotationSession` on x86_64 still sent
the device nothing (measured in an x86_64 26.7.1 guest on this host).

## Where we differ from Apple

A difference is made on purpose, listed here with its reason, and never
because matching was hard.

**Both devices.**

- **A surface read-only for its clients is never written.** Apple's host
  hands such a destination to its hardware, which on the M3 Max wrote into
  read-only pages, and the kernel panicked ("Surfaces read-only for their
  clients", above). Ours refuses the write and leaves the pages as they
  were:
  - a transform into it: kIOReturnNotWritable;
  - a decoder's frame into it: -12911, with the buffer given back;
  - `jdec` into it: -16990.

  A read-only source is read, as Apple's is. Host safety. No x86_64 guest
  can send such a surface (above), so this is for arm64 guests.

**The VideoToolbox device.**

- **What a backend decodes right is shown, where Apple's hardware
  refuses it.** Capabilities are not part of the contract: the host
  follows Apple's protocol, and shows what its backend decodes right, as
  Apple's host shows what its hardware does. So an AV1 frame coded
  `showable_frame` 0 and shown again, which AVD refused with -12909 in one
  of the two measurements ("AV1", above), is shown, as libdav1d shows it.
  The same rule gives the next three.
- **A decode session's smallest size is the backend's.** Apple's AVD
  refuses every codec under 64 a side at `dsta`, with -12911 and no
  `Dpba`, H.264 and HEVC by the SPS's coded size ("The decoder session",
  "AV1", "VP9", above). Core asks the backend, and each refuses a size it
  does not decode right in the same way, with -12911 and no `Dpba`, so
  that the guest decodes the stream itself, as it does a stream AVD
  refuses (`crates/reims-vgpu-video-vaapi/src/va/backend.rs`, `refusal`;
  `crates/reims-vgpu-video-nvidia/src/nvdec/decoder.rs`, `TooSmall`):
  - VA-API under the driver's smallest surface for the profile
    (`VASurfaceAttribMinWidth` and `MinHeight`), which Mesa's
    `vaCreateContext` would refuse: on radeonsi (an RX 7900 XTX and a
    Radeon 780M, Mesa 26.2.4), 64x64 for H.264 and HEVC, AVD's own, and
    16x16 for VP9 and AV1. So VP9 and AV1 from 16 to 63 a side decode
    there, where AVD refuses them;
  - VA-API, 10-bit VP9 and AV1 32 or fewer samples wide: radeonsi decodes
    their inter frames wrongly on both GPUs, as ffmpeg's VA-API decode
    does, and 34 wide and more right ([bugs.md](bugs.md), U15);
  - NVIDIA under NVDEC's smallest: on an RTX 5080 Laptop GPU (driver
    615.71.09), 48x64 for H.264, which decodes there and not on AVD,
    144x144 for HEVC and 128x128 for VP9 and AV1, which are larger than
    AVD's;
  - VideoToolbox: AVD itself, as on Apple's host.

  Each size from 2x2 up was checked through core's session on each
  backend: decoded with every frame equal to ffmpeg's software decode, or
  refused at `dsta` with -12911 ([vaapi.md](vaapi.md), "The smallest
  sizes"). Core keeps the refusals that no backend here gets past: 3-byte
  NAL unit lengths, H.264 SPSes that allow fields, and VP9 at 12 bits,
  each with Apple's status (`host/decoder.rs`, `session_refusal`). An
  x86_64 26.7.1 guest on this host decoded VP9 and AV1 of 16x16 to 48x64
  on VA-API, every frame matching, and fell back to its software decoder
  for H.264 the backend refused, as it does behind Apple's host.
- **A VP9 decoder does not list `MaxDecoderPixelRate`.** Apple's VP9
  decoder lists it and reads it as its hardware's decode rate, 7365427200
  on the M3 Max for every stream ("VP9", above). No backend here reports
  a rate of its own, so ours lists the decoder's other two properties
  alone, and answers `dcop` of it -12900, as Apple's decoders answer a
  property they lack (`supported_properties`,
  `crates/reims-vgpu-video-core/src/host/decoder.rs`).
- **A VP9 frame of another size is shown, at its own size.** Apple's VP9
  decoder emits a frame whose coded size is not the description's with
  -12909 and no image, inter and key frames alike, so a stream that
  libvpx resizes loses every frame after the resize ("VP9", above). Ours
  sends a `Dpba` at the frame's size before its `Dbuf`, as Apple's host
  does for AV1, and shows it there; the guest's `Dpba` handler is the
  same for every codec (read). Where a backend cannot decode such a
  frame, it refuses it: NVIDIA a frame larger than the key frame
  ([bugs.md](bugs.md), D16), with -12910 and no image, and each frame
  after it with -12909 until a key frame decodes, as Apple's decoder
  answers the frames after one it refuses; VideoToolbox's frames get
  AVD's own -12909, as on Apple's host. An x86_64 26.7.1 guest
  on this host showed all 240 frames of p0-720p-resize, its 180 of 960x540
  among them, each matching. The lends follow the frames decoded: such a
  frame's buffer is held while a slot holds it, where Apple's decoder lets
  a refused frame's buffer go at the next frame, and the slots' buffers
  with it; a frame NVIDIA refuses is lent its buffer as AVD lends one it
  refuses ("VP9", above).
- **The JPEG engine is the backend's.** Where the backend has no JPEG
  decoder, `jcap`, `jdec` and `jenc` answer `,err` -16994 at once, as
  Apple's host without JPEG hardware answers `jcap` ("The JPEG engine",
  above), not with the silence of a host whose CMPhoto lacks the
  functions, which costs the guest a 1.1 s timeout. No backend encodes
  JPEG, so `jenc` answers so on every host. `jcap`'s capabilities are the
  backend's, not Apple's hardware's, and the same on every backend that
  has a JPEG decoder: one core and no 16-bit tables, missing EOI, restart
  logging or interchange formats
  (`crates/reims-vgpu-video-core/src/host/jpeg.rs`,
  `crates/reims-vgpu-video-vaapi/src/va/jpeg.rs`,
  `crates/reims-vgpu-video-nvidia/src/jpeg.rs`). What is offered is what
  the driver does, as with the codec lists. On NVIDIA the engine is
  nvJPEG's hardware decoder, where the host has libnvjpeg, a CUDA
  toolkit library that the driver does not install
  ([nvidia.md](nvidia.md), "The JPEG engine"); without it, or on a GPU
  whose hardware decoder does not open, the guest is told there is no
  JPEG engine, as above.

  Scales of 1/2, 1/4 and 1/8, and every layout Apple's hardware writes
  from each sampling, go through the driver's video processing
  ([vaapi.md](vaapi.md), "Scales and conversions"). Its pixels are not
  Apple's: Apple's `BGRA` from 4:2:0 looks to take the nearest chroma
  sample ("The JPEG engine", above), where ours interpolates it from 4:2:0
  and 4:4:4, and takes the nearest from 4:2:2 as radeonsi does. Where
  Apple's hardware decodes and VA-API's does not, VA-API refuses
  ([vaapi.md](vaapi.md), "The JPEG decoder", "What it refuses"):
  - a scale, with kIOReturnUnsupported, and a layout of another sampling
    or `BGRA`, with kIOReturnError, on a driver without video processing
    or whose trial run of that conversion failed. Neither happened on
    radeonsi;
  - 4:1:1, which radeonsi does not decode, RGB components, and 16-bit
    tables with values over 255, with kIOReturnBadMedia. Apple's status
    for these is not measured.

  On NVIDIA, scales of 1/2, 1/4 and 1/8 are nvJPEG's hardware decoder's
  own. The CPU makes of its planes, by core's reference's filters, what
  they are not as the reference has it: an odd size at a scale, the
  layouts of another sampling than the stream's, and `BGRA` at a scale,
  which takes the nearest chroma sample, as Apple's looks to. At scale 1
  its `BGRA` interpolates the chroma, from every sampling. It refuses 4:1:1,
  which nvJPEG's hardware decoder does not decode, RGB components and
  values over 255 in 16-bit tables, which that decoder reads as their low
  8 bits, with kIOReturnBadMedia, as VA-API does ([nvidia.md](nvidia.md),
  "The JPEG engine").

  A stream without its EOI is kIOReturnBadMedia on both, but Apple's
  writes the image first and ours writes nothing. A decode under 24
  blocks of area, which Apple's hardware refuses with
  kIOReturnBadArgument ("The JPEG engine", above), goes to the backend,
  by the rule above: VA-API and NVIDIA decode down to 1x1, as
  `crates/reims-vgpu-video-vaapi/tools/jpeg-check.sh` checks against
  libjpeg, and an x86_64 26.7.1 guest's decodes on this host of 17x9,
  1x1, 15x15 and four small crops came back with status 0 and ImageIO's
  luma.

  Damaged entropy-coded data, which VCN decodes to wrong pixels with
  status 0, is refused before the driver sees it, on every backend, by a walk of the scan
  on the CPU that answers as Apple's hardware does ("The JPEG engine",
  above; [vaapi.md](vaapi.md), "What it refuses"). Its rule for data after
  the last MCU is ours, fitted to Apple's answers, and differs from them
  on one measured case: Apple's took 35 zero bytes after a grey image's
  last MCU, which ours refuses as past its 32. What Apple's writes into
  the destination before such a refusal is not measured; ours writes
  nothing.

- **Closing a decoder ends its waits for the guest at once.** Apple's
  `-dec`, and the device's reset, invalidate a decoder session by waiting
  for the message its queue has in hand, and do not invalidate its reply
  clerk, which goes only with the session (read). A handler in that
  message waiting for the guest, on a `Dbuf` or a `Demt`, cannot get its
  reply: the reply comes through the device's queue-0 handler, which is
  the thread doing the waiting. So the wait runs out, 1.1 s for a `Dbuf`
  and 10 s for a `Demt`, with the device stalled for it. Ours cancels the
  session's waits first, so the handler ends at once: what the guest sees
  is the same, sooner. The cancelled handler sends nothing more, as
  Apple's sends nothing once invalidated
  (`crates/reims-vgpu-video-core/src/host/decoder.rs`, `invalidate`).

- **Closing an encoder ends its waits too, and lets its frames go.**
  Apple's encoder session invalidates the same way, by waiting for its
  queue. Its handlers wait for the guest on `Epba` and `Efds` (1.1 s) and
  on `Eemt` (10 s), with the same stall. Its CompleteInvalidate then waits
  for every pending frame to be emitted, with no limit. Ours cancels the
  waits as for a decoder, and drops the backend encoder and its pending
  frames, with their surfaces, at invalidation. Nothing is sent once
  invalidated, so the guest sees the same, sooner
  (`crates/reims-vgpu-video-core/src/host/encoder.rs`, `invalidate`).
- **A frame the guest has given up on is decoded and not emitted.**
  The guest waits for each `dfrm`'s reply before it sends the next, so a
  later `dfrm` queued behind the frame in hand means its wait has ended.
  On an x86_64 guest without VtWaitPatch that is every synchronous frame,
  since that wait ends at once ("The guest's 10 s waits expire at once on
  x86_64", below). With it, the guest waits the full 10 s, and this
  applies only to a frame that takes the host longer than that. The
  guest's VideoToolbox has then emitted the frame as failed. It discards a
  `Demt` for it ("Unrecognised VTVideoDecoderFrame token"), and in an
  x86_64 26.7.1 guest without the patch on this host its `Dbuf` handler
  stopped answering at the seventh such frame, so each frame after it
  cost 1.1 s and 10 s. Apple's host asks a buffer for every frame and
  emits it. Ours decodes such a frame, so that later frames keep their
  references, and replies to its `dfrm`, but asks no buffer for it. Nor
  does it send a `Demt` when the guest gave up while the `Dbuf` waited. In
  the same guest the Aerial wallpaper's session then lived 30 s, against
  99 s without it, and no `Dbuf` timed out. Apple's host never meets the
  unpatched x86_64 guest this was made for, since it serves arm64 guests,
  and on those the wait is 2.147 s
  (`crates/reims-vgpu-video-core/src/host/decoder.rs`, `guest_gave_up`).
- **A truncated H.264 or HEVC slice is decoded on VA-API.** Apple's AVD
  answers -12909 for it, and for each frame after it until an I picture
  ("H.264", above) or an IRAP picture ("HEVC", above). Our parsers stop at
  the slice header, and radeonsi reports nothing of the slice data: after
  a truncated slice, `vaSyncSurface2` and `vaSyncSurface` answer
  `VA_STATUS_SUCCESS`, `vaQuerySurfaceStatus` `VASurfaceReady`, and
  `vaQuerySurfaceError` `VA_STATUS_ERROR_UNIMPLEMENTED` (**measured**,
  [bugs.md](bugs.md), B41). So ours sends such a slice to the GPU, and
  emits what the GPU made with noErr, and the frames after it too. A
  garbage NAL unit, or one with the forbidden bit, is found by the parser
  and answered as Apple's is. On NVIDIA, NVDEC's status fails the frame,
  and the frames after it are answered as Apple's are ("H.264", "HEVC",
  above).
- **The frames in flight after a failure are refused too.** After an
  H.264 or HEVC frame that fails, Apple's AVD decodes asynchronously the
  frames it already holds, one for H.264 and two for HEVC in the runs
  measured ("H.264", "HEVC", above), and fails the rest. Ours has no
  frame in flight when a sample fails, so it refuses every frame after it
  in both modes, as AVD does synchronously. How many AVD holds depends on
  its pipeline and timing, which no guest can count on.
- **A lost H.264 frame's stand-in is ours.** On VA-API it is a copy of
  the last reference. On NVIDIA it is the last reference's own surface,
  or a `not_existing` entry where the DPB names that surface already, as
  NVIDIA's parser names a lost frame ([nvidia.md](nvidia.md),
  "Concealment"). Apple's AVD emits the frames after the loss too, but how
  it conceals the loss, and so their pixels, was not compared.
- **HEVC is listed for profiles 1 to 3.** Apple's AVD lists 1 to 4
  ("HEVC", above). The host offers what its backend decodes into core's
  pixel formats, NV12 and P010: profile 4, the range extensions, needs
  4:2:2 or 4:4:4 buffers core has no format for, so no backend lists it
  and its streams are refused
  (`crates/reims-vgpu-video-videotoolbox/src/macos/capabilities.rs`,
  `four_two_zero`). radeonsi has no HEVC range-extension profile
  either ([vaapi.md](vaapi.md), "The AMD drivers").
- **No `,dp2` with a frame.** Apple's AVD sends each frame's 544-byte
  histogram as its surface's data property 2 ("Colour and HDR", above).
  Ours sends none: what the counts count, and what in the guest reads
  them, are not known ([bugs.md](bugs.md), B28). Without it the guest
  sets no data property, its apply skipping an absent one (read).
- **`dcap` for `dvh1` is NULL.** Apple's host answers Dolby Vision's
  capabilities, profiles 5 and 8 ("Colour and HDR", above), though its
  `decs` lists no Dolby Vision decoder. Ours lists none either, and
  claims none: profile 5's base layer is not plain HEVC, and what
  Apple's host sends with a Dolby Vision frame is not measured
  ([bugs.md](bugs.md), B29). The guest does not see it: with no hardware
  decoder for `dvh1`, it makes the same Dolby Vision keys itself, and its
  HEVC dictionary had them in the guest runs of "Colour and HDR".
- **AV1 is listed for profile 0 alone.** Apple's lists 0 to 2, 4:4:4 and
  12 bits among them ("AV1", above). The host offers what its backend
  decodes: core's pixel formats are NV12 and P010, and the Radeon 780M has
  no Profile2 ([vaapi.md](vaapi.md), "The AMD drivers"), so profiles
  1 and 2 are refused and not listed.
- **AV1's film grain mode is "Immediate" alone.** Apple's AV1 decoder
  takes "Defer" too, and then leaves the grain to the caller. Every
  backend here applies it where it decodes, so ours answers "Defer" with
  -12900 rather than take a mode its frames do not keep. The guest never
  sends it: it rewrites "Defer" to "Immediate" (read).
- **On NVIDIA, AV1 with a side of 4n+3 is refused.** Apple's AV1 decoder
  decodes every size from 64x64 ("AV1", above), and so do the VA-API and
  VideoToolbox backends, VA-API from 16x16. NVDEC decodes an AV1 frame
  427, 643, 363 or 1283 wide or high, any side 3 more than a multiple of
  4, with wrong blocks in every frame and no error, at 8 and 10 bits,
  through NVIDIA's own parser too ([bugs.md](bugs.md), U7). Wrong pictures
  shown without a word are worse than a refusal the guest sees, so the
  NVIDIA backend (`crates/reims-vgpu-video-nvidia/src/decode.rs`,
  `Av1Session`):
  - refuses at `dsta` a description of such a size with -12911 and no
    `Dpba`, as Apple's decoder refuses a size it does not decode;
  - decodes a frame of such a size mid-stream, and each inter frame
    predicted from one, all the same, so that later frames keep their
    references, and emits it with -12909 and no image, as Apple's VP9
    decoder emits a frame of another size ("VP9", above). A key or
    intra-only frame of another size decodes again.

  Measured on an RTX 5080 Laptop GPU (driver 615.71.09), 2026-10-24: five
  streams of such sizes refused at their open, and aom-360p-resize-427,
  640x360 key frames with 427x240 frames between, emitting its 3 key
  frames, matching, and 27 frames with -12909; 642x362 and 644x364 decode
  and match. Undo it, `decodes_wrongly` in `src/nvdec/av1.rs` (its test
  `nvdec::av1::tests::a_side_of_4n_plus_3_is_one_nvdec_decodes_wrongly`),
  once NVIDIA's decoder decodes such sizes right.
- **Motion estimation answers where Apple's host ends the VM** ("Where
  Apple's host ends the VM", above), for host safety:
  - `mcop` of a key the processor does not know is -12900 with no `,val`,
    `kVTPropertyNotSupportedErr`, as the encoder's unknown keys are;
  - `mepf` after `minv` sends the `Memt` Apple's processor sends, -17691
    and no buffer, and then replies -17691, the processor's failure,
    where Apple's host traps between the two. `mest` after `minv` is
    -17691 too, which is not measured on Apple's;
  - a second `+mep` on an open UUID is -19093 with no reply, as Apple's,
    and leaves no lock held.
- **The host session's invalidate stops its motion-estimation sessions.**
  Apple's does not reach them, and its complete-invalidate waits for their
  frames. Ours invalidates them with the decoders and encoders, cancelling
  their waits for the guest, so that a reset never waits 10 s on a
  `Memt`. The device is resetting, so the guest sees nothing either way.
- **A session's end waits 10 s at most for its thread.** Apple's `-dec`,
  `-enc`, `-mep` and reset wait for the session's queue to finish the
  message in hand, with no limit, on the device's thread, so a codec call
  that never returned would stop the whole VM. Ours waits 10 s in all,
  the guest's own wait for a codec operation: for the thread to finish the
  message in hand, and then, its queue closed, for it to end. A reset
  waits one 10 s for all its sessions together in its invalidate, and one
  more in its complete-invalidate for their threads to end. Past it, an
  error is logged, the thread is left running, detached, and the end goes
  on (`crates/reims-vgpu-video-core/src/host/session_thread.rs`):
  - the thread keeps its backend, the message in hand, those queued
    behind it and their surfaces, which the guest sees as Busy, and they
    count against the bounds of the guest's messages below. If its call
    returns, it drops them and ends, and while the host session lives its
    next sweep sends their `rels`;
  - the session's later messages are -19094 once it is closed, as for any
    UUID without a session, and -19098 between a reset's two phases,
    where Apple's queues them; nothing is queued or held for them;
  - while 4 such threads run in the process, every `+dec`, `+enc` and
    `+mep` gets the create reply with "not available now", as past 16
    sessions (below), so what they hold stays bounded, and the guest
    decodes and encodes in software.

  Host safety. No backend call that never returns is known. A call stuck
  inside a surface's lend would hold every other lend, and none is made
  inside one: the encoder and the motion session copy their sources out
  first (below), the decoder reads its frame back before the guest's
  buffer is lent, and the JPEG engine decodes into a buffer of its own
  (below).
- **An encoder encodes a copy of its source.** Apple's host hands its
  encoder the guest's pixel buffer. Ours copies the picture out of the
  surface's read lend, each plane's rows packed at the encoded size, into
  a buffer the session keeps, and calls the backend with no lend held
  (`crates/reims-vgpu-video-core/src/host/encoder/source_copy.rs`), as the
  motion session copies its frames. A host may lend one surface at a time
  in the process, as reims-vgpu does, so a backend call that never
  returned inside a lend would hold every other lend. The guest sees no
  change: the surface is held until the frame is encoded and emitted, as
  Apple's host holds it. Host safety. The copy took 0.17 to 0.21 ms a
  frame at 1920x1080 NV12, 0.39 to 0.48 at P010, 1.0 to 1.2 at 3840x2160
  NV12 and 2.1 to 2.2 at P010, against encodes of 3.2 to 26.5 ms, in the
  suite's `encode` on VA-API (`vaapi-conformance`) on both radeonsi GPUs, with rows
  padded to a stride past the picture's (2026-10-24).
- **The JPEG engine decodes into a buffer of its own, and waits 10 s at
  most.** Apple's host hands CMPhoto's hardware decode the guest's
  destination and waits for it with no limit, on the device's queue ("The
  JPEG engine", above). Ours copies the source out of its read lend, has
  the backend decode on a thread of its own into a buffer of the host's
  with no lend held, and then copies the output into the destination's
  top-left corner in its write lend, by the CPU alone
  (`crates/reims-vgpu-video-core/src/host/jpeg.rs`). The guest sees no
  difference: the same statuses, the corner written and the rest as the
  guest left it, and nothing written on a failure. The delivering thread
  waits for the decode 10 s, the bound of a session's end (above). Past
  it:
  - the `jdec` answers kIOReturnTimeout (0xe00002d6) with nothing
    written, long after the guest's own 1.1 s wait has passed. The thread
    is left running, detached, with its backend and the host's two
    buffers, and no guest surface: the message's surfaces are let go
    with its reply, as after any `jdec`;
  - the thread counts against the 4 above. While 4 run in the process,
    a `jdec` answers kIOReturnBusy (0xe00002d5) at once, and the backend
    is not asked.

  Both statuses are ours. Host safety.
- **`Mspa` asks for the backend's formats, with no cache mode or
  wiring.** Apple's lists 83 formats, `CacheMode` and
  `IOSurfaceWiringAssertion` ("Measured on macOS 27", above). Ours lists
  what the backend reads, and leaves out the two keys, which say how
  Apple's hardware maps a surface. The guest's session converts a
  client's frames to what is listed. What is offered is what the backend
  does, as with the codec lists.
- **An unknown encoder property is refused, not a crash.** Apple's AVE
  encoder traps on `ecop` of a key it lacks, `Paravirtualized` among them,
  and the host process dies (measured on Apple's host session on the M3
  Max, 2026-10-24). Apple's software encoders answer -12900, and so does
  ours. This is for host safety: a guest's question must not end the VM.
- **What the guest sends is held to bounds of our own**, for host safety
  ([hardening.md](hardening.md), "Bounds of our own", has each, with how a
  guest would trigger it). Where Apple's host would take the input, ours
  refuses it as malformed:
  - a property list, `=pls` or a sample buffer leaf, that decodes to
    more than 65,536 values, nests more than 64 deep, or decodes to more
    than 16 MiB of strings and data beyond its own length, or that is a
    binary plist of more than 65,536 objects. A binary plist that refers
    to one object many times decodes to more than its bytes, and
    CoreFoundation's writer, which the guest's plists come from, never
    shares a container and writes only the objects the root reaches
    (`crates/reims-vgpu-video-core/src/wire/property_list.rs`).
  - a sample buffer that decodes to more than 65,536 values, its
    dictionaries, arrays, keys, nulls and format descriptions and its
    property lists' values between them, or whose property lists decode
    to more than 16 MiB of strings and data beyond its length: each leaf
    is a property list, and many small ones would otherwise decode to far
    more than their bytes (`crates/reims-vgpu-video-core/src/wire/sample_buffer.rs`).
  - a sample buffer whose dictionaries, arrays and format descriptions
    nest more than 32 deep. Apple's reader recurses with no limit, and
    would overflow its stack too (`crates/reims-vgpu-video-core/src/wire/sample_buffer.rs`).
  - an AV1 frame of more than 256 tiles, which is refused as unsupported,
    -12910, since Mesa's decoder holds 256
    (`crates/reims-vgpu-video-codec/src/av1/frame.rs`, `MAX_TILES`).
- **No message to the guest is longer than its 0x4000-byte buffers.**
  Apple's host ends the VM on one, and sends one when a reply echoes a
  guest's large property value, or a sweep releases more than 2,047
  surfaces at once. Ours sends no such reply, so the guest's wait for it
  times out, and sends the IDs in as many `rels` boxes as it takes, which
  the kext reads one by one (`crates/reims-vgpu-video-core/src/host.rs`,
  `HostShared::send`).
- **A host session keeps 16 decoder sessions at most, 16 encoder
  sessions and 16 motion-estimation sessions.** Apple's host makes a
  session for every `+dec`. Each of ours has a thread and GPU surfaces, so
  a `+dec` past 16 gets the create reply with `,err` -12913,
  `kVTVideoDecoderNotAvailableNowErr` (`VTErrors.h:42` in the macOS SDK),
  and no session is kept; `+enc` gets -12915 (`:45`), and `+mep` its
  not-found, -19350 (`crates/reims-vgpu-video-core/src/host.rs`,
  `refuse_for_now`).
- **What a host session holds of the guest's messages is bounded.**
  Apple's host queues every message for a session's serial queue, and
  gathers a fragmented `dfrm` or reply, with no limit. Ours holds 512 MiB
  and 4,096 messages at most, queued or as fragments, and one decoder,
  encoder or motion-estimation session half those bytes and a quarter of
  those messages, so that one session the guest floods cannot starve the
  rest. It answers a delivery past either with -12904
  (`kVTAllocationFailedErr`), queuing nothing. A `dfrm` past 128 MiB is
  dropped with no reply, and a reply past 16 MiB is malformed, -19093
  (`crates/reims-vgpu-video-core/src/host/guest_bounds.rs`).
- **The guest-to-host queue holds 1024 descriptors**, where Apple's queues
  hold 256 (Transport, above). On an x86_64 guest's 4 KiB pages a
  0x200000-byte message and its status are up to 514 segments, and
  AppleVirtIO refuses a message of more segments than the queue holds, so
  with 256 no message over 1 MiB was safe (measured in an x86_64 26.7.1
  guest). 1024 is QEMU's most, and holds every sample buffer message the
  guest makes; a message of over 4 MiB less a page would still be
  refused. The host-to-guest queue, whose buffers are 0x4000 bytes, keeps
  256 (`vendor/qemu/hw/display/apple-videotoolbox-pci.c`).
- **A message over 16 MiB is answered -12904, unread.** The guest's
  largest message is a 0x200000-byte sample and its header, but its
  descriptors can add up to many times the host's RAM, which a host that
  gathers the element first would allocate and copy. Ours answers a
  readable part over 16 MiB with -12904 (`kVTAllocationFailedErr`), as it
  answers a delivery past what the host session holds, with no reply: the
  library before its host session sees it (`reims_vgpu::MAX_MESSAGE_SIZE`)
  and the QEMU device before it copies. What Apple's host does with one is
  not measured. Host safety ([hardening.md](hardening.md), F46).

**The M2 scaler device** (`crates/reims-vgpu-video-core/src/scaler.rs`,
`scaler/plan.rs`; [scaler.md](scaler.md)):

- **No host pointer reaches the guest.** Apple's reply carries its own
  heap pointers: type 1's `R+0x10`, where it puts the translated
  capabilities memory, and type 2's side-buffer addresses, where it puts
  its gather and scatter buffers. A host address tells a guest where the
  host's heap is. Ours leaves what the guest sent there. The guest reads
  neither.
- **The surface IDs stay the guest's.** Apple's host writes its own
  IOSurfaces' IDs into `R+8` and `R+0xc`. A Linux host has no IOSurface
  IDs to write, and the guest does not read them back. Ours leaves the
  guest's.
- **A request before DRIVER_OK is a needs-reset.** Apple's handler passes
  its accelerator, made at DRIVER_OK, to
  `IOSurfaceAcceleratorParavirtProcessGuestData`, which dereferences it:
  before DRIVER_OK, or after a reset, that pointer is null, and the host
  process would crash (read). Ours resets the device instead, as Apple's
  does when it cannot make the accelerator.
- **A host without a GPU refuses the guest's features.** Apple's host
  always has its scaler. Ours transforms with a kernel of its own on the
  GPU the library draws with, through Metal on macOS and the Vulkan
  platform on Linux; a host where that opens no device, or whose device
  refuses the kernel, has no scaler ([scaler.md](scaler.md), "A host
  without a GPU"). Its scaler device is still made, so the VM starts, and
  its transport refuses the guest's features: FEATURES_OK does not stay
  set. The guest kext's start
  sets ACKNOWLEDGE, DRIVER and then FEATURES_OK, reading each back, and
  gives up at the first that did not stay set: it writes FAILED and returns
  false before DRIVER_OK, before its type 1 request, and registers no
  service, so IOSurfaceAccelerator finds no scaler, as in a VM without the
  device. The other answers were worse for the guest: a device that never
  replied to type 1 would hang the kext's start, which waits for that reply
  with no timeout, and a needs-reset at DRIVER_OK leaves a bound driver
  whose every transform fails.
- **A surface the mapper does not have is kIOReturnNotFound.** Apple's
  host asks `IOSurfaceGetID` of the null surface and transforms anyway;
  what its kernel answers is not measured. Ours answers 0xe00002f0
  without a transform.
- **A window outside its surface is kIOReturnBadArgument**, and an option
  this host does not do ([scaler.md](scaler.md), "How this host does a
  transform") kIOReturnUnsupported, the status Apple's scaler gives a
  conversion it does not do. What Apple's kernel does with either is not
  measured.
- **A readable part over 64 KiB is a needs-reset.** Apple's host copies
  the readable part, whatever its size, into a buffer it allocates before
  it checks anything, and replies an unknown type's unchanged. So a
  guest's descriptors could have the host allocate and copy gigabytes.
  The guest kext's largest request is a transform's 0x608 bytes. Ours
  refuses past 64 KiB (`scaler::MAX_REQUEST_SIZE`), and the QEMU device
  checks it before it copies the element. Host safety.
- **The capabilities list four format classes**, `2Planes420`,
  `ARGB8888`, `Interleaved422` and `2Planes42010`, the eight formats this
  host does, where the M3 Max's list every class. As with the codecs the
  VideoToolbox device lists, what is offered is what the host does. The
  sizes, 4 to 65536 a side, the ratios, 8, 8, 8 and 4, and offsets
  anywhere are the M3 Max's. There is no HDR, ASE, comm API, histogram or
  compression, so a guest never sends side buffers, though the device
  handles them as Apple's does.

**Our x86_64 guests.**

- **The guest's 10 s reply waits wait 10 s** in the x86_64 guests the
  measurements here ran in, which carry `VtWaitPatch.kext`, a plugin
  outside this repository that patches the guest's VideoToolbox in memory
  ("What an x86_64 guest has", below). That is not the host. Apple's
  25G241 x86_64 build expires every 10 s wait at once (`INT32_MIN`), its
  arm64e build waits 2.147 s (`fcvtzs` saturates), and macOS 27 waits
  10 s. The patch waits 10 s, macOS 27's behaviour. The arm64 build's
  2.147 s would be the closer match to the same build, but it needs a
  saturating sequence the convert's five bytes cannot hold, and the
  convert's page has no code cave to put one in. 10 s is Apple's too,
  keeps the larger margin (a cold 4K first frame took 906 ms on
  radeonsi), and has no wrap for timeouts past `INT32_MAX`. Every
  in-range value converts as Apple's does. Only the 10 s codec timeout
  reaches the convert, so the default 1.1 s waits are Apple's unchanged.
  Users' guests do not have the patch, and this host serves them as "A
  frame the guest has given up on is decoded and not emitted" says,
  above.

## What an x86_64 guest has

macOS 26.7.1 (25G241), read in the release's images and its x86_64
caches, 2026-10-24:

| | arm64 guest | x86_64 guest |
|---|---|---|
| VideoToolbox's guest half | ships | ships: the `ParavirtualizedVideoDecoder_*` and `…Encoder_*` symbols are in the x86_64 shared cache |
| VideoToolboxParavirtualizationSupport.framework | ships | ships |
| IOSurfaceAccelerator.framework | ships | ships |
| AppleVirtIO.kext | ships | ships, in the Boot KC |
| AppleVideoToolboxParavirtualization.kext | ships | missing: it ships for arm64e alone |
| AppleM2ScalerParavirtDriver.kext | ships | missing: it ships for arm64e alone |
| AppleParavirtIOSurface.kext, the mapper both kexts link | ships | missing; the x86_64 port's kext gives it, on reims-vgpu's 106b:eeef ([qemu.md](../qemu.md), "The IOSurface device over PCI") |

What the kexts link, from their Info.plist:

- **The video kext** links AppleVirtIO, AppleParavirtIOSurface, IOSurface
  and the iokit and libkern KPIs.
- **The scaler kext** links those too, and also bsd, mach, unsupported,
  private, AppleARMPlatform and IODARTFamily. The last two have no x86_64
  form, and they are the scaler port's own work.
- Neither kext changes transport between the two architectures: both are
  already PCI.
- macOS 27 has no x86_64 release, so an x86_64 guest is macOS 26 at most.

**The guest's 10 s waits expire at once on x86_64.** The guest's reply
clerk turns a wait's timeout into nanoseconds as an `int32_t`: the
default, 1.1 s, comes from a cached value, and any other through
`(int32_t)(CMTimeGetSeconds(timeout) * 1000000000.0)`. The codec
operations' timeout is 10 s, which is out of an `int32_t`'s range.
- **Measured** in an x86_64 26.7.1 (25G241) guest without the patch below
  on this host, 2026-10-24: every `efrm` and the `ecfr` of a 10-frame
  encode logged `timeout waiting for pending reply` within a few
  milliseconds of being sent. The host replied to each 15 ms later.
  EncodeFrame then failed with -19092, so each frame reached the client
  with that error, and the host's `Eemt` for it came too late to count.
  The 1.1 s waits (`+enc`, `esta`, the properties) worked.
- **Measured for a decode** in the same guest, 2026-10-24. The Aerial
  wallpaper decodes synchronously: `,dfl` 0 on each of the 15 `dfrm`s its
  host session took up. It sent 11 in 65 ms while the host was still
  decoding the first (906 ms, cold), and 28,619 in the 99 s the session
  lived, of which the per-session bound refused 24,693. It logged
  `VideoPlayerLayer failed to create decoded pixel buffer` from 6 ms after
  the session opened, before the host's first `Dbuf`, and 4,110 `dfrm`
  reply timeouts in under two minutes. Its VideoToolbox discarded the
  host's `Demt`s for the first six frames as "Unrecognised
  VTVideoDecoderFrame token", though the decoder answered them. From the
  seventh on the guest answered no `Dbuf`.
- **Why**, read in the 25G241 x86_64 shared caches (2026-10-24):
  `VTParavirtualizationReplyClerkWaitForReply` converts with one
  truncating `cvttsd2si` into a 32-bit register, then calls
  `dispatch_time`. Out of range, that instruction gives `0x80000000`,
  `INT32_MIN`, so the deadline is 2.1 s in the past and the wait returns
  at once unless the reply is already in. The 25G241 arm64e cache's copy
  converts with `fcvtzs` into a 32-bit register, which saturates to
  `INT32_MAX`, so an arm64 guest of that build waits 2.147 s, not 10 s.
  macOS 27 (26A434) on the M3 Max converts into a 64-bit register, and
  waits the 10 s. What each instruction gives out of range is the
  architectures'; the arm64 wait is read, not run.
- **Every timeout that reaches the convert is the 10 s codec timeout**
  (read 2026-10-24, every guest-side caller of
  `VTParavirtualizationGuestSendMessage[WithIOSurfaces]ToHostAndCopyReplySync`).
  Each caller passes either `kVTParavirtualizationDefaultReplyTimeout`
  (1.1 s), which the function serves from a cached `int32_t` and never
  converts, or `kVTParavirtualizationCodecOperationReplyTimeout` (10 s),
  which does reach the `cvttsd2si`. The callers that pass the 10 s
  timeout, and the only ones that do:
  - the encoder's `efrm` and `ecfr`, and its tile and multi-image frames;
  - the decoder's synchronous `dfrm` and `dtil`, each where the frame is
    not `kVTDecodeFrame_EnableAsynchronousDecompression`; an asynchronous
    `dfrm` keeps the 1.1 s default, so an asynchronous decode works on
    such a guest;
  - motion estimation's `mepf` and `mcfr`.

  So the one value the `cvttsd2si` ever sees is `10 * 1e9`.
- **The host cannot answer in time.** The guest's wait follows its send at
  once, and a reply that is not in by then is too late.
- **The test guests patch the convert to wait the full 10 s.** The
  `VtWaitPatch.kext` Lilu plugin, outside this repository, changes two
  spans of bytes on the convert's page through code signing's
  `_cs_validate_page`, so the guest waits the full 10 s, as macOS 27's
  build of this function does: it widens the convert to 64 bits
  (`cvttsd2si %xmm0,%r14d` -> `...,%r14`, REX.W) and turns the
  sign-extending `movslq %r14d,%rsi` after it, the next use of the value,
  into `mov %r14,%rsi`, so the whole 64-bit result reaches
  `dispatch_time`. The 10 s timeout then waits 10 s instead of expiring
  at `INT32_MIN`, and every in-range value converts bit for bit as
  before (checked under Rosetta, and against arm64's `fcvtzs`). This is a
  difference from Apple's 25G241 x86_64 on purpose, in those guests alone
  ("Where we differ from Apple", "Our x86_64 guests"); it is reversible
  (an in-memory page, never the cache file) and ships to no user image.
