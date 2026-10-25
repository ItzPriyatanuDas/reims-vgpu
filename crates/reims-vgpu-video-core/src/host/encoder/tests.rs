//! Encoder sessions end to end: a guest encoder that sends what the
//! guest's does, a device thread that delivers queue 0 in order as the
//! device does, and a stand-in encoder whose samples are bytes it read
//! from each frame.
//!
//! The guest sends what its encoder sends: `+enc`, the property calls
//! (`ecop`, `eset`, `emul`, `esav`, `esup`), `esta`, `efrm` with its
//! surface, `ecfr`, `einv` and the multipass and tile calls. It answers
//! `Epba`, `Efds` and `mpin`, and gathers `Eemt`'s fragments before it
//! reads one.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::backend::{BackendError, Codec, NoDecoders, PixelFormat};
use crate::encoder_backend::{
    BackendEncoder, EncodedFrame, EncoderBackend, EncoderCapabilities, EncoderConfig,
    EncoderSettings, EntropyMode, SourceFrame,
};
use crate::fourcc::FourCc;
use crate::host::test_queues::{CountingSink, Progress};
use crate::host::{HostSession, names};
use crate::runtime::StdRuntime;
use crate::status::{NO_ERROR, Status};
use crate::surfaces::{
    GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
};
use crate::sync::Mutex;
use crate::wire::WireError;
use crate::wire::image_description::{AVC_CONFIGURATION, ImageDescription};
use crate::wire::message::{
    GuestUuid, MediaTime, Message, MessageFlags, MessageHeader, MessageWriter, ValueType,
    read_header,
};
use crate::wire::property_list::as_i64;
use crate::wire::sample_buffer::{SampleBuffer, SerializedValue};
use crate::wire::surface_boxes::read_surface_release;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// The guest's buffers' rows: wider than the frame, so that a read at the
/// wrong stride shows.
const BYTES_PER_ROW: usize = 80;
const CHROMA_OFFSET: usize = 4096;
const AVCC: &[u8] = &[
    1, 77, 0, 30, 0xff, 0xe1, 0, 4, 0x67, 77, 0, 30, 1, 0, 2, 0x68, 0xce,
];
const OTHER_AVCC: &[u8] = &[1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 100, 1, 0, 1, 0x68];
/// A frame whose first luma sample is this encodes to a sample larger than
/// four `Eemt`s.
const LARGE: u8 = 0xee;
const LARGE_SAMPLE: usize = 60_000;
/// One whose first luma sample is this does not encode.
const BROKEN: u8 = 0xff;
/// One whose first luma sample is this the encoder drops, as VideoToolbox
/// drops every frame of a session one pixel wide or tall.
const DROPPED: u8 = 0xdd;
/// One whose first luma sample is this the encoder takes, then fails with
/// [`FAILED_STATUS`], as a VideoToolbox encoder fails a frame in its output
/// callback.
const FAILS: u8 = 0xfa;
/// The API's status a failed frame carries: `kVTAllocationFailedErr`.
const FAILED_STATUS: i32 = -12904;
/// The stand-in's status for a frame it was handed inside a lend (ours).
const ENCODED_IN_A_LEND: i32 = -1;
const WAIT: Duration = Duration::from_secs(5);
const NO_REPLY: Duration = Duration::from_millis(300);
const OUR_H264_ID: &str = "reims-vgpu-video.videoencoder.avc";

thread_local! {
    /// The lends of guest surfaces this thread is inside now: reims-vgpu
    /// lends one surface at a time in the process, so a backend call inside
    /// one would hold every other lend.
    static LENDS: Cell<usize> = const { Cell::new(0) };
}

/// Runs `lend` as a lend of a surface's memory, counted in [`LENDS`].
fn lent<T>(lend: impl FnOnce() -> T) -> T {
    LENDS.with(|lends| lends.set(lends.get() + 1));
    let result = lend();
    LENDS.with(|lends| lends.set(lends.get() - 1));
    result
}

/// The stand-in encoder's backend: H.264 from `420v`. Each frame's sample
/// is one NAL unit: a slice's header byte (IDR for a sync frame), then the
/// luma at the first and last pixel and the second row's first, each at the
/// stride the backend is given, and the first chroma sample. When `hold_back` is set it keeps
/// every frame until it is flushed, as Apple's software H.264 encoder did
/// (measured on macOS 27, 26A434, 2026-10-24).
#[derive(Default)]
struct StandIn {
    hold_back: AtomicBool,
    /// While set, the encoders are listed but none can be made, as when
    /// every hardware encoder session of a Mac is taken.
    taken: AtomicBool,
    configuration: Arc<Mutex<Vec<u8>>>,
    opened: Mutex<Vec<EncoderConfig>>,
    reconfigured: Arc<Mutex<Vec<EncoderSettings>>>,
    /// How many times the guest's `epfr` prepared an encoder.
    prepared: Arc<AtomicU64>,
    /// While set, an encoder's prepare fails with [`FAILED_STATUS`].
    prepare_fails: Arc<AtomicBool>,
}

impl StandIn {
    fn new() -> Self {
        let backend = Self::default();
        *backend.configuration.lock() = AVCC.to_vec();
        backend
    }
}

impl EncoderBackend for StandIn {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        vec![EncoderCapabilities {
            codec: Codec::H264,
            profiles: vec![66, 77, 100],
            max_width: 4096,
            max_height: 4096,
            source_formats: vec![PixelFormat::Nv12VideoRange],
            frame_reordering: false,
        }]
    }

    /// -12912 while `taken`, as Apple's AVE answers `+enc` then.
    fn session_capabilities(&self, codec: Codec) -> Result<Option<EncoderCapabilities>, i32> {
        if self.taken.load(Ordering::SeqCst) {
            return Err(Status::VideoEncoderMalfunction.code());
        }
        Ok(self
            .encoders()
            .into_iter()
            .find(|listed| listed.codec == codec))
    }

    /// A `Quality` above 0.9 is refused, with the status a backend chose.
    fn check_property(&self, codec: Codec, key: &str, value: &plist::Value) -> Result<(), i32> {
        let too_good = key == "Quality" && value.as_real().is_some_and(|quality| quality > 0.9);
        if codec == Codec::H264 && too_good {
            return Err(Status::Parameter.code());
        }
        Ok(())
    }

    /// A width of 2000 fails with an API status of its own, as
    /// VideoToolbox's `VTCompressionSessionCreate` answers -12903 for a
    /// size AVE does not take.
    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        self.opened.lock().push(config.clone());
        if config.width == 2000 {
            return Err(BackendError::Native {
                status: Status::InvalidSession.code(),
                message: "the API refused the size".into(),
            });
        }
        Ok(Box::new(StandInEncoder {
            hold_back: self.hold_back.load(Ordering::SeqCst),
            configuration: self.configuration.clone(),
            reconfigured: self.reconfigured.clone(),
            prepared: self.prepared.clone(),
            prepare_fails: self.prepare_fails.clone(),
            held: Vec::new(),
            frames: 0,
        }))
    }
}

struct StandInEncoder {
    hold_back: bool,
    configuration: Arc<Mutex<Vec<u8>>>,
    reconfigured: Arc<Mutex<Vec<EncoderSettings>>>,
    prepared: Arc<AtomicU64>,
    prepare_fails: Arc<AtomicBool>,
    held: Vec<EncodedFrame>,
    frames: u32,
}

impl BackendEncoder for StandInEncoder {
    fn configuration(&self) -> Vec<u8> {
        self.configuration.lock().clone()
    }

