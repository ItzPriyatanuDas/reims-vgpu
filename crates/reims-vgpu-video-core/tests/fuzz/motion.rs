//! The host session's motion-estimation sessions under seeded fuzzing.
//!
//! A stand-in guest opens motion-estimation sessions with specifications
//! of any kind, starts them at any size, sends `mepf`s lending any of its
//! surfaces under any UUIDs, sets and copies properties of every key with
//! values of every type, invalidates and closes them, and sends messages of
//! anything, now and then mutated. A thread of its own answers what the
//! host asks (`Mspa`, `Mvpb`, `Mbuf`, `Memt`), mostly as a guest does, and
//! now and then with an error, mangled, with the wrong surfaces, to another
//! request or not at all. The estimator is a stand-in whose fields come
//! from a hash of what it is asked: on blocks of any size, with as many
//! vectors as it likes and any values, as a backend might give.
//!
//! Each input is checked for:
//! - no panic on any thread, within its time and memory bounds;
//! - every message to the guest within the guest's 0x4000-byte buffer,
//!   and readable;
//! - every `Memt`'s buffer, and every UUID an `Mdpb` names, one the guest
//!   gave;
//! - no surface held once the session is dropped.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::ThreadId;
use std::time::Duration;

use reims_vgpu_video_core::backend::{Backend, BackendError, PixelFormat};
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::host::{GUEST_BUFFER_SIZE, GuestSink, HostSession};
use reims_vgpu_video_core::motion::{
    MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator, MotionEstimatorConfig,
    MotionField, MotionPicture, MotionVector,
};
use reims_vgpu_video_core::plist::{self, Value};
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::{
    GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
};
use reims_vgpu_video_core::sync::Mutex;
use reims_vgpu_video_core::wire::message::{
    GuestUuid, Message, MessageFlags, MessageHeader, MessageWriter, ValueType, read_header,
};
use reims_vgpu_video_core::wire::surface_boxes::read_surface_release;

use crate::generate::{message, plist as plists};
use crate::support::{Rng, Target, long, maybe_mutate, mix, quick};

/// One input: a host session, its stand-in estimator and a guest's
/// sequence. Counted in the whole process, for the session's threads.
const MOTION: Target = Target::new("motion-sessions").process();

