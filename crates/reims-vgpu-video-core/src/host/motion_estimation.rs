//! Motion-estimation sessions: what the host does for each `+mep` the
//! guest's `VTMotionEstimationSession` opens, following Apple's host
//! processor session
//! with a [`MotionEstimator`] where Apple has a
//! `VTMotionEstimationProcessor`.
//!
//! Each session has a thread of its own, its serial queue: the device's
//! thread hands it each message and returns at once, and the session's
//! thread handles them in order. The guest lends the two frames of each
//! `mepf`; the session answers it, estimates, asks the guest for a vector
//! buffer (`Mbuf`), lets the frames go, writes the vectors into the buffer
//! as Apple's grid of half floats, emits it (`Memt`), and tells the guest
//! which of its pixel buffers it has let go (`Mdpb`), in the order Apple's
//! host does (measured on macOS 27). The processor's work is the backend's;
//! everything the guest sees is here.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Weak};

use crate::backend::{BackendError, PixelFormat};
use crate::fourcc::FourCc;
use crate::host::HostShared;
use crate::host::codec_lists::{MOTION_PROCESSOR_ID, MOTION_PROCESSOR_NAME};
use crate::host::encoder::planes_hold;
use crate::host::mapped_surfaces::{SurfaceHold, ending_together};
use crate::host::names::{self, MotionMessage};
use crate::host::refused::{RefusedKind, RefusedSession};
use crate::host::reply_clerk::{CODEC_OPERATION_REPLY_TIMEOUT, DEFAULT_REPLY_TIMEOUT};
use crate::host::session::{
    Created, Job, SessionEntry, SessionKind, SessionLink, SessionQueue, create_reply, property_key,
    reply, reply_error,
};
use crate::motion::{
    AppleGrid, BlockSize, MAX_AREA, MAX_SIDE, MIN_SIDE, MotionEstimationBackend,
    MotionEstimationCapabilities, MotionEstimator, MotionEstimatorConfig, MotionField,
    MotionPicture, half_float_pair, resample_row,
};
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::surfaces::{PlaneSource, SurfaceLayout};
use crate::wire::WireError;
use crate::wire::message::{
    GuestUuid, Message, MessageFlags, MessageWriter, ValueType, read_header,
};
use crate::wire::property_list::{dictionary, integer};

/// A motion-estimation session's thread (ours) and log name.
const KIND: SessionKind = SessionKind {
    thread: "reims-vgpu-video.motion",
    name: "a motion-estimation session",
};

/// The alignments the host asks the guest's source buffers to have, as
/// Apple's processor asks (measured on macOS 27: `Mspa`).
const SOURCE_ALIGNMENT: i64 = 64;

/// `kCVPixelFormatType_TwoComponent16Half`, the vector buffers' format.
const VECTOR_FORMAT: FourCc = FourCc::new(*b"2C0h");

/// `kVTMotionEstimationFrameFlags_CurrentBufferWillBeNextReferenceBuffer`
/// (`VTMotionEstimationSession.h`): the processor keeps the current frame
/// for the next one.
const CURRENT_IS_NEXT_REFERENCE: u32 = 1 << 0;

/// The pixel buffers a session remembers at most (ours). A guest's
/// processor lends two or three at a time, and the clean-up after each
/// frame forgets the dead, but a malformed `mepf` gets no clean-up, so a
/// guest sending them could grow the table, and the next `Mdpb` past the
/// guest's buffer, without end. Before a table this full takes more, the
/// session cleans up, sending `Mdpb` sooner than Apple's would, and a
/// frame it still has no room for is malformed. 256 UUIDs are 4 KiB, well
/// within a message.
const MAX_PIXEL_BUFFERS: usize = 256;

/// The specification's and properties' keys, as `VTMotionEstimationSession.h`
/// spells their values, and as the guest's session sends them.
const MOTION_VECTOR_SIZE: &str = "MotionVectorSize";
const USE_MULTI_PASS_SEARCH: &str = "UseMultiPassSearch";
const TRUE_MOTION: &str = "TrueMotion";
const INCLUDE_MOTION_CONFIDENCE: &str = "IncludeMotionConfidence";
const PROPERTY_MODE: &str = "PropertyMode";
const CLIENT_PID: &str = "ClientPID";
const IS_HARDWARE_ACCELERATED: &str = "VTIsHardwareAccelerated";
const HOST_PROCESSOR_ID: &str = "HostProcessorID";
const IMPLEMENTATION_NAME: &str = "CMClassImplementationName";

