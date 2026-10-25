//! The host session's decoders on VideoToolbox: core's [`DecoderBackend`]
//! over VTDecompressionSession, so that the guest's VideoToolbox decodes
//! on the host's own.
//!
//! Each decoder session the guest starts is one VTDecompressionSession on
//! the hardware decoder (`RequireHardwareAcceleratedVideoDecoder`), made
//! from the guest's configuration record, writing IOSurface-backed
//! CVPixelBuffers in the format core asks the guest for. Frames are decoded
//! one at a time, synchronously and without temporal processing, so that
//! each comes out before its decode call returns, in decode order, as
//! VideoToolbox's decoders emit them to a guest of Apple's host. A pixel
//! transfer writes each into the guest's buffer where the host lends its
//! IOSurface (`zero_copy.rs`), and core copies it there through
//! [`DecodedFrame::read_back`] where not.

use std::ffi::c_void;
use std::sync::Arc;

use reims_vgpu_video_codec::{av1, vp9};
use reims_vgpu_video_core::backend::{
    BackendDecoder, BackendError, Codec, CodecCapabilities, CompressedFrame, DecodedFrame,
    DecoderBackend, DecoderConfig, DeviceWrite, FrameDecoded, FrameReadBack, PixelFormat,
    copy_biplanar,
};
use reims_vgpu_video_core::device_memory::GuestTarget;
use reims_vgpu_video_core::surfaces::PlaneDestination;
use reims_vgpu_video_core::sync::Mutex;
use reims_vgpu_video_core::wire::message::MediaTime;

use super::capabilities;
use super::cf::{self, Owned, constant};
use super::ffi::core_foundation::OSStatus;
use super::ffi::core_media::{
    CMBlockBufferCreateWithMemoryBlock, CMBlockBufferRef, CMBlockBufferReplaceDataBytes,
    CMSampleBufferCreateReady, CMSampleBufferRef, CMSampleTimingInfo, CMTime,
    kCMBlockBufferAssureMemoryNowFlag,
};
use super::ffi::core_video::CVImageBufferRef;
use super::ffi::video_toolbox::{
    VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSessionCanAcceptFormatDescription, VTDecompressionSessionCreate,
    VTDecompressionSessionDecodeFrame, VTDecompressionSessionFinishDelayedFrames,
    VTDecompressionSessionInvalidate, VTDecompressionSessionRef,
    VTDecompressionSessionWaitForAsynchronousFrames, kVTDecodeInfo_FrameDropped,
};
use super::pixel_buffer::{LockedForReading, PixelBuffer, fourcc_text, io_surface_attributes};
use super::session::{Session, frame_id_of, frame_reference};
use super::status::{native_error, output_error};
use super::stream_format::{format_description, output_format, stream_size};
use super::zero_copy::{self, PixelTransfer};

/// Which of VideoToolbox's decoders a session may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderChoice {
    /// The hardware decoder alone, as the backend's sessions use: a host
    /// without one for the stream refuses it.
    Hardware,
    /// The software decoder alone, which the tests compare the hardware's
    /// frames with.
    Software,
}

/// Decoders on this host's VideoToolbox.
pub struct VideoToolboxBackend {
    capabilities: Vec<CodecCapabilities>,
}

impl VideoToolboxBackend {
    /// A backend on this host's VideoToolbox. What it decodes is read from
    /// VideoToolbox now, once.
    pub fn new() -> Self {
        let capabilities = capabilities::decoders();
        for codec in &capabilities {
            log::info!(
                "videotoolbox: decodes {:?} in hardware, profiles {:?}",
                codec.codec,
                codec.profiles
            );
        }
        Self { capabilities }
    }
}

impl Default for VideoToolboxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl DecoderBackend for VideoToolboxBackend {
    fn capabilities(&self) -> Vec<CodecCapabilities> {
        self.capabilities.clone()
    }

