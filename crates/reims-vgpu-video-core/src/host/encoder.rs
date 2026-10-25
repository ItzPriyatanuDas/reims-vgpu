//! Encoder sessions: what the host does for each `+enc` the guest's
//! VideoToolbox opens, following Apple's host encoder session
//! with a [`BackendEncoder`] where Apple has a `VTVideoEncoder`.
//!
//! Each session has a thread of its own, its serial queue: the device's
//! thread hands it each message and returns at once, and the session's
//! thread handles them in order. The guest lends each source frame's surface
//! with its `efrm`; the session copies the frame out of it and hands the
//! copy to the backend with no lend held (ours: [`source_copy`]), and emits
//! what comes out with `Eemt`, a sample buffer the guest's VideoToolbox
//! hands its client. Before the first one it asks the guest to make the
//! stream's format description (`Efds`), as Apple's encoders ask their
//! session. A frame's surface is held until it has been both encoded and
//! emitted, as Apple's pending frames hold their pixel buffers.

mod properties;
mod source_copy;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use crate::backend::{BackendError, Codec, PixelFormat};
use crate::encoder_backend::{
    BackendEncoder, EncodedFrame, EncoderBackend, EncoderCapabilities, EncoderConfig, SourceFrame,
};
use crate::fourcc::FourCc;
use crate::host::HostShared;
use crate::host::codec_lists::{apple_encoder_id, codec_name, encoder_id};
use crate::host::guest_bounds::GUEST_BUFFER_SIZE;
use crate::host::mapped_surfaces::SurfaceHold;
use crate::host::names::{self, EncoderMessage};
use crate::host::refused::{RefusedKind, RefusedSession};
use crate::host::reply_clerk::{CODEC_OPERATION_REPLY_TIMEOUT, DEFAULT_REPLY_TIMEOUT};
use crate::host::session::{
    Created, Job, SessionEntry, SessionKind, SessionLink, SessionQueue, create_reply, property_key,
    reply, reply_error,
};
use crate::runtime::Runtime;
use crate::status::{NO_ERROR, OsStatus, Status};
use crate::surfaces::PlaneSource;
use crate::wire::WireError;
use crate::wire::image_description::{
    AV1_CONFIGURATION, AVC_CONFIGURATION, HEVC_CONFIGURATION, ImageDescription, VP9_CONFIGURATION,
};
use crate::wire::message::{
    GuestUuid, MediaTime, Message, MessageFlags, MessageWriter, ValueType, read_header,
};
use crate::wire::property_list::{dictionary, integer};
use crate::wire::sample_buffer::{
    MEDIA_TYPE_VIDEO, SampleBuffer, SampleTiming, SerializedFormatDescription, SerializedValue,
};

use self::properties::{EncoderProperties, MULTI_PASS_STORAGE, PARAVIRTUALIZED, SetOutcome};
use self::source_copy::{EncodeTimes, SourceCopy};

/// An encoder session's thread (ours) and log name.
const KIND: SessionKind = SessionKind {
    thread: "reims-vgpu-video.encoder",
    name: "an encoder session",
};

/// The first guest protocol that answers `Epba`: the host waits for its
/// reply only from protocol 3 on.
const EPBA_REPLY_PROTOCOL: u32 = 3;

/// `kVTEncodeFrameOptionKey_ForceKeyFrame`, the frame option a client
/// forces a key frame with, which Apple's host passes on to its encoder in
/// `,fpr`. Its value as the SDK's constant holds it, printed on macOS 27.0.1
/// (26A434, 2026-10-24).
const FORCE_KEY_FRAME: &str = "EncoderForceKeyframe";

/// The alignments the host asks the guest's source buffers to have: Apple's
/// AVC encoder asks for 64 for both (measured on macOS 27, 26A434,
/// 2026-10-24: its `Epba`). Our encoders copy each frame, so they only make
/// rows start where a copy is quick.
const SOURCE_ALIGNMENT: i64 = 64;
/// `kVTEncodeInfo_FrameDropped` (`VTErrors.h`): an `Eemt`'s info flag for a
/// frame the encoder dropped, which comes with no sample.
const INFO_FRAME_DROPPED: u32 = 1 << 1;

/// Makes the session `+enc` asks for and its create reply. Its `,cod`
/// must be nonzero, and every parameter must read, or there is no
/// session and no reply.
///
/// `,cid` may name this host's encoder for the codec (the guest's
/// registered `paravirtualized:` encoders send it), Apple's AVE encoder
/// for it (the guest's two fixed encoders send that), or nothing, which
/// picks the codec's encoder. Any other ID, a codec the backend does not
/// list, or an encoder for another codec makes a session whose reply
/// says -12908, as Apple's selection finds none (measured on macOS 27,
/// 26A434, 2026-10-24: an unknown ID, and `ave.hevc` for `avc1`). A
/// listed codec the backend cannot make an encoder of now
/// ([`EncoderBackend::session_capabilities`]) makes a session whose
/// reply says the backend's status, as Apple's host puts its encoder's
/// creation status there and keeps the session.
pub(crate) fn create(message: &Message<'_>, host: &Arc<HostShared>) -> Created {
    let status = |error: WireError| error.status();
    let codec_type = message.s32(names::CODEC).map_err(status)?.unwrap_or(0);
    if codec_type == 0 {
        return Err(Status::MalformedMessage);
    }
    let guest_version = message.u32(names::VERSION).map_err(status)?.unwrap_or(0);
    let encoder_asked = message.property_list(names::CODEC_ID).map_err(status)?;
    message.dictionary(names::GUEST_INFO).map_err(status)?;
    let capabilities = Codec::from_fourcc(FourCc(codec_type.cast_unsigned()))
        .filter(|&codec| {
            encoder_asked.as_ref().is_none_or(|asked| {
                asked.as_string() == Some(encoder_id(codec))
                    || apple_encoder_id(codec).is_some_and(|id| asked.as_string() == Some(id))
            })
        })
        .map(|codec| host.encoder_backend.session_capabilities(codec))
        .transpose();
    let capabilities = match capabilities {
        Ok(Some(Some(capabilities))) => capabilities,
        Ok(_) => {
            let (session, reply) =
                RefusedSession::create(message, RefusedKind::Encoder).map_err(status)?;
            return Ok((SessionEntry::Refused(session), reply));
        }
        Err(code) => {
            log::warn!("encoder session refused for now: {code}");
            let (session, reply) =
                RefusedSession::create_with_status(message, RefusedKind::Encoder, code)
                    .map_err(status)?;
            return Ok((SessionEntry::Refused(session), reply));
        }
    };
    let session = start(
        message.header().guest_uuid,
        guest_version,
        capabilities,
        host,
    )?;
    let reply = create_reply(message, NO_ERROR).map_err(status)?;
    Ok((SessionEntry::Running(session), reply))
}