    /// Refuses a frame handed to it inside a lend, so that every test
    /// that encodes checks the session holds none.
    fn encode(&mut self, frame: &SourceFrame<'_>) -> Result<Vec<EncodedFrame>, BackendError> {
        if LENDS.with(Cell::get) != 0 {
            return Err(BackendError::Native {
                status: ENCODED_IN_A_LEND,
                message: "encoded inside a lend".into(),
            });
        }
        let [luma, chroma] = frame.planes else {
            return Err(BackendError::BadData("not two planes".into()));
        };
        let width = usize::try_from(frame.width).unwrap_or(0);
        let rows = usize::try_from(frame.height).unwrap_or(0);
        let last = (rows - 1) * luma.bytes_per_row + width - 1;
        let first = luma.bytes[0];
        if first == BROKEN {
            // The frames held with it are lost, as the encode driver loses
            // the run a failed picture is in.
            self.held.clear();
            return Err(BackendError::BadData("a broken picture".into()));
        }
        if first == DROPPED {
            return Ok(vec![EncodedFrame::dropped(frame.frame_id)]);
        }
        if first == FAILS {
            // Taken, and failed with its own error, in its place among the
            // frames held with it.
            let failed = EncodedFrame::failed(
                frame.frame_id,
                BackendError::Native {
                    status: FAILED_STATUS,
                    message: "the encoder's output for this frame".into(),
                },
            );
            if self.hold_back {
                self.held.push(failed);
                return Ok(Vec::new());
            }
            return Ok(vec![failed]);
        }
        let sync = self.frames == 0 || frame.force_key_frame;
        self.frames += 1;
        let mut unit = vec![
            if sync { 0x65 } else { 0x41 },
            first,
            luma.bytes[last],
            luma.bytes[luma.bytes_per_row],
            chroma.bytes[0],
        ];
        if first == LARGE {
            unit.resize(LARGE_SAMPLE, LARGE);
        }
        let encoded = EncodedFrame {
            frame_id: frame.frame_id,
            // One NAL unit after its 4-byte length.
            data: [&u32::try_from(unit.len()).unwrap().to_be_bytes()[..], &unit].concat(),
            presentation_time: frame.presentation_time,
            decode_time: MediaTime::INVALID,
            duration: frame.duration,
            sync,
            depends_on_others: !sync,
            depended_on_by_others: true,
            error: None,
        };
        if self.hold_back {
            self.held.push(encoded);
            return Ok(Vec::new());
        }
        Ok(vec![encoded])
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>, BackendError> {
        Ok(std::mem::take(&mut self.held))
    }

    /// CAVLC cannot be had once it runs.
    fn reconfigure(&mut self, settings: &EncoderSettings) -> Result<(), BackendError> {
        if settings.entropy == Some(EntropyMode::Cavlc) {
            return Err(BackendError::Unsupported("CAVLC while running".into()));
        }
        self.reconfigured.lock().push(settings.clone());
        Ok(())
    }

    /// Counted, and failed with the API's status while `prepare_fails`.
    fn prepare(&mut self) -> Result<(), BackendError> {
        if self.prepare_fails.load(Ordering::SeqCst) {
            return Err(BackendError::Native {
                status: FAILED_STATUS,
                message: "the encoder did not prepare".into(),
            });
        }
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// What runs when one of the host's holds on a surface ends.
type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

/// The guest's IOSurfaces, as the host's mapper reaches them: memory by
/// mapping ID, the holds the host has on each, and what runs when a hold
/// ends.
#[derive(Default)]
struct GuestMemory {
    surfaces: Mutex<HashMap<u64, Arc<SurfaceMemory>>>,
    holds: Arc<Mutex<HashMap<u64, usize>>>,
    on_release: Arc<Mutex<Option<ReleaseHook>>>,
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
}

impl GuestSurface for HeldSurface {
    fn layout(&self) -> SurfaceLayout {
        self.memory.layout.clone()
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        lent(|| write(&mut self.memory.bytes.lock()));
        Ok(())
    }

    fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
        lent(|| read(&self.memory.bytes.lock()));
        Ok(())
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
        }))
    }
}

impl GuestMemory {
    fn held(&self) -> usize {
        self.holds.lock().values().sum()
    }
}

/// An encoded frame as the guest's encoder got it.
#[derive(Clone, Debug)]
struct Emitted {
    frame_id: i64,
    status: i32,
    info: u32,
    uuid: Option<GuestUuid>,
    sample: Option<SampleBuffer>,
}

/// What the guest's side keeps.
#[derive(Default)]
struct GuestState {
    /// The host's messages in arrival order, as `Epba 0x2`, `Eemt 3`,
    /// `rels [1]`, `reply efrm`.
    seen: Mutex<Vec<String>>,
    attributes: Mutex<Option<plist::Dictionary>>,
    /// Each `Efds`'s codec, size and extensions.
    descriptions: Mutex<Vec<(i32, i32, i32, plist::Dictionary)>>,
    emitted: Mutex<Vec<Emitted>>,
    /// The fragments of the `Eemt` being gathered.
    partial: Mutex<Vec<u8>>,
    /// Each `Eemt` message's length, flags and reply ID.
    emit_messages: Mutex<Vec<(usize, u32, u64)>>,
    waiting: Mutex<HashMap<u64, Sender<Vec<u8>>>>,
}

/// The guest encoder, its handler thread, and the device's queue-0 thread.
struct Guest {
    uuid: GuestUuid,
    state: Arc<GuestState>,
    memory: Arc<GuestMemory>,
    backend: Arc<StandIn>,
    session: Arc<HostSession>,
    queue0: Option<Sender<Vec<u8>>>,
    next_reply_id: AtomicU64,
    next_surface: AtomicU64,
    /// How far the queues have got.
    progress: Arc<Progress>,
    threads: Vec<JoinHandle<()>>,
}