    fn open(&self, config: &DecoderConfig) -> Result<Box<dyn BackendDecoder>, BackendError> {
        if !self
            .capabilities
            .iter()
            .any(|listed| listed.codec == config.codec)
        {
            return Err(BackendError::Unsupported(format!(
                "no hardware decoder of {:?} here",
                config.codec
            )));
        }
        Ok(Box::new(Decoder::open(config, DecoderChoice::Hardware)?))
    }
}

/// What the output callback was handed for one frame.
struct Output {
    frame_id: i64,
    status: OSStatus,
    flags: VTDecodeInfoFlags,
    image: Option<PixelBuffer>,
}

/// The output callback's state: what it was handed since the session last
/// took it. The callback's reference value is `Arc::as_ptr` of the
/// decoder's, which the decoder holds until its session is invalidated
/// (`Decoder`'s Drop), so it outlives every callback. It is Sync, as
/// VideoToolbox may call back on a thread of its own.
#[derive(Default)]
struct Outputs(Mutex<Vec<Output>>);

impl Outputs {
    fn take(&self) -> Vec<Output> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// The output callback: keeps what VideoToolbox hands it. Nothing here
/// panics (a retain, a lock, a push), and an `extern "C"` function that did
/// would end the process rather than unwind into VideoToolbox.
unsafe extern "C" fn decoded(
    outputs: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    flags: VTDecodeInfoFlags,
    image: CVImageBufferRef,
    _presentation_time: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the reference value is the decoder's Outputs (`Outputs`),
    // live until after its session is invalidated, when no callback runs;
    // only shared references to it are ever made.
    let outputs = unsafe { &*outputs.cast::<Outputs>() };
    // SAFETY: the image, when there is one, is a live CVPixelBuffer the
    // callback borrows; a reference of our own is taken.
    let image = unsafe { PixelBuffer::retain(image) };
    outputs.0.lock().push(Output {
        frame_id: frame_id_of(frame),
        status,
        flags,
        image,
    });
}

/// One VTDecompressionSession and what it decodes.
pub struct Decoder {
    /// Invalidated first on drop (the Drop impl), after which it calls back
    /// no more, so its outputs may go after it.
    session: Session,
    outputs: Arc<Outputs>,
    description: Owned,
    config: DecoderConfig,
    format: PixelFormat,
    choice: DecoderChoice,
    /// The stream's own size, where its record gives it: the most of each
    /// frame that is the picture (`stream_size`).
    picture: Option<(u32, u32)>,
    /// For AV1, the stream's frames as its headers code them, which
    /// VideoToolbox does not say, for core to lend the guest's buffers by
    /// ([`BackendDecoder::frames_decoded`]).
    av1_frames: Option<av1::Decoder<av1::FramesOnly>>,
    /// For VP9, the same.
    vp9_frames: Option<vp9::Decoder<vp9::FramesOnly>>,
    /// The last decode's frames, from `av1_frames` or `vp9_frames`.
    frames_decoded: Vec<FrameDecoded>,
    /// The pixel transfer its frames write the guest's buffers through
    /// (`zero_copy.rs`), shared with each frame.
    transfer: Arc<PixelTransfer>,
}

/// A reader of the AV1 stream `config` describes, where it is one whose
/// `av1C` reads.
fn av1_frames(config: &DecoderConfig) -> Option<av1::Decoder<av1::FramesOnly>> {
    if config.codec != Codec::Av1 {
        return None;
    }
    av1::Decoder::from_av1c(av1::FramesOnly, &config.configuration)
        .inspect_err(|error| log::debug!("videotoolbox: AV1's frames not followed: {error}"))
        .ok()
}

/// A reader of the VP9 stream `config` describes.
fn vp9_frames(config: &DecoderConfig) -> Option<vp9::Decoder<vp9::FramesOnly>> {
    (config.codec == Codec::Vp9).then(|| vp9::Decoder::new(vp9::FramesOnly))
}

impl Decoder {
    /// A session for `config` on the decoder `choice` names, writing frames
    /// in the format the record's stream decodes into.
    pub fn open(config: &DecoderConfig, choice: DecoderChoice) -> Result<Self, BackendError> {
        let format = output_format(config)?;
        let description = format_description(config)?;
        let outputs = Arc::<Outputs>::default();
        let session = create_session(&description, format, choice, &outputs)?;
        Ok(Self {
            session,
            outputs,
            description,
            config: config.clone(),
            format,
            choice,
            picture: stream_size(config),
            av1_frames: av1_frames(config),
            vp9_frames: vp9_frames(config),
            frames_decoded: Vec::new(),
            transfer: Arc::default(),
        })
    }