/// Makes the session `+mep` asks for and its create reply. `,ver`,
/// `,meo` and `,gin` must read, or there is no session and no reply.
///
/// The specification selects the processor as Apple's host selects
/// one: a `CMClassImplementationName` must be this host's, a
/// `VTIsHardwareAccelerated` must be true, and a `HostProcessorID` must
/// be this host's ID. A specification that selects none, or a host
/// without motion estimation, makes a session whose reply says -19350,
/// as Apple's did for an unknown ID (measured on macOS 27).
pub(crate) fn create(message: &Message<'_>, host: &Arc<HostShared>) -> Created {
    let status = |error: WireError| error.status();
    message.u32(names::VERSION).map_err(status)?;
    let specification = message
        .dictionary(names::MOTION_OPTIONS)
        .map_err(status)?
        .unwrap_or_default();
    message.dictionary(names::GUEST_INFO).map_err(status)?;
    let backend = host
        .motion
        .clone()
        .filter(|_| selects_this_processor(&specification));
    let Some(backend) = backend else {
        let (session, reply) =
            RefusedSession::create(message, RefusedKind::MotionEstimation).map_err(status)?;
        return Ok((SessionEntry::Refused(session), reply));
    };
    let session = start(
        message.header().guest_uuid,
        backend,
        MotionProperties::from_specification(&specification),
        host,
    )?;
    let reply = create_reply(message, NO_ERROR).map_err(status)?;
    Ok((SessionEntry::Running(session), reply))
}

/// Whether `specification` selects this host's processor, as Apple's
/// registry filter and lookup would select it.
fn selects_this_processor(specification: &plist::Dictionary) -> bool {
    let name_matches = specification
        .get(IMPLEMENTATION_NAME)
        .is_none_or(|name| name.as_string() == Some(MOTION_PROCESSOR_NAME));
    let acceleration_matches = specification
        .get(IS_HARDWARE_ACCELERATED)
        .is_none_or(|accelerated| accelerated.as_boolean() == Some(true));
    let id_matches = specification
        .get(HOST_PROCESSOR_ID)
        .is_none_or(|id| id.as_string() == Some(MOTION_PROCESSOR_ID));
    name_matches && acceleration_matches && id_matches
}

/// The processor's properties, as Apple's answered them (measured on macOS
/// 27): the specification's block size and search, settable `TrueMotion`
/// and `PropertyMode`, and no motion confidence.
struct MotionProperties {
    block: BlockSize,
    multipass: bool,
    true_motion: bool,
    property_mode: plist::Value,
}

impl MotionProperties {
    fn from_specification(specification: &plist::Dictionary) -> Self {
        let flag = |key: &str| {
            specification
                .get(key)
                .and_then(plist::Value::as_boolean)
                .unwrap_or(false)
        };
        let size = specification.get(MOTION_VECTOR_SIZE).and_then(|size| {
            size.as_signed_integer()
                .or_else(|| size.as_real().filter(|real| *real == 4.0).map(|_| 4))
        });
        Self {
            block: BlockSize::from_motion_vector_size(size),
            multipass: flag(USE_MULTI_PASS_SEARCH),
            true_motion: flag(TRUE_MOTION),
            property_mode: integer(0),
        }
    }

    /// `mcop`'s answer for `key`: Apple's processor's for the keys it
    /// answered. Any other key is -12900, where Apple's processor traps,
    /// ending the VM.
    fn copy(&self, key: &str) -> (OsStatus, Option<plist::Value>) {
        let value = match key {
            INCLUDE_MOTION_CONFIDENCE => plist::Value::Boolean(false),
            MOTION_VECTOR_SIZE => integer(i64::from(self.block.pixels())),
            USE_MULTI_PASS_SEARCH => plist::Value::Boolean(self.multipass),
            TRUE_MOTION => plist::Value::Boolean(self.true_motion),
            PROPERTY_MODE => self.property_mode.clone(),
            // Apple's AVE processor answers false (measured on macOS 27);
            // the guest's session takes the processor by its matching info.
            IS_HARDWARE_ACCELERATED => plist::Value::Boolean(false),
            _ => return (Status::PropertyNotSupported.code(), None),
        };
        (NO_ERROR, Some(value))
    }

    /// `mset` of `key`: `TrueMotion`, `PropertyMode` and `ClientPID` are
    /// taken, as Apple's took them, and anything else answers -2002, as
    /// Apple's answered an unknown key and `Paravirtualized` (measured on
    /// macOS 27). A value of the wrong type is -12902 (ours: Apple's was
    /// not measured).
    fn set(&mut self, key: &str, value: Option<&plist::Value>) -> OsStatus {
        match (key, value) {
            (TRUE_MOTION, Some(plist::Value::Boolean(true_motion))) => {
                self.true_motion = *true_motion;
                NO_ERROR
            }
            (PROPERTY_MODE, Some(mode @ plist::Value::Integer(_))) => {
                self.property_mode = mode.clone();
                NO_ERROR
            }
            (TRUE_MOTION | PROPERTY_MODE, _) => Status::Parameter.code(),
            // The guest's session answers `ClientPID` itself, so it is
            // never read back.
            (CLIENT_PID, _) => NO_ERROR,
            _ => Status::MotionPropertyRefused.code(),
        }
    }
}

