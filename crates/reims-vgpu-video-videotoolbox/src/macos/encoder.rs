//! The host session's encoders on VideoToolbox: core's [`EncoderBackend`]
//! over VTCompressionSession, H.264 and HEVC on the hardware encoders
//! (`RequireHardwareAcceleratedVideoEncoder`), as Apple's host runs AVE for
//! a guest.
//!
//! The guest's properties go to the session as the guest set them: core's
//! keys and values are VideoToolbox's own (`kVTCompressionPropertyKey_*`,
//! `kVTProfileLevel_*`), so they map one to one. What VideoToolbox does not
//! list for its encoder, or a value outside its list, is refused with
//! VideoToolbox's -12900 before the session sees it ([`properties`]).
//!
//! The encoders are listed whatever their state, and what each takes is
//! read once VideoToolbox answers ([`VideoToolboxEncoderBackend`]), so a
//! backend made while every hardware encoder session of the Mac is taken
//! still offers them.
//!
//! Each source frame is copied from the guest's surface into a buffer of
//! the session's pool and encoded. The session emits on a thread of its
//! own; what it has emitted is handed back from the next call, in decode
//! order. Where the guest turned frame reordering off, an encode waits a
//! little for its own frame, so that each frame is emitted with its `efrm`
//! as the hardware allows ([`OWN_FRAME_WAIT`]).

pub(crate) mod properties;

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use reims_vgpu_video_core::backend::{BackendError, Codec, PixelFormat};
use reims_vgpu_video_core::encoder_backend::{
    BackendEncoder, EncodedFrame, EncoderBackend, EncoderCapabilities, EncoderConfig,
    EncoderSettings, SourceFrame,
};
use reims_vgpu_video_core::plist;
use reims_vgpu_video_core::runtime::{Bell, Runtime};
use reims_vgpu_video_core::status::OsStatus;
use reims_vgpu_video_core::sync::{Mutex, MutexGuard};
use reims_vgpu_video_core::wire::message::MediaTime;

use super::cf::{self, Cf, Owned, constant};
use super::decoder::{cm_time, media_time};
use super::ffi::core_foundation::OSStatus;
use super::ffi::core_media::{
    CMBlockBufferCopyDataBytes, CMBlockBufferGetDataLength, CMFormatDescriptionGetExtension,
    CMSampleBufferGetDataBuffer, CMSampleBufferGetDecodeTimeStamp, CMSampleBufferGetDuration,
    CMSampleBufferGetFormatDescription, CMSampleBufferGetPresentationTimeStamp,
    CMSampleBufferGetSampleAttachmentsArray, CMSampleBufferRef,
};
use super::ffi::core_video::{
    CVPixelBufferPoolCreatePixelBuffer, CVPixelBufferRef, kCVReturnSuccess,
};
use super::ffi::video_toolbox::{
    VTCompressionSessionCompleteFrames, VTCompressionSessionCreate,
    VTCompressionSessionEncodeFrame, VTCompressionSessionGetPixelBufferPool,
    VTCompressionSessionInvalidate, VTCompressionSessionPrepareToEncodeFrames,
    VTCompressionSessionRef, VTEncodeInfoFlags, kVTEncodeInfo_FrameDropped,
};
use super::pixel_buffer::PixelBuffer;
use super::session::{Session, frame_id_of, frame_reference};
use super::status::native_error;
use super::stream_format::codec_type;
use properties::{PropertyCheck, apply_properties, specification};

/// Which of VideoToolbox's encoders a session uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderChoice {
    /// The hardware encoder alone, as the backend's sessions use.
    Hardware,
    /// The software encoder alone, for streams the tests decode.
    #[cfg(test)]
    Software,
}

/// How long an encode waits for its own frame where the guest turned frame
/// reordering off: the hardware encoders here emitted each such frame
/// within 20 ms of its encode (AVE's HEVC encoder at 1280x720, the M3 Max,
/// 2026-10-24). Ours: a session whose encoder keeps a frame past this
/// stops waiting for the rest of its life, so that an encoder with a
/// look-ahead costs one wait, not one a frame.
pub(crate) const OWN_FRAME_WAIT: Duration = Duration::from_millis(100);

/// The kVTVideoEncoderMalfunctionErr Apple's host answers `+enc` with for
/// AVE while every hardware encoder session of the Mac is taken (measured
/// on the M3 Max, macOS 27.0.1, 2026-10-24, for H.264 and HEVC alike).
const ENCODER_MALFUNCTION: OsStatus = -12912;

