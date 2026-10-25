//! Motion-estimation sessions end to end: a guest processor that sends what the guest's
//! does, a device thread that delivers queue 0 in order as the device does, and a
//! stand-in estimator whose vectors say what it read of each frame.
//!
//! The guest sends what its processor sends: `+mep` with the session's
//! specification, `mest`, `mepf` with its two surfaces, the property calls,
//! `minv` and `-mep`. It answers `Mspa` and `Mvpb`, makes a `2C0h` buffer
//! of `Mvpb`'s size for each `Mbuf`, reads the vectors of each `Memt` and
//! forgets what `Mdpb` names.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::backend::{Backend, BackendError, PixelFormat};
use crate::fourcc::FourCc;
use crate::host::test_queues::{CountingSink, Progress};
use crate::host::{HostSession, names};
use crate::motion::{
    MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator, MotionEstimatorConfig,
    MotionField, MotionPicture, MotionVector, from_half_float,
};
use crate::runtime::StdRuntime;
use crate::status::{NO_ERROR, Status};
use crate::surfaces::{
    GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
};
use crate::sync::Mutex;
use crate::wire::WireError;
use crate::wire::message::{
    GuestUuid, Message, MessageFlags, MessageHeader, MessageWriter, ValueType, read_header,
};
use crate::wire::property_list::as_i64;
use crate::wire::surface_boxes::read_surface_release;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const BYTES_PER_ROW: usize = 80;
const CHROMA_OFFSET: usize = 4096;
/// A frame whose first luma sample is this does not estimate.
const BROKEN: u8 = 0xff;
const WAIT: Duration = Duration::from_secs(5);
const NO_REPLY: Duration = Duration::from_millis(300);
const OUR_ID: &str = "reims-vgpu-video.motionestimationprocessor";

/// The stand-in estimator's backend: `420v` and `420f` up to 4096x4096.
/// Its field is on 8x8 blocks; the vector of block (c, r) is the first luma
/// sample of the reference less the current's, across, and c/2 + 10 r/2
/// down, so that each 16x16 block of Apple's grid has one vector four
/// times over.
#[derive(Default)]
struct StandIn {
    opened: Mutex<Vec<MotionEstimatorConfig>>,
}

impl MotionEstimationBackend for StandIn {
    fn capabilities(&self) -> MotionEstimationCapabilities {
        MotionEstimationCapabilities {
            source_formats: vec![PixelFormat::Nv12VideoRange, PixelFormat::Nv12FullRange],
            max_width: 4096,
            max_height: 4096,
        }
    }

    fn open(
        &self,
        config: &MotionEstimatorConfig,
    ) -> Result<Box<dyn MotionEstimator>, BackendError> {
        self.opened.lock().push(*config);
        Ok(Box::new(StandInEstimator {
            width: config.width,
            height: config.height,
        }))
    }
}

struct StandInEstimator {
    width: u32,
    height: u32,
}

impl MotionEstimator for StandInEstimator {
    fn estimate(
        &mut self,
        reference: &MotionPicture<'_>,
        current: &MotionPicture<'_>,
    ) -> Result<MotionField, BackendError> {
        let first = |picture: &MotionPicture<'_>| {
            picture
                .planes
                .first()
                .and_then(|luma| luma.bytes.first())
                .copied()
                .unwrap_or(0)
        };
        let (from, to) = (first(reference), first(current));
        if to == BROKEN {
            return Err(BackendError::BadData("a broken picture".into()));
        }
        let (columns, rows) = (self.width.div_ceil(8), self.height.div_ceil(8));
        let mut vectors = Vec::new();
        for row in 0..rows {
            for column in 0..columns {
                vectors.push(MotionVector {
                    x: f32::from(from) - f32::from(to),
                    y: f32::from(u16::try_from(column / 2 + 10 * (row / 2)).unwrap_or(0)),
                });
            }
        }
        Ok(MotionField {
            block_size: 8,
            columns,
            rows,
            vectors,
        })
    }
}

/// What runs when one of the host's holds on a surface ends.
type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

/// The guest's IOSurfaces, by mapping ID, and the holds the host has.
#[derive(Default)]
struct GuestMemory {
    surfaces: Mutex<HashMap<u64, Arc<SurfaceMemory>>>,
    holds: Arc<Mutex<HashMap<u64, usize>>>,
    on_release: Arc<Mutex<Option<ReleaseHook>>>,
    lending: Arc<Lending>,
}

/// One lend at a time across every surface, as reims-vgpu's mapper lends
/// them (its `lend_surface_bytes` holds one process-wide lock): a lend
/// inside another is refused here, and counted, where reims-vgpu's would
/// wait on itself for ever.
#[derive(Default)]
struct Lending {
    busy: AtomicBool,
    nested: AtomicUsize,
}

impl Lending {
    fn lend(&self, lend: impl FnOnce()) -> Result<(), SurfaceError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            self.nested.fetch_add(1, Ordering::SeqCst);
            return Err(SurfaceError::Unreachable(
                "a lend inside another lend".into(),
            ));
        }
        lend();
        self.busy.store(false, Ordering::Release);
        Ok(())
    }
}

struct SurfaceMemory {
    layout: SurfaceLayout,
    bytes: Mutex<Vec<u8>>,
}

struct HeldSurface {
    mapping_id: u64,
    memory: Arc<SurfaceMemory>,
    holds: Arc<Mutex<HashMap<u64, usize>>>,
    on_release: Arc<Mutex<Option<ReleaseHook>>>,
    lending: Arc<Lending>,
}

impl GuestSurface for HeldSurface {
    fn layout(&self) -> SurfaceLayout {
        self.memory.layout.clone()
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        self.lending.lend(|| write(&mut self.memory.bytes.lock()))
    }

    fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
        self.lending.lend(|| read(&self.memory.bytes.lock()))
    }
}

impl Drop for HeldSurface {
    fn drop(&mut self) {
        if let Some(count) = self.holds.lock().get_mut(&self.mapping_id) {
            *count -= 1;
        }
        let hook = self.on_release.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl Surfaces for GuestMemory {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        let memory = self
            .surfaces
            .lock()
            .get(&mapping_id.0)
            .cloned()
            .ok_or(SurfaceError::Unknown(mapping_id.0))?;
        *self.holds.lock().entry(mapping_id.0).or_default() += 1;
        Ok(Box::new(HeldSurface {
            mapping_id: mapping_id.0,
            memory,
            holds: self.holds.clone(),
            on_release: self.on_release.clone(),
            lending: self.lending.clone(),
        }))
    }
}

impl GuestMemory {
    fn held(&self) -> usize {
        self.holds.lock().values().sum()
    }