impl Guest {
    fn new(backend: StandIn) -> Self {
        let memory = Arc::new(GuestMemory::default());
        let backend = Arc::new(backend);
        let (to_guest, from_host) = mpsc::channel::<Vec<u8>>();
        let progress = Arc::new(Progress::default());
        let session = Arc::new(HostSession::with_encoders(
            StdRuntime::shared(),
            Arc::new(CountingSink::new(to_guest, progress.clone())),
            memory.clone(),
            Arc::new(NoDecoders),
            backend.clone(),
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
        let handler = {
            let state = state.clone();
            let queue0 = queue0.clone();
            let progress = progress.clone();
            StdRuntime::thread("test-guest", move || {
                while let Ok(message) = from_host.recv() {
                    handle_host_message(&state, &queue0, &message);
                    progress.handled_one();
                }
            })
            .expect("a thread")
        };
        Self {
            uuid: GuestUuid([0x3e; 16]),
            state,
            memory,
            backend,
            session,
            queue0: Some(queue0),
            next_reply_id: AtomicU64::new(100),
            next_surface: AtomicU64::new(1),
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

    /// A message of the guest's session, flag 0x1 and the next reply ID,
    /// with `parameters`.
    fn message(
        &self,
        message_type: &[u8; 4],
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> (u64, Vec<u8>) {
        let reply_id = self.next_reply_id.fetch_add(1, Ordering::SeqCst);
        let mut message = MessageWriter::new(
            FourCc::new(*message_type),
            MessageFlags::GUEST_EXPECTS_REPLY,
            Some(self.uuid),
            reply_id,
        );
        parameters(&mut message);
        (reply_id, message.finish())
    }

    /// `VTParavirtualizationGuestSendMessageToHostAndCopyReplySync`.
    fn call(
        &self,
        message_type: &[u8; 4],
        timeout: Duration,
        parameters: impl FnOnce(&mut MessageWriter),
    ) -> Option<Vec<u8>> {
        let (reply_id, message) = self.message(message_type, parameters);
        self.call_with(reply_id, message, timeout)
    }

    fn call_with(&self, reply_id: u64, message: Vec<u8>, timeout: Duration) -> Option<Vec<u8>> {
        let (arrived, reply) = mpsc::channel();
        self.state.waiting.lock().insert(reply_id, arrived);
        self.send(message);
        let answer = reply.recv_timeout(timeout).ok();
        self.state.waiting.lock().remove(&reply_id);
        answer
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

    /// `+enc` for `codec`, with `,cid` when given: the reply's status.
    fn create(&self, codec: &[u8; 4], encoder_id: Option<&str>) -> Result<i32, WireError> {
        let reply = self
            .call(b"+enc", WAIT, |message| {
                message.s32(names::CODEC, i32::from_be_bytes(*codec));
                message.u32(names::VERSION, 6);
                if let Some(id) = encoder_id {
                    let _ = message
                        .property_list(names::CODEC_ID, Some(&plist::Value::String(id.to_owned())));
                }
                let _ = message.property_list(names::GUEST_INFO, None);
            })
            .ok_or(WireError::Malformed("no +enc reply"))?;
        let reply = Message::parse(&reply)?;
        assert_eq!(reply.u32(names::VERSION)?, Some(6));
        let build = reply
            .dictionary(names::HOST_INFO)?
            .and_then(|info| info.get("VTHostBuildVersion").cloned());
        assert!(matches!(build, Some(plist::Value::String(_))));
        Ok(reply.s32(names::ERROR)?.unwrap_or(-1))
    }

    /// `esta` for a picture of `width` by `height`.
    fn start(&self, width: i32, height: i32) -> Result<Option<i32>, WireError> {
        self.status_of(b"esta", WAIT, |message| {
            message.i32_pair(names::DIMENSIONS, ValueType::Dimensions, (width, height));
        })
    }

    /// A new `420v` buffer, filled with `luma` and `chroma`, the rows'
    /// padding left zero: its mapping ID.
    fn surface(&self, luma: u8, chroma: u8) -> u64 {
        let mapping_id = self.next_surface.fetch_add(1, Ordering::SeqCst);
        let surface = nv12_surface();
        {
            let mut bytes = surface.bytes.lock();
            let width = usize::try_from(WIDTH).unwrap_or(0);
            for row in 0..usize::try_from(HEIGHT).unwrap_or(0) {
                let start = row * BYTES_PER_ROW;
                bytes[start..start + width].fill(luma);
            }
            for row in 0..usize::try_from(HEIGHT / 2).unwrap_or(0) {
                let start = CHROMA_OFFSET + row * BYTES_PER_ROW;
                bytes[start..start + width].fill(chroma);
            }
        }
        self.memory
            .surfaces
            .lock()
            .insert(mapping_id, Arc::new(surface));
        mapping_id
    }

    /// `efrm` for frame `frame_id` at `frame_id`/30 s, lending `surface`
    /// when given: the reply's status and info flags.
    fn encode(
        &self,
        frame_id: i64,
        surface: Option<u64>,
        force_key_frame: bool,
    ) -> Result<Option<(i32, u32)>, WireError> {
        let (reply_id, message) = self.message(b"efrm", |message| {
            message.s64(names::FRAME, frame_id);
            message.time(names::PRESENTATION_TIME, time(frame_id));
            message.time(names::DURATION, time(1));
            if force_key_frame {
                let _ = message.property_list(
                    names::FRAME_PROPERTIES,
                    Some(&plist::Value::Dictionary(
                        [(
                            "EncoderForceKeyframe".to_owned(),
                            plist::Value::Boolean(true),
                        )]
                        .into_iter()
                        .collect(),
                    )),
                );
            }
            message.uuid(names::PIXEL_BUFFER_UUID, frame_uuid(frame_id));
        });
        let message = match surface {
            Some(mapping_id) => with_surface(mapping_id, &message),
            None => message,
        };
        let timeout = if surface.is_some() { WAIT } else { NO_REPLY };
        let Some(reply) = self.call_with(reply_id, message, timeout) else {
            return Ok(None);
        };
        let reply = Message::parse(&reply)?;
        Ok(Some((
            reply.s32(names::ERROR)?.unwrap_or(-1),
            reply.u32(names::INFO_FLAGS)?.unwrap_or(u32::MAX),
        )))
    }

    /// `ecfr`, to the end of the stream.
    fn complete(&self) -> Result<Option<i32>, WireError> {
        self.status_of(b"ecfr", WAIT, |message| {
            message.time(names::PRESENTATION_TIME, MediaTime::INVALID);
        })
    }

    /// `ecop`: the reply's status and value.
    fn copy_property(&self, key: &str) -> Result<(i32, Option<plist::Value>), WireError> {
        let reply = self
            .call(b"ecop", WAIT, |message| {
                let _ = message
                    .property_list(names::PROPERTY_KEY, Some(&plist::Value::String(key.into())));
            })
            .ok_or(WireError::Malformed("no ecop reply"))?;
        let reply = Message::parse(&reply)?;
        Ok((
            reply.s32(names::ERROR)?.unwrap_or(-1),
            reply.property_list(names::PROPERTY_VALUE)?,
        ))
    }

    /// `eset`: the reply's status, or None with no reply.
    fn set_property(&self, key: &str, value: plist::Value) -> Result<Option<i32>, WireError> {
        self.status_of(b"eset", NO_REPLY.max(WAIT / 10), |message| {
            let _ =
                message.property_list(names::PROPERTY_KEY, Some(&plist::Value::String(key.into())));
            let _ = message.property_list(names::PROPERTY_VALUE, Some(&value));
        })
    }

    /// The encoder's `Finalize`: `-enc`, sent without waiting.
    fn close(&self) {
        self.send(
            MessageWriter::new(
                FourCc::new(*b"-enc"),
                MessageFlags::NONE,
                Some(self.uuid),
                0,
            )
            .finish(),
        );
    }

    /// Waits until the session's thread has handled everything sent so
    /// far, its sweeps included, and the device thread has delivered it: an
    /// `epfr` round trip, which the session's thread answers in turn, then
    /// an `encs` one.
    fn settle(&self) {
        let _ = self.call(b"epfr", WAIT, |_| {});
        self.state.seen.lock().retain(|seen| seen != "reply epfr");
        self.settle_device();
    }

    /// Waits until the device thread has delivered everything sent so far
    /// and the guest has handled what came back: an `encs` round trip, and
    /// the end of its delivery, whose sweep sends its `rels` after the
    /// reply.
    fn settle_device(&self) {
        let reply_id = self.next_reply_id.fetch_add(1, Ordering::SeqCst);
        let mut encs = MessageWriter::new(
            FourCc::new(*b"encs"),
            MessageFlags::GUEST_EXPECTS_REPLY,
            None,
            reply_id,
        );
        encs.u32(names::VERSION, 6);
        let _ = self.call_with(reply_id, encs.finish(), WAIT);
        let _ = self.progress.settled(reply_id, WAIT);
        self.state.seen.lock().retain(|seen| seen != "reply encs");
    }
}

impl Guest {
    /// Until the guard is dropped, the host session sweeps the moment one
    /// of its holds on a surface ends, as a delivery on the device's thread
    /// may at any moment: each delivery ends with the sweep. The delivery
    /// is a message for no session, which gets no reply.
    fn sweep_whenever_a_hold_ends(&self) -> SweepWheneverAHoldEnds {
        let session = Arc::downgrade(&self.session);
        let stray = MessageWriter::new(
            FourCc::new(*b"efrm"),
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

/// Takes the sweep out when dropped, before the guest's teardown, whose
/// invalidation lets holds go under the session tables' locks.
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

/// The guest's handler for the host's messages: replies go to their
/// waiters, and the encoder session's requests are answered.
fn handle_host_message(state: &GuestState, queue0: &Sender<Vec<u8>>, message: &[u8]) {
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
    if kind == "Eemt" {
        state
            .emit_messages
            .lock()
            .push((message.len(), header.flags.0, header.reply_id));
        state.partial.lock().extend_from_slice(message);
        if header.flags.contains(MessageFlags::MORE_FRAGMENTS) {
            return;
        }
        let whole = std::mem::take(&mut *state.partial.lock());
        emitted(state, queue0, &header, &whole);
        return;
    }
    let Ok(parsed) = Message::parse(message) else {
        state.seen.lock().push(format!("{kind} unparsable"));
        return;
    };
    match &header.message_type.to_be_bytes() {
        b"Epba" => {
            state
                .seen
                .lock()
                .push(format!("{kind} {:#x}", header.flags.0));
            *state.attributes.lock() = parsed
                .dictionary(names::PIXEL_BUFFER_ATTRIBUTES)
                .ok()
                .flatten();
            if header.flags.contains(MessageFlags::HOST_EXPECTS_REPLY) {
                answer(queue0, &header, NO_ERROR, |_| {});
            }
        }
        b"Efds" => {
            state.seen.lock().push(kind);
            let read = |key| parsed.s32(key).ok().flatten().unwrap_or(0);
            let (codec, width, height) =
                (read(names::CODEC), read(names::WIDTH), read(names::HEIGHT));
            let extensions = parsed
                .dictionary(names::EXTENSIONS)
                .ok()
                .flatten()
                .unwrap_or_default();
            let record = extensions
                .get("SampleDescriptionExtensionAtoms")
                .and_then(plist::Value::as_dictionary)
                .and_then(|atoms| atoms.get("avcC"))
                .and_then(plist::Value::as_data)
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            let name = extensions
                .get("FormatName")
                .and_then(plist::Value::as_string)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            state
                .descriptions
                .lock()
                .push((codec, width, height, extensions));
            // `VTEncoderSessionCreateVideoFormatDescription`, then the
            // description as an ImageDescription.
            let description = ImageDescription {
                codec: FourCc(codec.cast_unsigned()),
                width: u16::try_from(width).unwrap_or(0),
                height: u16::try_from(height).unwrap_or(0),
                depth: 24,
                compressor_name: name,
                extensions: vec![(AVC_CONFIGURATION, record)],
            }
            .to_bytes();
            answer(queue0, &header, NO_ERROR, |reply| {
                reply.image_description(names::CREATED_FORMAT_DESCRIPTION, &description);
            });
        }
        b"mpin" => {
            state
                .seen
                .lock()
                .push(format!("{kind} {:#x}", header.flags.0));
            answer(queue0, &header, NO_ERROR, |_| {});
        }
        _ => state.seen.lock().push(kind),
    }
}

/// `Eemt`, gathered: what it carries, and the reply.
fn emitted(state: &GuestState, queue0: &Sender<Vec<u8>>, last: &MessageHeader, whole: &[u8]) {
    let Ok(parsed) = Message::parse(whole) else {
        state.seen.lock().push("Eemt unparsable".into());
        return;
    };
    let frame_id = parsed.s64(names::FRAME).ok().flatten().unwrap_or(-1);
    state.seen.lock().push(format!("Eemt {frame_id}"));
    let sample = parsed
        .sample_buffer(names::SAMPLE_BUFFER)
        .ok()
        .flatten()
        .and_then(|serialization| SampleBuffer::parse(&serialization).ok());
    state.emitted.lock().push(Emitted {
        frame_id,
        status: parsed.s32(names::ERROR).ok().flatten().unwrap_or(-1),
        info: parsed
            .u32(names::ENCODE_INFO_FLAGS)
            .ok()
            .flatten()
            .unwrap_or(u32::MAX),
        uuid: parsed.uuid(names::PIXEL_BUFFER_UUID).ok().flatten(),
        sample,
    });
    // The reply answers the last fragment.
    answer(queue0, last, NO_ERROR, |_| {});
}

fn answer(
    queue0: &Sender<Vec<u8>>,
    request: &MessageHeader,
    status: i32,
    extra: impl FnOnce(&mut MessageWriter),
) {
    let mut reply = MessageWriter::reply_to(request, MessageFlags::REPLY_TO_HOST);
    reply.s32(names::ERROR, status);
    extra(&mut reply);
    let _ = queue0.send(reply.finish());
}

/// A 64x48 `420v` buffer, both planes 80 bytes a row, chroma at 4096.
fn nv12_surface() -> SurfaceMemory {
    SurfaceMemory {
        layout: SurfaceLayout {
            pixel_format: FourCc::new(*b"420v"),
            width: WIDTH,
            height: HEIGHT,
            planes: vec![
                PlaneLayout {
                    offset: 0,
                    bytes_per_row: BYTES_PER_ROW,
                    width: WIDTH,
                    height: HEIGHT,
                },
                PlaneLayout {
                    offset: CHROMA_OFFSET,
                    bytes_per_row: BYTES_PER_ROW,
                    width: WIDTH / 2,
                    height: HEIGHT / 2,
                },
            ],
            bytes_per_row: 0,
        },
        bytes: Mutex::new(vec![0; CHROMA_OFFSET + BYTES_PER_ROW * 24]),
    }
}

/// The guest kext's `subs` box for one surface, then the message.
fn with_surface(mapping_id: u64, message: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x30];
    bytes.extend_from_slice(b"subs");
    bytes.extend_from_slice(&mapping_id.to_be_bytes());
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(message);
    bytes
}

/// `frames`/30 of a second.
fn time(frames: i64) -> MediaTime {
    MediaTime {
        value: frames,
        timescale: 30,
        flags: MediaTime::FLAG_VALID,
        epoch: 0,
    }
}

/// The UUID the guest keeps frame `frame_id` under.
fn frame_uuid(frame_id: i64) -> GuestUuid {
    let mut uuid = [0xf0; 16];
    uuid[8..].copy_from_slice(&frame_id.to_be_bytes());
    GuestUuid(uuid)
}

fn started(backend: StandIn) -> Result<Guest, WireError> {
    let guest = Guest::new(backend);
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    guest.seen();
    Ok(guest)
}

/// The value of `key` in a serialized dictionary.
fn entry<'a>(entries: &'a [(String, SerializedValue)], key: &str) -> Option<&'a SerializedValue> {
    entries
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn boolean(value: bool) -> SerializedValue {
    SerializedValue::PropertyList(plist::Value::Boolean(value))
}

/// A sample's attachments, as `(key, value)`.
fn attachments(sample: &SampleBuffer) -> Vec<(String, SerializedValue)> {
    match sample.sample_attachments.as_deref() {
        Some([SerializedValue::Dictionary(entries)]) => entries.clone(),
        _ => Vec::new(),
    }
}

#[test]
fn opening_takes_this_hosts_id_apples_and_none() -> Result<(), WireError> {
    for id in [
        Some(OUR_H264_ID),
        Some("com.apple.videotoolbox.videoencoder.ave.avc"),
        None,
    ] {
        let guest = Guest::new(StandIn::new());
        assert_eq!(guest.create(b"avc1", id)?, NO_ERROR, "{id:?}");
        assert_eq!(guest.seen(), ["reply +enc"]);
    }
    Ok(())
}

#[test]
fn an_encoder_the_host_lacks_is_refused_in_its_reply() -> Result<(), WireError> {
    for (codec, id) in [
        (b"avc1", Some("com.example.unknown")),
        (
            b"avc1",
            Some("com.apple.videotoolbox.videoencoder.ave.hevc"),
        ),
        (b"hvc1", None),
    ] {
        let guest = Guest::new(StandIn::new());
        assert_eq!(
            guest.create(codec, id)?,
            Status::CouldNotFindVideoEncoder.code(),
            "{id:?}"
        );
        assert!(guest.backend.opened.lock().is_empty());
    }
    Ok(())
}

/// While the backend can make no encoder of a codec it lists, `encs` still
/// lists it and `+enc` answers the backend's status, as Apple's host lists
/// AVE and answers -12912 while every hardware encoder session of the Mac
/// is taken (this Mac, macOS 27.0.1, 2026-10-24). The session has no
/// encoder and answers nothing, and once an encoder can be made a new one
/// opens.
#[test]
fn a_listed_encoder_that_cannot_be_made_now_is_refused_with_its_status() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    guest.backend.taken.store(true, Ordering::SeqCst);
    let reply_id = guest.next_reply_id.fetch_add(1, Ordering::SeqCst);
    let mut encs = MessageWriter::new(
        FourCc::new(*b"encs"),
        MessageFlags::GUEST_EXPECTS_REPLY,
        None,
        reply_id,
    );
    encs.u32(names::VERSION, 6);
    let reply = guest
        .call_with(reply_id, encs.finish(), WAIT)
        .ok_or(WireError::Malformed("no encs reply"))?;
    let listed = Message::parse(&reply)?.property_list(names::ENCODERS)?;
    assert!(matches!(listed, Some(plist::Value::Array(entries)) if entries.len() == 1));
    assert_eq!(
        guest.create(b"avc1", Some(OUR_H264_ID))?,
        Status::VideoEncoderMalfunction.code()
    );
    assert!(guest.call(b"esup", NO_REPLY, |_| {}).is_none());
    let start = guest.call(b"esta", NO_REPLY, |message| {
        message.i32_pair(names::DIMENSIONS, ValueType::Dimensions, (64, 48));
    });
    assert!(start.is_none());
    assert!(guest.backend.opened.lock().is_empty());
    guest.close();
    guest.backend.taken.store(false, Ordering::SeqCst);
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    assert_eq!(guest.backend.opened.lock().len(), 1);
    Ok(())
}

#[test]
fn a_create_without_a_codec_gets_no_reply() {
    let guest = Guest::new(StandIn::new());
    let reply = guest.call(b"+enc", NO_REPLY, |message| {
        message.u32(names::VERSION, 6);
    });
    assert!(reply.is_none());
}

#[test]
fn properties_answer_as_apples_encoders_do() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    let reply = guest
        .call(b"esup", WAIT, |_| {})
        .ok_or(WireError::Malformed("no esup reply"))?;
    let reply = Message::parse(&reply)?;
    assert_eq!(reply.s32(names::ERROR)?, Some(NO_ERROR));
    let supported = reply
        .dictionary(names::SUPPORTED_PROPERTIES)?
        .unwrap_or_default();
    for key in [
        "AverageBitRate",
        "ProfileLevel",
        "UsingHardwareAcceleratedVideoEncoder",
        "H264EntropyMode",
    ] {
        assert!(supported.contains_key(key), "{key}");
    }
    assert_eq!(
        guest.copy_property("UsingHardwareAcceleratedVideoEncoder")?,
        (NO_ERROR, Some(plist::Value::Boolean(true)))
    );
    assert_eq!(
        guest.copy_property("NoSuchProperty")?.0,
        Status::PropertyNotSupported.code()
    );
    assert_eq!(
        guest.set_property("AverageBitRate", plist::Value::Integer(1_500_000.into()))?,
        Some(NO_ERROR)
    );
    assert_eq!(
        guest
            .copy_property("AverageBitRate")?
            .1
            .as_ref()
            .and_then(as_i64),
        Some(1_500_000)
    );
    assert_eq!(
        guest.set_property(
            "UsingHardwareAcceleratedVideoEncoder",
            plist::Value::Boolean(false)
        )?,
        Some(Status::PropertyReadOnly.code())
    );
    assert_eq!(
        guest.set_property("AllowFrameReordering", plist::Value::Integer(1.into()))?,
        Some(Status::PropertyNotSupported.code())
    );
    // The host keeps Paravirtualized: no reply.
    assert_eq!(
        guest.set_property("Paravirtualized", plist::Value::Boolean(true))?,
        None
    );
    let set_many = |properties: Vec<(&str, plist::Value)>| {
        guest.status_of(b"emul", NO_REPLY, |message| {
            let _ = message.property_list(
                names::PROPERTIES,
                Some(&plist::Value::Dictionary(
                    properties
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value))
                        .collect(),
                )),
            );
        })
    };
    assert_eq!(
        set_many(vec![
            ("RealTime", plist::Value::Boolean(true)),
            ("Paravirtualized", plist::Value::Boolean(true)),
        ])?,
        Some(NO_ERROR)
    );
    assert_eq!(
        set_many(vec![("Paravirtualized", plist::Value::Boolean(true))])?,
        None
    );
    assert_eq!(
        guest.status_of(b"esav", WAIT, |_| {})?,
        Some(Status::UnimplementedFunction.code())
    );
    Ok(())
}

#[test]
fn starting_asks_for_source_buffers_before_replying() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    assert_eq!(
        guest.set_property("AverageBitRate", plist::Value::Integer(800_000.into()))?,
        Some(NO_ERROR)
    );
    guest.seen();
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    // A guest of protocol 6 is asked for its reply.
    assert_eq!(guest.seen(), ["Epba 0x2", "reply esta"]);
    let attributes = guest.state.attributes.lock().clone().unwrap_or_default();
    assert_eq!(
        attributes.get("PixelFormatType"),
        Some(&plist::Value::Array(vec![plist::Value::Integer(
            i64::from(u32::from_be_bytes(*b"420v")).into()
        )]))
    );
    assert_eq!(attributes.get("Width").and_then(as_i64), Some(64));
    assert_eq!(attributes.get("Height").and_then(as_i64), Some(48));
    assert_eq!(
        attributes.get("BytesPerRowAlignment").and_then(as_i64),
        Some(64)
    );
    let opened = guest.backend.opened.lock().clone();
    assert_eq!(opened.len(), 1);
    assert_eq!((opened[0].width, opened[0].height), (64, 48));
    assert_eq!(opened[0].settings.average_bit_rate, Some(800_000));
    Ok(())
}

#[test]
fn starting_with_no_size_is_malformed() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", None)?, NO_ERROR);
    guest.seen();
    assert_eq!(guest.start(0, 48)?, Some(Status::MalformedMessage.code()));
    assert_eq!(guest.seen(), ["reply esta"]);
    assert!(guest.backend.opened.lock().is_empty());
    Ok(())
}

/// A size past the encoder's is answered as Apple's host answers a size
/// AVE does not take, with no `Epba`, and the backend is not asked.
#[test]
fn a_size_past_the_encoders_is_refused_as_apples_host_refuses_it() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", None)?, NO_ERROR);
    guest.seen();
    assert_eq!(
        guest.start(4097, 48)?,
        Some(Status::EncoderSizeUnsupported.code())
    );
    assert_eq!(guest.seen(), ["reply esta"]);
    assert!(guest.backend.opened.lock().is_empty());
    Ok(())
}