/// Starts an encoding session, its thread waiting for its first message.
fn start(
    guest_uuid: GuestUuid,
    guest_version: u32,
    capabilities: EncoderCapabilities,
    host: &Arc<HostShared>,
) -> Result<SessionQueue, Status> {
    let link = SessionLink::new(guest_uuid, host);
    let (jobs, queue) = mpsc::channel();
    let worker = Worker {
        link: link.clone(),
        backend: host.encoder_backend.clone(),
        guest_version,
        properties: EncoderProperties::new(capabilities.clone()),
        capabilities,
        encoder: None,
        dimensions: None,
        pending: BTreeMap::new(),
        format: None,
        multi_pass_storage: false,
        source: SourceCopy::default(),
        times: EncodeTimes::default(),
        runtime: host.runtime.clone(),
    };
    SessionQueue::start(link, host, &KIND, jobs, move || worker.run(&queue))
}

/// A frame the guest handed the host, until it has been both encoded and
/// emitted: the UUID the guest keeps it under, and its surface.
struct PendingFrame {
    uuid: Option<GuestUuid>,
    _surface: Arc<SurfaceHold>,
    submitted: bool,
    emitted: bool,
}

/// The format description the session's samples carry, and what made it.
struct StreamFormat {
    configuration: Vec<u8>,
    description: SerializedFormatDescription,
    /// Whether the samples have carried it yet: the first one after a
    /// change does, as Apple's serializer writes it when it differs from
    /// the last one written.
    written: bool,
}

/// The session's thread and what only it touches.
struct Worker {
    link: Arc<SessionLink>,
    backend: Arc<dyn EncoderBackend>,
    /// The guest's protocol version, from `+enc`.
    guest_version: u32,
    capabilities: EncoderCapabilities,
    properties: EncoderProperties,
    encoder: Option<Box<dyn BackendEncoder>>,
    /// `esta`'s dimensions: the source frames'. The stream's are
    /// [`encoded_size`] of them.
    dimensions: Option<(u32, u32)>,
    pending: BTreeMap<i64, PendingFrame>,
    format: Option<StreamFormat>,
    /// Whether Apple's host would hold a multi-pass storage for the guest
    /// now: [`Worker::set_multi_pass_storage`].
    multi_pass_storage: bool,
    /// The latest source frame, copied out of its surface for the backend.
    source: SourceCopy,
    times: EncodeTimes,
    /// The clock the times are on.
    runtime: Arc<dyn Runtime>,
}

/// What a handler does when its message does not read: Apple's handlers
/// send no reply, and the guest's wait times out.
type Handled = Result<(), WireError>;

impl Worker {
    fn run(mut self, queue: &Receiver<Job>) {
        while let Ok(job) = queue.recv() {
            match job {
                Job::Message {
                    bytes,
                    surfaces,
                    held,
                } => {
                    if !self.link.is_invalidated() {
                        self.handle(&bytes, &surfaces);
                    }
                    drop(held);
                    if !surfaces.is_empty() {
                        // Apple's block lets the message's surfaces go and
                        // sweeps.
                        self.link.let_go(surfaces);
                    }
                }
                Job::Invalidate(done) => {
                    self.encoder = None;
                    // In one `rels`, where a session destroyed with frames
                    // pending lets them go.
                    let pending = std::mem::take(&mut self.pending);
                    self.link.let_go(pending);
                    self.log_times();
                    done.give(());
                }
            }
        }
        self.log_times();
    }

    /// Logs where the frames since the last log spent their time.
    fn log_times(&mut self) {
        if let Some(summary) = std::mem::take(&mut self.times).summary() {
            log::info!("encoder session ends: {summary}");
        }
    }