#[test]
fn motion_sessions() {
    let report = quick(&MOTION, 60, motion_case);
    print_depth(report.inputs);
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn motion_sessions_long() {
    let report = long(&MOTION, motion_case);
    print_depth(report.inputs);
}

/// How deep the inputs got: what came back to the guest.
static TO_GUEST: AtomicU64 = AtomicU64::new(0);
static EMITTED_WITH_VECTORS: AtomicU64 = AtomicU64::new(0);
static EMITTED_WITH_ERRORS: AtomicU64 = AtomicU64::new(0);

fn print_depth(inputs: u64) {
    eprintln!(
        "motion: {} messages to the guest, {} frames emitted with vectors and {} with an \
         error, in {inputs} inputs",
        TO_GUEST.load(Ordering::Relaxed),
        EMITTED_WITH_VECTORS.load(Ordering::Relaxed),
        EMITTED_WITH_ERRORS.load(Ordering::Relaxed),
    );
}

const fn key(name: &[u8; 4]) -> FourCc {
    FourCc::new(*name)
}

const VERSION: FourCc = key(b",ver");
const ERROR: FourCc = key(b",err");
const OPTIONS: FourCc = key(b",meo");
const GUEST_INFO: FourCc = key(b",gin");
const DIMENSIONS: FourCc = key(b",vdi");
const REQUEST: FourCc = key(b",rid");
const UUIDS: FourCc = key(b",ui+");
const UUID: FourCc = key(b",uui");
const FRAME_FLAGS: FourCc = key(b",mef");
const PROPERTY_KEY: FourCc = key(b",key");
const PROPERTY_VALUE: FourCc = key(b",val");
const PROPERTIES: FourCc = key(b",pro");

/// How many surfaces the guest has, with mapping IDs from 1.
const SURFACES: u64 = 8;
/// The most memory one surface has: a layout may claim more.
const MOST_SURFACE_BYTES: u64 = 1 << 20;
/// The most vectors the stand-in gives, whatever size it says its field is.
const MOST_VECTORS: u64 = 4096;
/// How long the guest waits for one reply, and for all of an input's: the
/// host's comes at once or never.
const REPLY_WAIT: Duration = Duration::from_millis(10);
const REPLY_WAITS: Duration = Duration::from_millis(50);

const OUR_ID: &str = "reims-vgpu-video.motionestimationprocessor";
const PROPERTY_NAMES: [&str; 11] = [
    "MotionVectorSize",
    "UseMultiPassSearch",
    "TrueMotion",
    "IncludeMotionConfidence",
    "PropertyMode",
    "ClientPID",
    "VTIsHardwareAccelerated",
    "Label",
    "Paravirtualized",
    "HostProcessorID",
    "CMClassImplementationName",
];

fn hash(seed: u64, bytes: &[u8]) -> u64 {
    mix(bytes.iter().fold(seed ^ 0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    }))
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A backend failure of each kind, by `h`.
fn failure(h: u64) -> BackendError {
    let message = "the stand-in fails".to_owned();
    match (h >> 20) % 4 {
        0 => BackendError::Unsupported(message),
        1 => BackendError::BadData(message),
        2 => BackendError::Malfunction(message),
        _ => BackendError::Native {
            status: [-12902, -17691, -1, 0, 1, i32::MIN][to_usize((h >> 24) % 6)],
            message,
        },
    }
}

/// The stand-in backend: formats and sizes of any kind.
struct StandIn {
    seed: u64,
    capabilities: MotionEstimationCapabilities,
}

impl StandIn {
    fn new(rng: &mut Rng) -> Self {
        let formats = [
            PixelFormat::Nv12VideoRange,
            PixelFormat::Nv12FullRange,
            PixelFormat::P010VideoRange,
        ];
        // Most often both of the formats the pool's sources are in.
        let source_formats = (formats.into_iter()).filter(|_| !rng.one_in(8)).collect();
        let seed = rng.next_u64();
        let mut side = || {
            if rng.one_in(10) {
                *rng.pick(&[0, 2, 64, 640])
            } else {
                *rng.pick(&[4096, 16384, u32::MAX])
            }
        };
        Self {
            seed,
            capabilities: MotionEstimationCapabilities {
                source_formats,
                max_width: side(),
                max_height: side(),
            },
        }
    }
}

impl MotionEstimationBackend for StandIn {
    fn capabilities(&self) -> MotionEstimationCapabilities {
        self.capabilities.clone()
    }

    fn open(
        &self,
        config: &MotionEstimatorConfig,
    ) -> Result<Box<dyn MotionEstimator>, BackendError> {
        let h = hash(
            self.seed,
            &[config.width.to_be_bytes(), config.height.to_be_bytes()].concat(),
        );
        if h.is_multiple_of(12) {
            return Err(failure(h));
        }
        Ok(Box::new(StandInEstimator {
            seed: h,
            width: config.width,
            height: config.height,
            frames: 0,
        }))
    }
}

struct StandInEstimator {
    seed: u64,
    width: u32,
    height: u32,
    frames: u64,
}

impl MotionEstimator for StandInEstimator {
    fn estimate(
        &mut self,
        reference: &MotionPicture<'_>,
        current: &MotionPicture<'_>,
    ) -> Result<MotionField, BackendError> {
        self.frames += 1;
        let first = |picture: &MotionPicture<'_>| {
            (picture.planes.first())
                .map(|plane| plane.bytes.iter().take(16).copied().collect::<Vec<u8>>())
                .unwrap_or_default()
        };
        let h = hash(
            self.seed ^ self.frames,
            &[first(reference), first(current)].concat(),
        );
        if h.is_multiple_of(12) {
            return Err(failure(h));
        }
        let block = [0, 1, 3, 4, 8, 16, 32, 1 << 20, u32::MAX][to_usize((h >> 8) % 9)];
        let (columns, rows) = match (h >> 16) % 4 {
            0 => (u32::MAX, u32::MAX),
            1 => (((h >> 24) % 9) as u32, ((h >> 28) % 9) as u32),
            _ => (
                self.width.div_ceil(block.max(1)),
                self.height.div_ceil(block.max(1)),
            ),
        };
        let wanted = u64::from(columns) * u64::from(rows);
        let count = match (h >> 32) % 4 {
            0 => (h >> 36) % 64,
            _ => wanted,
        }
        .min(MOST_VECTORS);
        let values = [
            0.0,
            -8.0,
            3.5,
            -0.25,
            65504.0,
            1e30,
            -1e-30,
            f32::NAN,
            f32::INFINITY,
        ];
        let vectors = (0..count)
            .map(|n| {
                let v = mix(h ^ n);
                MotionVector {
                    x: values[to_usize(v % 9)],
                    y: values[to_usize((v >> 8) % 9)],
                }
            })
            .collect();
        Ok(MotionField {
            block_size: block,
            columns,
            rows,
            vectors,
        })
    }
}

