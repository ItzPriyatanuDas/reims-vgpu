//! The host session under seeded fuzzing.
//!
//! A stand-in guest opens decoder, encoder and motion-estimation sessions
//! and closes them, starts them, sends frames whole and in fragments, sets
//! and copies properties, has the JPEG engine decode, asks for the host's
//! lists, and sends messages of anything, now and then mutated. A thread
//! of its own answers what the host asks (`Dbuf`, `Demt`, `Epba`, `Efds`,
//! `Eemt`), mostly as a guest does, and now and then late in pieces, with
//! an error, mangled, twice, to another request or not at all. The session
//! runs on the stand-ins of [`standins`], which reach no GPU, driver or
//! VideoToolbox.
//!
//! Each input is checked for:
//! - no panic on any thread, within its time and memory bounds;
//! - every message to the guest within the guest's 0x4000-byte buffer,
//!   and readable;
//! - every frame and buffer a `Demt` or `Dfub` names, one the guest gave;
//! - every `Eemt` reading as one message from its fragments, with a sample
//!   buffer that reads and holds the sample the encoder made, unless it
//!   carries an error;
//! - no surface held once the session is dropped.

mod standins;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reims_vgpu_video_core::backend::{Backend, PixelFormat};
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::host::{GUEST_BUFFER_SIZE, GuestSink, HostSession};
use reims_vgpu_video_core::plist::{self, Value};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::MappingId;
use reims_vgpu_video_core::sync::{Condvar, Mutex};
use reims_vgpu_video_core::wire::message::{
    GuestUuid, MediaTime, Message, MessageFlags, MessageHeader, MessageWriter, ValueType,
    read_header,
};
use reims_vgpu_video_core::wire::sample_buffer::{SampleBuffer, SampleTiming};
use reims_vgpu_video_core::wire::surface_boxes::{SurfaceList, read_surface_release, surface_list};

use self::standins::{Decoders, Encoders, Jpeg, Pool, Produced, SURFACES, hash};
use crate::generate::{description, message, plist as plists, record, sample_buffer};
use crate::support::{Rng, Target, long, maybe_mutate, mix, quick};

/// One input: a host session, its stand-ins and a guest's sequence.
/// Counted in the whole process, for the session's threads.
const HOST: Target = Target::new("host-session").process();