/// Where a listed encoder's capabilities come from: VideoToolbox, or a
/// stand-in in the tests.
pub(crate) trait EncoderReader: Send + Sync {
    /// The check of the properties `codec`'s encoder takes, or None while
    /// VideoToolbox lists none.
    fn property_check(&self, codec: Codec) -> Option<PropertyCheck>;

    /// What `codec`'s encoder encodes, from its properties; None where it
    /// encodes nothing core offers.
    fn capabilities(
        &self,
        codec: Codec,
        reorders: bool,
        check: &PropertyCheck,
    ) -> Option<EncoderCapabilities>;
}

/// VideoToolbox's own answers ([`properties`]).
struct VideoToolboxReader;

impl EncoderReader for VideoToolboxReader {
    fn property_check(&self, codec: Codec) -> Option<PropertyCheck> {
        properties::property_check(codec)
    }

    fn capabilities(
        &self,
        codec: Codec,
        reorders: bool,
        check: &PropertyCheck,
    ) -> Option<EncoderCapabilities> {
        properties::capabilities(codec, reorders, check)
    }
}

/// What is known of one listed encoder.
#[derive(Clone, Debug)]
enum Reading {
    /// Its properties have not been read yet: every hardware encoder session of
    /// the Mac was taken whenever they were asked for.
    Unread,
    /// What it encodes, and the check of the properties it takes.
    Read(EncoderCapabilities, PropertyCheck),
    /// It encodes no profile core offers, so it is not offered.
    NotOffered,
}

/// One hardware encoder VideoToolbox's list names.
struct Listed {
    codec: Codec,
    reorders: bool,
    reading: Mutex<Reading>,
}

/// Encoders on this host's VideoToolbox.
///
/// They are listed as VideoToolbox's encoder list names them, whatever
/// their state, as Apple's host lists AVE's: its `encs` reads the registry
/// alone, and it listed AVE's H.264 and HEVC encoders while every hardware
/// encoder session of the Mac was taken. What each encodes and the
/// properties it takes are read from VideoToolbox once they answer: at the
/// backend's making, or later where every session was taken then. Until they
/// read, an encoder is listed by its codec alone. Each `+enc` asks
/// VideoToolbox for the encoder's properties again, and answers -12912 where
/// it has none, as Apple's host does while every session is taken.
pub struct VideoToolboxEncoderBackend {
    listed: Vec<Listed>,
    reader: Box<dyn EncoderReader>,
    /// What its sessions' waits for their own frames are bounded on.
    runtime: Arc<dyn Runtime>,
}

impl VideoToolboxEncoderBackend {
    /// A backend on this host's hardware encoders, as the type says, whose
    /// sessions wait on `runtime`'s clock.
    pub fn new(runtime: Arc<dyn Runtime>) -> Self {
        Self::with_reader(
            properties::listed_encoders(),
            Box::new(VideoToolboxReader),
            runtime,
        )
    }

    /// A backend on the encoders `listed` names, each a codec and whether
    /// it reorders frames, read through `reader`.
    pub(crate) fn with_reader(
        listed: Vec<(Codec, bool)>,
        reader: Box<dyn EncoderReader>,
        runtime: Arc<dyn Runtime>,
    ) -> Self {
        let backend = Self {
            listed: listed
                .into_iter()
                .map(|(codec, reorders)| Listed {
                    codec,
                    reorders,
                    reading: Mutex::new(Reading::Unread),
                })
                .collect(),
            reader,
            runtime,
        };
        for listed in &backend.listed {
            backend.read(listed);
        }
        backend
    }

    /// `listed`'s reading, read now where it has not read yet.
    fn read(&self, listed: &Listed) -> Reading {
        let mut reading = listed.reading.lock();
        if matches!(*reading, Reading::Unread)
            && let Some(check) = self.reader.property_check(listed.codec)
        {
            *reading = self.reading_of(listed, check);
        }
        reading.clone()
    }

    /// What `check` says of `listed`, logged.
    fn reading_of(&self, listed: &Listed, check: PropertyCheck) -> Reading {
        match self
            .reader
            .capabilities(listed.codec, listed.reorders, &check)
        {
            Some(capabilities) => {
                log::info!(
                    "videotoolbox: encodes {:?} in hardware, profiles {:?}, up to {}x{}",
                    capabilities.codec,
                    capabilities.profiles,
                    capabilities.max_width,
                    capabilities.max_height
                );
                Reading::Read(capabilities, check)
            }
            None => Reading::NotOffered,
        }
    }