    fn insert(&self, mapping_id: u64, surface: SurfaceMemory) {
        self.surfaces.lock().insert(mapping_id, Arc::new(surface));
    }
}

/// A frame's vectors as the guest's processor got them.
#[derive(Clone, Debug, PartialEq)]
struct Emitted {
    request: i64,
    status: i32,
    info: u32,
    uuid: Option<GuestUuid>,
    /// The buffer's vectors, row by row, when it came with one.
    vectors: Option<Vec<Vec<(f32, f32)>>>,
}

/// What the guest's side keeps.
#[derive(Default)]
struct GuestState {
    /// The host's messages in arrival order, as `Mspa`, `Mbuf 7`,
    /// `rels [1, 2]`, `Memt 7`, `Mdpb 2`, `reply mepf`.
    seen: Mutex<Vec<String>>,
    source_attributes: Mutex<Option<plist::Dictionary>>,
    vector_attributes: Mutex<Option<plist::Dictionary>>,
    emitted: Mutex<Vec<Emitted>>,
    dropped: Mutex<Vec<Vec<GuestUuid>>>,
    /// The vector buffers the guest made, by UUID: their mapping IDs.
    buffers: Mutex<HashMap<GuestUuid, u64>>,
    waiting: Mutex<HashMap<u64, Sender<Vec<u8>>>>,
}

/// The guest processor, its handler thread, and the device's queue-0
/// thread.
struct Guest {
    uuid: GuestUuid,
    state: Arc<GuestState>,
    memory: Arc<GuestMemory>,
    backend: Arc<StandIn>,
    session: Arc<HostSession>,
    queue0: Option<Sender<Vec<u8>>>,
    next_reply_id: AtomicU64,
    next_surface: Arc<AtomicU64>,
    /// How far the queues have got.
    progress: Arc<Progress>,
    threads: Vec<JoinHandle<()>>,
}

impl Guest {
    fn new(with_motion: bool) -> Self {
        let memory = Arc::new(GuestMemory::default());
        let backend = Arc::new(StandIn::default());
        let (to_guest, from_host) = mpsc::channel::<Vec<u8>>();
        let progress = Arc::new(Progress::default());
        let parts = Backend {
            motion: with_motion.then(|| backend.clone() as Arc<dyn MotionEstimationBackend>),
            ..Backend::none("motion alone")
        };
        let session = Arc::new(HostSession::with_backend(
            StdRuntime::shared(),
            Arc::new(CountingSink::new(to_guest, progress.clone())),
            memory.clone(),
            &parts,
        ));
        let (queue0, device_queue) = mpsc::channel::<Vec<u8>>();
        let device = {
            let session = session.clone();
            let progress = progress.clone();
            StdRuntime::thread("test-device", move || {
                while let Ok(message) = device_queue.recv() {
                    session.deliver(&message);
                    progress.delivered(&message);
                }
            })
            .expect("a thread")
        };
        let state = Arc::new(GuestState::default());
        let next_surface = Arc::new(AtomicU64::new(1));
        let handler = {
            let state = state.clone();
            let queue0 = queue0.clone();
            let memory = memory.clone();
            let next_surface = next_surface.clone();
            let progress = progress.clone();
            StdRuntime::thread("test-guest", move || {
                while let Ok(message) = from_host.recv() {
                    handle_host_message(&state, &memory, &next_surface, &queue0, &message);
                    progress.handled_one();
                }
            })
            .expect("a thread")
        };
        Self {
            uuid: GuestUuid([0x4d; 16]),
            state,
            memory,
            backend,
            session,
            queue0: Some(queue0),
            next_reply_id: AtomicU64::new(100),
            next_surface,
            progress,
            threads: vec![device, handler],
        }
    }

    fn seen(&self) -> Vec<String> {
        std::mem::take(&mut *self.state.seen.lock())
    }

    fn emitted(&self) -> Vec<Emitted> {
        std::mem::take(&mut *self.state.emitted.lock())
    }

    fn send(&self, message: Vec<u8>) {
        if let Some(queue0) = &self.queue0 {
            let _ = queue0.send(message);
        }
    }