/// A backend's own API status reaches the guest as it is, as Apple's host
/// sends its encoder's, with no `Epba` before it.
#[test]
fn an_api_status_reaches_the_guest_as_it_is() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", None)?, NO_ERROR);
    guest.seen();
    assert_eq!(guest.start(2000, 48)?, Some(Status::InvalidSession.code()));
    assert_eq!(guest.seen(), ["reply esta"]);
    Ok(())
}

/// An odd size is taken as Apple's AVE takes it: the source buffers keep
/// it, and the stream is the frames' top left at the even size below it.
#[test]
fn an_odd_size_encodes_its_top_left_at_the_even_size_below() -> Result<(), WireError> {
    const MARK: u8 = 0xe1;
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    guest.seen();
    assert_eq!(guest.start(63, 47)?, Some(NO_ERROR));
    assert_eq!(guest.seen(), ["Epba 0x2", "reply esta"]);
    let attributes = guest.state.attributes.lock().clone().unwrap_or_default();
    assert_eq!(attributes.get("Width").and_then(as_i64), Some(63));
    assert_eq!(attributes.get("Height").and_then(as_i64), Some(47));
    let opened = guest.backend.opened.lock().clone();
    assert_eq!((opened[0].width, opened[0].height), (62, 46));

    // A 63x47 buffer: luma 0x51, but its last column and row MARK.
    let surface = nv12_surface();
    {
        let mut bytes = surface.bytes.lock();
        for row in 0..47 {
            let start = row * BYTES_PER_ROW;
            bytes[start..start + 63].fill(if row == 46 { MARK } else { 0x51 });
            bytes[start + 62] = MARK;
        }
        for row in 0..24 {
            let start = CHROMA_OFFSET + row * BYTES_PER_ROW;
            bytes[start..start + 64].fill(0x82);
        }
    }
    let mut layout = surface.layout.clone();
    (layout.width, layout.height) = (63, 47);
    (layout.planes[0].width, layout.planes[0].height) = (63, 47);
    let mapping_id = guest.next_surface.fetch_add(1, Ordering::SeqCst);
    guest
        .memory
        .surfaces
        .lock()
        .insert(mapping_id, Arc::new(SurfaceMemory { layout, ..surface }));
    assert_eq!(
        guest.encode(1, Some(mapping_id), false)?,
        Some((NO_ERROR, 0))
    );
    guest.settle();
    assert_eq!(guest.seen(), ["Efds", "Eemt 1", "reply efrm", "rels [1]"]);
    let descriptions = guest.state.descriptions.lock().clone();
    assert_eq!((descriptions[0].1, descriptions[0].2), (62, 46));
    // The stream's last pixel is row 45's column 61: the source's last
    // column and row, MARK, are left out.
    let emitted = guest.emitted();
    let Some(sample) = &emitted[0].sample else {
        panic!("frame 1 has its sample");
    };
    assert_eq!(
        sample.data.as_deref(),
        Some(&[0, 0, 0, 5, 0x65, 0x51, 0x51, 0x51, 0x82][..])
    );
    // A frame of another size is refused, the even one included.
    let even = guest.surface(0x51, 0x82);
    assert_eq!(
        guest.encode(2, Some(even), false)?,
        Some((Status::Parameter.code(), 0))
    );
    // Its `rels` follows the reply, from the session's thread.
    guest.settle();
    assert_eq!(guest.seen(), ["reply efrm", "rels [2]"]);
    // A side of 1 has no even size: refused with no `Epba`, the backend
    // not asked.
    for (width, height) in [(1, 47), (63, 1), (1, 1)] {
        assert_eq!(guest.start(width, height)?, Some(Status::Parameter.code()));
    }
    assert_eq!(guest.seen(), ["reply esta", "reply esta", "reply esta"]);
    assert_eq!(guest.backend.opened.lock().len(), 1);
    Ok(())
}