    fn session(&self) -> VTDecompressionSessionRef {
        self.session.as_ptr()
    }

    /// Takes `config`, which a sample's format description brought: the
    /// same session where it can go on, a new one where it cannot.
    fn reconfigure(&mut self, config: &DecoderConfig) -> Result<(), BackendError> {
        if *config == self.config {
            return Ok(());
        }
        let format = output_format(config)?;
        let description = format_description(config)?;
        // SAFETY: a live session and format description.
        let accepts = unsafe {
            VTDecompressionSessionCanAcceptFormatDescription(self.session(), description.as_ptr())
        } != 0;
        if accepts && format == self.format {
            if config.configuration != self.config.configuration {
                self.av1_frames = av1_frames(config);
                self.vp9_frames = vp9_frames(config);
            }
            self.description = description;
            self.config = config.clone();
            self.picture = stream_size(config);
            return Ok(());
        }
        *self = Self::open(config, self.choice)?;
        Ok(())
    }

    /// The frames `frame`'s sample carried, read from its headers beside
    /// the decode, which VideoToolbox `decoded` or refused: none for a
    /// codec of one frame a sample, or where the reader refused an AV1
    /// sample.
    ///
    /// A VP9 sample AVD refused that the reader took is refused from its
    /// first frame of another size than the description's, or from its
    /// first frame, and the reader refuses each frame after it until a key
    /// frame: AVD refuses a frame of another size, and each after it until
    /// a key frame of the description's size, and lends a buffer for each
    /// (measured on Apple's host session on the M3 Max, macOS 27.0.1,
    /// 2026-10-25, with libvpx's dynamic resize and its spatial layers).
    fn frames_read(&mut self, frame: &CompressedFrame<'_>, decoded: bool) -> Vec<FrameDecoded> {
        let id = frame.frame_id.cast_unsigned();
        if let Some(reader) = self.av1_frames.as_mut() {
            return match reader.decode(frame.data, id) {
                Ok(_) => reader.steps().iter().map(FrameDecoded::from).collect(),
                Err(error) => {
                    log::debug!("videotoolbox: AV1's frames not read: {error}");
                    Vec::new()
                }
            };
        }
        let Some(reader) = self.vp9_frames.as_mut() else {
            return Vec::new();
        };
        let read = reader.decode(frame.data, id);
        let mut steps: Vec<FrameDecoded> = (reader.take_steps().iter())
            .map(FrameDecoded::from)
            .collect();
        if let Err(error) = &read {
            log::debug!("videotoolbox: a VP9 sample's frames refused by its reader: {error}");
        }
        if decoded || read.is_err() {
            return steps;
        }
        reader.mark_failed();
        let size = (self.config.width, self.config.height);
        let first = (steps.iter())
            .position(|step| !step.shown_again && (step.width, step.height) != size)
            .unwrap_or(0);
        for (index, step) in steps.iter_mut().enumerate() {
            step.output = false;
            if index >= first {
                step.refused = true;
                step.shown_again = false;
                step.references = [None; 8];
            }
        }
        steps
    }