    /// A message of the guest's processor, flag 0x1 and the next reply ID.
    fn message(
        &self,
        message_type: &[u8; 4],
        uuid: Option<GuestUuid>,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> (u64, Vec<u8>) {
        let reply_id = self.next_reply_id.fetch_add(1, Ordering::SeqCst);
        let mut message = MessageWriter::new(
            FourCc::new(*message_type),
            MessageFlags::GUEST_EXPECTS_REPLY,
            uuid,
            reply_id,
        );
        parameters(&mut message);
        (reply_id, message.finish())
    }

    fn call_with(&self, reply_id: u64, message: Vec<u8>, timeout: Duration) -> Option<Vec<u8>> {
        let (arrived, reply) = mpsc::channel();
        self.state.waiting.lock().insert(reply_id, arrived);
        self.send(message);
        let answer = reply.recv_timeout(timeout).ok();
        self.state.waiting.lock().remove(&reply_id);
        answer
    }

    fn call(
        &self,
        message_type: &[u8; 4],
        timeout: Duration,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> Option<Vec<u8>> {
        let (reply_id, message) = self.message(message_type, Some(self.uuid), parameters);
        self.call_with(reply_id, message, timeout)
    }

    /// A call answered with `,err` alone, or None with no reply.
    fn status_of(
        &self,
        message_type: &[u8; 4],
        timeout: Duration,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> Result<Option<i32>, WireError> {
        match self.call(message_type, timeout, parameters) {
            None => Ok(None),
            Some(reply) => Ok(Message::parse(&reply)?.s32(names::ERROR)?),
        }
    }

    /// `meps`: the processors listed.
    fn processors(&self) -> Result<Vec<plist::Value>, WireError> {
        let (reply_id, message) = self.message(b"meps", None, |message| {
            message.u32(names::VERSION, 6);
        });
        let reply = self
            .call_with(reply_id, message, WAIT)
            .ok_or(WireError::Malformed("no meps reply"))?;
        let reply = Message::parse(&reply)?;
        assert_eq!(reply.u32(names::VERSION)?, Some(6));
        match reply.property_list(names::MOTION_ESTIMATORS)? {
            Some(plist::Value::Array(processors)) => Ok(processors),
            _ => Err(WireError::Malformed("no ,mep")),
        }
    }

    /// `+mep` with `specification` as `,meo`: the reply's status, or None
    /// with no reply.
    fn create(&self, specification: &[(&str, plist::Value)]) -> Result<Option<i32>, WireError> {
        let specification: plist::Dictionary = specification
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        let Some(reply) = self.call(b"+mep", NO_REPLY.max(WAIT / 5), |message| {
            message.u32(names::VERSION, 6);
            let _ = message.property_list(
                names::MOTION_OPTIONS,
                Some(&plist::Value::Dictionary(specification)),
            );
            let _ = message.property_list(names::GUEST_INFO, None);
        }) else {
            return Ok(None);
        };
        let reply = Message::parse(&reply)?;
        assert_eq!(reply.u32(names::VERSION)?, Some(6));
        let build = reply
            .dictionary(names::HOST_INFO)?
            .and_then(|info| info.get("VTHostBuildVersion").cloned());
        assert!(matches!(build, Some(plist::Value::String(_))));
        reply.s32(names::ERROR)
    }

    /// `mest` for pictures of `width` by `height`.
    fn start(&self, width: i32, height: i32) -> Result<Option<i32>, WireError> {
        self.status_of(b"mest", WAIT, |message| {
            message.i32_pair(names::DIMENSIONS, ValueType::Dimensions, (width, height));
        })
    }

    /// A new 64x48 `420v` frame whose luma is all `luma`: its mapping ID.
    fn frame(&self, luma: u8) -> u64 {
        let mapping_id = self.next_surface.fetch_add(1, Ordering::SeqCst);
        let surface = nv12_surface(*b"420v", WIDTH, HEIGHT);
        surface.bytes.lock()[..CHROMA_OFFSET].fill(luma);
        self.memory.insert(mapping_id, surface);
        mapping_id
    }

    /// `mepf` of `request`, lending `surfaces` and naming `uuids`, with
    /// `flags`: the reply's status, or None with no reply.
    fn process(
        &self,
        request: i64,
        surfaces: &[u64],
        uuids: &[GuestUuid],
        flags: u32,
    ) -> Result<Option<i32>, WireError> {
        let (reply_id, message) = self.message(b"mepf", Some(self.uuid), |message| {
            message.s64(names::REQUEST, request);
            message.uuid_array(names::PIXEL_BUFFER_UUIDS, uuids);
            message.u32(names::MOTION_FRAME_FLAGS, flags);
            let _ = message.property_list(names::MOTION_OPTIONS, None);
        });
        let message = with_surfaces(surfaces, &message);
        let timeout = if surfaces.len() == 2 && uuids.len() == 2 {
            WAIT
        } else {
            NO_REPLY
        };
        match self.call_with(reply_id, message, timeout) {
            None => Ok(None),
            Some(reply) => Ok(Message::parse(&reply)?.s32(names::ERROR)?),
        }
    }

    /// `mcop`: the reply's status and value.
    fn copy_property(&self, key: &str) -> Result<(i32, Option<plist::Value>), WireError> {
        let reply = self
            .call(b"mcop", WAIT, |message| {
                let _ = message
                    .property_list(names::PROPERTY_KEY, Some(&plist::Value::String(key.into())));
            })
            .ok_or(WireError::Malformed("no mcop reply"))?;
        let reply = Message::parse(&reply)?;
        Ok((
            reply.s32(names::ERROR)?.unwrap_or(-1),
            reply.property_list(names::PROPERTY_VALUE)?,
        ))
    }

    /// `mset`: the reply's status, or None with no reply.
    fn set_property(
        &self,
        key: plist::Value,
        value: plist::Value,
    ) -> Result<Option<i32>, WireError> {
        self.status_of(b"mset", NO_REPLY, |message| {
            let _ = message.property_list(names::PROPERTY_KEY, Some(&key));
            let _ = message.property_list(names::PROPERTY_VALUE, Some(&value));
        })
    }

    /// The processor's finalize: `-mep`, sent without waiting.
    fn close(&self) {
        self.send(
            MessageWriter::new(
                FourCc::new(*b"-mep"),
                MessageFlags::NONE,
                Some(self.uuid),
                0,
            )
            .finish(),
        );
    }

    /// Waits until the session's thread has handled everything sent so far
    /// and the device thread has delivered what came back: an `mcfr` round
    /// trip, then a `meps` one and the end of its delivery, whose sweep
    /// sends its `rels` after the reply.
    fn settle(&self) {
        let _ = self.call(b"mcfr", WAIT, |_| {});
        let (reply_id, meps) = self.message(b"meps", None, |message| {
            message.u32(names::VERSION, 6);
        });
        let _ = self.call_with(reply_id, meps, WAIT);
        let _ = self.progress.settled(reply_id, WAIT);
        self.state
            .seen
            .lock()
            .retain(|seen| seen != "reply mcfr" && seen != "reply meps");
    }

    /// Until the guard is dropped, the host session sweeps the moment one
    /// of its holds on a surface ends, as a delivery on the device's thread
    /// may at any moment: a message for no session, which gets no reply.
    fn sweep_whenever_a_hold_ends(&self) -> SweepWheneverAHoldEnds {
        let session = Arc::downgrade(&self.session);
        let stray = MessageWriter::new(
            FourCc::new(*b"mcfr"),
            MessageFlags::NONE,
            Some(GuestUuid([0x77; 16])),
            0,
        )
        .finish();
        *self.memory.on_release.lock() = Some(Arc::new(move || {
            if let Some(session) = session.upgrade() {
                session.deliver(&stray);
            }
        }));
        SweepWheneverAHoldEnds(self.memory.on_release.clone())
    }
}

/// Takes the sweep out when dropped, before the guest's teardown.
struct SweepWheneverAHoldEnds(Arc<Mutex<Option<ReleaseHook>>>);

impl Drop for SweepWheneverAHoldEnds {
    fn drop(&mut self) {
        self.0.lock().take();
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.session.invalidate();
        self.session.complete_invalidate();
        self.queue0.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// The guest's handler for the host's messages.
fn handle_host_message(
    state: &GuestState,
    memory: &GuestMemory,
    next_surface: &AtomicU64,
    queue0: &Sender<Vec<u8>>,
    message: &[u8],
) {
    if let Some((ids, _)) = read_surface_release(message) {
        state.seen.lock().push(format!(
            "rels {:?}",
            ids.iter().map(|id| id.0).collect::<Vec<_>>()
        ));
        return;
    }
    let Ok(header) = read_header(message, 0) else {
        state.seen.lock().push("unreadable".into());
        return;
    };
    let kind = header.message_type.to_string();
    if header.flags.contains(MessageFlags::REPLY_TO_GUEST) {
        state.seen.lock().push(format!("reply {kind}"));
        if let Some(waiter) = state.waiting.lock().remove(&header.reply_id) {
            let _ = waiter.send(message.to_vec());
        }
        return;
    }
    let Ok(parsed) = Message::parse(message) else {
        state.seen.lock().push(format!("{kind} unparsable"));
        return;
    };
    let request = parsed.s64(names::REQUEST).ok().flatten();
    match &header.message_type.to_be_bytes() {
        b"Mspa" | b"Mvpb" => {
            state
                .seen
                .lock()
                .push(format!("{kind} {:#x}", header.flags.0));
            let attributes = parsed
                .dictionary(names::PIXEL_BUFFER_ATTRIBUTES)
                .ok()
                .flatten();
            if kind == "Mspa" {
                *state.source_attributes.lock() = attributes;
            } else {
                *state.vector_attributes.lock() = attributes;
            }
            answer(queue0, &header, NO_ERROR, &[], |_| {});
        }
        b"Mbuf" => {
            state
                .seen
                .lock()
                .push(format!("Mbuf {}", request.unwrap_or(-1)));
            let attributes = state.vector_attributes.lock().clone().unwrap_or_default();
            let side = |key| {
                attributes
                    .get(key)
                    .and_then(as_i64)
                    .and_then(|side| u32::try_from(side).ok())
                    .unwrap_or(0)
            };
            let mapping_id = next_surface.fetch_add(1, Ordering::SeqCst);
            memory.insert(mapping_id, vector_surface(side("Width"), side("Height")));
            let mut uuid = [0xb0; 16];
            uuid[8..].copy_from_slice(&mapping_id.to_be_bytes());
            let uuid = GuestUuid(uuid);
            state.buffers.lock().insert(uuid, mapping_id);
            answer(queue0, &header, NO_ERROR, &[mapping_id], |reply| {
                reply.uuid(names::PIXEL_BUFFER_UUID, uuid);
            });
        }
        b"Memt" => {
            state
                .seen
                .lock()
                .push(format!("Memt {}", request.unwrap_or(-1)));
            let uuid = parsed.uuid(names::PIXEL_BUFFER_UUID).ok().flatten();
            let vectors = uuid
                .and_then(|uuid| state.buffers.lock().get(&uuid).copied())
                .and_then(|mapping_id| memory.surfaces.lock().get(&mapping_id).cloned())
                .map(|surface| read_vectors(&surface));
            state.emitted.lock().push(Emitted {
                request: request.unwrap_or(-1),
                status: parsed.s32(names::ERROR).ok().flatten().unwrap_or(-1),
                info: parsed
                    .u32(names::INFO_FLAGS)
                    .ok()
                    .flatten()
                    .unwrap_or(u32::MAX),
                uuid,
                vectors,
            });
            answer(queue0, &header, NO_ERROR, &[], |_| {});
        }
        b"Mdpb" => {
            let uuids = parsed
                .uuid_array(names::PIXEL_BUFFER_UUIDS)
                .ok()
                .flatten()
                .unwrap_or_default();
            state
                .seen
                .lock()
                .push(format!("Mdpb {:#x} {}", header.flags.0, uuids.len()));
            state.dropped.lock().push(uuids);
        }
        _ => state.seen.lock().push(kind),
    }
}

/// A guest reply to `request`, lending `surfaces`.
fn answer(
    queue0: &Sender<Vec<u8>>,
    request: &MessageHeader,
    status: i32,
    surfaces: &[u64],
    extra: impl FnOnce(&mut MessageWriter),
) {
    let mut reply = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
    reply.s32(names::ERROR, status);
    extra(&mut reply);
    let _ = queue0.send(with_surfaces(surfaces, &reply.finish()));
}

/// The vectors of a `2C0h` buffer, row by row.
fn read_vectors(surface: &SurfaceMemory) -> Vec<Vec<(f32, f32)>> {
    let bytes = surface.bytes.lock();
    let layout = &surface.layout;
    let width = usize::try_from(layout.width).unwrap_or(0);
    (0..usize::try_from(layout.height).unwrap_or(0))
        .map(|row| {
            let start = row * layout.bytes_per_row;
            bytes[start..start + width * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|vector| {
                    (
                        from_half_float(u16::from_le_bytes([vector[0], vector[1]])),
                        from_half_float(u16::from_le_bytes([vector[2], vector[3]])),
                    )
                })
                .collect()
        })
        .collect()
}

/// A biplanar 4:2:0 buffer, both planes 80 bytes a row, chroma at 4096.
fn nv12_surface(format: [u8; 4], width: u32, height: u32) -> SurfaceMemory {
    SurfaceMemory {
        layout: SurfaceLayout {
            pixel_format: FourCc::new(format),
            width,
            height,
            planes: vec![
                PlaneLayout {
                    offset: 0,
                    bytes_per_row: BYTES_PER_ROW,
                    width,
                    height,
                },
                PlaneLayout {
                    offset: CHROMA_OFFSET,
                    bytes_per_row: BYTES_PER_ROW,
                    width: width / 2,
                    height: height / 2,
                },
            ],
            bytes_per_row: 0,
        },
        bytes: Mutex::new(vec![0; CHROMA_OFFSET + BYTES_PER_ROW * 24]),
    }
}

/// A `2C0h` buffer of `width` by `height`, its rows 64-byte aligned, as
/// the guest's pool makes one: NaN everywhere to start with, so that a
/// vector the host did not write shows.
fn vector_surface(width: u32, height: u32) -> SurfaceMemory {
    let bytes_per_row = (usize::try_from(width).unwrap_or(0) * 4).div_ceil(64) * 64;
    let length = bytes_per_row * usize::try_from(height).unwrap_or(0);
    SurfaceMemory {
        layout: SurfaceLayout {
            pixel_format: FourCc::new(*b"2C0h"),
            width,
            height,
            planes: Vec::new(),
            bytes_per_row,
        },
        bytes: Mutex::new([0x00, 0x7e].repeat(length / 2)),
    }
}

/// The guest kext's `subs` box for up to five surfaces, then the message.
fn with_surfaces(mapping_ids: &[u64], message: &[u8]) -> Vec<u8> {
    if mapping_ids.is_empty() {
        return message.to_vec();
    }
    let mut bytes = vec![0, 0, 0, 0x30];
    bytes.extend_from_slice(b"subs");
    for slot in 0..5 {
        let id = mapping_ids.get(slot).copied().unwrap_or(0);
        bytes.extend_from_slice(&id.to_be_bytes());
    }
    bytes.extend_from_slice(message);
    bytes
}

/// The UUID the guest keeps source frame `n` under.
fn source_uuid(n: u8) -> GuestUuid {
    GuestUuid([n; 16])
}

fn specification(block: Option<i64>) -> Vec<(&'static str, plist::Value)> {
    let mut specification = vec![("HostProcessorID", plist::Value::String(OUR_ID.to_owned()))];
    if let Some(block) = block {
        specification.push(("MotionVectorSize", plist::Value::Integer(block.into())));
    }
    specification
}

fn started() -> Result<Guest, WireError> {
    let guest = Guest::new(true);
    assert_eq!(guest.create(&specification(None))?, Some(NO_ERROR));
    let (width, height) = (WIDTH.cast_signed(), HEIGHT.cast_signed());
    assert_eq!(guest.start(width, height)?, Some(NO_ERROR));
    guest.seen();
    Ok(guest)
}

/// What the stand-in gives a 64x48 pair on Apple's 10x4 grid: the
/// shift across, and c + 10 r down, within the picture's 4x3 blocks.
fn expected_vectors(shift: f32) -> Vec<Vec<(f32, f32)>> {
    (0..4u8)
        .map(|row| {
            (0..10u8)
                .map(|column| {
                    if column < 4 && row < 3 {
                        (shift, f32::from(column + 10 * row))
                    } else {
                        (0.0, 0.0)
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn meps_lists_the_processor_only_with_motion_estimation() -> Result<(), WireError> {
    let guest = Guest::new(true);
    let processors = guest.processors()?;
    assert_eq!(processors.len(), 1);
    let entry = processors[0]
        .as_dictionary()
        .ok_or(WireError::Malformed("an entry"))?;
    assert_eq!(
        entry
            .get("CMClassImplementationID")
            .and_then(plist::Value::as_string),
        Some(OUR_ID)
    );
    let matching = entry
        .get("CMMatchingInfo")
        .and_then(plist::Value::as_dictionary)
        .ok_or(WireError::Malformed("matching info"))?;
    for key in ["VTIsParavirtualizationAware", "VTIsHardwareAccelerated"] {
        assert_eq!(
            matching.get(key),
            Some(&plist::Value::Boolean(true)),
            "{key}"
        );
    }
    drop(guest);
    let without = Guest::new(false);
    assert!(without.processors()?.is_empty());
    assert_eq!(
        without.create(&specification(None))?,
        Some(Status::MotionEstimationProcessorNotFound.code())
    );
    Ok(())
}

#[test]
fn the_specification_selects_the_processor_as_apples_host_does() -> Result<(), WireError> {
    let not_found = Some(Status::MotionEstimationProcessorNotFound.code());
    let refusals: [(&str, plist::Value); 3] = [
        ("HostProcessorID", plist::Value::String("another".into())),
        (
            "CMClassImplementationName",
            plist::Value::String("another".into()),
        ),
        ("VTIsHardwareAccelerated", plist::Value::Boolean(false)),
    ];
    for refusal in refusals {
        let guest = Guest::new(true);
        assert_eq!(
            guest.create(std::slice::from_ref(&refusal))?,
            not_found,
            "{}",
            refusal.0
        );
        // A refused session exists, and answers nothing.
        let started = guest.status_of(b"mest", NO_REPLY, |message| {
            message.i32_pair(names::DIMENSIONS, ValueType::Dimensions, (64, 48));
        })?;
        assert_eq!(started, None);
    }
    let guest = Guest::new(true);
    assert_eq!(guest.create(&[])?, Some(NO_ERROR));
    Ok(())
}

#[test]
fn a_second_create_on_a_uuid_is_malformed_and_the_session_goes_on() -> Result<(), WireError> {
    let guest = started()?;
    assert_eq!(guest.create(&specification(None))?, None);
    let (reference, current) = (guest.frame(10), guest.frame(13));
    let uuids = [source_uuid(1), source_uuid(2)];
    assert_eq!(
        guest.process(1, &[reference, current], &uuids, 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].status, NO_ERROR);
    Ok(())
}

#[test]
fn a_start_asks_for_sources_and_for_vector_buffers_on_apples_grid() -> Result<(), WireError> {
    let guest = Guest::new(true);
    assert_eq!(guest.create(&specification(None))?, Some(NO_ERROR));
    guest.seen();
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    assert_eq!(guest.seen(), ["Mspa 0x2", "Mvpb 0x2", "reply mest"]);
    let sources = guest
        .state
        .source_attributes
        .lock()
        .clone()
        .ok_or(WireError::Malformed("Mspa"))?;
    let number = |dictionary: &plist::Dictionary, key| dictionary.get(key).and_then(as_i64);
    assert_eq!(number(&sources, "Width"), Some(64));
    assert_eq!(number(&sources, "Height"), Some(48));
    assert_eq!(number(&sources, "BytesPerRowAlignment"), Some(64));
    let formats: Vec<i64> = sources
        .get("PixelFormatType")
        .and_then(plist::Value::as_array)
        .map(|formats| formats.iter().filter_map(as_i64).collect())
        .unwrap_or_default();
    let fourcc = |code: &[u8; 4]| i64::from(u32::from_be_bytes(*code));
    assert_eq!(formats, [fourcc(b"420v"), fourcc(b"420f")]);
    assert!(sources.contains_key("IOSurfaceProperties"));
    let vectors = guest
        .state
        .vector_attributes
        .lock()
        .clone()
        .ok_or(WireError::Malformed("Mvpb"))?;
    assert_eq!(number(&vectors, "Width"), Some(10));
    assert_eq!(number(&vectors, "Height"), Some(4));
    assert_eq!(number(&vectors, "PixelFormatType"), Some(fourcc(b"2C0h")));
    assert!(vectors.contains_key("IOSurfaceProperties"));
    // 4x4 blocks: four times as many each way.
    let fine = Guest::new(true);
    assert_eq!(fine.create(&specification(Some(4)))?, Some(NO_ERROR));
    assert_eq!(fine.start(1920, 1080)?, Some(NO_ERROR));
    let vectors = fine
        .state
        .vector_attributes
        .lock()
        .clone()
        .ok_or(WireError::Malformed("Mvpb"))?;
    assert_eq!(number(&vectors, "Width"), Some(480));
    assert_eq!(number(&vectors, "Height"), Some(272));
    let opened = fine.backend.opened.lock().clone();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].block, crate::motion::BlockSize::Four);
    Ok(())
}

#[test]
fn a_size_apples_host_refuses_or_the_backend_cannot_take_starts_nothing() -> Result<(), WireError> {
    let guest = Guest::new(true);
    assert_eq!(guest.create(&specification(None))?, Some(NO_ERROR));
    guest.seen();
    let parameter = Some(Status::Parameter.code());
    for (width, height) in [
        (1, 48),
        (64, 0),
        (-2, 48),
        (16385, 16),
        (16384, 16384),
        (5000, 64),
    ] {
        assert_eq!(guest.start(width, height)?, parameter, "{width}x{height}");
    }
    assert_eq!(guest.seen(), ["reply mest"; 6]);
    assert!(guest.backend.opened.lock().is_empty());
    assert_eq!(guest.start(2, 2)?, Some(NO_ERROR));
    Ok(())
}

#[test]
fn a_frame_is_answered_then_emitted_in_apples_order() -> Result<(), WireError> {
    let guest = started()?;
    let (reference, current) = (guest.frame(10), guest.frame(13));
    let uuids = [source_uuid(1), source_uuid(2)];
    assert_eq!(
        guest.process(7, &[reference, current], &uuids, 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let vector_buffer = current + 1;
    assert_eq!(
        guest.seen(),
        [
            "reply mepf".to_owned(),
            "Mbuf 7".to_owned(),
            format!("rels [{reference}, {current}]"),
            "Memt 7".to_owned(),
            "Mdpb 0x0 2".to_owned(),
            format!("rels [{vector_buffer}]"),
        ]
    );
    assert_eq!(guest.state.dropped.lock().clone(), [uuids.to_vec()]);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    let mut uuid = [0xb0; 16];
    uuid[8..].copy_from_slice(&vector_buffer.to_be_bytes());
    assert_eq!(
        emitted[0],
        Emitted {
            request: 7,
            status: NO_ERROR,
            info: 0,
            uuid: Some(GuestUuid(uuid)),
            vectors: Some(expected_vectors(-3.0)),
        }
    );
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn the_sources_go_before_memt_and_the_buffer_after_mdpb_whoever_sweeps() -> Result<(), WireError> {
    let guest = started()?;
    let _sweep = guest.sweep_whenever_a_hold_ends();
    for request in 0..8 {
        let (reference, current) = (guest.frame(10), guest.frame(13));
        let uuids = [source_uuid(1), source_uuid(2)];
        assert_eq!(
            guest.process(request, &[reference, current], &uuids, 0)?,
            Some(NO_ERROR)
        );
        guest.settle();
        let seen = guest.seen();
        let at = |wanted: &str| seen.iter().position(|seen| seen == wanted);
        let released = |mapping_id: u64| {
            seen.iter().position(|seen| {
                seen.strip_prefix("rels [")
                    .and_then(|ids| ids.strip_suffix(']'))
                    .is_some_and(|ids| ids.split(", ").any(|id| id == mapping_id.to_string()))
            })
        };
        let emitted = at(&format!("Memt {request}"));
        let dropped = at("Mdpb 0x0 2");
        assert!(emitted.is_some() && dropped.is_some(), "{seen:?}");
        assert!(released(reference) < emitted, "{seen:?}");
        // In one `rels`, though a sweep came as each was let go.
        assert_eq!(released(reference), released(current), "{seen:?}");
        assert!(released(current + 1) > dropped, "{seen:?}");
    }
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_current_frame_flagged_as_the_next_reference_is_kept_for_it() -> Result<(), WireError> {
    let guest = started()?;
    let (first, second, third) = (guest.frame(10), guest.frame(12), guest.frame(15));
    let (a, b, c) = (source_uuid(1), source_uuid(2), source_uuid(3));
    assert_eq!(
        guest.process(1, &[first, second], &[a, b], 1)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let seen = guest.seen();
    assert!(seen.contains(&format!("rels [{first}]")), "{seen:?}");
    let second_released = format!("{second}");
    assert!(
        !seen
            .iter()
            .any(|seen| seen.starts_with("rels") && seen.contains(&second_released)),
        "{seen:?}"
    );
    assert_eq!(guest.memory.holds.lock().get(&second), Some(&1));
    assert_eq!(
        guest.process(2, &[second, third], &[b, c], 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let seen = guest.seen();
    assert!(
        seen.contains(&format!("rels [{second}, {third}]")),
        "{seen:?}"
    );
    assert_eq!(guest.state.dropped.lock().clone(), [vec![a], vec![b, c]]);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].vectors, Some(expected_vectors(-2.0)));
    assert_eq!(emitted[1].vectors, Some(expected_vectors(-3.0)));
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn one_surface_lent_as_both_frames_is_read_once() -> Result<(), WireError> {
    // The tests' surfaces lock their memory to read it, as a mapper may:
    // a second read inside the first would wait for ever.
    let guest = started()?;
    let frame = guest.frame(10);
    let uuid = source_uuid(1);
    assert_eq!(
        guest.process(1, &[frame, frame], &[uuid, uuid], 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].vectors, Some(expected_vectors(0.0)));
    Ok(())
}

#[test]
fn a_malformed_frame_gets_no_reply() -> Result<(), WireError> {
    let guest = started()?;
    let (reference, current, other) = (guest.frame(10), guest.frame(13), guest.frame(14));
    let (a, b) = (source_uuid(1), source_uuid(2));
    assert_eq!(guest.process(1, &[reference], &[a, b], 0)?, None);
    assert_eq!(guest.process(2, &[reference, current], &[a], 0)?, None);
    assert_eq!(
        guest.process(3, &[reference, current, other], &[a, b], 0)?,
        None
    );
    // A UUID the host holds with another surface: the current frame kept,
    // then named again with another.
    assert_eq!(
        guest.process(4, &[reference, current], &[a, b], 1)?,
        Some(NO_ERROR)
    );
    guest.settle();
    assert_eq!(guest.process(5, &[other, reference], &[b, a], 0)?, None);
    // Swapped UUIDs are taken, as Apple's were.
    assert_eq!(
        guest.process(6, &[current, reference], &[b, a], 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let requests: Vec<i64> = guest.emitted().iter().map(|e| e.request).collect();
    assert_eq!(requests, [4, 6]);
    Ok(())
}

/// A malformed frame's surfaces go together, in one `rels`, though a sweep
/// came as each was let go.
#[test]
fn a_malformed_frames_surfaces_go_in_one_rels() -> Result<(), WireError> {
    let guest = started()?;
    let _sweep = guest.sweep_whenever_a_hold_ends();
    let frames = [guest.frame(10), guest.frame(13), guest.frame(14)];
    let uuids = [source_uuid(1), source_uuid(2)];
    assert_eq!(guest.process(1, &frames, &uuids, 0)?, None);
    guest.settle();
    let released: Vec<String> = (guest.seen().into_iter())
        .filter(|seen| seen.starts_with("rels "))
        .collect();
    let [a, b, c] = frames;
    assert_eq!(released, [format!("rels [{a}, {b}, {c}]")]);
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

/// Malformed frames leave their UUIDs in the table with no clean-up. Past
/// [`super::MAX_PIXEL_BUFFERS`] the session cleans up first, so that
/// neither the table nor an `Mdpb` grows without end: one at a time, each
/// dead UUID is still named once; in a burst, whose queued messages keep
/// the one surface they all lend alive, the table fills, the frames past it
/// are refused, and the session goes on. The host holds that surface
/// through the burst here, as its queued messages do while the session's
/// thread is behind the device's, which a loaded host need not be.
#[test]
fn malformed_frames_cannot_grow_the_pixel_buffer_table() -> Result<(), WireError> {
    let guest = started()?;
    let (reference, current, other) = (guest.frame(10), guest.frame(13), guest.frame(14));
    let (a, b) = (source_uuid(1), source_uuid(2));
    // `b` lives on with `current`, kept as the next reference.
    assert_eq!(
        guest.process(1, &[reference, current], &[a, b], 1)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let fresh = |n: u16| {
        let mut uuid = [0xc0; 16];
        uuid[14..].copy_from_slice(&n.to_be_bytes());
        GuestUuid(uuid)
    };
    // `b` named with another surface: malformed, after `fresh(n)` is in.
    let malformed = |n: u16| {
        let (_, message) = guest.message(b"mepf", Some(guest.uuid), |message| {
            message.s64(names::REQUEST, 1000 + i64::from(n));
            message.uuid_array(names::PIXEL_BUFFER_UUIDS, &[fresh(n), b]);
            message.u32(names::MOTION_FRAME_FLAGS, 1);
            let _ = message.property_list(names::MOTION_OPTIONS, None);
        });
        guest.send(with_surfaces(&[reference, other], &message));
    };
    let count = 300_u16;
    for n in 0..count {
        malformed(n);
        guest.settle();
    }
    // `b` kept with `current` again.
    assert_eq!(
        guest.process(2, &[reference, current], &[a, b], 1)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let dropped = std::mem::take(&mut *guest.state.dropped.lock());
    let named = dropped.concat();
    for n in 0..count {
        let times = named.iter().filter(|uuid| **uuid == fresh(n)).count();
        assert_eq!(times, 1, "fresh({n})");
    }
    let shared = &guest.session.shared;
    let burst_lends = crate::host::mapped_surfaces::hold(
        &shared.mapped,
        &*shared.surfaces,
        &[MappingId(reference)],
    );
    assert!(burst_lends.is_ok());
    for n in count..3 * count {
        malformed(n);
    }
    guest.settle();
    shared.let_go(burst_lends);
    assert_eq!(
        guest.process(3, &[current, reference], &[b, a], 0)?,
        Some(NO_ERROR)
    );
    guest.settle();
    let dropped = [dropped, std::mem::take(&mut *guest.state.dropped.lock())].concat();
    let sizes: Vec<usize> = dropped.iter().map(Vec::len).collect();
    assert!(
        sizes.iter().all(|size| *size <= super::MAX_PIXEL_BUFFERS),
        "{sizes:?}"
    );
    // The burst filled the table.
    assert!(
        sizes
            .iter()
            .filter(|size| **size >= super::MAX_PIXEL_BUFFERS - 2)
            .count()
            >= 2,
        "{sizes:?}"
    );
    let requests: Vec<i64> = guest.emitted().iter().map(|e| e.request).collect();
    assert_eq!(requests, [1, 2, 3]);
    Ok(())
}

#[test]
fn frames_the_session_cannot_take_are_refused_in_the_reply() -> Result<(), WireError> {
    let guest = Guest::new(true);
    assert_eq!(guest.create(&specification(None))?, Some(NO_ERROR));
    let (reference, current) = (guest.frame(10), guest.frame(13));
    let uuids = [source_uuid(1), source_uuid(2)];
    // Before `mest`.
    assert_eq!(
        guest.process(1, &[reference, current], &uuids, 0)?,
        Some(Status::InvalidSession.code())
    );
    assert_eq!(guest.start(32, 48)?, Some(NO_ERROR));
    // Of another size.
    assert_eq!(
        guest.process(2, &[reference, current], &uuids, 0)?,
        Some(Status::Parameter.code())
    );
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    // Of a format the backend does not take.
    let p010 = guest.next_surface.fetch_add(1, Ordering::SeqCst);
    guest
        .memory
        .insert(p010, nv12_surface(*b"x420", WIDTH, HEIGHT));
    assert_eq!(
        guest.process(3, &[reference, p010], &[source_uuid(1), source_uuid(3)], 0)?,
        Some(Status::Parameter.code())
    );
    guest.settle();
    assert!(guest.emitted().is_empty());
    assert!(!guest.seen().iter().any(|seen| seen.starts_with("Mbuf")));
    // A frame the estimator cannot take is emitted with its status, and no
    // buffer is asked for.
    let broken = guest.frame(BROKEN);
    assert_eq!(
        guest.process(
            4,
            &[reference, broken],
            &[source_uuid(1), source_uuid(4)],
            0
        )?,
        Some(NO_ERROR)
    );
    guest.settle();
    assert!(!guest.seen().iter().any(|seen| seen.starts_with("Mbuf")));
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].status, Status::Parameter.code());
    assert_eq!(emitted[0].uuid, None);
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn properties_answer_as_apples_processor_did() -> Result<(), WireError> {
    let guest = Guest::new(true);
    let mut multipass = specification(Some(8));
    multipass.push(("UseMultiPassSearch", plist::Value::Boolean(true)));
    assert_eq!(guest.create(&multipass)?, Some(NO_ERROR));
    let boolean = plist::Value::Boolean;
    for (key, value) in [
        ("IncludeMotionConfidence", boolean(false)),
        ("MotionVectorSize", plist::Value::Integer(16.into())),
        ("UseMultiPassSearch", boolean(true)),
        ("TrueMotion", boolean(false)),
        ("PropertyMode", plist::Value::Integer(0.into())),
        ("VTIsHardwareAccelerated", boolean(false)),
    ] {
        assert_eq!(guest.copy_property(key)?, (NO_ERROR, Some(value)), "{key}");
    }
    // Where Apple's processor traps.
    assert_eq!(
        guest.copy_property("Label")?,
        (Status::PropertyNotSupported.code(), None)
    );
    let key = |key: &str| plist::Value::String(key.to_owned());
    assert_eq!(
        guest.set_property(key("TrueMotion"), boolean(true))?,
        Some(NO_ERROR)
    );
    assert_eq!(
        guest.copy_property("TrueMotion")?,
        (NO_ERROR, Some(boolean(true)))
    );
    let mode = plist::Value::Integer(2.into());
    assert_eq!(
        guest.set_property(key("PropertyMode"), mode.clone())?,
        Some(NO_ERROR)
    );
    assert_eq!(guest.copy_property("PropertyMode")?, (NO_ERROR, Some(mode)));
    assert_eq!(
        guest.set_property(key("ClientPID"), plist::Value::Integer(42.into()))?,
        Some(NO_ERROR)
    );
    let refused = Some(Status::MotionPropertyRefused.code());
    assert_eq!(
        guest.set_property(key("NoSuchProperty"), boolean(true))?,
        refused
    );
    assert_eq!(
        guest.set_property(key("Paravirtualized"), boolean(true))?,
        refused
    );
    assert_eq!(
        guest.set_property(plist::Value::Integer(1.into()), boolean(true))?,
        None
    );
    assert_eq!(
        guest.status_of(b"mmul", WAIT, |message| {
            let _ = message.property_list(names::PROPERTIES, None);
        })?,
        Some(Status::Parameter.code())
    );
    let properties: plist::Dictionary = [("TrueMotion".to_owned(), boolean(false))]
        .into_iter()
        .collect();
    assert_eq!(
        guest.status_of(b"mmul", WAIT, |message| {
            let _ = message.property_list(
                names::PROPERTIES,
                Some(&plist::Value::Dictionary(properties)),
            );
        })?,
        Some(NO_ERROR)
    );
    assert_eq!(
        guest.copy_property("TrueMotion")?,
        (NO_ERROR, Some(boolean(false)))
    );
    assert_eq!(
        guest.status_of(b"msav", WAIT, |_| {})?,
        Some(Status::UnimplementedFunction.code())
    );
    assert_eq!(guest.status_of(b"mesu", NO_REPLY, |_| {})?, None);
    assert_eq!(guest.status_of(b"mxyz", NO_REPLY, |_| {})?, None);
    Ok(())
}

#[test]
fn a_frame_after_minv_fails_with_the_sessions_malfunction() -> Result<(), WireError> {
    let guest = started()?;
    assert_eq!(guest.status_of(b"minv", WAIT, |_| {})?, Some(NO_ERROR));
    let (reference, current) = (guest.frame(10), guest.frame(13));
    let malfunction = Status::SessionMalfunction.code();
    assert_eq!(
        guest.process(
            1,
            &[reference, current],
            &[source_uuid(1), source_uuid(2)],
            0
        )?,
        Some(malfunction)
    );
    guest.settle();
    let seen = guest.seen();
    let emitted_at = seen.iter().position(|seen| seen == "Memt 1");
    let replied_at = seen.iter().position(|seen| seen == "reply mepf");
    assert!(emitted_at < replied_at && emitted_at.is_some(), "{seen:?}");
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!((emitted[0].status, emitted[0].uuid), (malfunction, None));
    assert_eq!(guest.start(64, 48)?, Some(malfunction));
    Ok(())
}

#[test]
fn a_closed_session_is_gone() -> Result<(), WireError> {
    let guest = started()?;
    let (reference, current) = (guest.frame(10), guest.frame(13));
    assert_eq!(
        guest.process(
            1,
            &[reference, current],
            &[source_uuid(1), source_uuid(2)],
            1
        )?,
        Some(NO_ERROR)
    );
    guest.settle();
    guest.close();
    let (reply_id, message) = guest.message(b"mcfr", Some(guest.uuid), |_| {});
    assert_eq!(guest.call_with(reply_id, message, NO_REPLY), None);
    assert_eq!(
        guest.session.deliver(
            &MessageWriter::new(
                FourCc::new(*b"mcfr"),
                MessageFlags::NONE,
                Some(guest.uuid),
                0
            )
            .finish()
        ),
        Status::SessionNotFound.code()
    );
    // Its kept frame went with it.
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}