#[test]
fn a_frame_is_described_emitted_and_released_after_its_reply() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(0x51, 0x82);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Efds", "Eemt 1", "reply efrm", "rels [1]"]);
    let descriptions = guest.state.descriptions.lock().clone();
    assert_eq!(descriptions.len(), 1);
    let (codec, width, height, extensions) = &descriptions[0];
    assert_eq!(
        (*codec, *width, *height),
        (i32::from_be_bytes(*b"avc1"), 64, 48)
    );
    assert_eq!(
        extensions.get("FormatName"),
        Some(&plist::Value::String("H.264".into()))
    );
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    let first = &emitted[0];
    assert_eq!(
        (first.frame_id, first.status, first.info, first.uuid),
        (1, NO_ERROR, 0, Some(frame_uuid(1)))
    );
    let Some(sample) = &first.sample else {
        panic!("frame 1 has its sample");
    };
    // Luma at both corners and the second row's start, then chroma: read
    // at the buffer's stride, so that no row's padding, 0, is among them.
    assert_eq!(
        sample.data.as_deref(),
        Some(&[0, 0, 0, 5, 0x65, 0x51, 0x51, 0x51, 0x82][..])
    );
    assert_eq!(sample.sizes, [9]);
    assert_eq!(sample.timings.len(), 1);
    assert_eq!(sample.timings[0].presentation, time(1));
    assert_eq!(sample.timings[0].duration, time(1));
    let Some(format) = &sample.format_description else {
        panic!("the first sample carries its format description");
    };
    assert_eq!(format.codec, FourCc::new(*b"avc1"));
    let format_extensions = format.extensions.clone().unwrap_or_default();
    let Some(SerializedValue::Dictionary(atoms)) =
        entry(&format_extensions, "SampleDescriptionExtensionAtoms")
    else {
        panic!("the description has its atoms");
    };
    assert_eq!(
        entry(atoms, "avcC"),
        Some(&SerializedValue::PropertyList(plist::Value::Data(
            AVCC.to_vec()
        )))
    );
    assert!(entry(&format_extensions, "VerbatimSampleDescription").is_some());
    assert_eq!(
        attachments(sample),
        [
            ("DependsOnOthers".to_owned(), boolean(false)),
            ("EarlierDisplayTimesAllowed".to_owned(), boolean(false)),
        ]
    );

    let second = guest.surface(0x52, 0x83);
    assert_eq!(guest.encode(2, Some(second), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Eemt 2", "reply efrm", "rels [2]"]);
    let emitted = guest.emitted();
    let Some(sample) = &emitted[0].sample else {
        panic!("frame 2 has its sample");
    };
    assert!(sample.format_description.is_none());
    assert_eq!(
        attachments(sample),
        [
            ("NotSync".to_owned(), boolean(true)),
            ("DependsOnOthers".to_owned(), boolean(true)),
            ("EarlierDisplayTimesAllowed".to_owned(), boolean(false)),
        ]
    );
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_forced_key_frame_reaches_the_encoder() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    for frame in 1..=3 {
        let surface = guest.surface(0x40, 0x80);
        let force = frame == 3;
        assert_eq!(
            guest.encode(frame, Some(surface), force)?,
            Some((NO_ERROR, 0))
        );
    }
    guest.settle();
    let sync: Vec<bool> = guest
        .emitted()
        .iter()
        .filter_map(|emitted| emitted.sample.as_ref())
        .map(|sample| !attachments(sample).contains(&("NotSync".to_owned(), boolean(true))))
        .collect();
    assert_eq!(sync, [true, false, true]);
    Ok(())
}

#[test]
fn held_back_frames_are_emitted_when_the_guest_completes_them() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    for frame in 1..=2 {
        let surface = guest.surface(0x30, 0x90);
        assert_eq!(
            guest.encode(frame, Some(surface), false)?,
            Some((NO_ERROR, 0))
        );
    }
    guest.settle();
    // The surfaces stay held while their frames wait in the encoder.
    assert_eq!(guest.seen(), ["reply efrm", "reply efrm"]);
    assert_eq!(guest.memory.held(), 2);
    assert_eq!(guest.complete()?, Some(NO_ERROR));
    // Everything sent, whatever sent it, before the order is read.
    guest.settle();
    assert_eq!(
        guest.seen(),
        [
            "Efds",
            "Eemt 1",
            "rels [1]",
            "Eemt 2",
            "rels [2]",
            "reply ecfr"
        ]
    );
    assert_eq!(
        guest
            .emitted()
            .iter()
            .map(|emitted| (emitted.frame_id, emitted.uuid))
            .collect::<Vec<_>>(),
        [(1, Some(frame_uuid(1))), (2, Some(frame_uuid(2)))]
    );
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

/// A held-back frame's surface is let go only once the guest has its
/// `Eemt`, so a sweep on any thread at that moment sends its `rels` after
/// the `Eemt`, Apple's order. Let go before the send, both surfaces' `rels`
/// came first.
#[test]
fn a_held_back_frames_release_follows_its_emit_whenever_the_host_sweeps() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    for frame in 1..=2 {
        let surface = guest.surface(0x30, 0x90);
        assert_eq!(
            guest.encode(frame, Some(surface), false)?,
            Some((NO_ERROR, 0))
        );
    }
    guest.settle();
    assert_eq!(guest.seen(), ["reply efrm", "reply efrm"]);
    let sweeps = guest.sweep_whenever_a_hold_ends();
    let completed = guest.complete();
    guest.settle();
    drop(sweeps);
    assert_eq!(completed?, Some(NO_ERROR));
    assert_eq!(
        guest.seen(),
        [
            "Efds",
            "Eemt 1",
            "rels [1]",
            "Eemt 2",
            "rels [2]",
            "reply ecfr"
        ]
    );
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

/// The same for a frame a failed picture lost, which `ecfr` answers with
/// -12912.
#[test]
fn a_lost_frames_release_follows_its_emit_whenever_the_host_sweeps() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x30, 0x90);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    let broken = guest.surface(BROKEN, 0x80);
    assert_eq!(
        guest.encode(2, Some(broken), false)?,
        Some((Status::Parameter.code(), 0))
    );
    guest.settle();
    guest.seen();
    let sweeps = guest.sweep_whenever_a_hold_ends();
    let completed = guest.complete();
    guest.settle();
    drop(sweeps);
    assert_eq!(completed?, Some(NO_ERROR));
    assert_eq!(guest.seen(), ["Eemt 1", "rels [1]", "reply ecfr"]);
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_second_start_emits_the_frames_the_encoder_held() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x30, 0x90);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["reply efrm"]);
    assert_eq!(guest.memory.held(), 1);
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    guest.settle();
    assert_eq!(
        guest.seen(),
        ["Efds", "Eemt 1", "rels [1]", "Epba 0x2", "reply esta"]
    );
    assert_eq!(guest.memory.held(), 0);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!((emitted[0].frame_id, emitted[0].status), (1, NO_ERROR));
    assert!(emitted[0].sample.is_some());
    assert_eq!(guest.backend.opened.lock().len(), 2);
    Ok(())
}