    /// The encoder of `codec` VideoToolbox lists.
    fn listed(&self, codec: Codec) -> Option<&Listed> {
        self.listed.iter().find(|listed| listed.codec == codec)
    }
}

impl EncoderBackend for VideoToolboxEncoderBackend {
    fn encoders(&self) -> Vec<EncoderCapabilities> {
        self.listed
            .iter()
            .filter_map(|listed| match self.read(listed) {
                Reading::Read(capabilities, _) => Some(capabilities),
                // Listed by its codec alone, as the guest's `encs` entry is.
                Reading::Unread => Some(EncoderCapabilities {
                    codec: listed.codec,
                    profiles: Vec::new(),
                    max_width: 0,
                    max_height: 0,
                    source_formats: Vec::new(),
                    frame_reordering: listed.reorders,
                }),
                Reading::NotOffered => None,
            })
            .collect()
    }

    fn session_capabilities(&self, codec: Codec) -> Result<Option<EncoderCapabilities>, OsStatus> {
        let Some(listed) = self.listed(codec) else {
            return Ok(None);
        };
        let Some(check) = self.reader.property_check(codec) else {
            log::warn!(
                "videotoolbox: the {codec:?} hardware encoder lists no properties now: \
                 every hardware encoder session may be taken"
            );
            return Err(ENCODER_MALFUNCTION);
        };
        let mut reading = listed.reading.lock();
        if matches!(*reading, Reading::Unread) {
            *reading = self.reading_of(listed, check);
        }
        Ok(match &*reading {
            Reading::Read(capabilities, _) => Some(capabilities.clone()),
            Reading::Unread | Reading::NotOffered => None,
        })
    }

    fn open(&self, config: &EncoderConfig) -> Result<Box<dyn BackendEncoder>, BackendError> {
        let read = self
            .listed(config.codec)
            .is_some_and(|listed| matches!(self.read(listed), Reading::Read(..)));
        if !read {
            return Err(BackendError::Unsupported(format!(
                "no hardware encoder of {:?} here",
                config.codec
            )));
        }
        Ok(Box::new(Encoder::open(
            config,
            EncoderChoice::Hardware,
            Arc::clone(&self.runtime),
        )?))
    }

    fn check_property(
        &self,
        codec: Codec,
        key: &str,
        value: &plist::Value,
    ) -> Result<(), OsStatus> {
        match self.listed(codec).map(|listed| self.read(listed)) {
            Some(Reading::Read(_, check)) => check.check(key, value),
            _ => Ok(()),
        }
    }
}

/// What the output callback has been handed, each frame with its own
/// result, and the stream's configuration record from the latest sample's
/// format description.
#[derive(Default)]
struct Emitted {
    frames: Vec<EncodedFrame>,
    configuration: Vec<u8>,
}

/// The output callback's state. The callback's reference value is
/// `Arc::as_ptr` of the encoder's, which the encoder holds until its
/// session is invalidated (`Encoder`'s Drop), so it outlives every
/// callback. It is Sync, as the session calls back on a thread of its own.
#[derive(Default)]
struct Outputs {
    emitted: Mutex<Emitted>,
    /// Rung by the callback after each frame it keeps.
    arrived: Bell,
}

impl Outputs {
    fn lock(&self) -> MutexGuard<'_, Emitted> {
        self.emitted.lock()
    }
}