    /// What the callback has been handed since the last call, as frames
    /// ready for output. A frame VideoToolbox could not decode is an error.
    fn ready(&self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        let mut frames: Vec<Box<dyn DecodedFrame>> = Vec::new();
        for output in self.outputs.take() {
            if output.status != 0 {
                return Err(output_error(output.status, output.flags));
            }
            let Some(image) = output.image else {
                continue;
            };
            if output.flags & kVTDecodeInfo_FrameDropped != 0 {
                continue;
            }
            frames.push(Box::new(Frame::new(
                output.frame_id,
                image,
                self.format,
                self.picture,
                &self.transfer,
            )?));
        }
        Ok(frames)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // The session calls back no more once invalidated, so the outputs
        // its callback's reference value points at may go after it.
        self.session.invalidate();
    }
}

/// `{RequireHardwareAcceleratedVideoDecoder: true}`, or for the software
/// decoder `{EnableHardwareAcceleratedVideoDecoder: false}`.
fn specification(choice: DecoderChoice) -> Owned {
    match choice {
        DecoderChoice::Hardware => cf::dictionary(&[(
            constant!(
                video_toolbox::kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder
            ),
            cf::boolean(true),
        )]),
        DecoderChoice::Software => cf::dictionary(&[(
            constant!(
                video_toolbox::kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder
            ),
            cf::boolean(false),
        )]),
    }
}

fn create_session(
    description: &Owned,
    format: PixelFormat,
    choice: DecoderChoice,
    outputs: &Arc<Outputs>,
) -> Result<Session, BackendError> {
    let specification = specification(choice);
    let attributes = io_surface_attributes(format.fourcc().0);
    let callback = VTDecompressionOutputCallbackRecord {
        callback: Some(decoded),
        ref_con: Arc::as_ptr(outputs).cast_mut().cast(),
    };
    let mut session: VTDecompressionSessionRef = std::ptr::null_mut();
    // SAFETY: the description, specification and attributes are live; the
    // callback record is copied by the call, and its reference value is the
    // caller's Outputs, which the decoder keeps until the session is
    // invalidated (`Outputs`).
    let status = unsafe {
        VTDecompressionSessionCreate(
            std::ptr::null(),
            description.as_ptr(),
            specification.as_ptr(),
            attributes.as_ptr(),
            &raw const callback,
            &raw mut session,
        )
    };
    // SAFETY: a Create call's result, a decompression session.
    let session = unsafe { Session::take(session, VTDecompressionSessionInvalidate) };
    if status != 0 {
        return Err(native_error("VTDecompressionSessionCreate", status));
    }
    session
        .ok_or_else(|| BackendError::Malfunction("VTDecompressionSessionCreate gave none".into()))
}

/// A CMTime from core's media time, which has the same fields.
pub(crate) fn cm_time(time: MediaTime) -> CMTime {
    CMTime {
        value: time.value,
        timescale: time.timescale,
        flags: time.flags,
        epoch: time.epoch,
    }
}

/// Core's media time from a CMTime.
pub(crate) fn media_time(time: CMTime) -> MediaTime {
    MediaTime {
        value: time.value,
        timescale: time.timescale,
        flags: time.flags,
        epoch: time.epoch,
    }
}

/// A sample buffer of the frame's bytes, copied into a block buffer of its
/// own, with its timing, in the session's format.
///
/// The bytes go as one sample, whatever sizes the guest's sample buffer
/// lists. VideoToolbox decodes each sample of a sample buffer as a frame of
/// its own and calls back for each, so a guest's sample buffer of N copies
/// of a key frame made N frames, all held until the decode call returned.
/// Apple's host hands its decoder the guest's sample buffer as one frame,
/// and a decoder emits once for each frame it is handed; the VA-API backend
/// reads a sample's bytes as one sample too.
pub(crate) fn sample_buffer(
    frame: &CompressedFrame<'_>,
    description: &Owned,
) -> Result<Owned, BackendError> {
    let length = frame.data.len();
    if length == 0 {
        return Err(BackendError::BadData("an empty sample".into()));
    }
    let mut block: CMBlockBufferRef = std::ptr::null_mut();
    // SAFETY: a block buffer of `length` bytes that CoreMedia allocates
    // now; the out pointer is a live local.
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            std::ptr::null(),
            std::ptr::null_mut(),
            length,
            std::ptr::null(),
            std::ptr::null(),
            0,
            length,
            kCMBlockBufferAssureMemoryNowFlag,
            &raw mut block,
        )
    };
    // SAFETY: a Create call's result, or NULL.
    let block = unsafe { Owned::take(block.cast_const()) };
    if status != 0 {
        return Err(native_error("CMBlockBufferCreateWithMemoryBlock", status));
    }
    let block = block.ok_or_else(|| BackendError::Malfunction("no block buffer".into()))?;
    // SAFETY: the block has `length` bytes, and the frame's data as many.
    let status = unsafe {
        CMBlockBufferReplaceDataBytes(frame.data.as_ptr().cast(), block.as_mut_ptr(), 0, length)
    };
    if status != 0 {
        return Err(native_error("CMBlockBufferReplaceDataBytes", status));
    }
    let timing = CMSampleTimingInfo {
        duration: cm_time(frame.duration),
        presentation_time_stamp: cm_time(frame.presentation_time),
        decode_time_stamp: cm_time(frame.decode_time),
    };
    let mut sample: CMSampleBufferRef = std::ptr::null_mut();
    // SAFETY: the block and description are live; one sample, with one
    // timing entry and one size, the block's; the out pointer is live.
    let status = unsafe {
        CMSampleBufferCreateReady(
            std::ptr::null(),
            block.as_mut_ptr(),
            description.as_ptr(),
            1,
            1,
            &raw const timing,
            1,
            &raw const length,
            &raw mut sample,
        )
    };
    // SAFETY: a Create call's result, or NULL.
    let sample = unsafe { Owned::take(sample.cast_const()) };
    if status != 0 {
        return Err(native_error("CMSampleBufferCreateReady", status));
    }
    sample.ok_or_else(|| BackendError::Malfunction("no sample buffer".into()))
}