#[test]
fn frames_a_failure_loses_are_answered_when_the_guest_completes() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x30, 0x90);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    let broken = guest.surface(BROKEN, 0x80);
    assert_eq!(
        guest.encode(2, Some(broken), false)?,
        Some((Status::Parameter.code(), 0))
    );
    guest.settle();
    // The failed frame's surface goes with its reply; the lost one's stays
    // until the guest completes.
    assert_eq!(guest.memory.held(), 1);
    guest.seen();
    assert_eq!(guest.complete()?, Some(NO_ERROR));
    guest.settle();
    assert_eq!(guest.seen(), ["Eemt 1", "rels [1]", "reply ecfr"]);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        (emitted[0].frame_id, emitted[0].status, emitted[0].uuid),
        (
            1,
            Status::VideoEncoderMalfunction.code(),
            Some(frame_uuid(1))
        )
    );
    assert!(emitted[0].sample.is_none());
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_large_sample_comes_in_fragments_of_0x4000_bytes() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(LARGE, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    let messages = guest.state.emit_messages.lock().clone();
    assert!(messages.len() >= 4, "{messages:?}");
    let (last, fragments) = messages.split_last().unwrap_or((&(0, 0, 0), &[]));
    for &(length, flags, reply_id) in fragments {
        assert_eq!(length, 0x4000);
        assert_eq!((flags, reply_id), (MessageFlags::MORE_FRAGMENTS.0, 0));
    }
    assert!(last.0 <= 0x4000);
    assert_eq!(last.1, MessageFlags::HOST_EXPECTS_REPLY.0);
    assert_ne!(last.2, 0);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].uuid, Some(frame_uuid(1)));
    let Some(sample) = &emitted[0].sample else {
        panic!("the gathered sample reads");
    };
    assert_eq!(sample.data.as_ref().map(Vec::len), Some(LARGE_SAMPLE + 4));
    Ok(())
}

