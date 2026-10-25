//! The motion-estimation check: frames of [`super::texture`] moved by a
//! known vector, estimated through the host session as a guest's
//! `VTMotionEstimationSession` would ask, on any backend's motion part, and
//! the vectors the guest got checked against the way back, (-dx, -dy).
//!
//! A stand-in for the guest's processor drives a `HostSession` as core's motion tests
//! do: `meps`, `+mep` naming our processor with the case's `MotionVectorSize`, `mest`,
//! one `mepf` lending the two frames, and `-mep`. A second thread plays the guest's
//! handler: it keeps `Mspa`'s and `Mvpb`'s attributes, lends a vector buffer of
//! `Mvpb`'s size and `2C0h` for `Mbuf`, and reads the vectors when `Memt` names it. A
//! third delivers queue 0 in order.
//!
//! What a backend gives is checked on Apple's grid, as the guest sees it: a
//! case passes when at least [`LEAST_SHARE`] of the cells the picture
//! reaches are the way back and every padding cell is (0, 0). Edge cells,
//! whose content came from outside the reference, may differ, as Apple's
//! did. On 16x16 blocks a cell within 2 pixels of the way back each way is
//! the way back: Apple's processor gives multiples of 4 there, the nearest
//! to the motion. On 4x4 blocks it must be exact, as Apple's was. The 16x16
//! cases' shifts are never half way between two multiples of 4, where
//! Apple's split its cells between the two.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use reims_vgpu_video_core::backend::Backend;
use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::host::{GuestSink, HostSession};
use reims_vgpu_video_core::motion::{AppleGrid, BlockSize, from_half_float};
use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::runtime::{Runtime, StdRuntime};
use reims_vgpu_video_core::surfaces::{
    GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
};
use reims_vgpu_video_core::sync::Mutex;
use reims_vgpu_video_core::wire::message::{
    GuestUuid, Message, MessageFlags, MessageHeader, MessageWriter, ValueType, read_header,
};
use reims_vgpu_video_core::wire::surface_boxes::read_surface_release;

use super::texture::moved_luma;

const fn key(name: &[u8; 4]) -> FourCc {
    FourCc::new(*name)
}
const VERSION: FourCc = key(b",ver");
const ERROR: FourCc = key(b",err");
const PROCESSORS: FourCc = key(b",mep");
const OPTIONS: FourCc = key(b",meo");
const GUEST_INFO: FourCc = key(b",gin");
const DIMENSIONS: FourCc = key(b",vdi");
const REQUEST: FourCc = key(b",rid");
const UUID: FourCc = key(b",uui");
const UUIDS: FourCc = key(b",ui+");
const FLAGS: FourCc = key(b",mef");
const ATTRIBUTES: FourCc = key(b",pba");

/// Our processor's ID, as `meps` lists it.
const OUR_PROCESSOR: &str = "reims-vgpu-video.motionestimationprocessor";
/// The guest processor's waits: 10 s for `mepf`, 1.1 s for the rest.
const FRAME_WAIT: Duration = Duration::from_secs(10);
const WAIT: Duration = Duration::from_millis(1100);
/// How long the check waits for `Memt` after the `mepf` reply.
const EMIT_WAIT: Duration = Duration::from_secs(10);
/// The share of the picture's cells that must be the way back. Apple's
/// processor, through VideoToolbox on an M3 Max, made 75% to 100% of the
/// cases' (2026-10-24); a backend that has the sign, the unit or the layout
/// wrong makes next to none, so the check is of those, not of how good an
/// estimator is.
pub const LEAST_SHARE: f64 = 0.7;
/// Padding past each 64-byte-aligned row of the guest's source buffers: a
/// read at the wrong stride shows.
const ROW_PADDING: usize = 128;

/// One case: a size, a block size and a shift of the current frame.
#[derive(Clone, Debug)]
pub struct Case {
    /// `b`, the block size, `-`, the size, `-`, the shift.
    pub name: String,
    /// The picture's width.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// `MotionVectorSize`: 4 or 16.
    pub block: u32,
    /// How far the current frame's content moved, in pixels.
    pub shift: (f64, f64),
}