/// Starts an estimating session on `backend`, its thread waiting for its
/// first message.
fn start(
    guest_uuid: GuestUuid,
    backend: Arc<dyn MotionEstimationBackend>,
    properties: MotionProperties,
    host: &Arc<HostShared>,
) -> Result<SessionQueue, Status> {
    let link = SessionLink::new(guest_uuid, host);
    let (jobs, queue) = mpsc::channel();
    let worker = Worker {
        link: link.clone(),
        capabilities: backend.capabilities(),
        backend,
        properties,
        estimator: None,
        dimensions: None,
        processor_invalidated: false,
        kept: None,
        pixel_buffers: PixelBuffers::default(),
    };
    SessionQueue::start(link, host, &KIND, jobs, move || worker.run(&queue))
}

/// The pixel buffers the session made of guest surfaces, by the UUID the
/// guest gave each: Apple's host keeps a weak pixel buffer per UUID, and
/// here the pixel buffer is the surface's hold.
#[derive(Default)]
struct PixelBuffers {
    entries: Vec<(GuestUuid, Weak<SurfaceHold>)>,
}

impl PixelBuffers {
    /// Remembers `surface` under `uuid`. A UUID whose buffer lives on with
    /// another surface is malformed, as Apple's host refuses it.
    fn remember(&mut self, uuid: GuestUuid, surface: &Arc<SurfaceHold>) -> Result<(), WireError> {
        let full = !self.has_room_for(1);
        match self.entries.iter_mut().find(|(known, _)| *known == uuid) {
            Some((_, buffer)) => match buffer.upgrade() {
                Some(live) if live.mapping_id != surface.mapping_id => Err(WireError::Malformed(
                    "a pixel buffer UUID the host knows with another surface",
                )),
                Some(_) => Ok(()),
                None => {
                    *buffer = Arc::downgrade(surface);
                    Ok(())
                }
            },
            None if full => Err(WireError::Malformed(
                "more pixel buffers than a session remembers",
            )),
            None => {
                self.entries.push((uuid, Arc::downgrade(surface)));
                Ok(())
            }
        }
    }

    /// Whether `count` more entries would fill the table past its bound.
    fn has_room_for(&self, count: usize) -> bool {
        self.entries.len() + count <= MAX_PIXEL_BUFFERS
    }

    /// Takes the UUID of the live buffer `surface`, which the host then
    /// forgets, as Apple's emission takes the vector buffer's.
    fn take(&mut self, surface: &Arc<SurfaceHold>) -> Option<GuestUuid> {
        let index = self.entries.iter().position(|(_, buffer)| {
            buffer
                .upgrade()
                .is_some_and(|live| Arc::ptr_eq(&live, surface))
        })?;
        Some(self.entries.remove(index).0)
    }

    /// Forgets the buffers that have died, and returns their UUIDs, in
    /// order: what `Mdpb` names.
    fn dead(&mut self) -> Vec<GuestUuid> {
        let mut dead = Vec::new();
        self.entries.retain(|(uuid, buffer)| {
            if buffer.strong_count() > 0 {
                true
            } else {
                dead.push(*uuid);
                false
            }
        });
        dead
    }
}

/// The session's thread and what only it touches.
struct Worker {
    link: Arc<SessionLink>,
    backend: Arc<dyn MotionEstimationBackend>,
    capabilities: MotionEstimationCapabilities,
    properties: MotionProperties,
    estimator: Option<Box<dyn MotionEstimator>>,
    /// `mest`'s dimensions.
    dimensions: Option<(u32, u32)>,
    /// Whether `minv` invalidated the processor.
    processor_invalidated: bool,
    /// The current frame of a `mepf` flagged
    /// [`CURRENT_IS_NEXT_REFERENCE`], kept until the next frame is done, as
    /// Apple's processor keeps it (measured on macOS 27).
    kept: Option<Arc<SurfaceHold>>,
    pixel_buffers: PixelBuffers,
}

/// What a handler does when its message does not read: Apple's handlers
/// send no reply, and the guest's wait times out.
type Handled = Result<(), WireError>;

/// The frames of one `mepf`, read and checked.
struct FramePair {
    request: i64,
    reference: Arc<SurfaceHold>,
    current: Arc<SurfaceHold>,
    keep_current: bool,
}