#[test]
fn a_source_whose_planes_do_not_hold_the_picture_never_reaches_the_backend() -> Result<(), WireError>
{
    let guest = started(StandIn::new())?;
    let mut layouts = Vec::new();
    // A chroma plane with no rows, which fits any memory at any stride.
    let mut no_chroma_rows = nv12_surface().layout;
    no_chroma_rows.planes[1].height = 0;
    no_chroma_rows.planes[1].bytes_per_row = usize::MAX;
    layouts.push(no_chroma_rows);
    // Rows narrower than the picture.
    let mut narrow = nv12_surface().layout;
    narrow.planes[0].bytes_per_row = 16;
    layouts.push(narrow);
    // One plane for a two-plane format.
    let mut one_plane = nv12_surface().layout;
    one_plane.planes.clear();
    one_plane.bytes_per_row = BYTES_PER_ROW;
    layouts.push(one_plane);
    for (frame_id, layout) in (1..).zip(layouts) {
        let mapping_id = guest.next_surface.fetch_add(1, Ordering::SeqCst);
        let surface = SurfaceMemory {
            layout,
            ..nv12_surface()
        };
        guest
            .memory
            .surfaces
            .lock()
            .insert(mapping_id, Arc::new(surface));
        assert_eq!(
            guest.encode(frame_id, Some(mapping_id), false)?,
            Some((Status::Parameter.code(), 0)),
            "frame {frame_id}"
        );
    }
    guest.settle();
    assert!(guest.emitted().is_empty());
    assert_eq!(guest.memory.held(), 0);
    // A frame that holds the picture encodes after them.
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(4, Some(surface), false)?, Some((NO_ERROR, 0)));
    Ok(())
}

#[test]
fn a_frame_that_does_not_encode_is_answered_with_its_error() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(BROKEN, 0x80);
    assert_eq!(
        guest.encode(1, Some(surface), false)?,
        Some((Status::Parameter.code(), 0))
    );
    guest.settle();
    assert_eq!(guest.seen(), ["reply efrm", "rels [1]"]);
    assert!(guest.emitted().is_empty());
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

/// A frame the encoder dropped is emitted at once, flagged
/// `kVTEncodeInfo_FrameDropped` with no sample, and its surface goes, as
/// Apple's host forwards a dropped frame. It was emitted with an empty
/// sample buffer, after a format description it did not need.
#[test]
fn a_frame_the_encoder_drops_is_emitted_as_dropped_with_no_sample() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(DROPPED, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Eemt 1", "reply efrm", "rels [1]"]);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        (
            emitted[0].frame_id,
            emitted[0].status,
            emitted[0].info,
            emitted[0].uuid
        ),
        (1, NO_ERROR, 2, Some(frame_uuid(1)))
    );
    assert!(emitted[0].sample.is_none());
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

/// A frame the encoder took and failed is emitted at once with its own
/// status, info flags 0 and no sample, and its surface goes, as Apple's
/// host forwards an emit with its encoder's status. Its `efrm` is 0: the
/// encode call took it.
#[test]
fn a_frame_the_encoder_fails_is_emitted_with_its_own_status() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(FAILS, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Eemt 1", "reply efrm", "rels [1]"]);
    let emitted = guest.emitted();
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        (
            emitted[0].frame_id,
            emitted[0].status,
            emitted[0].info,
            emitted[0].uuid
        ),
        (1, FAILED_STATUS, 0, Some(frame_uuid(1)))
    );
    assert!(emitted[0].sample.is_none());
    assert_eq!(guest.memory.held(), 0);
    // The next frame encodes, and is the stream's first sample.
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(2, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Efds", "Eemt 2", "reply efrm", "rels [2]"]);
    Ok(())
}

/// The backend encodes a copy of the frame, with no lend of its surface
/// held: the stand-in refuses a frame handed to it inside a lend, as
/// reims-vgpu's one lend at a time would make a stuck call there hold every
/// other. Inside the read lend, this frame's `efrm` was answered -1. The
/// copy is the picture's alone, its rows packed: the byte after the first
/// row's last pixel is the second row's first, not the guest's padding.
#[test]
fn the_backend_encodes_a_copy_with_no_lend_held() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(0x40, 0x80);
    let width = usize::try_from(WIDTH).unwrap_or(0);
    guest
        .memory
        .surfaces
        .lock()
        .get(&surface)
        .map(|memory| memory.bytes.lock()[width] = 0x42)
        .ok_or(WireError::Malformed("no surface"))?;
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.seen(), ["Efds", "Eemt 1", "reply efrm", "rels [1]"]);
    let data = guest
        .emitted()
        .first()
        .and_then(|emitted| emitted.sample.as_ref())
        .and_then(|sample| sample.data.clone())
        .unwrap_or_default();
    assert_eq!(data, [0, 0, 0, 5, 0x65, 0x40, 0x40, 0x40, 0x80]);
    Ok(())
}

/// One frame's failure among frames the encoder gives back together loses
/// none of the others: each is emitted in its place with its own status,
/// the failed one with no sample. The whole batch went with the error
/// before, and `ecfr` answered every frame of it -12912.
#[test]
fn a_failed_frame_among_others_loses_none_of_them() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    for (frame, luma) in (1..).zip([0x30, 0x31, FAILS, 0x33]) {
        let surface = guest.surface(luma, 0x90);
        assert_eq!(
            guest.encode(frame, Some(surface), false)?,
            Some((NO_ERROR, 0))
        );
    }
    guest.settle();
    assert_eq!(guest.memory.held(), 4);
    guest.seen();
    assert_eq!(guest.complete()?, Some(NO_ERROR));
    guest.settle();
    assert_eq!(
        guest.seen(),
        [
            "Efds",
            "Eemt 1",
            "rels [1]",
            "Eemt 2",
            "rels [2]",
            "Eemt 3",
            "rels [3]",
            "Eemt 4",
            "rels [4]",
            "reply ecfr"
        ]
    );
    let emitted = guest.emitted();
    assert_eq!(
        emitted
            .iter()
            .map(|emitted| (
                emitted.frame_id,
                emitted.status,
                emitted.uuid,
                emitted.sample.is_some()
            ))
            .collect::<Vec<_>>(),
        [
            (1, NO_ERROR, Some(frame_uuid(1)), true),
            (2, NO_ERROR, Some(frame_uuid(2)), true),
            (3, FAILED_STATUS, Some(frame_uuid(3)), false),
            (4, NO_ERROR, Some(frame_uuid(4)), true),
        ]
    );
    // Each sample is its own frame's: the luma it read.
    let lumas: Vec<Option<u8>> = emitted
        .iter()
        .map(|emitted| {
            emitted
                .sample
                .as_ref()
                .and_then(|sample| sample.data.as_ref())
                .and_then(|data| data.get(5).copied())
        })
        .collect();
    assert_eq!(lumas, [Some(0x30), Some(0x31), None, Some(0x33)]);
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_frame_without_its_surface_gets_no_reply() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    assert_eq!(guest.encode(1, None, false)?, None);
    Ok(())
}

#[test]
fn a_frame_before_the_session_starts_is_refused() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", None)?, NO_ERROR);
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(
        guest.encode(1, Some(surface), false)?,
        Some((Status::InvalidSession.code(), 0))
    );
    guest.settle();
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_new_configuration_is_described_again() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    *guest.backend.configuration.lock() = OTHER_AVCC.to_vec();
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(2, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    let seen = guest.seen();
    assert_eq!(seen.iter().filter(|seen| *seen == "Efds").count(), 2);
    let emitted = guest.emitted();
    let records: Vec<Option<SerializedValue>> = emitted
        .iter()
        .map(|emitted| {
            let extensions = emitted
                .sample
                .as_ref()?
                .format_description
                .as_ref()?
                .extensions
                .clone()?;
            match entry(&extensions, "SampleDescriptionExtensionAtoms")? {
                SerializedValue::Dictionary(atoms) => entry(atoms, "avcC").cloned(),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        records,
        [AVCC, OTHER_AVCC].map(
            |record| Some(SerializedValue::PropertyList(plist::Value::Data(
                record.to_vec()
            )))
        )
    );
    Ok(())
}

#[test]
fn settings_reach_a_running_encoder() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    assert_eq!(
        guest.set_property("AverageBitRate", plist::Value::Integer(3_000_000.into()))?,
        Some(NO_ERROR)
    );
    let reconfigured = guest.backend.reconfigured.lock().clone();
    assert_eq!(
        reconfigured
            .last()
            .and_then(|settings| settings.average_bit_rate),
        Some(3_000_000)
    );
    Ok(())
}

/// `epfr` prepares the running encoder and answers with its status, as
/// Apple's host calls its encoder's `PrepareToEncodeFrames` there; before
/// `esta` there is none to prepare, and the answer is 0.
#[test]
fn preparing_reaches_the_running_encoder_with_its_status() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", Some(OUR_H264_ID))?, NO_ERROR);
    assert_eq!(guest.status_of(b"epfr", WAIT, |_| {})?, Some(NO_ERROR));
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    assert_eq!(guest.backend.prepared.load(Ordering::SeqCst), 0);
    assert_eq!(guest.status_of(b"epfr", WAIT, |_| {})?, Some(NO_ERROR));
    assert_eq!(guest.backend.prepared.load(Ordering::SeqCst), 1);
    guest.backend.prepare_fails.store(true, Ordering::SeqCst);
    assert_eq!(guest.status_of(b"epfr", WAIT, |_| {})?, Some(FAILED_STATUS));
    Ok(())
}