/// What a surface of the pool is most often.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A source frame of the session's size.
    Source,
    /// A vector buffer of the session's grid.
    Vectors,
    /// Anything.
    Any,
}

/// The pool: surfaces 1 to 3 sources, 4 and 5 vector buffers, the rest
/// anything.
fn kind_of(mapping_id: u64) -> Kind {
    match mapping_id {
        1..=3 => Kind::Source,
        4 | 5 => Kind::Vectors,
        _ => Kind::Any,
    }
}

/// One guest surface: its layout, and memory that need not be as long as
/// the layout says.
struct Surface {
    layout: SurfaceLayout,
    memory: Mutex<Vec<u8>>,
    holdable: bool,
    reachable: bool,
}

impl Surface {
    /// A source frame, a vector buffer, or either, most often one a
    /// guest's CoreVideo makes, of the session's size or another.
    fn new(rng: &mut Rng, size: (u32, u32), kind: Kind) -> Self {
        let format = match kind {
            Kind::Source if !rng.one_in(8) => *rng.pick(&[*b"420v", *b"420f"]),
            Kind::Vectors if !rng.one_in(8) => *b"2C0h",
            _ => *rng.pick(&[
                *b"420v", *b"420f", *b"420v", *b"x420", *b"2C0h", *b"2C0h", *b"BGRA",
            ]),
        };
        let format = if rng.one_in(16) {
            rng.next_u32().to_be_bytes()
        } else {
            format
        };
        let odd = if kind == Kind::Any { 3 } else { 24 };
        let side = |rng: &mut Rng, side: u32| match rng.below(odd) {
            0 => *rng.pick(&[0, 1, 2, 15, 17, 65, 1 << 15, u32::MAX]),
            1 => rng.range_u32(1, 200),
            _ => side,
        };
        let (width, height) = if &format == b"2C0h" {
            let grid = (size.0.div_ceil(16).max(10), size.1.div_ceil(16).max(4));
            let scale = if rng.bool() { 1 } else { 4 };
            (side(rng, grid.0 * scale), side(rng, grid.1 * scale))
        } else {
            (side(rng, size.0), side(rng, size.1))
        };
        let (w, h) = (u64::from(width), u64::from(height));
        let shapes: Vec<(u64, u64, u32)> = match &format {
            b"420v" | b"420f" => vec![
                (w, h, width),
                (w.div_ceil(2) * 2, h.div_ceil(2), width.div_ceil(2)),
            ],
            b"x420" => vec![
                (w * 2, h, width),
                (w.div_ceil(2) * 4, h.div_ceil(2), width.div_ceil(2)),
            ],
            b"2C0h" | b"BGRA" => vec![(w * 4, h, width)],
            _ => vec![(w, h, width)],
        };
        let align = *rng.pick(&[1, 16, 64]);
        // How often a layout is broken: most of the pool's are not.
        let broken = if kind == Kind::Any { 8 } else { 48 };
        let mut planes = Vec::new();
        let mut offset = 0u64;
        for (row, rows, samples) in shapes {
            let mut bytes_per_row = row.div_ceil(align).saturating_mul(align);
            if rng.one_in(broken) {
                bytes_per_row = bytes_per_row.saturating_sub(rng.range(1, 8));
            }
            planes.push(PlaneLayout {
                offset: to_usize(offset),
                bytes_per_row: to_usize(bytes_per_row),
                width: samples,
                height: u32::try_from(rows).unwrap_or(u32::MAX),
            });
            offset = offset.saturating_add(bytes_per_row.saturating_mul(rows));
        }
        let mut length = offset;
        if rng.one_in(broken) {
            length = length.saturating_sub(rng.range(1, 100));
        }
        if length > MOST_SURFACE_BYTES {
            length = rng.range(0, 4096);
        }
        let (planes, bytes_per_row) = match planes.as_slice() {
            [only] if rng.bool() => (Vec::new(), only.bytes_per_row),
            _ => (planes, 0),
        };
        Self {
            layout: SurfaceLayout {
                pixel_format: FourCc::new(format),
                width,
                height,
                planes,
                bytes_per_row,
            },
            memory: Mutex::new({
                let pattern = rng.bytes(64);
                (0..to_usize(length)).map(|i| pattern[i % 64]).collect()
            }),
            holdable: kind != Kind::Any || !rng.one_in(4),
            reachable: !rng.one_in(if kind == Kind::Any { 4 } else { 32 }),
        }
    }
}