/// The output callback: keeps each encoded frame, in the decode order
/// VideoToolbox calls it in, with its own result. A frame the encoder
/// failed is kept with its status and no sample, which core emits in its
/// place, as Apple's host forwards each emit with its own status, so that
/// one frame's error costs no other frame its sample.
///
/// Nothing here panics (a lock, a sample's reads into vectors, a push),
/// and an `extern "C"` function that did would end the process rather
/// than unwind into VideoToolbox.
unsafe extern "C" fn encoded(
    outputs: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    flags: VTEncodeInfoFlags,
    sample: CMSampleBufferRef,
) {
    // SAFETY: the reference value is the encoder's Outputs (`Outputs`),
    // live until after its session is invalidated, when no callback runs;
    // only shared references to it are ever made.
    let outputs = unsafe { &*outputs.cast::<Outputs>() };
    let frame_id = frame_id_of(frame);
    let mut emitted = outputs.lock();
    if status != 0 {
        emitted.frames.push(EncodedFrame::failed(
            frame_id,
            native_error("the encoder's output", status),
        ));
    } else if flags & kVTEncodeInfo_FrameDropped != 0 || sample.is_null() {
        // A frame the encoder dropped, as it drops every frame of a session
        // one pixel wide or tall: a frame with no data, which core emits to
        // the guest as dropped, as Apple's host forwards its encoder's. Left
        // out, it would never be emitted, and core would hold its pending
        // entry and surface until the guest completed its frames.
        emitted.frames.push(EncodedFrame::dropped(frame_id));
    } else {
        // SAFETY: a live sample buffer the callback borrows.
        let read = unsafe { read_sample(sample, frame_id) };
        match read {
            Ok((frame, configuration)) => {
                if let Some(configuration) = configuration {
                    emitted.configuration = configuration;
                }
                emitted.frames.push(frame);
            }
            Err(error) => emitted.frames.push(EncodedFrame::failed(frame_id, error)),
        }
    }
    drop(emitted);
    outputs.arrived.ring();
}

/// An encoded sample as core takes it, and the configuration record its
/// format description carries.
///
/// # Safety
///
/// `sample` is a live CMSampleBuffer.
unsafe fn read_sample(
    sample: CMSampleBufferRef,
    frame_id: i64,
) -> Result<(EncodedFrame, Option<Vec<u8>>), BackendError> {
    // SAFETY: a live sample buffer, as the caller says; its data buffer is
    // the sample's (the Get rule), live while the sample is.
    let block = unsafe { CMSampleBufferGetDataBuffer(sample) };
    if block.is_null() {
        return Err(BackendError::Malfunction(
            "an encoded sample with no data".into(),
        ));
    }
    // SAFETY: a live block buffer.
    let length = unsafe { CMBlockBufferGetDataLength(block) };
    let mut data = vec![0u8; length];
    // SAFETY: a live block buffer, its `length` bytes copied into as many
    // of `data`'s own.
    let status = unsafe { CMBlockBufferCopyDataBytes(block, 0, length, data.as_mut_ptr().cast()) };
    if status != 0 {
        return Err(native_error("CMBlockBufferCopyDataBytes", status));
    }
    let atoms_key =
        constant!(core_media::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms);
    // SAFETY: a live sample buffer; its format description is the sample's
    // and the extension the description's (the Get rule), each live while
    // the sample is, which outlasts this call.
    let atoms = unsafe {
        let description = CMSampleBufferGetFormatDescription(sample);
        if description.is_null() {
            None
        } else {
            Cf::borrow(CMFormatDescriptionGetExtension(
                description,
                atoms_key.as_ptr(),
            ))
        }
    };
    let configuration = atoms.and_then(|atoms| {
        ["avcC", "hvcC"]
            .into_iter()
            .find_map(|atom| atoms.get_str(atom)?.data())
    });
    // The first sample's attachments: absent keys mean a sync sample
    // that others depend on (CMSampleBuffer.h's defaults).
    // SAFETY: a live sample buffer, whose attachments array is its own
    // (the Get rule), live while it is.
    let attachments = unsafe { Cf::borrow(CMSampleBufferGetSampleAttachmentsArray(sample, 0)) };
    let first = attachments.and_then(|attachments| attachments.values().first().copied());
    let flag = |key: Cf<'_>| first.and_then(|first| first.get(key)).and_then(Cf::bool);
    let not_sync = flag(constant!(core_media::kCMSampleAttachmentKey_NotSync)).unwrap_or(false);
    let depends_on_others = flag(constant!(
        core_media::kCMSampleAttachmentKey_DependsOnOthers
    ))
    .unwrap_or(not_sync);
    let depended_on_by_others = flag(constant!(
        core_media::kCMSampleAttachmentKey_IsDependedOnByOthers
    ))
    .unwrap_or(true);
    // SAFETY: a live sample buffer.
    let (presentation_time, decode_time, duration) = unsafe {
        (
            CMSampleBufferGetPresentationTimeStamp(sample),
            CMSampleBufferGetDecodeTimeStamp(sample),
            CMSampleBufferGetDuration(sample),
        )
    };
    let (presentation_time, decode_time) = (media_time(presentation_time), media_time(decode_time));
    let decode_time = if decode_time == presentation_time {
        MediaTime::INVALID
    } else {
        decode_time
    };
    Ok((
        EncodedFrame {
            frame_id,
            data,
            presentation_time,
            decode_time,
            duration: media_time(duration),
            sync: !not_sync,
            depends_on_others,
            depended_on_by_others,
            error: None,
        },
        configuration,
    ))
}