/// The cases: whole shifts both ways on both block sizes, an odd size
/// whose last cells are partly outside the picture, a size under Apple's
/// grid's 10 by 4 least, and an HD frame.
pub fn cases() -> Vec<Case> {
    let case = |width, height, block, shift: (f64, f64)| Case {
        name: format!(
            "b{block}-{width}x{height}-{}{},{}",
            sign(shift.0),
            shift.0.abs(),
            shift.1
        ),
        width,
        height,
        block,
        shift,
    };
    vec![
        case(256, 128, 16, (8.0, 4.0)),
        case(256, 128, 16, (-12.0, 7.0)),
        case(256, 128, 4, (3.0, -2.0)),
        case(256, 128, 4, (-5.0, 7.0)),
        case(641, 361, 16, (7.0, -9.0)),
        case(641, 361, 4, (-4.0, 3.0)),
        case(1280, 720, 16, (20.0, -12.0)),
        case(128, 48, 16, (4.0, 3.0)),
    ]
}

fn sign(value: f64) -> &'static str {
    if value < 0.0 { "-" } else { "" }
}

/// What the guest got for one case.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Apple's grid for the case: its cells across.
    pub columns: u32,
    /// Its cells down.
    pub rows: u32,
    /// The cells across that the picture reaches; the rest are padding.
    pub picture_columns: u32,
    /// The cells down that it reaches.
    pub picture_rows: u32,
    /// Every cell, row by row.
    pub vectors: Vec<(f32, f32)>,
    /// From `mepf` to `Memt`.
    pub elapsed: Duration,
}

impl Outcome {
    /// The share of the picture's cells within `tolerance` pixels of
    /// `vector` each way.
    pub fn share(&self, vector: (f32, f32), tolerance: f32) -> f64 {
        let near = |cell: &(f32, f32)| {
            (cell.0 - vector.0).abs() <= tolerance && (cell.1 - vector.1).abs() <= tolerance
        };
        let mut hits = 0_u64;
        for row in 0..self.picture_rows {
            for column in 0..self.picture_columns {
                let index = (row * self.columns + column) as usize;
                hits += u64::from(self.vectors.get(index).is_some_and(near));
            }
        }
        hits as f64 / f64::from((self.picture_columns * self.picture_rows).max(1))
    }

    /// The padding cells that are not (0, 0).
    pub fn stray_padding(&self) -> usize {
        let mut stray = 0;
        for row in 0..self.rows {
            for column in 0..self.columns {
                let padding = row >= self.picture_rows || column >= self.picture_columns;
                let index = (row * self.columns + column) as usize;
                stray += usize::from(padding && self.vectors.get(index) != Some(&(0.0, 0.0)));
            }
        }
        stray
    }

    /// The commonest vector over the picture, for the log.
    fn commonest(&self) -> ((f32, f32), usize) {
        let mut counts: Vec<((f32, f32), usize)> = Vec::new();
        for row in 0..self.picture_rows {
            for column in 0..self.picture_columns {
                let index = (row * self.columns + column) as usize;
                let Some(vector) = self.vectors.get(index) else {
                    continue;
                };
                match counts.iter_mut().find(|(seen, _)| seen == vector) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((*vector, 1)),
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .unwrap_or(((0.0, 0.0), 0))
    }
}

/// Runs `case` on `backend` and says whether it passed, with its line.
pub fn check(backend: &Backend, case: &Case) -> (bool, String) {
    match run(backend, case) {
        Err(error) => (false, format!("FAIL {}: {error}", case.name)),
        Ok(outcome) => {
            let back = (-case.shift.0 as f32, -case.shift.1 as f32);
            let share = outcome.share(back, tolerance(case.block));
            let stray = outcome.stray_padding();
            let (commonest, count) = outcome.commonest();
            let passed = share >= LEAST_SHARE && stray == 0;
            (
                passed,
                format!(
                    "{} {}: {:.1}% of {}x{} cells are ({}, {}) within {}, commonest {commonest:?} \
                     x{count}, {stray} stray padding cells of {}x{}, {:.1} ms",
                    if passed { "PASS" } else { "FAIL" },
                    case.name,
                    share * 100.0,
                    outcome.picture_columns,
                    outcome.picture_rows,
                    back.0,
                    back.1,
                    tolerance(case.block),
                    outcome.columns,
                    outcome.rows,
                    outcome.elapsed.as_secs_f64() * 1000.0,
                ),
            )
        }
    }
}

/// How far from the way back a cell may be on `block`-pixel blocks, each
/// way: half of Apple's step of 4 on 16x16 blocks, none on 4x4.
pub fn tolerance(block: u32) -> f32 {
    if block >= 16 { 2.0 } else { 0.0 }
}

/// One of the guest's IOSurfaces.
struct Buffer {
    layout: SurfaceLayout,
    bytes: Mutex<Vec<u8>>,
    /// Whether the host may write it: the vector buffers alone.
    writable: bool,
}

/// The guest's IOSurfaces by mapping ID.
#[derive(Default)]
struct GuestMemory {
    surfaces: Mutex<HashMap<u64, Arc<Buffer>>>,
}

struct Held(Arc<Buffer>);

impl GuestSurface for Held {
    fn layout(&self) -> SurfaceLayout {
        self.0.layout.clone()
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        if !self.0.writable {
            return Err(SurfaceError::Unreachable("a source buffer".into()));
        }
        write(&mut self.0.bytes.lock());
        Ok(())
    }

    fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
        read(&self.0.bytes.lock());
        Ok(())
    }
}

impl Surfaces for GuestMemory {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        let buffer = self
            .surfaces
            .lock()
            .get(&mapping_id.0)
            .cloned()
            .ok_or(SurfaceError::Unknown(mapping_id.0))?;
        Ok(Box::new(Held(buffer)))
    }
}