/// The guest's surfaces, and how many holds the host has.
struct Pool {
    surfaces: Vec<Arc<Surface>>,
    holds: Arc<AtomicUsize>,
    lending: Arc<Lending>,
}

struct Held {
    surface: Arc<Surface>,
    holds: Arc<AtomicUsize>,
    lending: Arc<Lending>,
}

/// One lend at a time across the pool, as reims-vgpu's mapper lends
/// surfaces (one process-wide lock): a lend on another thread waits for
/// the one in hand, and a lend inside another on the same thread, which
/// would wait there for ever, is refused and counted, and an input with one
/// fails. So is a wait past [`LEND_WAIT`], which only a thread that waits
/// for something inside its lend makes.
#[derive(Default)]
struct Lending {
    turn: Mutex<()>,
    lender: Mutex<Option<ThreadId>>,
    nested: AtomicUsize,
}

/// The longest a lend waits for another thread's.
const LEND_WAIT: Duration = Duration::from_secs(10);

impl Lending {
    fn lend(&self, lend: impl FnOnce()) -> Result<(), SurfaceError> {
        let me = std::thread::current().id();
        if *self.lender.lock() == Some(me) {
            self.nested.fetch_add(1, Ordering::SeqCst);
            return Err(SurfaceError::Unreachable(
                "a lend inside another lend".into(),
            ));
        }
        let Some(_turn) = self.turn.try_lock_for(LEND_WAIT) else {
            self.nested.fetch_add(1, Ordering::SeqCst);
            return Err(SurfaceError::Unreachable(
                "a lend that waited past its bound for another".into(),
            ));
        };
        *self.lender.lock() = Some(me);
        lend();
        *self.lender.lock() = None;
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.holds.fetch_sub(1, Ordering::SeqCst);
    }
}

impl GuestSurface for Held {
    fn layout(&self) -> SurfaceLayout {
        self.surface.layout.clone()
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        if !self.surface.reachable {
            return Err(SurfaceError::Unreachable("the stand-in's memory".into()));
        }
        self.lending.lend(|| write(&mut self.surface.memory.lock()))
    }

    fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
        if !self.surface.reachable {
            return Err(SurfaceError::Unreachable("the stand-in's memory".into()));
        }
        self.lending.lend(|| read(&self.surface.memory.lock()))
    }
}

impl Surfaces for Pool {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        let surface = usize::try_from(mapping_id.0)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| self.surfaces.get(index))
            .ok_or(SurfaceError::Unknown(mapping_id.0))?;
        if !surface.holdable {
            return Err(SurfaceError::Unreachable("the stand-in's mapper".into()));
        }
        self.holds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Held {
            surface: Arc::clone(surface),
            holds: Arc::clone(&self.holds),
            lending: Arc::clone(&self.lending),
        }))
    }
}

/// What the guest has given and seen.
#[derive(Default)]
struct Given {
    /// Every UUID the guest named a pixel buffer by, in `mepf` or `Mbuf`.
    uuids: HashSet<[u8; 16]>,
    /// The replies that came back, by reply ID.
    replies: HashSet<u64>,
}

/// The guest's side: what reaches it, checked as it arrives.
struct Guest {
    inbox: Mutex<Option<Sender<Vec<u8>>>>,
    given: Arc<Mutex<Given>>,
}

impl GuestSink for Guest {
    fn send(&self, message: Vec<u8>) {
        assert!(
            message.len() <= GUEST_BUFFER_SIZE,
            "a {}-byte message to the guest",
            message.len()
        );
        if let Some(inbox) = self.inbox.lock().as_ref() {
            let _ = inbox.send(message);
        }
    }
}