/// One VTCompressionSession and what it has emitted.
pub struct Encoder {
    /// Invalidated first on drop (the Drop impl), after which it calls back
    /// no more, so its outputs may go after it.
    session: Session,
    outputs: Arc<Outputs>,
    codec: Codec,
    width: u32,
    height: u32,
    /// The guest's properties as they were last set on the session.
    applied: Vec<(String, plist::Value)>,
    /// Whether an encode waits for its own frame (OWN_FRAME_WAIT).
    waits: bool,
    /// Whether the session has been prepared to encode frames, which is
    /// done at the guest's `epfr`, or else at the first frame
    /// (`BackendEncoder::prepare`).
    prepared: bool,
    /// The clock an encode's wait for its own frame is bounded on.
    runtime: Arc<dyn Runtime>,
}

impl Encoder {
    /// A session for `config` on the encoder `choice` names, with the
    /// guest's properties set, whose waits for its own frames are bounded on
    /// `runtime`'s clock.
    pub fn open(
        config: &EncoderConfig,
        choice: EncoderChoice,
        runtime: Arc<dyn Runtime>,
    ) -> Result<Self, BackendError> {
        let width = i32::try_from(config.width)
            .map_err(|_| BackendError::Unsupported(format!("a width of {}", config.width)))?;
        let height = i32::try_from(config.height)
            .map_err(|_| BackendError::Unsupported(format!("a height of {}", config.height)))?;
        let specification = specification(choice);
        let outputs = Arc::<Outputs>::default();
        let mut session: VTCompressionSessionRef = std::ptr::null_mut();
        // SAFETY: the specification is live; the callback's reference value
        // is the encoder's Outputs, which it keeps until the session is
        // invalidated (`Outputs`); the out pointer is a live local. The
        // session's source buffers come from its own pool, so no attributes
        // are given.
        let status = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),
                width,
                height,
                codec_type(config.codec),
                specification.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                Some(encoded),
                Arc::as_ptr(&outputs).cast_mut().cast(),
                &raw mut session,
            )
        };
        // SAFETY: a Create call's result, a compression session.
        let session = unsafe { Session::take(session, VTCompressionSessionInvalidate) };
        if status != 0 {
            return Err(native_error("VTCompressionSessionCreate", status));
        }
        let session = session.ok_or_else(|| {
            BackendError::Malfunction("VTCompressionSessionCreate gave none".into())
        })?;
        let mut encoder = Self {
            session,
            outputs,
            codec: config.codec,
            width: config.width,
            height: config.height,
            applied: Vec::new(),
            waits: false,
            prepared: false,
            runtime,
        };
        encoder.apply(&config.settings)?;
        Ok(encoder)
    }

    fn session(&self) -> VTCompressionSessionRef {
        self.session.as_ptr()
    }

    /// Sets each property the guest changed since the last time, and
    /// clears each it took away.
    fn apply(&mut self, settings: &EncoderSettings) -> Result<(), BackendError> {
        apply_properties(&self.applied, &settings.properties, |key, value| {
            let key = cf::string(key);
            let value =
                match value {
                    Some(value) => Some(cf::from_plist(value).ok_or_else(|| {
                        BackendError::Unsupported(format!("a value of {value:?}"))
                    })?),
                    None => None,
                };
            (self.session)
                .set_property(key.cf(), value.as_ref().map(Owned::cf))
                .map_err(|status| native_error("VTSessionSetProperty", status))
        })?;
        self.applied = settings.properties.clone();
        // An encoder without reordering emits each frame on its own; with
        // it, frames come out in groups.
        self.waits = settings
            .properties
            .iter()
            .any(|(key, value)| key == "AllowFrameReordering" && value.as_boolean() == Some(false));
        Ok(())
    }

    /// A buffer of the session's pool, or one of the frame's format where
    /// the session has no pool. The pool's may be of another format.
    fn source_buffer(&self, format: PixelFormat) -> Result<PixelBuffer, BackendError> {
        // SAFETY: a live session; the pool is the session's, borrowed.
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session()) };
        if pool.is_null() {
            return PixelBuffer::new(
                format.fourcc().0,
                usize::try_from(self.width).unwrap_or(0),
                usize::try_from(self.height).unwrap_or(0),
            );
        }
        let mut buffer: CVPixelBufferRef = std::ptr::null_mut();
        // SAFETY: a live pool and a live out pointer.
        let status =
            unsafe { CVPixelBufferPoolCreatePixelBuffer(std::ptr::null(), pool, &raw mut buffer) };
        // SAFETY: a Create call's result, or NULL.
        let buffer = unsafe { PixelBuffer::take(buffer) };
        if status != kCVReturnSuccess {
            return Err(native_error("CVPixelBufferPoolCreatePixelBuffer", status));
        }
        buffer.ok_or_else(|| BackendError::Malfunction("the pool gave no buffer".into()))
    }

    /// What the session has emitted, in decode order, each frame with its
    /// own result.
    fn take(&self) -> Vec<EncodedFrame> {
        std::mem::take(&mut self.outputs.lock().frames)
    }

    /// Waits for `frame_id` to be emitted, for up to OWN_FRAME_WAIT on the
    /// runtime's clock; false when it was not.
    fn wait_for(&self, frame_id: i64) -> bool {
        let deadline = self.runtime.deadline_in(OWN_FRAME_WAIT);
        let here = || {
            (self.outputs.lock().frames.iter())
                .any(|frame| frame.frame_id == frame_id)
                .then_some(())
        };
        (self.outputs.arrived)
            .wait_until(&*self.runtime, deadline, here)
            .is_some()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // The session calls back no more once invalidated, so the outputs
        // its callback's reference value points at may go after it.
        self.session.invalidate();
    }
}