impl BackendDecoder for Decoder {
    fn output_format(&self) -> PixelFormat {
        self.format
    }

    fn decode(
        &mut self,
        frame: &CompressedFrame<'_>,
    ) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        if let Some(config) = frame.new_config {
            self.reconfigure(config)?;
        }
        let sample = sample_buffer(frame, &self.description)?;
        // Anything left from before, from a frame that failed, is not this
        // frame's.
        drop(self.outputs.take());
        let mut info: VTDecodeInfoFlags = 0;
        // SAFETY: a live session and sample buffer; no flags, so the frame
        // is decoded synchronously and its output callback runs before the
        // call returns. Its reference value is its ID, which the callback
        // only reads back.
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session(),
                sample.as_mut_ptr(),
                0,
                frame_reference(frame.frame_id),
                &raw mut info,
            )
        };
        // SAFETY: a live session.
        unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session()) };
        if status != 0 {
            drop(self.outputs.take());
            return Err(native_error("VTDecompressionSessionDecodeFrame", status));
        }
        let ready = self.ready();
        self.frames_decoded = self.frames_read(frame, ready.is_ok());
        ready
    }

    fn flush(&mut self) -> Result<Vec<Box<dyn DecodedFrame>>, BackendError> {
        // SAFETY: a live session.
        let status = unsafe { VTDecompressionSessionFinishDelayedFrames(self.session()) };
        // SAFETY: a live session.
        unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session()) };
        if status != 0 {
            return Err(native_error(
                "VTDecompressionSessionFinishDelayedFrames",
                status,
            ));
        }
        self.ready()
    }

    fn can_accept(&self, config: &DecoderConfig) -> bool {
        if config.codec != self.config.codec || output_format(config).ok() != Some(self.format) {
            return false;
        }
        let Ok(description) = format_description(config) else {
            return false;
        };
        // SAFETY: a live session and format description.
        let accepts = unsafe {
            VTDecompressionSessionCanAcceptFormatDescription(self.session(), description.as_ptr())
        };
        accepts != 0
    }

    /// AV1's and VP9's frames, read from the sample alongside
    /// VideoToolbox's decode ([`Decoder::frames_read`]): none where the
    /// reader refused an AV1 sample, and core then lends one buffer for
    /// each frame VideoToolbox gives.
    fn frames_decoded(&mut self) -> Vec<FrameDecoded> {
        std::mem::take(&mut self.frames_decoded)
    }
}