/// The guest's thread: checks each message and answers the host's
/// requests.
fn respond(inbox: &Receiver<Vec<u8>>, session: &HostSession, given: &Mutex<Given>, seed: u64) {
    let mut rng = Rng::new(seed);
    while let Ok(bytes) = inbox.recv() {
        TO_GUEST.fetch_add(1, Ordering::Relaxed);
        if read_surface_release(&bytes).is_some() {
            continue;
        }
        let header = read_header(&bytes, 0);
        assert!(header.is_ok(), "a message to the guest that does not read");
        let Ok(header) = header else { continue };
        let parsed = Message::parse(&bytes);
        assert!(parsed.is_ok(), "a message to the guest that does not parse");
        let Ok(message) = parsed else { continue };
        if header.flags.contains(MessageFlags::REPLY_TO_GUEST) {
            given.lock().replies.insert(header.reply_id);
            continue;
        }
        match &header.message_type.0.to_be_bytes() {
            b"Memt" => {
                match message.s32(ERROR) {
                    Ok(Some(0)) => EMITTED_WITH_VECTORS.fetch_add(1, Ordering::Relaxed),
                    _ => EMITTED_WITH_ERRORS.fetch_add(1, Ordering::Relaxed),
                };
                if let Ok(Some(uuid)) = message.uuid(UUID) {
                    assert!(
                        given.lock().uuids.contains(&uuid.0),
                        "a Memt of buffer {uuid:?}, which the guest never gave"
                    );
                }
            }
            b"Mdpb" => {
                assert!(
                    !header.flags.contains(MessageFlags::HOST_EXPECTS_REPLY),
                    "an Mdpb that waits"
                );
                for uuid in message.uuid_array(UUIDS).ok().flatten().unwrap_or_default() {
                    assert!(
                        given.lock().uuids.contains(&uuid.0),
                        "an Mdpb of {uuid:?}, which the guest never gave"
                    );
                }
                continue;
            }
            _ => {}
        }
        if header.flags.contains(MessageFlags::HOST_EXPECTS_REPLY) {
            for reply in answer(&mut rng, &header, given) {
                session.deliver(&reply);
            }
        }
    }
}

/// The guest's answer to one of the host's requests: mostly as a guest
/// does, now and then with an error, wrong surfaces, to another request,
/// mangled, twice, or none.
fn answer(rng: &mut Rng, request: &MessageHeader, given: &Mutex<Given>) -> Vec<Vec<u8>> {
    let choice = rng.below(64);
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
    let target = if choice == 4 {
        MessageHeader {
            reply_id: request.reply_id.wrapping_add(rng.range(1, 3)),
            ..*request
        }
    } else {
        *request
    };
    let mut reply = MessageWriter::reply_to(&target, MessageFlags::REPLY_TO_HOST);
    reply.s32(ERROR, status);
    let mut surfaces = Vec::new();
    if request.message_type == key(b"Mbuf") {
        if !rng.one_in(12) {
            let uuid = GuestUuid([*rng.pick(&[0xb1, 0xb2, 0xb3, 0x51]); 16]);
            given.lock().uuids.insert(uuid.0);
            reply.uuid(UUID, uuid);
        }
        surfaces = match rng.below(10) {
            0 => Vec::new(),
            1 => vec![rng.range(1, SURFACES), rng.range(1, SURFACES)],
            2 => vec![rng.interesting_u64()],
            3 => vec![rng.range(1, SURFACES)],
            _ => vec![rng.range(4, 5)],
        };
    }
    let mut bytes = reply.finish();
    if choice == 5 {
        let donor = bytes.clone();
        maybe_mutate(rng, &mut bytes, &donor, 1);
    }
    if !surfaces.is_empty() {
        bytes = message::with_surfaces(&surfaces, bytes);
    }
    note_given(given, &bytes);
    if choice == 6 {
        vec![bytes.clone(), bytes]
    } else {
        vec![bytes]
    }
}