impl BackendEncoder for Encoder {
    fn configuration(&self) -> Vec<u8> {
        self.outputs.lock().configuration.clone()
    }

    /// Prepares the session to encode, once: at the guest's `epfr`, where
    /// Apple's host prepares AVE, or else before the first frame. The guest
    /// sends `epfr` before its first frame whether or not its client
    /// prepares, so a guest's session is prepared where Apple's host
    /// prepares AVE. AVE takes `DataRateLimits` only before it is prepared:
    /// set after `VTCompressionSessionPrepareToEncodeFrames`, it was
    /// answered 0 and left the stream at the encoder's own rate (the M3
    /// Max, macOS 27.0.1, 2026-10-24).
    /// The guest sets its client's properties after `esta`, so a session
    /// prepared at its open would lose the guest's limit, which Apple's host
    /// keeps, and one prepared only at its first frame would keep a limit
    /// the client set after its own prepare, which Apple's host loses.
    fn prepare(&mut self) -> Result<(), BackendError> {
        if self.prepared {
            return Ok(());
        }
        // SAFETY: a live session.
        let status = unsafe { VTCompressionSessionPrepareToEncodeFrames(self.session()) };
        if status != 0 {
            return Err(native_error(
                "VTCompressionSessionPrepareToEncodeFrames",
                status,
            ));
        }
        self.prepared = true;
        Ok(())
    }