/// `MultiPassStorage` as Apple's host answers it on an encoder without the
/// property, its HEVC software encoder: not listed, refused, and a set of
/// anything but true releases the host's storage, which asks the guest to
/// invalidate its own.
#[test]
fn multi_pass_storage_is_refused_and_its_release_invalidates_the_guests() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let reply = guest
        .call(b"esup", WAIT, |_| {})
        .ok_or(WireError::Malformed("no esup reply"))?;
    let supported = Message::parse(&reply)?
        .dictionary(names::SUPPORTED_PROPERTIES)?
        .unwrap_or_default();
    assert!(!supported.contains_key("MultiPassStorage"));
    let refused = Status::PropertyNotSupported.code();
    assert_eq!(guest.copy_property("MultiPassStorage")?, (refused, None));
    guest.seen();
    let storage = |value| guest.set_property("MultiPassStorage", plist::Value::Boolean(value));
    assert_eq!(storage(true)?, Some(refused));
    assert_eq!(guest.seen(), ["reply eset"]);
    // A second true replaces the storage and lets nothing go.
    assert_eq!(storage(true)?, Some(refused));
    assert_eq!(guest.seen(), ["reply eset"]);
    assert_eq!(storage(false)?, Some(refused));
    assert_eq!(guest.seen(), ["mpin 0x2", "reply eset"]);
    // With no storage, there is nothing to release.
    assert_eq!(storage(false)?, Some(refused));
    assert_eq!(guest.seen(), ["reply eset"]);
    // Anything but true releases it, as false does.
    assert_eq!(storage(true)?, Some(refused));
    assert_eq!(
        guest.set_property("MultiPassStorage", plist::Value::Integer(1.into()))?,
        Some(refused)
    );
    assert_eq!(guest.seen(), ["reply eset", "mpin 0x2", "reply eset"]);
    assert_eq!(guest.copy_property("MultiPassStorage")?, (refused, None));
    // The session's end lets its storage go without a word.
    assert_eq!(storage(true)?, Some(refused));
    guest.seen();
    guest.close();
    guest.settle_device();
    assert_eq!(guest.seen(), Vec::<String>::new());
    Ok(())
}

#[test]
fn multipass_and_tiles_are_refused_as_for_an_encoder_without_them() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    let unimplemented = Some(Status::UnimplementedFunction.code());
    assert_eq!(
        guest.status_of(b"ebps", WAIT, |message| {
            message.u32(names::BEGIN_PASS_FLAGS, 0);
        })?,
        unimplemented
    );
    let reply = guest
        .call(b"eeps", WAIT, |_| {})
        .ok_or(WireError::Malformed("no eeps reply"))?;
    let reply = Message::parse(&reply)?;
    assert_eq!(reply.s32(names::FRAME_PROPERTIES)?, Some(0));
    assert_eq!(reply.s32(names::ERROR)?, unimplemented);
    assert_eq!(
        guest.status_of(b"ests", WAIT, |message| {
            message.i32_pair(names::TILE_DIMENSIONS, ValueType::Size, (64, 64));
        })?,
        unimplemented
    );
    assert_eq!(
        guest.status_of(b"ests", WAIT, |message| {
            message.i32_pair(names::TILE_DIMENSIONS, ValueType::Size, (0, 64));
        })?,
        Some(Status::MalformedMessage.code())
    );
    assert_eq!(
        guest.status_of(b"epti", WAIT, |message| {
            message.s32(names::TILE_COUNT, 4);
        })?,
        unimplemented
    );
    let surface = guest.surface(0x40, 0x80);
    let (reply_id, message) = guest.message(b"etil", |message| {
        message.s64(names::TILE, 1);
        message.i32_pair(names::TILE_OFFSET, ValueType::Point, (0, 0));
        message.i32_pair(names::TILE_APERTURE, ValueType::Size, (64, 48));
    });
    let reply = guest
        .call_with(reply_id, with_surface(surface, &message), WAIT)
        .ok_or(WireError::Malformed("no etil reply"))?;
    let reply = Message::parse(&reply)?;
    assert_eq!(reply.s32(names::ERROR)?, unimplemented);
    assert_eq!(reply.u32(names::ENCODE_INFO_FLAGS)?, Some(0));
    assert_eq!(guest.status_of(b"ecti", WAIT, |_| {})?, unimplemented);
    let reply = guest
        .call(b"emif", WAIT, |message| {
            message.s64(names::FRAME, 1);
        })
        .ok_or(WireError::Malformed("no emif reply"))?;
    let reply = Message::parse(&reply)?;
    assert_eq!(reply.s32(names::ERROR)?, unimplemented);
    assert_eq!(reply.u32(names::INFO_FLAGS)?, Some(0));
    assert_eq!(guest.status_of(b"epfr", WAIT, |_| {})?, Some(NO_ERROR));
    guest.settle();
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn invalidating_the_encoder_lets_its_frames_go() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    assert_eq!(guest.memory.held(), 1);
    guest.seen();
    assert_eq!(guest.status_of(b"einv", WAIT, |_| {})?, Some(NO_ERROR));
    guest.settle();
    assert_eq!(guest.seen(), ["rels [1]", "reply einv"]);
    assert_eq!(guest.memory.held(), 0);
    // Nothing is left to complete.
    assert_eq!(guest.complete()?, Some(NO_ERROR));
    assert!(guest.emitted().is_empty());
    Ok(())
}

#[test]
fn closing_ends_the_session_and_what_it_held() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.close();
    guest.settle_device();
    assert_eq!(guest.memory.held(), 0);
    let (_, esup) = guest.message(b"esup", |_| {});
    assert_eq!(guest.session.deliver(&esup), Status::SessionNotFound.code());
    Ok(())
}

#[test]
fn tearing_down_lets_held_frames_go() -> Result<(), WireError> {
    let backend = StandIn::new();
    backend.hold_back.store(true, Ordering::SeqCst);
    let guest = started(backend)?;
    let surface = guest.surface(0x40, 0x80);
    assert_eq!(guest.encode(1, Some(surface), false)?, Some((NO_ERROR, 0)));
    guest.settle();
    guest.session.invalidate();
    guest.session.complete_invalidate();
    assert_eq!(guest.memory.held(), 0);
    Ok(())
}

#[test]
fn a_backend_refuses_a_property_with_its_own_status() -> Result<(), WireError> {
    let guest = Guest::new(StandIn::new());
    assert_eq!(guest.create(b"avc1", None)?, NO_ERROR);
    assert_eq!(
        guest.set_property("Quality", plist::Value::Real(0.5))?,
        Some(NO_ERROR)
    );
    assert_eq!(
        guest.set_property("Quality", plist::Value::Real(0.95))?,
        Some(Status::Parameter.code())
    );
    assert_eq!(
        guest.copy_property("Quality")?,
        (NO_ERROR, Some(plist::Value::Real(0.5)))
    );
    assert_eq!(guest.start(64, 48)?, Some(NO_ERROR));
    let opened = guest.backend.opened.lock().clone();
    assert_eq!(opened[0].settings.quality, Some(0.5));
    assert_eq!(
        opened[0].settings.properties,
        [("Quality".to_owned(), plist::Value::Real(0.5))]
    );
    Ok(())
}

#[test]
fn a_setting_a_running_encoder_cannot_take_is_refused_and_undone() -> Result<(), WireError> {
    let guest = started(StandIn::new())?;
    assert_eq!(
        guest.set_property("H264EntropyMode", plist::Value::String("CAVLC".into()))?,
        Some(Status::PropertyNotSupported.code())
    );
    assert_eq!(guest.copy_property("H264EntropyMode")?, (NO_ERROR, None));
    assert_eq!(
        guest.set_property("H264EntropyMode", plist::Value::String("CABAC".into()))?,
        Some(NO_ERROR)
    );
    Ok(())
}