/// Notes the UUIDs a message of the guest's names, as the host reads them:
/// what it sends mutated or of anything included.
fn note_given(given: &Mutex<Given>, bytes: &[u8]) {
    let Ok(message) = Message::parse(bytes) else {
        return;
    };
    let mut given = given.lock();
    if let Ok(Some(uuid)) = message.uuid(UUID) {
        given.uuids.insert(uuid.0);
    }
    for uuid in message.uuid_array(UUIDS).ok().flatten().unwrap_or_default() {
        given.uuids.insert(uuid.0);
    }
}

/// The guest's side of one input.
struct Script<'a> {
    rng: &'a mut Rng,
    session: &'a HostSession,
    given: &'a Mutex<Given>,
    next_reply: u64,
    waited: Duration,
    sessions: Vec<GuestUuid>,
    /// The size of the pool's source frames.
    frame_size: (u32, u32),
}

impl Script<'_> {
    fn uuid(&mut self) -> GuestUuid {
        if !self.sessions.is_empty() && !self.rng.one_in(6) {
            return *self.rng.pick(&self.sessions);
        }
        match self.rng.below(12) {
            0 => GuestUuid::ZERO,
            1 => message::uuid(self.rng),
            _ => GuestUuid([0x61 + self.rng.below(2) as u8; 16]),
        }
    }

    fn request(&mut self, kind: &[u8; 4], uuid: Option<GuestUuid>) -> MessageWriter {
        self.next_reply += 1;
        let flags = if self.rng.one_in(16) {
            message::flags(self.rng)
        } else {
            MessageFlags::GUEST_EXPECTS_REPLY
        };
        MessageWriter::new(key(kind), flags, uuid, self.next_reply)
    }

    fn plist(&mut self, writer: &mut MessageWriter, key: FourCc, value: &Value) {
        // A value plist cannot encode goes as NULL.
        if writer.property_list(key, Some(value)).is_err() {
            let _ = writer.property_list(key, None);
        }
    }

    fn property_value(&mut self) -> Value {
        match self.rng.below(6) {
            0 => Value::Boolean(self.rng.bool()),
            1 => Value::Integer(i64::from(*self.rng.pick(&[4, 16, 8, 0, -4, 1 << 20])).into()),
            2 => Value::Integer(i64::from(self.rng.interesting_i32()).into()),
            3 => Value::String((*self.rng.pick(&PROPERTY_NAMES)).to_owned()),
            _ => plists::value(self.rng, 2),
        }
    }

    fn specification(&mut self) -> Value {
        let mut specification = plist::Dictionary::new();
        for name in PROPERTY_NAMES {
            if self.rng.one_in(3) {
                let value = match name {
                    "HostProcessorID" if !self.rng.one_in(4) => Value::String(OUR_ID.to_owned()),
                    _ => self.property_value(),
                };
                specification.insert(name.to_owned(), value);
            }
        }
        Value::Dictionary(specification)
    }

    fn size(&mut self) -> (i32, i32) {
        if self.rng.bool() {
            let side = |side: u32| i32::try_from(side).unwrap_or(i32::MAX);
            return (side(self.frame_size.0), side(self.frame_size.1));
        }
        let mut side = || {
            *self.rng.pick(&[
                0,
                1,
                2,
                16,
                17,
                64,
                65,
                200,
                1080,
                1920,
                8192,
                16384,
                16385,
                -1,
                i32::MAX,
            ])
        };
        (side(), side())
    }

    /// One message, and whether the guest waits for a reply.
    fn step(&mut self) {
        let uuid = self.uuid();
        let mut surfaces = Vec::new();
        let m = match self.rng.below(100) {
            0..4 => {
                let mut m = self.request(b"meps", None);
                m.u32(VERSION, *self.rng.pick(&[6, 6, 5, 0, u32::MAX]));
                m
            }
            4..14 => {
                let mut m = self.request(b"+mep", Some(uuid));
                if !self.rng.one_in(10) {
                    m.u32(VERSION, *self.rng.pick(&[6, 5, 0]));
                }
                if !self.rng.one_in(10) {
                    let specification = self.specification();
                    self.plist(&mut m, OPTIONS, &specification);
                }
                if self.rng.one_in(4) {
                    let info = Value::Dictionary(plists::dictionary(self.rng, 2));
                    self.plist(&mut m, GUEST_INFO, &info);
                }
                if !self.sessions.contains(&uuid) {
                    self.sessions.push(uuid);
                }
                m
            }
            14..18 => {
                let kind = *self.rng.pick(&[*b"-mep", *b"-mep", *b"-uui", *b"-all"]);
                if kind == *b"-mep" {
                    self.sessions.retain(|open| *open != uuid);
                }
                self.request(&kind, Some(uuid))
            }
            18..30 => {
                let mut m = self.request(b"mest", Some(uuid));
                if !self.rng.one_in(10) {
                    let size = self.size();
                    m.i32_pair(DIMENSIONS, ValueType::Dimensions, size);
                }
                m
            }
            30..70 => {
                // Most often to the session the input started.
                let uuid = if self.rng.one_in(4) {
                    uuid
                } else {
                    GuestUuid([0x61; 16])
                };
                let mut m = self.request(b"mepf", Some(uuid));
                let count = match self.rng.below(24) {
                    0 => 0,
                    1 => 1,
                    2 => 3,
                    _ => 2,
                };
                surfaces = (0..count)
                    .map(|_| match self.rng.below(32) {
                        0 => self.rng.interesting_u64(),
                        1..4 => self.rng.range(1, SURFACES),
                        _ => self.rng.range(1, 3),
                    })
                    .collect();
                if !self.rng.one_in(10) {
                    m.s64(REQUEST, self.rng.interesting_u64().cast_signed());
                }
                // Most often each surface's own UUID, as a guest names its
                // pixel buffers.
                let uuids: Vec<GuestUuid> = (0..match self.rng.below(10) {
                    0 => 1,
                    1 => 3,
                    _ => 2,
                })
                    .map(|n| match (self.rng.below(8), surfaces.get(n)) {
                        (1.., Some(&id)) => GuestUuid([0xa0 | (id as u8 & 0xf); 16]),
                        _ => GuestUuid([*self.rng.pick(&[0xa1, 0xa2, 0xa3, 0xb1, 0x00]); 16]),
                    })
                    .collect();
                for uuid in &uuids {
                    self.given.lock().uuids.insert(uuid.0);
                }
                if !self.rng.one_in(12) {
                    m.uuid_array(UUIDS, &uuids);
                }
                m.u32(FRAME_FLAGS, *self.rng.pick(&[0, 0, 0, 1, 2, u32::MAX]));
                if self.rng.one_in(4) {
                    let options = Value::Dictionary(plists::dictionary(self.rng, 2));
                    self.plist(&mut m, OPTIONS, &options);
                }
                m
            }
            70..85 => {
                let kind = *self.rng.pick(&[*b"mcop", *b"mset", *b"mmul"]);
                let mut m = self.request(&kind, Some(uuid));
                if kind == *b"mmul" {
                    let mut properties = plist::Dictionary::new();
                    for _ in 0..self.rng.count(4) {
                        let name = (*self.rng.pick(&PROPERTY_NAMES)).to_owned();
                        properties.insert(name, self.property_value());
                    }
                    if !self.rng.one_in(6) {
                        self.plist(&mut m, PROPERTIES, &Value::Dictionary(properties));
                    }
                } else {
                    let name = if self.rng.one_in(10) {
                        self.property_value()
                    } else {
                        Value::String((*self.rng.pick(&PROPERTY_NAMES)).to_owned())
                    };
                    self.plist(&mut m, PROPERTY_KEY, &name);
                    if kind == *b"mset" && !self.rng.one_in(8) {
                        let value = self.property_value();
                        self.plist(&mut m, PROPERTY_VALUE, &value);
                    }
                }
                m
            }
            85..95 => {
                // `minv` for good, now and then.
                let kind = if self.rng.one_in(8) {
                    *b"minv"
                } else {
                    *self.rng.pick(&[*b"mcfr", *b"msav", *b"mesu", *b"mxyz"])
                };
                self.request(&kind, Some(uuid))
            }
            _ => {
                let mut bytes = message::any(self.rng);
                if self.rng.bool() && !self.sessions.is_empty() {
                    // Aimed at a session.
                    if let Some(field) = bytes.get_mut(16..32) {
                        field.copy_from_slice(&uuid.0);
                    }
                }
                note_given(self.given, &bytes);
                self.session.deliver(&bytes);
                return;
            }
        };
        let reply_id = self.next_reply;
        let waits = m.finish();
        let mut bytes = if surfaces.is_empty() {
            waits
        } else {
            message::with_surfaces(&surfaces, waits)
        };
        if self.rng.one_in(24) {
            let donor = bytes.clone();
            maybe_mutate(self.rng, &mut bytes, &donor, 1);
        }
        note_given(self.given, &bytes);
        self.session.deliver(&bytes);
        // A guest waits for its reply; this one, a moment, and a few
        // moments an input.
        let start = StdRuntime.now();
        while StdRuntime.elapsed_since(start) < REPLY_WAIT
            && self.waited + StdRuntime.elapsed_since(start) < REPLY_WAITS
            && !self.given.lock().replies.contains(&reply_id)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.waited += StdRuntime.elapsed_since(start);
    }
}