    fn handle(&mut self, bytes: &[u8], surfaces: &[Arc<SurfaceHold>]) {
        let header = match read_header(bytes, 0) {
            Ok(header) => header,
            Err(error) => {
                log::warn!("encoder session message does not read: {error}");
                return;
            }
        };
        let Some(kind) = EncoderMessage::from_fourcc(header.message_type) else {
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
        kind: EncoderMessage,
        message: &Message<'_>,
        surfaces: &[Arc<SurfaceHold>],
    ) -> Handled {
        match kind {
            EncoderMessage::CopyProperty => self.copy_property(message),
            EncoderMessage::SetProperty => self.set_property(message),
            EncoderMessage::SetProperties => self.set_properties(message),
            EncoderMessage::SupportedProperties => self.supported_properties(message),
            EncoderMessage::StartSession => self.start_session(message),
            EncoderMessage::PrepareToEncodeFrames => self.prepare_to_encode_frames(message),
            EncoderMessage::EncodeFrame => self.encode_frame(message, surfaces),
            EncoderMessage::CompleteFrames => self.complete_frames(message),
            EncoderMessage::Invalidate => {
                // The pending frames go with the encoder, and their surfaces
                // with the sweep before the reply, so that the guest has its
                // `rels` by the time its invalidate returns: in one, as they
                // go together.
                self.encoder = None;
                let pending = std::mem::take(&mut self.pending);
                self.link.let_go(pending);
                self.log_times();
                self.reply_status(message, NO_ERROR)
            }
            EncoderMessage::SerializableProperties => {
                // Apple's AVC encoder has none to give (measured on macOS
                // 27: -12782), and neither has this host's.
                self.reply_status(message, Status::UnimplementedFunction.code())
            }
            EncoderMessage::EncodeMultiImageFrame => self.encode_multi_image_frame(message),
            EncoderMessage::BeginPass => {
                message.u32(names::BEGIN_PASS_FLAGS)?;
                self.reply_status(message, Status::UnimplementedFunction.code())
            }
            EncoderMessage::EndPass => {
                // `,fpr` before `,err`, as Apple's.
                let mut reply = reply(message);
                reply.s32(names::FRAME_PROPERTIES, 0);
                reply.s32(names::ERROR, Status::UnimplementedFunction.code());
                self.send(reply.finish());
                Ok(())
            }
            EncoderMessage::StartTileSession => self.start_tile_session(message),
            EncoderMessage::PrepareToEncodeTiles => {
                message.s32(names::TILE_COUNT)?;
                self.reply_status(message, Status::UnimplementedFunction.code())
            }
            EncoderMessage::EncodeTile => self.encode_tile(message, surfaces),
            EncoderMessage::CompleteTiles => {
                self.reply_status(message, Status::UnimplementedFunction.code())
            }
        }
    }

    fn send(&self, message: Vec<u8>) {
        if let Err(status) = self.link.send(message) {
            log::debug!("encoder session send dropped: {status:?}");
        }
    }

    fn reply_status(&self, message: &Message<'_>, status: OsStatus) -> Handled {
        let mut reply = reply(message);
        reply.s32(names::ERROR, status);
        self.send(reply.finish());
        Ok(())
    }

    /// `ecop`: a property's value. The key must be a string, or there is
    /// no reply.
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

    /// `eset`: sets a property. The key must be a string and not
    /// `Paravirtualized`, which the host keeps for itself, or there is no
    /// reply.
    fn set_property(&mut self, message: &Message<'_>) -> Handled {
        let key = property_key(message)?;
        if key == PARAVIRTUALIZED {
            return Err(WireError::Malformed("a property the host handles"));
        }
        let value = message.property_list(names::PROPERTY_VALUE)?;
        let status = self.set(&key, value);
        self.reply_status(message, status)
    }

    /// `emul`: `,pro` without `Paravirtualized` must not be empty, or there
    /// is no reply. Each is set in turn; the first refusal is the answer.
    fn set_properties(&mut self, message: &Message<'_>) -> Handled {
        let properties = message.dictionary(names::PROPERTIES)?.unwrap_or_default();
        let properties: Vec<(String, plist::Value)> = properties
            .into_iter()
            .filter(|(key, _)| key != PARAVIRTUALIZED)
            .collect();
        if properties.is_empty() {
            return Err(WireError::Malformed("no properties for the encoder"));
        }
        let mut status = NO_ERROR;
        for (key, value) in properties {
            let set = self.set(&key, Some(value));
            if status == NO_ERROR {
                status = set;
            }
        }
        self.reply_status(message, status)
    }

    /// Sets one property, and passes the settings on to an encoder that is
    /// running.
    /// The session's table decides first, then the backend
    /// ([`EncoderBackend::check_property`]), then an encoder that is
    /// running; a refusal leaves the property as it was.
    fn set(&mut self, key: &str, value: Option<plist::Value>) -> OsStatus {
        if key == MULTI_PASS_STORAGE {
            return self.set_multi_pass_storage(value.as_ref());
        }
        let earlier = self.properties.value(key);
        if let SetOutcome::Refused(status) = self.properties.set(key, value.clone()) {
            return status;
        }
        if let Some(value) = &value
            && let Err(status) = self
                .backend
                .check_property(self.capabilities.codec, key, value)
        {
            self.properties.restore(key, earlier);
            return status;
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return NO_ERROR;
        };
        match encoder.reconfigure(&self.properties.settings()) {
            Ok(()) => NO_ERROR,
            Err(error) => {
                log::warn!("the encoder cannot take {key} now: {error}");
                self.properties.restore(key, earlier);
                match error {
                    BackendError::Unsupported(_) => Status::PropertyNotSupported.code(),
                    other => encoder_status(&other),
                }
            }
        }
    }

    /// `MultiPassStorage`, which Apple's host translates before its encoder
    /// sees it: true makes a host storage, and anything else releases the
    /// one the session holds. The release asks the guest to invalidate its
    /// storage with `mpin` and waits up to 1.1 s for the reply, whose answer
    /// changes nothing. A second true replaces the storage without releasing
    /// the first, so it sends nothing, and the session's end clears the
    /// storage's way back first, so it sends nothing either. This host's
    /// encoders do one pass and have no such property, so the set is then
    /// refused with -12900, as Apple's HEVC software encoder refuses it
    /// (measured on Apple's host).
    fn set_multi_pass_storage(&mut self, value: Option<&plist::Value>) -> OsStatus {
        if value.and_then(plist::Value::as_boolean) == Some(true) {
            self.multi_pass_storage = true;
        } else if std::mem::take(&mut self.multi_pass_storage)
            && let Err(status) = self.link.request(
                names::MULTI_PASS_STORAGE_INVALIDATE,
                DEFAULT_REPLY_TIMEOUT,
                |_| {},
            )
        {
            log::debug!("the guest did not invalidate its multi-pass storage: {status:?}");
        }
        Status::PropertyNotSupported.code()
    }

    /// `esup`: the properties this session's encoder supports.
    fn supported_properties(&self, message: &Message<'_>) -> Handled {
        let mut reply = reply(message);
        reply.s32(names::ERROR, NO_ERROR);
        reply.property_list(
            names::SUPPORTED_PROPERTIES,
            Some(&plist::Value::Dictionary(self.properties.supported())),
        )?;
        self.send(reply.finish());
        Ok(())
    }

    /// `epfr`: prepares the running encoder, as Apple's host calls its
    /// encoder's `PrepareToEncodeFrames` and replies with its status. Before
    /// `esta` there is no encoder to prepare, and the answer is 0, as AVE's
    /// was.
    fn prepare_to_encode_frames(&mut self, message: &Message<'_>) -> Handled {
        let status = match self.encoder.as_mut().map(|encoder| encoder.prepare()) {
            None | Some(Ok(())) => NO_ERROR,
            Some(Err(error)) => {
                log::warn!("the encoder did not prepare: {error}");
                encoder_status(&error)
            }
        };
        self.reply_status(message, status)
    }

    /// `esta`: opens the backend encoder for the picture's dimensions, asks
    /// the guest for source buffers it takes (`Epba`) and replies with the
    /// status (measured on macOS 27: `Epba`, its reply, then `esta`'s). A
    /// dimension under 1 is answered with -19093. An odd dimension is taken,
    /// as Apple's AVE takes it: the source buffers keep the odd size, and
    /// the stream is [`encoded_size`]. A second `esta` first completes the
    /// frames the encoder before it holds: Apple's host starts its one
    /// encoder again, and ours makes a new one, which would otherwise drop
    /// them with their surfaces held.
    fn start_session(&mut self, message: &Message<'_>) -> Handled {
        let (width, height) = message
            .i32_pair(names::DIMENSIONS, ValueType::Dimensions)?
            .unwrap_or((0, 0));
        let (Ok(width @ 1..), Ok(height @ 1..)) = (u32::try_from(width), u32::try_from(height))
        else {
            return self.reply_status(message, Status::MalformedMessage.code());
        };
        let (encoded_width, encoded_height) = encoded_size(width, height);
        let status = if width > self.capabilities.max_width || height > self.capabilities.max_height
        {
            // As Apple's host answers a size its encoder does not take.
            Status::EncoderSizeUnsupported.code()
        } else if encoded_width == 0 || encoded_height == 0 {
            // A side of 1 leaves nothing to encode: the answer every
            // backend gave such a size before odd sizes were taken.
            log::warn!("cannot open an encoder: {width}x{height} has no even size to encode");
            Status::Parameter.code()
        } else {
            let config = EncoderConfig {
                codec: self.capabilities.codec,
                width: encoded_width,
                height: encoded_height,
                settings: self.properties.settings(),
            };
            if self.encoder.is_some() {
                let completed = self.complete_held_frames();
                if completed != NO_ERROR {
                    log::warn!("the encoder before this start completed with {completed}");
                }
            }
            match self.backend.open(&config) {
                Ok(encoder) => {
                    self.encoder = Some(encoder);
                    self.dimensions = Some((width, height));
                    self.format = None;
                    self.set_source_pixel_buffer_attributes(width, height)
                }
                Err(error) => {
                    log::warn!("cannot open an encoder: {error}");
                    encoder_status(&error)
                }
            }
        };
        self.reply_status(message, status)
    }

    /// `Epba`: the attributes the guest's source buffers are to have: the
    /// formats the backend takes, the session's size, IOSurface backing and
    /// Apple's AVC encoder's alignments. A guest of protocol 3 or later
    /// answers it within 1.1 s; an older one is not waited for.
    fn set_source_pixel_buffer_attributes(&self, width: u32, height: u32) -> OsStatus {
        let formats = self
            .capabilities
            .source_formats
            .iter()
            .map(|format| integer(i64::from(format.fourcc().0)))
            .collect();
        let attributes = plist::Value::Dictionary(dictionary([
            ("PixelFormatType", plist::Value::Array(formats)),
            ("Width", integer(i64::from(width))),
            ("Height", integer(i64::from(height))),
            (
                "IOSurfaceProperties",
                plist::Value::Dictionary(plist::Dictionary::new()),
            ),
            ("BytesPerRowAlignment", integer(SOURCE_ALIGNMENT)),
            ("PlaneAlignment", integer(SOURCE_ALIGNMENT)),
        ]));
        if self.guest_version < EPBA_REPLY_PROTOCOL {
            let mut message = MessageWriter::new(
                names::SET_SOURCE_PIXEL_BUFFER_ATTRIBUTES,
                MessageFlags::NONE,
                Some(self.link.guest_uuid),
                0,
            );
            if let Err(error) =
                message.property_list(names::PIXEL_BUFFER_ATTRIBUTES, Some(&attributes))
            {
                return error.status().code();
            }
            return match self.link.send(message.finish()) {
                Ok(()) => NO_ERROR,
                Err(status) => status.code(),
            };
        }
        let mut written = Ok(());
        let reply = self.link.request(
            names::SET_SOURCE_PIXEL_BUFFER_ATTRIBUTES,
            DEFAULT_REPLY_TIMEOUT,
            |message| {
                written = message
                    .property_list(names::PIXEL_BUFFER_ATTRIBUTES, Some(&attributes))
                    .map(|_| ());
            },
        );
        if let Err(error) = written {
            return error.status().code();
        }
        reply_error(reply)
    }

    /// `efrm`: one source frame. It needs exactly one surface, and every
    /// parameter must read, or there is no reply. The frame waits as pending
    /// until it is both encoded and emitted; the reply carries the encode's
    /// status and the info flags, 0.
    fn encode_frame(&mut self, message: &Message<'_>, surfaces: &[Arc<SurfaceHold>]) -> Handled {
        let frame_id = message.s64(names::FRAME)?.unwrap_or(0);
        let [surface] = surfaces else {
            return Err(WireError::Malformed("a frame without its one surface"));
        };
        let presentation_time = message
            .time(names::PRESENTATION_TIME)?
            .unwrap_or(MediaTime::INVALID);
        let duration = message.time(names::DURATION)?.unwrap_or(MediaTime::INVALID);
        let frame_properties = message.dictionary(names::FRAME_PROPERTIES)?;
        let uuid = message.uuid(names::PIXEL_BUFFER_UUID)?;
        message.dictionary(names::PIXEL_BUFFER_ATTACHMENTS)?;
        let force_key_frame = frame_properties
            .as_ref()
            .and_then(|properties| properties.get(FORCE_KEY_FRAME))
            .and_then(plist::Value::as_boolean)
            .unwrap_or(false);
        self.pending.entry(frame_id).or_insert(PendingFrame {
            uuid,
            _surface: surface.clone(),
            submitted: false,
            emitted: false,
        });
        let status = match self.encode(
            surface,
            frame_id,
            presentation_time,
            duration,
            force_key_frame,
        ) {
            Ok(frames) => {
                for frame in frames {
                    self.emit(&frame);
                }
                NO_ERROR
            }
            Err(status) => status,
        };
        let mut reply = reply(message);
        reply.s32(names::ERROR, status);
        reply.u32(names::INFO_FLAGS, 0);
        self.send(reply.finish());
        self.frame_submitted(frame_id, status != NO_ERROR);
        Ok(())
    }

    /// Copies the frame out of its surface, in the surface's read lend,
    /// then hands the copy to the backend with no lend held
    /// ([`source_copy`]).
    fn encode(
        &mut self,
        surface: &SurfaceHold,
        frame_id: i64,
        presentation_time: MediaTime,
        duration: MediaTime,
        force_key_frame: bool,
    ) -> Result<Vec<EncodedFrame>, OsStatus> {
        if self.encoder.is_none() {
            // No session started: VideoToolbox's answer for an encoder
            // without one.
            return Err(Status::InvalidSession.code());
        }
        let layout = surface.surface.layout();
        let pixel_format = PixelFormat::from_fourcc(layout.pixel_format)
            .filter(|format| self.capabilities.source_formats.contains(format))
            .ok_or_else(|| {
                log::warn!("a source frame in '{}'", layout.pixel_format);
                Status::Parameter.code()
            })?;
        if self.dimensions != Some((layout.width, layout.height)) {
            log::warn!(
                "a {}x{} source frame for a {:?} session",
                layout.width,
                layout.height,
                self.dimensions
            );
            return Err(Status::Parameter.code());
        }
        // The frame's top left at the stream's size: the guest's planes hold
        // more by a column or a row where that is odd.
        let (width, height) = encoded_size(layout.width, layout.height);
        let copying = self.runtime.now();
        let source = &mut self.source;
        let mut copied = Err(Status::VideoEncoderMalfunction.code());
        surface
            .surface
            .read(&mut |memory| {
                copied = match layout.split_for_reading(memory) {
                    Ok(planes)
                        if !planes_hold(&planes, pixel_format, layout.width, layout.height) =>
                    {
                        log::warn!("source buffer: its planes do not hold the picture");
                        Err(Status::Parameter.code())
                    }
                    Ok(planes) if !source.copy(&planes, pixel_format, width, height) => {
                        log::warn!("source buffer: its planes do not hold the picture's bytes");
                        Err(Status::Parameter.code())
                    }
                    Ok(_) => Ok(()),
                    Err(error) => {
                        log::warn!("source buffer: {error}");
                        Err(Status::Parameter.code())
                    }
                };
            })
            .map_err(|error| {
                log::warn!("source buffer: {error}");
                Status::Parameter.code()
            })?;
        copied?;
        self.times
            .add(|times| &mut times.copy, self.runtime.elapsed_since(copying));
        let Some(encoder) = self.encoder.as_mut() else {
            return Err(Status::InvalidSession.code());
        };
        let planes = self.source.planes();
        let encoding = self.runtime.now();
        let encoded = encoder
            .encode(&SourceFrame {
                frame_id,
                presentation_time,
                duration,
                force_key_frame,
                pixel_format,
                width,
                height,
                planes: &planes,
            })
            .map_err(|error| {
                log::warn!("frame {frame_id} does not encode: {error}");
                encoder_status(&error)
            });
        self.times.add(
            |times| &mut times.encode,
            self.runtime.elapsed_since(encoding),
        );
        self.times.frames += 1;
        encoded
    }

    /// The encode call for `frame_id` returned: the frame is taken when it
    /// has been emitted too, or at once when the encode failed.
    fn frame_submitted(&mut self, frame_id: i64, failed: bool) {
        if let Some(pending) = self.pending.get_mut(&frame_id) {
            pending.submitted = true;
            if pending.emitted || failed {
                self.pending.remove(&frame_id);
            }
        }
    }

    /// The frame was emitted: the UUID the guest keeps it under, and the
    /// frame, taken when its encode has returned too. The caller keeps a
    /// taken frame, and so its surface, until its `Eemt` has gone and been
    /// answered, then sweeps, so that the surface's `rels` follows the
    /// `Eemt` whichever thread's sweep sends it, as Apple's sweep follows
    /// the emit. When it went before the send, the device's thread's sweep
    /// now and then sent the `rels` first.
    fn frame_emitted(&mut self, frame_id: i64) -> (Option<GuestUuid>, Option<PendingFrame>) {
        let Some(pending) = self.pending.get_mut(&frame_id) else {
            return (None, None);
        };
        pending.emitted = true;
        let uuid = pending.uuid;
        let taken = if pending.submitted {
            self.pending.remove(&frame_id)
        } else {
            None
        };
        (uuid, taken)
    }

    /// `ecfr`: emits every frame the backend still holds, then replies. The
    /// time stamp is read; this host completes every frame.
    fn complete_frames(&mut self, message: &Message<'_>) -> Handled {
        message.time(names::PRESENTATION_TIME)?;
        let status = self.complete_held_frames();
        self.reply_status(message, status)
    }

    /// Emits every frame the backend still holds, then each frame it took
    /// and will not give back, with -12912 and no sample: frames a failed
    /// picture lost with it. A guest's client gets an answer for every frame
    /// it gave, as from an encoder that emits a failed frame with its error,
    /// and its surface goes.
    fn complete_held_frames(&mut self) -> OsStatus {
        let status = match self.encoder.as_mut().map(|encoder| encoder.flush()) {
            None => NO_ERROR,
            Some(Ok(frames)) => {
                for frame in frames {
                    self.emit(&frame);
                }
                NO_ERROR
            }
            Some(Err(error)) => {
                log::warn!("the encoder does not complete its frames: {error}");
                encoder_status(&error)
            }
        };
        let lost: Vec<i64> = (self.pending.iter())
            .filter(|(_, pending)| pending.submitted && !pending.emitted)
            .map(|(&frame_id, _)| frame_id)
            .collect();
        let mut surfaces = Vec::with_capacity(lost.len());
        for &frame_id in &lost {
            log::warn!("frame {frame_id} was lost in the encoder");
            let (uuid, taken) = self.frame_emitted(frame_id);
            self.emit_encoded_frame(
                frame_id,
                Status::VideoEncoderMalfunction.code(),
                0,
                uuid,
                None,
            );
            surfaces.push(taken);
        }
        // Their surfaces go together, after the last `Eemt`.
        if !surfaces.is_empty() {
            self.link.let_go(surfaces);
        }
        status
    }

    /// Emits one encoded frame: the format description first when the
    /// stream's is new, then `Eemt` with the sample buffer, waited for
    /// 10 s, then the sweep.
    ///
    /// A frame the encoder dropped, one with no data, is emitted with
    /// `kVTEncodeInfo_FrameDropped` and no sample, as Apple's host forwards
    /// its encoder's dropped frames, and with no format description asked
    /// for. A frame the encoder failed is emitted the same way with its
    /// error's status and info flags 0, as Apple's host forwards each emit
    /// with its own status and flags: the frames before and after it keep
    /// their samples.
    fn emit(&mut self, frame: &EncodedFrame) {
        let unsampled = match &frame.error {
            Some(error) => {
                log::warn!("frame {} does not encode: {error}", frame.frame_id);
                Some((encoder_status(error), 0))
            }
            None if frame.data.is_empty() => Some((NO_ERROR, INFO_FRAME_DROPPED)),
            None => None,
        };
        if let Some((status, info_flags)) = unsampled {
            let (uuid, taken) = self.frame_emitted(frame.frame_id);
            self.emit_encoded_frame(frame.frame_id, status, info_flags, uuid, None);
            self.link.let_go(taken);
            return;
        }
        let configuration = self
            .encoder
            .as_ref()
            .map(|encoder| encoder.configuration())
            .unwrap_or_default();
        let sample = match self.stream_format(configuration) {
            Ok(format) => {
                let description = (!format.written).then(|| format.description.clone());
                format.written = true;
                Ok(sample_buffer(frame, description))
            }
            Err(status) => Err(status),
        };
        let (uuid, taken) = self.frame_emitted(frame.frame_id);
        let (status, serialization) = match sample {
            Ok(sample) => (NO_ERROR, Some(sample.serialize())),
            Err(status) => (status, None),
        };
        self.emit_encoded_frame(frame.frame_id, status, 0, uuid, serialization.as_deref());
        self.link.let_go(taken);
    }

    /// The stream's format description, asking the guest to make one
    /// (`Efds`) when the configuration record is new, as Apple's encoders
    /// ask their session before their first emission (measured on macOS 27
    /// with Apple's software H.264 and HEVC encoders).
    fn stream_format(&mut self, configuration: Vec<u8>) -> Result<&mut StreamFormat, OsStatus> {
        let current = self
            .format
            .as_ref()
            .is_some_and(|format| format.configuration == configuration);
        if !current {
            let description = self.create_video_format_description(&configuration)?;
            self.format = Some(StreamFormat {
                configuration,
                description,
                written: false,
            });
        }
        self.format
            .as_mut()
            .ok_or(Status::VideoEncoderMalfunction.code())
    }

    /// `Efds`: the codec, the size and the extensions, with the
    /// configuration record; the guest makes the format description and
    /// answers `,err` and `=fds`, within 1.1 s. The samples' description is
    /// the guest's, with the extensions CoreMedia reads from an
    /// ImageDescription (measured on macOS 27: what Apple's host serialized
    /// from the guest's `=fds`).
    fn create_video_format_description(
        &self,
        configuration: &[u8],
    ) -> Result<SerializedFormatDescription, OsStatus> {
        let codec = self.capabilities.codec;
        let (width, height) = self
            .dimensions
            .map(|(width, height)| encoded_size(width, height))
            .ok_or(Status::InvalidSession.code())?;
        let record = configuration_atom(codec);
        let mut extensions = plist::Dictionary::new();
        extensions.insert(
            "FormatName".to_owned(),
            plist::Value::String(codec_name(codec).to_owned()),
        );
        extensions.insert(
            "SampleDescriptionExtensionAtoms".to_owned(),
            plist::Value::Dictionary(dictionary([(
                record,
                plist::Value::Data(configuration.to_vec()),
            )])),
        );
        for (key, value) in self.properties.colour_extensions() {
            extensions.insert(key.to_owned(), plist::Value::String(value));
        }
        let mut written = Ok(());
        let reply = self.link.request(
            names::CREATE_VIDEO_FORMAT_DESCRIPTION,
            DEFAULT_REPLY_TIMEOUT,
            |message| {
                message.s32(names::CODEC, codec.fourcc().0.cast_signed());
                message.s32(names::WIDTH, i32::try_from(width).unwrap_or(i32::MAX));
                message.s32(names::HEIGHT, i32::try_from(height).unwrap_or(i32::MAX));
                written = message
                    .property_list(
                        names::EXTENSIONS,
                        Some(&plist::Value::Dictionary(extensions.clone())),
                    )
                    .map(|_| ());
            },
        );
        written.map_err(|error| error.status().code())?;
        let reply = reply.map_err(Status::code)?;
        let code = |error: WireError| error.status().code();
        let message = Message::parse(&reply.bytes).map_err(code)?;
        let status = message.s32(names::ERROR).map_err(code)?.unwrap_or(0);
        if status != NO_ERROR {
            return Err(status);
        }
        let image_description = message
            .image_description(names::CREATED_FORMAT_DESCRIPTION)
            .map_err(code)?
            .ok_or(Status::VideoEncoderMalfunction.code())?;
        let parsed = ImageDescription::parse(image_description).map_err(code)?;
        Ok(serialized_description(
            codec,
            width,
            height,
            configuration,
            image_description,
            &parsed,
            &extensions,
        ))
    }

    /// `Eemt`: the frame's status and info flags, the UUID the guest keeps
    /// it under, and its sample buffer, cut into messages of 0x4000 bytes:
    /// each but the last flagged 0x10 with no reply asked, the first with
    /// every parameter, the others with their piece alone; the last waits
    /// 10 s for the guest's `,err`, which is not read
    /// (measured on macOS 27 with a 640x480 frame).
    fn emit_encoded_frame(
        &self,
        frame_id: i64,
        status: OsStatus,
        info_flags: u32,
        uuid: Option<GuestUuid>,
        serialization: Option<&[u8]>,
    ) {
        let header = |flags| {
            MessageWriter::new(
                names::EMIT_ENCODED_FRAME,
                flags,
                Some(self.link.guest_uuid),
                0,
            )
        };
        let mut message = header(MessageFlags::HOST_EXPECTS_REPLY);
        message.s64(names::FRAME, frame_id);
        message.s32(names::ERROR, status);
        message.u32(names::ENCODE_INFO_FLAGS, info_flags);
        if let Some(uuid) = uuid {
            message.uuid(names::PIXEL_BUFFER_UUID, uuid);
        }
        if let Some(mut rest) = serialization {
            rest = message.sample_buffer_with_limit(names::SAMPLE_BUFFER, GUEST_BUFFER_SIZE, rest);
            while !rest.is_empty() {
                message.change_flags(
                    MessageFlags::MORE_FRAGMENTS,
                    MessageFlags(
                        MessageFlags::MORE_FRAGMENTS.0 | MessageFlags::HOST_EXPECTS_REPLY.0,
                    ),
                );
                if let Err(status) = self.link.send(message.finish()) {
                    log::debug!("emitting frame {frame_id} stopped: {status:?}");
                    return;
                }
                message = header(MessageFlags::HOST_EXPECTS_REPLY);
                rest =
                    message.sample_buffer_with_limit(names::SAMPLE_BUFFER, GUEST_BUFFER_SIZE, rest);
            }
        }
        if let Err(error) = self
            .link
            .send_and_wait(message, CODEC_OPERATION_REPLY_TIMEOUT)
        {
            log::warn!("emitting frame {frame_id} failed: {error:?}");
        }
    }

    /// `emif`: a multi-image frame, which this host's encoders cannot
    /// encode: -12782, as Apple's host answers for an encoder without the
    /// call.
    fn encode_multi_image_frame(&self, message: &Message<'_>) -> Handled {
        message.s64(names::FRAME)?;
        message.time(names::PRESENTATION_TIME)?;
        message.time(names::DURATION)?;
        message.dictionary(names::FRAME_PROPERTIES)?;
        message.uuid_array(names::PIXEL_BUFFER_UUIDS)?;
        let mut reply = reply(message);
        reply.s32(names::ERROR, Status::UnimplementedFunction.code());
        reply.u32(names::INFO_FLAGS, 0);
        self.send(reply.finish());
        Ok(())
    }

    /// `ests`: a tile session, which this host's encoders do not have:
    /// -12782 once the message reads, -19093 for a tile under 1 pixel.
    fn start_tile_session(&self, message: &Message<'_>) -> Handled {
        message.dictionary(names::ENCODER_SPECIFICATION)?;
        let (width, height) = message
            .i32_pair(names::TILE_DIMENSIONS, ValueType::Size)?
            .unwrap_or((0, 0));
        if width < 1 || height < 1 {
            return self.reply_status(message, Status::MalformedMessage.code());
        }
        self.reply_status(message, Status::UnimplementedFunction.code())
    }

    /// `etil`: a tile, which needs its one surface and every parameter, or
    /// there is no reply; then -12782 and the info flags, 0.
    fn encode_tile(&self, message: &Message<'_>, surfaces: &[Arc<SurfaceHold>]) -> Handled {
        message.s64(names::TILE)?;
        if surfaces.len() != 1 {
            return Err(WireError::Malformed("a tile without its one surface"));
        }
        message.i32_pair(names::TILE_OFFSET, ValueType::Point)?;
        message.i32_pair(names::TILE_APERTURE, ValueType::Size)?;
        message.dictionary(names::TILE_PROPERTIES)?;
        message.uuid(names::PIXEL_BUFFER_UUID)?;
        let mut reply = reply(message);
        reply.s32(names::ERROR, Status::UnimplementedFunction.code());
        reply.u32(names::ENCODE_INFO_FLAGS, 0);
        self.send(reply.finish());
        Ok(())
    }
}

/// The size a stream of `width` by `height` source frames is encoded at:
/// each side rounded down to even, the frames' last column or row left
/// out. Apple's AVE does so: on this Mac (M3 Max, macOS 27.0.1, 26A434,
/// 2026-10-24, at 641x361, 640x361 and 641x360, H.264 and HEVC), its pixel
/// buffer attributes kept the odd size, and the format description and the
/// stream were 640x360, matching the source's top left at 46.8 dB of luma,
/// as an even encode did. 4:2:0 H.264 and HEVC crop in pairs of luma
/// samples, so no stream has an odd size. A side of 1 rounds to 0, and
/// `esta` refuses it before the backend is asked.
fn encoded_size(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

/// Whether `planes` hold a `width` by `height` picture in `format`, as
/// [`SourceFrame::planes`] promises a backend: luma, then interleaved
/// chroma, each with the picture's rows and each row's bytes. The layout
/// is the guest's: the surface only has to fit its memory.
pub(crate) fn planes_hold(
    planes: &[PlaneSource<'_>],
    format: PixelFormat,
    width: u32,
    height: u32,
) -> bool {
    let [luma, chroma] = planes else {
        return false;
    };
    let fits = |plane: &PlaneSource<'_>, samples: u32, rows: u32| {
        usize::try_from(samples)
            .ok()
            .and_then(|samples| samples.checked_mul(format.bytes_per_sample()))
            .is_some_and(|row| plane.bytes_per_row >= row)
            && plane.rows >= rows
    };
    // Cb and Cr interleaved: a pair for every two pixels.
    let chroma_samples = width.div_ceil(2).checked_mul(2);
    fits(luma, width, height)
        && chroma_samples.is_some_and(|samples| fits(chroma, samples, height.div_ceil(2)))
}

/// The status the guest sees for a backend's failure, as a VideoToolbox
/// encoder reports one: a size or setting it cannot take, or data that
/// does not encode, is a parameter error; anything else a malfunction. A
/// [`BackendError::Native`] status goes as it is, as Apple's host sends
/// its encoder's.
fn encoder_status(error: &BackendError) -> OsStatus {
    match error {
        BackendError::Unsupported(_) | BackendError::BadData(_) => Status::Parameter.code(),
        // A decoder's alone: none reaches here.
        BackendError::Malfunction(_) | BackendError::SkippedLeadingPicture(_) => {
            Status::VideoEncoderMalfunction.code()
        }
        BackendError::Native { status, .. } => {
            BackendError::native_status(*status).unwrap_or(Status::VideoEncoderMalfunction.code())
        }
    }
}

/// The extension a codec's configuration record goes in.
fn configuration_atom(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "avcC",
        Codec::Hevc => "hvcC",
        Codec::Av1 => "av1C",
        Codec::Vp9 => "vpcC",
    }
}

/// The format description the samples carry, as Apple's host serialized
/// the one it made of the guest's ImageDescription (measured on macOS 27,
/// with Apple's software H.264 and HEVC encoders): the extensions sent in
/// `Efds`, the ImageDescription itself, what CoreMedia reads from its
/// header (`FormatName`, `Depth`, `Version`, `RevisionLevel`, the
/// qualities), a progressive frame of square pixels in video range, and
/// chroma sited left in both fields, which Apple's carried though neither
/// the encoder nor the guest had set it.
fn serialized_description(
    codec: Codec,
    width: u32,
    height: u32,
    configuration: &[u8],
    image_description: &[u8],
    parsed: &ImageDescription,
    sent: &plist::Dictionary,
) -> SerializedFormatDescription {
    let leaf = |value: plist::Value| SerializedValue::PropertyList(value);
    let field16 = |offset: usize| {
        (image_description.get(offset..))
            .and_then(<[u8]>::first_chunk)
            .map_or(0, |bytes| i64::from(u16::from_be_bytes(*bytes)))
    };
    let field32 = |offset: usize| {
        (image_description.get(offset..))
            .and_then(<[u8]>::first_chunk)
            .map_or(0, |bytes| i64::from(u32::from_be_bytes(*bytes)))
    };
    let format_name = String::from_utf8(parsed.compressor_name.clone())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| codec_name(codec).to_owned());
    let mut extensions = vec![
        (
            "FormatName".to_owned(),
            leaf(plist::Value::String(format_name)),
        ),
        ("SpatialQuality".to_owned(), leaf(integer(field32(28)))),
        ("Version".to_owned(), leaf(integer(field16(16)))),
        (
            "VerbatimSampleDescription".to_owned(),
            leaf(plist::Value::Data(image_description.to_vec())),
        ),
        (
            "CVPixelAspectRatio".to_owned(),
            SerializedValue::Dictionary(vec![
                ("HorizontalSpacing".to_owned(), leaf(integer(1))),
                ("VerticalSpacing".to_owned(), leaf(integer(1))),
            ]),
        ),
        ("RevisionLevel".to_owned(), leaf(integer(field16(18)))),
        ("TemporalQuality".to_owned(), leaf(integer(field32(24)))),
        (
            "SampleDescriptionExtensionAtoms".to_owned(),
            SerializedValue::Dictionary(vec![(
                configuration_atom(codec).to_owned(),
                leaf(plist::Value::Data(configuration.to_vec())),
            )]),
        ),
        (
            "FullRangeVideo".to_owned(),
            leaf(plist::Value::Boolean(false)),
        ),
        ("CVFieldCount".to_owned(), leaf(integer(1))),
        ("Depth".to_owned(), leaf(integer(i64::from(parsed.depth)))),
        (
            "CVImageBufferChromaLocationTopField".to_owned(),
            leaf(plist::Value::String("Left".to_owned())),
        ),
        (
            "CVImageBufferChromaLocationBottomField".to_owned(),
            leaf(plist::Value::String("Left".to_owned())),
        ),
    ];
    for (key, value) in sent {
        if key.starts_with("CVImageBuffer") {
            extensions.push((key.clone(), leaf(value.clone())));
        }
    }
    // The record the guest made it of, should it differ from what was
    // sent: its own extension atom wins.
    let record = match codec {
        Codec::H264 => AVC_CONFIGURATION,
        Codec::Hevc => HEVC_CONFIGURATION,
        Codec::Av1 => AV1_CONFIGURATION,
        Codec::Vp9 => VP9_CONFIGURATION,
    };
    if let Some(atom) = parsed.extension(record)
        && atom != configuration
    {
        log::warn!("the guest's format description has another configuration record");
    }
    SerializedFormatDescription {
        media_type: MEDIA_TYPE_VIDEO,
        codec: codec.fourcc(),
        width: i32::try_from(width).unwrap_or(i32::MAX),
        height: i32::try_from(height).unwrap_or(i32::MAX),
        extensions: Some(extensions),
    }
}