/// A decoded frame, in VideoToolbox's IOSurface-backed buffer until core
/// copies it.
pub struct Frame {
    frame_id: i64,
    image: PixelBuffer,
    format: PixelFormat,
    width: u32,
    height: u32,
    /// Its decoder's pixel transfer, which writes it into a guest's buffer
    /// where the host lends one ([`DecodedFrame::write_to_guest`]).
    transfer: Arc<PixelTransfer>,
}

impl Frame {
    /// A frame of `image`, no larger than `picture` where that is given:
    /// VideoToolbox's buffer is the format description's size, and only
    /// its top left the stream's picture.
    pub(crate) fn new(
        frame_id: i64,
        image: PixelBuffer,
        format: PixelFormat,
        picture: Option<(u32, u32)>,
        transfer: &Arc<PixelTransfer>,
    ) -> Result<Self, BackendError> {
        if image.format() != format.fourcc().0 {
            return Err(BackendError::Malfunction(format!(
                "a frame in {} where {} was asked",
                fourcc_text(image.format()),
                fourcc_text(format.fourcc().0)
            )));
        }
        let (width, height) = image.size();
        let (width, height) = (
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        let (width, height) =
            picture.map_or((width, height), |(w, h)| (width.min(w), height.min(h)));
        Ok(Self {
            frame_id,
            image,
            format,
            width,
            height,
            transfer: Arc::clone(transfer),
        })
    }

    /// The pixel buffer VideoToolbox gave it.
    #[cfg(test)]
    pub(crate) fn image(&self) -> &PixelBuffer {
        &self.image
    }
}

impl DecodedFrame for Frame {
    fn frame_id(&self) -> i64 {
        self.frame_id
    }

    fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    /// Locks the pixel buffer VideoToolbox gave for reading, until the
    /// copy out of it, the CPU's, is done.
    fn read_back(&self) -> Result<Box<dyn FrameReadBack + '_>, BackendError> {
        Ok(Box::new(LockedFrame {
            frame: self,
            locked: self.image.lock_for_reading()?,
        }))
    }

    /// Writes the frame into the guest buffer's IOSurface by its decoder's
    /// pixel transfer, where the host lends one and the writer's checks
    /// pass (`zero_copy.rs`): otherwise declined, with why, and core copies
    /// it on the CPU.
    fn write_to_guest(&self, target: &GuestTarget<'_>) -> Result<DeviceWrite, BackendError> {
        let source = zero_copy::Source {
            image: &self.image,
            format: self.format,
            width: self.width,
            height: self.height,
        };
        zero_copy::write(&source, target, &*self.transfer)
    }
}

/// A frame whose pixel buffer is locked for reading.
struct LockedFrame<'a> {
    frame: &'a Frame,
    locked: LockedForReading<'a>,
}

impl FrameReadBack for LockedFrame<'_> {
    fn copy_to(&self, planes: &mut [PlaneDestination<'_>]) -> Result<(), BackendError> {
        if planes.len() < 2 {
            return Err(BackendError::Malfunction(format!(
                "4:2:0 needs two destination planes, not {}",
                planes.len()
            )));
        }
        let sources = self.locked.planes();
        let [(luma, luma_geometry), (chroma, chroma_geometry), ..] = sources.as_slice() else {
            return Err(BackendError::Malfunction(format!(
                "a 4:2:0 frame of {} planes",
                sources.len()
            )));
        };
        let frame = self.frame;
        copy_biplanar(
            frame.format,
            frame.width,
            frame.height,
            [
                (luma, luma_geometry.bytes_per_row),
                (chroma, chroma_geometry.bytes_per_row),
            ],
            planes,
        );
        Ok(())
    }
}