fn motion_case(rng: &mut Rng) {
    let size = (*rng.pick(&[64, 65, 200, 17]), *rng.pick(&[48, 33, 120, 16]));
    let pool = Arc::new(Pool {
        surfaces: (1..=SURFACES)
            .map(|id| Arc::new(Surface::new(rng, size, kind_of(id))))
            .collect(),
        holds: Arc::new(AtomicUsize::new(0)),
        lending: Arc::default(),
    });
    let backend = Backend {
        motion: (!rng.one_in(24))
            .then(|| Arc::new(StandIn::new(rng)) as Arc<dyn MotionEstimationBackend>),
        ..Backend::none("fuzz")
    };
    let given = Arc::new(Mutex::new(Given::default()));
    let (to_guest, inbox) = mpsc::channel();
    let guest = Arc::new(Guest {
        inbox: Mutex::new(Some(to_guest)),
        given: given.clone(),
    });
    let session = Arc::new(HostSession::with_backend(
        StdRuntime::shared(),
        guest.clone(),
        pool.clone(),
        &backend,
    ));
    let responder = {
        let session = session.clone();
        let given = guest.given.clone();
        let seed = rng.next_u64();
        std::thread::Builder::new()
            .name("fuzz-motion-guest".to_owned())
            .spawn(move || respond(&inbox, &session, &given, seed))
    };
    assert!(responder.is_ok(), "the guest's thread does not start");
    let steps = rng.range(4, 48);
    let mut script = Script {
        rng,
        session: &session,
        given: &given,
        next_reply: 1000,
        waited: Duration::ZERO,
        sessions: Vec::new(),
        frame_size: size,
    };
    // Most inputs open a session and start it first.
    if !script.rng.one_in(6) {
        let uuid = GuestUuid([0x61; 16]);
        let mut m = script.request(b"+mep", Some(uuid));
        m.u32(VERSION, 6);
        let _ = m.property_list(
            OPTIONS,
            Some(&Value::Dictionary(
                [(
                    "HostProcessorID".to_owned(),
                    Value::String(OUR_ID.to_owned()),
                )]
                .into_iter()
                .collect(),
            )),
        );
        session.deliver(&m.finish());
        script.sessions.push(uuid);
        let mut m = script.request(b"mest", Some(uuid));
        let side = |side: u32| i32::try_from(side).unwrap_or(i32::MAX);
        m.i32_pair(
            DIMENSIONS,
            ValueType::Dimensions,
            (side(size.0), side(size.1)),
        );
        session.deliver(&m.finish());
    }
    for _ in 0..steps {
        script.step();
    }
    let ending = script.rng.below(3);
    let late = (ending == 0).then(|| message::any(script.rng));
    if ending == 1 {
        // A guest that closes what it opened.
        for uuid in script.sessions.clone() {
            let m = script.request(b"-mep", Some(uuid));
            session.deliver(&m.finish());
        }
    }
    session.invalidate();
    if let Some(late) = late {
        // A message the device delivered after its reset.
        session.deliver(&late);
    }
    session.complete_invalidate();
    guest.inbox.lock().take();
    if let Ok(responder) = responder {
        // A panic on it is the input's failure, which the runner reports.
        let _ = responder.join();
    }
    drop(session);
    assert_eq!(
        pool.holds.load(Ordering::SeqCst),
        0,
        "surfaces still held after the host session was dropped"
    );
    assert_eq!(
        pool.lending.nested.load(Ordering::SeqCst),
        0,
        "a surface lent inside another's lend, or after a wait past its bound"
    );
}