impl Worker {
    fn run(mut self, queue: &Receiver<Job>) {
        while let Ok(job) = queue.recv() {
            match job {
                Job::Message {
                    bytes,
                    surfaces,
                    held,
                } => {
                    let lent = !surfaces.is_empty();
                    // The surfaces the message leaves, where it is malformed
                    // or the session invalidated, end together.
                    let left = ending_together(|| {
                        if !self.link.is_invalidated() {
                            self.handle(&bytes, surfaces);
                        }
                    });
                    drop(held);
                    if lent || !left.is_empty() {
                        // Apple's block lets the message's surfaces go, and
                        // the delivery's sweep follows; this one makes sure
                        // of it.
                        self.link.let_go(left);
                    }
                }
                Job::Invalidate(done) => {
                    self.estimator = None;
                    self.kept = None;
                    done.give(());
                }
            }
        }
    }

    fn handle(&mut self, bytes: &[u8], surfaces: Vec<Arc<SurfaceHold>>) {
        let header = match read_header(bytes, 0) {
            Ok(header) => header,
            Err(error) => {
                log::warn!("motion-estimation session message does not read: {error}");
                return;
            }
        };
        let Some(kind) = MotionMessage::from_fourcc(header.message_type) else {
            // Logged, with no reply.
            log::warn!("unrecognized messageType '{}'", header.message_type);
            return;
        };
        let handled = match Message::parse(bytes) {
            Ok(message) => self.handle_message(kind, &message, surfaces),
            Err(error) => Err(error),
        };
        if let Err(error) = handled {
            log::warn!("'{}' got no reply: {error}", header.message_type);
        }
    }

    fn handle_message(
        &mut self,
        kind: MotionMessage,
        message: &Message<'_>,
        surfaces: Vec<Arc<SurfaceHold>>,
    ) -> Handled {
        match kind {
            MotionMessage::StartSession => self.start_session(message),
            MotionMessage::ProcessFrames => self.process_frames(message, surfaces),
            // Each frame is done before the next message is handled.
            MotionMessage::CompleteFrames => self.reply_status(message, NO_ERROR),
            MotionMessage::CopyProperty => self.copy_property(message),
            MotionMessage::SetProperty => self.set_property(message),
            MotionMessage::SetProperties => self.set_properties(message),
            MotionMessage::SerializableProperties => {
                // Apple's processor has none to give (measured on macOS 27:
                // -12782), and neither has this host's.
                self.reply_status(message, Status::UnimplementedFunction.code())
            }
            MotionMessage::Invalidate => {
                // The kept frame goes with the estimator, and its surface
                // with the sweep before the reply, so that the guest has
                // its `rels` by the time its invalidate returns.
                self.estimator = None;
                self.processor_invalidated = true;
                let kept = self.kept.take();
                self.link.let_go(kept);
                self.reply_status(message, NO_ERROR)
            }
            // Never answered.
            MotionMessage::SupportedProperties => Ok(()),
        }
    }

    fn send(&self, message: Vec<u8>) {
        if let Err(status) = self.link.send(message) {
            log::debug!("motion-estimation session send dropped: {status:?}");
        }
    }

    fn reply_status(&self, message: &Message<'_>, status: OsStatus) -> Handled {
        let mut reply = reply(message);
        reply.s32(names::ERROR, status);
        self.send(reply.finish());
        Ok(())
    }

    /// `mcop`: the key must be a string, or there is no reply.
    fn copy_property(&self, message: &Message<'_>) -> Handled {
        let key = property_key(message)?;
        let (status, value) = self.properties.copy(&key);
        let mut reply = reply(message);
        reply.s32(names::ERROR, status);
        if let Some(value) = value {
            reply.property_list(names::PROPERTY_VALUE, Some(&value))?;
        }
        self.send(reply.finish());
        Ok(())
    }

    /// `mset`: the key must be a string, or there is no reply.
    fn set_property(&mut self, message: &Message<'_>) -> Handled {
        let key = property_key(message)?;
        let value = message.property_list(names::PROPERTY_VALUE)?;
        let status = self.properties.set(&key, value.as_ref());
        self.reply_status(message, status)
    }

    /// `mmul`: each of `,pro` in turn, the first refusal the answer; no
    /// `,pro` is -12902, as Apple's answered (measured on macOS 27).
    fn set_properties(&mut self, message: &Message<'_>) -> Handled {
        let Some(properties) = message.dictionary(names::PROPERTIES)? else {
            return self.reply_status(message, Status::Parameter.code());
        };
        let mut status = NO_ERROR;
        for (key, value) in &properties {
            let set = self.properties.set(key, Some(value));
            if status == NO_ERROR {
                status = set;
            }
        }
        self.reply_status(message, status)
    }

    /// `mest`: opens the estimator for the pictures' dimensions, asks the
    /// guest for sources it takes (`Mspa`) and for vector buffers (`Mvpb`),
    /// then replies (measured on macOS 27). A side under 2 or past 16384, or
    /// more than 16384x8192, is -12902 with neither sent, as Apple's
    /// answered, and so is a size past the backend's.
    fn start_session(&mut self, message: &Message<'_>) -> Handled {
        let (width, height) = message
            .i32_pair(names::DIMENSIONS, ValueType::Dimensions)?
            .unwrap_or((0, 0));
        let mut kept = None;
        let status = self.start(width, height, &mut kept);
        let replied = self.reply_status(message, status);
        // The frame the estimator before kept goes once the reply has, with
        // a sweep, so that its `rels` follows it whichever thread's sweep
        // comes first.
        self.link.let_go(kept);
        replied
    }