struct ChannelSink(Mutex<Sender<Vec<u8>>>);

impl GuestSink for ChannelSink {
    fn send(&self, message: Vec<u8>) {
        let _ = self.0.lock().send(message);
    }
}

/// The guest processor's side of what the host sends.
#[derive(Default)]
struct Guest {
    memory: Arc<GuestMemory>,
    next_mapping_id: AtomicU64,
    waiting: Mutex<HashMap<u64, Sender<Vec<u8>>>>,
    source_attributes: Mutex<Option<plist::Dictionary>>,
    vector_attributes: Mutex<Option<plist::Dictionary>>,
    /// The vector buffers lent, by UUID.
    vector_buffers: Mutex<HashMap<GuestUuid, Arc<Buffer>>>,
    /// Each `Memt`: its status and the buffer it named.
    emitted: Mutex<Vec<(i32, Option<Arc<Buffer>>)>>,
    emitted_signal: Mutex<Option<Sender<()>>>,
}

impl Guest {
    fn handle(&self, message: &[u8], queue0: &Sender<Vec<u8>>) {
        if read_surface_release(message).is_some() {
            return;
        }
        let Ok(header) = read_header(message, 0) else {
            return;
        };
        if header.flags.contains(MessageFlags::REPLY_TO_GUEST) {
            if let Some(waiter) = self.waiting.lock().remove(&header.reply_id) {
                let _ = waiter.send(message.to_vec());
            }
            return;
        }
        let Ok(parsed) = Message::parse(message) else {
            return;
        };
        match &header.message_type.to_be_bytes() {
            b"Mspa" | b"Mvpb" => {
                let attributes = parsed.dictionary(ATTRIBUTES).ok().flatten();
                if header.message_type == key(b"Mspa") {
                    *self.source_attributes.lock() = attributes;
                } else {
                    *self.vector_attributes.lock() = attributes;
                }
                answer(queue0, &header, &[], |_| {});
            }
            b"Mbuf" => {
                let attributes = self.vector_attributes.lock().clone().unwrap_or_default();
                let side = |name| {
                    attributes
                        .get(name)
                        .and_then(plist::Value::as_signed_integer)
                        .and_then(|side| u32::try_from(side).ok())
                        .unwrap_or(0)
                };
                let buffer = Arc::new(vector_buffer(side("Width"), side("Height")));
                let mapping_id = self.lend(buffer.clone());
                let mut uuid = [0xb0; 16];
                uuid[8..].copy_from_slice(&mapping_id.to_be_bytes());
                let uuid = GuestUuid(uuid);
                self.vector_buffers.lock().insert(uuid, buffer);
                answer(queue0, &header, &[mapping_id], |reply| {
                    reply.uuid(UUID, uuid);
                });
            }
            b"Memt" => {
                let status = parsed.s32(ERROR).ok().flatten().unwrap_or(-1);
                let buffer = parsed
                    .uuid(UUID)
                    .ok()
                    .flatten()
                    .and_then(|uuid| self.vector_buffers.lock().get(&uuid).cloned());
                self.emitted.lock().push((status, buffer));
                answer(queue0, &header, &[], |_| {});
                if let Some(signal) = self.emitted_signal.lock().as_ref() {
                    let _ = signal.send(());
                }
            }
            _ => {}
        }
    }