/// One encoded frame as a sample buffer, in the shape Apple's host
/// serialized its encoders' (measured on macOS 27): the output time stamp,
/// one timing entry (the decode time stamp the presentation one where the
/// backend gives none), the data and its size, the format description when
/// it is new, and the sample's attachments: `NotSync` true on all but sync
/// samples, `DependsOnOthers`, and `EarlierDisplayTimesAllowed` false.
fn sample_buffer(
    frame: &EncodedFrame,
    format_description: Option<SerializedFormatDescription>,
) -> SampleBuffer {
    let decode = if frame.decode_time.is_valid() {
        frame.decode_time
    } else {
        frame.presentation_time
    };
    let flag = |value| SerializedValue::PropertyList(plist::Value::Boolean(value));
    let mut attachments = Vec::with_capacity(3);
    if !frame.sync {
        attachments.push(("NotSync".to_owned(), flag(true)));
    }
    attachments.push(("DependsOnOthers".to_owned(), flag(frame.depends_on_others)));
    attachments.push(("EarlierDisplayTimesAllowed".to_owned(), flag(false)));
    SampleBuffer {
        sample_count: 1,
        timings: vec![SampleTiming {
            duration: frame.duration,
            presentation: frame.presentation_time,
            decode,
        }],
        sizes: vec![u32::try_from(frame.data.len()).unwrap_or(u32::MAX)],
        data: Some(frame.data.clone()),
        output_presentation_time: Some(frame.presentation_time),
        format_description,
        cryptor: None,
        propagated_attachments: None,
        other_attachments: None,
        sample_attachments: Some(vec![SerializedValue::Dictionary(attachments)]),
    }
}

#[cfg(test)]
mod tests;