    /// Opens the estimator; the frame the one before kept goes into `kept`.
    fn start(&mut self, width: i32, height: i32, kept: &mut Option<Arc<SurfaceHold>>) -> OsStatus {
        if self.processor_invalidated {
            return Status::SessionMalfunction.code();
        }
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return Status::Parameter.code();
        };
        let fits = width >= MIN_SIDE
            && height >= MIN_SIDE
            && width <= MAX_SIDE.min(self.capabilities.max_width)
            && height <= MAX_SIDE.min(self.capabilities.max_height)
            && u64::from(width) * u64::from(height) <= MAX_AREA;
        if !fits {
            return Status::Parameter.code();
        }
        let config = MotionEstimatorConfig {
            width,
            height,
            block: self.properties.block,
            true_motion: self.properties.true_motion || self.properties.multipass,
        };
        // The estimator before goes first, and the frame it kept.
        self.estimator = None;
        *kept = self.kept.take();
        let estimator = match self.backend.open(&config) {
            Ok(estimator) => estimator,
            Err(error) => {
                log::warn!("cannot open a motion estimator: {error}");
                return motion_status(&error);
            }
        };
        log::info!(
            "motion estimator opened: {width}x{height}, blocks of {}, true motion {}",
            config.block.pixels(),
            config.true_motion
        );
        self.estimator = Some(estimator);
        self.dimensions = Some((width, height));
        let status = self.set_source_attributes(width, height);
        if status != NO_ERROR {
            return status;
        }
        self.set_vector_attributes(&self.grid(width, height))
    }

    fn grid(&self, width: u32, height: u32) -> AppleGrid {
        AppleGrid {
            width,
            height,
            block: self.properties.block,
        }
    }

    /// `Mspa`: the sources' attributes: the backend's formats, the
    /// session's size, IOSurface backing, and Apple's processor's
    /// alignments (measured on macOS 27). The guest converts a client's
    /// frames that do not match.
    fn set_source_attributes(&self, width: u32, height: u32) -> OsStatus {
        let formats = (self.capabilities.source_formats.iter())
            .map(|format| integer(i64::from(format.fourcc().0)))
            .collect();
        let attributes = plist::Value::Dictionary(dictionary([
            ("Width", integer(i64::from(width))),
            ("Height", integer(i64::from(height))),
            ("PixelFormatType", plist::Value::Array(formats)),
            ("BytesPerRowAlignment", integer(SOURCE_ALIGNMENT)),
            ("PlaneAlignment", integer(SOURCE_ALIGNMENT)),
            (
                "IOSurfaceProperties",
                plist::Value::Dictionary(plist::Dictionary::new()),
            ),
        ]));
        self.set_attributes(names::SET_MOTION_SOURCE_ATTRIBUTES, &attributes)
    }

    /// `Mvpb`: the vector buffers' attributes: the grid, `2C0h` and
    /// IOSurface backing, as Apple's (measured on macOS 27).
    fn set_vector_attributes(&self, grid: &AppleGrid) -> OsStatus {
        let attributes = plist::Value::Dictionary(dictionary([
            ("Width", integer(i64::from(grid.columns()))),
            ("Height", integer(i64::from(grid.rows()))),
            ("PixelFormatType", integer(i64::from(VECTOR_FORMAT.0))),
            (
                "IOSurfaceProperties",
                plist::Value::Dictionary(plist::Dictionary::new()),
            ),
        ]));
        self.set_attributes(names::SET_MOTION_VECTOR_ATTRIBUTES, &attributes)
    }

    /// Sends `Mspa` or `Mvpb` and waits 1.1 s for the guest's `,err`.
    fn set_attributes(&self, message_type: FourCc, attributes: &plist::Value) -> OsStatus {
        let mut written = Ok(());
        let reply = self
            .link
            .request(message_type, DEFAULT_REPLY_TIMEOUT, |message| {
                written = message
                    .property_list(names::PIXEL_BUFFER_ATTRIBUTES, Some(attributes))
                    .map(|_| ());
            });
        if let Err(error) = written {
            return error.status().code();
        }
        reply_error(reply)
    }

    /// `mepf`: exactly two surfaces, `,rid`, `,ui+` of exactly two UUIDs
    /// (the reference's, then the current's), each UUID one the host does
    /// not know with another surface, `,mef` and `,meo`; else no reply.
    fn process_frames(
        &mut self,
        message: &Message<'_>,
        surfaces: Vec<Arc<SurfaceHold>>,
    ) -> Handled {
        let Ok([reference, current]) = <[Arc<SurfaceHold>; 2]>::try_from(surfaces) else {
            return Err(WireError::Malformed("frames without their two surfaces"));
        };
        let request = message.s64(names::REQUEST)?.unwrap_or(0);
        let uuids = message
            .uuid_array(names::PIXEL_BUFFER_UUIDS)?
            .unwrap_or_default();
        let [reference_uuid, current_uuid] = uuids[..] else {
            return Err(WireError::Malformed("frames without their two UUIDs"));
        };
        self.make_room_for(2);
        self.pixel_buffers.remember(reference_uuid, &reference)?;
        self.pixel_buffers.remember(current_uuid, &current)?;
        let flags = message.u32(names::MOTION_FRAME_FLAGS)?.unwrap_or(0);
        message.dictionary(names::MOTION_OPTIONS)?;
        let frames = FramePair {
            request,
            reference,
            current,
            keep_current: flags & CURRENT_IS_NEXT_REFERENCE != 0,
        };
        if self.processor_invalidated {
            // Apple's processor emits the frame with -17691 and fails it,
            // and its host then traps; this host emits it and fails it.
            let status = Status::SessionMalfunction.code();
            self.let_frames_go(frames);
            self.emit(request, status, None);
            self.reply_status(message, status)?;
            self.clean_up();
            return Ok(());
        }
        if let Err(status) = self.check_frames(&frames) {
            // Failed before it was taken: no emission, and the frames'
            // buffers named as let go after the reply.
            self.let_frames_go(frames);
            self.reply_status(message, status)?;
            self.clean_up();
            return Ok(());
        }
        // The reply goes first, as Apple's processor answers at once and
        // estimates after (measured on macOS 27).
        self.reply_status(message, NO_ERROR)?;
        self.estimate_and_emit(frames);
        Ok(())
    }

    /// The checks a frame pair fails before it is estimated, answered in
    /// `mepf`'s reply: a session started, and sources of a format the
    /// backend takes and of the session's size. Apple's answers were not
    /// measured; these are ours.
    fn check_frames(&self, frames: &FramePair) -> Result<(), OsStatus> {
        if self.estimator.is_none() {
            return Err(Status::InvalidSession.code());
        }
        for surface in [&frames.reference, &frames.current] {
            let layout = surface.surface.layout();
            if self.source_format(&layout.pixel_format).is_none() {
                log::warn!("a motion-estimation source in '{}'", layout.pixel_format);
                return Err(Status::Parameter.code());
            }
            if self.dimensions != Some((layout.width, layout.height)) {
                log::warn!(
                    "a {}x{} motion-estimation source for a {:?} session",
                    layout.width,
                    layout.height,
                    self.dimensions
                );
                return Err(Status::Parameter.code());
            }
        }
        Ok(())
    }

    fn source_format(&self, fourcc: &FourCc) -> Option<PixelFormat> {
        PixelFormat::from_fourcc(*fourcc)
            .filter(|format| self.capabilities.source_formats.contains(format))
    }

    /// Estimates, asks the guest for a vector buffer, lets the frames go,
    /// writes the vectors, emits them and cleans up, in the order Apple's
    /// host does each (measured on macOS 27): `Mbuf` and its reply, the
    /// sources' `rels`, `Memt` and its reply, `Mdpb`, then the vector
    /// buffer's `rels`, which Apple's sent before or after `Mdpb`.
    fn estimate_and_emit(&mut self, frames: FramePair) {
        let request = frames.request;
        let field = self.estimate(&frames);
        let mut buffer = Err(Status::InvalidSession.code());
        // A surface a reply lent with no buffer goes with the sources.
        let refused = ending_together(|| {
            buffer = match &field {
                Ok(_) => self.create_vector_buffer(request),
                Err(status) => Err(*status),
            };
        });
        self.let_frames_go_with(frames, refused);
        let (status, written) = match (&field, &buffer) {
            (Ok(field), Ok(buffer)) => match self.write_vectors(field, buffer) {
                Ok(()) => (NO_ERROR, true),
                Err(status) => (status, false),
            },
            (Err(status), _) | (Ok(_), Err(status)) => (*status, false),
        };
        self.emit(request, status, buffer.as_ref().ok().filter(|_| written));
        // `Mdpb` before the buffer goes, written or not, so that its `rels`
        // follows, whichever thread's sweep sends it.
        self.clean_up();
        self.link.let_go(buffer);
    }

    /// The backend's field for the pair.
    fn estimate(&mut self, frames: &FramePair) -> Result<MotionField, OsStatus> {
        let reference_layout = frames.reference.surface.layout();
        let current_layout = frames.current.surface.layout();
        let formats = (
            self.source_format(&reference_layout.pixel_format),
            self.source_format(&current_layout.pixel_format),
        );
        let (
            (Some(reference_format), Some(current_format)),
            Some(estimator),
            Some((width, height)),
        ) = (formats, self.estimator.as_mut(), self.dimensions)
        else {
            return Err(Status::InvalidSession.code());
        };
        let mut read = |reference: &[u8], current: &[u8]| -> Result<MotionField, OsStatus> {
            let reference_planes = split_source(&reference_layout, reference)?;
            let current_planes = split_source(&current_layout, current)?;
            if !planes_hold(&reference_planes, reference_format, width, height)
                || !planes_hold(&current_planes, current_format, width, height)
            {
                log::warn!("motion-estimation source: its planes do not hold the picture");
                return Err(Status::Parameter.code());
            }
            estimator
                .estimate(
                    &MotionPicture {
                        pixel_format: reference_format,
                        planes: &reference_planes,
                    },
                    &MotionPicture {
                        pixel_format: current_format,
                        planes: &current_planes,
                    },
                )
                .inspect(|field| {
                    log::debug!(
                        "motion estimated: {}x{} blocks of {}",
                        field.columns,
                        field.rows,
                        field.block_size
                    );
                })
                .map_err(|error| {
                    log::warn!("the motion estimator failed: {error}");
                    motion_status(&error)
                })
        };
        // Each frame is copied out in a lend of its own, and the estimate
        // runs with none held: a host may lend one surface at a time in
        // the process, as reims-vgpu does, so a read inside another waits
        // on itself for ever. One surface lent as both frames is copied
        // once.
        let copy = |surface: &SurfaceHold| -> Result<Vec<u8>, OsStatus> {
            let mut bytes = Vec::new();
            surface
                .surface
                .read(&mut |memory| bytes = memory.to_vec())
                .map_err(|error| {
                    log::warn!("motion-estimation source: {error}");
                    Status::Parameter.code()
                })?;
            Ok(bytes)
        };
        let reference = copy(&frames.reference)?;
        if frames.reference.mapping_id == frames.current.mapping_id {
            return read(&reference, &reference);
        }
        let current = copy(&frames.current)?;
        read(&reference, &current)
    }

    /// Lets the pair's frames go, but for a current frame to keep: the
    /// frame kept before goes now, its next frame done.
    fn let_frames_go(&mut self, frames: FramePair) {
        self.let_frames_go_with(frames, ());
    }

    /// [`Worker::let_frames_go`], letting go of `also` in the same step.
    /// In one step for every other sweep: the device's thread sweeps as it
    /// delivers the `Mbuf` reply this follows, and a sweep between the two
    /// sources' ends would split their `rels`.
    fn let_frames_go_with<T>(&mut self, frames: FramePair, also: T) {
        let previous = self.kept.take();
        if frames.keep_current {
            self.kept = Some(frames.current.clone());
        }
        self.link.let_go((frames, previous, also));
    }

    /// `Mbuf`: a vector buffer for `request`, its surface, waited for 1.1 s;
    /// the guest's `,err`, a UUID the host knows with another surface, or a
    /// reply without a surface is the frame's status.
    fn create_vector_buffer(&mut self, request: i64) -> Result<Arc<SurfaceHold>, OsStatus> {
        let code = |error: WireError| error.status().code();
        let reply = self
            .link
            .request(
                names::CREATE_MOTION_VECTOR_BUFFER,
                DEFAULT_REPLY_TIMEOUT,
                |message| {
                    message.s64(names::REQUEST, request);
                },
            )
            .map_err(Status::code)?;
        let message = Message::parse(&reply.bytes).map_err(code)?;
        let status = message.s32(names::ERROR).map_err(code)?.unwrap_or(0);
        if status != NO_ERROR {
            return Err(status);
        }
        let uuid = message.uuid(names::PIXEL_BUFFER_UUID).map_err(code)?;
        let (Some(uuid), Some(surface)) = (uuid, reply.surface) else {
            // Apple's makes its pixel buffer of the surface, and fails
            // without one.
            return Err(Status::Parameter.code());
        };
        self.make_room_for(1);
        self.pixel_buffers.remember(uuid, &surface).map_err(code)?;
        Ok(surface)
    }

    /// Cleans up first where `count` more pixel buffers would fill the
    /// table past [`MAX_PIXEL_BUFFERS`]. Only malformed frames leave so
    /// many, and the clean-up forgets those whose surfaces have gone. A
    /// table still full holds buffers still lent, as when queued messages
    /// lend one surface under many UUIDs, and a frame past it is malformed.
    fn make_room_for(&mut self, count: usize) {
        if !self.pixel_buffers.has_room_for(count) {
            self.clean_up();
        }
    }

    /// Writes `field` into the guest's vector buffer on Apple's grid, as
    /// many of its vectors as the buffer holds. A buffer that is not
    /// `2C0h` is -12902 (ours).
    fn write_vectors(&self, field: &MotionField, buffer: &SurfaceHold) -> Result<(), OsStatus> {
        let (width, height) = self.dimensions.ok_or(Status::InvalidSession.code())?;
        let grid = self.grid(width, height);
        let layout = buffer.surface.layout();
        if layout.pixel_format != VECTOR_FORMAT {
            log::warn!("a motion vector buffer in '{}'", layout.pixel_format);
            return Err(Status::Parameter.code());
        }
        let columns = grid.columns().min(layout.width);
        let rows = grid.rows().min(layout.height);
        let mut written = Err(Status::SessionMalfunction.code());
        let mut vectors = Vec::new();
        buffer
            .surface
            .write(&mut |memory| {
                written = match layout.split(memory) {
                    Ok(mut planes) => match planes.first_mut() {
                        Some(plane) => {
                            for row in 0..rows.min(plane.rows) {
                                resample_row(field, &grid, row, columns, &mut vectors);
                                let start = usize::try_from(row)
                                    .ok()
                                    .and_then(|row| row.checked_mul(plane.bytes_per_row));
                                let Some(bytes) =
                                    start.and_then(|start| plane.bytes.get_mut(start..))
                                else {
                                    break;
                                };
                                let room = bytes.len().min(plane.bytes_per_row) / 4;
                                let fit = room.min(usize::try_from(columns).unwrap_or(0));
                                for (slot, vector) in bytes
                                    .as_chunks_mut::<4>()
                                    .0
                                    .iter_mut()
                                    .take(fit)
                                    .zip(&vectors)
                                {
                                    slot.copy_from_slice(&half_float_pair(*vector));
                                }
                            }
                            Ok(())
                        }
                        None => Err(Status::Parameter.code()),
                    },
                    Err(error) => {
                        log::warn!("motion vector buffer: {error}");
                        Err(Status::Parameter.code())
                    }
                };
            })
            .map_err(|error| {
                log::warn!("motion vector buffer: {error}");
                Status::Parameter.code()
            })?;
        written
    }

    /// `Memt`: the request, its status, the info flags (0) and the vector
    /// buffer's UUID when there is a buffer, waited for 10 s; the guest's
    /// answer is not read (measured on macOS 27: no `,mad` and no
    /// attachments).
    fn emit(&mut self, request: i64, status: OsStatus, buffer: Option<&Arc<SurfaceHold>>) {
        let uuid = buffer.and_then(|buffer| self.pixel_buffers.take(buffer));
        let sent = self.link.request(
            names::EMIT_MOTION_VECTORS,
            CODEC_OPERATION_REPLY_TIMEOUT,
            |message| {
                message.s64(names::REQUEST, request);
                message.s32(names::ERROR, status);
                message.u32(names::INFO_FLAGS, 0);
                if let Some(uuid) = uuid {
                    message.uuid(names::PIXEL_BUFFER_UUID, uuid);
                }
            },
        );
        if let Err(error) = sent {
            log::warn!("emitting motion request {request} failed: {error:?}");
        }
    }

    /// `Mdpb`: the UUIDs of the buffers the host has let go, when there are
    /// some, with no flags and no reply.
    fn clean_up(&mut self) {
        let dead = self.pixel_buffers.dead();
        if dead.is_empty() {
            return;
        }
        let mut message = MessageWriter::new(
            names::DROPPED_PIXEL_BUFFERS,
            MessageFlags::NONE,
            Some(self.link.guest_uuid),
            0,
        );
        message.uuid_array(names::PIXEL_BUFFER_UUIDS, &dead);
        self.send(message.finish());
    }
}

/// A source's planes in its memory.
fn split_source<'a>(
    layout: &SurfaceLayout,
    memory: &'a [u8],
) -> Result<Vec<PlaneSource<'a>>, OsStatus> {
    layout.split_for_reading(memory).map_err(|error| {
        log::warn!("motion-estimation source: {error}");
        Status::Parameter.code()
    })
}

/// The status the guest sees for a backend's failure: a size or source it
/// cannot take is a parameter error, anything else the session's
/// malfunction; a [`BackendError::Native`] status goes as it is.
fn motion_status(error: &BackendError) -> OsStatus {
    match error {
        BackendError::Unsupported(_) | BackendError::BadData(_) => Status::Parameter.code(),
        // A decoder's alone: none reaches here.
        BackendError::Malfunction(_) | BackendError::SkippedLeadingPicture(_) => {
            Status::SessionMalfunction.code()
        }
        BackendError::Native { status, .. } => {
            BackendError::native_status(*status).unwrap_or(Status::SessionMalfunction.code())
        }
    }
}

#[cfg(test)]
mod tests;