    fn encode(&mut self, frame: &SourceFrame<'_>) -> Result<Vec<EncodedFrame>, BackendError> {
        if (frame.width, frame.height) != (self.width, self.height) {
            return Err(BackendError::BadData(format!(
                "a {}x{} frame for a {}x{} session",
                frame.width, frame.height, self.width, self.height
            )));
        }
        self.prepare()?;
        let mut buffer = self.source_buffer(frame.pixel_format)?;
        if buffer.format() != frame.pixel_format.fourcc().0 {
            // The pool's buffers are in the format the session's profile
            // encodes, 420v for a Main profile; VideoToolbox converts any
            // other buffer it is handed, as it would for Apple's host.
            buffer = PixelBuffer::new(
                frame.pixel_format.fourcc().0,
                usize::try_from(self.width).unwrap_or(0),
                usize::try_from(self.height).unwrap_or(0),
            )?;
        }
        buffer.fill_420(frame.planes, frame.pixel_format, frame.width)?;
        let options = frame.force_key_frame.then(|| {
            cf::dictionary(&[(
                constant!(video_toolbox::kVTEncodeFrameOptionKey_ForceKeyFrame),
                cf::boolean(true),
            )])
        });
        let mut info: VTEncodeInfoFlags = 0;
        // SAFETY: a live session and buffer; the options are live or NULL;
        // the frame's reference value is its ID, which the callback only
        // reads back.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session(),
                buffer.as_ptr(),
                cm_time(frame.presentation_time),
                cm_time(frame.duration),
                options.as_ref().map_or(std::ptr::null(), Owned::as_ptr),
                frame_reference(frame.frame_id),
                &raw mut info,
            )
        };
        if status != 0 {
            return Err(native_error("VTCompressionSessionEncodeFrame", status));
        }
        if self.waits && !self.wait_for(frame.frame_id) {
            log::info!(
                "videotoolbox: the {:?} encoder kept frame {} past {:?}; not waiting again",
                self.codec,
                frame.frame_id,
                OWN_FRAME_WAIT
            );
            self.waits = false;
        }
        Ok(self.take())
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>, BackendError> {
        // SAFETY: a live session; an invalid time completes every frame,
        // and the call returns once each has been emitted.
        let status = unsafe {
            VTCompressionSessionCompleteFrames(self.session(), cm_time(MediaTime::INVALID))
        };
        if status != 0 {
            return Err(native_error("VTCompressionSessionCompleteFrames", status));
        }
        Ok(self.take())
    }

    fn reconfigure(&mut self, settings: &EncoderSettings) -> Result<(), BackendError> {
        self.apply(settings)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use reims_vgpu_video_core::runtime::StdRuntime;
    use reims_vgpu_video_core::wire::property_list::dictionary;

    use super::super::ffi::video_toolbox::kVTPropertyNotSupportedErr;
    use super::*;

    /// Encoders that answer only while `free`, as the Mac's hardware
    /// encoders answer only while one of their sessions is free; HEVC's
    /// encodes no profile core offers.
    struct StandIn {
        free: Arc<AtomicBool>,
        read: Arc<AtomicUsize>,
    }

    impl EncoderReader for StandIn {
        fn property_check(&self, _codec: Codec) -> Option<PropertyCheck> {
            let levels = ["H264_High_AutoLevel", "HEVC_Main42210_AutoLevel"]
                .map(|level| plist::Value::String(level.into()));
            let supported = dictionary([(
                "ProfileLevel",
                plist::Value::Dictionary(dictionary([
                    ("PropertyType", plist::Value::String("Enumeration".into())),
                    ("SupportedValueList", plist::Value::Array(levels.to_vec())),
                ])),
            )]);
            self.free
                .load(Ordering::SeqCst)
                .then(|| PropertyCheck::new(&supported))
        }

        fn capabilities(
            &self,
            codec: Codec,
            reorders: bool,
            check: &PropertyCheck,
        ) -> Option<EncoderCapabilities> {
            self.read.fetch_add(1, Ordering::SeqCst);
            let profiles = check.profiles(codec);
            (!profiles.is_empty()).then(|| EncoderCapabilities {
                codec,
                profiles,
                max_width: 4096,
                max_height: 4096,
                source_formats: vec![PixelFormat::Nv12VideoRange],
                frame_reordering: reorders,
            })
        }
    }

    /// The output callback keeps each frame with its own result: an error
    /// among the frames of one take is its own frame's, and the samples
    /// before and after it keep their bytes. Driven with sample buffers
    /// made here, so no encoder runs.
    #[test]
    fn an_output_error_stands_for_its_own_frame_alone() -> Result<(), BackendError> {
        use reims_vgpu_video_core::backend::{CompressedFrame, DecoderConfig};
        use reims_vgpu_video_core::wire::message::MediaTime;

        let record = vec![
            1, 77, 0, 30, 0xff, 0xe1, 0, 4, 0x67, 77, 0, 30, 1, 0, 2, 0x68, 0xce,
        ];
        let description = super::super::stream_format::format_description(&DecoderConfig {
            codec: Codec::H264,
            width: 64,
            height: 48,
            configuration: record.clone(),
        })?;
        let time = |value| MediaTime {
            value,
            timescale: 30,
            flags: MediaTime::FLAG_VALID,
            epoch: 0,
        };
        let sample = |frame_id: i64, data: &[u8]| {
            super::super::decoder::sample_buffer(
                &CompressedFrame {
                    frame_id,
                    data,
                    sample_sizes: &[],
                    presentation_time: time(frame_id),
                    decode_time: MediaTime::INVALID,
                    duration: time(1),
                    new_config: None,
                },
                &description,
            )
        };
        let first = sample(1, &[0, 0, 0, 2, 0x65, 1])?;
        let third = sample(3, &[0, 0, 0, 2, 0x41, 3])?;
        let outputs = Arc::<Outputs>::default();
        let reference: *mut c_void = Arc::as_ptr(&outputs).cast_mut().cast();
        let frame = frame_reference;
        // SAFETY: the reference value is a live Outputs, as a session's is;
        // each sample buffer is live for its call, and a null one goes with
        // the error and the drop, as VideoToolbox passes them.
        unsafe {
            encoded(reference, frame(1), 0, 0, first.as_mut_ptr());
            encoded(reference, frame(2), -12904, 0, std::ptr::null_mut());
            encoded(reference, frame(3), 0, 0, third.as_mut_ptr());
            encoded(
                reference,
                frame(4),
                0,
                kVTEncodeInfo_FrameDropped,
                std::ptr::null_mut(),
            );
        }
        let frames = std::mem::take(&mut outputs.lock().frames);
        let ids: Vec<i64> = frames.iter().map(|frame| frame.frame_id).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(frames[0].data, [0, 0, 0, 2, 0x65, 1]);
        assert_eq!(frames[2].data, [0, 0, 0, 2, 0x41, 3]);
        assert_eq!(frames[0].presentation_time, time(1));
        assert!(frames[0].error.is_none() && frames[2].error.is_none());
        assert!(frames[1].data.is_empty());
        assert!(matches!(
            frames[1].error,
            Some(BackendError::Native { status: -12904, .. })
        ));
        assert_eq!(frames[3], EncodedFrame::dropped(4));
        assert_eq!(outputs.lock().configuration, record);
        Ok(())
    }

    /// A backend made while every hardware encoder session is taken
    /// lists its encoders by their codecs, as Apple's host lists AVE's then,
    /// and answers `+enc` with -12912, as Apple's does. Once a session is
    /// free, `+enc` reads the encoder and the listing has what it reads;
    /// an encoder with no profile core offers goes from the listing.
    #[test]
    fn encoders_listed_while_all_are_taken_are_read_once_one_is_free() {
        let free = Arc::new(AtomicBool::new(false));
        let read = Arc::new(AtomicUsize::new(0));
        let backend = VideoToolboxEncoderBackend::with_reader(
            vec![(Codec::H264, true), (Codec::Hevc, false)],
            Box::new(StandIn {
                free: free.clone(),
                read: read.clone(),
            }),
            StdRuntime::shared(),
        );
        let codecs = |backend: &VideoToolboxEncoderBackend| -> Vec<(Codec, Vec<u32>)> {
            (backend.encoders().into_iter())
                .map(|listed| (listed.codec, listed.profiles))
                .collect()
        };
        assert_eq!(
            codecs(&backend),
            [(Codec::H264, vec![]), (Codec::Hevc, vec![])]
        );
        assert_eq!(
            backend.session_capabilities(Codec::H264),
            Err(ENCODER_MALFUNCTION)
        );
        assert_eq!(backend.session_capabilities(Codec::Vp9), Ok(None));
        assert_eq!(read.load(Ordering::SeqCst), 0);

        free.store(true, Ordering::SeqCst);
        let session = backend.session_capabilities(Codec::H264);
        assert_eq!(
            session.map(|c| c.map(|c| (c.profiles, c.frame_reordering))),
            Ok(Some((vec![100], true)))
        );
        assert_eq!(codecs(&backend), [(Codec::H264, vec![100])]);
        assert_eq!(backend.session_capabilities(Codec::Hevc), Ok(None));
        assert_eq!(read.load(Ordering::SeqCst), 2);
        assert_eq!(
            backend.check_property(Codec::H264, "NoSuchProperty", &plist::Value::Boolean(true)),
            Err(kVTPropertyNotSupportedErr)
        );

        // Taken again: what was read stays listed, and `+enc` is refused
        // again, as Apple's is whenever every session is taken.
        free.store(false, Ordering::SeqCst);
        assert_eq!(codecs(&backend), [(Codec::H264, vec![100])]);
        assert_eq!(
            backend.session_capabilities(Codec::H264),
            Err(ENCODER_MALFUNCTION)
        );
        assert_eq!(read.load(Ordering::SeqCst), 2);
    }
}