#[test]
fn host_session() {
    let report = quick(&HOST, 200, host_case);
    print_depth(report.inputs);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn host_session_long() {
    let report = long(&HOST, host_case);
    print_depth(report.inputs);
}

/// How deep the inputs got: what came back to the guest.
static TO_GUEST: AtomicU64 = AtomicU64::new(0);
static DECODED_EMITTED: AtomicU64 = AtomicU64::new(0);
static ENCODED_EMITTED: AtomicU64 = AtomicU64::new(0);
static JPEG_DECODED: AtomicU64 = AtomicU64::new(0);

fn print_depth(inputs: u64) {
    eprintln!(
        "host: {} messages to the guest, {} decoded frames and {} encoded ones emitted, \
         {} AV1 frames told of, {} JPEG decodes, in {inputs} inputs",
        TO_GUEST.load(Ordering::Relaxed),
        DECODED_EMITTED.load(Ordering::Relaxed),
        ENCODED_EMITTED.load(Ordering::Relaxed),
        standins::AV1_STEPS.load(Ordering::Relaxed),
        JPEG_DECODED.load(Ordering::Relaxed),
    );
}

const fn key(name: &[u8; 4]) -> FourCc {
    FourCc::new(*name)
}

const VERSION: FourCc = key(b",ver");
const ERROR: FourCc = key(b",err");
const CODEC: FourCc = key(b",cod");
const CODEC_ID: FourCc = key(b",cid");
const GUEST_INFO: FourCc = key(b",gin");
const FORMAT_DESCRIPTION: FourCc = key(b",fds");
const GUEST_POOL: FourCc = key(b",blu");
const FRAME: FourCc = key(b",frm");
const SAMPLE_BUFFER: FourCc = key(b",sbf");
const ENCODE_INFO_FLAGS: FourCc = key(b",efl");
/// `kVTEncodeInfo_FrameDropped`: an `Eemt` of a frame the encoder dropped.
const INFO_FRAME_DROPPED: u32 = 1 << 1;
const DECODE_FLAGS: FourCc = key(b",dfl");
const FRAME_OPTIONS: FourCc = key(b",fop");
const PIXEL_BUFFER_UUID: FourCc = key(b",uui");
const PIXEL_BUFFER_UUIDS: FourCc = key(b",ui+");
const PROPERTY_KEY: FourCc = key(b",key");
const PROPERTY_VALUE: FourCc = key(b",val");
const PROPERTIES: FourCc = key(b",pro");
const DIMENSIONS: FourCc = key(b",vdi");
const PRESENTATION_TIME: FourCc = key(b",pts");
const DURATION: FourCc = key(b",dur");
const FRAME_PROPERTIES: FourCc = key(b",fpr");
const PIXEL_BUFFER_ATTACHMENTS: FourCc = key(b",pat");
const BEGIN_PASS_FLAGS: FourCc = key(b",bfl");
const ENCODER_SPECIFICATION: FourCc = key(b",esp");
const TILE_DIMENSIONS: FourCc = key(b",twh");
const TILE_COUNT: FourCc = key(b",ept");
const TILE: FourCc = key(b",til");
const TILE_OFFSET: FourCc = key(b",tio");
const TILE_APERTURE: FourCc = key(b",tia");
const TILE_PROPERTIES: FourCc = key(b",tip");
const WIDTH: FourCc = key(b",wdt");
const HEIGHT: FourCc = key(b",hgt");
const JPEG_OPTIONS: FourCc = key(b",joo");
const CREATED_FORMAT_DESCRIPTION: FourCc = key(b"=fds");

/// The codecs a guest names: the host's four, and others.
const CODEC_TYPES: [[u8; 4]; 7] = [
    *b"avc1", *b"hvc1", *b"av01", *b"vp09", *b"avc1", *b"hvc1", *b"mp4v",
];

/// The properties a guest sets: the encoder's and the decoder's, and
/// others (`host/encoder/properties.rs`, `host/decoder.rs`).
const PROPERTY_KEYS: [&str; 26] = [
    "AllowFrameReordering",
    "AllowTemporalCompression",
    "AverageBitRate",
    "ColorPrimaries",
    "DataRateLimits",
    "ExpectedFrameRate",
    "H264EntropyMode",
    "MaxKeyFrameInterval",
    "MaxKeyFrameIntervalDuration",
    "MaximizePowerEfficiency",
    "MultiPassStorage",
    "Paravirtualized",
    "PrioritizeEncodingSpeedOverQuality",
    "ProfileLevel",
    "Quality",
    "RealTime",
    "TransferFunction",
    "UsingHardwareAcceleratedVideoEncoder",
    "UsingHardwareAcceleratedVideoDecoder",
    "YCbCrMatrix",
    "AV1FilmGrainMode",
    "PixelBufferPoolIsShared",
    "NumberOfPendingFrames",
    "FieldMode",
    "",
    "ProfileLevel",
];

/// Values of the enumerations, so that a set is often taken.
const PROPERTY_NAMES: [&str; 22] = [
    "H264_Baseline_AutoLevel",
    "H264_Main_AutoLevel",
    "H264_High_AutoLevel",
    "H264_Main_4_1",
    "H264_Baseline_1_3",
    "H264_High_5_2",
    "HEVC_Main_AutoLevel",
    "HEVC_Main10_AutoLevel",
    "ITU_R_709_2",
    "ITU_R_2020",
    "P3_D65",
    "SMPTE_ST_2084_PQ",
    "ITU_R_2100_HLG",
    "IEC_sRGB",
    "Identity",
    "ITU_R_601_4",
    "ITU_R_2100_ICtCp",
    "CAVLC",
    "CABAC",
    "Immediate",
    "UseGamma",
    "",
];

/// What the guest gave the host, which the host may name back to it.
#[derive(Default)]
struct Given {
    frames: HashSet<i64>,
    buffers: HashSet<[u8; 16]>,
}

/// The host's messages the guest has yet to look at, and the replies to
/// its own requests.
#[derive(Default)]
struct Inbox {
    unread: VecDeque<Vec<u8>>,
    replies: HashMap<u64, i32>,
    closed: bool,
}

/// The guest's end of the device's queue 1.
struct Guest {
    inbox: Mutex<Inbox>,
    arrived: Condvar,
    given: Mutex<Given>,
    produced: Produced,
}

impl Guest {
    /// Notes the frame and buffer `bytes` give the host, as the host reads
    /// them: a frame ID or a pixel buffer's UUID.
    fn note(&self, bytes: &[u8]) {
        let Ok(message) = Message::parse(bytes) else {
            return;
        };
        let mut given = self.given.lock();
        if let Ok(Some(frame)) = message.s64(FRAME) {
            given.frames.insert(frame);
        }
        if let Ok(Some(uuid)) = message.uuid(PIXEL_BUFFER_UUID) {
            given.buffers.insert(uuid.0);
        }
    }

    /// Waits up to `timeout` for the host's reply to `reply_id`, and
    /// returns its status.
    fn wait_reply(&self, reply_id: u64, timeout: Duration) -> Option<i32> {
        let deadline = StdRuntime.deadline_in(timeout);
        let mut inbox = self.inbox.lock();
        loop {
            if let Some(status) = inbox.replies.remove(&reply_id) {
                return Some(status);
            }
            let now = StdRuntime.now();
            if now >= deadline {
                return None;
            }
            (self.arrived).wait_for(&mut inbox, deadline.saturating_duration_since(now));
        }
    }

    fn close(&self) {
        self.inbox.lock().closed = true;
        self.arrived.notify_all();
    }
}

impl GuestSink for Guest {
    fn send(&self, message: Vec<u8>) {
        assert!(
            message.len() <= GUEST_BUFFER_SIZE,
            "a {}-byte message to the guest, past its {GUEST_BUFFER_SIZE}-byte buffer",
            message.len()
        );
        TO_GUEST.fetch_add(1, Ordering::Relaxed);
        let mut inbox = self.inbox.lock();
        if message.get(4..8) == Some(b"rels") {
            assert!(
                read_surface_release(&message).is_some(),
                "a rels box the guest cannot read"
            );
        } else {
            let header = read_header(&message, 0);
            assert!(
                header.is_ok(),
                "a message to the guest whose header does not read: {:?}",
                header.err()
            );
            if let Ok(header) = header
                && header.flags.contains(MessageFlags::REPLY_TO_GUEST)
            {
                let status = Message::parse(&message)
                    .ok()
                    .and_then(|reply| reply.s32(ERROR).ok().flatten())
                    .unwrap_or(0);
                inbox.replies.insert(header.reply_id, status);
            }
        }
        inbox.unread.push_back(message);
        drop(inbox);
        self.arrived.notify_all();
    }
}

/// The guest's thread that answers the host's requests until the guest
/// closes, and checks what the host emits.
fn respond(guest: &Guest, session: &HostSession, seed: u64, buffers_from: &[u64]) {
    let mut emitted: HashMap<[u8; 16], Vec<u8>> = HashMap::new();
    let mut replies: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut buffers = 0u64;
    loop {
        let message = {
            let mut inbox = guest.inbox.lock();
            loop {
                if inbox.closed {
                    return;
                }
                if let Some(message) = inbox.unread.pop_front() {
                    break message;
                }
                guest.arrived.wait(&mut inbox);
            }
        };
        let Ok(header) = read_header(&message, 0) else {
            // A rels box: read when it came.
            continue;
        };
        let more = header.flags.contains(MessageFlags::MORE_FRAGMENTS);
        if trace() {
            let status = Message::parse(&message)
                .ok()
                .and_then(|message| message.s32(ERROR).ok().flatten());
            eprintln!(
                "host: {} {:#x} {} of {} bytes, {status:?}",
                header.message_type,
                header.flags.0,
                header.reply_id,
                message.len()
            );
        }
        match &header.message_type.0.to_be_bytes() {
            b"Eemt" => {
                let pieces = emitted.entry(header.guest_uuid.0).or_default();
                pieces.extend_from_slice(&message);
                if more {
                    continue;
                }
                let whole = std::mem::take(pieces);
                check_encoded(guest, &whole);
            }
            b"Demt" => check_decoded(guest, &message),
            b"Dfub" => check_freed(guest, &message),
            b"jdec" => {
                if let Ok(message) = Message::parse(&message)
                    && message.s32(ERROR) == Ok(Some(0))
                {
                    JPEG_DECODED.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        if more || !header.flags.contains(MessageFlags::HOST_EXPECTS_REPLY) {
            continue;
        }
        let mut rng = Rng::new(mix(seed
            ^ header.reply_id
            ^ u64::from(header.message_type.0) << 32));
        let Ok(request) = Message::parse(&message) else {
            continue;
        };
        for reply in answer(&mut rng, &header, &request, &mut buffers, buffers_from) {
            guest.note(&reply);
            // A reply in pieces is read whole, as the reply clerk joins them.
            let joined = replies.entry(header.reply_id).or_default();
            joined.extend_from_slice(&reply);
            guest.note(joined);
            if read_header(&reply, 0)
                .is_ok_and(|header| !header.flags.contains(MessageFlags::MORE_FRAGMENTS))
            {
                replies.remove(&header.reply_id);
            }
            session.deliver(&reply);
        }
    }
}

/// An `Eemt`, joined from its fragments: one message, whose sample buffer
/// reads and holds a sample the encoder made, or whose error says why it
/// has none, or which is flagged dropped for a frame the encoder gave back
/// with no data.
fn check_encoded(guest: &Guest, whole: &[u8]) {
    let message = Message::parse(whole);
    assert!(
        message.is_ok(),
        "an Eemt's {} bytes do not read as one message: {:?}",
        whole.len(),
        message.err()
    );
    let Ok(message) = message else { return };
    let status = message.s32(ERROR);
    let frame = message.s64(FRAME);
    let serialization = message.sample_buffer(SAMPLE_BUFFER);
    assert!(
        matches!(status, Ok(Some(_))) && matches!(frame, Ok(Some(_))) && serialization.is_ok(),
        "an Eemt without its status, frame or sample buffer: {status:?}, {frame:?}"
    );
    let (Ok(Some(status)), Ok(Some(frame)), Ok(serialization)) = (status, frame, serialization)
    else {
        return;
    };
    let dropped = message
        .u32(ENCODE_INFO_FLAGS)
        .ok()
        .flatten()
        .is_some_and(|flags| flags & INFO_FRAME_DROPPED != 0);
    assert_eq!(
        status == 0 && !dropped,
        serialization.is_some(),
        "an Eemt of status {status} (dropped: {dropped}) with a sample buffer: {}",
        serialization.is_some()
    );
    if dropped {
        // A frame the encoder gave back with no data.
        let empty = hash(0, &[]);
        let made = guest
            .produced
            .lock()
            .get(&frame)
            .is_some_and(|hashes| hashes.contains(&empty));
        assert!(
            status == 0 && made,
            "frame {frame} emitted as dropped with status {status}, which the encoder did not drop"
        );
        return;
    }
    let Some(serialization) = serialization else {
        return;
    };
    let sample = SampleBuffer::parse(&serialization);
    assert!(
        sample.is_ok(),
        "the host's sample buffer of {} bytes does not read: {:?}",
        serialization.len(),
        sample.err()
    );
    let Ok(sample) = sample else { return };
    let data = hash(0, sample.data.as_deref().unwrap_or_default());
    let made = guest
        .produced
        .lock()
        .get(&frame)
        .is_some_and(|hashes| hashes.contains(&data));
    assert!(
        made,
        "frame {frame}'s {}-byte sample is not one the encoder made",
        sample.data.as_ref().map_or(0, Vec::len)
    );
    ENCODED_EMITTED.fetch_add(1, Ordering::Relaxed);
}

/// A `Demt` names a frame the guest sent, and a buffer it lent.
fn check_decoded(guest: &Guest, bytes: &[u8]) {
    let Ok(message) = Message::parse(bytes) else {
        return;
    };
    let frame = message.s64(FRAME).ok().flatten().unwrap_or(0);
    let buffer = message.uuid(PIXEL_BUFFER_UUID).ok().flatten();
    let given = guest.given.lock();
    assert!(
        frame == 0 || given.frames.contains(&frame),
        "a Demt of frame {frame}, which the guest never sent"
    );
    if let Some(buffer) = buffer {
        assert!(
            given.buffers.contains(&buffer.0),
            "a Demt into buffer {buffer:?}, which the guest never lent"
        );
        DECODED_EMITTED.fetch_add(1, Ordering::Relaxed);
    }
}

/// A `Dfub` names only buffers the guest lent.
fn check_freed(guest: &Guest, bytes: &[u8]) {
    let Ok(message) = Message::parse(bytes) else {
        return;
    };
    let buffers = message.uuid_array(PIXEL_BUFFER_UUIDS).ok().flatten();
    let given = guest.given.lock();
    for buffer in buffers.unwrap_or_default() {
        assert!(
            given.buffers.contains(&buffer.0),
            "a Dfub of buffer {buffer:?}, which the guest never lent"
        );
    }
}

/// The guest's answer to one of the host's requests: mostly as a guest's
/// VideoToolbox answers, and now and then not.
fn answer(
    rng: &mut Rng,
    request: &MessageHeader,
    message: &Message<'_>,
    buffers: &mut u64,
    buffers_from: &[u64],
) -> Vec<Vec<u8>> {
    let choice = rng.below(96);
    if choice == 0 {
        // Never answered: the host's wait ends at its timeout or when the
        // session goes.
        return Vec::new();
    }
    let status = if (1..4).contains(&choice) {
        rng.interesting_i32()
    } else {
        0
    };
    let mut head = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
    head.s32(ERROR, status);
    let mut rest = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
    let mut surfaces = Vec::new();
    match &request.message_type.0.to_be_bytes() {
        b"Dbuf" => {
            *buffers += 1;
            if !rng.one_in(16) {
                let mut uuid = [0xb0; 16];
                uuid[8..].copy_from_slice(&buffers.to_be_bytes());
                rest.uuid(PIXEL_BUFFER_UUID, GuestUuid(uuid));
            }
            surfaces = match rng.below(12) {
                0 => Vec::new(),
                1 => vec![rng.range(1, SURFACES), rng.range(1, SURFACES)],
                2 => vec![rng.interesting_u64()],
                3 => vec![rng.range(1, SURFACES)],
                _ => vec![
                    buffers_from
                        .get(rng.below(buffers_from.len().max(1)))
                        .copied()
                        .unwrap_or(1),
                ],
            };
        }
        b"Efds" if !rng.one_in(16) => {
            let codec = message.s32(CODEC).ok().flatten().unwrap_or(0);
            let side = |value: Option<i32>| u16::try_from(value.unwrap_or(0)).unwrap_or(1);
            let size = (
                side(message.s32(WIDTH).ok().flatten()),
                side(message.s32(HEIGHT).ok().flatten()),
            );
            let record = rng.bool().then(|| rng.some_bytes(40));
            let created =
                description::description(rng, FourCc(codec.cast_unsigned()), size, record);
            rest.image_description(CREATED_FORMAT_DESCRIPTION, &created);
        }
        _ => {}
    }
    let with_surfaces = |bytes: Vec<u8>| {
        if surfaces.is_empty() {
            bytes
        } else {
            message::with_surfaces(&surfaces, bytes)
        }
    };
    let rest = rest.finish();
    // The parameters after the head's header, for a reply in one piece.
    let rest_parameters = rest.get(48..).unwrap_or_default();
    match choice {
        4 | 5 => {
            // In two pieces, the clerk to join them.
            head.change_flags(MessageFlags::MORE_FRAGMENTS, MessageFlags::MORE_FRAGMENTS);
            vec![head.finish(), with_surfaces(rest)]
        }
        6 => {
            let mut bytes = head.finish();
            bytes.extend_from_slice(rest_parameters);
            let donor = bytes.clone();
            maybe_mutate(rng, &mut bytes, &donor, 1);
            vec![with_surfaces(bytes)]
        }
        7 => {
            // To another request.
            let other = MessageHeader {
                reply_id: request.reply_id.wrapping_add(rng.range(1, 3)),
                ..*request
            };
            let mut bytes = MessageWriter::reply_to(&other, MessageFlags::REPLY_TO_HOST);
            bytes.s32(ERROR, status);
            vec![with_surfaces(bytes.finish())]
        }
        _ => {
            let mut bytes = head.finish();
            bytes.extend_from_slice(rest_parameters);
            let bytes = with_surfaces(bytes);
            if choice == 8 {
                vec![bytes.clone(), bytes]
            } else {
                vec![bytes]
            }
        }
    }
}

/// Whether `RVGV_FUZZ_TRACE` asks for every message on stderr, and the
/// host's log with them.
fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| {
        let trace = std::env::var_os("RVGV_FUZZ_TRACE").is_some();
        if trace && log::set_logger(&TraceLog).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
        trace
    })
}

/// The host's log, on stderr, for a trace.
struct TraceLog;

impl log::Log for TraceLog {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        eprintln!("log: {} {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

/// How long the guest waits for one reply, and for all of an input's:
/// a guest waits longer, but the host's answer either comes at once or
/// never.
const REPLY_WAIT: Duration = Duration::from_millis(20);
const REPLY_WAITS: Duration = Duration::from_millis(60);

/// The guest's side of one input.
struct Script<'a> {
    rng: &'a mut Rng,
    session: &'a HostSession,
    guest: &'a Guest,
    pool: &'a Pool,
    next_reply: u64,
    next_frame: i64,
    waited: Duration,
    /// The codec the guest opened each session for.
    codecs: HashMap<[u8; 16], FourCc>,
    /// The sessions the host said it opened.
    sessions: Sessions,
    /// The codecs the backend decodes and encodes, as `decs` and `encs`
    /// list them.
    decodable: Vec<FourCc>,
    encodable: Vec<(FourCc, Vec<FourCc>)>,
    /// The surface each encoder session's frames come from.
    sources: HashMap<[u8; 16], u64>,
    /// The sessions the host started.
    started: HashSet<[u8; 16]>,
    /// Each session's `dfrm`s since the last whole one, joined as the host
    /// joins them.
    frames: HashMap<[u8; 16], Vec<u8>>,
}

/// What a session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Decoder,
    Encoder,
}

/// The sessions the host has, as the guest knows them from the host's
/// replies: each UUID's decoder and encoder, and whether the host serves
/// it. A session refused for its codec answers nothing.
#[derive(Default)]
struct Sessions {
    decoders: HashMap<[u8; 16], bool>,
    encoders: HashMap<[u8; 16], bool>,
}

impl Sessions {
    /// Where the host routes a message for `uuid`: to its decoder first,
    /// then its encoder (`host.rs`, `route`), and whether that one serves.
    fn routed(&self, uuid: GuestUuid) -> Option<(Kind, bool)> {
        if let Some(&serving) = self.decoders.get(&uuid.0) {
            return Some((Kind::Decoder, serving));
        }
        (self.encoders.get(&uuid.0)).map(|&serving| (Kind::Encoder, serving))
    }

    /// The UUIDs whose messages a session of `kind` serves, in order.
    fn serving(&self, kind: Kind) -> Vec<GuestUuid> {
        let mut uuids: Vec<GuestUuid> = (self.decoders.keys().chain(self.encoders.keys()))
            .map(|&uuid| GuestUuid(uuid))
            .filter(|&uuid| self.routed(uuid) == Some((kind, true)))
            .collect();
        uuids.sort_by_key(|uuid| uuid.0);
        uuids.dedup();
        uuids
    }

    /// Every UUID with a session, in order.
    fn all(&self) -> Vec<GuestUuid> {
        let mut uuids: Vec<GuestUuid> = (self.decoders.keys().chain(self.encoders.keys()))
            .map(|&uuid| GuestUuid(uuid))
            .collect();
        uuids.sort_by_key(|uuid| uuid.0);
        uuids.dedup();
        uuids
    }
}

/// The sizes the host's decoders see most.
fn picture_size(rng: &mut Rng) -> (u16, u16) {
    if rng.one_in(4) {
        description::size(rng)
    } else {
        (rng.range(64, 320) as u16, rng.range(64, 320) as u16)
    }
}

impl Script<'_> {
    fn uuid(&mut self) -> GuestUuid {
        let open = self.sessions.all();
        if !open.is_empty() && !self.rng.one_in(3) {
            return *self.rng.pick(&open);
        }
        match self.rng.below(24) {
            0 => GuestUuid::ZERO,
            1 => message::uuid(self.rng),
            _ => GuestUuid([0x51 + self.rng.below(3) as u8; 16]),
        }
    }

    /// A UUID of an open session of `kind`, most often.
    fn uuid_for(&mut self, kind: Kind) -> GuestUuid {
        let open = self.sessions.serving(kind);
        if self.rng.one_in(8) {
            self.uuid()
        } else if open.is_empty() {
            // Where the guest's start opens one.
            GuestUuid([if kind == Kind::Decoder { 0x51 } else { 0x52 }; 16])
        } else {
            *self.rng.pick(&open)
        }
    }

    /// Whether the host answers `header`'s message, which lent `lent`
    /// surfaces: the host session's own messages, and an open session's
    /// of its kind. The guest waits
    /// only for those, as for the others the host sends nothing.
    fn answered(&self, header: &MessageHeader, lent: usize) -> bool {
        let kind = header.message_type.0.to_be_bytes();
        if [*b"efrm", *b"etil"].contains(&kind) && lent != 1 {
            // Without its one surface: no reply.
            return false;
        }
        if [
            *b"decs", *b"dcap", *b"encs", *b"meps", *b"jcap", *b"jdec", *b"jenc", *b"+dec",
            *b"+enc", *b"+mep",
        ]
        .contains(&kind)
        {
            return true;
        }
        let types = match self.sessions.routed(header.guest_uuid) {
            Some((Kind::Decoder, true)) => message::DECODER_TYPES,
            Some((Kind::Encoder, true)) => message::ENCODER_TYPES,
            _ => return false,
        };
        types.contains(&kind) && !header.flags.contains(MessageFlags::MORE_FRAGMENTS)
    }

    fn codec(&mut self) -> FourCc {
        if self.rng.one_in(10) {
            FourCc(self.rng.interesting_u32())
        } else {
            FourCc::new(*self.rng.pick(&CODEC_TYPES))
        }
    }

    fn request(&mut self, kind: &[u8; 4], uuid: GuestUuid) -> MessageWriter {
        let flags = if self.rng.one_in(16) {
            message::flags(self.rng)
        } else {
            MessageFlags::GUEST_EXPECTS_REPLY
        };
        MessageWriter::new(FourCc::new(*kind), flags, Some(uuid), self.next_reply)
    }

    fn time(&mut self) -> MediaTime {
        if self.rng.one_in(6) {
            sample_buffer::time(self.rng)
        } else {
            MediaTime {
                value: self.next_frame * 20,
                timescale: 600,
                flags: MediaTime::FLAG_VALID,
                epoch: 0,
            }
        }
    }

    fn plist(&mut self, writer: &mut MessageWriter, key: FourCc, value: &Value) {
        // A value plist cannot encode goes as NULL.
        if writer.property_list(key, Some(value)).is_err() {
            let _ = writer.property_list(key, None);
        }
    }

    /// A property's value: of the type its key takes, most often.
    fn property_value(&mut self) -> Value {
        match self.rng.below(8) {
            0 => Value::Boolean(self.rng.bool()),
            1 => Value::Integer(i64::from(self.rng.interesting_i32()).into()),
            2 => Value::Real(*self.rng.pick(&[
                0.0,
                -1.0,
                0.5,
                1.0,
                29.97,
                1e300,
                f64::NAN,
                f64::INFINITY,
            ])),
            3 => Value::String((*self.rng.pick(&PROPERTY_NAMES)).to_owned()),
            4 => Value::Array(
                (0..self.rng.count(4))
                    .map(|_| Value::Integer(self.rng.interesting_u64().cast_signed().into()))
                    .collect(),
            ),
            _ => plists::value(self.rng, 2),
        }
    }

    /// One message of the guest's, in pieces where it is cut into them,
    /// and whether the guest waits for its reply.
    fn next(&mut self) -> (Vec<Vec<u8>>, bool) {
        let uuid = self.uuid();
        let decoder = self.uuid_for(Kind::Decoder);
        let encoder = self.uuid_for(Kind::Encoder);
        let choice = if self.sessions.all().is_empty() && self.rng.bool() {
            4
        } else {
            self.rng.below(100)
        };
        let mut wait = true;
        let mut surfaces: Vec<u64> = Vec::new();
        let mut m;
        match choice {
            0..4 => {
                let kind = *self
                    .rng
                    .pick(&[*b"decs", *b"dcap", *b"encs", *b"meps", *b"jcap"]);
                m = self.request(&kind, GuestUuid::ZERO);
                if !self.rng.one_in(8) {
                    m.u32(VERSION, *self.rng.pick(&[6, 5, 1, 0, u32::MAX]));
                }
                let codec = self.codec();
                m.s32(CODEC, codec.0.cast_signed());
                if kind == *b"jcap" && self.rng.one_in(6) {
                    surfaces = message::surface_ids(self.rng);
                }
            }
            4..12 => {
                let encoder = self.rng.one_in(3);
                let uuid = if self.rng.one_in(8) {
                    uuid
                } else if encoder {
                    GuestUuid([*self.rng.pick(&[0x52, 0x54]); 16])
                } else {
                    GuestUuid([*self.rng.pick(&[0x51, 0x53]); 16])
                };
                m = self.request(if encoder { b"+enc" } else { b"+dec" }, uuid);
                let listed: Vec<FourCc> = if encoder {
                    self.encodable.iter().map(|(codec, _)| *codec).collect()
                } else {
                    self.decodable.clone()
                };
                let codec = if listed.is_empty() || self.rng.one_in(6) {
                    self.codec()
                } else {
                    *self.rng.pick(&listed)
                };
                self.codecs.insert(uuid.0, codec);
                if !self.rng.one_in(16) {
                    m.s32(CODEC, codec.0.cast_signed());
                }
                if !self.rng.one_in(8) {
                    m.u32(VERSION, *self.rng.pick(&[6, 5, 4, 3, 2, 0]));
                }
                if self.rng.one_in(4) {
                    let id = if self.rng.bool() {
                        let kind = if encoder {
                            "videoencoder"
                        } else {
                            "videodecoder"
                        };
                        let name = *self.rng.pick(&["avc", "hevc", "av1", "vp9"]);
                        format!("reims-vgpu-video.{kind}.{name}")
                    } else {
                        plists::string(self.rng)
                    };
                    self.plist(&mut m, CODEC_ID, &Value::String(id));
                }
                if self.rng.one_in(4) {
                    let info = Value::Dictionary(plists::dictionary(self.rng, 2));
                    self.plist(&mut m, GUEST_INFO, &info);
                }
            }
            12..14 => {
                let kind = *self.rng.pick(&[*b"+mep", *b"-mep", *b"-all"]);
                m = self.request(&kind, uuid);
            }
            14..18 => {
                let kind = *self.rng.pick(&[*b"-dec", *b"-enc", *b"-uui"]);
                m = self.request(&kind, uuid);
            }
            18..25 => {
                let uuid = decoder;
                m = self.request(b"dsta", uuid);
                let codec = match self.codecs.get(&uuid.0) {
                    Some(&codec) if !self.rng.one_in(8) => codec,
                    _ => self.codec(),
                };
                let record = (!self.rng.one_in(4)).then(|| record::record(self.rng, codec));
                let size = picture_size(self.rng);
                let fds = description::description(self.rng, codec, size, record);
                m.image_description(FORMAT_DESCRIPTION, &fds);
                if !self.rng.one_in(8) {
                    m.s32(GUEST_POOL, if self.rng.one_in(8) { 1 } else { 0 });
                }
            }
            25..50 => return self.decode_frame(decoder),
            50..55 => {
                let kind = *self.rng.pick(&[
                    *b"dfin", *b"dfin", *b"dcan", *b"dsup", *b"dcop", *b"dset", *b"dmul", *b"dsav",
                    *b"dsts", *b"dtil", *b"dtfi", *b"dinv",
                ]);
                m = self.request(&kind, decoder);
                match &kind {
                    b"dcan" | b"dsts" => {
                        let codec = self.codec();
                        let size = picture_size(self.rng);
                        let record = Some(record::record(self.rng, codec));
                        let fds = description::description(self.rng, codec, size, record);
                        m.image_description(FORMAT_DESCRIPTION, &fds);
                    }
                    b"dcop" | b"dset" => {
                        let name = Value::String((*self.rng.pick(&PROPERTY_KEYS)).to_owned());
                        self.plist(&mut m, PROPERTY_KEY, &name);
                        if kind == *b"dset" {
                            let value = self.property_value();
                            self.plist(&mut m, PROPERTY_VALUE, &value);
                        }
                    }
                    b"dmul" => {
                        let properties = self.properties();
                        self.plist(&mut m, PROPERTIES, &properties);
                    }
                    b"dtil" => {
                        m.s64(TILE, self.rng.interesting_u64().cast_signed());
                    }
                    _ => {}
                }
            }
            55..60 => {
                let kind = *self.rng.pick(&[
                    *b"esta", *b"esta", *b"esup", *b"eset", *b"eset", *b"ecop", *b"emul",
                ]);
                let uuid = encoder;
                m = self.request(&kind, uuid);
                match &kind {
                    b"esta" => {
                        let source = match self.sources.get(&uuid.0) {
                            Some(&source) if !self.rng.one_in(4) => source,
                            _ => self.rng.range(1, SURFACES),
                        };
                        let (width, height) = match self.pool.get(source) {
                            Some(surface) if !self.rng.one_in(6) => {
                                let side = |side: u32| i32::try_from(side).unwrap_or(i32::MAX);
                                (side(surface.layout.width), side(surface.layout.height))
                            }
                            _ => (self.rng.interesting_i32(), self.rng.interesting_i32()),
                        };
                        if !self.rng.one_in(10) {
                            m.i32_pair(DIMENSIONS, ValueType::Dimensions, (width, height));
                        }
                    }
                    b"eset" | b"ecop" => {
                        let name = Value::String((*self.rng.pick(&PROPERTY_KEYS)).to_owned());
                        self.plist(&mut m, PROPERTY_KEY, &name);
                        if kind == *b"eset" && !self.rng.one_in(10) {
                            let value = self.property_value();
                            self.plist(&mut m, PROPERTY_VALUE, &value);
                        }
                    }
                    b"emul" => {
                        let properties = self.properties();
                        self.plist(&mut m, PROPERTIES, &properties);
                    }
                    _ => {}
                }
            }
            60..72 => {
                let uuid = encoder;
                m = self.request(b"efrm", uuid);
                surfaces = match (self.rng.below(16), self.sources.get(&uuid.0)) {
                    (0, _) => Vec::new(),
                    (1, _) => message::surface_ids(self.rng),
                    (2..4, _) | (_, None) => vec![self.rng.range(1, SURFACES)],
                    (_, Some(&source)) => vec![source],
                };
                self.next_frame += 1;
                let frame = self.frame_id();
                m.s64(FRAME, frame);
                let time = self.time();
                m.time(PRESENTATION_TIME, time);
                if !self.rng.one_in(6) {
                    let duration = MediaTime { value: 20, ..time };
                    m.time(DURATION, duration);
                }
                if self.rng.one_in(4) {
                    let properties = Value::Dictionary(
                        [(
                            "EncoderForceKeyframe".to_owned(),
                            if self.rng.one_in(6) {
                                self.property_value()
                            } else {
                                Value::Boolean(true)
                            },
                        )]
                        .into_iter()
                        .collect(),
                    );
                    self.plist(&mut m, FRAME_PROPERTIES, &properties);
                }
                if !self.rng.one_in(4) {
                    let uuid = message::uuid(self.rng);
                    m.uuid(PIXEL_BUFFER_UUID, uuid);
                }
                if self.rng.one_in(8) {
                    let attachments = Value::Dictionary(plists::dictionary(self.rng, 2));
                    self.plist(&mut m, PIXEL_BUFFER_ATTACHMENTS, &attachments);
                }
            }
            72..76 => {
                let kind = *self.rng.pick(&[
                    *b"ecfr", *b"ecfr", *b"epfr", *b"emif", *b"ebps", *b"eeps", *b"ests", *b"epti",
                    *b"etil", *b"ecti", *b"einv", *b"esav",
                ]);
                m = self.request(&kind, encoder);
                match &kind {
                    b"ecfr" => {
                        let time = self.time();
                        m.time(PRESENTATION_TIME, time);
                    }
                    b"emif" => {
                        m.s64(FRAME, self.next_frame);
                        let time = self.time();
                        m.time(PRESENTATION_TIME, time);
                        let uuids: Vec<GuestUuid> = (0..self.rng.count(3))
                            .map(|_| message::uuid(self.rng))
                            .collect();
                        m.uuid_array(PIXEL_BUFFER_UUIDS, &uuids);
                    }
                    b"ebps" => {
                        m.u32(BEGIN_PASS_FLAGS, self.rng.interesting_u32());
                    }
                    b"ests" => {
                        let specification = Value::Dictionary(plists::dictionary(self.rng, 2));
                        self.plist(&mut m, ENCODER_SPECIFICATION, &specification);
                        let size = (self.rng.interesting_i32(), self.rng.interesting_i32());
                        m.i32_pair(TILE_DIMENSIONS, ValueType::Size, size);
                    }
                    b"epti" => {
                        m.s32(TILE_COUNT, self.rng.interesting_i32());
                    }
                    b"etil" => {
                        surfaces = vec![self.rng.range(1, SURFACES)];
                        m.s64(TILE, self.rng.interesting_u64().cast_signed());
                        let point = (self.rng.interesting_i32(), self.rng.interesting_i32());
                        m.i32_pair(TILE_OFFSET, ValueType::Point, point);
                        m.i32_pair(TILE_APERTURE, ValueType::Size, point);
                        let properties = Value::Dictionary(plists::dictionary(self.rng, 1));
                        self.plist(&mut m, TILE_PROPERTIES, &properties);
                    }
                    _ => {}
                }
            }
            76..85 => {
                let kind = if self.rng.one_in(8) { b"jenc" } else { b"jdec" };
                m = self.request(kind, GuestUuid::ZERO);
                let source = self.rng.range(1, SURFACES);
                let destination = self.rng.range(1, SURFACES);
                surfaces = match self.rng.below(32) {
                    0 => vec![source],
                    1 => vec![source, destination, source],
                    _ => vec![source, destination],
                };
                if let (Some(source), Some(destination)) =
                    (self.pool.get(source), self.pool.get(destination))
                {
                    let layout = &destination.layout;
                    let options =
                        self.jpeg_options(layout.pixel_format, layout.width, layout.height);
                    if !self.rng.one_in(16) {
                        self.plist(&mut m, JPEG_OPTIONS, &Value::Dictionary(options));
                    }
                    if self.rng.bool() {
                        let stream = self.rng.some_bytes(300);
                        source.fill(&stream);
                    }
                }
            }
            85..90 => {
                // A stray reply, which may meet a request the host waits on.
                let kind = *self.rng.pick(message::HOST_REQUEST_TYPES);
                m = MessageWriter::new(
                    FourCc::new(kind),
                    MessageFlags::REPLY_TO_HOST,
                    Some(uuid),
                    self.rng.range(98, 112),
                );
                m.s32(ERROR, 0);
                if self.rng.bool() {
                    m.uuid(PIXEL_BUFFER_UUID, message::uuid(self.rng));
                    surfaces = vec![self.rng.range(1, SURFACES)];
                }
                wait = false;
            }
            _ => return (vec![message::any(self.rng)], false),
        }
        let mut bytes = m.finish();
        if !surfaces.is_empty() {
            bytes = message::with_surfaces(&surfaces, bytes);
        }
        (vec![bytes], wait)
    }

    /// A frame's ID: the next most often.
    fn frame_id(&mut self) -> i64 {
        if self.rng.one_in(10) {
            self.rng.interesting_u64().cast_signed()
        } else {
            self.next_frame
        }
    }

    /// A `dmul` or `emul`'s properties.
    fn properties(&mut self) -> Value {
        let mut properties = plist::Dictionary::new();
        for _ in 0..self.rng.count(5) {
            let name = (*self.rng.pick(&PROPERTY_KEYS)).to_owned();
            let value = self.property_value();
            properties.insert(name, value);
        }
        if self.rng.one_in(8) {
            Value::Array(Vec::new())
        } else {
            Value::Dictionary(properties)
        }
    }

    /// A `jdec`'s options, for a destination in `format` of `width` by
    /// `height`: most often consistent, as CMPhoto writes them.
    fn jpeg_options(&mut self, format: FourCc, width: u32, height: u32) -> plist::Dictionary {
        let rng = &mut *self.rng;
        let taken = match &format.0.to_be_bytes() {
            b"420v" | b"420f" => Some(0),
            b"422v" | b"yuvs" | b"2vuy" => Some(1),
            b"444f" => Some(2),
            _ => None,
        };
        let subsampling = match taken {
            _ if rng.one_in(16) => i64::from(rng.interesting_i32()),
            Some(taken) if !rng.one_in(4) => {
                if rng.one_in(6) {
                    4
                } else {
                    taken
                }
            }
            _ => rng.below(5) as i64,
        };
        let (mcu_width, mcu_height) = match subsampling {
            0 => (16, 16),
            1 => (16, 8),
            3 => (32, 8),
            _ => (8, 8),
        };
        let side = |rng: &mut Rng, most: u32| {
            if rng.one_in(10) {
                i64::from(rng.interesting_i32())
            } else {
                i64::from(rng.range_u32(32.min(most.clamp(1, 400)), most.clamp(1, 400)))
            }
        };
        let (image_width, image_height) = (side(rng, width), side(rng, height));
        let mcus = |side: i64, mcu: i64| (side.max(0) + mcu - 1) / mcu;
        let (across, down) = (mcus(image_width, mcu_width), mcus(image_height, mcu_height));
        let scale = *rng.pick(&[1, 1, 1, 2, 4, 8, 3]);
        let number = |rng: &mut Rng, value: i64| {
            if rng.one_in(24) {
                Value::Real(value as f64 + 0.5)
            } else if rng.one_in(200) {
                Value::String(value.to_string())
            } else {
                Value::Integer(value.into())
            }
        };
        let mut options = plist::Dictionary::new();
        let mut put = |rng: &mut Rng, name: &str, value: i64| {
            if !rng.one_in(100) {
                let value = number(rng, value);
                options.insert(name.to_owned(), value);
            }
        };
        put(rng, "Flags", 0);
        put(rng, "Width", image_width);
        put(rng, "Height", image_height);
        put(rng, "Subsampling", subsampling);
        let off = |rng: &mut Rng| if rng.one_in(12) { 1 } else { 0 };
        let mcus_across = across + off(rng);
        let mcus_down = down + off(rng);
        put(rng, "MCUsHor", mcus_across);
        put(rng, "MCUsVer", mcus_down);
        put(rng, "DecodeWidth", image_width / scale);
        put(rng, "DecodeHeight", image_height / scale);
        if rng.one_in(4) {
            let x = rng.range(0, across.max(1) as u64 - 1);
            let y = rng.range(0, down.max(1) as u64 - 1);
            let crop = [
                ("X", x),
                ("Y", y),
                ("Width", rng.range(0, across.max(1) as u64 - x + 2)),
                ("Height", rng.range(0, down.max(1) as u64 - y + 2)),
            ];
            let rect = crop
                .into_iter()
                .map(|(name, value)| (name.to_owned(), Value::Real(value as f64)))
                .collect();
            options.insert("CropRect".to_owned(), Value::Dictionary(rect));
        }
        options
    }

    /// A `dfrm`: a sample buffer of a frame's bytes, now and then with a
    /// new format description, whole or cut into fragments.
    fn decode_frame(&mut self, uuid: GuestUuid) -> (Vec<Vec<u8>>, bool) {
        self.next_frame += 1;
        let frame = self.frame_id();
        let flags = if self.rng.one_in(8) {
            message::flags(self.rng)
        } else {
            MessageFlags::GUEST_EXPECTS_REPLY
        };
        let kind = FourCc::new(*b"dfrm");
        let mut head = MessageWriter::new(kind, flags, Some(uuid), self.next_reply);
        if !self.rng.one_in(16) {
            head.s64(FRAME, frame);
        }
        if !self.rng.one_in(4) {
            let decode_flags = if self.rng.one_in(6) {
                self.rng.interesting_u32()
            } else {
                *self.rng.pick(&[0, 1, 2, 3, 4, 8])
            };
            head.u32(DECODE_FLAGS, decode_flags);
        }
        if self.rng.one_in(6) {
            let options = Value::Dictionary(plists::dictionary(self.rng, 2));
            self.plist(&mut head, FRAME_OPTIONS, &options);
        }
        let data = self.rng.some_bytes(64);
        let sizes = if self.rng.one_in(6) {
            (0..self.rng.count(3))
                .map(|_| self.rng.interesting_u32())
                .collect()
        } else {
            vec![data.len() as u32]
        };
        let format = self.rng.one_in(6).then(|| {
            let codec = match self.codecs.get(&uuid.0) {
                Some(&codec) if !self.rng.one_in(6) => codec,
                _ => self.codec(),
            };
            let (width, height) = picture_size(self.rng);
            let record = Some(self.rng.some_bytes(40));
            sample_buffer::format_description(
                self.rng,
                codec,
                (i32::from(width), i32::from(height)),
                record,
            )
        });
        let sample = if self.rng.one_in(5) {
            sample_buffer::for_sample(self.rng, data, sizes, format)
        } else {
            // As a guest's VideoToolbox writes one: a sample, its timing
            // and its size.
            let time = self.time();
            SampleBuffer {
                sample_count: 1,
                timings: vec![SampleTiming {
                    duration: MediaTime { value: 20, ..time },
                    presentation: time,
                    decode: time,
                }],
                sizes: vec![data.len() as u32],
                data: Some(data),
                output_presentation_time: None,
                format_description: format,
                cryptor: None,
                propagated_attachments: None,
                other_attachments: None,
                sample_attachments: None,
            }
        };
        let serialization = sample.serialize();
        if self.rng.one_in(4) {
            let limit = self.rng.range(head.len() as u64 + 18, 2000) as usize;
            let header = (kind, flags, uuid, self.next_reply);
            let pieces = message::fragments(head, header, SAMPLE_BUFFER, &serialization, limit);
            return (pieces, flags.contains(MessageFlags::GUEST_EXPECTS_REPLY));
        }
        head.sample_buffer_with_limit(SAMPLE_BUFFER, usize::MAX, &serialization);
        (
            vec![head.finish()],
            flags.contains(MessageFlags::GUEST_EXPECTS_REPLY),
        )
    }

    /// Sends the next message, and waits for its reply where the guest
    /// would.
    fn step(&mut self) {
        // A session the host serves but that has not started is started,
        // most often, as a guest starts each before its frames.
        for kind in [Kind::Decoder, Kind::Encoder] {
            let waiting: Vec<GuestUuid> = (self.sessions.serving(kind).into_iter())
                .filter(|uuid| !self.started.contains(&uuid.0))
                .collect();
            if !waiting.is_empty() && !self.rng.one_in(3) {
                let uuid = *self.rng.pick(&waiting);
                match kind {
                    Kind::Decoder => self.start_decoder(uuid),
                    Kind::Encoder => self.start_encoder(uuid),
                }
            }
        }
        let next = self.next();
        self.send(next);
    }

    /// Sends one message, and waits for its reply where the guest would;
    /// a `+dec` or `+enc` the host takes is a session the guest has.
    fn send(&mut self, (pieces, mut wait): (Vec<Vec<u8>>, bool)) -> bool {
        let reply_id = self.next_reply;
        self.next_reply += 1;
        let mut delivered = true;
        let mut last = None;
        let mut lent = 0;
        for mut piece in pieces {
            if self.rng.one_in(12) {
                let donor = piece.clone();
                maybe_mutate(self.rng, &mut piece, &donor, 1);
                wait = false;
            }
            self.note(&piece);
            let (status, ids) = if self.rng.one_in(16) {
                // As the device does when it has read the subs box itself.
                let ids: Vec<MappingId> = message::surface_ids(self.rng)
                    .into_iter()
                    .map(MappingId)
                    .collect();
                (self.session.deliver_from_guest(&piece, &ids), ids)
            } else {
                let ids = match surface_list(&piece) {
                    SurfaceList::Surfaces(ids) => ids,
                    _ => Vec::new(),
                };
                (self.session.deliver(&piece), ids)
            };
            // Each is held, or the message has none.
            lent = if ids.iter().all(|id| self.pool.holdable(id.0)) {
                ids.len()
            } else {
                0
            };
            if trace() {
                let header =
                    read_header(&piece, 0).map(|h| (h.message_type, h.flags.0, h.guest_uuid.0[0]));
                eprintln!(
                    "guest: {header:?} of {} bytes, {lent} surfaces: {status}",
                    piece.len()
                );
            }
            if status != 0 {
                wait = false;
                delivered = false;
            }
            last = read_header(&piece, 0).ok();
        }
        let Some(header) = last.filter(|_| delivered) else {
            return false;
        };
        wait &= header.flags.contains(MessageFlags::GUEST_EXPECTS_REPLY)
            && header.reply_id == reply_id
            && self.answered(&header, lent);
        let status = if wait && self.waited < REPLY_WAITS {
            let started = StdRuntime.now();
            let status = self.guest.wait_reply(reply_id, REPLY_WAIT);
            self.waited += StdRuntime.elapsed_since(started);
            if trace() && status.is_none() {
                eprintln!("guest: no reply to {} {reply_id}", header.message_type);
            }
            status
        } else {
            None
        };
        let uuid = header.guest_uuid.0;
        let sessions = &mut self.sessions;
        let kind = header.message_type.0.to_be_bytes();
        if [*b"dsta", *b"esta"].contains(&kind) && status == Some(0) {
            self.started.insert(uuid);
        } else if [
            *b"+dec", *b"+enc", *b"-dec", *b"-enc", *b"-uui", *b"dinv", *b"einv",
        ]
        .contains(&kind)
            && status != Some(-19093)
        {
            self.started.remove(&uuid);
        }
        match &kind {
            b"+dec" => {
                sessions.decoders.insert(uuid, status == Some(0));
            }
            b"+enc" => {
                sessions.encoders.insert(uuid, status == Some(0));
            }
            b"-dec" => {
                sessions.decoders.remove(&uuid);
            }
            b"-enc" => {
                sessions.encoders.remove(&uuid);
            }
            b"-uui" => {
                sessions.decoders.remove(&uuid);
                sessions.encoders.remove(&uuid);
            }
            _ => {}
        }
        delivered
    }

    /// A guest's start: a decoder session started on a codec the host
    /// lists, an encoder session started on the size of a surface in a
    /// format it takes, or neither, each now and then.
    fn setup(&mut self) {
        let decoder = GuestUuid([0x51; 16]);
        if !self.decodable.is_empty() && !self.rng.one_in(4) {
            let codec = *self.rng.pick(&self.decodable);
            self.codecs.insert(decoder.0, codec);
            let mut m = self.request(b"+dec", decoder);
            m.s32(CODEC, codec.0.cast_signed()).u32(VERSION, 6);
            self.send((vec![m.finish()], true));
            self.start_decoder(decoder);
        }
        let encoder = GuestUuid([0x52; 16]);
        if !self.encodable.is_empty() && self.rng.bool() {
            let (codec, _) = self.rng.pick(&self.encodable).clone();
            self.codecs.insert(encoder.0, codec);
            let mut m = self.request(b"+enc", encoder);
            m.s32(CODEC, codec.0.cast_signed())
                .u32(VERSION, *self.rng.pick(&[6, 6, 2]));
            self.send((vec![m.finish()], true));
            for _ in 0..self.rng.count(3) {
                let mut m = self.request(b"eset", encoder);
                let name = Value::String((*self.rng.pick(&PROPERTY_KEYS)).to_owned());
                self.plist(&mut m, PROPERTY_KEY, &name);
                let value = self.property_value();
                self.plist(&mut m, PROPERTY_VALUE, &value);
                self.send((vec![m.finish()], true));
            }
            self.start_encoder(encoder);
        }
    }

    /// `dsta` for the decoder session of `uuid`, on its codec.
    fn start_decoder(&mut self, uuid: GuestUuid) {
        let codec = match self.codecs.get(&uuid.0) {
            Some(&codec) => codec,
            None => self.codec(),
        };
        let mut m = self.request(b"dsta", uuid);
        let record = Some(self.rng.some_bytes(40));
        let size = picture_size(self.rng);
        let fds = description::description(self.rng, codec, size, record);
        m.image_description(FORMAT_DESCRIPTION, &fds)
            .s32(GUEST_POOL, 0);
        self.send((vec![m.finish()], true));
    }

    /// `esta` for the encoder session of `uuid`, on the size of a surface
    /// in a format its codec's encoder takes, which its frames then come
    /// from.
    fn start_encoder(&mut self, uuid: GuestUuid) {
        let formats = match self.codecs.get(&uuid.0) {
            Some(codec) => (self.encodable.iter())
                .find(|(offered, _)| offered == codec)
                .map(|(_, formats)| formats.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let sources: Vec<u64> = (1..=SURFACES)
            .filter(|&id| {
                self.pool.holdable(id)
                    && self
                        .pool
                        .get(id)
                        .is_some_and(|surface| formats.contains(&surface.layout.pixel_format))
            })
            .collect();
        let source = if sources.is_empty() {
            self.rng.range(1, SURFACES)
        } else {
            *self.rng.pick(&sources)
        };
        self.sources.insert(uuid.0, source);
        let mut m = self.request(b"esta", uuid);
        if let Some(surface) = self.pool.get(source) {
            let side = |side: u32| i32::try_from(side).unwrap_or(i32::MAX);
            let size = (side(surface.layout.width), side(surface.layout.height));
            m.i32_pair(DIMENSIONS, ValueType::Dimensions, size);
        }
        self.send((vec![m.finish()], true));
    }

    /// Notes what a message gives the host, and what the `dfrm`s of its
    /// session give joined, as the host joins them.
    fn note(&mut self, bytes: &[u8]) {
        self.guest.note(bytes);
        let Ok(header) = read_header(bytes, 0) else {
            return;
        };
        if header.message_type != FourCc::new(*b"dfrm") {
            return;
        }
        let joined = self.frames.entry(header.guest_uuid.0).or_default();
        joined.extend_from_slice(bytes);
        self.guest.note(joined);
        if !header.flags.contains(MessageFlags::MORE_FRAGMENTS) {
            joined.clear();
        }
    }
}

/// How many messages a guest sends after its start: a few to dozens.
fn script_rng_steps(rng: &mut Rng) -> usize {
    match rng.below(8) {
        0 | 1 => rng.below(4),
        2 => rng.range(48, 160) as usize,
        _ => rng.range(4, 48) as usize,
    }
}

/// One input: a session over stand-ins, a guest's messages, and the
/// session's end.
fn host_case(rng: &mut Rng) {
    let produced: Produced = Arc::default();
    let pool = Arc::new(Pool::new(rng));
    let backend = Backend {
        name: "fuzz".to_owned(),
        decoders: (!rng.one_in(10)).then(|| Arc::new(Decoders::new(rng)) as _),
        encoders: (!rng.one_in(10)).then(|| Arc::new(Encoders::new(rng, produced.clone())) as _),
        motion: None,
        jpeg: (!rng.one_in(6)).then(|| Arc::new(Jpeg::new(rng)) as _),
    };
    let guest = Arc::new(Guest {
        inbox: Mutex::default(),
        arrived: Condvar::new(),
        given: Mutex::default(),
        produced,
    });
    let session = Arc::new(HostSession::with_backend(
        StdRuntime::shared(),
        guest.clone(),
        pool.clone(),
        &backend,
    ));
    let responder = {
        let guest = guest.clone();
        let session = session.clone();
        let seed = rng.next_u64();
        let buffers_from: Vec<u64> = (1..=SURFACES)
            .filter(|&id| {
                pool.get(id).is_some_and(|surface| {
                    PixelFormat::from_fourcc(surface.layout.pixel_format).is_some()
                })
            })
            .collect();
        std::thread::Builder::new()
            .name("fuzz-guest".to_owned())
            .spawn(move || respond(&guest, &session, seed, &buffers_from))
    };
    assert!(responder.is_ok(), "the guest's thread does not start");
    let decodable = backend
        .decoders
        .iter()
        .flat_map(|decoders| decoders.capabilities())
        .map(|offered| offered.codec.fourcc())
        .collect();
    let encodable = backend
        .encoders
        .iter()
        .flat_map(|encoders| encoders.encoders())
        .map(|offered| {
            let formats = offered.source_formats.iter().map(|f| f.fourcc()).collect();
            (offered.codec.fourcc(), formats)
        })
        .collect();
    let steps = script_rng_steps(rng);
    let mut script = Script {
        rng,
        session: &session,
        guest: &guest,
        pool: &pool,
        next_reply: 1000,
        next_frame: 0,
        waited: Duration::ZERO,
        codecs: HashMap::new(),
        sessions: Sessions::default(),
        decodable,
        encodable,
        sources: HashMap::new(),
        started: HashSet::new(),
        frames: HashMap::new(),
    };
    script.setup();
    for _ in 0..steps {
        script.step();
    }
    let ending = script.rng.below(4);
    if ending == 0 {
        // A guest that finishes its frames before it goes.
        for uuid in script.sessions.all() {
            let mut m = match script.sessions.routed(uuid) {
                Some((Kind::Decoder, _)) => script.request(b"dfin", uuid),
                _ => {
                    let mut m = script.request(b"ecfr", uuid);
                    m.time(PRESENTATION_TIME, MediaTime::INVALID);
                    m
                }
            };
            m.change_flags(MessageFlags::GUEST_EXPECTS_REPLY, MessageFlags(u32::MAX));
            script.send((vec![m.finish()], true));
        }
    }
    let late = (ending == 1).then(|| message::any(script.rng));
    guest.close();
    if let Ok(responder) = responder {
        // A panic on it is the input's failure, which the runner reports.
        let _ = responder.join();
    }
    if let Some(late) = late {
        // A device reset, then a message the device delivered after it.
        session.invalidate();
        session.deliver(&late);
        session.complete_invalidate();
    }
    drop(session);
    assert_eq!(
        pool.holds(),
        0,
        "surfaces still held after the host session was dropped"
    );
}