    /// Puts `buffer` in the guest's memory: its mapping ID.
    fn lend(&self, buffer: Arc<Buffer>) -> u64 {
        let mapping_id = self.next_mapping_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.memory.surfaces.lock().insert(mapping_id, buffer);
        mapping_id
    }
}

/// The guest's reply to `request`, lending `surfaces`.
fn answer(
    queue0: &Sender<Vec<u8>>,
    request: &MessageHeader,
    surfaces: &[u64],
    extra: impl FnOnce(&mut MessageWriter),
) {
    let mut reply = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
    reply.s32(ERROR, 0);
    extra(&mut reply);
    let _ = queue0.send(with_surfaces(surfaces, &reply.finish()));
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

/// A `420v` source of the texture moved by `shift`, grey chroma: both
/// planes [`ROW_PADDING`] past a 64-byte-aligned row, chroma page-aligned.
fn source_buffer(width: u32, height: u32, shift: (f64, f64)) -> Buffer {
    let (w, h) = (width as usize, height as usize);
    let bytes_per_row = w.div_ceil(64) * 64 + ROW_PADDING;
    let chroma_rows = h.div_ceil(2);
    let chroma_offset = (bytes_per_row * h).div_ceil(4096) * 4096;
    let mut bytes = vec![0x80; chroma_offset + bytes_per_row * chroma_rows];
    let luma = moved_luma(w, h, bytes_per_row, shift.0, shift.1);
    bytes[..luma.len()].copy_from_slice(&luma);
    Buffer {
        layout: SurfaceLayout {
            pixel_format: key(b"420v"),
            width,
            height,
            planes: vec![
                PlaneLayout {
                    offset: 0,
                    bytes_per_row,
                    width,
                    height,
                },
                PlaneLayout {
                    offset: chroma_offset,
                    bytes_per_row,
                    width: width.div_ceil(2),
                    height: height.div_ceil(2),
                },
            ],
            bytes_per_row: 0,
        },
        bytes: Mutex::new(bytes),
        writable: false,
    }
}

/// A `2C0h` buffer of `width` by `height` cells, 64-byte-aligned rows, as
/// the guest's pool makes one: NaN everywhere to start with, so that a
/// cell the host did not write shows.
fn vector_buffer(width: u32, height: u32) -> Buffer {
    let bytes_per_row = (width as usize * 4).div_ceil(64) * 64;
    let length = bytes_per_row * height as usize;
    Buffer {
        layout: SurfaceLayout {
            pixel_format: key(b"2C0h"),
            width,
            height,
            planes: Vec::new(),
            bytes_per_row,
        },
        bytes: Mutex::new([0x00, 0x7e].repeat(length / 2)),
        writable: true,
    }
}

/// The vectors of a `2C0h` buffer, row by row.
fn read_vectors(buffer: &Buffer) -> Vec<(f32, f32)> {
    let bytes = buffer.bytes.lock();
    let layout = &buffer.layout;
    let mut vectors = Vec::new();
    for row in 0..layout.height as usize {
        let start = row * layout.bytes_per_row;
        let Some(line) = bytes.get(start..start + layout.width as usize * 4) else {
            break;
        };
        for cell in line.as_chunks::<4>().0 {
            vectors.push((
                from_half_float(u16::from_le_bytes([cell[0], cell[1]])),
                from_half_float(u16::from_le_bytes([cell[2], cell[3]])),
            ));
        }
    }
    vectors
}

/// The guest processor's calls.
struct Caller {
    guest: Arc<Guest>,
    queue0: Sender<Vec<u8>>,
    uuid: GuestUuid,
    next_reply_id: AtomicU64,
}

impl Caller {
    fn message(
        &self,
        kind: &[u8; 4],
        uuid: Option<GuestUuid>,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> (u64, Vec<u8>) {
        let reply_id = self.next_reply_id.fetch_add(1, Ordering::SeqCst);
        let mut message =
            MessageWriter::new(key(kind), MessageFlags::GUEST_EXPECTS_REPLY, uuid, reply_id);
        parameters(&mut message);
        (reply_id, message.finish())
    }

    fn call_with(
        &self,
        kind: &[u8; 4],
        reply_id: u64,
        message: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        let (arrived, reply) = mpsc::channel();
        self.guest.waiting.lock().insert(reply_id, arrived);
        let _ = self.queue0.send(message);
        let reply = reply.recv_timeout(timeout);
        self.guest.waiting.lock().remove(&reply_id);
        reply.map_err(|_| format!("no {} reply", String::from_utf8_lossy(kind)))
    }

    /// A call's `,err`.
    fn status(&self, kind: &[u8; 4], reply: &[u8]) -> Result<i32, String> {
        Message::parse(reply)
            .and_then(|message| message.s32(ERROR))
            .map_err(|error| format!("{}: {error}", String::from_utf8_lossy(kind)))?
            .ok_or_else(|| format!("{} replied with no ,err", String::from_utf8_lossy(kind)))
    }
}

/// Runs `case` on `backend`: what the guest's processor got.
pub fn run(backend: &Backend, case: &Case) -> Result<Outcome, String> {
    let block = match case.block {
        4 => BlockSize::Four,
        16 => BlockSize::Sixteen,
        other => return Err(format!("a MotionVectorSize of {other}")),
    };
    let guest = Arc::new(Guest::default());
    let (to_guest, from_host) = mpsc::channel::<Vec<u8>>();
    let session = Arc::new(HostSession::with_backend(
        StdRuntime::shared(),
        Arc::new(ChannelSink(Mutex::new(to_guest))),
        guest.memory.clone(),
        backend,
    ));
    let (queue0, device_queue) = mpsc::channel::<Vec<u8>>();
    let device = {
        let session = session.clone();
        StdRuntime::thread("conformance-device", move || {
            // An empty message ends the device's thread.
            while let Ok(message) = device_queue.recv()
                && !message.is_empty()
            {
                session.deliver(&message);
            }
        })
        .map_err(|e| e.to_string())?
    };
    let handler = {
        let guest = guest.clone();
        let queue0 = queue0.clone();
        StdRuntime::thread("conformance-guest", move || {
            while let Ok(message) = from_host.recv() {
                guest.handle(&message, &queue0);
            }
        })
        .map_err(|e| e.to_string())?
    };
    let caller = Caller {
        guest: guest.clone(),
        queue0,
        uuid: GuestUuid([0x6d; 16]),
        next_reply_id: AtomicU64::new(100),
    };
    let outcome = drive(&caller, case, block);
    // `-mep`, then the device's thread ends; the session's end closes the
    // guest's queue, which ends the handler's.
    let (_, close) = caller.message(b"-mep", Some(caller.uuid), |_| {});
    let _ = caller.queue0.send(close);
    let _ = caller.queue0.send(Vec::new());
    let _ = device.join();
    session.invalidate();
    session.complete_invalidate();
    drop(session);
    drop(caller);
    let _ = handler.join();
    outcome
}

/// The guest processor's session: `meps`, `+mep`, `mest`, one `mepf`.
fn drive(caller: &Caller, case: &Case, block: BlockSize) -> Result<Outcome, String> {
    let (reply_id, meps) = caller.message(b"meps", None, |message| {
        message.u32(VERSION, 6);
    });
    let reply = caller.call_with(b"meps", reply_id, meps, WAIT)?;
    let listed = Message::parse(&reply)
        .and_then(|reply| reply.property_list(PROCESSORS))
        .map_err(|error| error.to_string())?;
    let ours = match &listed {
        Some(plist::Value::Array(processors)) => processors.iter().any(|processor| {
            processor
                .as_dictionary()
                .and_then(|entry| entry.get("CMClassImplementationID"))
                .and_then(plist::Value::as_string)
                == Some(OUR_PROCESSOR)
        }),
        _ => false,
    };
    if !ours {
        return Err(format!("meps lists no processor of ours: {listed:?}"));
    }
    let specification: plist::Dictionary = [
        (
            "HostProcessorID".to_owned(),
            plist::Value::String(OUR_PROCESSOR.to_owned()),
        ),
        (
            "MotionVectorSize".to_owned(),
            plist::Value::Integer(i64::from(case.block).into()),
        ),
    ]
    .into_iter()
    .collect();
    let (reply_id, create) = caller.message(b"+mep", Some(caller.uuid), |message| {
        message.u32(VERSION, 6);
        let _ = message.property_list(OPTIONS, Some(&plist::Value::Dictionary(specification)));
        let _ = message.property_list(GUEST_INFO, None);
    });
    let reply = caller.call_with(b"+mep", reply_id, create, WAIT)?;
    let status = caller.status(b"+mep", &reply)?;
    if status != 0 {
        return Err(format!("+mep answered {status}"));
    }
    let (reply_id, start) = caller.message(b"mest", Some(caller.uuid), |message| {
        message.i32_pair(
            DIMENSIONS,
            ValueType::Dimensions,
            (case.width as i32, case.height as i32),
        );
    });
    let reply = caller.call_with(b"mest", reply_id, start, WAIT)?;
    let status = caller.status(b"mest", &reply)?;
    if status != 0 {
        return Err(format!("mest answered {status}"));
    }
    let takes_420v = caller
        .guest
        .source_attributes
        .lock()
        .as_ref()
        .and_then(|attributes| attributes.get("PixelFormatType").cloned())
        .is_some_and(|formats| {
            let code = i64::from(u32::from_be_bytes(*b"420v"));
            match formats {
                plist::Value::Array(formats) => formats
                    .iter()
                    .any(|format| format.as_signed_integer() == Some(code)),
                other => other.as_signed_integer() == Some(code),
            }
        });
    if !takes_420v {
        return Err("Mspa lists no 420v".into());
    }
    let grid = AppleGrid {
        width: case.width,
        height: case.height,
        block,
    };
    let reference = caller
        .guest
        .lend(Arc::new(source_buffer(case.width, case.height, (0.0, 0.0))));
    let current = caller
        .guest
        .lend(Arc::new(source_buffer(case.width, case.height, case.shift)));
    let (emitted, signal) = mpsc::channel();
    *caller.guest.emitted_signal.lock() = Some(emitted);
    let started = StdRuntime.now();
    let (reply_id, frames) = caller.message(b"mepf", Some(caller.uuid), |message| {
        message.s64(REQUEST, 1);
        message.uuid_array(UUIDS, &[GuestUuid([0x51; 16]), GuestUuid([0x52; 16])]);
        message.u32(FLAGS, 0);
        let _ = message.property_list(OPTIONS, None);
    });
    let reply = caller.call_with(
        b"mepf",
        reply_id,
        with_surfaces(&[reference, current], &frames),
        FRAME_WAIT,
    )?;
    let status = caller.status(b"mepf", &reply)?;
    if status != 0 {
        return Err(format!("mepf answered {status}"));
    }
    signal
        .recv_timeout(EMIT_WAIT)
        .map_err(|_| "no Memt".to_owned())?;
    let elapsed = StdRuntime.elapsed_since(started);
    let (status, buffer) = caller
        .guest
        .emitted
        .lock()
        .first()
        .cloned()
        .ok_or("no Memt")?;
    if status != 0 {
        return Err(format!("Memt carried {status}"));
    }
    let buffer = buffer.ok_or("Memt named no vector buffer")?;
    if (buffer.layout.width, buffer.layout.height) != (grid.columns(), grid.rows()) {
        return Err(format!(
            "a {}x{} vector buffer for Apple's {}x{} grid",
            buffer.layout.width,
            buffer.layout.height,
            grid.columns(),
            grid.rows()
        ));
    }
    Ok(Outcome {
        columns: grid.columns(),
        rows: grid.rows(),
        picture_columns: case.width.div_ceil(case.block),
        picture_rows: case.height.div_ceil(case.block),
        vectors: read_vectors(&buffer),
        elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reims_vgpu_video_core::backend::{BackendError, PixelFormat};
    use reims_vgpu_video_core::motion::{
        MotionEstimationBackend, MotionEstimationCapabilities, MotionEstimator,
        MotionEstimatorConfig, MotionField, MotionPicture, MotionVector,
    };

    /// A block search of our own on the CPU, on 8x8 blocks, a size neither
    /// of Apple's grids has, so that the check runs through core's resample.
    struct BlockSearch;

    /// How far the search looks, each way.
    const REACH: i64 = 13;
    const BLOCK: u32 = 8;

    impl MotionEstimationBackend for BlockSearch {
        fn capabilities(&self) -> MotionEstimationCapabilities {
            MotionEstimationCapabilities {
                source_formats: vec![PixelFormat::Nv12VideoRange],
                max_width: 4096,
                max_height: 4096,
            }
        }

        fn open(
            &self,
            config: &MotionEstimatorConfig,
        ) -> Result<Box<dyn MotionEstimator>, BackendError> {
            Ok(Box::new(Search(*config)))
        }
    }

    struct Search(MotionEstimatorConfig);

    impl MotionEstimator for Search {
        fn estimate(
            &mut self,
            reference: &MotionPicture<'_>,
            current: &MotionPicture<'_>,
        ) -> Result<MotionField, BackendError> {
            let (width, height) = (i64::from(self.0.width), i64::from(self.0.height));
            let (reference, current) = (&reference.planes[0], &current.planes[0]);
            let at = |plane: &reims_vgpu_video_core::surfaces::PlaneSource<'_>, x: i64, y: i64| {
                let (x, y) = (x.clamp(0, width - 1), y.clamp(0, height - 1));
                i64::from(plane.bytes[y as usize * plane.bytes_per_row + x as usize])
            };
            let (columns, rows) = (self.0.width.div_ceil(BLOCK), self.0.height.div_ceil(BLOCK));
            let mut vectors = Vec::new();
            for row in 0..rows {
                for column in 0..columns {
                    let (left, top) = (i64::from(column * BLOCK), i64::from(row * BLOCK));
                    let mut best = (i64::MAX, MotionVector::default());
                    for dy in -REACH..=REACH {
                        for dx in -REACH..=REACH {
                            let mut sad = 0;
                            for y in top..top + i64::from(BLOCK) {
                                for x in left..left + i64::from(BLOCK) {
                                    sad +=
                                        (at(current, x, y) - at(reference, x + dx, y + dy)).abs();
                                }
                            }
                            if sad < best.0 {
                                best = (
                                    sad,
                                    MotionVector {
                                        x: dx as f32,
                                        y: dy as f32,
                                    },
                                );
                            }
                        }
                    }
                    vectors.push(best.1);
                }
            }
            Ok(MotionField {
                block_size: BLOCK,
                columns,
                rows,
                vectors,
            })
        }
    }

    fn backend() -> Backend {
        Backend {
            motion: Some(Arc::new(BlockSearch)),
            ..Backend::none("block search")
        }
    }

    /// The check passes a backend that finds the motion, on both of Apple's
    /// grids, through the host session.
    #[test]
    fn a_block_search_passes_on_both_grids() {
        for case in cases()
            .into_iter()
            .filter(|case| case.name == "b16-256x128-8,4" || case.name == "b4-256x128-3,-2")
        {
            let (passed, line) = check(&backend(), &case);
            assert!(passed, "{line}");
        }
    }

    /// A grid smaller than Apple's least has padding cells, all (0, 0).
    #[test]
    fn the_padding_of_a_small_picture_is_zero() -> Result<(), String> {
        let case = Case {
            name: "small".into(),
            width: 64,
            height: 32,
            block: 16,
            shift: (2.0, 1.0),
        };
        let outcome = run(&backend(), &case)?;
        assert_eq!((outcome.columns, outcome.rows), (10, 4));
        assert_eq!((outcome.picture_columns, outcome.picture_rows), (4, 2));
        assert_eq!(outcome.stray_padding(), 0);
        Ok(())
    }

    /// A backend with no motion part is refused at `meps`.
    #[test]
    fn a_backend_without_motion_estimation_fails() {
        let (passed, line) = check(&Backend::none("nothing"), &cases()[0]);
        assert!(!passed);
        assert!(line.contains("meps lists no processor"), "{line}");
    }

    #[test]
    fn every_case_has_its_own_name() {
        let names: Vec<String> = cases().into_iter().map(|case| case.name).collect();
        for (index, name) in names.iter().enumerate() {
            assert!(!names[..index].contains(name), "{name}");
        }
        assert!(names.contains(&"b16-256x128-8,4".to_owned()), "{names:?}");
    }
}
